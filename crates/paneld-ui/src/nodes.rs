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
//! it), and the rest is walked through. A node that now makes other
//! objects (another shape) is built again where it was, alone; a
//! `repeat` whose items changed keeps the items it still lists (moved
//! into place, updated in place), builds the new ones and deletes the
//! rest. What the walk cannot do at all (a page that is not there)
//! marks the table broken, and the caller builds the page again.

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
    /// `alerts.f` (`count`, `shown`, ...); `None`: `alerts` whole.
    Alerts(Option<String>),
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
    /// Fields of `alerts` that changed (`*`: all).
    pub alerts: BTreeSet<String>,
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

    /// Field `field` of `alerts` changed (`count`, `shown`, ...).
    pub fn alert(&mut self, field: &str) {
        self.alerts.insert(field.to_string());
    }

    /// Everything in `other` too.
    pub fn merge(&mut self, other: Changes) {
        for (k, v) in other.sources {
            self.sources.entry(k).or_default().extend(v);
        }
        self.vars.extend(other.vars);
        self.calls.extend(other.calls);
        self.alerts.extend(other.alerts);
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
            Dep::Alerts(None) => !self.alerts.is_empty(),
            Dep::Alerts(Some(f)) => self.alerts.contains(f) || self.alerts.contains(ALL),
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
        out.extend(self.alerts.iter().map(|f| format!("alerts.{f}")));
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
    push_frame_rc(names.into_iter().map(|(n, d)| (n, Rc::new(d))).collect())
}

/// [`push_frame`] with deps as kept in a record.
pub(crate) fn push_frame_rc(names: Vec<(String, Rc<Vec<Dep>>)>) -> FrameGuard {
    let before = CUR.with(|c| c.get());
    let idx = FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        f.push(Frame { names, parent: before });
        f.len() - 1
    });
    CUR.with(|c| c.set(Some(idx)));
    FrameGuard(before)
}

/// The deps of local `name` where the walk is, as bound.
pub(crate) fn bound_deps(name: &str) -> Rc<Vec<Dep>> {
    local_deps(name).unwrap_or_default()
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
        "alerts" => out.push(Dep::Alerts(step(0))),
        "now" => {
            let second = step(0).is_none_or(|f| paneld_config::cel::NOW_SECOND_FIELDS.contains(&f.as_str()));
            out.push(if second { Dep::NowSecond } else { Dep::NowMinute });
        }
        root => match local_deps(root) {
            Some(d) => out.extend(d.iter().cloned()),
            None if GLOBALS.contains(&root) => {}
            None => out.push(remote_dep(root, r)),
        },
    }
}

/// A read of remote instance `inst` (schema 17): a member's value
/// (`dev.temp`) is its source's `value`, a member's status field
/// (`dev.status.temp.ok`) that field; `dev.ok`, `dev.error` and the
/// instance whole read every member (ADR 0024, phase 4: a poll of one
/// member, or a run starting, does not touch the readers of another).
fn remote_dep(inst: &str, r: &Read) -> Dep {
    let step = |i: usize| r.path.get(i).map(|s| s.name.as_str());
    match (step(0), step(1)) {
        (Some("status"), Some(m)) => Dep::Source(format!("{inst}.{m}"), step(2).map(str::to_string)),
        (None | Some("ok" | "error" | "status"), _) => Dep::Remote(inst.to_string()),
        (Some(m), _) => Dep::Source(format!("{inst}.{m}"), Some("value".into())),
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
    /// One item of a `repeat`: its key; what it draws follows.
    Item,
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
    /// A `repeat`'s item keys; an item's own key.
    pub keys: Vec<String>,
    /// A `repeat` that draws no items: why (the text it shows).
    failed: Option<String>,
    /// A component call's params, each with the deps of what it was
    /// made from: bound again when an update walks into it.
    params: Vec<(String, Rc<Vec<Dep>>)>,
    /// The keys motion knows its widgets by: forgotten when it goes.
    motion: Vec<u64>,
    /// When an `ago()` text it shows reads otherwise (Unix seconds).
    ago: Option<i64>,
    /// Its kind and place in the config, for messages.
    what: String,
    place: paneld_config::source::Span,
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
    /// Update: the nodes being set up again.
    static REUSE: std::cell::RefCell<Vec<Reuse>> = const { std::cell::RefCell::new(Vec::new()) };
    static CHANGES: std::cell::RefCell<Changes> = std::cell::RefCell::new(Changes::default());
    /// Update: nodes set up again in this walk.
    static REAPPLIED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Objects a page has made, since start (a fresh render's, a
    /// running button's overlay): an update that makes none keeps
    /// every object it had.
    static MADE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Update: screens of objects made off the page (another shape),
    /// deleted when the walk ends.
    static DOOMED: std::cell::RefCell<Vec<*mut lvgl_sys::lv_obj_t>> = const { std::cell::RefCell::new(Vec::new()) };
    /// Update: what a finger holds (ADR 0024, "Touch"): left as it
    /// is, and nothing is built again or deleted, until the release.
    static HOLD: std::cell::RefCell<Option<Vec<*mut lvgl_sys::lv_obj_t>>> = const { std::cell::RefCell::new(None) };
    /// Update: something waits for the release.
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Update: subtrees built again and `repeat`s that changed items.
    static RESTRUCTURED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        t.recs.push(Rec {
            kind,
            block,
            made: Vec::new(),
            sub: own.clone(),
            own,
            end: 0,
            fp: None,
            keys: Vec::new(),
            failed: None,
            params: Vec::new(),
            motion: Vec::new(),
            ago: None,
            what: what.to_string(),
            place: span,
        });
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

/// A `repeat`'s item keys (an item's own), and why it draws none.
pub(crate) fn set_keys(idx: usize, keys: Vec<String>, failed: Option<String>) {
    with_table(|t| {
        t.recs[idx].keys = keys;
        t.recs[idx].failed = failed;
    });
}

/// Build: a component call's params with their deps.
pub(crate) fn set_params(idx: usize, params: Vec<(String, Rc<Vec<Dep>>)>) {
    with_table(|t| t.recs[idx].params = params);
}

/// A component call's params as recorded.
pub(crate) fn params_of(idx: usize) -> Vec<(String, Rc<Vec<Dep>>)> {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).map(|r| r.params.clone()).unwrap_or_default())
}

/// Motion recorded `key` for the node being drawn.
pub(crate) fn note_motion(key: u64) {
    let owner = match mode() {
        Mode::Build => OPEN.with(|o| o.borrow().last().copied()),
        Mode::Update => REUSE.with(|r| r.borrow().last().map(|e| e.idx)),
        _ => None,
    };
    let Some(owner) = owner else { return };
    with_table(|t| {
        if let Some(r) = t.recs.get_mut(owner) {
            if !r.motion.contains(&key) {
                r.motion.push(key);
            }
        }
    });
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
            // Once one object is of another shape, the node is built
            // again: nothing more of it is reused.
            let next = REUSE.with(|r| {
                let mut r = r.borrow_mut();
                let e = r.last_mut()?;
                let at = e.pos;
                e.pos += 1;
                (!e.bad).then_some((e.idx, at))
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
                    // Another shape: the node is built again when its
                    // setup ends. Until then this one is made off the
                    // page, and deleted then.
                    // Each setup has its own screen for them: deleting one
                    // (its node built again) leaves what a setup around
                    // it still holds.
                    let doomed = REUSE.with(|r| {
                        let mut r = r.borrow_mut();
                        let e = r.last_mut()?;
                        e.bad = true;
                        if e.doomed.is_null() {
                            e.doomed = Obj::new_scratch_screen().raw();
                        }
                        Some(e.doomed)
                    });
                    let doomed = doomed.unwrap_or_else(|| {
                        mark_broken("a node changed shape");
                        let d = Obj::new_scratch_screen().raw();
                        DOOMED.with(|l| l.borrow_mut().push(d));
                        d
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
        OPEN.with(|o| o.borrow_mut().push(idx));
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

/// A node being set up again: the next of its objects to reuse,
/// whether one was of another shape, and where those were made.
pub(crate) struct Reuse {
    idx: usize,
    pos: usize,
    bad: bool,
    doomed: *mut lvgl_sys::lv_obj_t,
}

/// What a setup that found another shape made off the page.
pub(crate) struct Doomed(*mut lvgl_sys::lv_obj_t);

impl Doomed {
    /// Delete it now (before the node's own objects, which it may
    /// point at).
    pub(crate) fn delete(self) {
        if !self.0.is_null() {
            crate::ui::delete_objs(&[self.0]);
        }
    }

    /// Delete it when the walk ends (the node stays as it is for now).
    pub(crate) fn later(self) {
        if !self.0.is_null() {
            DOOMED.with(|l| l.borrow_mut().push(self.0));
        }
    }
}

/// Update: set node `idx` up again; its objects are reused in order.
pub(crate) fn begin_reuse(idx: usize) {
    REUSE.with(|r| r.borrow_mut().push(Reuse { idx, pos: 0, bad: false, doomed: std::ptr::null_mut() }));
    REAPPLIED.with(|n| n.set(n.get() + 1));
}

/// Update: node `idx` is set up; true when it made other objects than
/// it had (another shape): the caller builds it again. With what that
/// setup made off the page.
pub(crate) fn end_reuse(idx: usize) -> (bool, Doomed) {
    let used = REUSE.with(|r| r.borrow_mut().pop());
    let all = with_table(|t| t.recs.get(idx).map(|r| r.made.len()));
    match used {
        Some(e) if e.idx == idx => (e.bad || Some(e.pos) != all, Doomed(e.doomed)),
        other => {
            mark_broken("a node made other objects");
            (false, Doomed(other.map_or(std::ptr::null_mut(), |e| e.doomed)))
        }
    }
}

/// Update: the walk is past record `idx` and its subtree.
pub(crate) fn leave(idx: usize) {
    let end = with_table(|t| t.recs.get(idx).map(|r| r.end).unwrap_or(0));
    if CURSOR.with(|c| c.get()) != end {
        mark_broken("the page's nodes are not where they were");
        CURSOR.with(|c| c.set(end));
    }
    let top = OPEN.with(|o| o.borrow_mut().pop());
    if top != Some(idx) {
        mark_broken("the page's nodes are not where they were");
    }
}

/// Update: the walk passes over record `idx`'s subtree, left as it is
/// for the release: its `ago()` texts are not due before then (they
/// are timed again when it is set up).
pub(crate) fn skip_held(idx: usize) {
    let end = end_of(idx);
    with_table(|t| {
        let end = end.min(t.recs.len());
        for r in &mut t.recs[idx..end] {
            r.ago = None;
        }
    });
    CURSOR.with(|c| c.set(end));
    defer();
}

/// Update: a finger holds something on the page.
pub(crate) fn holding() -> bool {
    HOLD.with(|h| h.borrow().is_some())
}

/// Update: whether node `idx`'s objects are, or are under, what a
/// finger holds.
pub(crate) fn is_held(idx: usize) -> bool {
    let objs = objs(idx);
    HOLD.with(|h| h.borrow().as_ref().is_some_and(|held| objs.iter().any(|&o| crate::ui::is_within(o, held))))
}

/// Update: whether what a finger holds is under node `idx`'s object.
pub(crate) fn holds_under(idx: usize) -> bool {
    let Some(&obj) = objs(idx).first() else { return false };
    HOLD.with(|h| h.borrow().as_ref().is_some_and(|held| held.iter().any(|&o| crate::ui::is_within(o, &[obj]))))
}

/// Update: whether what a finger holds is one of the objects of record
/// `idx`'s subtree (a `repeat` item) or under one.
pub(crate) fn item_holds(idx: usize) -> bool {
    let objs: Vec<*mut lvgl_sys::lv_obj_t> = TABLE.with(|t| {
        let t = t.borrow();
        let Some(t) = t.as_ref() else { return Vec::new() };
        let end = t.recs.get(idx).map_or(idx, |r| r.end).min(t.recs.len());
        t.recs[idx..end].iter().flat_map(|r| r.made.iter().map(|m| m.obj)).collect()
    });
    HOLD.with(|h| h.borrow().as_ref().is_some_and(|held| held.iter().any(|&o| crate::ui::is_within(o, &objs))))
}

/// Update: something was left for the release (it is done then).
pub(crate) fn defer() {
    HELD.with(|h| h.set(true));
}

/// Record `idx` is set up when next looked at, whatever its inputs.
pub(crate) fn clear_fp(idx: usize) {
    with_table(|t| {
        if let Some(r) = t.recs.get_mut(idx) {
            r.fp = None;
        }
    });
}

/// The index past record `idx`'s subtree.
pub(crate) fn end_of(idx: usize) -> usize {
    with_table(|t| t.recs.get(idx).map(|r| r.end).unwrap_or(0))
}

pub(crate) fn cursor() -> usize {
    CURSOR.with(|c| c.get())
}

pub(crate) fn set_cursor(at: usize) {
    CURSOR.with(|c| c.set(at));
}

// ---------------------------------------------------------------------------
// Structure in place (ADR 0024 phase 2): records cut out and put back
// (a `repeat` item kept), subtrees built again where they were.
// ---------------------------------------------------------------------------

/// Records taken out of the table: their `end`s count from their start.
pub(crate) struct Segment(Vec<Rec>);

/// Update: cut records `start..end` (whole subtrees) out of the table.
/// What the walk is in (the records open) shrinks around them.
pub(crate) fn cut(start: usize, end: usize) -> Segment {
    let len = end - start;
    let open = OPEN.with(|o| o.borrow().clone());
    Segment(with_table(|t| {
        let mut seg: Vec<Rec> = t.recs.drain(start..end).collect();
        for r in &mut seg {
            r.end -= start;
        }
        for r in &mut t.recs[start..] {
            r.end -= len;
        }
        for &i in &open {
            if i < start {
                t.recs[i].end -= len;
            }
        }
        seg
    }))
}

/// Update: put `seg` back in at `pos`; what the walk is in grows
/// around it.
pub(crate) fn splice(pos: usize, seg: Segment) -> usize {
    let mut seg = seg.0;
    let m = seg.len();
    for r in &mut seg {
        r.end += pos;
    }
    let open = OPEN.with(|o| o.borrow().clone());
    with_table(|t| {
        for r in &mut t.recs[pos..] {
            r.end += m;
        }
        for &i in &open {
            if i < pos {
                t.recs[i].end += m;
            }
        }
        t.recs.splice(pos..pos, seg);
    });
    m
}

impl Segment {
    /// The items of a `repeat`'s records (cut from past the repeat's
    /// own), each by its key.
    pub(crate) fn into_items(self) -> Vec<(String, Segment)> {
        let mut out = Vec::new();
        let mut recs = self.0.into_iter().peekable();
        let mut at = 0;
        while let Some(first) = recs.next() {
            let end = first.end;
            let key = first.keys.first().cloned().unwrap_or_default();
            let mut seg = vec![first];
            while seg.len() < end - at {
                match recs.next() {
                    Some(r) => seg.push(r),
                    None => break,
                }
            }
            for r in &mut seg {
                r.end -= at;
            }
            at = end;
            out.push((key, Segment(seg)));
        }
        out
    }

    /// Delete what these records made; with `forget`, motion forgets
    /// what they showed (they are gone for good, not built again).
    pub(crate) fn discard(self, forget: bool) {
        let objs: Vec<*mut lvgl_sys::lv_obj_t> = self.0.iter().flat_map(|r| r.made.iter().map(|m| m.obj)).collect();
        crate::ui::delete_objs(&outermost(&objs));
        if forget {
            let keys: Vec<u64> = self.0.iter().flat_map(|r| r.motion.iter().copied()).collect();
            crate::motion::forget_keys(&keys);
        }
    }

    /// The text typed into the keypads it made, in order.
    pub(crate) fn typed(&self) -> Vec<String> {
        self.0.iter().flat_map(|r| r.made.iter().filter(|m| m.tag == "textarea").map(|m| crate::ui::textarea_text(m.obj))).collect()
    }
}

/// Put `typed` (from [`Segment::typed`]) back into the keypads records
/// `start..end` made, in order, when they are as many.
pub(crate) fn restore_typed(start: usize, end: usize, typed: &[String]) {
    let tas: Vec<*mut lvgl_sys::lv_obj_t> = TABLE.with(|t| {
        let t = t.borrow();
        let Some(t) = t.as_ref() else { return Vec::new() };
        t.recs[start..end.min(t.recs.len())].iter().flat_map(|r| r.made.iter().filter(|m| m.tag == "textarea").map(|m| m.obj)).collect()
    });
    if tas.len() == typed.len() {
        for (ta, text) in tas.into_iter().zip(typed) {
            if !text.is_empty() {
                crate::ui::set_textarea_text(ta, text);
            }
        }
    }
}

/// Update: delete what record `idx` itself made (a `repeat`'s text
/// saying why it draws no items).
pub(crate) fn discard_own(idx: usize) {
    let made = with_table(|t| t.recs.get_mut(idx).map(|r| std::mem::take(&mut r.made)).unwrap_or_default());
    let objs: Vec<*mut lvgl_sys::lv_obj_t> = made.iter().map(|m| m.obj).collect();
    crate::ui::delete_objs(&outermost(&objs));
}

/// Those of `objs` not under another of them: deleting these deletes
/// all, each once.
fn outermost(objs: &[*mut lvgl_sys::lv_obj_t]) -> Vec<*mut lvgl_sys::lv_obj_t> {
    objs.iter().copied().filter(|&o| !crate::ui::is_within(crate::ui::parent_of(o), objs)).collect()
}

/// Update: node `idx` (open) is built again: its records go, it is no
/// longer open. Returns what it had.
pub(crate) fn take_node(idx: usize) -> Segment {
    OPEN.with(|o| {
        let mut o = o.borrow_mut();
        if o.last() == Some(&idx) {
            o.pop();
        }
    });
    cut(idx, end_of(idx))
}

/// Update: open record `idx` again (built again in its place).
pub(crate) fn reopen(idx: usize) {
    OPEN.with(|o| o.borrow_mut().push(idx));
}

/// Update: delete the objects made off the page by setups that found
/// another shape and were left as they were (the walk is over).
fn clear_doomed() {
    let doomed = DOOMED.with(|d| std::mem::take(&mut *d.borrow_mut()));
    crate::ui::delete_objs(&doomed);
}

/// Update: start building records within an update (a subtree again,
/// new `repeat` items); they go in place with [`end_sub`].
pub(crate) fn begin_sub() -> usize {
    with_table(|t| t.recs.len())
}

/// Update: the records built since [`begin_sub`] (`start`) go in at
/// `pos`; what the walk is in reads what they read. The walk goes on
/// past them. Returns how many.
pub(crate) fn end_sub(start: usize, pos: usize) -> usize {
    let open = OPEN.with(|o| o.borrow().clone());
    let seg = with_table(|t| {
        let mut seg: Vec<Rec> = t.recs.drain(start..).collect();
        for r in &mut seg {
            r.end -= start;
        }
        let mut sub = Vec::new();
        for r in &seg {
            union(&mut sub, &r.sub);
            // Made as a fresh render makes them: restyled at the
            // update's end if it sets them after.
            for m in &r.made {
                crate::ui::touch(m.obj);
            }
        }
        for &i in &open {
            union(&mut t.recs[i].sub, &sub);
        }
        seg
    });
    let m = splice(pos, Segment(seg));
    CURSOR.with(|c| c.set(pos + m));
    RESTRUCTURED.with(|n| n.set(n.get() + 1));
    m
}

/// Update: a `repeat` changed its items in place.
pub(crate) fn note_restructured() {
    RESTRUCTURED.with(|n| n.set(n.get() + 1));
}

/// The objects of records `start..end` that are `parent`'s children,
/// in render order.
pub(crate) fn tops(start: usize, end: usize, parent: *mut lvgl_sys::lv_obj_t) -> Vec<*mut lvgl_sys::lv_obj_t> {
    TABLE.with(|t| {
        let t = t.borrow();
        let Some(t) = t.as_ref() else { return Vec::new() };
        t.recs[start.min(t.recs.len())..end.min(t.recs.len())]
            .iter()
            .flat_map(|r| r.made.iter().map(|m| m.obj))
            .filter(|&o| crate::ui::parent_of(o) == parent)
            .collect()
    })
}

/// Where in `parent` the objects of the records from `start` on go:
/// where the first of them is (theirs, else the next record's), else
/// at the end (before a running button's overlay).
pub(crate) fn insert_index(start: usize, parent: *mut lvgl_sys::lv_obj_t) -> u32 {
    let first = TABLE.with(|t| {
        let t = t.borrow();
        let t = t.as_ref()?;
        let len = t.recs.len();
        t.recs[start.min(len)..]
            .iter()
            .flat_map(|r| r.made.iter().map(|m| m.obj))
            .find(|&o| crate::ui::parent_of(o) == parent)
    });
    let end = crate::ui::content_end(parent);
    first.map_or(end, |o| crate::ui::index_of(o).min(end))
}

/// Put the objects of records `start..end` that are `parent`'s
/// children at `at` on, in render order.
pub(crate) fn place(start: usize, end: usize, parent: *mut lvgl_sys::lv_obj_t, at: u32) {
    for (k, o) in tops(start, end, parent).into_iter().enumerate() {
        crate::ui::move_to_index(o, at + k as u32);
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

/// A `repeat`'s item keys as drawn, and why it drew none.
pub(crate) fn repeat_state(idx: usize) -> (Vec<String>, Option<String>) {
    TABLE.with(|t| t.borrow().as_ref().and_then(|t| t.recs.get(idx)).map(|r| (r.keys.clone(), r.failed.clone())).unwrap_or_default())
}

/// What an update walk needs to start: the page drawn, whole, and
/// the objects it was drawn in.
pub(crate) struct Start {
    pub page_root: *mut lvgl_sys::lv_obj_t,
    holder: *mut lvgl_sys::lv_obj_t,
}

/// Start an update walk of `page` for `changes`, if the table can do
/// it: `Err` says why not (the caller builds the page again).
pub(crate) fn begin_update(page: &str, changes: &Changes, hold: Option<Vec<*mut lvgl_sys::lv_obj_t>>) -> Result<Option<Start>, &'static str> {
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
        Ok(hit.then_some(Start { page_root: t.page_root, holder: t.holder }))
    })?;
    // A finger on the page's own background holds no node of it (only
    // its changes of structure wait).
    if let Some(start) = &state {
        let page = [start.holder, start.page_root, crate::ui::parent_of(start.holder)];
        let hold = hold.map(|h| h.into_iter().filter(|o| !page.contains(o)).collect());
        reset_frames();
        CHANGES.with(|c| *c.borrow_mut() = changes.clone());
        CURSOR.with(|c| c.set(0));
        REUSE.with(|r| r.borrow_mut().clear());
        OPEN.with(|o| o.borrow_mut().clear());
        REAPPLIED.with(|n| n.set(0));
        VISITED.with(|n| n.set(0));
        RESTRUCTURED.with(|n| n.set(0));
        HELD.with(|h| h.set(false));
        HOLD.with(|h| *h.borrow_mut() = hold);
    }
    Ok(state)
}

/// End an update walk: `Err` when it found something it could not do
/// in place.
pub(crate) fn end_update() -> Result<Walked, &'static str> {
    CHANGES.with(|c| *c.borrow_mut() = Changes::default());
    HOLD.with(|h| *h.borrow_mut() = None);
    OPEN.with(|o| o.borrow_mut().clear());
    clear_doomed();
    let done = TABLE.with(|t| match t.borrow_mut().as_mut() {
        Some(t) => {
            if t.broken.is_none() && CURSOR.with(|c| c.get()) != t.recs.len() {
                t.broken = Some("the walk ended early");
            }
            t.broken.map_or(Ok(()), Err)
        }
        None => Err("no page drawn"),
    });
    done.map(|()| Walked {
        visited: VISITED.with(|v| v.get()),
        reapplied: REAPPLIED.with(|n| n.get()),
        restructured: RESTRUCTURED.with(|n| n.get()),
        held: HELD.with(|h| h.get()),
    })
}

/// What an update walk did.
pub(crate) struct Walked {
    pub visited: usize,
    pub reapplied: usize,
    pub restructured: usize,
    /// Something under a finger waits for the release.
    pub held: bool,
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
            Some(r) => format!("made by `{}` at {:?} byte {}", r.what, r.place.file, r.place.start),
            None => "not made by a node of the page (a part of one, or the page itself)".to_string(),
        }
    })
}

/// Records and objects of the page drawn, for `ctl status`.
pub fn table_size() -> (usize, usize) {
    TABLE.with(|t| t.borrow().as_ref().map_or((0, 0), |t| (t.recs.len(), t.recs.iter().map(|r| r.made.len()).sum())))
}

/// The table of a page kept off screen (the page cache, ADR 0024
/// section 3): taken from the page drawn when it is left, put back
/// when it shows again.
pub(crate) struct Saved(Table);

impl Saved {
    pub(crate) fn page(&self) -> &str {
        &self.0.page
    }

    /// What anything on the page reads: its dep index, which the
    /// changes made while it is away are checked against.
    pub(crate) fn deps(&self) -> Vec<Dep> {
        let mut out = Vec::new();
        for r in &self.0.recs {
            union(&mut out, &r.own);
        }
        out
    }

    /// Records and objects made (a part of what the page costs).
    pub(crate) fn size(&self) -> (usize, usize) {
        (self.0.recs.len(), self.0.recs.iter().map(|r| r.made.len()).sum())
    }
}

/// Take the table of the page drawn, if it can be kept: drawn whole,
/// on its holder, and updatable in place.
pub(crate) fn take_table() -> Option<Saved> {
    TABLE.with(|t| {
        let mut t = t.borrow_mut();
        let ok = t.as_ref().is_some_and(|t| t.broken.is_none() && !t.holder.is_null() && !t.page_root.is_null());
        let table = t.take();
        ok.then(|| table.map(Saved)).flatten()
    })
}

/// Make `saved` the table of the page drawn again.
pub(crate) fn put_table(saved: Saved) {
    TABLE.with(|t| *t.borrow_mut() = Some(saved.0));
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
        // An alert acknowledged: `active` and `shown`, not `count`.
        let mut al = Changes::default();
        al.alert("active");
        al.alert("shown");
        assert!(al.hits(&Dep::Alerts(Some("shown".into()))) && al.hits(&Dep::Alerts(None)) && !al.hits(&Dep::Alerts(Some("count".into()))));
        al.alert(ALL);
        assert!(al.hits(&Dep::Alerts(Some("count".into()))));
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
        let e = parse("value > 1 && page.title != '' && vars.x && sources.y.ok && now.time != '' && ago(sources.y.at) != '' && d1.temp > 1 && d1.status.load.running && d1.status.fan[0] && d2.ok && len(alerts.shown) > 0 && alerts['count'] > 0 && alerts[vars.x] != null");
        expr_deps(1, &e, &mut d);
        union(&mut d, &[]);
        let mut want = vec![
            Dep::Source("cpu".into(), Some("value".into())),
            Dep::Var(Some("title".into())),
            Dep::Var(Some("x".into())),
            Dep::Source("y".into(), Some("ok".into())),
            Dep::NowMinute,
            Dep::Source("y".into(), Some("at".into())),
            Dep::Source("d1.temp".into(), Some("value".into())),
            Dep::Source("d1.load".into(), Some("running".into())),
            Dep::Source("d1.fan".into(), None),
            Dep::Remote("d2".into()),
            Dep::Alerts(Some("shown".into())),
            Dep::Alerts(Some("count".into())),
            Dep::Alerts(None),
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
            assert_eq!(d, vec![Dep::Source("dev.x".into(), Some("value".into()))]);
        }
        assert_eq!(all_local_deps().len(), 2);
        clear_reads();
    }
}
