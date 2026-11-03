//! Collect `secret NAME { env = X | file = PATH }` into a map.
//!
//! ADR 0005 and the requirements say secrets are
//! referenced, never stored in config, and masked in logs. This
//! module reads them at startup and surfaces them in the eval
//! context as `secrets.<name>`.
//!
//! Scope today: `env` and `file` sources, with the file contents
//! trimmed; `cmd` comes later. [`Mask`] hides their values in what
//! commands print, before it is logged, shown or stored.

use std::collections::BTreeMap;

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_ui::Value;

pub fn resolve(doc: &Document) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "secret" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        if let Some(v) = resolve_one(b) {
            out.insert(name.text.clone(), Value::String(v));
        }
    }
    out
}

fn resolve_one(b: &Block) -> Option<String> {
    if let Some(env_name) = find_simple(b, "env") {
        return std::env::var(env_name.trim()).ok();
    }
    if let Some(path) = find_simple(b, "file") {
        return std::fs::read_to_string(path.trim())
            .ok()
            .map(|s| s.trim().to_string());
    }
    None
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

/// Values shorter than this are not masked: hiding every `1` or `ok`
/// in command output would mangle it and hide nothing worth hiding.
const MIN_MASKED_LEN: usize = 4;

/// Replaces secret values with `****` in command output. A command
/// that echoes its argv or env (usage text, a verbose error) would
/// otherwise put a token on the screen and in the journal.
#[derive(Clone, Debug, Default)]
pub struct Mask(Vec<String>);

impl Mask {
    pub fn new(secrets: &BTreeMap<String, Value>) -> Self {
        let mut values: Vec<String> = secrets
            .values()
            .filter_map(|v| match v {
                Value::String(s) if s.len() >= MIN_MASKED_LEN => Some(s.clone()),
                _ => None,
            })
            .collect();
        // Longest first, so a secret that contains another is hidden
        // whole.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        values.dedup();
        Self(values)
    }

    /// From a command context's `secrets` map, if it has one.
    pub fn from_ctx(ctx: &paneld_ui::Context) -> Self {
        match ctx.get("secrets") {
            Some(Value::Map(m)) => Self::new(m),
            _ => Self::default(),
        }
    }

    /// `value` with every string in it (map keys too) masked; numbers
    /// and the shape stay as they are. Used on parsed output, where a
    /// secret is in its unescaped form.
    pub fn apply_value(&self, value: Value) -> Value {
        if self.0.is_empty() {
            return value;
        }
        match value {
            Value::String(s) => Value::String(self.apply(&s)),
            Value::List(l) => Value::List(l.into_iter().map(|v| self.apply_value(v)).collect()),
            Value::Map(m) => Value::Map(m.into_iter().map(|(k, v)| (self.apply(&k), self.apply_value(v))).collect()),
            other => other,
        }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for v in &self.0 {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "****");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_hides_values_longest_first_and_skips_short_ones() {
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), Value::String("s3cr3t-token".into()));
        m.insert("b".to_string(), Value::String("s3cr3t".into()));
        m.insert("c".to_string(), Value::String("ok".into()));
        let mask = Mask::new(&m);
        assert_eq!(mask.apply("auth s3cr3t-token / s3cr3t: ok"), "auth **** / ****: ok");
        assert_eq!(Mask::default().apply("s3cr3t"), "s3cr3t");
        // Parsed values: strings masked, numbers untouched.
        let mut m2 = BTreeMap::new();
        m2.insert("pin".to_string(), Value::String("1234".into()));
        let mask = Mask::new(&m2);
        let v = crate::sources::parse_json(r#"{"ts": 1762341234, "user": "a 1234 b", "1234": true}"#).unwrap();
        let Value::Map(out) = mask.apply_value(v) else { panic!() };
        assert_eq!(out["ts"], Value::Int(1_762_341_234));
        assert_eq!(out["user"], Value::String("a **** b".into()));
        assert!(out.contains_key("****"));
    }
}
