//! `active = visible` on a source (schema 4): it polls only while a
//! page that shows it is on screen and the screen is awake. Which
//! sources a page shows is worked out at load from its expressions,
//! those of the components it uses and the shell; sources an alert
//! reads always count as shown (ADR 0013: alerts never go stale).
//! What an expression reads comes from `paneld_config::reads`, the
//! analysis `paneld check` uses too.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use paneld_config::cel;
use paneld_config::parse::{Block, Document, Item, RawValue};
use paneld_config::reads::{self, Read};

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

/// Marks a page that reads a field of `now` that changes each minute
/// (`now.time`). Not a source name: those have no `:`.
const NOW_MINUTE: &str = "now:minute";
/// Marks a page that reads `now.second`, `now.unix`, or `now` whole.
const NOW_SECOND: &str = "now:second";

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

/// The sources whose `age` the page on screen reads by name
/// (`sources.x.age`), timed by the main loop as `ago()` texts are.
/// Empty before a page is drawn and asleep.
pub fn ages_on_page() -> Vec<String> {
    with(|s| match &s.shown {
        Some(p) if !s.asleep => s
            .pages
            .get(p)
            .map(|set| set.iter().filter_map(|k| k.strip_suffix(":age").map(str::to_string)).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    })
}

/// How often the clock must redraw the page on screen: what it (with
/// its shell) reads of `now` (`ago()` texts are timed by the main loop). `None` when it reads no
/// time, before any page is drawn, and asleep (the wake redraws).
pub fn clock_on_page() -> Option<crate::clock::Tick> {
    use crate::clock::Tick;
    with(|s| match &s.shown {
        Some(p) if !s.asleep => {
            let set = s.pages.get(p)?;
            if set.contains(NOW_SECOND) {
                Some(Tick::Second)
            } else if set.contains(NOW_MINUTE) {
                Some(Tick::Minute)
            } else {
                None
            }
        }
        _ => None,
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
                    for read in reads::reads(&e) {
                        read_keys(&read, remotes, out);
                    }
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
    for r in reads::value_reads(v) {
        read_keys(&r, remotes, out);
    }
}

/// What one read (`paneld_config::reads`) needs polled or redrawn.
/// `sources.x` and `sources['x']` read `x`; `sources` otherwise (a
/// computed name, passed whole) reads every source. `device1.temp`
/// and `device1.status.temp` read that member; `device1` otherwise
/// (`device1.ok`, passed whole) reads all its members.
fn read_keys(r: &Read, remotes: &BTreeMap<String, Vec<String>>, out: &mut BTreeSet<String>) {
    let step = |i: usize| r.path.get(i).map(|s| s.name.as_str());
    // `age` and `running` of source `name`, when the read names the
    // field at `i`. A whole map (`has(sources.x)`, passed to a
    // component) does not count: those fields must be read by name to
    // be redrawn as they change (schema 4).
    let live = |name: &str, i: usize, out: &mut BTreeSet<String>| {
        if let Some(f @ ("age" | "running")) = step(i) {
            out.insert(format!("{name}:{f}"));
        }
    };
    match r.root.as_str() {
        "sources" => {
            out.insert(step(0).unwrap_or(ALL).to_string());
            if let Some(name) = step(0) {
                live(name, 1, out);
            }
        }
        "now" => {
            let second = step(0).is_none_or(|f| paneld_config::cel::NOW_SECOND_FIELDS.contains(&f));
            out.insert(if second { NOW_SECOND } else { NOW_MINUTE }.to_string());
        }
        root => {
            let Some(members) = remotes.get(root) else { return };
            let member = match (step(0), step(1)) {
                (Some("status"), Some(m)) if members.iter().any(|x| x == m) => Some((m, true)),
                (Some(m), _) if members.iter().any(|x| x == m) => Some((m, false)),
                _ => None,
            };
            match member {
                Some((m, status)) => {
                    out.insert(format!("{root}.{m}"));
                    if status {
                        live(&format!("{root}.{m}"), 2, out);
                    }
                }
                None => {
                    for m in members {
                        out.insert(format!("{root}.{m}"));
                    }
                }
            }
        }
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

    /// The tests that use the global state, one at a time.
    static GLOBAL: Mutex<()> = Mutex::new(());

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
    fn pages_that_read_the_clock_are_marked_by_how_often_it_changes() {
        use crate::clock::Tick;
        let d = doc(
            "component shell { column { slot { } } }\n\
             component clock { text { value = {{ now.time }} } }\n\
             page plain { text { value = {{ sources.x.value }} } }\n\
             page minute { clock { } }\n\
             page date { text { value = {{ now['iso_date'] }} } }\n\
             page since { text { value = {{ ago(sources.x.at) }} } }\n\
             page second { text { value = {{ now.second }} } }\n\
             page unix { text { value = {{ now.unix - 1 }} {{ now.time }} } }\n\
             page whole { text { value = {{ size(now) }} } }\n",
        );
        let (pages, _) = analyse(&d, &BTreeMap::new());
        assert_eq!(pages["plain"], set(&["x"]));
        assert_eq!(pages["minute"], set(&[NOW_MINUTE]));
        assert_eq!(pages["date"], set(&[NOW_MINUTE]));
        // `ago()` alone: the main loop times it by the texts shown.
        assert_eq!(pages["since"], set(&["x"]));
        assert_eq!(pages["second"], set(&[NOW_SECOND]));
        assert!(pages["unix"].contains(NOW_SECOND));
        assert_eq!(pages["whole"], set(&[NOW_SECOND]));
        // What the clock redraws follows the page shown and sleep.
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        install(&d, &BTreeMap::new());
        set_asleep(false);
        with(|s| s.shown = None);
        assert_eq!(clock_on_page(), None, "before a page is drawn");
        show("plain");
        assert_eq!(clock_on_page(), None);
        show("minute");
        assert_eq!(clock_on_page(), Some(Tick::Minute));
        show("unix");
        assert_eq!(clock_on_page(), Some(Tick::Second));
        set_asleep(true);
        assert_eq!(clock_on_page(), None, "asleep");
        set_asleep(false);
    }

    #[test]
    fn age_and_running_readers_are_marked() {
        let d = doc(
            "page a { text { value = {{ sources.x.age }} {{ sources.y.value }} } }\n\
             page b { text { value = {{ sources.x.running }} {{ d1.status.t.age }} } }\n\
             page c { text { value = {{ size(sources.x) }} {{ has(sources.x) }} } }\n\
             page e { text { value = {{ sources[vars.n].age }} } }\n",
        );
        let (pages, _) = analyse(&d, &remotes(&[("d1", &["t"])]));
        assert_eq!(pages["a"], set(&["x", "x:age", "y"]));
        assert_eq!(pages["b"], set(&["d1.t", "d1.t:age", "x", "x:running"]));
        // Read whole, not by name: no redraws for `age` / `running`.
        assert_eq!(pages["c"], set(&["x"]));
        assert_eq!(pages["e"], set(&["*"]));
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
        install(&d, &remotes(&[("d1", &["t"])]));
        set_asleep(false);
        show("a");
        assert_eq!(ages_on_page(), vec!["x".to_string()]);
        show("b");
        set_asleep(true);
        assert!(ages_on_page().is_empty(), "asleep");
        set_asleep(false);
        show("e");
        assert!(ages_on_page().is_empty(), "a computed name does not subscribe");
        set_asleep(false);
    }

    #[test]
    fn what_is_shown_follows_the_page_and_sleep() {
        let _g = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
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
        show("a");
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
