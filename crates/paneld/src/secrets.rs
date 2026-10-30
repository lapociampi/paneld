//! Collect `secret NAME { env = X | file = PATH }` into a map.
//!
//! ADR 0005 and the requirements say secrets are
//! referenced, never stored in config, and masked in logs. This
//! module reads them at startup and surfaces them in the eval
//! context as `secrets.<name>`.
//!
//! Scope today: `env` and `file` sources, with the file contents
//! trimmed. `cmd` and masked logging come in a later phase.

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
