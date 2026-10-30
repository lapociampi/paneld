//! Resolve the active theme's token map and splice it into the eval
//! context under the key `theme`.
//!
//! Scope for v1: tokens only (colors, fonts, sizes, free extras).
//! `style <selector> { }` sub-blocks and theme-local replacement
//! `component NAME { }` blocks are deferred -- the renderer still
//! runs; selectors and overrides just don't take effect yet.
//!
//! Resolution order:
//!
//! 1. Find the active theme name: `screen { theme = ... }` or
//!    the first `theme NAME { }` block if `screen.theme` is unset.
//! 2. Walk `extends = parent` chain to the base, with cycle guard.
//! 3. Merge tokens from parent to child, child wins on conflict.
//! 4. Each token's raw value is kept as a `Value::String` -- the
//!    renderer's `parse_hex_color` / `parse_size` handle shape
//!    checks when the token is consumed.

use std::collections::BTreeMap;

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_ui::Value;

pub fn resolve(doc: &Document) -> BTreeMap<String, Value> {
    let Some(active) = active_theme_name(doc) else {
        return BTreeMap::new();
    };
    let mut seen = Vec::new();
    merge_theme(doc, &active, &mut seen)
}

fn active_theme_name(doc: &Document) -> Option<String> {
    // 1. Explicit `screen { theme = ... }` or `screen:theme = ...`.
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "screen" {
                if let Some(v) = find_simple(b, "theme") {
                    return Some(v.trim().to_string());
                }
            }
        }
        if let Item::Assign(a) = item {
            if let AssignTarget::Qualified { category, key } = &a.target {
                if category.text == "screen" && key.text == "theme" {
                    return Some(a.value.text.trim().to_string());
                }
            }
        }
    }
    // 2. Fall back to the first `theme NAME` defined.
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "theme" {
                if let Some(name) = &b.name {
                    return Some(name.text.clone());
                }
            }
        }
    }
    None
}

fn find_theme<'a>(doc: &'a Document, name: &str) -> Option<&'a Block> {
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "theme"
                && b.name.as_ref().is_some_and(|n| n.text == name)
            {
                return Some(b);
            }
        }
    }
    None
}

fn merge_theme(doc: &Document, name: &str, seen: &mut Vec<String>) -> BTreeMap<String, Value> {
    if seen.iter().any(|s| s == name) {
        // `extends` cycle; stop here so we don't recurse forever.
        return BTreeMap::new();
    }
    seen.push(name.to_string());

    let Some(theme) = find_theme(doc, name) else {
        return BTreeMap::new();
    };

    // Parent tokens first, child wins.
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(parent) = find_simple(theme, "extends") {
        out = merge_theme(doc, parent.trim(), seen);
    }
    for item in &theme.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        if key.text == "extends" {
            continue;
        }
        out.insert(key.text.clone(), Value::String(a.value.text.clone()));
    }
    out
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
