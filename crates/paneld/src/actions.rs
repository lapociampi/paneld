//! Collect top-level `action NAME { ... }` blocks and run them on
//! tap. An action today carries an optional `run` command and an
//! optional `set { k = v; ... }` block. The worker interpolates
//! `run` through [`crate::runner`] in its own thread; on a successful
//! run (or on an action without `run`) it evaluates each `set` RHS,
//! coerces it to the var's declared type, writes to the shared vars
//! map, and sends [`UiEvent::VarsChanged`] so the main loop
//! re-renders. Named action-call args, `then`/`else`, `refresh`,
//! `confirm`, `pin`, secret resolution: later phases.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Receiver;
// Sends wake the UI thread's pump, which waits in `poll`.
use paneld_ui::WakingSender as Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use paneld_config::parse::{AssignTarget, Block, Document, Item, RawValue};
use paneld_ui::{ActionArg, Context, InlineSetItem, UiEvent, Value};

use crate::runner;
use crate::vars;


/// A collected top-level `action NAME { ... }`.
#[derive(Clone, Debug, Default)]
pub struct ActionDef {
    pub run: Option<String>,
    pub set: Vec<SetAssign>,
    /// `refresh = NAME` or `refresh = [NAME, ...]` -- source names to
    /// re-run after a successful `run` (or on an action without `run`).
    pub refresh: Vec<String>,
    /// `then = NAME` -- chain to this action on success. Simple ident
    /// form only today; the block form (`then NAME { arg = ... }`) and
    /// the `result.*` identifier the chained action can read come in
    /// a later phase.
    pub then: Option<String>,
    /// `else = NAME` -- chain on failure, same scope as `then`.
    pub else_: Option<String>,
    /// `env { KEY = value; ... }` pairs appended to the clean child
    /// environment (ADR 0013). Values are raw text interpolated at
    /// run time against the eval ctx so secrets can land in env.
    pub env: Vec<EnvPair>,
    /// `cwd = <path>` on the action, overriding ADR 0013's default
    /// (the config file's directory). A relative path joins onto
    /// the default.
    pub cwd: Option<String>,
    /// Per-action `timeout = <dur>`; falls back to
    /// `WorkerCtx::app_defaults.timeout`.
    pub timeout: Option<Duration>,
    /// `confirm = <message>` on an action pops a modal with OK /
    /// Cancel before running. Interpolated against the eval ctx at
    /// prompt time so the message can mention the item.
    pub confirm: Option<ConfirmPrompt>,
    /// `title = <text>`: the action's name in dialogs and messages.
    pub title: Option<ConfirmPrompt>,
    /// `pin = true`: ask the PIN (schema section 14) before
    /// `confirm` and `run`.
    pub pin: bool,
    /// `goto = <page> | back`, after a successful run.
    pub goto: Option<String>,
    /// `done = <text>`: shown briefly on success (`output` = stdout).
    pub done: Option<ConfirmPrompt>,
    /// `failed = <text>`: shown on failure; default stderr.
    pub failed: Option<ConfirmPrompt>,
    /// `param name: type [= default] [{ options }]` (schema 6).
    pub params: Vec<ParamDef>,
    /// A remote member's block (schema 17): its command is built from
    /// it and its kind, and the kind's params are bound for its
    /// expressions.
    pub remote: Option<Block>,
}

impl ActionDef {
    /// It runs a command: a `run`, or a remote member's request.
    pub fn runs(&self) -> bool {
        self.run.is_some()
            || self.remote.as_ref().is_some_and(|b| {
                b.items.iter().any(|it| matches!(it, Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "send" || paneld_config::remote::METHODS.contains(&key.text.as_str()))))
            })
    }

    /// Bind a remote member's params into `ctx`.
    pub fn bind(&self, ctx: &mut Context) {
        if let Some(b) = &self.remote {
            crate::remotes::bind(b, ctx);
        }
    }
}

/// One declared action parameter (schema 6).
#[derive(Clone, Debug)]
pub struct ParamDef {
    pub name: String,
    pub ty: String,
    pub default: Option<ConfirmPrompt>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// `options = [...]` (an expression or a list literal).
    pub options: Option<ConfirmPrompt>,
    /// `ask = select | slider | keypad | toggle`.
    pub ask: Option<String>,
    /// `pattern = <regex>`: the whole text must match.
    pub pattern: Option<String>,
    pub label: Option<String>,
}

impl ParamDef {
    fn from_ast(p: &paneld_config::parse::Param) -> Self {
        let mut def = ParamDef {
            name: p.name.text.clone(),
            ty: p.ty.text.clone(),
            default: p.default.as_ref().map(|d| ConfirmPrompt { raw: d.text.clone(), span: d.span }),
            min: None,
            max: None,
            options: None,
            ask: None,
            label: None,
            pattern: None,
        };
        for item in p.options.iter().flat_map(|o| &o.items) {
            let Item::Assign(a) = item else { continue };
            let AssignTarget::Simple { key } = &a.target else { continue };
            let text = a.value.text.trim();
            match key.text.as_str() {
                "min" => def.min = paneld_config::value::parse_number(text),
                "max" => def.max = paneld_config::value::parse_number(text),
                "options" => def.options = Some(ConfirmPrompt { raw: a.value.text.clone(), span: a.value.span }),
                "ask" => def.ask = Some(text.to_string()),
                "pattern" => def.pattern = Some(paneld_config::value::unquote_pattern(&a.value.text)),
                "label" => def.label = Some(text.trim_matches('"').to_string()),
                _ => {}
            }
        }
        def
    }

    /// The `options` list, evaluated against `ctx`.
    pub fn option_values(&self, ctx: &Context) -> Vec<Value> {
        let Some(o) = &self.options else { return Vec::new() };
        match paneld_ui::resolve_value(&o.raw, o.span, ctx) {
            Some(Value::List(l)) => l,
            Some(Value::String(s)) => paneld_config::value::split_list(&s)
                .unwrap_or_default()
                .into_iter()
                .map(|t| Value::String(t.trim().to_string()))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// What [`prepare_args`] makes of a call's args.
pub enum Prepared {
    /// Typed, checked, defaults filled in.
    Ready(Vec<ActionArg>),
    /// This param was not passed and has `ask`: show a dialog.
    Ask(Box<ParamDef>),
    /// Refused before anything runs, with the reason.
    Refused(String),
}

/// A value as param type `ty`; `None` if it is not one.
fn param_value(ty: &str, v: Value) -> Option<Value> {
    use paneld_config::value::{parse_color, parse_duration, parse_size};
    match (ty, &v) {
        // Strict for params: `10.9` is not an int (truncating it would
        // slip past `max`), and a bool is not a number.
        ("int", Value::Double(d)) if d.fract() != 0.0 => return None,
        ("int" | "number", Value::Bool(_)) => return None,
        _ => {}
    }
    match ty {
        "duration" => parse_duration(&v.to_display_string()).map(|_| Value::String(v.to_display_string())),
        "color" => parse_color(&v.to_display_string()).map(|_| Value::String(v.to_display_string())),
        "size" => parse_size(&v.to_display_string()).map(|_| Value::String(v.to_display_string())),
        "action" | "source" => Some(Value::String(v.to_display_string())),
        _ => vars::coerce(ty, v),
    }
}

/// Schema 6 and section 5's order ("params asked, PIN, confirm,
/// run"): type each passed arg and check it against `min`, `max` and
/// `options`, fill in defaults, and say which missing param to ask
/// for. Args no param declares (a `repeat` item, component params)
/// pass through. `defaults = false` leaves missing params out (the
/// built-in actions pick their own).
pub fn prepare_args(def: &ActionDef, mut args: Vec<ActionArg>, ctx: &Context, defaults: bool) -> Prepared {
    for p in &def.params {
        let given = args.iter().position(|a| a.name == p.name);
        let raw = match given {
            Some(i) => args[i].value.clone(),
            None if !defaults => continue,
            // Not passed: ask for it, else take the default.
            None if p.ask.is_some() => return Prepared::Ask(Box::new(p.clone())),
            None => match &p.default {
                Some(d) => match paneld_ui::resolve_value(&d.raw, d.span, ctx) {
                    Some(v) => v,
                    None => return Prepared::Refused(format!("the default of `{}` did not evaluate", p.name)),
                },
                None => return Prepared::Refused(format!("`{}` was not given", p.name)),
            },
        };
        let Some(v) = param_value(&p.ty, raw.clone()) else {
            let article = if p.ty.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
            return Prepared::Refused(format!("`{}` = {} is not {article} {}", p.name, raw.to_display_string(), p.ty));
        };
        let num = match &v {
            Value::Int(n) => Some(*n as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        };
        if let (Some(n), Some(min)) = (num, p.min) {
            if n < min {
                return Prepared::Refused(format!("`{}` = {} is below {min}", p.name, v.to_display_string()));
            }
        }
        if let (Some(n), Some(max)) = (num, p.max) {
            if n > max {
                return Prepared::Refused(format!("`{}` = {} is above {max}", p.name, v.to_display_string()));
            }
        }
        if let Some(pattern) = &p.pattern {
            let text = v.to_display_string();
            if paneld_config::value::pattern_matches(pattern, &text) != Ok(true) {
                return Prepared::Refused(format!("`{}` = {text} does not match its pattern", p.name));
            }
        }
        if p.options.is_some() {
            let shown = v.to_display_string();
            let options = p.option_values(ctx);
            if !options.iter().any(|o| o.to_display_string() == shown) {
                return Prepared::Refused(format!("`{}` = {shown} is not one of its options", p.name));
            }
        }
        match given {
            Some(i) => args[i].value = v,
            None => args.push(ActionArg { name: p.name.clone(), value: v }),
        }
    }
    Prepared::Ready(args)
}

/// Raw text + span of a message property (`confirm`, `done`,
/// `failed`), interpolated by the main loop when it is shown.
#[derive(Clone, Debug)]
pub struct ConfirmPrompt {
    pub raw: String,
    pub span: paneld_config::source::Span,
}

/// One `env { KEY = value }` entry. `value_raw` is the raw text,
/// interpolated the same way `run` is before exec.
#[derive(Clone, Debug)]
pub struct EnvPair {
    pub key: String,
    pub value_raw: String,
}

/// One `set { key = value }` entry; `value_raw` is the raw text as it
/// came out of the parser, ready for the same interpolate/eval pipeline
/// used for `text { value = ... }`.
#[derive(Clone, Debug)]
pub struct SetAssign {
    pub key: String,
    pub value_raw: String,
    pub value_span: paneld_config::source::Span,
}

/// Scan `doc` for top-level `action NAME { run = ...; set { ... } }`
/// blocks.
pub fn collect(doc: &Document) -> HashMap<String, ActionDef> {
    let mut out = HashMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text == "action" {
            if let Some(name) = &b.name {
                out.insert(name.text.clone(), parse_def(b));
            }
        }
    }
    // Inline handlers with action keys (`on_tap { run = ...; pin = true }`)
    // become anonymous actions, so they take the same path, gates
    // included, as a named one.
    collect_inline(&doc.items, &mut out);
    out
}

fn collect_inline(items: &[Item], out: &mut HashMap<String, ActionDef>) {
    for item in items {
        match item {
            Item::Block(b) => {
                if matches!(b.kind.text.as_str(), "on_tap" | "on_hold" | "on_change" | "on_submit")
                    && b.is_inline_action()
                {
                    out.insert(b.inline_action_name(), parse_def(b));
                }
                collect_inline(&b.items, out);
            }
            Item::Repeat(r) => collect_inline(&r.items, out),
            _ => {}
        }
    }
}

/// The definition in an `action NAME { }` block, or in an inline
/// handler block (same keys).
fn parse_def(b: &Block) -> ActionDef {
    {
        let mut def = ActionDef {
            params: b
                .items
                .iter()
                .filter_map(|it| match it {
                    Item::Param(p) => Some(ParamDef::from_ast(p)),
                    _ => None,
                })
                .collect(),
            remote: paneld_config::remote::remote_of(b).is_some().then(|| b.clone()),
            ..ActionDef::default()
        };
        if let Some(run) = find_simple(b, "run") {
            def.run = Some(run.to_string());
        }
        if let Some(set_block) = find_block(b, "set") {
            def.set = collect_set(set_block);
        }
        if let Some(raw) = find_simple(b, "refresh") {
            def.refresh = parse_refresh(raw);
        }
        if let Some(raw) = find_simple(b, "then") {
            let t = raw.trim();
            if !t.is_empty() {
                def.then = Some(t.to_string());
            }
        }
        if let Some(raw) = find_simple(b, "else") {
            let t = raw.trim();
            if !t.is_empty() {
                def.else_ = Some(t.to_string());
            }
        }
        if let Some(env_block) = find_block(b, "env") {
            def.env = collect_env(env_block);
        }
        if let Some(raw) = find_simple(b, "cwd") {
            let unquoted = paneld_config::value::parse_string(raw);
            let t = unquoted.trim();
            if !t.is_empty() {
                def.cwd = Some(t.to_string());
            }
        }
        if let Some(raw) = find_simple(b, "timeout") {
            if let Some(d) = paneld_config::value::parse_duration(raw) {
                def.timeout = Some(d);
            }
        }
        if let Some((raw, span)) = find_simple_with_span(b, "confirm") {
            def.confirm = Some(ConfirmPrompt { raw: raw.to_string(), span });
        }
        if let Some((raw, span)) = find_simple_with_span(b, "title") {
            def.title = Some(ConfirmPrompt { raw: raw.to_string(), span });
        }
        if let Some(raw) = find_simple(b, "pin") {
            def.pin = paneld_config::value::parse_bool(raw.trim()).unwrap_or(false);
        }
        if let Some(raw) = find_simple(b, "goto") {
            let t = raw.trim();
            if !t.is_empty() {
                def.goto = Some(t.to_string());
            }
        }
        if let Some((raw, span)) = find_simple_with_span(b, "done") {
            def.done = Some(ConfirmPrompt { raw: raw.to_string(), span });
        }
        if let Some((raw, span)) = find_simple_with_span(b, "failed") {
            def.failed = Some(ConfirmPrompt { raw: raw.to_string(), span });
        }
        def
    }
}

/// Pick an action's effective cwd per ADR 0013: an explicit `cwd =
/// <path>` on the action if present (joined onto `base` when
/// relative), otherwise `base` itself (the config file's directory).
fn resolve_action_cwd(base: &std::path::Path, cwd: Option<&str>) -> std::path::PathBuf {
    match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let p = std::path::Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(p)
            }
        }
        None => base.to_path_buf(),
    }
}

fn collect_env(block: &Block) -> Vec<EnvPair> {
    let mut out = Vec::new();
    for item in &block.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        out.push(EnvPair {
            key: key.text.clone(),
            value_raw: a.value.text.clone(),
        });
    }
    out
}

/// Parse the `refresh` value: either a bare ident `foo` or a list
/// `[foo, bar]`. Each item is trimmed; blanks are dropped.
fn parse_refresh(raw: &str) -> Vec<String> {
    let t = raw.trim();
    if t.is_empty() {
        return Vec::new();
    }
    if let Some(items) = paneld_config::value::split_list(t) {
        return items
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    vec![t.to_string()]
}

fn collect_set(block: &Block) -> Vec<SetAssign> {
    let mut out = Vec::new();
    for item in &block.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key } = &a.target else {
            continue;
        };
        let RawValue { text, span } = &a.value;
        out.push(SetAssign {
            key: key.text.clone(),
            value_raw: text.clone(),
            value_span: *span,
        });
    }
    out
}

/// What the worker thread needs to run actions, apply `set`,
/// re-run sources listed in `refresh`, and chain `then` / `else`.
pub struct WorkerCtx {
    pub actions: HashMap<String, ActionDef>,
    /// Without `secrets`; see `secrets`.
    pub base_ctx: Context,
    /// Bound as `secrets` only where schema 16 allows it: an action's
    /// `run` and `env`, and the commands of sources it refreshes.
    pub secrets: BTreeMap<String, Value>,
    pub vars: Arc<Mutex<BTreeMap<String, Value>>>,
    /// Live-shared source values so a `run =` that references
    /// `sources.X.value` sees the current sample. Updated by the
    /// main loop as `SourceUpdate`s arrive.
    pub source_values: Arc<Mutex<BTreeMap<String, Value>>>,
    pub var_types: BTreeMap<String, String>,
    pub event_tx: Sender<UiEvent>,
    pub source_blocks: BTreeMap<String, Block>,
    pub source_tx: Sender<crate::sources::SourceUpdate>,
    /// ADR 0013 default cwd for every action command: the directory
    /// of the main config file. A per-action `cwd` overrides.
    pub config_dir: std::path::PathBuf,
    /// ADR 0013 daemon-wide defaults (timeout, max_output) with
    /// per-action overrides on top.
    pub app_defaults: runner::AppDefaults,
}

/// One queued action invocation: its name plus the args already
/// evaluated at render time (empty for `on_tap = foo`).
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub args: Vec<ActionArg>,
    /// Set only by the main loop's gate pipeline once the action's
    /// PIN and confirm (if any) were satisfied. The worker refuses a
    /// gated action without it, so a new path to the worker cannot
    /// silently skip the gates.
    pub cleared: bool,
}

impl Call {
    pub fn new(name: String, args: Vec<ActionArg>) -> Self {
        Self { name, args, cleared: false }
    }
}

/// `true` when `def` needs the main loop's gates before it may run.
fn is_gated(def: &ActionDef) -> bool {
    def.pin || def.confirm.is_some()
}

/// Spawn the action worker. Each incoming [`Call`] is looked up in
/// `actions`. Calls of one action run in the order they came, on
/// that action's own thread (its lane), so a slow action never holds
/// up another and two quick toggles reach the device in order;
/// `app:max_running` caps the commands themselves (ADR 0013). An
/// action that only sets vars runs here at once, in order with the
/// others like it.
pub fn spawn_worker(wctx: WorkerCtx, rx: Receiver<Call>) {
    let wctx = Arc::new(wctx);
    thread::spawn(move || {
        let mut lanes: HashMap<String, Sender<(Call, ActionDef, String)>> = HashMap::new();
        for call in rx {
            let key = run_key(&call.name, &call.args);
            let refused = |why: &str| {
                eprintln!("paneld: action `{}`: {why}", call.name);
                // Free it in the main loop all the same.
                let _ = wctx.event_tx.send(UiEvent::ActionDone {
                    name: call.name.clone(),
                    key: key.clone(),
                    ok: false,
                    result: result_value(false, None, String::new(), why.to_string()),
                });
            };
            let Some(def) = wctx.actions.get(&call.name).cloned() else {
                refused("unknown action");
                continue;
            };
            if is_gated(&def) && !call.cleared {
                refused("refused, its PIN / confirm gate was not passed");
                continue;
            }
            if !def.runs() && def.refresh.is_empty() {
                run_call(&wctx, call, def, key);
                continue;
            }
            let lane = lanes.entry(call.name.clone()).or_insert_with(|| {
                let (tx, rx) = std::sync::mpsc::channel::<(Call, ActionDef, String)>();
                let wctx = Arc::clone(&wctx);
                thread::spawn(move || {
                    for (call, def, key) in rx {
                        run_call(&wctx, call, def, key);
                    }
                });
                tx.into()
            });
            let _ = lane.send((call, def, key));
        }
    });
}

/// Held from reading `vars` for a `set` to writing the result, so two
/// actions counting up do not lose one of the steps.
static SET_LOCK: Mutex<()> = Mutex::new(());

/// [`SET_LOCK`], for the main loop's inline `set`s.
pub fn set_lock() -> std::sync::MutexGuard<'static, ()> {
    SET_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// What makes two calls "the same" for schema 5's "while an action
/// runs, further calls of it are ignored": the action and its args,
/// so another row of a `repeat` is queued, not ignored.
pub fn run_key(name: &str, args: &[ActionArg]) -> String {
    let mut parts: Vec<String> = args
        .iter()
        .map(|a| format!("{}={}", a.name, crate::ctl::to_json(&a.value)))
        .collect();
    parts.sort();
    format!("{name}({})", parts.join(","))
}

/// Run one call of `def` (gates already passed) and report it.
fn run_call(wctx: &WorkerCtx, call: Call, def: ActionDef, key: String) {
    let name = call.name;
    let had_run = def.runs();
    let mut ok = true;
    // Schema 5: `result` = {ok, code, stdout, stderr}.
    let mut code: Option<i32> = None;
    let mut stdout = String::new();
    let mut stderr = String::new();

    if had_run {
        let mut ctx = ctx_with_vars_and_args(&wctx.base_ctx, &wctx.source_values, &wctx.vars, &call.args);
        ctx.insert("secrets".into(), Value::Map(wctx.secrets.clone()));
        // Bound last: a kind's params win over any arg of that name (a
        // `repeat` item called `host`). The check refuses an action
        // param that clashes with one.
        def.bind(&mut ctx);
        let file = paneld_config::source::FileId::default();
        let timeout = def.timeout.unwrap_or(wctx.app_defaults.timeout);
        // Split first, then interpolate each argv item. A remote
        // member's command is built around it (ADR 0018).
        let request = match &def.remote {
            Some(b) => crate::remotes::request(b, &ctx, timeout, false)
                .and_then(|r| r.ok_or_else(|| "nothing to run".to_string()))
                .map_err(|e| runner::RunError { message: e }),
            None => runner::command_argv(def.run.as_deref().unwrap_or_default(), |t| paneld_ui::interpolate(t, file, 0, &ctx))
                .map(|argv| crate::remotes::Request { argv, stdin: None }),
        };
        // Interpolate each env value against the same ctx so
        // `env { TOKEN = {{ secrets.token }} }` reaches the
        // child.
        let env_pairs: Vec<(String, String)> = def
            .env
            .iter()
            .map(|e| {
                (
                    e.key.clone(),
                    paneld_ui::interpolate(&e.value_raw, file, 0, &ctx),
                )
            })
            .collect();
        let cwd = resolve_action_cwd(&wctx.config_dir, def.cwd.as_deref());
        let stdin = request.as_ref().ok().and_then(|r| r.stdin.clone());
        let opts = runner::Opts {
            timeout,
            max_bytes: wctx.app_defaults.max_output,
            kind: runner::Kind::Action,
            name: &name,
            cwd: Some(&cwd),
            env: &env_pairs,
            stdin: stdin.as_deref(),
        };
        // Nothing a command prints reaches the log, the screen
        // or `result` with a secret in it.
        let mask = crate::secrets::Mask::new(&wctx.secrets);
        match request.and_then(|r| runner::exec(&r.argv, opts)) {
            Ok(mut r) => {
                r.stdout = mask.apply(&r.stdout);
                r.stderr = mask.apply(&r.stderr);
                eprintln!(
                    "paneld: action `{name}`: exit {:?}, timed_out {}{}",
                    r.exit_code,
                    r.timed_out,
                    if r.too_large { ", output too large" } else { "" }
                );
                if !r.stdout.trim().is_empty() {
                    eprintln!("paneld: action `{name}` stdout: {}", r.stdout.trim());
                }
                if !r.stderr.trim().is_empty() {
                    eprintln!("paneld: action `{name}` stderr: {}", r.stderr.trim());
                }
                ok = r.succeeded();
                code = r.exit_code;
                stdout = r.stdout;
                stderr = if r.too_large {
                    "output too large".to_string()
                } else if r.timed_out && def.remote.is_some() {
                    "timed out; it may still have run".to_string()
                } else if r.timed_out {
                    "timed out".to_string()
                } else if let Some(why) = def.remote.as_ref().filter(|_| !ok).and_then(|b| crate::remotes::explain(b, r.exit_code, &r.stderr)) {
                    mask.apply(&why)
                } else {
                    r.stderr
                };
            }
            Err(e) => {
                let e = mask.apply(&e.to_string());
                eprintln!("paneld: action `{name}`: {e}");
                ok = false;
                stderr = e;
            }
        }
    }

    // `set` runs after a successful `run`, or on an action that
    // has no `run` at all (schema allows it: "an action may
    // only change the UI").
    let should_set = (had_run && ok) || !had_run;
    if should_set && !def.set.is_empty() {
        let _set = set_lock();
        let mut ctx = ctx_with_vars_and_args(&wctx.base_ctx, &wctx.source_values, &wctx.vars, &call.args);
        def.bind(&mut ctx);
        apply_set(
            &name,
            &def.set,
            &ctx,
            &wctx.var_types,
            &wctx.vars,
            &wctx.event_tx,
        );
    }


    // `refresh` runs after a successful `run` on the same
    // basis as `set`. Each named source is re-executed and
    // the fresh value is sent through the normal source
    // channel so the main loop treats it like a scheduled
    // poll tick.
    let should_refresh = (had_run && ok) || !had_run;
    if should_refresh && !def.refresh.is_empty() {
        let mut ctx = ctx_with_vars_and_args(&wctx.base_ctx, &wctx.source_values, &wctx.vars, &call.args);
        ctx.insert("secrets".into(), Value::Map(wctx.secrets.clone()));
        for src_name in &def.refresh {
            let Some(block) = wctx.source_blocks.get(src_name) else {
                eprintln!(
                    "paneld: action `{name}`: refresh `{src_name}` is not a source"
                );
                continue;
            };
            let started = std::time::Instant::now();
            match crate::sources::resolve_one(
                block,
                &ctx,
                &wctx.config_dir,
                wctx.app_defaults,
            ) {
                Ok(v) => {
                    let _ = wctx.source_tx.send(crate::sources::SourceUpdate {
                        name: src_name.clone(),
                        value: v,
                        status: false,
                        started,
                    });
                }
                Err(e) => {
                    eprintln!(
                        "paneld: action `{name}`: refresh `{src_name}`: {e}"
                    );
                    let _ = wctx.source_tx.send(crate::sources::SourceUpdate {
                        name: src_name.clone(),
                        value: crate::sources::not_ok(&e),
                        status: true,
                        started,
                    });
                }
            }
        }
    }

    let result = result_value(ok, code, stdout, stderr);

    // The main loop does `goto` and the `done` / `failed`
    // message, and frees the action for its next call. Sent
    // before the chain: schema 5 order is goto, then `then`.
    let _ = wctx.event_tx.send(UiEvent::ActionDone {
        name,
        key,
        ok,
        result: result.clone(),
    });

    // `then` / `else` chain to another action on success /
    // failure, with `result` bound for it. They go back through
    // the main loop as a fresh call, so the chained action's
    // own PIN and confirm still apply.
    let next = if had_run && ok {
        def.then.as_ref()
    } else if had_run {
        def.else_.as_ref()
    } else {
        None
    };
    if let Some(next) = next {
        let _ = wctx.event_tx.send(UiEvent::Chained {
            name: next.clone(),
            args: vec![ActionArg { name: "result".into(), value: result }],
        });
    }
}

/// Schema 5's `result`: `{ok, code, stdout, stderr}`. `code` is -1
/// when the command did not exit on its own (timeout, killed, failed
/// to start) or there was no `run`.
fn result_value(ok: bool, code: Option<i32>, stdout: String, stderr: String) -> Value {
    let mut m = BTreeMap::new();
    m.insert("ok".to_string(), Value::Bool(ok));
    m.insert("code".to_string(), Value::Int(code.map(i64::from).unwrap_or(-1)));
    m.insert("stdout".to_string(), Value::String(stdout));
    m.insert("stderr".to_string(), Value::String(stderr));
    Value::Map(m)
}

/// Build an eval context from the worker-side pieces: the base ctx
/// (theme/secrets, snapshotted at daemon start), a fresh snapshot
/// of `sources` and `vars` (the main loop writes to both as new
/// samples arrive), and each action-call arg bound as a bare
/// identifier so `{{ vm }}` in a `run` string resolves to the arg
/// value. Args win over base ctx bindings on name clash.
fn ctx_with_vars_and_args(
    base: &Context,
    sources: &Arc<Mutex<BTreeMap<String, Value>>>,
    vars: &Arc<Mutex<BTreeMap<String, Value>>>,
    args: &[ActionArg],
) -> Context {
    let mut ctx = base.clone();
    let sources_snapshot = sources.lock().unwrap().clone();
    crate::remotes::expose(&mut ctx, sources_snapshot);
    let snapshot = vars.lock().unwrap().clone();
    ctx.insert("vars".into(), Value::Map(snapshot));
    for a in args {
        ctx.insert(a.name.clone(), a.value.clone());
    }
    ctx
}

fn apply_set(
    action_name: &str,
    assigns: &[SetAssign],
    ctx: &Context,
    var_types: &BTreeMap<String, String>,
    vars: &Arc<Mutex<BTreeMap<String, Value>>>,
    event_tx: &Sender<UiEvent>,
) {
    let mut changed = false;
    for a in assigns {
        let Some(ty) = var_types.get(&a.key) else {
            eprintln!(
                "paneld: action `{action_name}`: `set {}` targets an undeclared var",
                a.key
            );
            continue;
        };
        let Some(v) = paneld_ui::resolve_value(&a.value_raw, a.value_span, ctx) else {
            eprintln!(
                "paneld: action `{action_name}`: `set {}`: expression did not evaluate",
                a.key
            );
            continue;
        };
        let Some(coerced) = vars::coerce(ty, v) else {
            eprintln!(
                "paneld: action `{action_name}`: `set {}`: value is not a {ty}",
                a.key
            );
            continue;
        };
        {
            let mut guard = vars.lock().unwrap();
            // A run from before a reload: its var may be gone, or have
            // another type now. Leave those alone.
            match guard.get(&a.key) {
                Some(old) if std::mem::discriminant(old) == std::mem::discriminant(&coerced) || matches!(old, Value::Null) => {
                    guard.insert(a.key.clone(), coerced);
                }
                _ => {
                    eprintln!("paneld: action `{action_name}`: `set {}`: the var changed with a reload; not set", a.key);
                    continue;
                }
            }
        }
        changed = true;
    }
    if changed {
        let _ = event_tx.send(UiEvent::VarsChanged);
    }
}

/// Apply an inline `on_tap { set { k = v } }` payload on the UI
/// thread. The main loop calls this directly on [`UiEvent::InlineSet`]
/// so a tap writes vars and triggers a re-render without waiting on
/// the action-worker queue.
pub fn apply_inline_set(
    assigns: &[InlineSetItem],
    ctx: &Context,
    var_types: &BTreeMap<String, String>,
    vars: &Arc<Mutex<BTreeMap<String, Value>>>,
) -> bool {
    let mut changed = false;
    for a in assigns {
        let Some(ty) = var_types.get(&a.key) else {
            eprintln!("paneld: inline on_tap set: `{}` is not a declared var", a.key);
            continue;
        };
        let file = paneld_config::source::FileId::default();
        let span = paneld_config::source::Span { file, start: 0, end: 0 };
        // Where it was drawn: a `repeat` item, a component's params.
        let mut local;
        let ctx = if a.locals.is_empty() {
            ctx
        } else {
            local = ctx.clone();
            // Where it was drawn wins, except for the event's own value.
            for l in a.locals.iter().filter(|l| !matches!(l.name.as_str(), "event" | "value")) {
                local.insert(l.name.clone(), l.value.clone());
            }
            &local
        };
        let Some(v) = paneld_ui::resolve_value(&a.value_raw, span, ctx) else {
            eprintln!("paneld: inline on_tap set `{}`: expression did not evaluate", a.key);
            continue;
        };
        let Some(coerced) = vars::coerce(ty, v) else {
            eprintln!("paneld: inline on_tap set `{}`: value is not a {ty}", a.key);
            continue;
        };
        vars.lock().unwrap().insert(a.key.clone(), coerced);
        changed = true;
    }
    changed
}

fn find_simple_with_span<'a>(
    node: &'a Block,
    key: &str,
) -> Option<(&'a str, paneld_config::source::Span)> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return Some((a.value.text.as_str(), a.value.span));
        }
    }
    None
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

fn find_block<'a>(node: &'a Block, kind: &str) -> Option<&'a Block> {
    for item in &node.items {
        if let Item::Block(b) = item {
            if b.kind.text == kind {
                return Some(b);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_doc(src: &str) -> Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        use std::path::PathBuf;
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, lex_errs) = tokenize(id, src);
        assert!(lex_errs.is_empty(), "lex errors: {lex_errs:?}");
        let (doc, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "parse errors: {diags:?}");
        doc
    }

    #[test]
    fn collects_run_only_action() {
        let doc = parse_doc("action a { run = echo hi }\n");
        let actions = collect(&doc);
        let a = actions.get("a").expect("action a collected");
        assert_eq!(a.run.as_deref(), Some("echo hi"));
        assert!(a.set.is_empty());
    }

    #[test]
    fn collects_set_block() {
        let doc = parse_doc(
            "var tab: int = 0\naction a { set { tab = 1 } }\n",
        );
        let actions = collect(&doc);
        let a = actions.get("a").expect("action a collected");
        assert!(a.run.is_none());
        assert_eq!(a.set.len(), 1);
        assert_eq!(a.set[0].key, "tab");
        assert_eq!(a.set[0].value_raw.trim(), "1");
    }

    #[test]
    fn collects_run_and_set() {
        let doc = parse_doc(
            "var n: int = 0\naction a { run = /bin/true\n set { n = n + 1 } }\n",
        );
        let a = collect(&doc).remove("a").unwrap();
        assert_eq!(a.run.as_deref(), Some("/bin/true"));
        assert_eq!(a.set.len(), 1);
    }

    #[test]
    fn refresh_parses_scalar_and_list() {
        assert_eq!(parse_refresh("vms"), vec!["vms"]);
        assert_eq!(
            parse_refresh("[vms, hosts, cpu]"),
            vec!["vms".to_string(), "hosts".to_string(), "cpu".to_string()]
        );
        assert_eq!(parse_refresh("[ , ,]"), Vec::<String>::new());
        assert!(parse_refresh("   ").is_empty());
    }

    #[test]
    fn collects_refresh() {
        let doc = parse_doc(
            "source vms { run = /bin/true }\naction a { run = /bin/true\n refresh = vms }\n",
        );
        let a = collect(&doc).remove("a").unwrap();
        assert_eq!(a.refresh, vec!["vms".to_string()]);
    }

    #[test]
    fn collects_env_block() {
        let doc = parse_doc(
            "action a { run = /bin/true\n env { FOO = bar\n BAZ = {{ secrets.token }} } }\n",
        );
        let a = collect(&doc).remove("a").unwrap();
        assert_eq!(a.env.len(), 2);
        assert_eq!(a.env[0].key, "FOO");
        assert_eq!(a.env[0].value_raw.trim(), "bar");
        assert_eq!(a.env[1].key, "BAZ");
        assert!(a.env[1].value_raw.contains("secrets.token"));
    }

    #[test]
    fn collects_then_and_else() {
        let doc = parse_doc(
            "action notify_ok { run = /bin/true }\n\
             action notify_bad { run = /bin/true }\n\
             action a { run = /bin/true; then = notify_ok; else = notify_bad }\n",
        );
        let a = collect(&doc).remove("a").unwrap();
        assert_eq!(a.then.as_deref(), Some("notify_ok"));
        assert_eq!(a.else_.as_deref(), Some("notify_bad"));
    }

    #[test]
    fn ctx_overlay_puts_args_on_top_of_base_bindings() {
        let base = Context::new();
        let sources = Arc::new(Mutex::new(BTreeMap::new()));
        let vars = Arc::new(Mutex::new(BTreeMap::new()));
        let args = vec![ActionArg {
            name: "vm".into(),
            value: Value::String("alpha".into()),
        }];
        let ctx = ctx_with_vars_and_args(&base, &sources, &vars, &args);
        assert_eq!(ctx.get("vm"), Some(&Value::String("alpha".into())));
        assert!(ctx.contains_key("vars"));
        assert!(ctx.contains_key("sources"));
    }

    fn worker_for(doc: &Document) -> (Sender<Call>, Receiver<UiEvent>) {
        worker_with(doc, BTreeMap::new())
    }

    fn worker_with(doc: &Document, secrets: BTreeMap<String, Value>) -> (Sender<Call>, Receiver<UiEvent>) {
        let (tx, rx, _) = worker_full(doc, secrets);
        (tx, rx)
    }

    #[allow(clippy::type_complexity)]
    fn worker_full(
        doc: &Document,
        secrets: BTreeMap<String, Value>,
    ) -> (Sender<Call>, Receiver<UiEvent>, Arc<Mutex<BTreeMap<String, Value>>>) {
        let vars = Arc::new(Mutex::new(crate::vars::initial_values(doc)));
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (source_tx, _source_rx) = std::sync::mpsc::channel();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_worker(
            WorkerCtx {
                actions: collect(doc),
                base_ctx: Context::new(),
                secrets,
                vars: vars.clone(),
                source_values: Arc::new(Mutex::new(BTreeMap::new())),
                var_types: crate::vars::types(doc),
                event_tx: event_tx.into(),
                source_blocks: BTreeMap::new(),
                source_tx: source_tx.into(),
                config_dir: std::env::temp_dir(),
                app_defaults: runner::AppDefaults::default(),
            },
            rx,
        );
        (tx.into(), event_rx, vars)
    }

    #[test]
    fn params_are_typed_checked_defaulted_or_asked() {
        let doc = parse_doc(
            "action a {\n\
                 param n: int { min = 1; max = 10 }\n\
                 param mode: string = fast { options = [fast, slow] }\n\
                 param m: int { ask = select; options = [5, 30] }\n\
                 run = /bin/true\n}\n",
        );
        let def = collect(&doc).remove("a").unwrap();
        let ctx = Context::new();
        let arg = |k: &str, v: Value| ActionArg { name: k.into(), value: v };
        let prep = |args: Vec<ActionArg>| prepare_args(&def, args, &ctx, true);
        let s = |t: &str| Value::String(t.into());

        // All given: typed (the keypad's "7" is an int) and checked.
        match prep(vec![arg("n", s("7")), arg("mode", s("slow")), arg("m", Value::Int(30)), arg("item", s("x"))]) {
            Prepared::Ready(a) => {
                assert_eq!(a.iter().find(|a| a.name == "n").unwrap().value, Value::Int(7));
                assert!(a.iter().any(|a| a.name == "item"), "undeclared args pass through");
            }
            _ => panic!("should be ready"),
        }
        let refused = |args: Vec<ActionArg>, why: &str| match prep(args) {
            Prepared::Refused(m) => assert!(m.contains(why), "{m}"),
            Prepared::Ready(a) => panic!("should be refused: {why}; ready {a:?}; options {:?}", def.params[1].option_values(&ctx)),
            Prepared::Ask(p) => panic!("should be refused: {why}; asks {}", p.name),
        };
        refused(vec![arg("n", s("x7")), arg("m", Value::Int(5))], "is not an int");
        // Review round 9: 10.9 must not become 10 and slip under max.
        refused(vec![arg("n", Value::Double(9.5)), arg("m", Value::Int(5))], "is not an int");
        refused(vec![arg("n", Value::Bool(true)), arg("m", Value::Int(5))], "is not an int");
        refused(vec![arg("n", Value::Int(11)), arg("m", Value::Int(5))], "above 10");
        refused(vec![arg("n", Value::Int(0)), arg("m", Value::Int(5))], "below 1");
        // `pattern`: the whole text.
        let doc = parse_doc("action u { param unit: string { pattern = \"[a-z0-9][a-z0-9-]*\" }\n run = /bin/true }\n");
        let def = collect(&doc).remove("u").unwrap();
        let check = |v: &str| matches!(prepare_args(&def, vec![arg("unit", s(v))], &ctx, true), Prepared::Ready(_));
        assert!(check("nginx"));
        assert!(!check("--force"), "a leading option");
        assert!(!check("nginx; reboot"));
        assert!(!check(""));
        refused(vec![arg("n", Value::Int(2)), arg("mode", s("turbo")), arg("m", Value::Int(5))], "not one of");
        refused(vec![arg("m", Value::Int(5))], "`n` was not given");
        // `mode` defaults; `m` is asked.
        match prep(vec![arg("n", Value::Int(2))]) {
            Prepared::Ask(p) => assert_eq!(p.name, "m"),
            _ => panic!("should ask for m"),
        }
        match prep(vec![arg("n", Value::Int(2)), arg("m", Value::Int(5))]) {
            Prepared::Ready(a) => assert_eq!(a.iter().find(|a| a.name == "mode").unwrap().value, s("fast")),
            _ => panic!("should be ready"),
        }
        // Built-ins: missing params stay missing.
        assert!(matches!(prepare_args(&def, Vec::new(), &ctx, false), Prepared::Ready(a) if a.is_empty()));
    }

    #[test]
    fn sets_keep_their_order_and_no_step_is_lost() {
        // Review round 7: per-call threads let `two` finish before
        // `one`, and two counters read the same old value.
        let doc = parse_doc(
            "var n: int = 0\nvar c: int = 0\n\
             action one { set { n = 1 } }\naction two { set { n = 2 } }\n\
             action up_a { run = /bin/true\n set { c = {{ vars.c + 1 }} } }\n\
             action up_b { run = /bin/true\n set { c = {{ vars.c + 1 }} } }\n",
        );
        let (tx, events, vars) = worker_full(&doc, BTreeMap::new());
        let call = |n: &str| Call { name: n.into(), args: Vec::new(), cleared: true };
        let wait = |count: usize| {
            let mut done = 0;
            while done < count {
                if let UiEvent::ActionDone { .. } = events.recv_timeout(Duration::from_secs(10)).unwrap() {
                    done += 1;
                }
            }
        };
        for _ in 0..50 {
            tx.send(call("one")).unwrap();
            tx.send(call("two")).unwrap();
            wait(2);
            assert_eq!(vars.lock().unwrap()["n"], Value::Int(2));
        }
        for _ in 0..25 {
            tx.send(call("up_a")).unwrap();
            tx.send(call("up_b")).unwrap();
        }
        wait(50);
        assert_eq!(vars.lock().unwrap()["c"], Value::Int(50));
    }

    #[test]
    fn different_actions_run_side_by_side_and_calls_of_one_queue() {
        // Review round 6: everything ran one after another on one
        // thread. Review round 7: calls of one action must keep their
        // order (a toggle switched on then off).
        let doc = parse_doc(
            "action slow { run = /bin/sleep 0.5 }\naction slower { run = /bin/sleep 0.5 }\n\
             action log { run = /bin/sh -c 'sleep $1; echo $2' x {{ wait }} {{ tag }} }\n",
        );
        let (tx, events) = worker_for(&doc);
        let arg = |k: &str, v: &str| ActionArg { name: k.into(), value: Value::String(v.into()) };
        assert_ne!(run_key("slow", &[arg("vm", "a")]), run_key("slow", &[arg("vm", "b")]));
        let started = std::time::Instant::now();
        for name in ["slow", "slower"] {
            tx.send(Call { name: name.into(), args: Vec::new(), cleared: true }).unwrap();
        }
        tx.send(Call::new("nope".into(), Vec::new())).unwrap();
        let mut keys = Vec::new();
        while keys.len() < 3 {
            if let UiEvent::ActionDone { key, .. } = events.recv_timeout(Duration::from_secs(5)).unwrap() {
                keys.push(key);
            }
        }
        assert!(started.elapsed() < Duration::from_millis(950), "ran one after another: {:?}", started.elapsed());
        keys.sort();
        assert_eq!(keys, [run_key("nope", &[]), run_key("slow", &[]), run_key("slower", &[])]);

        // The first call is slower than the second, yet finishes first.
        for (wait, tag) in [("0.3", "first"), ("0", "second")] {
            tx.send(Call { name: "log".into(), args: vec![arg("wait", wait), arg("tag", tag)], cleared: true }).unwrap();
        }
        let mut order = Vec::new();
        while order.len() < 2 {
            if let UiEvent::ActionDone { result: Value::Map(m), .. } = events.recv_timeout(Duration::from_secs(5)).unwrap() {
                order.push(m["stdout"].to_display_string().trim().to_string());
            }
        }
        assert_eq!(order, ["first", "second"]);
    }

    #[test]
    fn command_output_has_secrets_masked() {
        // Review round 6: a command echoing its argv put the token on
        // the screen (failed toast, `result`) and in the journal.
        let doc = parse_doc("action leak { run = /bin/sh -c 'echo \"$1\"; echo \"$1\" >&2; exit 1' x {{ secrets.tok }} }\n");
        let mut secrets = BTreeMap::new();
        secrets.insert("tok".to_string(), Value::String("hunter2-token".into()));
        let (tx, events) = worker_with(&doc, secrets);
        tx.send(Call { name: "leak".into(), args: Vec::new(), cleared: true }).unwrap();
        loop {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::ActionDone { ok, result, .. }) => {
                    assert!(!ok);
                    let Value::Map(m) = result else { panic!("result is a map") };
                    assert_eq!(m["stdout"], Value::String("****\n".into()));
                    assert_eq!(m["stderr"], Value::String("****\n".into()));
                    break;
                }
                Ok(_) => continue,
                Err(e) => panic!("no ActionDone: {e}"),
            }
        }
    }

    #[test]
    fn chained_and_uncleared_gated_actions_never_run() {
        // Review round 4, finding 1: `then` into a `pin = true` action
        // must go back through the main loop's gates, and the worker
        // must refuse a gated call that did not pass them.
        let marker = std::env::temp_dir().join(format!(
            "paneld-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let src = format!(
            "action start {{ run = /bin/true\n then = boom }}\n\
             action boom {{ pin = true\n run = /bin/touch {} }}\n",
            marker.display()
        );
        let doc = parse_doc(&src);
        let (tx, events) = worker_for(&doc);

        // The next event that is not an `ActionDone`.
        let next_action = || loop {
            match events.recv_timeout(Duration::from_secs(5)) {
                Ok(UiEvent::ActionDone { .. }) => continue,
                other => return other,
            }
        };

        tx.send(Call::new("start".into(), Vec::new())).unwrap();
        match next_action() {
            Ok(UiEvent::Chained { name, args }) => {
                assert_eq!(name, "boom");
                // `then` passes the schema's `result` along.
                assert!(args.iter().any(|a| a.name == "result"));
            }
            other => panic!("expected the chain to come back as Chained, got {other:?}"),
        }

        // A direct uncleared call is refused too.
        tx.send(Call::new("boom".into(), Vec::new())).unwrap();
        // A cleared no-op afterwards proves the worker processed the
        // refused call before we look at the marker.
        tx.send(Call { name: "start".into(), args: Vec::new(), cleared: true }).unwrap();
        let _ = next_action();
        assert!(!marker.exists(), "gated action ran without passing its gate");

        // And a cleared call does run.
        tx.send(Call { name: "boom".into(), args: Vec::new(), cleared: true }).unwrap();
        let ran = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            marker.exists()
        });
        let _ = std::fs::remove_file(&marker);
        assert!(ran, "cleared gated call should run");
    }

    #[test]
    fn action_cwd_defaults_and_overrides() {
        use std::path::{Path, PathBuf};
        let base = Path::new("/etc/paneld");
        assert_eq!(resolve_action_cwd(base, None), PathBuf::from("/etc/paneld"));
        assert_eq!(resolve_action_cwd(base, Some("  ")), PathBuf::from("/etc/paneld"));
        assert_eq!(resolve_action_cwd(base, Some("scripts")), PathBuf::from("/etc/paneld/scripts"));
        assert_eq!(resolve_action_cwd(base, Some("/srv")), PathBuf::from("/srv"));
    }

    #[test]
    fn inline_handlers_with_action_keys_become_gated_actions() {
        let doc = parse_doc(
            "page home {\n\
                 button { on_tap { set { x = 1 }; pin = true } }\n\
                 button { on_tap { goto = home } }\n\
                 repeat i in {{ [1] }} { button { on_hold { run = /bin/true } } }\n\
             }\n",
        );
        let actions = collect(&doc);
        let inline: Vec<&ActionDef> = actions
            .iter()
            .filter(|(k, _)| k.starts_with("@inline:"))
            .map(|(_, v)| v)
            .collect();
        // The pure `goto` shortcut stays a shortcut.
        assert_eq!(inline.len(), 2);
        assert!(inline.iter().any(|d| d.pin && is_gated(d) && d.set.len() == 1));
        assert!(inline.iter().any(|d| d.run.as_deref() == Some("/bin/true")));
    }

    #[test]
    fn collects_pin_confirm_and_timeout() {
        let doc = parse_doc(
            "action a { pin = true\n confirm = Sure?\n timeout = 3s\n run = /bin/true }\n",
        );
        let a = collect(&doc).remove("a").unwrap();
        assert!(a.pin);
        assert_eq!(a.confirm.unwrap().raw.trim(), "Sure?");
        assert_eq!(a.timeout, Some(Duration::from_secs(3)));
        assert!(is_gated(&collect(&doc)["a"]));
    }

    #[test]
    fn ctx_overlay_reads_live_source_values() {
        // Pre-existing bug fix: the action worker now sees fresh
        // sources (not a daemon-start snapshot). Push a value into
        // the shared map and prove `ctx.sources` reflects it.
        let base = Context::new();
        let sources = Arc::new(Mutex::new(BTreeMap::new()));
        let vars = Arc::new(Mutex::new(BTreeMap::new()));
        {
            let mut g = sources.lock().unwrap();
            let mut inner = BTreeMap::new();
            inner.insert("value".to_string(), Value::Int(42));
            g.insert("cpu".to_string(), Value::Map(inner));
        }
        let ctx = ctx_with_vars_and_args(&base, &sources, &vars, &[]);
        let Some(Value::Map(ref smap)) = ctx.get("sources") else {
            panic!("sources missing");
        };
        let Some(Value::Map(ref cpu)) = smap.get("cpu") else {
            panic!("cpu missing");
        };
        assert_eq!(cpu.get("value"), Some(&Value::Int(42)));
    }

    #[test]
    fn an_inline_set_skips_what_it_cannot_set_and_reads_its_locals() {
        let types = BTreeMap::from([("n".to_string(), "int".to_string()), ("s".to_string(), "string".to_string())]);
        let vars = Arc::new(Mutex::new(BTreeMap::new()));
        let item = |key: &str, raw: &str, locals: Vec<ActionArg>| InlineSetItem { key: key.into(), value_raw: raw.into(), locals };
        let ctx = Context::new();
        // Undeclared, not evaluating, the wrong type: nothing changes.
        assert!(!apply_inline_set(&[item("x", "1", vec![])], &ctx, &types, &vars));
        assert!(!apply_inline_set(&[item("n", "{{ nope }}", vec![])], &ctx, &types, &vars));
        assert!(!apply_inline_set(&[item("n", "{{ [1] }}", vec![])], &ctx, &types, &vars));
        assert!(vars.lock().unwrap().is_empty());
        // A local binding (a repeat item) is in scope, and wins over a
        // global of the same name (a remote called like the item).
        let row = ActionArg { name: "row".into(), value: Value::Map(BTreeMap::from([("id".to_string(), Value::Int(7))])) };
        assert!(apply_inline_set(&[item("n", "{{ row.id * 2 }}", vec![row])], &ctx, &types, &vars));
        assert_eq!(vars.lock().unwrap()["n"], Value::Int(14));
        let mut g = Context::new();
        g.insert("who".into(), Value::String("global".into()));
        let local = ActionArg { name: "who".into(), value: Value::String("local".into()) };
        assert!(apply_inline_set(&[item("s", "{{ who }}", vec![local])], &g, &types, &vars));
        assert_eq!(vars.lock().unwrap()["s"], Value::String("local".into()));
        // Not over the event's value.
        let mut e = Context::new();
        e.insert("value".into(), Value::String("new".into()));
        let stale = ActionArg { name: "value".into(), value: Value::String("old".into()) };
        assert!(apply_inline_set(&[item("s", "{{ value }}", vec![stale])], &e, &types, &vars));
        assert_eq!(vars.lock().unwrap()["s"], Value::String("new".into()));
    }
}