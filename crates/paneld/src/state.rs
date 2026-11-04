//! Local alert state file (ADR 0014). One JSON file at
//! `app:state` (default `/var/lib/paneld/state`), holding the active
//! alerts with their original `since` timestamps so a daemon restart
//! during a long-running incident doesn't lose when the alert first
//! fired. Writes are rate-limited to one every 10 seconds, use a
//! temp-file + atomic rename, and go to disk only when something
//! changed. A corrupt file is quarantined as `state.bad` and the
//! engine starts fresh.
//!
//! Holds the active alerts, the resolved ones (`history`, pruned by
//! the engine to `alerts:history`) and reminder dates.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::alerts::{Alert, Resolved};

const VERSION: u32 = 1;
const WRITE_INTERVAL: Duration = Duration::from_secs(10);

/// What persists on disk per alert. A superset of what `Alert`
/// holds plus the ack/snooze flags that aren't user-flippable yet
/// but will be as soon as the UI gains an ack path.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedAlert {
    pub rule: String,
    pub instance: String,
    pub severity: String,
    pub title: String,
    pub text: String,
    pub link: String,
    /// Unix epoch seconds.
    pub since: u64,
    pub acked: bool,
    /// Unix epoch seconds, or `None` when not snoozed.
    pub snoozed_until: Option<u64>,
}

impl SavedAlert {
    pub fn from_alert(a: &Alert) -> Self {
        Self {
            rule: a.rule.clone(),
            instance: a.instance.clone(),
            severity: a.severity.clone(),
            title: a.title.clone(),
            text: a.text.clone(),
            link: a.link.clone(),
            since: to_unix(a.since),
            acked: a.acked,
            snoozed_until: a.snoozed_until.map(to_unix),
        }
    }
}

/// When a reminder was first seen (its default start) and last done,
/// as Unix seconds.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SavedReminder {
    pub first_seen: u64,
    pub done: Option<u64>,
}

/// Everything the state file holds (ADR 0014).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StateData {
    pub alerts: BTreeMap<String, SavedAlert>,
    pub reminders: BTreeMap<String, SavedReminder>,
    /// Oldest first.
    pub history: Vec<Resolved>,
}

/// Rate-limited state file writer. Owns the state path, remembers
/// what was last on disk, and only writes when the running set
/// actually diverges from it, at most every [`WRITE_INTERVAL`].
pub struct Store {
    path: PathBuf,
    tmp: PathBuf,
    last_write: Instant,
    on_disk: StateData,
    pending: Option<StateData>,
}

impl Store {
    /// Open the state file. A readable file populates `on_disk`; an
    /// unreadable file is renamed to `state.bad` and the store opens
    /// empty.
    pub fn open(path: PathBuf) -> Self {
        let mut tmp = path.clone();
        tmp.set_extension("tmp");
        // Clear a stale `.tmp` from a daemon that died mid-write;
        // we never trust an unrenamed temp file.
        let _ = fs::remove_file(&tmp);
        let on_disk = match fs::read_to_string(&path) {
            Ok(s) => match parse(&s) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "paneld: state file {} is corrupt ({e}); quarantining as .bad",
                        path.display()
                    );
                    let mut bad = path.clone();
                    bad.set_extension("bad");
                    let _ = fs::rename(&path, &bad);
                    StateData::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StateData::default(),
            Err(e) => {
                eprintln!(
                    "paneld: cannot read state file {}: {e}; starting empty",
                    path.display()
                );
                StateData::default()
            }
        };
        Self {
            path,
            tmp,
            // Pretend the last write was long ago so the first dirty
            // tick after startup lands immediately. `checked_sub`
            // avoids the theoretical underflow on a monotonic clock
            // that's younger than the interval.
            last_write: Instant::now()
                .checked_sub(WRITE_INTERVAL)
                .unwrap_or_else(Instant::now),
            on_disk,
            pending: None,
        }
    }

    /// What the daemon loaded at startup. The alert engine replays
    /// these timestamps onto freshly fired alerts so `since` survives
    /// a restart.
    pub fn snapshot(&self) -> &StateData {
        &self.on_disk
    }

    /// Note the current alerts and reminder dates. Writes are
    /// deferred: nothing goes to disk until [`Self::flush_if_due`]
    /// decides enough time has passed.
    pub fn note(
        &mut self,
        alerts: &BTreeMap<String, Alert>,
        reminders: &BTreeMap<String, SavedReminder>,
        history: &[Resolved],
    ) {
        let snapshot = StateData {
            alerts: alerts
                .iter()
                .map(|(k, v)| (k.clone(), SavedAlert::from_alert(v)))
                .collect(),
            reminders: reminders.clone(),
            history: history.to_vec(),
        };
        if snapshot != self.on_disk {
            self.pending = Some(snapshot);
        } else {
            // Running set matches disk: drop any older pending.
            self.pending = None;
        }
    }

    /// If there's pending state and [`WRITE_INTERVAL`] has elapsed,
    /// write it out. Returns true iff a write happened.
    pub fn flush_if_due(&mut self, now: Instant) -> bool {
        let Some(_) = self.pending.as_ref() else { return false };
        if now.duration_since(self.last_write) < WRITE_INTERVAL {
            return false;
        }
        self.flush(now)
    }

    /// Force a write regardless of the rate limit. Called on daemon
    /// shutdown so the final state hits disk.
    pub fn force_flush(&mut self) -> bool {
        if self.pending.is_none() {
            return false;
        }
        self.flush(Instant::now())
    }

    fn flush(&mut self, now: Instant) -> bool {
        let Some(snapshot) = self.pending.take() else {
            return false;
        };
        match write_atomic(&self.path, &self.tmp, &snapshot) {
            Ok(()) => {
                self.on_disk = snapshot;
                self.last_write = now;
                true
            }
            Err(e) => {
                eprintln!(
                    "paneld: cannot write state file {}: {e}",
                    self.path.display()
                );
                // Keep `pending` so the next tick retries, and
                // bump `last_write` so we back off a full interval
                // instead of hammering the failing filesystem.
                self.pending = Some(snapshot);
                self.last_write = now;
                false
            }
        }
    }
}

fn write_atomic(path: &Path, tmp: &Path, data: &StateData) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let serialised = render(data);
    {
        // Alert titles and text may carry secrets or source output,
        // so lock the file down to the owning user. ADR 0014.
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        f.write_all(serialised.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)
}

fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the serialised JSON. Hand-rolled rather than pull a JSON
/// crate for one object.
fn render(data: &StateData) -> String {
    let alerts = &data.alerts;
    let mut out = String::new();
    out.push_str("{\n  \"version\": ");
    out.push_str(&VERSION.to_string());
    out.push_str(",\n  \"alerts\": {");
    for (i, (key, a)) in alerts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    ");
        out.push_str(&json_string(key));
        out.push_str(": {\n      \"rule\": ");
        out.push_str(&json_string(&a.rule));
        out.push_str(",\n      \"instance\": ");
        out.push_str(&json_string(&a.instance));
        out.push_str(",\n      \"severity\": ");
        out.push_str(&json_string(&a.severity));
        out.push_str(",\n      \"title\": ");
        out.push_str(&json_string(&a.title));
        out.push_str(",\n      \"text\": ");
        out.push_str(&json_string(&a.text));
        out.push_str(",\n      \"link\": ");
        out.push_str(&json_string(&a.link));
        out.push_str(",\n      \"since\": ");
        out.push_str(&a.since.to_string());
        out.push_str(",\n      \"acked\": ");
        out.push_str(if a.acked { "true" } else { "false" });
        out.push_str(",\n      \"snoozed_until\": ");
        match a.snoozed_until {
            Some(t) => out.push_str(&t.to_string()),
            None => out.push_str("null"),
        }
        out.push_str("\n    }");
    }
    if !alerts.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("},\n  \"reminders\": {");
    for (i, (name, r)) in data.reminders.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    ");
        out.push_str(&json_string(name));
        out.push_str(&format!(": {{ \"first_seen\": {}, \"done\": ", r.first_seen));
        match r.done {
            Some(t) => out.push_str(&t.to_string()),
            None => out.push_str("null"),
        }
        out.push_str(" }");
    }
    if !data.reminders.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("},\n  \"history\": [");
    for (i, r) in data.history.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "\n    {{ \"id\": {}, \"severity\": {}, \"title\": {}, \"text\": {}, \"since\": {}, \"ended\": {} }}",
            json_string(&r.id),
            json_string(&r.severity),
            json_string(&r.title),
            json_string(&r.text),
            r.since,
            r.ended
        ));
    }
    if !data.history.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("]\n}\n");
    out
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- parser ------------------------------------------------------------

#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Minimal reader for the shape [`render`] produces. Not a general
/// JSON parser: it accepts a top-level object with `version` and
/// `alerts`, each alert a flat object of our known fields.
fn parse(src: &str) -> Result<StateData, ParseError> {
    let mut p = Parser { src: src.as_bytes(), i: 0 };
    p.skip_ws();
    p.expect(b'{')?;
    let mut version: Option<u64> = None;
    let mut alerts: Option<BTreeMap<String, SavedAlert>> = None;
    let mut reminders: BTreeMap<String, SavedReminder> = BTreeMap::new();
    let mut history: Vec<Resolved> = Vec::new();
    loop {
        p.skip_ws();
        if p.peek() == Some(b'}') {
            break;
        }
        let key = p.string()?;
        p.skip_ws();
        p.expect(b':')?;
        p.skip_ws();
        match key.as_str() {
            "version" => version = Some(p.unsigned()?),
            "alerts" => alerts = Some(parse_alerts_map(&mut p)?),
            "reminders" => reminders = parse_reminders_map(&mut p)?,
            "history" => history = parse_history(&mut p)?,
            _ => p.skip_value()?,
        }
        p.skip_ws();
        if p.peek() == Some(b',') {
            p.i += 1;
        }
    }
    let version = version.ok_or_else(|| ParseError("missing version".into()))?;
    if version != VERSION as u64 {
        return Err(ParseError(format!(
            "unsupported state file version {version}"
        )));
    }
    Ok(StateData { alerts: alerts.unwrap_or_default(), reminders, history })
}

fn parse_history(p: &mut Parser) -> Result<Vec<Resolved>, ParseError> {
    p.expect(b'[')?;
    let mut out = Vec::new();
    loop {
        p.skip_ws();
        if p.peek() == Some(b']') {
            p.i += 1;
            return Ok(out);
        }
        p.expect(b'{')?;
        let mut r = Resolved {
            id: String::new(),
            severity: String::new(),
            title: String::new(),
            text: String::new(),
            since: 0,
            ended: 0,
        };
        loop {
            p.skip_ws();
            if p.peek() == Some(b'}') {
                p.i += 1;
                break;
            }
            let key = p.string()?;
            p.skip_ws();
            p.expect(b':')?;
            p.skip_ws();
            match key.as_str() {
                "id" => r.id = p.string()?,
                "severity" => r.severity = p.string()?,
                "title" => r.title = p.string()?,
                "text" => r.text = p.string()?,
                "since" => r.since = p.unsigned()?,
                "ended" => r.ended = p.unsigned()?,
                _ => p.skip_value()?,
            }
            p.skip_ws();
            if p.peek() == Some(b',') {
                p.i += 1;
            }
        }
        out.push(r);
        p.skip_ws();
        if p.peek() == Some(b',') {
            p.i += 1;
        }
    }
}

fn parse_reminders_map(p: &mut Parser) -> Result<BTreeMap<String, SavedReminder>, ParseError> {
    p.expect(b'{')?;
    let mut out = BTreeMap::new();
    loop {
        p.skip_ws();
        if p.peek() == Some(b'}') {
            p.i += 1;
            return Ok(out);
        }
        let name = p.string()?;
        p.skip_ws();
        p.expect(b':')?;
        p.skip_ws();
        p.expect(b'{')?;
        let mut r = SavedReminder::default();
        loop {
            p.skip_ws();
            if p.peek() == Some(b'}') {
                p.i += 1;
                break;
            }
            let key = p.string()?;
            p.skip_ws();
            p.expect(b':')?;
            p.skip_ws();
            match key.as_str() {
                "first_seen" => r.first_seen = p.unsigned()?,
                "done" if p.peek() == Some(b'n') => p.expect_kw("null")?,
                "done" => r.done = Some(p.unsigned()?),
                _ => p.skip_value()?,
            }
            p.skip_ws();
            if p.peek() == Some(b',') {
                p.i += 1;
            }
        }
        out.insert(name, r);
        p.skip_ws();
        if p.peek() == Some(b',') {
            p.i += 1;
        }
    }
}

fn parse_alerts_map(p: &mut Parser) -> Result<BTreeMap<String, SavedAlert>, ParseError> {
    p.expect(b'{')?;
    let mut out = BTreeMap::new();
    loop {
        p.skip_ws();
        if p.peek() == Some(b'}') {
            p.i += 1;
            return Ok(out);
        }
        let key = p.string()?;
        p.skip_ws();
        p.expect(b':')?;
        p.skip_ws();
        let a = parse_alert(p)?;
        out.insert(key, a);
        p.skip_ws();
        if p.peek() == Some(b',') {
            p.i += 1;
        }
    }
}

fn parse_alert(p: &mut Parser) -> Result<SavedAlert, ParseError> {
    p.expect(b'{')?;
    let mut rule = String::new();
    let mut instance = String::new();
    let mut severity = String::new();
    let mut title = String::new();
    let mut text = String::new();
    let mut link = String::new();
    let mut since: u64 = 0;
    let mut acked = false;
    let mut snoozed_until: Option<u64> = None;
    loop {
        p.skip_ws();
        if p.peek() == Some(b'}') {
            p.i += 1;
            break;
        }
        let key = p.string()?;
        p.skip_ws();
        p.expect(b':')?;
        p.skip_ws();
        match key.as_str() {
            "rule" => rule = p.string()?,
            "instance" => instance = p.string()?,
            "severity" => severity = p.string()?,
            "title" => title = p.string()?,
            "text" => text = p.string()?,
            "link" => link = p.string()?,
            "since" => since = p.unsigned()?,
            "acked" => acked = p.bool()?,
            "snoozed_until" => {
                if p.peek() == Some(b'n') {
                    p.expect_kw("null")?;
                    snoozed_until = None;
                } else {
                    snoozed_until = Some(p.unsigned()?);
                }
            }
            _ => p.skip_value()?,
        }
        p.skip_ws();
        if p.peek() == Some(b',') {
            p.i += 1;
        }
    }
    Ok(SavedAlert {
        rule,
        instance,
        severity,
        title,
        text,
        link,
        since,
        acked,
        snoozed_until,
    })
}

struct Parser<'a> {
    src: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.i).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek() != Some(c) {
            return Err(ParseError(format!(
                "expected `{}` at offset {}",
                c as char, self.i
            )));
        }
        self.i += 1;
        Ok(())
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), ParseError> {
        let bytes = kw.as_bytes();
        if self.src[self.i..].starts_with(bytes) {
            self.i += bytes.len();
            Ok(())
        } else {
            Err(ParseError(format!(
                "expected `{kw}` at offset {}",
                self.i
            )))
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    match self.peek() {
                        Some(b'"') => {
                            out.push('"');
                            self.i += 1;
                        }
                        Some(b'\\') => {
                            out.push('\\');
                            self.i += 1;
                        }
                        Some(b'n') => {
                            out.push('\n');
                            self.i += 1;
                        }
                        Some(b'r') => {
                            out.push('\r');
                            self.i += 1;
                        }
                        Some(b't') => {
                            out.push('\t');
                            self.i += 1;
                        }
                        Some(b'/') => {
                            out.push('/');
                            self.i += 1;
                        }
                        Some(b'u') => {
                            self.i += 1;
                            if self.i + 4 > self.src.len() {
                                return Err(ParseError("truncated \\u escape".into()));
                            }
                            let hex = std::str::from_utf8(&self.src[self.i..self.i + 4])
                                .map_err(|_| ParseError("bad \\u escape".into()))?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| ParseError("bad \\u escape".into()))?;
                            self.i += 4;
                            if let Some(c) = char::from_u32(code) {
                                out.push(c);
                            } else {
                                return Err(ParseError(
                                    "surrogates not supported".into(),
                                ));
                            }
                        }
                        _ => return Err(ParseError("bad escape".into())),
                    }
                }
                _ => {
                    // Multi-byte UTF-8 continuation bytes come along
                    // for the ride through the slice path at EOS; here
                    // we push one byte at a time because the quote and
                    // escape characters we care about are all ASCII,
                    // and the eventual String will still be valid
                    // UTF-8 because the input is.
                    let start = self.i;
                    while let Some(c) = self.peek() {
                        if matches!(c, b'"' | b'\\') {
                            break;
                        }
                        self.i += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.src[start..self.i]).map_err(
                        |_| ParseError("non-UTF-8 inside string".into()),
                    )?);
                }
            }
        }
        Err(ParseError("unterminated string".into()))
    }

    fn unsigned(&mut self) -> Result<u64, ParseError> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.i += 1;
            } else {
                break;
            }
        }
        if start == self.i {
            return Err(ParseError(format!("expected number at {}", self.i)));
        }
        let s = std::str::from_utf8(&self.src[start..self.i]).unwrap();
        s.parse::<u64>()
            .map_err(|e| ParseError(format!("bad number: {e}")))
    }

    fn bool(&mut self) -> Result<bool, ParseError> {
        if self.src[self.i..].starts_with(b"true") {
            self.i += 4;
            Ok(true)
        } else if self.src[self.i..].starts_with(b"false") {
            self.i += 5;
            Ok(false)
        } else {
            Err(ParseError(format!("expected bool at {}", self.i)))
        }
    }

    fn skip_value(&mut self) -> Result<(), ParseError> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => {
                let _ = self.string()?;
            }
            Some(b'{') | Some(b'[') => {
                let open = self.peek().unwrap();
                let close = if open == b'{' { b'}' } else { b']' };
                self.i += 1;
                let mut depth = 1;
                while depth > 0 {
                    self.skip_ws();
                    match self.peek() {
                        Some(b'"') => {
                            let _ = self.string()?;
                        }
                        Some(c) if c == open => {
                            self.i += 1;
                            depth += 1;
                        }
                        Some(c) if c == close => {
                            self.i += 1;
                            depth -= 1;
                        }
                        Some(_) => self.i += 1,
                        None => {
                            return Err(ParseError("unterminated container".into()));
                        }
                    }
                }
            }
            Some(c) if c.is_ascii_digit() || c == b'-' => {
                if c == b'-' {
                    self.i += 1;
                }
                let _ = self.unsigned()?;
            }
            Some(b't') => self.expect_kw("true")?,
            Some(b'f') => self.expect_kw("false")?,
            Some(b'n') => self.expect_kw("null")?,
            _ => return Err(ParseError("unexpected token".into())),
        }
        Ok(())
    }
}

/// Resolve the state-file path from the config, falling back to the
/// ADR 0014 default. `app:state = <path>` is read as the first
/// qualified assign; a `state = <path>` inside `app { ... }` works
/// too.
pub fn config_path(doc: &paneld_config::parse::Document) -> PathBuf {
    use paneld_config::parse::{AssignTarget, Item};
    for item in &doc.items {
        if let Item::Assign(a) = item {
            if let AssignTarget::Qualified { category, key } = &a.target {
                if category.text == "app" && key.text == "state" {
                    return PathBuf::from(paneld_config::value::parse_string(&a.value.text));
                }
            }
        }
        if let Item::Block(b) = item {
            if b.kind.text == "app" {
                for inner in &b.items {
                    if let Item::Assign(a) = inner {
                        if let AssignTarget::Simple { key } = &a.target {
                            if key.text == "state" {
                                return PathBuf::from(paneld_config::value::parse_string(&a.value.text));
                            }
                        }
                    }
                }
            }
        }
    }
    PathBuf::from("/var/lib/paneld/state")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(alerts: BTreeMap<String, SavedAlert>) -> StateData {
        StateData { alerts, reminders: BTreeMap::new(), history: Vec::new() }
    }

    #[test]
    fn roundtrips_reminders() {
        let mut d = StateData::default();
        d.reminders.insert("a".into(), SavedReminder { first_seen: 100, done: None });
        d.reminders.insert("b \"x\"".into(), SavedReminder { first_seen: 1, done: Some(2) });
        assert_eq!(parse(&render(&d)).expect("parse"), d);
        let r = |id: &str| Resolved {
            id: id.into(),
            severity: "warning".into(),
            title: "T \"q\" \u{e9}".into(),
            text: String::new(),
            since: 5,
            ended: 9,
        };
        d.history = vec![r("a:a"), r("b:b")];
        assert_eq!(parse(&render(&d)).expect("parse"), d);
    }

    fn sample_alert() -> Alert {
        Alert {
            rule: "pi_hot".into(),
            instance: "pi_hot".into(),
            severity: "warning".into(),
            title: "Pi hot".into(),
            text: "72.3 C".into(),
            link: "".into(),
            since: UNIX_EPOCH + Duration::from_secs(1_762_000_000),
            fired_at: Instant::now(),
            acked: false,
            snoozed_until: None,
        }
    }

    #[test]
    fn roundtrips_a_single_alert() {
        let a = sample_alert();
        let mut m = BTreeMap::new();
        m.insert("pi_hot:pi_hot".to_string(), SavedAlert::from_alert(&a));
        let s = render(&data(m.clone()));
        let parsed = parse(&s).expect("parse");
        assert_eq!(parsed.alerts, m);
    }

    #[test]
    fn handles_empty_set() {
        let s = render(&StateData::default());
        let parsed = parse(&s).expect("parse");
        assert_eq!(parsed, StateData::default());
        assert!(s.contains("\"alerts\": {}"));
        assert!(s.contains("\"reminders\": {}"));
    }

    #[test]
    fn preserves_non_ascii_text() {
        let a = Alert {
            text: "72\u{00b0} C \u{65e5}".into(),
            ..sample_alert()
        };
        let mut m = BTreeMap::new();
        m.insert("pi_hot:pi_hot".into(), SavedAlert::from_alert(&a));
        let parsed = parse(&render(&data(m))).expect("parse").alerts;
        assert_eq!(parsed["pi_hot:pi_hot"].text, "72\u{00b0} C \u{65e5}");
    }

    #[test]
    fn escapes_quotes_and_control() {
        let a = Alert {
            title: "quoth: \"nevermore\"\n".into(),
            ..sample_alert()
        };
        let mut m = BTreeMap::new();
        m.insert("pi_hot:pi_hot".into(), SavedAlert::from_alert(&a));
        let parsed = parse(&render(&data(m))).expect("parse").alerts;
        assert_eq!(parsed["pi_hot:pi_hot"].title, "quoth: \"nevermore\"\n");
    }

    #[test]
    fn snoozed_until_null_vs_some() {
        let a_null = sample_alert();
        let a_snoozed = Alert {
            snoozed_until: Some(UNIX_EPOCH + Duration::from_secs(1_762_000_600)),
            ..sample_alert()
        };
        let mut m = BTreeMap::new();
        m.insert("a".into(), SavedAlert::from_alert(&a_null));
        m.insert("b".into(), SavedAlert::from_alert(&a_snoozed));
        let parsed = parse(&render(&data(m))).expect("parse").alerts;
        assert_eq!(parsed["a"].snoozed_until, None);
        assert_eq!(parsed["b"].snoozed_until, Some(1_762_000_600));
    }

    #[test]
    fn rejects_wrong_version() {
        let bad = r#"{"version": 7, "alerts": {}}"#;
        assert!(parse(bad).is_err());
    }

    #[test]
    fn ignores_unknown_fields() {
        let s = r#"{
            "version": 1,
            "extra": "ignored",
            "alerts": {
                "pi_hot:pi_hot": {
                    "rule": "pi_hot",
                    "instance": "pi_hot",
                    "severity": "warning",
                    "title": "Pi hot",
                    "text": "",
                    "link": "",
                    "since": 1000,
                    "acked": false,
                    "snoozed_until": null,
                    "notes": "forward-compat"
                }
            }
        }"#;
        let m = parse(s).expect("parse");
        assert_eq!(m.alerts["pi_hot:pi_hot"].since, 1000);
        assert!(m.reminders.is_empty(), "older files have no reminders");
    }

    #[test]
    fn store_writes_only_when_due() {
        let dir = std::env::temp_dir().join(format!("paneld-state-{}", unique_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state");
        let mut store = Store::open(path.clone());

        let mut set = BTreeMap::new();
        set.insert("a".into(), sample_alert());
        store.note(&set, &BTreeMap::new(), &[]);

        // First flush: eligible (last_write was pushed back).
        assert!(store.flush_if_due(Instant::now()));
        assert!(path.exists());

        // Note the same set again: no diff, no pending, so no write.
        store.note(&set, &BTreeMap::new(), &[]);
        assert!(!store.flush_if_due(Instant::now()));

        // Change the set but try to flush immediately: suppressed by
        // the 10s interval.
        let mut set2 = set.clone();
        set2.get_mut("a").unwrap().text = "new".into();
        store.note(&set2, &BTreeMap::new(), &[]);
        assert!(!store.flush_if_due(Instant::now()));

        // force_flush bypasses the interval.
        assert!(store.force_flush());
        let on_disk = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.alerts["a"].text, "new");

        // A reminder date change is a change too.
        let mut r = BTreeMap::new();
        r.insert("restore".to_string(), SavedReminder { first_seen: 5, done: Some(9) });
        store.note(&set2, &r, &[]);
        assert!(store.force_flush());
        let on_disk = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.reminders, r);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_failure_bumps_last_write_so_retries_back_off() {
        // Point the state at a path whose parent CANNOT be created
        // (a path under a regular file). First flush fails, pending
        // stays set, last_write is bumped: a flush_if_due called a
        // moment later must return false.
        let dir = std::env::temp_dir().join(format!(
            "paneld-state-backoff-{}",
            unique_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "I am a file").unwrap();
        let bad_path = blocker.join("state"); // parent is a file
        let mut store = Store::open(bad_path);

        let mut set = BTreeMap::new();
        set.insert("a".into(), sample_alert());
        store.note(&set, &BTreeMap::new(), &[]);

        let t0 = Instant::now();
        assert!(!store.flush_if_due(t0), "write should fail");
        // Subsequent attempt a second later must be suppressed by
        // the backoff.
        let t1 = t0 + Duration::from_secs(1);
        assert!(!store.flush_if_due(t1), "second attempt must back off");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_quarantines_corrupt_file() {
        let dir = std::env::temp_dir().join(format!(
            "paneld-state-corrupt-{}",
            unique_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state");
        std::fs::write(&path, "{ this is not JSON").unwrap();
        let store = Store::open(path.clone());
        assert_eq!(store.snapshot(), &StateData::default());
        assert!(dir.join("state.bad").exists());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn unique_nanos() -> u32 {
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    }

    /// A minimal valid file with `rest` added to the top object.
    fn file(rest: &str) -> String {
        format!("{{\"version\": {VERSION}, \"alerts\": {{}}{rest}}}")
    }

    #[test]
    fn reads_every_json_escape() {
        let src = format!(
            "{{\"version\": {VERSION}, \"alerts\": {{\"a\": {{\"title\": \"q\\\"b\\\\s\\/n\\nr\\rt\\tu\\u00e9\\u0041\"}}}}}}"
        );
        let d = parse(&src).expect("parse");
        assert_eq!(d.alerts["a"].title, "q\"b\\s/n\nr\rt\tu\u{e9}A");
    }

    #[test]
    fn refuses_broken_strings() {
        for (bad, why) in [
            (r#"{"version": 1, "alerts": {"a": {"title": "x\q"}}}"#, "bad escape"),
            (r#"{"version": 1, "alerts": {"a": {"title": "\u12"#, "truncated"),
            (r#"{"version": 1, "alerts": {"a": {"title": "\uzzzz"}}}"#, "bad \\u"),
            (r#"{"version": 1, "alerts": {"a": {"title": "\ud800"}}}"#, "surrogates"),
            (r#"{"version": 1, "alerts": {"a": {"title": "open"#, "unterminated string"),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.0.contains(why), "{bad}: {}", e.0);
        }
    }

    #[test]
    fn skips_fields_it_does_not_know() {
        // A newer daemon's extra fields, of every JSON kind, at the
        // top, in an alert, a reminder and a history entry.
        let extra = r#""x": {"a": [1, -2, "]}", {"b": null}], "c": true, "d": false, "e": "}"}"#;
        let src = format!(
            "{{\"version\": {VERSION}, {extra}, \"alerts\": {{\"k\": {{{extra}, \"rule\": \"r\", \"acked\": true}}}}, \
             \"reminders\": {{\"m\": {{{extra}, \"first_seen\": 3, \"done\": null}}}}, \
             \"history\": [{{{extra}, \"id\": \"h\", \"ended\": 9}}], \"tail\": -7}}"
        );
        let d = parse(&src).expect("parse");
        assert_eq!(d.alerts["k"].rule, "r");
        assert!(d.alerts["k"].acked);
        assert_eq!(d.reminders["m"], SavedReminder { first_seen: 3, done: None });
        assert_eq!(d.history[0].id, "h");
        assert_eq!(d.history[0].ended, 9);
    }

    #[test]
    fn refuses_what_is_not_the_state_file() {
        for bad in [
            "",
            "[]",
            "{\"alerts\": {}}",
            &file(", \"x\": @"),
            &file(", \"x\": [1, 2"),
            &file(", \"x\": nul"),
            &format!("{{\"version\": {VERSION}, \"alerts\": {{\"a\": {{\"acked\": maybe}}}}}}"),
            &format!("{{\"version\": {VERSION}, \"alerts\": {{\"a\": {{\"since\": \"x\"}}}}}}"),
            &format!("{{\"version\": {VERSION}, \"alerts\": {{\"a\": {{}}}}"),
            "{\"version\": 1 x}",
        ] {
            assert!(parse(bad).is_err(), "accepted `{bad}`");
        }
        assert!(parse(&file("")).is_ok());
    }

    #[test]
    fn a_corrupt_file_is_set_aside_and_a_stale_temp_removed() {
        let dir = std::env::temp_dir().join(format!("paneld-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state");
        fs::write(&path, "{ not json").unwrap();
        fs::write(dir.join("state.tmp"), "half a write").unwrap();
        let store = Store::open(path.clone());
        assert_eq!(store.snapshot(), &StateData::default());
        assert!(!path.exists() && dir.join("state.bad").exists(), "not quarantined");
        assert!(!dir.join("state.tmp").exists(), "stale temp file kept");
        // A path that cannot be read as a file opens empty too.
        fs::create_dir_all(dir.join("adir")).unwrap();
        assert_eq!(Store::open(dir.join("adir")).snapshot(), &StateData::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_state_path_comes_from_app() {
        let path = |src: &str| {
            let mut map = paneld_config::SourceMap::new();
            let id = map.add(PathBuf::from("t.conf"), src.to_string());
            let (toks, _) = paneld_config::lex::tokenize(id, src);
            let (doc, _) = paneld_config::parse(&toks, &map);
            config_path(&doc)
        };
        assert_eq!(path("page p { }\n"), PathBuf::from("/var/lib/paneld/state"));
        assert_eq!(path("app { state = /x/s }\n"), PathBuf::from("/x/s"));
        assert_eq!(path("app:state = /y/s\n"), PathBuf::from("/y/s"));
        assert_eq!(path("app { timeout = 5s }\n"), PathBuf::from("/var/lib/paneld/state"));
        assert_eq!(path("app { state = \"/my dir/s\" }\n"), PathBuf::from("/my dir/s"));
    }
}