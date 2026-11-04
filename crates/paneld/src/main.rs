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
mod history;
mod pin;
mod png;
mod reminders;
mod remotes;
mod runner;
mod screen;
mod secrets;
mod sources;
mod state;
mod themes;
mod tz;
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

#[derive(Debug, PartialEq, Eq)]
enum ShotStep {
    /// Press at (x, y) for `press_ms`, release, then `--settle`.
    Touch { x: i32, y: i32, press_ms: u32 },
    /// Run the UI this long without touching it.
    Wait(u32),
}

/// `paneld shot`: render the config headless to a PNG.
#[derive(Debug, PartialEq, Eq)]
struct ShotOpts {
    config: PathBuf,
    out: PathBuf,
    page: Option<String>,
    size: (i32, i32),
    /// Touches and pauses, in order.
    taps: Vec<ShotStep>,
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
                o.taps.push(ShotStep::Touch {
                    x: x.trim().parse().map_err(|_| format!("bad `{a}` x"))?,
                    y: y.trim().parse().map_err(|_| format!("bad `{a}` y"))?,
                    press_ms: if a == "--hold" { 1000 } else { 100 },
                });
            }
            "--wait" => {
                let ms = val(&mut i, a)?.parse().map_err(|_| "bad `--wait` ms")?;
                o.taps.push(ShotStep::Wait(ms));
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
  paneld ctl [--json] COMMAND         Talk to a running daemon:
      ping | status | check | reload | exit
      eval '<cel>'                    Evaluate against the live values
      set vars.<name>=<value>         Set a var
      goto <page>                     Show a page (`back` = previous)
      alert fire <id> [severity=..] [title=..] [text=..] [link=..]
      alert resolve <id>
      alert ack <id> | alert snooze <id> [duration]
      reminder done <name>
  paneld hash-pin                     Read a PIN on stdin, print its
                                      argon2 hash for security.pin
  paneld shot -o FILE.png [--page P] [--size WxH] [--settle MS]
              [--tap X,Y | --hold X,Y | --wait MS]...
                                      Render the config headless to a PNG,
                                      after the given steps (runs sources
                                      and touched actions for real)

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
        Ok((_, mut doc, _)) => {
            // Same last-one-wins view of `display` options as the rest.
            paneld_config::apply_overrides(&mut doc);
            display::DisplayConfig::from_doc(&doc)
        }
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
    let mut live = match setup_live_state(&config, &screen, None, None) {
        Ok(l) => l,
        Err(_) => return ExitCode::from(1),
    };

    let mut pump = ui.pump();
    'main: loop {
        // Drain the ctl channel (and the config watcher).
        while let Ok(msg) = control_rx.try_recv() {
            match msg {
                ctl::ControlMsg::Reload => {
                    let _ = reload(&mut live, &config, &screen);
                }
                ctl::ControlMsg::Request(req) => {
                    if req.cmd == "exit" {
                        let _ = req.reply.send(Ok(paneld_ui::Value::String("exiting".into())));
                        break 'main;
                    }
                    let reply = handle_request(&req, &mut live, &config, &screen);
                    let _ = req.reply.send(reply);
                }
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
    let mut dirty = std::mem::take(&mut live.redraw);
    // Runs from before a reload: their completion, and their `then` /
    // `else`, which go through the new config's gates. (Their `set`
    // and `refresh` went to the old config's vars and sources.)
    let mut stale = std::mem::take(&mut live.stale_events);
    stale.retain(|rx| loop {
        match rx.try_recv() {
            Ok(paneld_ui::UiEvent::ActionDone { name, key, ok, result }) => {
                live.running.remove(&key);
                dirty |= action_done(live, screen, &name, ok, result);
            }
            Ok(paneld_ui::UiEvent::Chained { name, args }) => {
                live.deferred.push_back(actions::Call::new(name, args));
            }
            Ok(paneld_ui::UiEvent::Wake) => live.touched = true,
            Ok(_) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) => break true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break false,
        }
    });
    // Kept until the old worker has gone and its queue disconnects:
    // a run that outlasts several reloads must still report back, or
    // its action would count as running for good.
    live.stale_events = stale;
    while let Ok(ev) = live.event_rx.try_recv() {
        match ev {
            paneld_ui::UiEvent::Action { name, args } => {
                // A tap queued before a modal opened in this same drain
                // is stale once the modal is up: drop it.
                if live.pending.is_none() {
                    route_call(live, screen, actions::Call::new(name, args), false);
                }
            }
            paneld_ui::UiEvent::Chained { name, args } => {
                // A `then` / `else` chain is kept until any modal
                // closes, then gets its own gates.
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
            paneld_ui::UiEvent::AskReply { accept, value } => {
                // Only an ask dialog answers; anything else stays pending.
                let asking = matches!(live.pending, Some(Pending { stage: Stage::Ask(_), .. }));
                if let Some(Pending { mut call, stage: Stage::Ask(asked) }) = live.pending.take().filter(|_| asking) {
                    match value.filter(|_| accept) {
                        Some(v) => {
                            call.args.push(paneld_ui::ActionArg {
                                name: asked.param.name.clone(),
                                value: asked_value(&asked, v),
                            });
                            // Checks it, asks the next one or goes on to
                            // the PIN and confirm.
                            route_call(live, screen, call, false);
                        }
                        None if accept => {
                            // OK on an empty keypad: ask again.
                            show_ask_modal(screen, &call.name, &asked.param, &asked.options, &live.event_tx);
                            live.pending = Some(Pending { call, stage: Stage::Ask(asked) });
                        }
                        None => {}
                    }
                    dirty = live.pending.is_none();
                }
            }
            paneld_ui::UiEvent::ConfirmReply { accept } => {
                if let Some(Pending { mut call, stage }) = live.pending.take() {
                    if accept && matches!(stage, Stage::Confirm) {
                        call.cleared = true;
                        dispatch(live, call);
                    }
                }
                dirty = true;
            }
            paneld_ui::UiEvent::Goto { page } => {
                if go_to(live, &page) {
                    dirty = true;
                }
            }
            paneld_ui::UiEvent::Wake => live.touched = true,
            paneld_ui::UiEvent::ActionDone { name, key, ok, result } => {
                live.running.remove(&key);
                if action_done(live, screen, &name, ok, result) {
                    dirty = true;
                }
            }
            paneld_ui::UiEvent::InlineSet { assigns } => {
                let _set = actions::set_lock();
                let ctx = build_ctx(
                    &live.source_values,
                    &live.theme,
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
                let _set = actions::set_lock();
                let mut ctx = build_ctx(
                    &live.source_values,
                    &live.theme,
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
        let mut value = update.value;
        if update.status {
            // Only `ok` / `error` change: the rest of the last result
            // (`value`, `at`, `history`, `exit`, ...) stays, and no
            // event goes out.
            let mut values = live.source_values.lock().unwrap();
            if let (Some(paneld_ui::Value::Map(prev)), paneld_ui::Value::Map(m)) = (values.get(&update.name), &value) {
                let mut merged = prev.clone();
                for k in ["ok", "error"] {
                    merged.insert(k.to_string(), m.get(k).cloned().unwrap_or(paneld_ui::Value::Null));
                }
                value = paneld_ui::Value::Map(merged);
            }
            values.insert(update.name.clone(), value);
            dirty = true;
            continue;
        }
        let event_value = value.clone();
        live.histories.record(&update.name, &mut value, std::time::SystemTime::now());
        live.source_values
            .lock()
            .unwrap()
            .insert(update.name.clone(), value);
        let event_ctx = build_ctx(
            &live.source_values,
            &live.theme,
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
        &live.vars,
        None,
    );
    let now_instant = std::time::Instant::now();
    let now_wall = std::time::SystemTime::now();
    if live.alerts.tick(&alerts_ctx, now_instant, now_wall) {
        dirty = true;
    }
    if live.reminders.tick(&mut live.alerts, &alerts_ctx, now_wall) {
        dirty = true;
    }
    // Keep the state file (ADR 0014) in sync. `note` is cheap:
    // it only flags a pending write when the set actually
    // changed. `flush_if_due` writes at most once per 10s.
    if live.alerts.note_resolved(now_wall) {
        dirty = true;
    }
    live.state.note(live.alerts.active_alerts(), live.reminders.dates(), live.alerts.history());
    live.state.flush_if_due(now_instant);

    if live.toast_until.is_some_and(|t| std::time::Instant::now() >= t) {
        screen.top_layer().clear_children();
        live.toast_until = None;
    }

    if screen_step(live) {
        dirty = true;
    }
    // Asleep: no redraws (ADR 0001, idle means asleep); render once
    // on wake instead.
    if dirty && live.screen.asleep() {
        live.redraw = true;
        dirty = false;
    }

    if dirty && live.pending.is_none() {
        let ctx = build_ctx(
            &live.source_values,
            &live.theme,
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
    /// Past source values (schema 4, `history`).
    histories: history::Histories,
    /// Schema 10; due reminders are alerts in `alerts`.
    reminders: reminders::Book,
    state: state::Store,
    /// The state file `paneld shot` uses instead of `app:state`.
    state_override: Option<std::path::PathBuf>,
    sources_shutdown: sources::ShutdownHandle,
    /// Clone of the action table the worker holds. Lets the main
    /// loop look up `confirm` without crossing the channel.
    actions: std::collections::HashMap<String, actions::ActionDef>,
    /// Set while a PIN / confirm / notice modal owns the screen.
    /// Blocks re-renders of the page tree so the modal stays up.
    pending: Option<Pending>,
    /// `then` / `else` calls that arrived while a modal was up.
    deferred: std::collections::VecDeque<actions::Call>,
    /// Calls handed to the worker and not yet done, by
    /// [`actions::run_key`], with the action's name.
    running: std::collections::HashMap<String, String>,
    /// Event queues of configs replaced by a reload, kept until their
    /// worker has reported the runs it still had (see `reload`).
    stale_events: Vec<std::sync::mpsc::Receiver<paneld_ui::UiEvent>>,
    /// The page before the last navigation, for `goto = back`.
    previous_page: Option<String>,
    /// When the current toast (top layer) is removed.
    toast_until: Option<std::time::Instant>,
    /// Set by ctl requests that change what is shown; consumed by the
    /// next `process`.
    redraw: bool,
    /// Screen power and night hours (schema 12).
    screen: screen::Screen,
    /// The sleep cover was touched since the last step.
    touched: bool,
    pin_gate: pin::Gate,
}

/// An action parked behind a modal, and which modal it is.
struct Pending {
    call: actions::Call,
    stage: Stage,
}

enum Stage {
    /// A dialog asks for this param (schema 6 `ask`).
    Ask(Box<Asking>),
    Pin,
    Confirm,
    /// A message with only OK (lockout, PIN not configured). The
    /// parked call is dropped on dismiss.
    Notice,
}

/// The param a dialog asks for, with the options as the dialog shows
/// them: a dropdown answers with an index into these, whatever the
/// options are by the time OK is pressed.
struct Asking {
    param: actions::ParamDef,
    options: Vec<paneld_ui::Value>,
}

/// Load and check the config; the issues, with locations, if any.
fn load_checked(config: &std::path::Path) -> Result<paneld_config::parse::Document, Vec<ctl::CtlError>> {
    let (source, doc, mut diags) = match paneld_config::load(config) {
        Ok(parts) => parts,
        Err(e) => return Err(vec![ctl::CtlError::msg(e.to_string())]),
    };
    diags.extend(paneld_config::check(&doc, &source));
    if diags.is_empty() {
        return Ok(doc);
    }
    Err(diags
        .iter()
        .map(|d| {
            let loc = source.location(d.span);
            ctl::CtlError {
                message: d.message.clone(),
                file: Some(loc.path.display().to_string()),
                line: Some(loc.line),
                column: Some(loc.column),
            }
        })
        .collect())
}

fn issue_text(i: &ctl::CtlError) -> String {
    match (&i.file, i.line, i.column) {
        (Some(f), Some(l), Some(c)) => format!("{f}:{l}:{c}: {}", i.message),
        _ => format!("paneld: {}", i.message),
    }
}

/// `state_path`: where the alert state file lives; `None` = the
/// config's `app:state` (the daemon). `paneld shot` passes a scratch
/// path so it never opens, and so never touches, the real one.
fn setup_live_state(
    config: &std::path::Path,
    screen: &paneld_ui::ui::Obj<'_>,
    state_path: Option<&std::path::Path>,
    previous_sources: Option<&std::collections::BTreeMap<String, paneld_ui::Value>>,
) -> Result<LiveState, Vec<ctl::CtlError>> {
    // ADR 0013: commands run with cwd set to the config file's
    // directory by default, so `run = ./scripts/x.sh` works. A
    // per-source / per-action `cwd` overrides on top.
    let config_dir = config_dir_of(config);
    paneld_ui::set_asset_dir(&config_dir);
    let mut doc = match load_checked(config) {
        Ok(doc) => doc,
        Err(issues) => {
            for i in &issues {
                eprintln!("{}", issue_text(i));
            }
            eprintln!(
                "paneld: {} config issue{} found; refusing to apply",
                issues.len(),
                if issues.len() == 1 { "" } else { "s" }
            );
            return Err(issues);
        }
    };
    // `kind[name]:key = value` overrides land in their blocks so every
    // consumer below reads one form (and `action[x]:pin = true` is
    // honored).
    paneld_config::apply_overrides(&mut doc);
    // Remote instances become sources and actions named
    // `<instance>.<member>` (ADR 0018), after the overrides so
    // `remote[x]:host` counts.
    paneld_config::remote::lower(&mut doc);
    remotes::install(remotes::Remotes::from_doc(
        &doc,
        &state_path.map(std::path::Path::to_path_buf).unwrap_or_else(|| state::config_path(&doc)),
    ));

    // Pull the ADR 0013 defaults out once (10s timeout, 1 MiB
    // max_output unless `app:timeout` / `app:max_output` say
    // otherwise). Per-source and per-action overrides apply on top.
    let app_defaults = runner::AppDefaults::from_doc(&doc);
    runner::set_max_running(app_defaults.max_running);

    // Resolve secrets first so they're available for the first
    // source-run's interpolation. (Secrets never change at runtime
    // in v1; a reload re-reads them.)
    let secret_values = secrets::resolve(&doc);
    let pin_gate = pin::Gate::from_doc(&doc, &secret_values);
    let theme = themes::resolve(&doc);
    paneld_ui::set_styles(themes::styles(&doc, None));
    let vars_map = vars::initial_values(&doc);
    let var_types = vars::types(&doc);
    let vars_shared = std::sync::Arc::new(std::sync::Mutex::new(vars_map));
    let source_values = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::BTreeMap::<String, paneld_ui::Value>::new(),
    ));
    let base_ctx =
        build_ctx(&source_values, &theme, &vars_shared, None);
    // Sources interpolate only their `run` and `env`, where secrets
    // are allowed.
    let command_ctx = with_secrets(base_ctx.clone(), &secret_values);
    let (source_tx, source_rx) =
        std::sync::mpsc::channel::<sources::SourceUpdate>();
    let mut histories = history::Histories::from_doc(&doc);
    match previous_sources {
        // A reload: show the values we have and refresh them behind
        // the screen, so a slow source does not freeze the UI.
        Some(prev) => {
            let names = sources::collect_blocks(&doc);
            *source_values.lock().unwrap() =
                prev.iter().filter(|(k, _)| names.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
            sources::resolve_all_in_background(
                doc.clone(),
                command_ctx.clone(),
                config_dir.clone(),
                app_defaults,
                source_tx.clone(),
            );
        }
        None => {
            let mut values = sources::resolve_all(&doc, &command_ctx, &config_dir, app_defaults);
            let now = std::time::SystemTime::now();
            for (name, v) in values.iter_mut() {
                histories.record(name, v, now);
            }
            *source_values.lock().unwrap() = values;
        }
    }
    sources::fill_not_run(&doc, &mut source_values.lock().unwrap());
    let sources_shutdown = sources::spawn_pollers(
        &doc,
        &command_ctx,
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
            secrets: secret_values.clone(),
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

    let current_page = find_start_page(&doc);
    let mut alerts_engine = alerts::Engine::new(&doc);
    let state_store = state::Store::open(match state_path {
        Some(p) => p.to_path_buf(),
        None => state::config_path(&doc),
    });
    alerts_engine.restore(
        state_store
            .snapshot()
            .alerts
            .iter()
            .map(|(k, v)| {
                let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
                let carry = alerts::Carry {
                    since: at(v.since),
                    acked: v.acked,
                    snoozed_until: v.snoozed_until.map(at),
                    rank: alerts::severity_rank(&v.severity),
                };
                (k.clone(), carry)
            })
            .collect(),
    );
    // Alerts raised over ctl have no rule to raise them again: bring
    // them back from the state file as they were.
    for saved in state_store.snapshot().alerts.values().filter(|a| a.rule == alerts::CTL_RULE) {
        alerts_engine.fire_external(
            &saved.instance,
            &saved.severity,
            &saved.title,
            &saved.text,
            &saved.link,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(saved.since),
        );
    }
    let ctx_no_alerts =
        build_ctx(&source_values, &theme, &vars_shared, None);
    alerts_engine.tick(
        &ctx_no_alerts,
        std::time::Instant::now(),
        std::time::SystemTime::now(),
    );
    let reminder_book = reminders::Book::new(
        &doc,
        tz::Zone::local(),
        &state_store.snapshot().reminders,
        std::time::SystemTime::now(),
    );
    reminder_book.tick(&mut alerts_engine, &ctx_no_alerts, std::time::SystemTime::now());
    alerts_engine.forget_unfired();
    alerts_engine.restore_history(state_store.snapshot().history.clone());
    alerts_engine.note_resolved_while_down(&state_store.snapshot().alerts, std::time::SystemTime::now());
    let ctx = build_ctx(
        &source_values,
        &theme,
        &vars_shared,
        Some(&alerts_engine),
    );
    screen.clear_children();
    // A toast from before a reload belongs to the old state.
    screen.top_layer().clear_children();
    paneld_ui::reset_action_bindings();
    paneld_ui::render_page(&doc, &current_page, &ctx, &event_tx, screen);

    let screen_state =
        screen::Screen::new(screen::Config::from_doc(&doc), tz::Zone::local(), config_dir.clone());
    Ok(LiveState {
        doc,
        source_values,
        source_rx,
        theme,
        vars: vars_shared,
        var_types,
        event_tx,
        event_rx,
        action_tx,
        rendered_page: current_page.clone(),
        current_page,
        alerts: alerts_engine,
        histories,
        reminders: reminder_book,
        state: state_store,
        state_override: state_path.map(std::path::Path::to_path_buf),
        sources_shutdown,
        actions: action_table_for_main,
        pending: None,
        deferred: std::collections::VecDeque::new(),
        running: std::collections::HashMap::new(),
        stale_events: Vec::new(),
        previous_page: None,
        toast_until: None,
        redraw: false,
        screen: screen_state,
        touched: false,
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
    mut call: actions::Call,
    pin_done: bool,
) {
    let Some(def) = live.actions.get(&call.name).cloned() else {
        // Unknown action: let the worker log it.
        let _ = live.action_tx.send(call);
        return;
    };
    // Schema 5: while an action runs, further calls of it are ignored
    // (checked first, so nobody fills in a dialog for nothing).
    if live.running.contains_key(&actions::run_key(&call.name, &call.args)) {
        eprintln!("paneld: action `{}` is still running; call ignored", call.name);
        return;
    }
    // Schema 5 order: params (typed, checked, asked), then PIN, then
    // confirm. Built-in actions choose their own defaults.
    let builtin = paneld_config::library::BUILTIN_ACTIONS.contains(&call.name.as_str());
    let mut ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
    def.bind(&mut ctx);
    match actions::prepare_args(&def, std::mem::take(&mut call.args), &ctx, !builtin) {
        actions::Prepared::Ready(args) => call.args = args,
        actions::Prepared::Ask(param) => {
            let options = param.option_values(&ctx);
            show_ask_modal(screen, &call.name, &param, &options, &live.event_tx);
            live.pending = Some(Pending { call, stage: Stage::Ask(Box::new(Asking { param, options })) });
            return;
        }
        actions::Prepared::Refused(why) => {
            eprintln!("paneld: action `{}` refused: {why}", call.name);
            show_notice(screen, &format!("Not run: {}.", why.replace('`', "")), &live.event_tx);
            live.pending = Some(Pending { call, stage: Stage::Notice });
            return;
        }
    }
    // Schema 5: while an action runs, further calls of it are ignored.
    if live.running.contains_key(&actions::run_key(&call.name, &call.args)) {
        eprintln!("paneld: action `{}` is still running; call ignored", call.name);
        return;
    }
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
    if let Some(cp) = &def.confirm {
        let mut ctx = build_ctx(
            &live.source_values,
            &live.theme,
            &live.vars,
            Some(&live.alerts),
        );
        // The message can name the call's args (`Stop VM {{ vm }}?`)
        // and a remote's params (`Reboot {{ host }}?`), bound the same
        // way the worker binds them for `run`.
        for a in &call.args {
            ctx.insert(a.name.clone(), a.value.clone());
        }
        def.bind(&mut ctx);
        let message = paneld_ui::interpolate(&cp.raw, cp.span.file, cp.span.start, &ctx);
        show_confirm_modal(screen, &call.name, &message, &live.event_tx);
        live.pending = Some(Pending { call, stage: Stage::Confirm });
        return;
    }
    let mut call = call;
    call.cleared = true;
    dispatch(live, call);
}

/// `alert_ack { id }` / `alert_snooze { id; for }` (ADR 0016).
fn run_builtin(live: &mut LiveState, call: &actions::Call) -> Result<(), ctl::CtlError> {
    let arg = |name: &str| {
        call.args
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.value.to_display_string())
    };
    let id = arg("id").ok_or_else(|| ctl::CtlError::msg("needs `id`"))?;
    let found = match call.name.as_str() {
        "reminder_done" => {
            let now = std::time::SystemTime::now();
            if !live.reminders.done(&id, now) {
                return Err(ctl::CtlError::msg(format!("no due reminder `{id}`")));
            }
            let ctx = build_ctx(&live.source_values, &live.theme, &live.vars, None);
            live.reminders.tick(&mut live.alerts, &ctx, now);
            true
        }
        "alert_ack" => live.alerts.ack(&id),
        "alert_snooze" => {
            let d = match arg("for") {
                Some(t) => paneld_config::value::parse_duration(&t)
                    .ok_or_else(|| ctl::CtlError::msg(format!("`for = {t}` is not a duration")))?,
                None => live.alerts.snooze_choices()[0].1,
            };
            let until = std::time::SystemTime::now()
                .checked_add(d)
                .ok_or_else(|| ctl::CtlError::msg("snooze is too long"))?;
            live.alerts.snooze(&id, until)
        }
        other => return Err(ctl::CtlError::msg(format!("no built-in `{other}`"))),
    };
    if !found {
        return Err(ctl::CtlError::msg(format!("no active alert `{id}`")));
    }
    live.redraw = true;
    Ok(())
}

/// Run the screen rules (schema 12) for this pass and apply what they
/// ask for. Returns true if the page must be redrawn.
fn screen_step(live: &mut LiveState) -> bool {
    let now = std::time::SystemTime::now();
    let shown = live.alerts.shown(now);
    let inputs = screen::Inputs {
        now,
        inactive: std::time::Duration::from_millis(paneld_ui::inactive_ms().into()),
        touched: std::mem::take(&mut live.touched),
        shown: &shown,
    };
    let mut redraw = false;
    for effect in live.screen.step(&inputs) {
        match effect {
            screen::Effect::Sleep { absorb } => {
                screen_command(&live.screen, live.screen.cfg.off.clone(), None);
                paneld_ui::show_sleep_cover(absorb, live.event_tx.clone());
            }
            screen::Effect::Wake => {
                paneld_ui::hide_sleep_cover();
                // An alert wake has no touch: restart the idle clock.
                paneld_ui::trigger_activity();
                screen_command(&live.screen, live.screen.cfg.on.clone(), None);
                redraw = true;
            }
            screen::Effect::Dim(level) => {
                screen_command(&live.screen, live.screen.cfg.dim.clone(), Some(level));
            }
            screen::Effect::Theme(name) => {
                live.theme = themes::resolve_with(&live.doc, name.as_deref());
                paneld_ui::set_styles(themes::styles(&live.doc, name.as_deref()));
                redraw = true;
            }
        }
    }
    redraw
}

/// Run a screen command (`on`, `off`, `dim` with `level`) in the
/// background: a slow backlight must not stall the UI.
fn screen_command(s: &screen::Screen, template: Option<String>, level: Option<u8>) {
    let Some(template) = template else { return };
    // One thread runs them all, in order: an `on` must not overtake
    // the `off` before it.
    static QUEUE: std::sync::OnceLock<std::sync::mpsc::Sender<ScreenJob>> = std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<ScreenJob>();
        std::thread::spawn(move || {
            for job in rx {
                run_screen_job(job);
            }
        });
        tx
    });
    let _ = queue.send(ScreenJob { template, level, cwd: s.cwd.clone() });
}

struct ScreenJob {
    template: String,
    level: Option<u8>,
    cwd: std::path::PathBuf,
}

fn run_screen_job(job: ScreenJob) {
    let mut ctx = paneld_ui::Context::new();
    if let Some(l) = job.level {
        ctx.insert("level".into(), paneld_ui::Value::Int(l.into()));
    }
    let file = paneld_config::source::FileId::default();
    let result = runner::command_argv(&job.template, |t| paneld_ui::interpolate(t, file, 0, &ctx)).and_then(|argv| {
        runner::exec(
            &argv,
            runner::Opts {
                timeout: std::time::Duration::from_secs(10),
                max_bytes: 64 * 1024,
                kind: runner::Kind::Other,
                name: "screen",
                cwd: Some(&job.cwd),
                env: &[],
                stdin: None,
            },
        )
    });
    match result {
        Ok(r) if r.succeeded() => {}
        Ok(r) => eprintln!("paneld: screen command failed: exit {:?} {}", r.exit_code, r.stderr.trim()),
        Err(e) => eprintln!("paneld: screen command: {e}"),
    }
}

/// Hand a call that passed its gates to the worker and mark the
/// action running until its `ActionDone` arrives.
fn dispatch(live: &mut LiveState, call: actions::Call) {
    // ADR 0016: built-in actions act on the alert engine here, after
    // the same gates as any action (`action[alert_ack]:pin = true`).
    if paneld_config::library::BUILTIN_ACTIONS.contains(&call.name.as_str()) {
        if let Err(e) = run_builtin(live, &call) {
            eprintln!("paneld: action `{}`: {}", call.name, e.message);
        }
        return;
    }
    let key = actions::run_key(&call.name, &call.args);
    if live.running.contains_key(&key) {
        eprintln!("paneld: action `{}` is still running; call ignored", call.name);
        return;
    }
    live.running.insert(key, call.name.clone());
    let _ = live.action_tx.send(call);
}

/// Switch to `page` (`back` = the previous one). Returns true when the
/// page changed.
fn go_to(live: &mut LiveState, page: &str) -> bool {
    let target = if page == "back" {
        match live.previous_page.clone() {
            Some(p) => p,
            None => return false,
        }
    } else {
        page.to_string()
    };
    if target == live.current_page {
        return false;
    }
    live.previous_page = Some(std::mem::replace(&mut live.current_page, target));
    true
}

/// Finish an action call (schema 5): `goto` on success, and the
/// `done` / `failed` message as a toast. Returns true when the page
/// changed.
fn action_done(
    live: &mut LiveState,
    screen: &paneld_ui::ui::Obj<'_>,
    name: &str,
    ok: bool,
    result: paneld_ui::Value,
) -> bool {
    let Some(def) = live.actions.get(name).cloned() else {
        return false;
    };
    let field = |k: &str| match &result {
        paneld_ui::Value::Map(m) => m.get(k).map(|v| v.to_display_string()).unwrap_or_default(),
        _ => String::new(),
    };
    let message = if ok {
        def.done.as_ref().map(|d| {
            let mut ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
            def.bind(&mut ctx);
            ctx.insert("output".into(), paneld_ui::Value::String(field("stdout").trim().to_string()));
            ctx.insert("result".into(), result.clone());
            paneld_ui::interpolate(&d.raw, d.span.file, d.span.start, &ctx)
        })
    } else {
        Some(match &def.failed {
            Some(f) => {
                let mut ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
                def.bind(&mut ctx);
                ctx.insert("result".into(), result.clone());
                paneld_ui::interpolate(&f.raw, f.span.file, f.span.start, &ctx)
            }
            None => {
                let err = field("stderr");
                let err = err.trim();
                if err.is_empty() {
                    match shown_name(name) {
                        Some(n) => format!("{n} failed"),
                        None => "Failed".to_string(),
                    }
                } else {
                    // One line is enough for a toast.
                    err.lines().next().unwrap_or(err).to_string()
                }
            }
        })
    };
    if let Some(text) = message.filter(|t| !t.trim().is_empty()) {
        let token = if ok { "ok" } else { "critical" };
        let bg = match live.theme.get(token) {
            Some(paneld_ui::Value::String(c)) => c.clone(),
            _ => String::new(),
        };
        paneld_ui::render_toast(&screen.top_layer(), &text, &bg);
        live.toast_until = Some(std::time::Instant::now() + TOAST_FOR);
    }
    match (ok, def.goto.as_deref()) {
        (true, Some(page)) => go_to(live, page),
        _ => false,
    }
}

/// How long a `done` / `failed` toast stays up.
const TOAST_FOR: std::time::Duration = std::time::Duration::from_secs(3);

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
    box_.msgbox_add_title(&match shown_name(action_name) {
        Some(n) => format!("PIN for {n}"),
        None => "Enter PIN".to_string(),
    });
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
}

/// A dialog asking for `param` of `action_name` (schema 6 `ask`):
/// a dropdown of its `options`, a slider over `min`..`max`, a toggle,
/// or a digit keypad, with Cancel and OK.
fn show_ask_modal(
    screen: &paneld_ui::ui::Obj<'_>,
    action_name: &str,
    param: &actions::ParamDef,
    options: &[paneld_ui::Value],
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    use paneld_ui::ui::{Size, ValueWidget};
    use paneld_ui::EventValue;
    paneld_ui::reset_action_bindings();
    screen.clear_children();
    let box_ = screen.new_msgbox();
    let label = param.label.clone().unwrap_or_else(|| param.name.clone());
    box_.msgbox_add_title(&match shown_name(action_name) {
        Some(n) => format!("{n}: {label}"),
        None => label,
    });
    let content = box_.msgbox_content();
    match param.ask.as_deref() {
        Some("slider") => {
            let (min, max) = (param.min.unwrap_or(0.0) as i32, param.max.unwrap_or(100.0) as i32);
            let s = content.new_slider();
            s.set_slider_range(min, max);
            s.set_slider_value(min);
            s.set_size(Size::Percent(100), Size::Content);
            s.on_change_ask(ValueWidget::Slider, sink.clone());
            paneld_ui::set_ask_value(Some(EventValue::Int(min.into())));
        }
        Some("toggle") => {
            let t = content.new_toggle();
            t.on_change_ask(ValueWidget::Toggle, sink.clone());
            paneld_ui::set_ask_value(Some(EventValue::Bool(false)));
        }
        Some("keypad") => {
            let ta = content.new_textarea(false, 12);
            ta.set_size(Size::Percent(100), Size::Content);
            box_.msgbox_add_ask_button("Cancel", false, sink.clone());
            box_.align_top_mid(8);
            let kb = screen.new_number_keyboard();
            kb.attach_keyboard_to(&ta);
            kb.on_change_ask(ValueWidget::Keypad, sink.clone());
            paneld_ui::set_ask_value(None);
            return;
        }
        // `select`, and anything else: pick from the options.
        _ => {
            // One row per option, even one whose text has a newline.
            let options: Vec<String> = options.iter().map(|v| v.to_display_string().replace('\n', " ")).collect();
            let d = content.new_dropdown();
            d.set_dropdown_options(&options.join("\n"));
            d.set_size(Size::Percent(100), Size::Content);
            d.on_change_ask(ValueWidget::Dropdown, sink.clone());
            paneld_ui::set_ask_value(Some(EventValue::Int(0)));
        }
    }
    box_.msgbox_add_ask_button("Cancel", false, sink.clone());
    box_.msgbox_add_ask_button("OK", true, sink.clone());
}

/// The arg an ask dialog's widget value stands for: a dropdown gives
/// the index of the chosen option.
fn asked_value(asked: &Asking, v: paneld_ui::EventValue) -> paneld_ui::Value {
    use paneld_ui::{EventValue, Value};
    match (asked.param.ask.as_deref(), v) {
        (Some("slider" | "toggle" | "keypad"), v) | (_, v @ (EventValue::Bool(_) | EventValue::String(_) | EventValue::Double(_))) => match v {
            EventValue::Bool(b) => Value::Bool(b),
            EventValue::Int(n) => Value::Int(n),
            EventValue::Double(d) => Value::Double(d),
            EventValue::String(s) => Value::String(s),
        },
        (_, EventValue::Int(i)) => asked.options.get(i.max(0) as usize).cloned().unwrap_or(Value::Null),
    }
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

/// An action's name as shown on screen; `None` for an inline
/// handler's internal `@inline:...` name.
fn shown_name(name: &str) -> Option<&str> {
    (!name.starts_with('@')).then_some(name)
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
    box_.msgbox_add_title(&match shown_name(action_name) {
        Some(n) => format!("Run {n}?"),
        None => "Are you sure?".to_string(),
    });
    box_.msgbox_add_text(message);
    box_.msgbox_add_confirm_button("Cancel", false, sink.clone());
    box_.msgbox_add_confirm_button("OK", true, sink.clone());
}

/// Schema 15: `app:start_page`, else the first page in `nav` order,
/// else the first page defined (else `home`, which then shows "not
/// found").
fn find_start_page(doc: &paneld_config::parse::Document) -> String {
    use paneld_config::parse::{AssignTarget, Item};
    // After `apply_overrides` all `app` options sit in one `app` block.
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "app" || b.name.is_some() {
            continue;
        }
        for inner in &b.items {
            if let Item::Assign(a) = inner {
                if matches!(&a.target, AssignTarget::Simple { key } if key.text == "start_page") {
                    return a.value.text.trim().to_string();
                }
            }
        }
    }
    let pages = doc.items.iter().filter_map(|item| match item {
        Item::Block(b) if b.kind.text == "page" => b.name.as_ref().map(|n| (b, n.text.clone())),
        _ => None,
    });
    let nav_pos = |b: &paneld_config::parse::Block| {
        b.items.iter().find_map(|it| match it {
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "nav") => {
                a.value.text.trim().parse::<i64>().ok()
            }
            _ => None,
        })
    };
    let mut first_defined = None;
    let mut first_nav: Option<(i64, String)> = None;
    for (b, name) in pages {
        first_defined.get_or_insert_with(|| name.clone());
        if let Some(pos) = nav_pos(b) {
            let earlier = match &first_nav {
                Some((p, _)) => pos < *p,
                None => true,
            };
            if earlier {
                first_nav = Some((pos, name));
            }
        }
    }
    first_nav
        .map(|(_, n)| n)
        .or(first_defined)
        .unwrap_or_else(|| "home".to_string())
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
    // A snapshot must never touch the real alert state file.
    let scratch_state = std::env::temp_dir().join(format!("paneld-shot-{}.state", std::process::id()));
    let mut live = match setup_live_state(&o.config, &screen, Some(&scratch_state), None) {
        Ok(l) => l,
        Err(_) => return ExitCode::from(1),
    };

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
    for step in &o.taps {
        match *step {
            ShotStep::Touch { x, y, press_ms } => {
                mem.set_pointer(x, y, true);
                settle(&mut live, press_ms);
                mem.set_pointer(x, y, false);
                settle(&mut live, o.settle_ms);
            }
            ShotStep::Wait(ms) => settle(&mut live, ms),
        }
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
    let (json, args) = match args.first().map(String::as_str) {
        Some("--json") => (true, &args[1..]),
        _ => (false, args),
    };
    let path = ctl::default_socket_path();
    ExitCode::from(ctl::call(&path, args, json) as u8)
}

/// Reload the config in place. On failure the running config stays
/// (config errors never blank the screen).
fn reload(
    live: &mut LiveState,
    config: &std::path::Path,
    screen: &paneld_ui::ui::Obj<'_>,
) -> Result<(), Vec<ctl::CtlError>> {
    // Write pending alert and reminder state first: the new state
    // reads the file, and an ack from a moment ago must be in it.
    // A ctl request in this same pass (`reminder done`, `alert ack`)
    // is not noted yet: note it now.
    live.alerts.note_resolved(std::time::SystemTime::now());
    live.state.note(live.alerts.active_alerts(), live.reminders.dates(), live.alerts.history());
    live.state.force_flush();
    let state_override = live.state_override.clone();
    let previous_sources = live.source_values.lock().unwrap().clone();
    match setup_live_state(config, screen, state_override.as_deref(), Some(&previous_sources)) {
        Ok(mut fresh) => {
            fresh.pin_gate.carry_from(&live.pin_gate);
            fresh.screen.carry_from(&live.screen);
            // Runs started under the old config finish on its worker,
            // which reports to the old event queue: keep reading that
            // queue for them, so a run is not started twice meanwhile
            // and its toast still shows.
            fresh.histories.carry_from(&mut live.histories);
            // A sleeping screen's cover must wake through the new queue.
            paneld_ui::set_wake_sink(fresh.event_tx.clone());
            fresh.running = std::mem::take(&mut live.running);
            fresh.stale_events = std::mem::take(&mut live.stale_events);
            let (tx, rx) = std::sync::mpsc::channel();
            drop(tx);
            fresh.stale_events.push(std::mem::replace(&mut live.event_rx, rx));
            // Stop the old poll / stream workers; the new state runs
            // its own.
            live.sources_shutdown.stop();
            *live = fresh;
            eprintln!("paneld: reloaded config");
            Ok(())
        }
        Err(issues) => {
            eprintln!("paneld: reload failed; keeping previous config");
            Err(issues)
        }
    }
}

/// Answer one ctl request (ADR 0007's command set) from the main loop.
fn handle_request(
    req: &ctl::Request,
    live: &mut LiveState,
    config: &std::path::Path,
    screen: &paneld_ui::ui::Obj<'_>,
) -> ctl::Reply {
    use paneld_ui::Value;
    use std::collections::BTreeMap;
    let issue_list = |issues: &[ctl::CtlError]| {
        Value::List(
            issues
                .iter()
                .map(|i| {
                    let mut m = BTreeMap::new();
                    m.insert("message".to_string(), Value::String(i.message.clone()));
                    if let (Some(f), Some(l), Some(c)) = (&i.file, i.line, i.column) {
                        m.insert("file".to_string(), Value::String(f.clone()));
                        m.insert("line".to_string(), Value::Int(l.into()));
                        m.insert("column".to_string(), Value::Int(c.into()));
                    }
                    Value::Map(m)
                })
                .collect(),
        )
    };
    match req.cmd.as_str() {
        "reload" => match reload(live, config, screen) {
            Ok(()) => Ok(Value::String("reloaded".into())),
            Err(issues) => {
                let mut first = issues.first().cloned().unwrap_or_default();
                if issues.len() > 1 {
                    first.message = format!("{} ({} more issue(s); see `paneld ctl check`)", first.message, issues.len() - 1);
                }
                Err(first)
            }
        },
        "check" => Ok(match load_checked(config) {
            Ok(_) => Value::List(Vec::new()),
            Err(issues) => issue_list(&issues),
        }),
        "eval" => {
            let expr = req.arg("expr")?;
            let file = paneld_config::source::FileId::default();
            let (parsed, diags) = paneld_config::cel::parse_fragment(&expr, file, 0);
            let parsed = match (parsed, diags.first()) {
                (Some(e), None) => e,
                (_, Some(d)) => return Err(ctl::CtlError::msg(d.message.clone())),
                (None, None) => return Err(ctl::CtlError::msg(format!("cannot parse `{expr}`"))),
            };
            let ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
            paneld_ui::eval(&parsed, &ctx).map_err(|e| ctl::CtlError::msg(e.message))
        }
        "set" => {
            let name = req.arg("var")?;
            let raw = req.arg("value")?;
            let Some(ty) = live.var_types.get(&name).cloned() else {
                return Err(ctl::CtlError::msg(format!("no var `{name}`")));
            };
            let span = paneld_config::source::Span {
                file: paneld_config::source::FileId::default(),
                start: 0,
                end: 0,
            };
            // Same lock as every other `set`: read, evaluate, write.
            let _set = actions::set_lock();
            let ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
            let value = paneld_ui::resolve_value(&raw, span, &ctx)
                .and_then(|v| vars::coerce(&ty, v))
                .ok_or_else(|| ctl::CtlError::msg(format!("`{raw}` does not fit var `{name}` ({ty})")))?;
            live.vars.lock().unwrap().insert(name, value.clone());
            live.redraw = true;
            Ok(value)
        }
        "goto" => {
            let page = req.arg("page")?;
            let exists = page == "back"
                || live.doc.items.iter().any(|it| matches!(it,
                    paneld_config::parse::Item::Block(b)
                        if b.kind.text == "page" && b.name.as_ref().is_some_and(|n| n.text == page)));
            if !exists {
                return Err(ctl::CtlError::msg(format!("no page `{page}`")));
            }
            if go_to(live, &page) {
                live.redraw = true;
            }
            Ok(Value::String(live.current_page.clone()))
        }
        "alert_fire" => {
            let id = req.arg("id")?;
            let severity = req.opt_arg("severity").unwrap_or_else(|| "warning".into());
            if !matches!(severity.as_str(), "critical" | "warning" | "info") {
                return Err(ctl::CtlError::msg(format!(
                    "severity `{severity}`: use critical, warning or info"
                )));
            }
            let title = req.opt_arg("title").unwrap_or_else(|| id.clone());
            let text = req.opt_arg("text").unwrap_or_default();
            let link = req.opt_arg("link").unwrap_or_default();
            if live.alerts.fire_external(&id, &severity, &title, &text, &link, std::time::SystemTime::now()) {
                live.redraw = true;
            }
            Ok(Value::String(format!("{}:{id}", alerts::CTL_RULE)))
        }
        "alert_ack" | "alert_snooze" => {
            // Accept the full `<rule>:<instance>` id, or a bare ctl id.
            let mut args = req.args.clone();
            if let Some(Value::String(id)) = args.get("id") {
                if !id.contains(':') {
                    args.insert("id".into(), Value::String(format!("{}:{id}", alerts::CTL_RULE)));
                }
            }
            let call = actions::Call::new(
                req.cmd.clone(),
                args.into_iter().map(|(name, value)| paneld_ui::ActionArg { name, value }).collect(),
            );
            run_builtin(live, &call).map(|()| Value::String("ok".into()))
        }
        "reminder_done" => {
            let call = actions::Call::new(
                req.cmd.clone(),
                vec![paneld_ui::ActionArg { name: "id".into(), value: Value::String(req.arg("id")?) }],
            );
            run_builtin(live, &call).map(|()| Value::String("ok".into()))
        }
        "alert_resolve" => {
            let id = req.arg("id")?;
            if live.alerts.resolve_external(&id) {
                live.redraw = true;
                Ok(Value::String("resolved".into()))
            } else {
                Err(ctl::CtlError::msg(format!("no active ctl alert `{id}`")))
            }
        }
        "status" => {
            let mut m = BTreeMap::new();
            m.insert("config".to_string(), Value::String(config.display().to_string()));
            m.insert("page".to_string(), Value::String(live.current_page.clone()));
            m.insert("sources".to_string(), Value::Map(live.source_values.lock().unwrap().clone()));
            m.insert("vars".to_string(), Value::Map(live.vars.lock().unwrap().clone()));
            m.insert("alerts".to_string(), live.alerts.context_value());
            let names: std::collections::BTreeSet<&String> = live.running.values().collect();
            let running: Vec<Value> = names.into_iter().map(|n| Value::String(n.clone())).collect();
            m.insert("running".to_string(), Value::List(running));
            // Event queues of replaced configs still waiting for their
            // worker to finish.
            m.insert("old_queues".to_string(), Value::Int(live.stale_events.len() as i64));
            Ok(Value::Map(m))
        }
        other => Err(ctl::CtlError::msg(format!("unknown command `{other}`"))),
    }
}

/// `ctx` plus `secrets`, for interpolating commands (`run`, `env`).
fn with_secrets(
    mut ctx: paneld_ui::Context,
    secrets: &std::collections::BTreeMap<String, paneld_ui::Value>,
) -> paneld_ui::Context {
    ctx.insert("secrets".into(), paneld_ui::Value::Map(secrets.clone()));
    ctx
}

fn build_ctx(
    source_values: &std::sync::Arc<
        std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>,
    >,
    theme: &std::collections::BTreeMap<String, paneld_ui::Value>,
    vars: &std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, paneld_ui::Value>>>,
    alerts: Option<&alerts::Engine>,
) -> paneld_ui::Context {
    // No `secrets` here: schema 16 allows them only in `run` and `env`,
    // and this context feeds pages, messages and alert texts (which
    // reach the screen and the state file). Command interpolation adds
    // them, see `with_secrets`.
    let mut ctx = paneld_ui::Context::new();
    let sources_snapshot = source_values.lock().unwrap().clone();
    remotes::expose(&mut ctx, sources_snapshot);
    ctx.insert("theme".into(), paneld_ui::Value::Map(theme.clone()));
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

    fn doc_of(src: &str) -> paneld_config::parse::Document {
        use paneld_config::lex::tokenize;
        use paneld_config::source::SourceMap;
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        let (mut doc, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        paneld_config::apply_overrides(&mut doc);
        doc
    }

    #[test]
    fn start_page_follows_schema_defaults() {
        assert_eq!(find_start_page(&doc_of("page a { }\npage b { }\n")), "a");
        assert_eq!(
            find_start_page(&doc_of("page a { }\npage b { nav = 2 }\npage c { nav = 1 }\n")),
            "c"
        );
        assert_eq!(
            find_start_page(&doc_of("page a { nav = 1 }\npage b { }\napp:start_page = b\n")),
            "b"
        );
        assert_eq!(find_start_page(&doc_of("source s { run = x }\n")), "home");
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
        assert_eq!(
            o.taps,
            vec![
                ShotStep::Touch { x: 10, y: 20, press_ms: 100 },
                ShotStep::Touch { x: 30, y: 40, press_ms: 1000 },
            ]
        );
        assert_eq!(o.settle_ms, 50);
        let Ok(Cmd::Shot(o)) = parse(&["shot", "-o", "x.png", "--wait", "500"]) else { panic!() };
        assert_eq!(o.taps, vec![ShotStep::Wait(500)]);
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
