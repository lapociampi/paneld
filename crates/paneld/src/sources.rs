//! Resolve top-level `source` blocks into evaluated [`Value`]s.
//!
//! Scope today (first live-data milestone): a one-shot resolution at
//! daemon startup. For each `source NAME { run = ...; parse = ...;
//! value = {{ ... }} }` in the config:
//!
//! 1. Execute `run` through [`crate::runner`] with a timeout.
//! 2. Parse stdout per `parse` (`number`, `line`, `string`, `json`).
//! 3. If a `value = {{ ... }}` is set, evaluate it with `out` bound
//!    to the parsed stdout. Otherwise `out` itself is the value.
//!
//! The result is a `BTreeMap<String, Value>` ready to splice into an
//! [`paneld_ui::Context`] under the key `sources`, so the renderer
//! can look up `sources.cpu_temp.value` naturally.
//!
//! Polling, source history, streaming / on-demand sources and secret
//! substitution come in later phases.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_config::value::parse_duration;
use paneld_ui::{Context, Value};

use crate::runner;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// A fresh value for a named source, broadcast each time a polling
/// worker runs its `run` command.
#[derive(Debug)]
pub struct SourceUpdate {
    pub name: String,
    pub value: Value,
}

/// Resolve every top-level `source` block in `doc`, producing the
/// map that lives under `sources` in a [`Context`].
///
/// Writes a one-line warning to stderr for each source that failed to
/// run; the UI still renders, with that source missing from the
/// context.
pub fn resolve_all(doc: &Document) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "source" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        match resolve_one(b) {
            Ok(v) => {
                out.insert(name.text.clone(), v);
            }
            Err(e) => {
                eprintln!("paneld: source `{}`: {}", name.text, e);
            }
        }
    }
    out
}

/// Spawn a background worker for each source that declares a
/// `poll = DURATION`. Each worker sleeps for that interval, re-runs
/// the source's command, and emits a [`SourceUpdate`] on the shared
/// channel. Sources without a `poll` key never re-emit; they stay
/// at whatever value [`resolve_all`] produced.
///
/// Returns the [`Receiver`] end of the channel. The main thread
/// drains it on every pump of the LVGL loop.
pub fn spawn_pollers(doc: &Document) -> Receiver<SourceUpdate> {
    let (tx, rx) = mpsc::channel::<SourceUpdate>();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "source" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        let Some(poll_raw) = find_simple(b, "poll") else {
            continue;
        };
        let Some(poll) = parse_duration(poll_raw.trim()) else {
            eprintln!(
                "paneld: source `{}`: unparseable poll `{}`; polling disabled",
                name.text, poll_raw
            );
            continue;
        };
        // Clone the source block owned so the worker thread can read
        // it without holding a borrow of the document.
        let block_clone = b.clone();
        let name_clone = name.text.clone();
        let tx_clone: Sender<SourceUpdate> = tx.clone();
        thread::spawn(move || poll_loop(block_clone, name_clone, poll, tx_clone));
    }
    rx
}

fn poll_loop(block: Block, name: String, interval: Duration, tx: Sender<SourceUpdate>) {
    loop {
        thread::sleep(interval);
        match resolve_one(&block) {
            Ok(v) => {
                if tx.send(SourceUpdate { name: name.clone(), value: v }).is_err() {
                    // Receiver dropped; the daemon is shutting down.
                    return;
                }
            }
            Err(e) => {
                eprintln!("paneld: source `{name}`: {e}");
            }
        }
    }
}

/// Resolve one `source` block. Returns the [`Value::Map`] stored
/// under its name in the sources map; `.value` is set so the
/// idiomatic `sources.X.value` works. `.ok` reflects the command's
/// exit code (missing today -> `false`).
fn resolve_one(b: &Block) -> Result<Value, String> {
    let run = find_simple(b, "run").ok_or_else(|| "missing `run`".to_string())?;
    let parse_mode = find_simple(b, "parse").unwrap_or("text").trim().to_string();

    let r = runner::exec(run, DEFAULT_TIMEOUT, DEFAULT_MAX_BYTES)
        .map_err(|e| format!("run: {e}"))?;
    if r.timed_out {
        return Err("timed out".into());
    }
    let ok = r.exit_code == Some(0);

    let out = parse_output(&r.stdout, &parse_mode)
        .ok_or_else(|| format!("could not parse output as `{parse_mode}`"))?;

    // If the source has `value = {{ ... }}`, evaluate it with `out`
    // in scope and use the result as the public value. Otherwise
    // `out` is the value itself.
    let value = if let Some((raw, span)) = find_raw(b, "value") {
        eval_value_transform(raw, span, &out)
            .unwrap_or_else(|e| Value::String(format!("[eval error: {e}]")))
    } else {
        out.clone()
    };

    let mut m: BTreeMap<String, Value> = BTreeMap::new();
    m.insert("value".into(), value);
    m.insert("ok".into(), Value::Bool(ok));
    m.insert("exit".into(), r.exit_code.map(|n| Value::Int(n as i64)).unwrap_or(Value::Null));
    m.insert("stdout".into(), Value::String(r.stdout));
    m.insert("stderr".into(), Value::String(r.stderr));
    Ok(Value::Map(m))
}

fn parse_output(text: &str, mode: &str) -> Option<Value> {
    let t = text.trim();
    match mode {
        "text" => Some(Value::String(t.to_string())),
        "lines" => Some(Value::List(
            t.lines().map(|l| Value::String(l.to_string())).collect(),
        )),
        "number" => t.parse::<f64>().ok().map(Value::Double),
        "json" => parse_json(t),
        _ => None,
    }
}

/// Evaluate the `value = {{ ... }}` transform with `out` bound.
fn eval_value_transform(
    raw: &str,
    span: paneld_config::source::Span,
    out: &Value,
) -> Result<Value, String> {
    use paneld_config::cel;
    use paneld_config::value::{classify, ValueShape};

    let mut ctx: Context = Context::new();
    ctx.insert("out".into(), out.clone());

    match classify(raw) {
        ValueShape::Literal(text) => Ok(Value::String(text.to_string())),
        ValueShape::Expression(body) => {
            let (expr, diags) = cel::parse_fragment(body, span.file, span.start + 2);
            if !diags.is_empty() {
                return Err(diags[0].message.clone());
            }
            let expr = expr.ok_or_else(|| "empty expression".to_string())?;
            paneld_ui::eval(&expr, &ctx).map_err(|e| e.message)
        }
        ValueShape::Interp(_) => {
            // Mixed-literal `value` on a source is unusual; stringify
            // the fragments.
            let (fragments, diags) = cel::parse_interp(raw, span.file, span.start);
            if !diags.is_empty() {
                return Err(diags[0].message.clone());
            }
            let joined = fragments
                .iter()
                .map(|e| {
                    paneld_ui::eval(e, &ctx)
                        .map(|v| v.to_display_string())
                        .unwrap_or_else(|e| format!("[{}]", e.message))
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(Value::String(joined))
        }
    }
}

fn find_simple<'a>(node: &'a Block, key: &str) -> Option<&'a str> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return Some(a.value.text.as_str());
        }
    }
    None
}

fn find_raw<'a>(
    node: &'a Block,
    key: &str,
) -> Option<(&'a str, paneld_config::source::Span)> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return Some((a.value.text.as_str(), a.value.span));
        }
    }
    None
}

// -- JSON parse ----------------------------------------------------------

/// Minimal JSON reader for `parse = json` sources. Enough to turn a
/// typical `jq` or REST payload into [`Value::Map`] / [`Value::List`]
/// trees. No streaming, no error recovery; a malformed input returns
/// `None` and the source is treated as failed.
fn parse_json(s: &str) -> Option<Value> {
    let mut p = JsonParser { bytes: s.as_bytes(), pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return None;
    }
    Some(v)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(Value::String),
            b't' | b'f' => self.parse_bool(),
            b'n' => self.parse_null(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_object(&mut self) -> Option<Value> {
        self.bump()?; // '{'
        let mut m: BTreeMap<String, Value> = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Some(Value::Map(m));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bump()? != b':' {
                return None;
            }
            let v = self.parse_value()?;
            m.insert(key, v);
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b'}' => return Some(Value::Map(m)),
                _ => return None,
            }
        }
    }

    fn parse_array(&mut self) -> Option<Value> {
        self.bump()?; // '['
        let mut items: Vec<Value> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Some(Value::List(items));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b']' => return Some(Value::List(items)),
                _ => return None,
            }
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if self.bump()? != b'"' {
            return None;
        }
        let mut out = String::new();
        loop {
            match self.bump()? {
                b'"' => return Some(out),
                b'\\' => match self.bump()? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    _ => return None,
                },
                b => out.push(b as char),
            }
        }
    }

    fn parse_bool(&mut self) -> Option<Value> {
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"true") {
            self.pos += 4;
            Some(Value::Bool(true))
        } else if self.bytes.get(self.pos..self.pos + 5) == Some(b"false") {
            self.pos += 5;
            Some(Value::Bool(false))
        } else {
            None
        }
    }

    fn parse_null(&mut self) -> Option<Value> {
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"null") {
            self.pos += 4;
            Some(Value::Null)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Value> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_double = false;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            is_double = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_double = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        if is_double {
            text.parse::<f64>().ok().map(Value::Double)
        } else {
            text.parse::<i64>().ok().map(Value::Int)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_number() {
        assert_eq!(parse_output("42", "number"), Some(Value::Double(42.0)));
        assert_eq!(parse_output("  2.5\n", "number"), Some(Value::Double(2.5)));
        assert_eq!(parse_output("abc", "number"), None);
    }

    #[test]
    fn parse_output_lines_splits_each_line() {
        assert_eq!(
            parse_output("one\ntwo\n", "lines"),
            Some(Value::List(vec![
                Value::String("one".into()),
                Value::String("two".into()),
            ]))
        );
    }

    #[test]
    fn parse_output_json_object() {
        let v = parse_output(r#"{"a": 1, "b": "two"}"#, "json").unwrap();
        if let Value::Map(m) = v {
            assert_eq!(m.get("a"), Some(&Value::Int(1)));
            assert_eq!(m.get("b"), Some(&Value::String("two".into())));
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn parse_output_json_array_of_objects() {
        let v = parse_output(r#"[{"name": "x"}, {"name": "y"}]"#, "json").unwrap();
        if let Value::List(items) = v {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected list");
        }
    }
}
