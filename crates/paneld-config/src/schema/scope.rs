//! Schema 16: every name an expression reads must be in scope where
//! it is written, and a field of a value whose shape is known at load
//! must exist (`sources.x` a defined source, `vars.y` a defined var,
//! `theme.t` a token, `now.f`, `alerts.f`, a remote's members).
//!
//! What is in scope, by place:
//!
//! | Place | Names |
//! | --- | --- |
//! | pages, components, theme styles | the globals, `page`, `nav`, a component's params, `repeat` bindings |
//! | `on_change` / `on_submit` | `value`, `event` too |
//! | an inline action (`on_tap { run = ... }`) | the names around it, but `page` and `nav` |
//! | an action | the globals, its params; `result` in `done`, `failed`, `then`, `else` (and everywhere in an action a `then` / `else` names); `output` in `done`; `value`, `event` in an action an `on_change` / `on_submit` names |
//! | a source | the globals but `alerts`; its `value`: `out`, `now` only |
//! | an alert | the globals (`alerts` only with `from`), `event` with `from` |
//! | a remote kind | its params in every member; `self` in its alerts |
//! | `screen:dim` | `level` only; the other screen commands nothing |
//!
//! A param's default reads the params declared before it, never
//! itself or a later one. The read analysis is [`crate::reads`].

use std::collections::{BTreeMap, HashSet};

use super::*;
use crate::reads::{self, Read};

/// The fields of `sources.<name>` (schema 4; `exit`, `stdout`,
/// `stderr` of a command's result, `status` of an `http` member).
const SOURCE_FIELDS: &[&str] = &["value", "ok", "error", "at", "age", "running", "history", "exit", "stdout", "stderr", "status"];
/// The fields of `alerts` (schema 9).
const ALERTS_FIELDS: &[&str] = &["active", "count", "worst", "shown", "snooze", "history", "recent"];
/// The fields of `page` (and of each `nav` entry, with `current`).
const PAGE_FIELDS: &[&str] = &["name", "title", "icon"];
/// Tokens a theme may leave unset (schema 11), readable as `theme.x`.
const OPTIONAL_TOKENS: &[&str] = &[
    "background", "track", "on_accent", "shadow", "background_image", "icons", "motion", "page_transition",
    "banner_transition", "value_transition", "tap_transition", "dialog_backdrop",
];

/// Which global roots a place has.
#[derive(Clone, Copy)]
struct Globals {
    sources: bool,
    vars: bool,
    alerts: bool,
    theme: bool,
    now: bool,
    /// Remote instances by name.
    remotes: bool,
}

const ALL: Globals = Globals { sources: true, vars: true, alerts: true, theme: true, now: true, remotes: true };
const NONE: Globals = Globals { sources: false, vars: false, alerts: false, theme: false, now: false, remotes: false };

#[derive(Clone)]
enum Local {
    Plain,
    /// `page`: its fields are known.
    Page,
    /// `self` in a remote kind's alert: the kind's members.
    Instance(Vec<String>),
    /// A param declared after the one whose default is checked.
    Later,
    /// The param whose default is checked.
    Itself,
}

#[derive(Clone)]
struct Scope {
    globals: Globals,
    /// Where, for messages: "a source's `value`".
    place: &'static str,
    locals: Vec<(String, Local)>,
}

impl Scope {
    fn new(globals: Globals, place: &'static str) -> Self {
        Scope { globals, place, locals: Vec::new() }
    }

    fn with(&self, name: &str, l: Local) -> Self {
        let mut s = self.clone();
        s.locals.push((name.to_string(), l));
        s
    }

    fn at(&self, place: &'static str) -> Self {
        Scope { place, ..self.clone() }
    }

    fn without(&self, names: &[&str]) -> Self {
        let mut s = self.clone();
        s.locals.retain(|(n, _)| !names.contains(&n.as_str()));
        s
    }

    fn local(&self, name: &str) -> Option<&Local> {
        self.locals.iter().rev().find(|(n, _)| n == name).map(|(_, l)| l)
    }
}

struct Env<'a> {
    names: &'a Names,
    vars: HashSet<String>,
    tokens: HashSet<String>,
    /// Remote instance to its kind's member names.
    remotes: BTreeMap<String, Vec<String>>,
    /// Remote instance to its kind's name.
    kinds: BTreeMap<String, String>,
    /// Actions an `on_change` / `on_submit` names (`None`: one gives
    /// its action from an expression, so any may be).
    change_targets: Option<HashSet<String>>,
    /// Actions a `then` / `else` names (`None` as above).
    chain_targets: Option<HashSet<String>>,
}

impl Env<'_> {
    fn targeted(set: &Option<HashSet<String>>, names: &[String]) -> bool {
        set.as_ref().is_none_or(|s| names.iter().any(|n| s.contains(n)))
    }
}

/// Check every expression of `doc` (overrides applied) for names out
/// of scope.
pub(super) fn check_scopes(doc: &Document, names: &Names, diags: &mut Vec<Diagnostic>) {
    let mut env = Env {
        names,
        vars: HashSet::new(),
        tokens: OPTIONAL_TOKENS
            .iter()
            .chain(THEME_COLOR_TOKENS)
            .chain(THEME_FONT_TOKENS)
            .chain(THEME_SIZE_TOKENS)
            .map(|s| s.to_string())
            .collect(),
        remotes: remote::instances(doc)
            .map(|(n, _, k)| (n.to_string(), remote::members(k).map(|(m, _)| m.to_string()).collect()))
            .collect(),
        kinds: remote::instances(doc)
            .filter_map(|(n, _, k)| Some((n.to_string(), k.name.as_ref()?.text.clone())))
            .collect(),
        change_targets: Some(HashSet::new()),
        chain_targets: Some(HashSet::new()),
    };
    for item in &doc.items {
        match item {
            Item::Var(v) => {
                env.vars.insert(v.name.text.clone());
            }
            Item::Block(b) if b.kind.text == "theme" => {
                for it in &b.items {
                    if let Item::Assign(a) = it {
                        if let AssignTarget::Simple { key } = &a.target {
                            env.tokens.insert(key.text.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    collect_targets(&doc.items, &mut env);

    let ui = Scope::new(ALL, "a page").with("page", Local::Page).with("nav", Local::Plain);
    for item in &doc.items {
        match item {
            Item::Var(v) => value(&v.default, &Scope::new(ALL, "a var's default"), &env, diags),
            Item::Assign(a) => value(&a.value, &Scope::new(ALL, "the config"), &env, diags),
            Item::Block(b) => match b.kind.text.as_str() {
                "source" => source(b, &[], &env, diags),
                "action" => {
                    let name = b.name.as_ref().map(|n| vec![n.text.clone()]).unwrap_or_default();
                    action(b, &Scope::new(ALL, "an action"), &name, &env, diags);
                }
                "alert" => alert(b, &[], None, &env, diags),
                "page" => ui_items(&b.items, &ui, &env, diags),
                "component" => component(b, &ui, &env, diags),
                "theme" => {
                    for it in &b.items {
                        match it {
                            Item::Block(c) if c.kind.text == "component" => component(c, &ui, &env, diags),
                            Item::Block(c) => ui_items(&c.items, &ui.at("a theme style"), &env, diags),
                            Item::Assign(a) => value(&a.value, &Scope::new(NONE, "a theme token"), &env, diags),
                            _ => {}
                        }
                    }
                }
                "screen" => screen(&b.items, &env, diags),
                "remote_kind" => kind(b, &env, diags),
                // An instance's params are literals (schema 17.3).
                "remote" => {}
                _ => plain(&b.items, &Scope::new(ALL, "the config"), &env, diags),
            },
            _ => {}
        }
    }
}

/// The actions named by `on_change` / `on_submit` and by `then` /
/// `else` anywhere in `items`.
fn collect_targets(items: &[Item], env: &mut Env) {
    fn add(set: &mut Option<HashSet<String>>, text: &str) {
        let t = text.trim();
        if matches!(classify(t), ValueShape::Literal(_)) {
            if let Some(s) = set {
                s.insert(t.to_string());
            }
        } else {
            *set = None;
        }
    }
    for it in items {
        match it {
            Item::Assign(a) => {
                let AssignTarget::Simple { key } = &a.target else { continue };
                match key.text.as_str() {
                    "on_change" | "on_submit" => add(&mut env.change_targets, &a.value.text),
                    "then" | "else" => add(&mut env.chain_targets, &a.value.text),
                    _ => {}
                }
            }
            Item::Block(b) => {
                if let Some(n) = &b.name {
                    match b.kind.text.as_str() {
                        "on_change" | "on_submit" => add(&mut env.change_targets, &n.text),
                        "then" | "else" => add(&mut env.chain_targets, &n.text),
                        _ => {}
                    }
                }
                collect_targets(&b.items, env);
            }
            Item::Repeat(r) => collect_targets(&r.items, env),
            _ => {}
        }
    }
}

/// Keys and blocks with no names of their own (reminders, settings,
/// `env`, call args).
fn plain(items: &[Item], scope: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) {
    for it in items {
        match it {
            Item::Assign(a) => value(&a.value, scope, env, diags),
            Item::Block(b) => plain(&b.items, scope, env, diags),
            Item::Repeat(r) => repeat(r, scope, env, diags, plain),
            _ => {}
        }
    }
}

fn repeat(r: &crate::parse::Repeat, scope: &Scope, env: &Env, diags: &mut Vec<Diagnostic>, body: fn(&[Item], &Scope, &Env, &mut Vec<Diagnostic>)) {
    if let (Some(e), _) = crate::cel::parse_fragment(&r.source.body, r.source.span.file, r.source.span.start + 2) {
        for read in reads::reads(&e) {
            check_read(&read, scope, env, diags);
        }
    }
    body(&r.items, &scope.with(&r.binding.text, Local::Plain), env, diags);
}

/// A UI tree: nodes, component calls (their args and slot children
/// are the caller's), handlers.
fn ui_items(items: &[Item], scope: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) {
    for it in items {
        match it {
            // A handler expression picking its action from `value` or
            // `event` has its own message (`resolve_refs_in_assign`).
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "action" || EVENT_HANDLER_KINDS.contains(&key.text.as_str())) => {
                value(&a.value, &scope.with("value", Local::Plain).with("event", Local::Plain), env, diags)
            }
            Item::Assign(a) => value(&a.value, scope, env, diags),
            Item::Repeat(r) => repeat(r, scope, env, diags, ui_items),
            Item::Block(b) if EVENT_HANDLER_KINDS.contains(&b.kind.text.as_str()) => {
                let s = if matches!(b.kind.text.as_str(), "on_change" | "on_submit") {
                    scope.with("value", Local::Plain).with("event", Local::Plain).at("an `on_change` / `on_submit`")
                } else {
                    scope.clone()
                };
                if b.is_inline_action() {
                    // Runs as an action, with the names where it is
                    // drawn as its args; `page` and `nav` are not args.
                    action_body(&b.items, &s.without(&["page", "nav"]).at("an inline action"), env, diags);
                } else {
                    ui_items(&b.items, &s, env, diags);
                }
            }
            Item::Block(b) => ui_items(&b.items, scope, env, diags),
            _ => {}
        }
    }
}

fn component(b: &Block, ui: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) {
    let s = params(&b.items, &ui.at("a component"), env, diags);
    ui_items(&b.items, &s, env, diags);
}

/// Bind `items`' params over `base`: each default is checked with the
/// params before it in scope (schema 6), and the result has them all.
fn params(items: &[Item], base: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) -> Scope {
    let ps: Vec<&Param> = items
        .iter()
        .filter_map(|it| match it {
            Item::Param(p) => Some(p),
            _ => None,
        })
        .collect();
    for (i, p) in ps.iter().enumerate() {
        if let Some(d) = &p.default {
            let mut s = base.at("a param's default");
            for q in &ps[..i] {
                s = s.with(&q.name.text, Local::Plain);
            }
            for q in &ps[i + 1..] {
                s = s.with(&q.name.text, Local::Later);
            }
            s = s.with(&p.name.text, Local::Itself);
            value(d, &s, env, diags);
        }
        // `options = {{ ... }}` is read before any arg is known.
        if let Some(o) = &p.options {
            plain(&o.items, &base.at("a param's options"), env, diags);
        }
    }
    let mut s = base.clone();
    for p in ps {
        s = s.with(&p.name.text, Local::Plain);
    }
    s
}

/// An action named `names` (the kind's sibling name and each
/// instance's `<instance>.<member>` for a kind's action).
fn action(b: &Block, base: &Scope, names: &[String], env: &Env, diags: &mut Vec<Diagnostic>) {
    let mut s = params(&b.items, base, env, diags);
    if Env::targeted(&env.change_targets, names) {
        s = s.with("value", Local::Plain).with("event", Local::Plain);
    }
    if Env::targeted(&env.chain_targets, names) {
        s = s.with("result", Local::Plain);
    }
    action_body(&b.items, &s, env, diags);
}

fn action_body(items: &[Item], s: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) {
    for it in items {
        match it {
            Item::Assign(a) => {
                let key = match &a.target {
                    AssignTarget::Simple { key } => key.text.as_str(),
                    _ => "",
                };
                match key {
                    "done" => value(&a.value, &s.with("result", Local::Plain).with("output", Local::Plain), env, diags),
                    "failed" => value(&a.value, &s.with("result", Local::Plain), env, diags),
                    _ => value(&a.value, s, env, diags),
                }
            }
            Item::Block(b) if matches!(b.kind.text.as_str(), "then" | "else") => {
                plain(&b.items, &s.with("result", Local::Plain), env, diags)
            }
            Item::Block(b) => action_body(&b.items, s, env, diags),
            _ => {}
        }
    }
}

/// A source, or a remote kind's `value` (with the kind's params).
fn source(b: &Block, kind_params: &[String], env: &Env, diags: &mut Vec<Diagnostic>) {
    let mut base = Scope::new(Globals { alerts: false, ..ALL }, "a source");
    // `value` is evaluated with `out` and the clock only.
    let mut val = Scope::new(Globals { now: true, ..NONE }, "a source's `value`").with("out", Local::Plain);
    for p in kind_params {
        base = base.with(p, Local::Plain);
        val = val.with(p, Local::Plain);
    }
    for it in &b.items {
        match it {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "value") => {
                value(&a.value, &val, env, diags)
            }
            Item::Assign(a) => value(&a.value, &base, env, diags),
            Item::Block(c) => plain(&c.items, &base, env, diags),
            _ => {}
        }
    }
}

/// An alert; a kind's has the kind's params and `self`.
fn alert(b: &Block, kind_params: &[String], members: Option<&[String]>, env: &Env, diags: &mut Vec<Diagnostic>) {
    let from = b.items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "from")));
    // A condition alert is evaluated before `alerts` is built.
    let (globals, place) = if from { (ALL, "an alert") } else { (Globals { alerts: false, ..ALL }, "a condition alert") };
    let mut s = Scope::new(globals, place);
    for p in kind_params {
        s = s.with(p, Local::Plain);
    }
    if let Some(m) = members {
        s = s.with("self", Local::Instance(m.to_vec()));
    }
    if from {
        s = s.with("event", Local::Plain);
    }
    plain(&b.items, &s, env, diags);
}

fn kind(b: &Block, env: &Env, diags: &mut Vec<Diagnostic>) {
    let kparams: Vec<String> = b
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Param(p) => Some(p.name.text.clone()),
            _ => None,
        })
        .collect();
    let members: Vec<String> = remote::members(b).map(|(m, _)| m.to_string()).collect();
    let kind_name = b.name.as_ref().map(|n| n.text.as_str()).unwrap_or("");
    let instances: Vec<&str> = env.kinds.iter().filter(|(_, k)| *k == kind_name).map(|(i, _)| i.as_str()).collect();
    let mut base = Scope::new(ALL, "a remote kind's action");
    for p in &kparams {
        base = base.with(p, Local::Plain);
    }
    for it in &b.items {
        let Item::Block(m) = it else { continue };
        match m.kind.text.as_str() {
            "value" => source(m, &kparams, env, diags),
            "action" => {
                let Some(name) = &m.name else { continue };
                let mut names = vec![name.text.clone()];
                names.extend(instances.iter().map(|i| format!("{i}.{}", name.text)));
                action(m, &base, &names, env, diags);
            }
            "alert" => alert(m, &kparams, Some(&members), env, diags),
            // Connection keys and `headers`: checked with the kind
            // (`check_kind`), they read its params only.
            _ => {}
        }
    }
}

fn screen(items: &[Item], env: &Env, diags: &mut Vec<Diagnostic>) {
    for it in items {
        match it {
            Item::Assign(a) => {
                let key = match &a.target {
                    AssignTarget::Simple { key } | AssignTarget::Qualified { key, .. } => key.text.as_str(),
                    _ => "",
                };
                let s = if key == "dim" {
                    Scope::new(NONE, "`screen:dim`").with("level", Local::Plain)
                } else {
                    Scope::new(NONE, "a screen command")
                };
                value(&a.value, &s, env, diags);
            }
            Item::Block(b) => plain(&b.items, &Scope::new(NONE, "`screen:night`"), env, diags),
            _ => {}
        }
    }
}

fn value(v: &crate::parse::RawValue, scope: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) {
    for read in reads::value_reads(v) {
        check_read(&read, scope, env, diags);
    }
}

/// Where a name that is not a global is available (schema 16).
fn only_in(name: &str) -> Option<&'static str> {
    Some(match name {
        "out" => "in a source's `value`",
        "event" => "in an alert with `from`, and in `on_change` / `on_submit`",
        "value" => "in `on_change` / `on_submit` (and an action they name)",
        "result" => "in an action's `done`, `failed`, `then` and `else` (and in an action a `then` / `else` names)",
        "output" => "in an action's `done`",
        "level" => "in `screen:dim`",
        "page" | "nav" => "in pages and components",
        _ => return None,
    })
}

fn hint(name: &str, candidates: impl IntoIterator<Item = String>) -> String {
    let c: Vec<String> = candidates.into_iter().collect();
    match crate::icons::nearest(name, c.iter().map(String::as_str)) {
        Some(s) => format!("; did you mean `{s}`?"),
        None => String::new(),
    }
}

fn check_read(r: &Read, scope: &Scope, env: &Env, diags: &mut Vec<Diagnostic>) {
    let root = r.root.as_str();
    match scope.local(root) {
        Some(Local::Plain) => return,
        Some(Local::Page) => return check_fields(r, 0, PAGE_FIELDS, "`page` has", diags),
        Some(Local::Instance(m)) => return check_instance(r, m, diags),
        Some(Local::Later) => {
            return diags.push(Diagnostic::new(
                r.root_span,
                format!("`{root}` is declared after this param: a default reads only the params before it"),
            ))
        }
        Some(Local::Itself) => {
            return diags.push(Diagnostic::new(r.root_span, format!("the default of `{root}` reads `{root}` itself")))
        }
        None => {}
    }
    let g = scope.globals;
    match root {
        "sources" if g.sources => check_named(r, &env.names.sources, "source", diags),
        "vars" if g.vars => check_named(r, &env.vars, "var", diags),
        "theme" if g.theme => {
            if let Some(st) = r.path.first() {
                if !env.tokens.contains(&st.name) {
                    let h = hint(&st.name, env.tokens.iter().cloned());
                    diags.push(Diagnostic::new(
                        st.span,
                        format!("unknown theme token `{}` in `{}`: no theme defines it{h}", st.name, r.written(0)),
                    ));
                }
            }
        }
        "secrets" => {
            if let Some(st) = r.path.first() {
                if !env.names.secrets.contains(&st.name) {
                    let h = hint(&st.name, env.names.secrets.iter().cloned());
                    diags.push(Diagnostic::new(st.span, format!("unknown secret `{}` in `{}`{h}", st.name, r.written(0))));
                }
            }
        }
        "now" if g.now => check_fields(r, 0, crate::cel::NOW_FIELDS, "`now` has", diags),
        "alerts" if g.alerts => check_fields(r, 0, ALERTS_FIELDS, "`alerts` has", diags),
        _ if g.remotes && env.remotes.contains_key(root) => check_instance(r, &env.remotes[root], diags),
        "self" => diags.push(Diagnostic::new(
            r.root_span,
            "`self` is the instance in a remote kind's alerts only; elsewhere use its name",
        )),
        _ => {
            let global = matches!(root, "sources" | "vars" | "alerts" | "theme" | "now") || env.remotes.contains_key(root);
            let msg = if global {
                format!("`{root}` is not available in {}", scope.place)
            } else if let Some(w) = only_in(root) {
                format!("`{root}` is not available in {}: it is only {w}", scope.place)
            } else {
                let mut known: Vec<String> = scope
                    .locals
                    .iter()
                    .filter(|(_, l)| !matches!(l, Local::Later | Local::Itself))
                    .map(|(n, _)| n.clone())
                    .collect();
                for (on, n) in [(g.sources, "sources"), (g.vars, "vars"), (g.alerts, "alerts"), (g.theme, "theme"), (g.now, "now")] {
                    if on {
                        known.push(n.to_string());
                    }
                }
                if g.remotes {
                    known.extend(env.remotes.keys().cloned());
                }
                // A var or source written without its root.
                let h = if g.vars && env.vars.contains(root) {
                    format!("; did you mean `vars.{root}`?")
                } else if g.sources && env.names.sources.contains(root) {
                    format!("; did you mean `sources.{root}`?")
                } else {
                    hint(root, known)
                };
                format!("unknown name `{root}` in {}{h}", scope.place)
            };
            diags.push(Diagnostic::new(r.root_span, msg));
        }
    }
}

/// `sources.x` / `vars.y`: `x` must be defined; a source's field too.
fn check_named(r: &Read, defined: &HashSet<String>, what: &str, diags: &mut Vec<Diagnostic>) {
    let Some(st) = r.path.first() else { return };
    if !defined.contains(&st.name) {
        // Remote members are not in `sources` (schema 17.3).
        let h = hint(&st.name, defined.iter().filter(|n| !n.contains('.')).cloned());
        diags.push(Diagnostic::new(st.span, format!("unknown {what} `{}` in `{}`{h}", st.name, r.written(0))));
    } else if what == "source" {
        check_fields(r, 1, SOURCE_FIELDS, "a source has", diags);
    }
}

/// Step `n` of `r`, when there is one, must be one of `fields`.
fn check_fields(r: &Read, n: usize, fields: &[&str], has: &str, diags: &mut Vec<Diagnostic>) {
    let Some(st) = r.path.get(n) else { return };
    if !fields.contains(&st.name.as_str()) {
        let h = hint(&st.name, fields.iter().map(|s| s.to_string()));
        diags.push(Diagnostic::new(
            st.span,
            format!("unknown field `{}` in `{}`; {has} {}{h}", st.name, r.written(n), fields.join(", ")),
        ));
    }
}

/// `device1.temp`, `device1.ok`, `device1.status.temp.at` (schema 17.3).
fn check_instance(r: &Read, members: &[String], diags: &mut Vec<Diagnostic>) {
    let Some(st) = r.path.first() else { return };
    let is_member = |n: &str| members.iter().any(|m| m == n);
    let no_member = |st: &crate::reads::Step, n: usize, diags: &mut Vec<Diagnostic>| {
        let h = hint(&st.name, members.iter().cloned());
        diags.push(Diagnostic::new(
            st.span,
            format!("`{}` has no member `{}` (in `{}`){h}", r.root, st.name, r.written(n)),
        ));
    };
    match st.name.as_str() {
        "ok" | "error" => {}
        "status" => match r.path.get(1) {
            Some(m) if !is_member(&m.name) => no_member(m, 1, diags),
            Some(_) => check_fields(r, 2, SOURCE_FIELDS, "a member's status has", diags),
            None => {}
        },
        n if is_member(n) => {}
        _ => no_member(st, 0, diags),
    }
}

#[cfg(test)]
mod tests {
    use crate::load;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// The check's messages for `text` as a config, `line:col: message`.
    fn check_text(text: &str) -> Vec<String> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/testdata")
            .join(format!("paneld-scope-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.conf");
        std::fs::write(&p, text).unwrap();
        let (map, doc, mut diags) = load(&p).unwrap();
        diags.extend(crate::check(&doc, &map));
        let _ = std::fs::remove_dir_all(&dir);
        diags
            .iter()
            .map(|d| {
                let l = map.location(d.span);
                format!("{}:{}: {}", l.line, l.column, d.message)
            })
            .collect()
    }

    fn ok(text: &str) {
        let d = check_text(text);
        assert!(d.is_empty(), "{d:#?}");
    }

    /// Exactly the `expected` messages (each a part of one), in order.
    fn errs(text: &str, expected: &[&str]) {
        let d = check_text(text);
        assert_eq!(d.len(), expected.len(), "{d:#?}");
        for (got, want) in d.iter().zip(expected) {
            assert!(got.contains(want), "{got:?} does not contain {want:?}\n{d:#?}");
        }
    }

    #[test]
    fn globals_and_their_known_fields() {
        ok("var tab: int = 0\nsecret tok { env = T }\nsource cpu { run = echo 1 {{ secrets.tok }} }\n\
            page p { text { value = {{ sources.cpu.value }} {{ sources['cpu'].at }} {{ vars.tab }} {{ theme.accent }} \
            {{ theme.banner_transition }} {{ now.time }} {{ alerts.count }} {{ page.title }} {{ len(nav) }} {{ size(sources) }} } }\n");
        errs(
            "var tab: int = 0\nsecret tok { env = T }\nsource cpu { run = echo {{ secrets.tokk }} }\n\
             page p { text { value = {{ zzz }} {{ tab }} {{ cpu }} {{ sources.cpuu.value }} {{ sources.cpu.valeu }} {{ vars.tabb }} \
             {{ theme.accnt }} {{ now.tim }} {{ alerts.cont }} {{ page.titel }} } }\n",
            &[
                "3:28: unknown secret `tokk` in `secrets.tokk`; did you mean `tok`?",
                "4:28: unknown name `zzz` in a page",
                "4:38: unknown name `tab` in a page; did you mean `vars.tab`?",
                "4:48: unknown name `cpu` in a page; did you mean `sources.cpu`?",
                "unknown source `cpuu` in `sources.cpuu`; did you mean `cpu`?",
                "unknown field `valeu` in `sources.cpu.valeu`; a source has value",
                "unknown var `tabb` in `vars.tabb`; did you mean `tab`?",
                "unknown theme token `accnt` in `theme.accnt`: no theme defines it; did you mean `accent`?",
                "unknown field `tim` in `now.tim`",
                "unknown field `cont` in `alerts.cont`",
                "unknown field `titel` in `page.titel`",
            ],
        );
        // A token a theme adds is a token.
        ok("theme t { brand = #ff0000 }\npage p { text { color = {{ theme.brand }} } }\n");
    }

    #[test]
    fn components_see_their_params_not_the_callers_names() {
        ok("component c { param a: int\n text { value = {{ string(a) }} {{ page.name }} } }\n\
            page p { repeat it in {{ [1] }} { c { a = {{ it }} } } }\n");
        errs(
            "component c { param a: int\n text { value = {{ string(a) + it }} } }\n\
             page p { repeat it in {{ [1] }} { c { a = {{ it + b }} } } }\n",
            &["2:32: unknown name `it` in a component", "3:51: unknown name `b` in a page"],
        );
    }

    #[test]
    fn repeat_bindings_nest_and_macro_names_stay_inside() {
        ok("page p { repeat x in {{ [[1]] }} { repeat y in {{ x }} { text { value = {{ x + y }} {{ [1].map(q, q + x) }} } } } }\n");
        errs(
            "page p { repeat x in {{ [1] }} { text { value = {{ x }} } }\n text { value = {{ x }} {{ [1].map(q, q) }} {{ q }} } }\n",
            &["2:20: unknown name `x` in a page", "2:48: unknown name `q` in a page"],
        );
    }

    #[test]
    fn handlers_actions_and_their_results() {
        ok("var on: bool = false\n\
            action go { param n: int\n run = echo {{ n }}\n done = {{ output }} {{ result.ok }}\n failed = {{ result.stderr }} }\n\
            action next { run = echo {{ result.stdout }} }\n\
            action first { run = true\n then = next }\n\
            action set_it { param value: bool\n run = echo {{ value }} {{ event.value }} }\n\
            page p { repeat r in {{ [1] }} {\n\
              toggle { on_change { set { on = {{ value }} } } }\n\
              toggle { on_change = set_it }\n\
              button { on_tap { run = echo {{ r }} } }\n\
              keypad { on_submit { run = echo {{ event.value }} {{ r }} } } } }\n");
        errs(
            "action go { param n: int\n run = echo {{ result.ok }} {{ m }}\n done = {{ value }} }\n\
             page p { text { value = {{ value }} {{ result }} {{ out }} {{ level }} {{ event }} }\n\
               button { on_tap { run = echo {{ page.name }} } } }\n",
            &[
                "2:16: `result` is not available in an action",
                "2:32: unknown name `m` in an action",
                "3:12: `value` is not available in an action",
                "`value` is not available in a page: it is only in `on_change`",
                "`result` is not available in a page",
                "`out` is not available in a page: it is only in a source's `value`",
                "`level` is not available in a page: it is only in `screen:dim`",
                "`event` is not available in a page",
                "5:33: `page` is not available in an inline action",
            ],
        );
    }

    #[test]
    fn sources_alerts_and_the_screen() {
        ok("source s { run = echo {{ vars.v }}\n parse = number\n value = {{ out * 2 }} {{ now.unix }} }\nvar v: int = 1\n\
            source log { run = tail -f x\n stream = true }\n\
            alert hot { when = {{ sources.s.value > 1 }}\n title = {{ vars.v }} }\n\
            alert ev { from = log\n when = {{ event.x == 1 && alerts.count < 9 }} }\n\
            screen { dim = echo {{ level }} }\n");
        errs(
            "var v: int = 1\nsource s { run = echo {{ alerts.count }}\n value = {{ out + vars.v + sources.s.value }} }\n\
             alert hot { when = {{ alerts.count > 0 && event.x }} }\n\
             screen { dim = echo {{ level }} {{ vars.v }}\n on = echo {{ level }} }\n",
            &[
                "2:26: `alerts` is not available in a source",
                "3:19: `vars` is not available in a source's `value`",
                "3:28: `sources` is not available in a source's `value`",
                "4:23: `alerts` is not available in a condition alert",
                "4:43: `event` is not available in a condition alert",
                "5:36: `vars` is not available in `screen:dim`",
                "6:15: `level` is not available in a screen command",
            ],
        );
    }

    #[test]
    fn remote_kinds_their_params_members_and_self() {
        let kind = "remote_kind box {\n param host: string\n via = local\n\
                    value temp { run = cat {{ host }}\n parse = number\n value = {{ out / 1000.0 }} {{ host }} }\n\
                    action reboot { param unit: string = x\n run = echo {{ host }} {{ unit }} }\n\
                    alert hot { when = {{ self.temp > 80 && self.status.temp.ok }}\n title = {{ host }} } }\n\
                    remote d1: box { host = a }\n";
        ok(&format!("{kind}page p {{ text {{ value = {{{{ d1.temp }}}} {{{{ d1.ok }}}} {{{{ d1.status.temp.at }}}} }} }}\n"));
        errs(
            &format!(
                "{kind}remote_kind k2 {{ via = local\n value v {{ run = echo {{{{ host }}}} }}\n alert a {{ when = {{{{ self.nope }}}} }} }}\n\
                 remote d2: k2 {{ }}\n\
                 page p {{ text {{ value = {{{{ d1.tmp }}}} {{{{ d1.status.nope }}}} {{{{ d1.status.temp.valeu }}}} {{{{ self.temp }}}} {{{{ host }}}} }} }}\n"
            ),
            &[
                "unknown name `host` in a source",
                "`self` has no member `nope`",
                "`d1` has no member `tmp` (in `d1.tmp`); did you mean `temp`?",
                "`d1` has no member `nope` (in `d1.status.nope`)",
                "unknown field `valeu` in `d1.status.temp.valeu`",
                "`self` is the instance in a remote kind's alerts only",
                "unknown name `host` in a page",
            ],
        );
    }

    #[test]
    fn a_param_default_reads_the_params_before_it() {
        ok("component c { param a: int = 1\n param b: int = {{ a + 1 }}\n text { value = {{ string(b) }} } }\n\
            action go { param a: int = 1\n param b: int = {{ a * 2 }}\n run = echo {{ b }} }\n");
        errs(
            "component c { param a: int = {{ b }}\n param b: int = {{ b + 1 }}\n text { value = x } }\n\
             action go { param a: int = {{ b }}\n param b: int = 1\n run = echo }\n",
            &[
                "1:33: `b` is declared after this param",
                "2:20: the default of `b` reads `b` itself",
                "4:31: `b` is declared after this param",
            ],
        );
    }
}
