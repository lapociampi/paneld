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
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use paneld_config::parse::{AssignTarget, Block, Document, Item, RawValue};
use paneld_ui::{ActionArg, Context, InlineSetItem, UiEvent, Value};

use crate::runner;
use crate::vars;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_BYTES: usize = 64 * 1024;

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
        if b.kind.text != "action" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        let mut def = ActionDef::default();
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
        out.insert(name.text.clone(), def);
    }
    out
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
    pub base_ctx: Context,
    pub vars: Arc<Mutex<BTreeMap<String, Value>>>,
    pub var_types: BTreeMap<String, String>,
    pub event_tx: Sender<UiEvent>,
    pub source_blocks: BTreeMap<String, Block>,
    pub source_tx: Sender<crate::sources::SourceUpdate>,
    /// The worker's own input, cloned so it can re-queue `then` /
    /// `else` chained actions behind any already-pending work.
    pub self_tx: Sender<Call>,
}

/// One queued action invocation: its name plus the args already
/// evaluated at render time (empty for `on_tap = foo`).
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub args: Vec<ActionArg>,
}

/// Spawn the action worker. Each incoming [`Call`] is looked up in
/// `actions`, its `run` interpolated and executed in its own short-
/// lived thread so a slow action never blocks the UI event stream.
/// `set` is applied on the worker thread after the run returns.
pub fn spawn_worker(wctx: WorkerCtx, rx: Receiver<Call>) {
    thread::spawn(move || {
        for call in rx {
            let Some(def) = wctx.actions.get(&call.name).cloned() else {
                eprintln!("paneld: action `{}`: unknown action", call.name);
                continue;
            };

            let name = call.name;
            let had_run = def.run.is_some();
            let mut ok = true;

            if let Some(raw) = def.run {
                let ctx = ctx_with_vars_and_args(&wctx.base_ctx, &wctx.vars, &call.args);
                let file = paneld_config::source::FileId::default();
                let cmd = paneld_ui::interpolate(&raw, file, 0, &ctx);
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
                let opts = runner::Opts {
                    timeout: DEFAULT_TIMEOUT,
                    max_bytes: DEFAULT_MAX_BYTES,
                    kind: runner::Kind::Action,
                    name: &name,
                    cwd: None,
                    env: &env_pairs,
                };
                match runner::exec(&cmd, opts) {
                    Ok(r) => {
                        eprintln!(
                            "paneld: action `{name}`: exit {:?}, timed_out {}",
                            r.exit_code, r.timed_out
                        );
                        if !r.stdout.trim().is_empty() {
                            eprintln!("paneld: action `{name}` stdout: {}", r.stdout.trim());
                        }
                        if !r.stderr.trim().is_empty() {
                            eprintln!("paneld: action `{name}` stderr: {}", r.stderr.trim());
                        }
                        if !r.timed_out {
                            ok = r.exit_code == Some(0);
                        } else {
                            ok = false;
                        }
                    }
                    Err(e) => {
                        eprintln!("paneld: action `{name}`: {e}");
                        ok = false;
                    }
                }
            }

            // `set` runs after a successful `run`, or on an action that
            // has no `run` at all (schema allows it: "an action may
            // only change the UI").
            let should_set = (had_run && ok) || !had_run;
            if should_set && !def.set.is_empty() {
                let ctx = ctx_with_vars_and_args(&wctx.base_ctx, &wctx.vars, &call.args);
                apply_set(
                    &name,
                    &def.set,
                    &ctx,
                    &wctx.var_types,
                    &wctx.vars,
                    &wctx.event_tx,
                );
            }

            // `then` / `else` chain to another action on success /
            // failure. Chained actions start with empty args; the
            // `result.*` identifier they can read, and the block
            // form `then foo { arg = ... }` come in a later phase.
            if had_run && ok {
                if let Some(next) = &def.then {
                    let _ = wctx.self_tx.send(Call {
                        name: next.clone(),
                        args: Vec::new(),
                    });
                }
            } else if had_run && !ok {
                if let Some(next) = &def.else_ {
                    let _ = wctx.self_tx.send(Call {
                        name: next.clone(),
                        args: Vec::new(),
                    });
                }
            }

            // `refresh` runs after a successful `run` on the same
            // basis as `set`. Each named source is re-executed and
            // the fresh value is sent through the normal source
            // channel so the main loop treats it like a scheduled
            // poll tick.
            let should_refresh = (had_run && ok) || !had_run;
            if should_refresh && !def.refresh.is_empty() {
                let ctx = ctx_with_vars_and_args(&wctx.base_ctx, &wctx.vars, &call.args);
                for src_name in &def.refresh {
                    let Some(block) = wctx.source_blocks.get(src_name) else {
                        eprintln!(
                            "paneld: action `{name}`: refresh `{src_name}` is not a source"
                        );
                        continue;
                    };
                    match crate::sources::resolve_one(block, &ctx) {
                        Ok(v) => {
                            let _ = wctx.source_tx.send(crate::sources::SourceUpdate {
                                name: src_name.clone(),
                                value: v,
                            });
                        }
                        Err(e) => {
                            eprintln!(
                                "paneld: action `{name}`: refresh `{src_name}`: {e}"
                            );
                        }
                    }
                }
            }
        }
    });
}

/// Build an eval context from the worker-side pieces: the base ctx
/// (sources/theme/secrets, snapshotted at daemon start), a fresh
/// snapshot of `vars` and each action-call arg bound as a bare
/// identifier so `{{ vm }}` in a `run` string resolves to the arg
/// value. Args win over base ctx bindings on name clash.
fn ctx_with_vars_and_args(
    base: &Context,
    vars: &Arc<Mutex<BTreeMap<String, Value>>>,
    args: &[ActionArg],
) -> Context {
    let mut ctx = base.clone();
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
            guard.insert(a.key.clone(), coerced);
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
        let vars = Arc::new(Mutex::new(BTreeMap::new()));
        let args = vec![ActionArg {
            name: "vm".into(),
            value: Value::String("alpha".into()),
        }];
        let ctx = ctx_with_vars_and_args(&base, &vars, &args);
        assert_eq!(ctx.get("vm"), Some(&Value::String("alpha".into())));
        assert!(ctx.contains_key("vars"));
    }
}
