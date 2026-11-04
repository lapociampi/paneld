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

/// A relative `file` is read from `base`, the main config file's
/// folder (as commands run there, ADR 0013).
pub fn resolve(doc: &Document, base: &std::path::Path) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "secret" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        if let Some(v) = resolve_one(b, base) {
            out.insert(name.text.clone(), Value::String(v));
        }
    }
    out
}

fn resolve_one(b: &Block, base: &std::path::Path) -> Option<String> {
    if let Some(env_name) = find_simple(b, "env") {
        return std::env::var(paneld_config::value::parse_string(env_name)).ok();
    }
    if let Some(path) = find_simple(b, "file") {
        // A quoted path keeps its spaces, not its quotes.
        return std::fs::read_to_string(base.join(paneld_config::value::parse_string(path)))
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

    /// `value` with every string in it (map keys too) masked, and a
    /// number whose digits hold a secret replaced with `"****"`; the
    /// rest of the shape stays. Used on parsed output, where a secret
    /// is in its unescaped form.
    pub fn apply_value(&self, value: Value) -> Value {
        if self.0.is_empty() {
            return value;
        }
        match value {
            Value::String(s) => Value::String(self.apply(&s)),
            Value::Int(_) | Value::Double(_) if self.apply(&value.to_display_string()) != value.to_display_string() => {
                Value::String("****".into())
            }
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
        // Numbers: masked only when their digits hold a secret.
        assert_eq!(out["ts"], Value::String("****".into()));
        let v = crate::sources::parse_json(r#"{"n": 17623}"#).unwrap();
        let Value::Map(n) = mask.apply_value(v) else { panic!() };
        assert_eq!(n["n"], Value::Int(17623));
        assert_eq!(out["user"], Value::String("a **** b".into()));
        assert!(out.contains_key("****"));
    }

    #[test]
    fn secrets_come_from_env_or_a_file_and_missing_ones_are_left_out() {
        let dir = std::env::temp_dir().join(format!("paneld-secrets-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a b")).unwrap();
        let file = dir.join("a b").join("tok");
        std::fs::write(&file, "from-file\n").unwrap();
        // The test binary's own environment always has PATH.
        let src = format!(
            "secret p {{ env = PATH }}\nsecret f {{ file = \"{}\" }}\nsecret gone {{ file = /nonexistent/x }}\nsecret unset {{ env = PANELD_TEST_UNSET_VAR }}\nsecret rel {{ file = \"a b/tok\" }}\n",
            file.display()
        );
        let mut map = paneld_config::SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.clone());
        let (toks, _) = paneld_config::lex::tokenize(id, &src);
        let (doc, d) = paneld_config::parse(&toks, &map);
        assert!(d.is_empty(), "{d:?}");
        let s = resolve(&doc, &dir);
        assert_eq!(s["f"], Value::String("from-file".into()), "trailing newline stripped");
        assert_eq!(s["rel"], Value::String("from-file".into()), "relative to the config folder");
        assert_eq!(s["p"], Value::String(std::env::var("PATH").unwrap()));
        assert!(!s.contains_key("gone") && !s.contains_key("unset"), "{:?}", s.keys());
        let _ = std::fs::remove_dir_all(&dir);
    }
}