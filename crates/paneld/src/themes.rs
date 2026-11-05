//! Resolve the active theme's token map and splice it into the eval
//! context under the key `theme`.
//!
//! Tokens (colors, fonts, sizes, free extras) and, through
//! [`styles`], the `style <selector> { }` bodies. Theme-local
//! replacement `component NAME { }` blocks are deferred.
//!
//! Resolution order:
//!
//! 1. Find the active theme name: `screen { theme = ... }` or
//!    the first `theme NAME { }` block if `screen.theme` is unset.
//! 2. Walk `extends = parent` chain to the base, with cycle guard.
//! 3. Merge tokens from parent to child, child wins on conflict.
//! 4. Each token's raw value is kept as a `Value::String` -- the
//!    renderer's `parse_color` / `parse_size` handle shape
//!    checks when the token is consumed.

use std::collections::BTreeMap;

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_ui::Value;

pub fn resolve(doc: &Document) -> BTreeMap<String, Value> {
    resolve_with(doc, None)
}

/// Like [`resolve`], with `theme` (e.g. the night theme) in place of
/// the configured one.
pub fn resolve_with(doc: &Document, theme: Option<&str>) -> BTreeMap<String, Value> {
    // themes.md, "Precedence": built-in defaults first, then the
    // active theme chain on top. So every required token exists even
    // with no theme configured.
    let mut out = builtin_tokens();
    if let Some(active) = theme.map(str::to_string).or_else(|| active_theme_name(doc)) {
        let mut seen = Vec::new();
        out.extend(merge_theme(doc, &active, &mut seen));
    }
    out
}

/// The `style <selector> { }` bodies of the active theme (or `theme`)
/// and the themes it extends, parents first: per selector, a key the
/// child sets replaces the parent's.
pub fn styles(doc: &Document, theme: Option<&str>) -> BTreeMap<String, Vec<Item>> {
    let mut chain = Vec::new();
    let mut name = theme.map(str::to_string).or_else(|| active_theme_name(doc));
    while let Some(n) = name.take() {
        if chain.contains(&n) {
            break;
        }
        let Some(t) = find_theme(doc, &n) else { break };
        chain.push(n);
        name = find_simple(t, "extends").map(|p| p.trim().to_string());
    }
    let mut out: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for n in chain.iter().rev() {
        let Some(t) = find_theme(doc, n) else { continue };
        for item in &t.items {
            let Item::Block(b) = item else { continue };
            let (true, Some(sel)) = (b.kind.text == "style", &b.name) else { continue };
            let body = out.entry(sel.text.trim().to_string()).or_default();
            for it in &b.items {
                let Item::Assign(a) = it else { continue };
                let AssignTarget::Simple { key } = &a.target else { continue };
                body.retain(|old| !matches!(old, Item::Assign(o) if matches!(&o.target, AssignTarget::Simple { key: k } if k.text == key.text)));
                body.push(it.clone());
            }
        }
    }
    out
}

/// The built-in token set: every token schema section 11 requires.
/// Light, matching LVGL's default theme so widgets that are not
/// themed yet still fit in. Fonts are the compiled Montserrat sizes.
fn builtin_tokens() -> BTreeMap<String, Value> {
    paneld_config::schema::BUILTIN_THEME_TOKENS
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        let (d, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        d
    }

    fn tok(m: &BTreeMap<String, Value>, k: &str) -> String {
        match m.get(k) {
            Some(Value::String(s)) => s.clone(),
            other => panic!("token {k}: {other:?}"),
        }
    }

    #[test]
    fn no_theme_still_has_every_required_token() {
        let m = resolve(&doc("page home { }\n"));
        for k in [
            "background", "surface", "text", "muted", "accent", "ok", "info", "warning",
            "critical", "font_small", "font", "font_big", "font_huge", "gap", "padding",
            "radius", "tap_min", "shadow", "icons", "motion",
        ] {
            assert!(m.contains_key(k), "missing built-in token `{k}`");
        }
    }

    #[test]
    fn theme_chain_overrides_builtins_child_last() {
        let m = resolve(&doc(
            "theme base { accent = #111111; surface = #222222 }\n\
             theme mine { extends = base; accent = #333333 }\n\
             screen { theme = mine }\n",
        ));
        assert_eq!(tok(&m, "accent"), "#333333");
        assert_eq!(tok(&m, "surface"), "#222222");
        assert_eq!(tok(&m, "warning"), "#ef6c00");
    }

    #[test]
    fn styles_merge_down_the_chain() {
        let d = doc("theme base { style .card { radius = 4; padding = 2 }\n style button:pressed { radius = 1 } }\n\
             theme mine { extends = base\n style .card { radius = 16 } }\n\
             screen { theme = mine }\n");
        let st = styles(&d, None);
        let keys = |sel: &str| -> Vec<(String, String)> {
            st[sel]
                .iter()
                .map(|it| match it {
                    Item::Assign(a) => match &a.target {
                        AssignTarget::Simple { key } => (key.text.clone(), a.value.text.trim().to_string()),
                        _ => panic!(),
                    },
                    _ => panic!(),
                })
                .collect()
        };
        assert_eq!(keys(".card"), [("padding".into(), "2".into()), ("radius".into(), "16".into())]);
        assert!(st.contains_key("button:pressed"));
        assert!(styles(&d, Some("base"))[".card"].len() == 2);
    }

    #[test]
    fn which_theme_is_active() {
        // The first theme when nothing picks one; `screen:theme` picks.
        let first = resolve(&doc("theme a { accent = #000001 }\ntheme b { accent = #000002 }\n"));
        assert_eq!(tok(&first, "accent"), "#000001");
        let picked = resolve(&doc("theme a { accent = #000001 }\ntheme b { accent = #000002 }\nscreen:theme = b\n"));
        assert_eq!(tok(&picked, "accent"), "#000002");
        // An unknown theme leaves the built-in tokens.
        let unknown = resolve(&doc("screen { theme = nope }\n"));
        assert_eq!(tok(&unknown, "accent"), "#2196f3");
        // `resolve_with` (the night theme) wins over the configured one.
        let night = resolve_with(&doc("theme a { accent = #000001 }\ntheme n { accent = #000009 }\nscreen:theme = a\n"), Some("n"));
        assert_eq!(tok(&night, "accent"), "#000009");
    }

    #[test]
    fn extends_cycles_and_missing_parents_end_quietly() {
        let d = doc("theme a { extends = b; accent = #00000a }\ntheme b { extends = a; surface = #00000b }\nscreen:theme = a\n");
        let m = resolve(&d);
        assert_eq!(tok(&m, "accent"), "#00000a");
        assert_eq!(tok(&m, "surface"), "#00000b");
        let st = styles(&doc("theme a { extends = a\n style .c { radius = 1 } }\n"), None);
        assert_eq!(st[".c"].len(), 1);
        let orphan = resolve(&doc("theme a { extends = gone; accent = #00000c }\n"));
        assert_eq!(tok(&orphan, "accent"), "#00000c");
        assert!(styles(&doc("page p { }\n"), None).is_empty());
        assert!(styles(&doc("theme a { }\n"), Some("nope")).is_empty());
    }
}