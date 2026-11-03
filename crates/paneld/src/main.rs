//! paneld: a generic touch panel.
//!
//! The binary has three modes:
//!   paneld             -> run the daemon (not implemented yet)
//!   paneld check [-c]  -> validate a config offline
//!   paneld ctl ...     -> talk to a running daemon over a Unix socket
//!
//! See `docs/architecture.md` and ADR 0007.

mod actions;
mod alerts;
mod ctl;
mod display;
mod runner;
mod secrets;
mod sources;
mod state;
mod themes;
mod vars;

use std::path::PathBuf;
use std::process::ExitCode;

const DEFAULT_CONFIG: &str = "/etc/paneld/paneld.conf";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Cmd::Help) => {
            print_help();
            ExitCode::SUCCESS
        }
        Ok(Cmd::Version) => {
            println!("paneld {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Cmd::Run { config }) => run_daemon(config),
        Ok(Cmd::Check { config }) => run_check(config),
        Ok(Cmd::Ctl { args }) => run_ctl(&args),
        Err(e) => {
            eprintln!("paneld: {e}");
            eprintln!("try `paneld --help`");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Help,
    Version,
    Run { config: PathBuf },
    Check { config: PathBuf },
    Ctl { args: Vec<String> },
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let mut config: Option<PathBuf> = None;
    let mut i = 0;

    // Flags before the subcommand.
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            "-c" | "--config" => {
                i += 1;
                let v = args.get(i).ok_or("`--config` needs a path")?;
                config = Some(PathBuf::from(v));
                i += 1;
            }
            s if s.starts_with("--config=") => {
                config = Some(PathBuf::from(&s["--config=".len()..]));
                i += 1;
            }
            s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
            _ => break,
        }
    }

    let sub = args.get(i).map(String::as_str);
    i += 1;

    match sub {
        None => Ok(Cmd::Run { config: config.unwrap_or_else(default_config) }),
        Some("check") => {
            // `--config` is also accepted after `check`.
            while i < args.len() {
                match args[i].as_str() {
                    "-h" | "--help" => return Ok(Cmd::Help),
                    "-c" | "--config" => {
                        i += 1;
                        let v = args.get(i).ok_or("`--config` needs a path")?;
                        config = Some(PathBuf::from(v));
                        i += 1;
                    }
                    s if s.starts_with("--config=") => {
                        config = Some(PathBuf::from(&s["--config=".len()..]));
                        i += 1;
                    }
                    s if s.starts_with('-') => return Err(format!("unknown option `{s}`")),
                    s => return Err(format!("unexpected argument `{s}` to `check`")),
                }
            }
            Ok(Cmd::Check { config: config.unwrap_or_else(default_config) })
        }
        Some("ctl") => Ok(Cmd::Ctl { args: args[i..].to_vec() }),
        Some(other) if other.starts_with('-') => Err(format!("unknown option `{other}`")),
        Some(other) => Err(format!("unknown subcommand `{other}`")),
    }
}

fn default_config() -> PathBuf {
    PathBuf::from(DEFAULT_CONFIG)
}

fn print_help() {
    println!(
        "paneld {} - a generic touch panel

Usage:
  paneld [--config PATH]              Run the daemon
  paneld check [--config PATH]        Validate a config, offline
  paneld ctl SUBCOMMAND [ARGS...]     Talk to a running daemon

Flags:
  -c, --config PATH   Config file (default: {DEFAULT_CONFIG})
  -h, --help          Show this help
  -V, --version       Show the version",
        env!("CARGO_PKG_VERSION")
    );
}

fn run_daemon(config: PathBuf) -> ExitCode {
    let ui = match paneld_ui::Ui::init() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("paneld: {e}");
            return ExitCode::from(70);
        }
    };

    // Start the ctl listener early so `paneld ctl exit` works even
    // if the first config fails to load.
    let (control_tx, control_rx) = std::sync::mpsc::channel::<ctl::ControlMsg>();
    let sock_path = ctl::default_socket_path();
    if let Err(e) = ctl::serve(&sock_path, control_tx.clone()) {
        eprintln!(
            "paneld: cannot bind ctl socket {}: {e}; `paneld ctl` will not work",
            sock_path.display()
        );
    }

    // Also watch the config file: an editor save fires Reload the
    // same way `paneld ctl reload` does.
    ctl::watch_config(
        &config,
        std::time::Duration::from_millis(500),
        control_tx,
    );

    // Peek at the config just to pick the display backend. The
    // live state will reload the full config a moment later.
    let display_cfg = match paneld_config::load(&config) {
        Ok((_, doc, _)) => display::DisplayConfig::from_doc(&doc),
        Err(_) => display::DisplayConfig::default(),
    };
    let _window = match display_cfg.open(&ui) {
        Some(w) => w,
        None => {
            eprintln!(
                "paneld: display backend {:?} failed to open; try `backend = sdl` or check the device path",
                display_cfg.backend
            );
            return ExitCode::from(70);
        }
    };
    let screen = ui.screen();

    // Initial live state. `reload` wipes and rebuilds this.
    let mut live = match setup_live_state(&config, &screen) {
        Ok(l) => l,
        Err(code) => return code,
    };

    let mut pump = ui.pump();
    'main: loop {
        // Drain the ctl channel.
        while let Ok(msg) = control_rx.try_recv() {
            match msg {
                ctl::ControlMsg::Exit => break 'main,
                ctl::ControlMsg::Reload => match setup_live_state(&config, &screen) {
                    Ok(fresh) => {
                        // Flush any pending alert state before the
                        // old Store is dropped, so a reload during
                        // a long incident doesn't lose timestamps
                        // that hadn't aged into the 10s window yet.
                        live.state.force_flush();
                        live = fresh;
                        eprintln!("paneld: reloaded config");
                    }
                    Err(_code) => {
                        eprintln!("paneld: reload failed; keeping previous config");
                    }
                },
            }
        }

        // Drain UI events: actions forward to the action worker,
        // gotos update the current page, vars changes trigger a
        // re-render (the worker already wrote the new values).
        let mut dirty = false;
        while let Ok(ev) = live.event_rx.try_recv() {
            match ev {
                paneld_ui::UiEvent::Action { name, args } => {
                    let _ = live.action_tx.send(actions::Call { name, args });
                }
                paneld_ui::UiEvent::Goto { page } => {
                    if page != live.current_page {
                        live.current_page = page;
                        dirty = true;
                    }
                }
                paneld_ui::UiEvent::InlineSet { assigns } => {
                    let ctx = build_ctx(
                        &live.source_values,
                        &live.theme,
                        &live.secrets,
                        &live.vars,
                        Some(&live.alerts),
                    );
                    if actions::apply_inline_set(
                        &assigns,
                        &ctx,
                        &live.var_types,
                        &live.vars,
                    ) {
                        dirty = true;
                    }
                }
                paneld_ui::UiEvent::ChangeInlineSet { assigns, event_value } => {
                    let mut ctx = build_ctx(
                        &live.source_values,
                        &live.theme,
                        &live.secrets,
                        &live.vars,
                        Some(&live.alerts),
                    );
                    let v = match event_value {
                        paneld_ui::EventValue::Bool(b) => paneld_ui::Value::Bool(b),
                        paneld_ui::EventValue::Int(n) => paneld_ui::Value::Int(n),
                        paneld_ui::EventValue::Double(d) => paneld_ui::Value::Double(d),
                    };
                    let mut event = std::collections::BTreeMap::new();
                    event.insert("value".to_string(), v);
                    ctx.insert("event".into(), paneld_ui::Value::Map(event));
                    if actions::apply_inline_set(
                        &assigns,
                        &ctx,
                        &live.var_types,
                        &live.vars,
                    ) {
                        dirty = true;
                    }
                }
                paneld_ui::UiEvent::VarsChanged => {
                    dirty = true;
                }
            }
        }

        // Drain source updates and re-render if anything changed.
        // Each new source value is also an event for any
        // `alert ... { from = <this source> }` rule. Insert the new
        // sample first so `sources.<name>.value` inside the rule's
        // expressions agrees with `event.value`, then build the eval
        // ctx including the current `alerts` map for symmetry with
        // the render-side ctx.
        while let Ok(update) = live.source_rx.try_recv() {
            let event_value = update.value.clone();
            live.source_values.insert(update.name.clone(), update.value);
            let event_ctx = build_ctx(
                &live.source_values,
                &live.theme,
                &live.secrets,
                &live.vars,
                Some(&live.alerts),
            );
            let _ = live.alerts.ingest_event(
                &update.name,
                &event_value,
                &event_ctx,
                std::time::SystemTime::now(),
            );
            dirty = true;
        }

        // Tick the alert engine against the latest sources + vars. A
        // transition (fire or resolve) marks the frame dirty so the
        // header badge and overview reflect it right away.
        let alerts_ctx = build_ctx(
            &live.source_values,
            &live.theme,
            &live.secrets,
            &live.vars,
            None,
        );
        let now_instant = std::time::Instant::now();
        if live.alerts.tick(
            &alerts_ctx,
            now_instant,
            std::time::SystemTime::now(),
        ) {
            dirty = true;
        }
        // Keep the state file (ADR 0014) in sync. `note` is cheap:
        // it only flags a pending write when the set actually
        // changed. `flush_if_due` writes at most once per 10s.
        live.state.note(live.alerts.active_alerts());
        live.state.flush_if_due(now_instant);

        if dirty {
            let ctx = build_ctx(
                &live.source_values,
                &live.theme,
                &live.secrets,
                &live.vars,
                Some(&live.alerts),
            );
            screen.clear_children();
            paneld_ui::reset_action_bindings();
            paneld_ui::render_page(
                &live.doc,
                &live.current_page,
                &ctx,
                &live.event_tx,
                &screen,
            );
        }

        pump.step();
    }

    live.state.force_flush();
    let _ = std::fs::remove_file(&sock_path);
    ExitCode::SUCCESS
}

/// Everything the main loop re-creates on a `reload`.
struct LiveState {
    doc: paneld_config::parse::Document,
    source_values: std::collections::BTreeMap<String, paneld_ui::Value>,
    source_rx: std::sync::mpsc::Receiver<sources::SourceUpdate>,
    theme: std::collections::BTreeMap<String, paneld_ui::Value>,
    secrets: std::collections::BTreeMap<String, paneld_ui::Value>,
    vars: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>>,
    var_types: std::collections::BTreeMap<String, String>,
    event_tx: std::sync::mpsc::Sender<paneld_ui::UiEvent>,
    event_rx: std::sync::mpsc::Receiver<paneld_ui::UiEvent>,
    action_tx: std::sync::mpsc::Sender<actions::Call>,
    current_page: String,
    alerts: alerts::Engine,
    state: state::Store,
}

fn setup_live_state(
    config: &std::path::Path,
    screen: &paneld_ui::ui::Obj<'_>,
) -> Result<LiveState, ExitCode> {
    use paneld_config::{check, load};
    let (source, doc, mut diags) = match load(config) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("paneld: {e}");
            return Err(ExitCode::from(1));
        }
    };
    diags.extend(check(&doc, &source));
    if !diags.is_empty() {
        for d in &diags {
            let loc = source.location(d.span);
            eprintln!("{loc}: {}", d.message);
        }
        eprintln!(
            "paneld: {} config issue{} found; refusing to apply",
            diags.len(),
            if diags.len() == 1 { "" } else { "s" }
        );
        return Err(ExitCode::from(1));
    }

    // Resolve secrets first so they're available for the first
    // source-run's interpolation. (Secrets never change at runtime
    // in v1; a reload re-reads them.)
    let secret_values = secrets::resolve(&doc);
    let theme = themes::resolve(&doc);
    let vars_map = vars::initial_values(&doc);
    let var_types = vars::types(&doc);
    let vars_shared = std::sync::Arc::new(std::sync::Mutex::new(vars_map));
    let base_ctx =
        build_ctx(&Default::default(), &theme, &secret_values, &vars_shared, None);
    let source_values = sources::resolve_all(&doc, &base_ctx);
    let (source_tx, source_rx) =
        std::sync::mpsc::channel::<sources::SourceUpdate>();
    sources::spawn_pollers(&doc, &base_ctx, source_tx.clone());
    let action_table = actions::collect(&doc);
    let source_blocks = sources::collect_blocks(&doc);

    // One channel carries actions, goto navs and vars-changed events.
    // The main loop demuxes.
    let (event_tx, event_rx) = std::sync::mpsc::channel::<paneld_ui::UiEvent>();
    let (action_tx, action_rx) = std::sync::mpsc::channel::<actions::Call>();
    actions::spawn_worker(
        actions::WorkerCtx {
            actions: action_table,
            base_ctx: base_ctx.clone(),
            vars: vars_shared.clone(),
            var_types: var_types.clone(),
            event_tx: event_tx.clone(),
            source_blocks,
            source_tx,
            self_tx: action_tx.clone(),
        },
        action_rx,
    );

    let current_page = find_start_page(&doc).unwrap_or_else(|| "home".to_string());
    let mut alerts_engine = alerts::Engine::new(&doc);
    let state_store = state::Store::open(state::config_path(&doc));
    alerts_engine.restore(
        state_store
            .snapshot()
            .iter()
            .map(|(k, v)| (k.clone(), std::time::UNIX_EPOCH + std::time::Duration::from_secs(v.since)))
            .collect(),
    );
    let ctx_no_alerts =
        build_ctx(&source_values, &theme, &secret_values, &vars_shared, None);
    alerts_engine.tick(
        &ctx_no_alerts,
        std::time::Instant::now(),
        std::time::SystemTime::now(),
    );
    let ctx = build_ctx(
        &source_values,
        &theme,
        &secret_values,
        &vars_shared,
        Some(&alerts_engine),
    );
    screen.clear_children();
    paneld_ui::reset_action_bindings();
    paneld_ui::render_page(&doc, &current_page, &ctx, &event_tx, screen);

    Ok(LiveState {
        doc,
        source_values,
        source_rx,
        theme,
        secrets: secret_values,
        vars: vars_shared,
        var_types,
        event_tx,
        event_rx,
        action_tx,
        current_page,
        alerts: alerts_engine,
        state: state_store,
    })
}

fn find_start_page(doc: &paneld_config::parse::Document) -> Option<String> {
    use paneld_config::parse::{AssignTarget, Item};
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "app" {
                for inner in &b.items {
                    if let Item::Assign(a) = inner {
                        if let AssignTarget::Simple { key } = &a.target {
                            if key.text == "start_page" {
                                return Some(a.value.text.trim().to_string());
                            }
                        }
                    }
                }
            }
        }
        if let Item::Assign(a) = item {
            if let AssignTarget::Qualified { category, key } = &a.target {
                if category.text == "app" && key.text == "start_page" {
                    return Some(a.value.text.trim().to_string());
                }
            }
        }
    }
    None
}

fn run_check(config: PathBuf) -> ExitCode {
    use paneld_config::{check, load};

    let (source, doc, mut diags) = match load(&config) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("paneld check: {e}");
            return ExitCode::from(1);
        }
    };
    diags.extend(check(&doc, &source));

    if diags.is_empty() {
        println!("{} ok", config.display());
        return ExitCode::SUCCESS;
    }

    for d in &diags {
        let loc = source.location(d.span);
        eprintln!("{loc}: {}", d.message);
    }
    eprintln!(
        "paneld check: {} issue{} found",
        diags.len(),
        if diags.len() == 1 { "" } else { "s" }
    );
    ExitCode::from(1)
}

fn run_ctl(args: &[String]) -> ExitCode {
    let Some(cmd) = args.first() else {
        eprintln!("paneld ctl: missing subcommand (try `ping`, `reload`, `exit`)");
        return ExitCode::from(2);
    };
    let path = ctl::default_socket_path();
    let code = ctl::call(&path, cmd);
    ExitCode::from(code as u8)
}

fn build_ctx(
    source_values: &std::collections::BTreeMap<String, paneld_ui::Value>,
    theme: &std::collections::BTreeMap<String, paneld_ui::Value>,
    secret_values: &std::collections::BTreeMap<String, paneld_ui::Value>,
    vars: &std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>>,
    alerts: Option<&alerts::Engine>,
) -> paneld_ui::Context {
    let mut ctx = paneld_ui::Context::new();
    ctx.insert("sources".into(), paneld_ui::Value::Map(source_values.clone()));
    ctx.insert("theme".into(), paneld_ui::Value::Map(theme.clone()));
    ctx.insert("secrets".into(), paneld_ui::Value::Map(secret_values.clone()));
    let snapshot = vars.lock().unwrap().clone();
    ctx.insert("vars".into(), paneld_ui::Value::Map(snapshot));
    if let Some(engine) = alerts {
        ctx.insert("alerts".into(), engine.context_value());
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cmd, String> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_args_is_run_with_default_config() {
        assert_eq!(parse(&[]), Ok(Cmd::Run { config: default_config() }));
    }

    #[test]
    fn help_short_and_long() {
        assert_eq!(parse(&["-h"]), Ok(Cmd::Help));
        assert_eq!(parse(&["--help"]), Ok(Cmd::Help));
    }

    #[test]
    fn version_short_and_long() {
        assert_eq!(parse(&["-V"]), Ok(Cmd::Version));
        assert_eq!(parse(&["--version"]), Ok(Cmd::Version));
    }

    #[test]
    fn config_before_subcommand() {
        assert_eq!(
            parse(&["-c", "/tmp/x.conf", "check"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
        assert_eq!(
            parse(&["--config=/tmp/x.conf", "check"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
    }

    #[test]
    fn config_after_check_subcommand() {
        assert_eq!(
            parse(&["check", "--config", "/tmp/x.conf"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
        assert_eq!(
            parse(&["check", "--config=/tmp/x.conf"]),
            Ok(Cmd::Check { config: PathBuf::from("/tmp/x.conf") })
        );
    }

    #[test]
    fn help_after_check_is_help() {
        assert_eq!(parse(&["check", "--help"]), Ok(Cmd::Help));
    }

    #[test]
    fn check_rejects_unexpected_positional_args() {
        assert!(parse(&["check", "garbage"]).is_err());
        assert!(parse(&["check", "foo", "bar"]).is_err());
    }

    #[test]
    fn check_rejects_unknown_flags() {
        let err = parse(&["check", "--nope"]).unwrap_err();
        assert!(err.contains("unknown option"), "got: {err}");
    }

    #[test]
    fn unknown_subcommand_error() {
        let err = parse(&["banana"]).unwrap_err();
        assert!(err.contains("unknown subcommand"), "got: {err}");
    }

    #[test]
    fn unknown_global_flag_error() {
        let err = parse(&["--whatever"]).unwrap_err();
        assert!(err.contains("unknown option"), "got: {err}");
    }

    #[test]
    fn ctl_passes_through_trailing_args() {
        assert_eq!(
            parse(&["ctl", "status"]),
            Ok(Cmd::Ctl { args: vec!["status".into()] })
        );
        assert_eq!(
            parse(&["ctl", "set", "foo=1", "bar=2"]),
            Ok(Cmd::Ctl { args: vec!["set".into(), "foo=1".into(), "bar=2".into()] })
        );
    }

    #[test]
    fn config_without_value_is_error() {
        assert!(parse(&["-c"]).is_err());
        assert!(parse(&["check", "-c"]).is_err());
    }
}
