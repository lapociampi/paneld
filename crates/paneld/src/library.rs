//! Library components shipped with the program (themes.md,
//! "Precedence", layer 2), written in the config language in
//! `library.conf`. A component the config already defines wins.

use std::collections::HashSet;

use paneld_config::parse::{Document, Item};

const LIBRARY: &str = include_str!("library.conf");

/// Append every library component the config does not define itself.
pub fn merge(doc: &mut Document) {
    let defined: HashSet<String> = doc
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Block(b) if b.kind.text == "component" => b.name.as_ref().map(|n| n.text.clone()),
            _ => None,
        })
        .collect();
    for item in parse().items {
        if let Item::Block(b) = &item {
            if b.name.as_ref().is_some_and(|n| defined.contains(&n.text)) {
                continue;
            }
        }
        doc.items.push(item);
    }
}

fn parse() -> Document {
    use paneld_config::lex::tokenize;
    use paneld_config::source::SourceMap;
    let mut map = SourceMap::new();
    let id = map.add(std::path::PathBuf::from("<library>"), LIBRARY.to_string());
    let (toks, lex_errs) = tokenize(id, LIBRARY);
    let (doc, diags) = paneld_config::parse(&toks, &map);
    // The library is part of the program; the tests below keep it
    // clean, so this can only fire during development.
    for e in &lex_errs {
        eprintln!("paneld: library: {e:?}");
    }
    for d in &diags {
        eprintln!("paneld: library: {}: {}", map.location(d.span), d.message);
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_parses_and_checks_clean() {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("<library>"), LIBRARY.to_string());
        let (toks, lex_errs) = tokenize(id, LIBRARY);
        assert!(lex_errs.is_empty(), "{lex_errs:?}");
        let (doc, mut diags) = paneld_config::parse(&toks, &map);
        diags.extend(paneld_config::check(&doc, &map));
        let msgs: Vec<String> = diags
            .iter()
            .map(|d| format!("{}: {}", map.location(d.span), d.message))
            .collect();
        assert!(msgs.is_empty(), "library diagnostics:\n{}", msgs.join("\n"));
    }

    #[test]
    fn a_config_component_replaces_the_library_one() {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let src = "component shell { column { slot { } } }\n";
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        let (mut doc, _) = paneld_config::parse(&toks, &map);
        merge(&mut doc);
        let shells = doc
            .items
            .iter()
            .filter(|it| matches!(it, Item::Block(b) if b.kind.text == "component"
                && b.name.as_ref().is_some_and(|n| n.text == "shell")))
            .count();
        assert_eq!(shells, 1);
    }
}
