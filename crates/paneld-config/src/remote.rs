//! Remotes (schema 17, ADR 0018): lowering instances into sources and
//! actions, and the pure pieces the daemon builds their commands
//! from (quoting for a remote shell, a curl config, URL encoding,
//! socket address rules).

use crate::parse::{Assign, AssignTarget, Block, Document, Ident, Item, RawValue};
use crate::schema::remote::{find_kind, via_of};

/// The key a lowered source or action carries: the instance it
/// belongs to. Only lowering writes it; the check refuses it in a
/// config.
pub const REMOTE_KEY: &str = "remote";

pub use crate::schema::remote::{members, METHODS};

/// One instance, as the daemon needs it to run its members.
#[derive(Clone, Debug)]
pub struct Remote {
    pub name: String,
    /// `local`, `ssh`, `http` or `socket`.
    pub via: &'static str,
    /// The kind block: connection keys, `headers`, `timeout`.
    pub kind: Block,
    /// Each kind param: name, type, and the instance's raw value (or
    /// the default); `None` when neither is set.
    pub params: Vec<(String, String, Option<String>)>,
}

/// Every instance of `doc` whose kind exists, by name.
pub fn remotes(doc: &Document) -> std::collections::BTreeMap<String, Remote> {
    let mut out = std::collections::BTreeMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "remote" {
            continue;
        }
        let (Some(name), Some(of)) = (&b.name, &b.of) else { continue };
        let Some(kind) = find_kind(doc, &of.text) else { continue };
        let Some(mut via) = via_of(kind) else { continue };
        // `via = local` on the instance (last one wins): an ssh kind
        // run on the panel itself.
        let local = b.items.iter().rev().find_map(|it| match it {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "via") => Some(a.value.text.trim() == "local"),
            _ => None,
        });
        if local == Some(true) && via == "ssh" {
            via = "local";
        }
        let params = kind
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Param(p) => {
                    // Last one wins: `remote[x]:host` overrides land at
                    // the end of the block.
                    let given = b.items.iter().rev().find_map(|it| match it {
                        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == p.name.text) => {
                            Some(a.value.text.trim().to_string())
                        }
                        _ => None,
                    });
                    let value = given.or_else(|| p.default.as_ref().map(|d| d.text.trim().to_string()));
                    Some((p.name.text.clone(), p.ty.text.clone(), value))
                }
                _ => None,
            })
            .collect();
        out.insert(name.text.clone(), Remote { name: name.text.clone(), via, kind: kind.clone(), params });
    }
    out
}

/// Add each instance's members to `doc` as `source` / `action` blocks
/// named `<instance>.<member>`, each with `remote = <instance>`. A
/// reference to a sibling member inside the kind (`refresh = temp`,
/// `then = reboot`) becomes the instance's (`device1.temp`). Run after
/// the check and the overrides.
pub fn lower(doc: &mut Document) {
    let mut added = Vec::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "remote" {
            continue;
        }
        let (Some(name), Some(of)) = (&b.name, &b.of) else { continue };
        let Some(kind) = find_kind(doc, &of.text) else { continue };
        let siblings: Vec<&str> = members(kind).map(|(n, _)| n).collect();
        for (member, m) in members(kind) {
            let mut block = m.clone();
            block.kind.text = if m.kind.text == "value" { "source" } else { "action" }.to_string();
            block.name = Some(Ident { text: format!("{}.{member}", name.text), span: m.name.as_ref().map_or(m.span, |n| n.span) });
            qualify_refs(&mut block.items, &name.text, &siblings);
            // The kind's `timeout` is its members' default.
            let has_timeout = |items: &[Item]| {
                items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "timeout")))
            };
            if !has_timeout(&block.items) {
                if let Some(t) = kind.items.iter().find(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "timeout"))) {
                    block.items.push(t.clone());
                }
            }
            block.items.push(Item::Assign(Assign {
                target: AssignTarget::Simple { key: Ident { text: REMOTE_KEY.into(), span: name.span } },
                value: RawValue { text: name.text.clone(), span: name.span },
                span: name.span,
            }));
            added.push(Item::Block(block));
        }
        // The kind's alerts, one per instance.
        for it in &kind.items {
            let Item::Block(a) = it else { continue };
            if a.kind.text != "alert" {
                continue;
            }
            let Some(alert_name) = &a.name else { continue };
            let mut block = a.clone();
            block.name = Some(Ident { text: format!("{}.{}", name.text, alert_name.text), span: alert_name.span });
            qualify_refs(&mut block.items, &name.text, &siblings);
            block.items.push(Item::Assign(Assign {
                target: AssignTarget::Simple { key: Ident { text: REMOTE_KEY.into(), span: name.span } },
                value: RawValue { text: name.text.clone(), span: name.span },
                span: name.span,
            }));
            added.push(Item::Block(block));
        }
    }
    doc.items.extend(added);
}

fn qualify_refs(items: &mut [Item], inst: &str, siblings: &[&str]) {
    let qualify = |t: &str| -> Option<String> {
        let t = t.trim();
        siblings.contains(&t).then(|| format!("{inst}.{t}"))
    };
    for it in items {
        match it {
            Item::Assign(a) => {
                let AssignTarget::Simple { key } = &a.target else { continue };
                match key.text.as_str() {
                    "then" | "else" | "from" | "action" => {
                        if let Some(q) = qualify(&a.value.text) {
                            a.value.text = q;
                        }
                    }
                    "refresh" => {
                        let t = a.value.text.trim();
                        let list = crate::value::split_list(t);
                        a.value.text = match list {
                            Some(items) => format!(
                                "[{}]",
                                items.iter().map(|i| qualify(i).unwrap_or_else(|| i.trim().to_string())).collect::<Vec<_>>().join(", ")
                            ),
                            None => qualify(t).unwrap_or_else(|| t.to_string()),
                        };
                    }
                    _ => {}
                }
            }
            Item::Block(b) if matches!(b.kind.text.as_str(), "then" | "else") => {
                if let Some(n) = &mut b.name {
                    if let Some(q) = qualify(&n.text) {
                        n.text = q;
                    }
                }
            }
            _ => {}
        }
    }
}

/// The instance a lowered source or action belongs to.
pub fn remote_of(b: &Block) -> Option<&str> {
    b.items.iter().find_map(|it| match it {
        Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == REMOTE_KEY) => Some(a.value.text.as_str()),
        _ => None,
    })
}

/// Quote `argv` for a POSIX shell on the far side of ssh: each item
/// in single quotes, a `'` inside as `'\''`.
pub fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Percent-encode everything but RFC 3986 unreserved characters, so a
/// value cannot add a path segment, a query or a fragment. A value of
/// just `.` or `..` is encoded whole, so it is not a path step.
pub fn percent_encode(s: &str) -> String {
    let dots = s == "." || s == "..";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'~') || (b == b'.' && !dots) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A value for a curl config line, in double quotes. `None` for a NUL,
/// which no config line can carry.
pub fn curl_quote(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\0' => return None,
            '\\' => out.push_str(r"\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    Some(out)
}

/// A URL or header value: no CR, LF or NUL (they would split the
/// request).
pub fn single_line(s: &str) -> bool {
    !s.contains(['\r', '\n', '\0'])
}

/// A Unix socket path socat reads as one address: no `,` (options),
/// `!` (`!!` pairs) or whitespace.
pub fn socket_path_ok(s: &str) -> bool {
    !s.is_empty() && !s.contains([',', '!']) && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A TCP host for socat: a name or IPv4 address, or IPv6 in brackets.
pub fn socket_host_ok(s: &str) -> bool {
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return !inner.is_empty() && inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_keeps_each_item_one_word() {
        let argv = vec!["echo".to_string(), "a b".into(), "it's".into(), "$(id)".into(), "".into()];
        assert_eq!(shell_quote(&argv), r#"'echo' 'a b' 'it'\''s' '$(id)' ''"#);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s|' {}", shell_quote(&argv[1..])))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a b|it's|$(id)||");
    }

    #[test]
    fn encoding_and_curl_quoting() {
        assert_eq!(percent_encode("../a b/?#&=%"), "..%2Fa%20b%2F%3F%23%26%3D%25");
        assert_eq!(percent_encode(".."), "%2E%2E");
        assert_eq!(percent_encode("a.txt"), "a.txt");
        assert_eq!(percent_encode("vm-101_x~"), "vm-101_x~");
        assert_eq!(curl_quote("a \"b\" \\ \n\r\t").unwrap(), r#""a \"b\" \\ \n\r\t""#);
        assert!(curl_quote("a\0").is_none());
        assert!(!single_line("a\r\nX: 1"));
    }

    #[test]
    fn lowering_names_members_and_qualifies_sibling_refs() {
        let src = "remote_kind k {\n param host: string\n param port: int = 22\n via = ssh\n host = {{ host }}\n key = /k\n timeout = 3s\n\
                   value temp { run = cat t }\n\
                   action reboot { run = reboot; refresh = [temp, other]; then = done }\n\
                   action done { }\n}\n\
                   remote d1: k { host = a }\nremote[d1]:host = b\n";
        let mut map = crate::SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = crate::lex::tokenize(id, src);
        let (mut doc, diags) = crate::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        crate::apply_overrides(&mut doc);
        lower(&mut doc);
        let block = |kind: &str, name: &str| {
            doc.items.iter().find_map(|it| match it {
                Item::Block(b) if b.kind.text == kind && b.name.as_ref().is_some_and(|n| n.text == name) => Some(b.clone()),
                _ => None,
            })
        };
        let temp = block("source", "d1.temp").expect("source d1.temp");
        assert_eq!(remote_of(&temp), Some("d1"));
        let reboot = block("action", "d1.reboot").unwrap();
        let get = |k: &str| reboot.items.iter().find_map(|it| match it {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == k) => Some(a.value.text.clone()),
            _ => None,
        });
        assert_eq!(get("refresh").as_deref(), Some("[d1.temp, other]"));
        assert_eq!(get("then").as_deref(), Some("d1.done"));
        // The kind's `timeout` reaches members without their own.
        assert_eq!(get("timeout").as_deref(), Some("3s"));
        // The kind's alerts, one per instance, `from` qualified.
        let src2 = "remote_kind k { via = local\n value log { run = cat; stream = true }\n alert ev { from = log } }\nremote r1: k { }\n";
        let mut map2 = crate::SourceMap::new();
        let id2 = map2.add(std::path::PathBuf::from("u.conf"), src2.to_string());
        let (toks2, _) = crate::lex::tokenize(id2, src2);
        let (mut doc2, _) = crate::parse(&toks2, &map2);
        lower(&mut doc2);
        let ev = doc2.items.iter().find_map(|it| match it {
            Item::Block(b) if b.kind.text == "alert" && b.name.as_ref().is_some_and(|n| n.text == "r1.ev") => Some(b.clone()),
            _ => None,
        });
        let ev = ev.expect("alert r1.ev");
        assert_eq!(remote_of(&ev), Some("r1"));
        assert!(ev.items.iter().any(|it| matches!(it, Item::Assign(a) if a.value.text == "r1.log")));
        let r = &remotes(&doc)["d1"];
        assert_eq!(r.via, "ssh");
        assert_eq!(r.params, [("host".into(), "string".into(), Some("b".into())), ("port".into(), "int".into(), Some("22".into()))]);
    }

    #[test]
    fn socket_addresses() {
        assert!(socket_path_ok("/run/x.sock"));
        assert!(!socket_path_ok("/run/x,fork"));
        assert!(!socket_path_ok("/run/a!!b"));
        assert!(socket_host_ok("nas.lan"));
        assert!(socket_host_ok("[fe80::1]"));
        assert!(!socket_host_ok("a:b"));
        assert!(!socket_host_ok("-x"));
    }
}
