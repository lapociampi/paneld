//! Library items shipped with the program (themes.md, "Precedence",
//! layer 2), written in the config language in `library.conf` and
//! merged by [`crate::load`], so `paneld check` and the daemon see the
//! same config.
//!
//! A config component or page with a library name replaces the library
//! one. Library actions are the built-in actions (ADR 0016): they are
//! always added, so a config that defines one gets the usual "defined
//! twice" error.

use std::collections::HashSet;

use crate::lex::tokenize;
use crate::parse::{parse, Diagnostic, Document, Item};
use crate::source::SourceMap;

const LIBRARY: &str = include_str!("library.conf");

/// Built-in actions (ADR 0016), handled by the daemon itself.
pub const BUILTIN_ACTIONS: &[&str] = &["alert_ack", "alert_snooze"];

/// How many top-level items the library adds to a config that
/// replaces none of them (for tests that count items).
pub fn item_count() -> usize {
    let mut doc = Document { items: Vec::new() };
    merge(&mut doc, &mut SourceMap::new(), &mut Vec::new());
    doc.items.len()
}

pub fn merge(doc: &mut Document, source: &mut SourceMap, diags: &mut Vec<Diagnostic>) {
    let defined: HashSet<(String, String)> = doc
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Block(b) => b.name.as_ref().map(|n| (b.kind.text.clone(), n.text.clone())),
            _ => None,
        })
        .collect();
    let id = source.add(std::path::PathBuf::from("<library>"), LIBRARY.to_string());
    let (toks, lex_errs) = tokenize(id, LIBRARY);
    diags.extend(lex_errs.into_iter().map(Diagnostic::from));
    let (lib, parse_diags) = parse(&toks, source);
    diags.extend(parse_diags);
    for item in lib.items {
        if let Item::Block(b) = &item {
            let replaced = b.kind.text != "action"
                && b.name
                    .as_ref()
                    .is_some_and(|n| defined.contains(&(b.kind.text.clone(), n.text.clone())));
            if replaced {
                continue;
            }
        }
        doc.items.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(src: &str) -> (SourceMap, Document, Vec<Diagnostic>) {
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, lex_errs) = tokenize(id, src);
        assert!(lex_errs.is_empty());
        let (mut doc, mut diags) = parse(&toks, &map);
        merge(&mut doc, &mut map, &mut diags);
        (map, doc, diags)
    }

    fn named(doc: &Document, kind: &str, name: &str) -> usize {
        doc.items
            .iter()
            .filter(|it| matches!(it, Item::Block(b) if b.kind.text == kind
                && b.name.as_ref().is_some_and(|n| n.text == name)))
            .count()
    }

    #[test]
    fn library_checks_clean() {
        let (map, doc, mut diags) = load_text("page home { }\n");
        diags.extend(crate::check(&doc, &map));
        let msgs: Vec<String> = diags
            .iter()
            .map(|d| format!("{}: {}", map.location(d.span), d.message))
            .collect();
        assert!(msgs.is_empty(), "library diagnostics:\n{}", msgs.join("\n"));
        for a in BUILTIN_ACTIONS {
            assert_eq!(named(&doc, "action", a), 1, "{a}");
        }
    }

    #[test]
    fn a_config_component_or_page_replaces_the_library_one() {
        let (_, doc, _) = load_text("component shell { column { slot { } } }\npage alerts { }\n");
        assert_eq!(named(&doc, "component", "shell"), 1);
        assert_eq!(named(&doc, "page", "alerts"), 1);
    }

    #[test]
    fn a_config_cannot_redefine_a_builtin_action() {
        let (map, doc, mut diags) = load_text("action alert_ack { run = /bin/true }\n");
        diags.extend(crate::check(&doc, &map));
        assert!(
            diags.iter().any(|d| d.message.contains("alert_ack")),
            "{:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }
}
