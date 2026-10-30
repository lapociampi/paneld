//! Top-level `var <name>: <type> = <default>` declarations (schema
//! section 3). Each var is in-memory, mutable state that an action's
//! `set { ... }` block can write and the UI reads as `vars.<name>`.
//!
//! Values live in an `Arc<Mutex<BTreeMap<String, Value>>>` shared by
//! the main (render) thread and the action worker, so a tap-triggered
//! `set` is visible on the next render tick.

use std::collections::BTreeMap;

use paneld_config::parse::{Document, Item, VarDecl};
use paneld_config::value::{parse_bool, parse_int, parse_number};
use paneld_ui::Value;

/// All `var` declarations in the document, in source order.
pub fn collect(doc: &Document) -> Vec<&VarDecl> {
    let mut out = Vec::new();
    for item in &doc.items {
        if let Item::Var(v) = item {
            out.push(v);
        }
    }
    out
}

/// Map from var name to its declared type text (`"int"`, `"string"`,
/// ...). The worker uses it to coerce a `set` RHS to the right type.
pub fn types(doc: &Document) -> BTreeMap<String, String> {
    collect(doc)
        .into_iter()
        .map(|v| (v.name.text.clone(), v.ty.text.clone()))
        .collect()
}

/// Evaluate each declaration's default literal into a [`Value`]. A
/// malformed default falls back to the type's zero so a reload never
/// produces a half-built var map; the schema check surfaces the
/// problem to the user separately.
pub fn initial_values(doc: &Document) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for decl in collect(doc) {
        out.insert(decl.name.text.clone(), parse_default(&decl.ty.text, &decl.default.text));
    }
    out
}

fn parse_default(ty: &str, raw: &str) -> Value {
    let trimmed = raw.trim();
    match ty {
        "int" => Value::Int(parse_int(trimmed).unwrap_or(0)),
        "number" => Value::Double(parse_number(trimmed).unwrap_or(0.0)),
        "bool" => Value::Bool(parse_bool(trimmed).unwrap_or(false)),
        _ => Value::String(unquote(trimmed)),
    }
}

/// Coerce a worker-side set RHS (already evaluated to a [`Value`]) to
/// the var's declared type. Returns `None` if the coercion is
/// nonsensical (e.g. assigning a list to an `int`), which the worker
/// reports and then leaves the var alone.
pub fn coerce(ty: &str, v: Value) -> Option<Value> {
    match (ty, v) {
        ("int", Value::Int(n)) => Some(Value::Int(n)),
        ("int", Value::Double(d)) => Some(Value::Int(d as i64)),
        ("int", Value::Bool(b)) => Some(Value::Int(if b { 1 } else { 0 })),
        ("number", Value::Int(n)) => Some(Value::Double(n as f64)),
        ("number", Value::Double(d)) => Some(Value::Double(d)),
        ("bool", Value::Bool(b)) => Some(Value::Bool(b)),
        ("string", Value::String(s)) => Some(Value::String(s)),
        ("string", other) => Some(Value::String(other.to_display_string())),
        ("list", v @ Value::List(_)) => Some(v),
        ("any", v) => Some(v),
        _ => None,
    }
}

/// Strip one layer of matching single or double quotes if the whole
/// text is wrapped in them. Keeps the config-level literal clean of
/// the quoting the parser preserved verbatim.
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let a = bytes[0];
        let b = bytes[bytes.len() - 1];
        if (a == b'"' || a == b'\'') && a == b {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(src: &str) -> Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        use std::path::PathBuf;
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, lex_errs) = tokenize(id, src);
        assert!(lex_errs.is_empty(), "lex errors: {lex_errs:?}");
        let (doc, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "parse errors: {diags:?}");
        doc
    }

    #[test]
    fn collects_and_parses_defaults() {
        let doc = load("var tab: int = 0\nvar vm: string = \"\"\nvar on: bool = true\n");
        let vals = initial_values(&doc);
        assert_eq!(vals.get("tab"), Some(&Value::Int(0)));
        assert_eq!(vals.get("vm"), Some(&Value::String(String::new())));
        assert_eq!(vals.get("on"), Some(&Value::Bool(true)));
    }

    #[test]
    fn types_map() {
        let doc = load("var tab: int = 0\nvar s: string = foo\n");
        let m = types(&doc);
        assert_eq!(m.get("tab"), Some(&"int".to_string()));
        assert_eq!(m.get("s"), Some(&"string".to_string()));
    }

    #[test]
    fn coerce_int_accepts_bool_and_double() {
        assert_eq!(coerce("int", Value::Bool(true)), Some(Value::Int(1)));
        assert_eq!(coerce("int", Value::Double(2.9)), Some(Value::Int(2)));
        assert_eq!(coerce("int", Value::String("x".into())), None);
    }

    #[test]
    fn malformed_default_falls_back_to_zero() {
        let doc = load("var tab: int = not_an_int\n");
        let vals = initial_values(&doc);
        assert_eq!(vals.get("tab"), Some(&Value::Int(0)));
    }
}
