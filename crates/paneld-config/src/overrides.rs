//! Fold `kind[name]:key = value` overrides into the named items they
//! target, so runtime consumers only ever read the block form.
//!
//! config-format, "Blocks": a named item is defined once, and single
//! properties of it can be overridden with `kind[name]:key`; options
//! are last-one-wins in load order. Each override replaces every
//! existing `key = ...` directly in the target block (keeping the
//! position of the first) or is appended when the block has none.
//! Overrides are applied in document order, so a later one wins.
//!
//! Settings categories (`app`, `screen`, `alerts`, ...) get the same
//! treatment: every `category { k = v }` block and `category:k = v`
//! option is merged into one block per category, holding the last
//! value of each key. Readers can then take the first match.
//!
//! Call this after [`crate::check`] passed: the check reports
//! overrides whose target is not defined, and those are left alone
//! here.

use crate::parse::{Assign, AssignTarget, Block, Document, Ident, Item};
use crate::schema::is_category;

pub fn apply_overrides(doc: &mut Document) {
    fold_named_overrides(doc);
    fold_categories(doc);
}

fn fold_categories(doc: &mut Document) {
    // Category kinds in order of first appearance.
    let mut kinds: Vec<String> = Vec::new();
    for item in &doc.items {
        let kind = match item {
            Item::Block(b) if b.name.is_none() && is_category(&b.kind.text) => &b.kind,
            Item::Assign(Assign { target: AssignTarget::Qualified { category, .. }, .. })
                if is_category(&category.text) =>
            {
                category
            }
            _ => continue,
        };
        if !kinds.contains(&kind.text) {
            kinds.push(kind.text.clone());
        }
    }

    for kind in kinds {
        let mut first_pos: Option<usize> = None;
        let mut first_ident: Option<Ident> = None;
        let mut first_span = None;
        // (key, assign) in order of each key's first appearance,
        // value replaced by later ones.
        let mut options: Vec<(String, Assign)> = Vec::new();
        let mut others: Vec<Item> = Vec::new();
        let mut put = |key: &Ident, a: Assign| {
            match options.iter_mut().find(|(k, _)| *k == key.text) {
                Some(slot) => slot.1 = a,
                None => options.push((key.text.clone(), a)),
            }
        };

        let mut i = 0;
        while i < doc.items.len() {
            let take = match &doc.items[i] {
                Item::Block(b) => b.name.is_none() && b.kind.text == kind,
                Item::Assign(Assign { target: AssignTarget::Qualified { category, .. }, .. }) => {
                    category.text == kind
                }
                _ => false,
            };
            if !take {
                i += 1;
                continue;
            }
            first_pos.get_or_insert(i);
            match doc.items.remove(i) {
                Item::Block(b) => {
                    first_ident.get_or_insert(b.kind.clone());
                    first_span.get_or_insert(b.span);
                    for inner in b.items {
                        match inner {
                            Item::Assign(a) => match &a.target {
                                AssignTarget::Simple { key } => {
                                    let key = key.clone();
                                    put(&key, a);
                                }
                                _ => others.push(Item::Assign(a)),
                            },
                            other => others.push(other),
                        }
                    }
                }
                Item::Assign(a) => {
                    let AssignTarget::Qualified { category, key } = &a.target else {
                        unreachable!()
                    };
                    first_ident.get_or_insert(category.clone());
                    first_span.get_or_insert(a.span);
                    let key = key.clone();
                    put(
                        &key,
                        Assign {
                            target: AssignTarget::Simple { key: key.clone() },
                            value: a.value,
                            span: a.span,
                        },
                    );
                }
                _ => unreachable!(),
            }
        }

        let (Some(pos), Some(ident), Some(span)) = (first_pos, first_ident, first_span) else {
            continue;
        };
        let mut items: Vec<Item> = options.into_iter().map(|(_, a)| Item::Assign(a)).collect();
        items.extend(others);
        doc.items.insert(
            pos,
            Item::Block(Block { kind: ident, name: None, items, span }),
        );
    }
}

fn fold_named_overrides(doc: &mut Document) {
    let overrides: Vec<Assign> = doc
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Assign(a) if matches!(a.target, AssignTarget::Indexed { .. }) => Some(a.clone()),
            _ => None,
        })
        .collect();

    for ov in overrides {
        let AssignTarget::Indexed { kind, name, key } = &ov.target else {
            continue;
        };
        let Some(block) = doc.items.iter_mut().find_map(|item| match item {
            Item::Block(b)
                if b.kind.text == kind.text
                    && b.name.as_ref().is_some_and(|n| n.text == name.text) =>
            {
                Some(b)
            }
            _ => None,
        }) else {
            continue;
        };
        let folded = Assign {
            target: AssignTarget::Simple { key: key.clone() },
            value: ov.value.clone(),
            span: ov.span,
        };
        let is_same_key = |item: &Item| {
            matches!(item, Item::Assign(a)
                if matches!(&a.target, AssignTarget::Simple { key: k } if k.text == key.text))
        };
        match block.items.iter().position(is_same_key) {
            Some(first) => {
                block.items[first] = Item::Assign(folded);
                let mut i = first + 1;
                while i < block.items.len() {
                    if is_same_key(&block.items[i]) {
                        block.items.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
            None => block.items.push(Item::Assign(folded)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tokenize;
    use crate::parse::Block;
    use crate::source::SourceMap;

    fn doc(src: &str) -> Document {
        let mut map = SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, lex_errs) = tokenize(id, src);
        assert!(lex_errs.is_empty(), "{lex_errs:?}");
        let (d, diags) = crate::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        d
    }

    fn block<'a>(d: &'a Document, kind: &str, name: &str) -> &'a Block {
        d.items
            .iter()
            .find_map(|i| match i {
                Item::Block(b)
                    if b.kind.text == kind && b.name.as_ref().is_some_and(|n| n.text == name) =>
                {
                    Some(b)
                }
                _ => None,
            })
            .expect("block")
    }

    fn values(b: &Block, key: &str) -> Vec<String> {
        b.items
            .iter()
            .filter_map(|i| match i {
                Item::Assign(a) => match &a.target {
                    AssignTarget::Simple { key: k } if k.text == key => {
                        Some(a.value.text.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn override_replaces_existing_key() {
        let mut d = doc("action reboot { run = /bin/true\n pin = false }\naction[reboot]:pin = true\n");
        apply_overrides(&mut d);
        assert_eq!(values(block(&d, "action", "reboot"), "pin"), vec!["true"]);
    }

    #[test]
    fn override_adds_missing_key() {
        let mut d = doc("action reboot { run = /bin/true }\naction[reboot]:pin = true\n");
        apply_overrides(&mut d);
        assert_eq!(values(block(&d, "action", "reboot"), "pin"), vec!["true"]);
    }

    #[test]
    fn later_override_wins() {
        let mut d = doc(
            "source cpu { run = a }\nsource[cpu]:run = b\nsource[cpu]:run = c\n",
        );
        apply_overrides(&mut d);
        assert_eq!(values(block(&d, "source", "cpu"), "run"), vec!["c"]);
    }

    #[test]
    fn duplicate_keys_in_block_collapse_to_the_override() {
        let mut d = doc("source cpu { run = a\n run = b }\nsource[cpu]:run = c\n");
        apply_overrides(&mut d);
        assert_eq!(values(block(&d, "source", "cpu"), "run"), vec!["c"]);
    }

    #[test]
    fn override_targets_only_the_matching_kind_and_name() {
        let mut d = doc(
            "action a { run = x }\naction b { run = y }\nsource a { run = z }\naction[a]:run = new\n",
        );
        apply_overrides(&mut d);
        assert_eq!(values(block(&d, "action", "a"), "run"), vec!["new"]);
        assert_eq!(values(block(&d, "action", "b"), "run"), vec!["y"]);
        assert_eq!(values(block(&d, "source", "a"), "run"), vec!["z"]);
    }

    fn category<'a>(d: &'a Document, kind: &str) -> Vec<&'a Block> {
        d.items
            .iter()
            .filter_map(|i| match i {
                Item::Block(b) if b.name.is_none() && b.kind.text == kind => Some(b),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn category_blocks_and_options_merge_last_wins() {
        let mut d = doc(
            "app { start_page = home\n timeout = 5s }\n\
             page home { }\npage alerts_page { }\n\
             app:start_page = alerts_page\n\
             app { timeout = 20s }\n",
        );
        apply_overrides(&mut d);
        let apps = category(&d, "app");
        assert_eq!(apps.len(), 1, "one merged app block");
        assert_eq!(values(apps[0], "start_page"), vec!["alerts_page"]);
        assert_eq!(values(apps[0], "timeout"), vec!["20s"]);
        // No qualified leftovers.
        assert!(!d.items.iter().any(|i| matches!(i,
            Item::Assign(Assign { target: AssignTarget::Qualified { .. }, .. }))));
    }

    #[test]
    fn categories_merge_independently_and_keep_position() {
        let mut d = doc("screen:idle = 1m\nsource s { run = x }\nalerts:history = 30d\nscreen:idle = 2m\n");
        apply_overrides(&mut d);
        let Item::Block(first) = &d.items[0] else { panic!("screen block first") };
        assert_eq!(first.kind.text, "screen");
        assert_eq!(values(first, "idle"), vec!["2m"]);
        assert_eq!(values(category(&d, "alerts")[0], "history"), vec!["30d"]);
    }

    #[test]
    fn override_of_undefined_item_is_left_alone() {
        let mut d = doc("action[ghost]:pin = true\n");
        apply_overrides(&mut d);
        assert_eq!(d.items.len(), 1);
    }
}
