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
mod pin;
mod png;
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
        Ok(Cmd::HashPin) => run_hash_pin(),
        Ok(Cmd::Shot(opts)) => run_shot(opts),
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
    HashPin,
    Shot(ShotOpts),
}

/// `paneld shot`: render the config headless to a PNG.
#[derive(Debug, PartialEq, Eq)]
struct ShotOpts {
    config: PathBuf,
    out: PathBuf,
    page: Option<String>,
    size: (i32, i32),
    /// (x, y, how long the press lasts in ms), in order.
    taps: Vec<(i32, i32, u32)>,
    settle_ms: u32,
}

fn parse_shot(args: &[String], config: Option<PathBuf>) -> Result<ShotOpts, String> {
    let mut o = ShotOpts {
        config: config.unwrap_or_else(default_config),
        out: PathBuf::new(),
        page: None,
        size: (800, 480),
        taps: Vec::new(),
        settle_ms: 300,
    };
    let mut i = 0;
    let val = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i).cloned().ok_or(format!("`{flag}` needs a value"))
    };
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-c" | "--config" => o.config = PathBuf::from(val(&mut i, a)?),
            "-o" | "--out" => o.out = PathBuf::from(val(&mut i, a)?),
            "--page" => o.page = Some(val(&mut i, a)?),
            "--size" => {
                let v = val(&mut i, a)?;
                let (w, h) = v.split_once('x').ok_or("`--size` is WIDTHxHEIGHT")?;
                o.size = (
                    w.parse().map_err(|_| "bad `--size` width")?,
                    h.parse().map_err(|_| "bad `--size` height")?,
                );
                if o.size.0 <= 0 || o.size.1 <= 0 || o.size.0 > 4096 || o.size.1 > 4096 {
                    return Err("`--size` must be within 1x1..4096x4096".into());
                }
            }
            "--tap" | "--hold" => {
                let v = val(&mut i, a)?;
                let (x, y) = v.split_once(',').ok_or(format!("`{a}` is X,Y"))?;
                o.taps.push((
                    x.trim().parse().map_err(|_| format!("bad `{a}` x"))?,
                    y.trim().parse().map_err(|_| format!("bad `{a}` y"))?,
                    if a == "--hold" { 1000 } else { 100 },
                ));
            }
            "--settle" => {
                o.settle_ms = val(&mut i, a)?.parse().map_err(|_| "bad `--settle` ms")?;
            }
            s => return Err(format!("unexpected argument `{s}` to `shot`")),
        }
        i += 1;
    }
    if o.out.as_os_str().is_empty() {
        return Err("`shot` needs `-o FILE.png`".into());
    }
    Ok(o)
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
        Some("shot") => parse_shot(&args[i..], config).map(Cmd::Shot),
        Some("hash-pin") => match args.get(i) {
            None => Ok(Cmd::HashPin),
            Some(s) => Err(format!(
                "unexpected argument `{s}` to `hash-pin` (the PIN is read from stdin)"
            )),
        },
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
  paneld hash-pin                     Read a PIN on stdin, print its
                                      argon2 hash for security.pin
  paneld shot -o FILE.png [--page P] [--size WxH] [--tap X,Y]...
              [--hold X,Y]... [--settle MS]
                                      Render the config headless to a PNG,
                                      after the given taps and holds (runs
                                      sources and touched actions for real)

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
                    Ok(mut fresh) => {
                        fresh.pin_gate.carry_from(&live.pin_gate);
                        // Flush any pending alert state before the
                        // old Store is dropped, so a reload during
                        // a long incident doesn't lose timestamps
                        // that hadn't aged into the 10s window yet.
                        live.state.force_flush();
                        // Signal the previous poll/stream workers to
                        // exit so a stream source isn't duplicated
                        // after reload. The new state has already
                        // spawned its own.
                        live.sources_shutdown.stop();
                        live = fresh;
                        eprintln!("paneld: reloaded config");
                    }
                    Err(_code) => {
                        eprintln!("paneld: reload failed; keeping previous config");
                    }
                },
            }
        }

        process(&mut live, &screen);
        pump.step();
    }

    live.state.force_flush();
    live.sources_shutdown.stop();
    // Give the SIGTERMs from `stop` a brief moment to land on stream
    // children before the daemon process exits and reparents them.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = std::fs::remove_file(&sock_path);
    ExitCode::SUCCESS
}

/// One pass of the main loop after the ctl messages: UI events,
/// deferred chained calls, source updates, alerts, state file and
/// (when something changed) a re-render. Shared by the daemon and
/// `paneld shot`.
fn process(live: &mut LiveState, screen: &paneld_ui::ui::Obj<'_>) {
    // Drain UI events: actions forward to the action worker,
    // gotos update the current page, vars changes trigger a
    // re-render (the worker already wrote the new values).
    let mut dirty = false;
    while let Ok(ev) = live.event_rx.try_recv() {
        match ev {
            paneld_ui::UiEvent::Action { name, args } => {
                // Taps cannot reach here while a modal is up (the
                // page's bindings are gone); `then` / `else` chains
                // from the worker can. Hold them until the modal
                // closes so they get their own gates.
                let call = actions::Call::new(name, args);
                if live.pending.is_some() {
                    live.deferred.push_back(call);
                } else {
                    route_call(live, screen, call, false);
                }
            }
            paneld_ui::UiEvent::PinEntered { digits } => {
                // Only meaningful while the PIN modal is up; a
                // stray READY (double OK) must not consume a
                // confirm or notice stage. An empty entry is not
                // an attempt.
                let at_pin = matches!(
                    live.pending,
                    Some(Pending { stage: Stage::Pin, .. })
                );
                if at_pin && !digits.is_empty() {
                    let Some(Pending { call, .. }) = live.pending.take() else {
                        unreachable!()
                    };
                    let now = std::time::Instant::now();
                    match live.pin_gate.check(&digits, now) {
                        pin::Check::Ok => route_call(live, screen, call, true),
                        pin::Check::Wrong { remaining } => {
                            eprintln!(
                                "paneld: wrong PIN for action `{}`, {remaining} left",
                                call.name
                            );
                            show_pin_modal(
                                screen,
                                &call.name,
                                Some(&format!(
                                    "Wrong PIN, {remaining} tr{} left",
                                    if remaining == 1 { "y" } else { "ies" }
                                )),
                                &live.event_tx,
                            );
                            live.pending = Some(Pending { call, stage: Stage::Pin });
                        }
                        pin::Check::LockedOut { for_ } => {
                            eprintln!("paneld: PIN locked out for {for_:?}");
                            show_notice(
                                screen,
                                &format!("Too many wrong PINs. Locked for {}s.", for_.as_secs()),
                                &live.event_tx,
                            );
                            live.pending = Some(Pending { call, stage: Stage::Notice });
                        }
                        pin::Check::NotConfigured => {
                            show_notice(
                                screen,
                                "No PIN is configured (security.pin); action refused.",
                                &live.event_tx,
                            );
                            live.pending = Some(Pending { call, stage: Stage::Notice });
                        }
                    }
                    dirty = live.pending.is_none();
                }
            }
            paneld_ui::UiEvent::ConfirmReply { accept } => {
                if let Some(Pending { mut call, stage }) = live.pending.take() {
                    if accept && matches!(stage, Stage::Confirm) {
                        call.cleared = true;
                        let _ = live.action_tx.send(call);
                    }
                }
                dirty = true;
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
                    paneld_ui::EventValue::String(s) => paneld_ui::Value::String(s),
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

    // Chained calls that arrived while a modal was up, in order,
    // until one of them opens a modal of its own.
    while live.pending.is_none() {
        let Some(call) = live.deferred.pop_front() else { break };
        route_call(live, screen, call, false);
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
        live.source_values
            .lock()
            .unwrap()
            .insert(update.name.clone(), update.value);
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

    if dirty && live.pending.is_none() {
        let ctx = build_ctx(
            &live.source_values,
            &live.theme,
            &live.secrets,
            &live.vars,
            Some(&live.alerts),
        );
        // A poll landing mid-entry must not wipe digits the user
        // is typing into a keypad on the same page.
        let typed = paneld_ui::snapshot_inputs();
        let same_page = live.rendered_page == live.current_page;
        screen.clear_children();
        paneld_ui::reset_action_bindings();
        paneld_ui::render_page(
            &live.doc,
            &live.current_page,
            &ctx,
            &live.event_tx,
            screen,
        );
        if same_page {
            paneld_ui::restore_inputs(&typed);
        }
        live.rendered_page = live.current_page.clone();
    }
}

/// Everything the main loop re-creates on a `reload`.
struct LiveState {
    doc: paneld_config::parse::Document,
    /// Shared with the action worker so a `run =` that references
    /// `sources.X.value` sees the current sample, not whatever was
    /// in the base ctx snapshot at daemon start.
    source_values: std::sync::Arc<
        std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>,
    >,
    source_rx: std::sync::mpsc::Receiver<sources::SourceUpdate>,
    theme: std::collections::BTreeMap<String, paneld_ui::Value>,
    secrets: std::collections::BTreeMap<String, paneld_ui::Value>,
    vars: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>>,
    var_types: std::collections::BTreeMap<String, String>,
    event_tx: std::sync::mpsc::Sender<paneld_ui::UiEvent>,
    event_rx: std::sync::mpsc::Receiver<paneld_ui::UiEvent>,
    action_tx: std::sync::mpsc::Sender<actions::Call>,
    current_page: String,
    /// The page of the last full render, so input text is only
    /// restored onto the same page. (A modal resets the input list,
    /// so nothing carries across one.)
    rendered_page: String,
    alerts: alerts::Engine,
    state: state::Store,
    sources_shutdown: sources::ShutdownHandle,
    /// Clone of the action table the worker holds. Lets the main
    /// loop look up `confirm` without crossing the channel.
    actions: std::collections::HashMap<String, actions::ActionDef>,
    /// Set while a PIN / confirm / notice modal owns the screen.
    /// Blocks re-renders of the page tree so the modal stays up.
    pending: Option<Pending>,
    /// `then` / `else` calls that arrived while a modal was up.
    deferred: std::collections::VecDeque<actions::Call>,
    pin_gate: pin::Gate,
}

/// An action parked behind a modal, and which modal it is.
struct Pending {
    call: actions::Call,
    stage: Stage,
}

enum Stage {
    Pin,
    Confirm,
    /// A message with only OK (lockout, PIN not configured). The
    /// parked call is dropped on dismiss.
    Notice,
}

fn setup_live_state(
    config: &std::path::Path,
    screen: &paneld_ui::ui::Obj<'_>,
) -> Result<LiveState, ExitCode> {
    use paneld_config::{check, load};
    // ADR 0013: commands run with cwd set to the config file's
    // directory by default, so `run = ./scripts/x.sh` works. A
    // per-source / per-action `cwd` overrides on top.
    let config_dir = config_dir_of(config);
    let (source, mut doc, mut diags) = match load(config) {
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
    // `kind[name]:key = value` overrides land in their blocks so every
    // consumer below reads one form (and `action[x]:pin = true` is
    // honored).
    paneld_config::apply_overrides(&mut doc);

    // Pull the ADR 0013 defaults out once (10s timeout, 1 MiB
    // max_output unless `app:timeout` / `app:max_output` say
    // otherwise). Per-source and per-action overrides apply on top.
    let app_defaults = runner::AppDefaults::from_doc(&doc);

    // Resolve secrets first so they're available for the first
    // source-run's interpolation. (Secrets never change at runtime
    // in v1; a reload re-reads them.)
    let secret_values = secrets::resolve(&doc);
    let pin_gate = pin::Gate::from_doc(&doc, &secret_values);
    let theme = themes::resolve(&doc);
    let vars_map = vars::initial_values(&doc);
    let var_types = vars::types(&doc);
    let vars_shared = std::sync::Arc::new(std::sync::Mutex::new(vars_map));
    let source_values = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::BTreeMap::<String, paneld_ui::Value>::new(),
    ));
    let base_ctx =
        build_ctx(&source_values, &theme, &secret_values, &vars_shared, None);
    let initial_sources = sources::resolve_all(&doc, &base_ctx, &config_dir, app_defaults);
    *source_values.lock().unwrap() = initial_sources;
    let (source_tx, source_rx) =
        std::sync::mpsc::channel::<sources::SourceUpdate>();
    let sources_shutdown = sources::spawn_pollers(
        &doc,
        &base_ctx,
        source_tx.clone(),
        &config_dir,
        app_defaults,
    );
    let action_table = actions::collect(&doc);
    let action_table_for_main = action_table.clone();
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
            source_values: source_values.clone(),
            var_types: var_types.clone(),
            event_tx: event_tx.clone(),
            source_blocks,
            source_tx,
            config_dir: config_dir.clone(),
            app_defaults,
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
        rendered_page: current_page.clone(),
        current_page,
        alerts: alerts_engine,
        state: state_store,
        sources_shutdown,
        actions: action_table_for_main,
        pending: None,
        deferred: std::collections::VecDeque::new(),
        pin_gate,
    })
}

/// The directory commands run in by default (ADR 0013): the config
/// file's own. `Path::parent` gives `""` for a bare `paneld.conf`,
/// which `current_dir` rejects, so that case maps to `.`.
fn config_dir_of(config: &std::path::Path) -> std::path::PathBuf {
    match config.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    }
}

/// Send `call` through the schema's gate order (section 5: PIN, then
/// confirm, then run). Parks it in `live.pending` behind a modal when
/// a gate applies; otherwise forwards it to the worker. `pin_done`
/// is true when the caller just verified the PIN for this call.
fn route_call(
    live: &mut LiveState,
    screen: &paneld_ui::ui::Obj<'_>,
    call: actions::Call,
    pin_done: bool,
) {
    let Some(def) = live.actions.get(&call.name).cloned() else {
        // Unknown action: let the worker log it.
        let _ = live.action_tx.send(call);
        return;
    };
    let now = std::time::Instant::now();
    if def.pin && !pin_done && !live.pin_gate.is_unlocked(now) {
        if !live.pin_gate.configured() {
            eprintln!(
                "paneld: action `{}` needs a PIN but security.pin is not set; refused",
                call.name
            );
            show_notice(
                screen,
                "No PIN is configured (security.pin); action refused.",
                &live.event_tx,
            );
            live.pending = Some(Pending { call, stage: Stage::Notice });
            return;
        }
        if let Some(d) = live.pin_gate.locked_for(now) {
            show_notice(
                screen,
                &format!("Too many wrong PINs. Locked for {}s.", d.as_secs().max(1)),
                &live.event_tx,
            );
            live.pending = Some(Pending { call, stage: Stage::Notice });
            return;
        }
        show_pin_modal(screen, &call.name, None, &live.event_tx);
        live.pending = Some(Pending { call, stage: Stage::Pin });
        return;
    }
    if let Some(cp) = def.confirm {
        let mut ctx = build_ctx(
            &live.source_values,
            &live.theme,
            &live.secrets,
            &live.vars,
            Some(&live.alerts),
        );
        // The message can name the call's args (`Stop VM {{ vm }}?`),
        // bound the same way the worker binds them for `run`.
        for a in &call.args {
            ctx.insert(a.name.clone(), a.value.clone());
        }
        let message = paneld_ui::interpolate(&cp.raw, cp.span.file, cp.span.start, &ctx);
        show_confirm_modal(screen, &call.name, &message, &live.event_tx);
        live.pending = Some(Pending { call, stage: Stage::Confirm });
        return;
    }
    let mut call = call;
    call.cleared = true;
    let _ = live.action_tx.send(call);
}

/// PIN entry modal: masked textarea in a msgbox plus a numeric
/// keyboard. The keyboard's OK sends `PinEntered`; Cancel sends a
/// `ConfirmReply { accept: false }` which drops the parked call.
fn show_pin_modal(
    screen: &paneld_ui::ui::Obj<'_>,
    action_name: &str,
    error: Option<&str>,
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    paneld_ui::reset_action_bindings();
    screen.clear_children();
    let box_ = screen.new_msgbox();
    box_.msgbox_add_title(&format!("PIN for {action_name}"));
    if let Some(e) = error {
        box_.msgbox_add_text(e);
    }
    let ta = box_.msgbox_content().new_textarea(true, 12);
    ta.set_size(paneld_ui::ui::Size::Percent(100), paneld_ui::ui::Size::Content);
    box_.msgbox_add_confirm_button("Cancel", false, sink.clone());
    // The keyboard takes the bottom half of the screen; keep the box
    // (and its Cancel) in the top half.
    box_.align_top_mid(8);
    let kb = screen.new_number_keyboard();
    kb.attach_keyboard_to(&ta);
    kb.on_submit_pin(sink.clone());
    kb.on_cancel_reply(sink.clone());
}

/// A message with a single OK that dismisses it.
fn show_notice(
    screen: &paneld_ui::ui::Obj<'_>,
    message: &str,
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    paneld_ui::reset_action_bindings();
    screen.clear_children();
    let box_ = screen.new_msgbox();
    box_.msgbox_add_text(message);
    box_.msgbox_add_confirm_button("OK", false, sink.clone());
}

/// Pop an action-confirm modal on `screen`, replacing the page so
/// the modal is the only live binding target until the user replies.
fn show_confirm_modal(
    screen: &paneld_ui::ui::Obj<'_>,
    action_name: &str,
    message: &str,
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    paneld_ui::reset_action_bindings();
    screen.clear_children();
    let box_ = screen.new_msgbox();
    box_.msgbox_add_title(&format!("Run {action_name}?"));
    box_.msgbox_add_text(message);
    box_.msgbox_add_confirm_button("Cancel", false, sink.clone());
    box_.msgbox_add_confirm_button("OK", true, sink.clone());
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

fn run_shot(o: ShotOpts) -> ExitCode {
    let ui = match paneld_ui::Ui::init() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("paneld shot: {e}");
            return ExitCode::from(70);
        }
    };
    let mut mem = ui.open_memory_display(o.size.0, o.size.1);
    let screen = ui.screen();
    let mut live = match setup_live_state(&o.config, &screen) {
        Ok(l) => l,
        Err(code) => return code,
    };
    // A snapshot must never write the real alert state file.
    let scratch_state = std::env::temp_dir().join(format!("paneld-shot-{}.state", std::process::id()));
    live.state = state::Store::open(scratch_state.clone());

    // Run the UI for `ms`, in real time so action and source threads
    // get to finish what a tap started.
    let settle = |live: &mut LiveState, ms: u32| {
        for _ in 0..ms.div_ceil(20).max(1) {
            ui.advance(20);
            process(live, &screen);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };

    if let Some(page) = &o.page {
        let _ = live.event_tx.send(paneld_ui::UiEvent::Goto { page: page.clone() });
    }
    settle(&mut live, o.settle_ms);
    for &(x, y, press_ms) in &o.taps {
        mem.set_pointer(x, y, true);
        settle(&mut live, press_ms);
        mem.set_pointer(x, y, false);
        settle(&mut live, o.settle_ms);
    }

    let (w, h) = mem.size();
    let png = png::encode_rgb(w, h, &mem.rgb());
    live.sources_shutdown.stop();
    let _ = std::fs::remove_file(&scratch_state);
    match std::fs::write(&o.out, png) {
        Ok(()) => {
            println!("{}", o.out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("paneld shot: writing {}: {e}", o.out.display());
            ExitCode::from(1)
        }
    }
}

/// `paneld hash-pin`: read one line (the PIN) from stdin and print
/// the argon2id PHC string to store in the secret `security.pin`
/// names. Digits only, since the panel's PIN pad is numeric.
fn run_hash_pin() -> ExitCode {
    let mut line = String::new();
    if let Err(e) = std::io::stdin().read_line(&mut line) {
        eprintln!("paneld hash-pin: reading stdin: {e}");
        return ExitCode::from(1);
    }
    let pin = line.trim_end_matches(['\n', '\r']);
    if pin.is_empty() || !pin.bytes().all(|b| b.is_ascii_digit()) {
        eprintln!("paneld hash-pin: the PIN must be one or more digits");
        return ExitCode::from(2);
    }
    match pin::hash(pin) {
        Ok(h) => {
            println!("{h}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("paneld hash-pin: {e}");
            ExitCode::from(1)
        }
    }
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
    source_values: &std::sync::Arc<
        std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>,
    >,
    theme: &std::collections::BTreeMap<String, paneld_ui::Value>,
    secret_values: &std::collections::BTreeMap<String, paneld_ui::Value>,
    vars: &std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>>,
    alerts: Option<&alerts::Engine>,
) -> paneld_ui::Context {
    let mut ctx = paneld_ui::Context::new();
    let sources_snapshot = source_values.lock().unwrap().clone();
    ctx.insert("sources".into(), paneld_ui::Value::Map(sources_snapshot));
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
    fn config_dir_handles_bare_and_nested_paths() {
        use std::path::Path;
        assert_eq!(config_dir_of(Path::new("paneld.conf")), PathBuf::from("."));
        assert_eq!(config_dir_of(Path::new("/etc/paneld/paneld.conf")), PathBuf::from("/etc/paneld"));
        assert_eq!(config_dir_of(Path::new("conf/p.conf")), PathBuf::from("conf"));
        assert_eq!(config_dir_of(Path::new("/")), PathBuf::from("."));
    }

    #[test]
    fn shot_parses_its_options() {
        let Ok(Cmd::Shot(o)) = parse(&[
            "-c", "a.conf", "shot", "-o", "x.png", "--page", "p", "--size", "320x240",
            "--tap", "10,20", "--hold", "30, 40", "--settle", "50",
        ]) else {
            panic!("expected shot");
        };
        assert_eq!(o.config, PathBuf::from("a.conf"));
        assert_eq!(o.out, PathBuf::from("x.png"));
        assert_eq!(o.page.as_deref(), Some("p"));
        assert_eq!(o.size, (320, 240));
        assert_eq!(o.taps, vec![(10, 20, 100), (30, 40, 1000)]);
        assert_eq!(o.settle_ms, 50);
        assert!(parse(&["shot"]).unwrap_err().contains("-o"));
        assert!(parse(&["shot", "-o", "x.png", "--size", "0x10"]).is_err());
    }

    #[test]
    fn hash_pin_takes_no_positional_pin() {
        assert_eq!(parse(&["hash-pin"]), Ok(Cmd::HashPin));
        let err = parse(&["hash-pin", "1234"]).unwrap_err();
        assert!(err.contains("stdin"), "got: {err}");
    }

    #[test]
    fn config_without_value_is_error() {
        assert!(parse(&["-c"]).is_err());
        assert!(parse(&["check", "-c"]).is_err());
    }
}
