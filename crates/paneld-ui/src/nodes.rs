//! The node table of the page on screen (ADR 0024): what each node of
//! the config made, what it reads, and the walk that updates it in
//! place when what it reads changes, instead of building the page
//! again.
//!
//! A fresh render records, in render order, one entry per widget node
//! (its LVGL objects, each as it was made), per component call and per
//! `repeat`, with the dep keys of what it reads. An update walks the
//! page with the same render code: a subtree that reads nothing that
//! changed is skipped, a node whose inputs changed is set up again
//! from what it was made as (so it ends as a fresh render would make
//! it), and the rest is walked through. Anything the walk cannot do in
//! place (another shape, another list of `repeat` items) marks the
//! table broken, and the caller builds the page again.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use paneld_config::cel::{Expr, ExprKind};
use paneld_config::reads::Read;

use crate::ui::{Obj, Pristine};

/// One thing an expression reads that can change while a page is up.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dep {
    /// `sources.x.f`; `None`: the source read whole.
    Source(String, Option<String>),
    /// `sources` whole or by a computed name.
    AnySource,
    /// A remote instance's members (`device1.temp`: sources `device1.*`).
    Remote(String),
    /// `vars.y`; `None`: `vars` whole.
    Var(Option<String>),
    Alerts,
    NowMinute,
    NowSecond,
    /// An `ago()` text, run again when it is due.
    Ago,
    /// The state of a call of this action (a button's `:running`);
    /// `None`: of any action (a name from an expression).
    Call(Option<String>),
}

/// What changed since the page was last drawn or updated: the typed
/// change names of the main loop (ADR 0024, "Change set").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// Source name to the fields that changed (`*`: all of them).
    pub sources: BTreeMap<String, BTreeSet<String>>,
    /// Var names (`*`: all).
    pub vars: BTreeSet<String>,
    pub alerts: bool,
    pub now_minute: bool,
    pub now_second: bool,
    pub ago: bool,
    /// Actions whose call state changed (`*`: any).
    pub calls: BTreeSet<String>,
}

const ALL: &str = "*";

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Changes::default()
    }

    /// Fields `fields` of source `name` changed.
    pub fn source<I: IntoIterator<Item = S>, S: Into<String>>(&mut self, name: &str, fields: I) {
        let set = self.sources.entry(name.to_string()).or_default();
        set.extend(fields.into_iter().map(Into::into));
    }

    /// Source `name` changed, any field.
    pub fn source_all(&mut self, name: &str) {
        self.source(name, [ALL]);
    }

    pub fn var(&mut self, name: &str) {
        self.vars.insert(name.to_string());
    }

    pub fn all_vars(&mut self) {
        self.vars.insert(ALL.into());
    }

    pub fn call(&mut self, action: &str) {
        self.calls.insert(action.to_string());
    }

    /// Everything in `other` too.
    pub fn merge(&mut self, other: Changes) {
        for (k, v) in other.sources {
            self.sources.entry(k).or_default().extend(v);
        }
        self.vars.extend(other.vars);
        self.calls.extend(other.calls);
        self.alerts |= other.alerts;
        self.now_minute |= other.now_minute;
        self.now_second |= other.now_second;
        self.ago |= other.ago;
    }

    /// Whether a reader of `dep` must look again.
    pub fn hits(&self, dep: &Dep) -> bool {
        let field = |name: &str, f: &str| {
            self.sources.get(name).is_some_and(|s| s.contains(ALL) || s.contains(f)) || (f == "age" && self.ago)
        };
        match dep {
            Dep::Source(n, None) => self.sources.contains_key(n) || self.ago,
            Dep::Source(n, Some(f)) => field(n, f),
            Dep::AnySource => !self.sources.is_empty() || self.ago,
            Dep::Remote(inst) => {
                let prefix = format!("{inst}.");
                self.ago || self.sources.keys().any(|k| k.starts_with(&prefix) || k == inst)
            }
            Dep::Var(None) => !self.vars.is_empty(),
            Dep::Var(Some(v)) => self.vars.contains(v) || self.vars.contains(ALL),
            Dep::Alerts => self.alerts,
            Dep::NowMinute => self.now_minute,
            Dep::NowSecond => self.now_second || self.now_minute,
            Dep::Ago => self.ago,
            Dep::Call(None) => !self.calls.is_empty(),
            Dep::Call(Some(a)) => self.calls.contains(a) || self.calls.contains(ALL),
        }
    }

    fn hits_any(&self, deps: &[Dep]) -> bool {
        deps.iter().any(|d| self.hits(d))
    }

    /// A short form for the perf line and logs (`sources.x.value`, ...).
    pub fn describe(&self) -> String {
        let mut out: Vec<String> = Vec::new();
        for (k, v) in &self.sources {
            out.extend(v.iter().map(|f| format!("sources.{k}.{f}")));
        }
        out.extend(self.vars.iter().map(|v| format!("vars.{v}")));
        if self.alerts {
            out.push("alerts".into());
        }
        if self.now_minute {
            out.push("now:minute".into());
        }
        if self.now_second {
            out.push("now:second".into());
        }
        if self.ago {
            out.push("ago".into());
        }
        out.extend(self.calls.iter().map(|c| format!("call:{c}")));
        out.join(",")
    }
}

/// Sorted, without repeats.
pub(crate) fn union(into: &mut Vec<Dep>, more: &[Dep]) {
    if more.is_empty() {
        return;
    }
    into.extend_from_slice(more);
    into.sort();
    into.dedup();
}

// ---------------------------------------------------------------------------
// What an expression reads, as dep keys
// ---------------------------------------------------------------------------

/// Names bound around the node being drawn (component params, a
/// `repeat` binding, `page` / `nav`), each with the dep keys of what
/// its value was made from.
struct Frame {
    names: Vec<(String, Rc<Vec<Dep>>)>,
    parent: Option<usize>,
}

thread_local! {
    static FRAMES: std::cell::RefCell<Vec<Frame>> = const { std::cell::RefCell::new(Vec::new()) };
    static CUR: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// The reads of each parsed expression (by the address of its
    /// cached parse), kept as long as the parse cache keeps it.
    static READS: std::cell::RefCell<std::collections::HashMap<usize, Rc<Vec<Read>>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Forget the reads of parses dropped from the cache (a reload).
pub(crate) fn clear_reads() {
    READS.with(|r| r.borrow_mut().clear());
}

/// Puts the frame in place before it back when dropped.
pub(crate) struct FrameGuard(Option<usize>);

impl Drop for FrameGuard {
    fn drop(&mut self) {
        CUR.with(|c| c.set(self.0));
    }
}

/// Bind `names` (with their deps) over the current frame.
pub(crate) fn push_frame(names: Vec<(String, Vec<Dep>)>) -> FrameGuard {
    let before = CUR.with(|c| c.get());
    let idx = FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        f.push(Frame { names: names.into_iter().map(|(n, d)| (n, Rc::new(d))).collect(), parent: before });
        f.len() - 1
    });
    CUR.with(|c| c.set(Some(idx)));
    FrameGuard(before)
}

/// The current frame, to go back to (a `slot` draws in its caller's).
pub(crate) fn current_frame() -> Option<usize> {
    CUR.with(|c| c.get())
}

/// Make `frame` current until the guard drops.
pub(crate) fn switch_frame(frame: Option<usize>) -> FrameGuard {
    let before = CUR.with(|c| c.replace(frame));
    FrameGuard(before)
}

fn reset_frames() {
    FRAMES.with(|f| f.borrow_mut().clear());
    CUR.with(|c| c.set(None));
}

/// The deps of local `name` as bound where the walk is, if bound.
fn local_deps(name: &str) -> Option<Rc<Vec<Dep>>> {
    FRAMES.with(|f| {
        let f = f.borrow();
        let mut at = CUR.with(|c| c.get());
        while let Some(i) = at {
            let frame = &f[i];
            if let Some((_, d)) = frame.names.iter().find(|(n, _)| n == name) {
                return Some(d.clone());
            }
            at = frame.parent;
        }
        None
    })
}

/// Every local bound where the walk is: what an inline handler's
/// binding carries along.
pub(crate) fn all_local_deps() -> Vec<Dep> {
    let mut out = Vec::new();
    FRAMES.with(|f| {
        let f = f.borrow();
        let mut at = CUR.with(|c| c.get());
        while let Some(i) = at {
            for (_, d) in &f[i].names {
                union(&mut out, d);
            }
            at = f[i].parent;
        }
    });
    out
}

/// Roots that are neither locals nor remote names, and constant while
/// a page is up (`theme`) or not read by it (`value`, `event`: a
/// handler's, evaluated when it fires).
const GLOBALS: &[&str] = &["theme", "value", "event", "secrets"];

/// Add the deps of one read to `out`. A read the analysis does not
/// understand gets the widest key of its root.
fn read_deps(r: &Read, out: &mut Vec<Dep>) {
    let step = |i: usize| r.path.get(i).map(|s| s.name.clone());
    match r.root.as_str() {
        "sources" => out.push(match (step(0), step(1)) {
            (None, _) => Dep::AnySource,
            (Some(n), Some(f)) => Dep::Source(n, Some(f)),
            (Some(n), None) => Dep::Source(n, None),
        }),
        "vars" => out.push(Dep::Var(step(0))),
        "alerts" => out.push(Dep::Alerts),
        "now" => {
            let second = step(0).is_none_or(|f| paneld_config::cel::NOW_SECOND_FIELDS.contains(&f.as_str()));
            out.push(if second { Dep::NowSecond } else { Dep::NowMinute });
        }
        root => match local_deps(root) {
            Some(d) => out.extend(d.iter().cloned()),
            None if GLOBALS.contains(&root) => {}
            None => out.push(Dep::Remote(root.to_string())),
        },
    }
}

/// `ago(...)` somewhere in `e`.
fn calls_ago(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Call(name, args) => name == "ago" || args.iter().any(calls_ago),
        ExprKind::Field(a, _) | ExprKind::Unary(_, a) => calls_ago(a),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => calls_ago(a) || calls_ago(b),
        ExprKind::Conditional(a, b, c) => calls_ago(a) || calls_ago(b) || calls_ago(c),
        ExprKind::Method(a, _, args) => calls_ago(a) || args.iter().any(calls_ago),
        ExprKind::List(xs) => xs.iter().any(calls_ago),
        ExprKind::Map(pairs) => pairs.iter().any(|(k, v)| calls_ago(k) || calls_ago(v)),
        _ => false,
    }
}

/// The deps of parsed expression `e` (cached under `key`, the address
/// of its parse) where the walk is now.
pub(crate) fn expr_deps(key: usize, e: &Expr, out: &mut Vec<Dep>) {
    let reads = READS.with(|r| {
        r.borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                let mut reads = paneld_config::reads::reads(e);
                if calls_ago(e) {
                    // `ago` is a function, not a read: marked by a root
                    // no config can use.
                    reads.push(Read { root: "@ago".into(), root_span: e.span, path: Vec::new(), computed: false });
                }
                Rc::new(reads)
            })
            .clone()
    });
    for r in reads.iter() {
        if r.root == "@ago" {
            out.push(Dep::Ago);
        } else {
            read_deps(r, out);
        }
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A widget node: owns its objects.
    Node,
    /// A component call: its own keys and handlers act on the root.
    Call,
    /// A `repeat`: its items' keys.
    Repeat,
}

/// One object a node made, as it was made, and by which maker.
pub(crate) struct Made {
    pub obj: *mut lvgl_sys::lv_obj_t,
    pristine: Pristine,
    tag: &'static str,
    sig: u64,
}

pub(crate) struct Rec {
    kind: Kind,
    /// The address of its `Block` (or `Repeat`) in the document drawn:
    /// valid as long as the table (a reload drops both).
    block: usize,
    pub made: Vec<Made>,
    own: Vec<Dep>,
    sub: Vec<Dep>,
    /// The index past its subtree.
    end: usize,
    /// What its inputs evaluated to when it was set up last.
    fp: Option<u64>,
    /// A `repeat`'s item keys.
    pub keys: Vec<String>,
    /// When an `ago()` text it shows reads otherwise (Unix seconds).
    ago: Option<i64>,
    /// Its kind and place in the config, for messages.
    what: String,
    place: String,
}

struct Table {
    page: String,
    recs: Vec<Rec>,
    holder: *mut lvgl_sys::lv_obj_t,
    page_root: *mut lvgl_sys::lv_obj_t,
    /// Something on the page cannot be updated in place: every change
    /// builds it again.
    broken: Option<&'static str>,
    /// When an `ago()` in a page title reads otherwise.
    page_ago: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Drawing something else (a toast): nothing recorded.
    Off,
    /// A fresh render of the page on screen: recorded.
    Build,
    /// Walking the page on screen to update it.
    Update,
    /// A fresh render to compare with (verify): nothing recorded,
    /// nothing remembered.
    Scratch,
}

thread_local! {
    static TABLE: std::cell::RefCell<Option<Table>> = const { std::cell::RefCell::new(None) };
    static MODE: std::cell::Cell<Mode> = const { std::cell::Cell::new(Mode::Off) };
    /// Build: the records open now (innermost last).
    static OPEN: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Update: the next record to visit.
    static CURSOR: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Update: the nodes being set up again, with the next of their
    /// objects to reuse.
    static REUSE: std::cell::RefCell<Vec<(usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
    static CHANGES: std::cell::RefCell<Changes> = std::cell::RefCell::new(Changes::default());
    /// Update: nodes set up again in this walk.
    static REAPPLIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Objects a page has made, since start (a fresh render's, a
    /// running button's overlay): an update that makes none keeps
    /// every object it had.
    static MADE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Update: where objects of another shape are made, off the page.
    static DOOMED: std::cell::Cell<*mut lvgl_sys::lv_obj_t> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

pub(crate) fn count_made(n: u64) {
    MADE.with(|m| m.set(m.get() + n));
}

/// Objects pages have made since start (`ctl status`, perf lines).
pub fn made_count() -> u64 {
    MADE.with(|m| m.get())
}

pub(crate) fn mode() -> Mode {
    MODE.with(|m| m.get())
}

/// Run `f` in `mode`, putting the mode before back after.
pub(crate) fn with_mode<R>(mode: Mode, f: impl FnOnce() -> R) -> R {
    let before = MODE.with(|m| m.replace(mode));
    let r = f();
    MODE.with(|m| m.set(before));
    r
}

/// Start recording a fresh render of `page`.
pub(crate) fn begin_build(page: &str) {
    reset_frames();
    OPEN.with(|o| o.borrow_mut().clear());
    TABLE.with(|t| {
        *t.borrow_mut() = Some(Table {
            page: page.to_string(),
            recs: Vec::new(),
            holder: std::ptr::null_mut(),
            page_root: std::ptr::null_mut(),
            broken: None,
            page_ago: None,
        })
    });
}

/// The page's holder and root, once made.
pub(crate) fn set_roots(holder: &Obj<'_>, page_root: &Obj<'_>) {
    if mode() != Mode::Build {
        return;
    }
    with_table(|t| {
        t.holder = holder.raw();
        t.page_root = page_root.raw();
    });
}

fn with_table<R: Default>(f: impl FnOnce(&mut Table) -> R) -> R {
    TABLE.with(|t| t.borrow_mut().as_mut().map(f).unwrap_or_default())
}

/// Forget the table (the screen was cleared, a reload, a theme switch).
pub fn forget() {
    TABLE.with(|t| *t.borrow_mut() = None);
}

/// The page can only be built again from now on, because of `why`.
pub(crate) fn mark_broken(why: &'static str) {
    if matches!(mode(), Mode::Build | Mode::Update) {
        with_table(|t| {
            t.broken.get_or_insert(why);
        });
    }
}

/// Build: open a record for `block` (its address) of `kind` reading
/// `own`; its objects and inner records follow until [`close`].
pub(crate) fn open(kind: Kind, block: usize, own: Vec<Dep>, what: &str, span: paneld_config::source::Span) -> usize {
    let idx = with_table(|t| {
        let place = format!("{:?} byte {}", span.file, span.start);
        t.recs.push(Rec { kind, block, made: Vec::new(), sub: own.clone(), own, end: 0, fp: None, keys: Vec::new(), ago: None, what: what.to_string(), place });
        t.recs.len() - 1
    });
    OPEN.with(|o| o.borrow_mut().push(idx));
    idx
}

/// Build: the record `idx` and what is under it are complete.
pub(crate) fn close(idx: usize) {
    OPEN.with(|o| {
        o.borrow_mut().pop();
    });
    let parent = OPEN.with(|o| o.borrow().last().copied());
    with_table(|t| {
        let end = t.recs.len();
        t.recs[idx].end = end;
        if let Some(p) = parent {
            let sub = std::mem::take(&mut t.recs[idx].sub);
            union(&mut t.recs[p].sub, &sub);
            t.recs[idx].sub = sub;
        }
    });
}

/// Build: a `repeat`'s item keys.
pub(crate) fn set_keys(idx: usize, keys: Vec<String>) {
    with_table(|t| t.recs[idx].keys = keys);
}

/// Make (or, setting a node up again, reuse) an object: `make` builds
/// it under `parent`; `reuse` sets on the object the node made before
/// what `make` would have given it (a label's text). `tag` and `sig`
/// say what was made and with which arguments `reuse` cannot change:
/// another one breaks the table.
pub(crate) fn made<'u>(
    parent: &Obj<'u>,
    tag: &'static str,
    sig: u64,
    make: impl FnOnce(&Obj<'u>) -> Obj<'u>,
    reuse: impl FnOnce(&Obj<'u>),
) -> Obj<'u> {
    match mode() {
        Mode::Build => {
            let obj = make(parent);
            if !obj.raw().is_null() {
                count_made(1);
            }
            let owner = OPEN.with(|o| o.borrow().last().copied()).filter(|_| !obj.raw().is_null());
            if let Some(owner) = owner {
                let pristine = obj.pristine();
                with_table(|t| t.recs[owner].made.push(Made { obj: obj.raw(), pristine, tag, sig }));
            }
            obj
        }
        Mode::Update => {
            let next = REUSE.with(|r| {
                let mut r = r.borrow_mut();
                let (idx, pos) = r.last_mut()?;
                let at = *pos;
                *pos += 1;
                Some((*idx, at))
            });
            let found = next.and_then(|(idx, at)| {
                TABLE.with(|t| {
                    let t = t.borrow();
                    let m = t.as_ref()?.recs.get(idx)?.made.get(at)?;
                    (m.tag == tag && m.sig == sig).then_some(m.obj)
                })
            });
            match found {
                Some(raw) => {
                    let obj = Obj::from_raw(raw);
                    TABLE.with(|t| {
                        let t = t.borrow();
                        if let Some((idx, at)) = next {
                            if let Some(m) = t.as_ref().and_then(|t| t.recs.get(idx)).and_then(|r| r.made.get(at)) {
                                obj.restore(&m.pristine);
                            }
                        }
                    });
                    reuse(&obj);
                    obj
                }
                None => {
                    // Another shape: the page is built again. Until then
                    // (a dialog may hold it off) the page shows what it
                    // did: this one is made off it, and deleted when the
                    // walk ends.
                    mark_broken("a node changed shape");
                    let doomed = DOOMED.with(|d| {
                        if d.get().is_null() {
                            d.set(Obj::new_scratch_screen().raw());
                        }
                        d.get()
                    });
                    make(&Obj::from_raw(doomed))
                }
            }
        }
        Mode::Off | Mode::Scratch => make(parent),
    }
}

/// [`made`] for a maker that may make nothing (a file that does not
/// load): nothing recorded then.
pub(crate) fn made_opt<'u>(
    parent: &Obj<'u>,
    tag: &'static str,
    sig: u64,
    make: impl FnOnce(&Obj<'u>) -> Option<Obj<'u>>,
    reuse: impl FnOnce(&Obj<'u>),
) -> Option<Obj<'u>> {
    match mode() {
        Mode::Build | Mode::Update => {
            let mut made_none = false;
            let obj = made(
                parent,
                tag,
                sig,
                |p| match make(p) {
                    Some(o) => o,
                    None => {
                        made_none = true;
                        Obj::from_raw(std::ptr::null_mut())
                    }
                },
                reuse,
            );
            if made_none {
                return None;
            }
            Some(obj)
        }
        Mode::Off | Mode::Scratch => make(parent),
    }
}

/// What the walk does with the record it reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Visit {
    /// Nothing under it changed: skipped.
    Skip,
    /// Something under it changed (or its own inputs may have): the
    /// caller looks closer.
    Look(usize),
}

/// Update: the record for `block` of `kind`, which must be the next
/// one; skipped with its subtree when nothing it reads changed.
/// `force`: looked at even so (a component call's root, whose call's
/// own keys changed).
pub(crate) fn visit(kind: Kind, block: usize, force: bool) -> Option<Visit> {
    let idx = CURSOR.with(|c| c.get());
    let ok = TABLE.with(|t| {
        let t = t.borrow();
        let t = t.as_ref()?;
        let r = t.recs.get(idx)?;
        (r.kind == kind && r.block == block).then_some((r.end, CHANGES.with(|c| c.borrow().hits_any(&r.sub))))
    });
    let Some((end, hit)) = ok else {
        mark_broken("the page's nodes are not where they were");
        return None;
    };
    VISITED.with(|v| v.set(v.get() + 1));
    if hit || force {
        CURSOR.with(|c| c.set(idx + 1));
        Some(Visit::Look(idx))
    } else {
        CURSOR.with(|c| c.set(end));
        Some(Visit::Skip)
    }
}

/// Update: whether what record `idx` itself reads changed.
pub(crate) fn own_hit(idx: usize) -> bool {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).is_some_and(|r| CHANGES.with(|c| c.borrow().hits_any(&r.own))))
}

/// Update: whether a call state record `idx` reads changed (no
/// fingerprint covers those).
pub(crate) fn call_hit(idx: usize) -> bool {
    TABLE.with(|t| {
        t.borrow().as_ref().and_then(|t| t.recs.get(idx)).is_some_and(|r| {
            CHANGES.with(|c| {
                let c = c.borrow();
                r.own.iter().any(|d| matches!(d, Dep::Call(_)) && c.hits(d))
            })
        })
    })
}

/// Update: record `idx`'s inputs now hash to `fp`; whether that is
/// another value than when it was set up last (keeps the new one).
pub(crate) fn fp_changed(idx: usize, fp: u64) -> bool {
    with_table(|t| match t.recs.get_mut(idx) {
        Some(r) => r.fp.replace(fp) != Some(fp),
        None => true,
    })
}

/// Update: set node `idx` up again; its objects are reused in order.
pub(crate) fn begin_reuse(idx: usize) {
    REUSE.with(|r| r.borrow_mut().push((idx, 0)));
    REAPPLIED.with(|n| n.set(n.get() + 1));
}

pub(crate) fn end_reuse(idx: usize) {
    let used = REUSE.with(|r| r.borrow_mut().pop());
    let all = with_table(|t| t.recs.get(idx).map(|r| r.made.len()));
    if used.map(|(i, n)| (i, Some(n))) != Some((idx, all)) {
        mark_broken("a node made other objects");
    }
}

/// Update: the walk is past record `idx` and its subtree.
pub(crate) fn leave(idx: usize) {
    let end = with_table(|t| t.recs.get(idx).map(|r| r.end).unwrap_or(0));
    if CURSOR.with(|c| c.get()) != end {
        mark_broken("the page's nodes are not where they were");
        CURSOR.with(|c| c.set(end));
    }
}

/// What record `idx` itself reads.
pub(crate) fn own_of(idx: usize) -> Vec<Dep> {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).map(|r| r.own.clone()).unwrap_or_default())
}

/// Whether record `idx` itself reads `dep`.
pub(crate) fn reads(idx: usize, dep: &Dep) -> bool {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).is_some_and(|r| r.own.contains(dep)))
}

/// When record `idx`'s `ago()` texts read otherwise.
pub(crate) fn set_ago(idx: usize, due: Option<i64>) {
    with_table(|t| {
        if let Some(r) = t.recs.get_mut(idx) {
            r.ago = due;
        }
    });
}

/// When the soonest `ago()` text on the page drawn reads otherwise.
pub(crate) fn page_ago() -> Option<i64> {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.iter().filter_map(|r| r.ago).chain(t.page_ago).min()))
}

/// When an `ago()` in the page's titles reads otherwise.
pub(crate) fn set_page_ago(due: Option<i64>) {
    if matches!(mode(), Mode::Build | Mode::Update) {
        with_table(|t| t.page_ago = due);
    }
}

/// Build: what the records open now read themselves (what a node
/// inherits from, as its text color).
pub(crate) fn ancestor_deps() -> Vec<Dep> {
    let open = OPEN.with(|o| o.borrow().clone());
    let mut out = Vec::new();
    TABLE.with(|t| {
        if let Some(t) = t.borrow().as_ref() {
            for i in open {
                union(&mut out, &t.recs[i].own);
            }
        }
    });
    out
}

/// The objects record `idx` made.
pub(crate) fn objs(idx: usize) -> Vec<*mut lvgl_sys::lv_obj_t> {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).map(|r| r.made.iter().map(|m| m.obj).collect()).unwrap_or_default())
}

/// A `repeat`'s item keys as drawn.
pub(crate) fn keys(idx: usize) -> Vec<String> {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).map(|r| r.keys.clone()).unwrap_or_default())
}

/// What an update walk needs to start: the page drawn, whole, and
/// the objects it was drawn in.
pub(crate) struct Start {
    pub page_root: *mut lvgl_sys::lv_obj_t,
}

/// Start an update walk of `page` for `changes`, if the table can do
/// it: `Err` says why not (the caller builds the page again).
pub(crate) fn begin_update(page: &str, changes: &Changes) -> Result<Option<Start>, &'static str> {
    let state = TABLE.with(|t| {
        let t = t.borrow();
        let Some(t) = t.as_ref() else { return Err("no page drawn") };
        if t.page != page {
            return Err("another page");
        }
        if let Some(why) = t.broken {
            return Err(why);
        }
        if t.holder.is_null() || t.page_root.is_null() {
            return Err("no page drawn");
        }
        let hit = t.recs.iter().any(|r| changes.hits_any(&r.own));
        Ok(hit.then_some(Start { page_root: t.page_root }))
    })?;
    if state.is_some() {
        reset_frames();
        CHANGES.with(|c| *c.borrow_mut() = changes.clone());
        CURSOR.with(|c| c.set(0));
        REUSE.with(|r| r.borrow_mut().clear());
        REAPPLIED.with(|n| n.set(0));
        VISITED.with(|n| n.set(0));
    }
    Ok(state)
}

/// End an update walk: `Err` when it found something it could not do
/// in place.
pub(crate) fn end_update() -> Result<(usize, usize), &'static str> {
    CHANGES.with(|c| *c.borrow_mut() = Changes::default());
    let doomed = DOOMED.with(|d| d.replace(std::ptr::null_mut()));
    if !doomed.is_null() {
        Obj::from_raw(doomed).delete();
    }
    let done = TABLE.with(|t| match t.borrow_mut().as_mut() {
        Some(t) => {
            if t.broken.is_none() && CURSOR.with(|c| c.get()) != t.recs.len() {
                t.broken = Some("the walk ended early");
            }
            t.broken.map_or(Ok(()), Err)
        }
        None => Err("no page drawn"),
    });
    done.map(|()| (VISITED.with(|v| v.get()), REAPPLIED.with(|n| n.get())))
}

/// The page holder of the page drawn (to compare it with a fresh one).
pub(crate) fn holder() -> Option<*mut lvgl_sys::lv_obj_t> {
    TABLE.with(|t| t.borrow().as_ref().map(|t| t.holder).filter(|h| !h.is_null()))
}

/// Which node made `obj` (for a verify message): its kind and place.
pub(crate) fn describe_obj(obj: *mut lvgl_sys::lv_obj_t) -> String {
    TABLE.with(|t| {
        let t = t.borrow();
        let Some(t) = t.as_ref() else { return String::new() };
        match t.recs.iter().find(|r| r.made.iter().any(|m| m.obj == obj)) {
            Some(r) => format!("made by `{}` at {}", r.what, r.place),
            None => "not made by a node of the page (a part of one, or the page itself)".to_string(),
        }
    })
}

/// Records and objects of the page drawn, for `ctl status`.
pub fn table_size() -> (usize, usize) {
    TABLE.with(|t| t.borrow().as_ref().map_or((0, 0), |t| (t.recs.len(), t.recs.iter().map(|r| r.made.len()).sum())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_hit_the_readers_of_what_changed() {
        let mut c = Changes::default();
        c.source("cpu", ["at", "history"]);
        assert!(c.hits(&Dep::Source("cpu".into(), Some("at".into()))));
        assert!(!c.hits(&Dep::Source("cpu".into(), Some("value".into()))), "a same-output poll");
        assert!(c.hits(&Dep::Source("cpu".into(), None)));
        assert!(c.hits(&Dep::AnySource));
        assert!(!c.hits(&Dep::Source("mem".into(), None)));
        assert!(!c.hits(&Dep::Var(None)));
        c.var("mode");
        assert!(c.hits(&Dep::Var(Some("mode".into()))) && c.hits(&Dep::Var(None)) && !c.hits(&Dep::Var(Some("x".into()))));
        let mut r = Changes::default();
        r.source_all("d1.temp");
        assert!(r.hits(&Dep::Remote("d1".into())) && !r.hits(&Dep::Remote("d2".into())));
        // The minute tick is a second tick too; `ago` moves every age.
        let mut m = Changes { now_minute: true, ..Default::default() };
        assert!(m.hits(&Dep::NowSecond) && m.hits(&Dep::NowMinute));
        m.now_minute = false;
        m.now_second = true;
        assert!(m.hits(&Dep::NowSecond) && !m.hits(&Dep::NowMinute));
        let a = Changes { ago: true, ..Default::default() };
        assert!(a.hits(&Dep::Source("x".into(), Some("age".into()))) && !a.hits(&Dep::Source("x".into(), Some("value".into()))));
        let mut k = Changes::default();
        k.call("vm_stop");
        assert!(k.hits(&Dep::Call(Some("vm_stop".into()))) && k.hits(&Dep::Call(None)) && !k.hits(&Dep::Call(Some("x".into()))));
        let mut all = Changes::default();
        all.merge(c);
        all.merge(k);
        assert_eq!(all.describe(), "sources.cpu.at,sources.cpu.history,vars.mode,call:vm_stop");
    }

    #[test]
    fn locals_carry_the_deps_of_what_they_were_made_from() {
        let f = paneld_config::source::FileId::default();
        let parse = |s: &str| paneld_config::cel::parse_fragment(s, f, 0).0.unwrap();
        reset_frames();
        let _page = push_frame(vec![("page".into(), vec![Dep::Var(Some("title".into()))])]);
        let outer = current_frame();
        let _call = push_frame(vec![("value".into(), vec![Dep::Source("cpu".into(), Some("value".into()))])]);
        let mut d = Vec::new();
        let e = parse("value > 1 && page.title != '' && vars.x && sources.y.ok && now.time != '' && ago(sources.y.at) != '' && d1.temp > 1");
        expr_deps(1, &e, &mut d);
        union(&mut d, &[]);
        let mut want = vec![
            Dep::Source("cpu".into(), Some("value".into())),
            Dep::Var(Some("title".into())),
            Dep::Var(Some("x".into())),
            Dep::Source("y".into(), Some("ok".into())),
            Dep::NowMinute,
            Dep::Source("y".into(), Some("at".into())),
            Dep::Remote("d1".into()),
            Dep::Ago,
        ];
        want.sort();
        d.sort();
        d.dedup();
        assert_eq!(d, want);
        // A slot draws in its caller's frame: the param is not bound there.
        {
            let _slot = switch_frame(outer);
            let mut d = Vec::new();
            expr_deps(2, &parse("value + dev.x"), &mut d);
            assert_eq!(d, vec![Dep::Remote("dev".into())]);
        }
        assert_eq!(all_local_deps().len(), 2);
        clear_reads();
    }
}
