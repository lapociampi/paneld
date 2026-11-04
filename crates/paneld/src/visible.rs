//! `active = visible` on a source (schema 4): it polls only while a
//! page that shows it is on screen and the screen is awake. Which
//! sources a page shows is worked out at load from its expressions,
//! those of the components it uses and the shell; sources an alert
//! reads always count as shown (ADR 0013: alerts never go stale).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use paneld_config::cel::{self, Expr, ExprKind};
use paneld_config::parse::{Block, Document, Item, RawValue};
use paneld_config::value::{classify, ValueShape};

#[derive(Default)]
struct State {
    /// Page name to the sources its expressions read.
    pages: BTreeMap<String, BTreeSet<String>>,
    /// Read by alerts: always shown.
    always: BTreeSet<String>,
    /// The page on screen; `None` before the first one is drawn.
    shown: Option<String>,
    asleep: bool,
    /// Bumped on every change (and on shutdown), so a poller only
    /// goes to sleep if nothing changed since it looked.
    generation: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Notified when what is shown may have changed (and on shutdown), so
/// a waiting poller looks again.
static CHANGED: Condvar = Condvar::new();

/// Marks "every source": a page that reads `sources` as a whole or
/// by a computed name.
const ALL: &str = "*";

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(State::default))
}

/// Wake every waiting poller (what is shown changed, or shutdown).
pub fn notify() {
    with(|s| s.generation += 1);
    CHANGED.notify_all();
}

/// Sleep until `until`, or until what is shown changes. Asleep this
/// costs nothing: no wakeups but the deadline and real changes.
/// `seen` is the generation [`is_shown`] gave: if it moved on since,
/// there is no sleep at all.
pub fn wait(until: Instant, seen: u64) {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    while g.as_ref().map_or(0, |s| s.generation) == seen {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        g = CHANGED.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// Work out the sources each page of `doc` shows (on load and reload).
/// `remotes`: the remote instance names, whose members read as
/// `device1.temp`.
pub fn install(doc: &Document, remotes: &BTreeMap<String, Vec<String>>) {
    let (pages, always) = analyse(doc, remotes);
    with(|s| {
        s.pages = pages;
        s.always = always;
    });
    notify();
}

/// `page` is now on screen.
pub fn show(page: &str) {
    if with(|s| s.shown.replace(page.to_string()).as_deref() != Some(page)) {
        notify();
    }
}

/// The screen went to sleep, or woke.
pub fn set_asleep(asleep: bool) {
    if with(|s| std::mem::replace(&mut s.asleep, asleep) != asleep) {
        notify();
    }
}

/// Whether source `name` is on screen now (before any page is drawn
/// everything is), and the generation to [`wait`] on.
pub fn is_shown(name: &str) -> (bool, u64) {
    with(|s| {
        let shown = if s.always.contains(name) || s.always.contains(ALL) {
            true
        } else {
            match &s.shown {
                None => true,
                Some(_) if s.asleep => false,
                Some(p) => s.pages.get(p).is_some_and(|set| set.contains(name) || set.contains(ALL)),
            }
        };
        (shown, s.generation)
    })
}

fn analyse(doc: &Document, remotes: &BTreeMap<String, Vec<String>>) -> (BTreeMap<String, BTreeSet<String>>, BTreeSet<String>) {
    let components: BTreeMap<&str, &Block> = doc
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Block(b) if b.kind.text == "component" => b.name.as_ref().map(|n| (n.text.as_str(), b)),
            _ => None,
        })
        .collect();
    let mut shell = BTreeSet::new();
    if let Some(b) = components.get("shell") {
        walk(&b.items, &components, remotes, &mut Vec::new(), &mut shell);
    }
    let mut pages = BTreeMap::new();
    let mut always = BTreeSet::new();
    for it in &doc.items {
        let Item::Block(b) = it else { continue };
        match b.kind.text.as_str() {
            "page" => {
                let Some(name) = &b.name else { continue };
                let mut set = shell.clone();
                walk(&b.items, &components, remotes, &mut vec!["shell"], &mut set);
                pages.insert(name.text.clone(), set);
            }
            // What an action shows in its dialogs (`confirm`, `ask`
            // options, `done`) and uses: kept fresh like alerts.
            "action" => walk(&b.items, &components, remotes, &mut Vec::new(), &mut always),
            "alert" => {
                walk(&b.items, &components, remotes, &mut Vec::new(), &mut always);
                // A remote kind's alert reads its instance as `self`:
                // the members it names (`self.temp`), not all of them.
                if let Some(inst) = paneld_config::remote::remote_of(b) {
                    let mut with_self = remotes.clone();
                    with_self.insert("self".into(), remotes.get(inst).cloned().unwrap_or_default());
                    let mut read = BTreeSet::new();
                    walk(&b.items, &components, &with_self, &mut Vec::new(), &mut read);
                    for name in read {
                        match name.strip_prefix("self.") {
                            Some(m) => always.insert(format!("{inst}.{m}")),
                            None => always.insert(name),
                        };
                    }
                }
                // `from = <source>`: an event alert reads its stream.
                for inner in &b.items {
                    if let Item::Assign(a) = inner {
                        if matches!(&a.target, paneld_config::parse::AssignTarget::Simple { key } if key.text == "from") {
                            always.insert(a.value.text.trim().to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (pages, always)
}

/// Collect the sources read under `items`, going into components
/// (each once per path, so a component that uses itself ends).
fn walk<'a>(
    items: &'a [Item],
    components: &BTreeMap<&'a str, &'a Block>,
    remotes: &BTreeMap<String, Vec<String>>,
    path: &mut Vec<&'a str>,
    out: &mut BTreeSet<String>,
) {
    for it in items {
        match it {
            Item::Assign(a) => value_refs(&a.value, remotes, out),
            Item::Repeat(r) => {
                if let (Some(e), _) = cel::parse_fragment(&r.source.body, r.source.span.file, r.source.span.start) {
                    expr_refs(&e, remotes, out);
                }
                walk(&r.items, components, remotes, path, out);
            }
            Item::Block(b) => {
                walk(&b.items, components, remotes, path, out);
                if let Some(c) = components.get(b.kind.text.as_str()) {
                    if !path.contains(&b.kind.text.as_str()) {
                        path.push(&b.kind.text);
                        walk(&c.items, components, remotes, path, out);
                        path.pop();
                    }
                }
            }
            Item::Param(p) => {
                if let Some(d) = &p.default {
                    value_refs(d, remotes, out);
                }
                // `options = {{ sources.x.value }}` of an asked param.
                if let Some(o) = &p.options {
                    walk(&o.items, components, remotes, path, out);
                }
            }
            Item::Var(_) => {}
        }
    }
}

fn value_refs(v: &RawValue, remotes: &BTreeMap<String, Vec<String>>, out: &mut BTreeSet<String>) {
    let exprs: Vec<Expr> = match classify(&v.text) {
        ValueShape::Literal(_) => return,
        ValueShape::Expression(body) => cel::parse_fragment(body, v.span.file, v.span.start).0.into_iter().collect(),
        ValueShape::Interp(_) => cel::parse_interp(&v.text, v.span.file, v.span.start).0,
    };
    for e in &exprs {
        expr_refs(e, remotes, out);
    }
}

/// `sources.x` and `sources['x']` read `x`; `sources` otherwise (a
/// computed name, passed whole) reads every source. `device1.temp`
/// and `device1.status.temp` read that member; `device1` otherwise
/// (`device1.ok`, passed whole) reads all its members.
fn expr_refs(e: &Expr, remotes: &BTreeMap<String, Vec<String>>, out: &mut BTreeSet<String>) {
    let ident = |e: &Expr| match &e.kind {
        ExprKind::Ident(n) => Some(n.clone()),
        _ => None,
    };
    let all_of = |r: &str, out: &mut BTreeSet<String>| {
        for m in &remotes[r] {
            out.insert(format!("{r}.{m}"));
        }
    };
    match &e.kind {
        ExprKind::Ident(n) if n == "sources" => {
            out.insert(ALL.to_string());
        }
        ExprKind::Ident(n) if remotes.contains_key(n) => all_of(n, out),
        ExprKind::Field(inner, field) if ident(inner).as_deref() == Some("sources") => {
            out.insert(field.clone());
        }
        ExprKind::Index(inner, idx) if ident(inner).as_deref() == Some("sources") => match &idx.kind {
            ExprKind::StringLit(name) => {
                out.insert(name.clone());
            }
            _ => {
                out.insert(ALL.to_string());
                expr_refs(idx, remotes, out);
            }
        },
        ExprKind::Field(inner, field) if ident(inner).is_some_and(|r| remotes.contains_key(&r)) => {
            let r = ident(inner).unwrap();
            if remotes[&r].contains(field) {
                out.insert(format!("{r}.{field}"));
            } else {
                all_of(&r, out);
            }
        }
        // `device1.status.temp`
        ExprKind::Field(inner, member) if matches!(&inner.kind, ExprKind::Field(r, s) if s == "status" && ident(r).is_some_and(|r| remotes.contains_key(&r))) => {
            let ExprKind::Field(r, _) = &inner.kind else { unreachable!() };
            let r = ident(r).unwrap();
            if remotes[&r].contains(member) {
                out.insert(format!("{r}.{member}"));
            } else {
                all_of(&r, out);
            }
        }
        ExprKind::Field(a, _) | ExprKind::Unary(_, a) => expr_refs(a, remotes, out),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
            expr_refs(a, remotes, out);
            expr_refs(b, remotes, out);
        }
        ExprKind::Conditional(a, b, c) => {
            expr_refs(a, remotes, out);
            expr_refs(b, remotes, out);
            expr_refs(c, remotes, out);
        }
        ExprKind::Call(_, args) | ExprKind::List(args) => args.iter().for_each(|a| expr_refs(a, remotes, out)),
        ExprKind::Method(a, _, args) => {
            expr_refs(a, remotes, out);
            args.iter().for_each(|x| expr_refs(x, remotes, out));
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            expr_refs(k, remotes, out);
            expr_refs(v, remotes, out);
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> Document {
        let mut map = paneld_config::SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = paneld_config::lex::tokenize(id, src);
        let (d, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        d
    }

    fn set(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    fn remotes(r: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        r.iter().map(|(n, ms)| (n.to_string(), ms.iter().map(|m| m.to_string()).collect())).collect()
    }

    #[test]
    fn pages_read_their_own_components_and_the_shell() {
        let d = doc(
            "component shell { column { text { value = {{ sources.clock.value }} }\n slot { } } }\n\
             component tile { param v: number\n text { value = {{ string(v) + sources.unit.value }} } }\n\
             component loop { loop { } }\n\
             page a { tile { v = {{ sources.cpu.value }} }\n loop { } }\n\
             page b { repeat x in {{ sources.vms.value }} { text { value = {{ d1.temp }} {{ d1.status.load.ok }} {{ d1.ok }} } } }\n\
             alert hot { when = {{ sources.temp.value > 70 }} }\n\
             alert ev { from = log\n when = {{ event.x }} }\n\
             page c { text { value = {{ sources['idx'].value }} {{ d2.ok }} } }\n\
             page d { text { value = {{ sources[vars.name].value }} } }\n\
             page e { text { value = {{ size(sources) }} } }\n\
             action ask { param u: string { ask = select; options = {{ sources.units.value }} }\n confirm = {{ sources.vm.value }}? }\n\
             alert d1.hot { when = {{ self.temp > 80 }}\n remote = d1 }\n",
        );
        let (pages, always) = analyse(&d, &remotes(&[("d1", &["temp", "load"]), ("d2", &["a", "b"])]));
        assert_eq!(pages["a"], set(&["clock", "cpu", "unit"]));
        // `d1.ok` alone reads all of d1.
        assert_eq!(pages["b"], set(&["clock", "d1.load", "d1.temp", "vms"]));
        assert_eq!(pages["c"], set(&["clock", "d2.a", "d2.b", "idx"]));
        assert_eq!(pages["d"], set(&["*", "clock"]));
        assert_eq!(pages["e"], set(&["*", "clock"]));
        // A kind alert: only the member it reads, `self.temp`.
        assert_eq!(always, set(&["d1.temp", "log", "temp", "units", "vm"]));
    }

    #[test]
    fn what_is_shown_follows_the_page_and_sleep() {
        let d = doc("page a { text { value = {{ sources.x.value }} } }\npage b { text { value = y } }\nalert al { when = {{ sources.z.value > 1 }} }\n");
        install(&d, &BTreeMap::new());
        with(|s| s.shown = None);
        set_asleep(false);
        assert!(is_shown("x").0 && is_shown("y").0, "before a page is drawn, all are");
        show("a");
        assert!(is_shown("x").0 && !is_shown("y").0);
        show("b");
        assert!(!is_shown("x").0);
        assert!(is_shown("z").0, "an alert's source always is");
        show("a");
        set_asleep(true);
        assert!(!is_shown("x").0 && is_shown("z").0);
        set_asleep(false);
        show("nope");
        assert!(!is_shown("x").0);
        // A whole-`sources` read by an alert keeps every source going.
        install(&doc("alert a { when = {{ size(sources) > 1 }} }\npage p { }\n"), &BTreeMap::new());
        show("p");
        assert!(is_shown("anything").0);
        // A change between looking and waiting is not slept through.
        let (_, gen) = is_shown("anything");
        notify();
        let t = Instant::now();
        wait(t + std::time::Duration::from_secs(5), gen);
        assert!(t.elapsed() < std::time::Duration::from_secs(1));
    }
}
