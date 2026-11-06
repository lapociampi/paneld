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
mod clock;
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
mod visible;

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
    /// `display:rotation` for the shot: `size`, the PNG and the tap
    /// points are then the panel's (unrotated), as on the device.
    rotation: i32,
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
        rotation: 0,
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
            "--rotation" => {
                o.rotation = match val(&mut i, a)?.as_str() {
                    "0" => 0,
                    "90" => 90,
                    "180" => 180,
                    "270" => 270,
                    _ => return Err("`--rotation` is 0, 90, 180 or 270".into()),
                };
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
      theme <name> | theme --reset    Switch the theme until restart
      alert fire <id> [severity=..] [title=..] [text=..] [link=..]
      alert resolve <id>
      alert ack <id> | alert snooze <id> [duration]
      reminder done <name>
  paneld hash-pin                     Read a PIN on stdin, print its
                                      argon2 hash for security.pin
  paneld shot -o FILE.png [--page P] [--size WxH] [--settle MS]
              [--rotation DEG] [--tap X,Y | --hold X,Y | --wait MS]...
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

    // Also watch the config file, its includes and the theme folders
    // (inotify): a save fires Reload the same way `paneld ctl reload`
    // does.
    ctl::watch_config(
        &config,
        std::time::Duration::from_millis(200),
        control_tx,
    );

    // Peek at the config just to pick the display backend. The
    // live state will reload the full config a moment later.
    let display_cfg = match paneld_config::load(&config) {
        Ok((source, mut doc, _)) => {
            // Same last-one-wins view of `display` options as the rest.
            paneld_config::apply_overrides(&mut doc);
            display::DisplayConfig::from_doc_at(&doc, Some(&source))
        }
        Err(_) => display::DisplayConfig::default(),
    };
    // Read once: a reload only says that these changed.
    display::set_started(display_cfg.clone());
    // The config's display, else the next one that opens (schema 13);
    // a config display that did not open is shown on the screen.
    let display::Opened { window: _window, problem: display_problem } = match display_cfg.open(&ui) {
        Some(o) => o,
        None => {
            eprintln!("paneld: no display could be opened; check `display:backend` and `display:device` (schema 13)");
            return ExitCode::from(70);
        }
    };
    let _ = DISPLAY_PROBLEM.set(display_problem);
    let screen = ui.screen();

    // Initial live state. `reload` wipes and rebuilds this.
    let mut live = match setup_live_state(&config, &screen, None, None) {
        Ok(l) => l,
        Err(_) => return ExitCode::from(1),
    };

    raise_display_problem(&mut live);

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

/// `PANELD_PERF` set at start: time page rebuilds (docs/deploy.md,
/// Diagnostics). Read once.
fn perf_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PANELD_PERF").is_some())
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
                let args = live.calls.get(&key).map(|c| c.args.clone()).unwrap_or_default();
                dirty |= call_done(live, &key, ok);
                dirty |= action_done(live, screen, &name, &args, ok, result);
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
        if perf_on() {
            eprintln!("perf event at={}", paneld_ui::perf::mono_us());
        }
        match ev {
            paneld_ui::UiEvent::Action { name, args } => {
                // A tap queued before a modal opened in this same drain
                // is stale once the modal is up: drop it.
                if live.pending.is_none() {
                    route_call(live, screen, actions::Call::new(name, args), false);
                }
            }
            paneld_ui::UiEvent::LateAction { name, mut args, late } => {
                if live.pending.is_none() {
                    let ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
                    args.extend(actions::late_args(&late, &args, &ctx));
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
                                &action_title(live, &call.name, &call.args),
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
                            show_ask_modal(screen, &action_title(live, &call.name, &call.args), &asked.param, &asked.options, &live.event_tx);
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
                let args = live.calls.get(&key).map(|c| c.args.clone()).unwrap_or_default();
                // Its buttons leave `:running`.
                dirty |= call_done(live, &key, ok);
                if action_done(live, screen, &name, &args, ok, result) {
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
                // Schema 8.4: the new value is `value` and `event.value`.
                let mut event = std::collections::BTreeMap::new();
                event.insert("value".to_string(), v.clone());
                ctx.insert("event".into(), paneld_ui::Value::Map(event));
                ctx.insert("value".into(), v);
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
        // A run started: only `running` changed.
        if update.began {
            dirty |= visible::running_on_page(&update.name);
            continue;
        }
        // A run that started before the value on screen was taken is
        // stale (a slow refresh overtaken by a poll).
        match live.source_started.get(&update.name) {
            // Its run is over all the same: `running` may have changed.
            Some(shown) if update.started < *shown => {
                dirty |= visible::running_on_page(&update.name);
                continue;
            }
            _ => {
                live.source_started.insert(update.name.clone(), update.started);
            }
        }
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
            dirty |= visible::on_page(&update.name);
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
        // Only a page that reads the source changes (an alert it
        // raises marks the frame dirty in the tick below).
        dirty |= visible::on_page(&update.name);
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

    // A call dispatched in this pass, shown on the page: its buttons
    // show `:running`.
    dirty |= std::mem::take(&mut live.redraw);
    if screen_step(live) {
        dirty = true;
    }
    // A page that reads `now` (or `ago()`): redrawn when the minute
    // (or second) it shows is over. Wakes the loop then, not earlier;
    // none of this while asleep or on a page without the clock.
    if let Some(tick) = visible::clock_on_page() {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        if live.clock_drawn != Some((tick, tick.key(since_epoch.as_secs() as i64))) {
            dirty = true;
        }
        // A little past the change, so the clock has moved when we look.
        paneld_ui::wake_within(tick.ms_to_next(since_epoch) + 2);
    }
    // An `ago()` text on the page: redrawn when it would read otherwise
    // (`5m ago` turning `6m ago`), and the loop wakes then. Not asleep.
    if let Some(due) = live.ago_due.filter(|_| !live.screen.asleep()) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let due_ms = due.saturating_mul(1000);
        if now_ms >= due_ms {
            dirty = true;
        } else {
            paneld_ui::wake_within((due_ms - now_ms) as u64 + 2);
        }
    }
    // Asleep: no redraws (ADR 0001, idle means asleep); render once
    // on wake instead.
    if dirty && live.screen.asleep() {
        live.redraw = true;
        dirty = false;
    }
    // A page coming in is not redrawn under way: the change shows
    // when it is in (its end wakes the loop).
    if dirty && paneld_ui::page_transition_running() {
        live.redraw = true;
        dirty = false;
    }
    // Nor while the user is in the middle of something on it (a finger
    // down, a slider or roller dragged, a dropdown open): a redraw would
    // drop the press or close the list. It follows as soon as that ends
    // (a release wakes the loop). A navigation goes ahead.
    if dirty && live.current_page == live.rendered_page && paneld_ui::interacting() {
        live.redraw = true;
        dirty = false;
    }

    if dirty && live.pending.is_none() {
        let perf_t0 = perf_on().then(std::time::Instant::now);
        if perf_t0.is_some() {
            paneld_ui::perf::take();
        }
        let lap = |t: &mut Option<std::time::Instant>| -> u128 {
            t.map_or(0, |x| {
                let now = std::time::Instant::now();
                *t = Some(now);
                (now - x).as_micros()
            })
        };
        let mut perf_lap = perf_t0;
        let drawn_at = clock::unix_now();
        let ctx = build_ctx(
            &live.source_values,
            &live.theme,
            &live.vars,
            Some(&live.alerts),
        );
        let perf_ctx = lap(&mut perf_lap);
        // A poll landing mid-entry must not wipe digits the user
        // is typing into a keypad on the same page.
        let typed = paneld_ui::snapshot_inputs();
        let same_page = live.rendered_page == live.current_page;
        // After a sleep everything is drawn as it is now, at once.
        let instant = std::mem::take(&mut live.instant);
        if instant {
            paneld_ui::motion::next_render_instant();
        }
        // Only a navigation moves the page (themes.md, "Motion"); a
        // value change redraws it in place. Not with fonts to free:
        // the page being left uses them.
        let transition = if same_page || instant || live.release_fonts {
            None
        } else {
            paneld_ui::motion::Settings::from_theme(Some(&live.theme)).page_transition()
        };
        if transition.is_some() {
            paneld_ui::begin_page_change(screen);
        } else {
            screen.clear_children();
        }
        // After a theme switch: free the old theme's fonts, now that no
        // object uses them (the toast goes too; no dialog is up here),
        // as a reload does. Else switching themes fills the font cache.
        if std::mem::take(&mut live.release_fonts) {
            screen.top_layer().clear_children();
            live.toast_until = None;
            screen.release_fonts();
        }
        let perf_clear = lap(&mut perf_lap);
        paneld_ui::reset_action_bindings();
        paneld_ui::set_call_states(live.calls.values().cloned().collect());
        paneld_ui::take_ago_next();
        paneld_ui::render_page(
            &live.ui_doc,
            &live.current_page,
            &ctx,
            &live.event_tx,
            screen,
        );
        let perf_render = lap(&mut perf_lap);
        let perf_fine = if perf_t0.is_some() { paneld_ui::perf::take() } else { String::new() };
        live.ago_due = min_due(paneld_ui::take_ago_next(), age_due(live));
        if same_page {
            paneld_ui::restore_inputs(&typed);
        }
        if let Some((slide, ms)) = transition {
            let back = live.went_back
                || matches!(
                    (nav_pos(&live.ui_doc, &live.current_page), nav_pos(&live.ui_doc, &live.rendered_page)),
                    (Some(to), Some(from)) if to < from
                );
            paneld_ui::run_page_transition(screen, slide, back, ms);
        }
        live.went_back = false;
        if let Some(t0) = perf_t0 {
            let perf_tr = lap(&mut perf_lap);
            eprintln!(
                "perf build page={} same={} us={} ctx={} clear={} render={} [{}] tr={} at={}",
                live.current_page,
                same_page,
                t0.elapsed().as_micros(),
                perf_ctx,
                perf_clear,
                perf_render,
                perf_fine,
                perf_tr,
                paneld_ui::perf::mono_us(),
            );
        }
        live.rendered_page = live.current_page.clone();
        RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        visible::show(&live.current_page);
        live.clock_drawn = visible::clock_on_page().map(|t| (t, t.key(drawn_at)));
    }
}

/// Everything the main loop re-creates on a `reload`.
struct LiveState {
    doc: paneld_config::parse::Document,
    /// `doc` as the pages are drawn: the active theme's components in
    /// place of the config's and the library's (themes.md).
    ui_doc: paneld_config::parse::Document,
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
    /// When the run behind each source's shown value started.
    source_started: std::collections::HashMap<String, std::time::Instant>,
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
    /// Calls running or whose last run failed, by run key, for the
    /// buttons that make them (`:running`, `:failed`).
    calls: std::collections::HashMap<String, paneld_ui::CallState>,
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
    /// The theme the night hours ask for (`None`: the config's).
    screen_theme: Option<String>,
    /// The theme chosen with `paneld ctl theme <name>`: wins over the
    /// config's and the night's until `theme --reset` or a restart.
    ctl_theme: Option<String>,
    /// A theme switch happened: free the fonts at the next redraw.
    release_fonts: bool,
    /// The sleep cover was touched since the last step.
    touched: bool,
    /// The screen woke: the next render sets every value at once.
    instant: bool,
    /// The last navigation was `goto = back` (a slide comes from the
    /// left).
    went_back: bool,
    /// What the page drawn last reads of the clock, and the minute or
    /// second it was drawn in: a new one redraws it.
    clock_drawn: Option<(clock::Tick, i64)>,
    /// When the soonest `ago()` text on the page drawn last changes
    /// (Unix seconds): it is redrawn then.
    ago_due: Option<i64>,
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
    let (source, mut doc, mut diags) = match paneld_config::load(config) {
        Ok(parts) => parts,
        Err(e) => return Err(vec![ctl::CtlError::msg(e.to_string())]),
    };
    diags.extend(paneld_config::check(&doc, &source));
    // A broken theme folder nothing names: said, and left out.
    let (diags, warnings) = paneld_config::split_warnings(&mut doc, &source, diags);
    for w in &warnings {
        eprintln!("paneld: warning: {}: {}", source.location(w.span), w.message);
    }
    // Edits to any file read here reload the config, a broken one's too.
    ctl::watch_files(source.files().map(|(_, p)| p.to_path_buf()).filter(|p| p.is_absolute()).collect());
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
    previous: Option<&LiveState>,
) -> Result<LiveState, Vec<ctl::CtlError>> {
    let previous_sources = previous.map(|p| p.source_values.lock().unwrap().clone());
    let previous_sources = previous_sources.as_ref();
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
    // What the pages are drawn from: the active theme's components in
    // place of the config's and the library's.
    let ui_doc = themes::ui_doc(&doc, None);
    install_visible(&doc, &ui_doc);
    // `now` in local time; a reload reads the zone again.
    clock::set_zone(tz::Zone::local());
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
    let secret_values = secrets::resolve(&doc, &config_dir);
    let pin_gate = pin::Gate::from_doc(&doc, &secret_values);
    let theme = themes::resolve(&doc);
    apply_theme_ui(&doc, &theme, None);
    let vars_map = vars::initial_values(&doc);
    let var_types = vars::types(&doc);
    let vars_shared = match previous {
        // A reload keeps the same vars: a var still declared with the
        // same type keeps a value the screen set (an edited default
        // applies to one still at its old default), and a run started
        // before the reload sets them in the map the screen reads.
        Some(p) => {
            let old_defaults = vars::initial_values(&p.doc);
            let mut old = p.vars.lock().unwrap();
            let kept = vars_map
                .into_iter()
                .map(|(k, v)| match (old.get(&k), p.var_types.get(&k)) {
                    (Some(value), Some(ty)) if var_types.get(&k) == Some(ty) && old_defaults.get(&k) != Some(value) => {
                        (k, value.clone())
                    }
                    _ => (k, v),
                })
                .collect();
            *old = kept;
            p.vars.clone()
        }
        None => std::sync::Arc::new(std::sync::Mutex::new(vars_map)),
    };
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
            event_tx: event_tx.clone().into(),
            source_blocks,
            source_tx: source_tx.into(),
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
    let drawn_at = clock::unix_now();
    let ctx = build_ctx(
        &source_values,
        &theme,
        &vars_shared,
        Some(&alerts_engine),
    );
    screen.clear_children();
    // A toast from before a reload belongs to the old state.
    screen.top_layer().clear_children();
    // No object points at a font now: free them, so fonts tried across
    // reloads do not fill the cache and an edited `.ttf` is read again.
    screen.release_fonts();
    paneld_ui::reset_action_bindings();
    // A reload: what the old config drew is no guide to the new one.
    paneld_ui::motion::forget();
    paneld_ui::take_ago_next();
    paneld_ui::render_page(&ui_doc, &current_page, &ctx, &event_tx, screen);
    let ago_due = paneld_ui::take_ago_next();
    RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    visible::show(&current_page);
    let clock_drawn = visible::clock_on_page().map(|t| (t, t.key(drawn_at)));

    let screen_state =
        screen::Screen::new(screen::Config::from_doc(&doc), tz::Zone::local(), config_dir.clone());
    Ok(LiveState {
        doc,
        ui_doc,
        source_values,
        source_rx,
        source_started: Default::default(),
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
        calls: std::collections::HashMap::new(),
        stale_events: Vec::new(),
        previous_page: None,
        toast_until: None,
        redraw: false,
        screen: screen_state,
        screen_theme: None,
        ctl_theme: None,
        release_fonts: false,
        touched: false,
        instant: false,
        went_back: false,
        clock_drawn,
        ago_due,
        pin_gate,
    })
}

/// Which sources each page reads (`active = visible`), through the
/// components the pages are drawn with (`ui_doc`, the active theme's).
fn install_visible(doc: &paneld_config::parse::Document, ui_doc: &paneld_config::parse::Document) {
    visible::install(
        ui_doc,
        &paneld_config::remote::remotes(doc)
            .into_iter()
            .map(|(name, r)| {
                let values = paneld_config::remote::members(&r.kind)
                    .filter(|(_, b)| b.kind.text == "value")
                    .map(|(m, _)| m.to_string())
                    .collect();
                (name, values)
            })
            .collect(),
    );
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
            show_ask_modal(screen, &action_title(live, &call.name, &call.args), &param, &options, &live.event_tx);
            live.pending = Some(Pending { call, stage: Stage::Ask(Box::new(Asking { param: *param, options })) });
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
        show_pin_modal(screen, &action_title(live, &call.name, &call.args), None, &live.event_tx);
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
        show_confirm_modal(screen, &action_title(live, &call.name, &call.args), &message, &live.event_tx);
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
                // Nothing is on screen: `active = visible` sources rest.
                visible::set_asleep(true);
            }
            screen::Effect::Wake => {
                paneld_ui::hide_sleep_cover();
                live.instant = true;
                visible::set_asleep(false);
                // An alert wake has no touch: restart the idle clock.
                paneld_ui::trigger_activity();
                screen_command(&live.screen, live.screen.cfg.on.clone(), None);
                redraw = true;
            }
            screen::Effect::Dim(level) => {
                screen_command(&live.screen, live.screen.cfg.dim.clone(), Some(level));
            }
            screen::Effect::Theme(name) => {
                live.screen_theme = name;
                // A theme chosen over ctl stays until `theme --reset`.
                if live.ctl_theme.is_none() {
                    apply_active_theme(live);
                    redraw = true;
                }
            }
        }
    }
    redraw
}

/// The theme in force: one chosen over ctl, else the night theme in
/// night hours, else the config's (`None`).
fn active_theme(live: &LiveState) -> Option<&str> {
    live.ctl_theme.as_deref().or(live.screen_theme.as_deref())
}

/// Apply the theme in force: tokens, styles, and components (another
/// shell, other tiles), and which sources each page reads through them.
fn apply_active_theme(live: &mut LiveState) {
    let name = active_theme(live).map(str::to_string);
    live.theme = themes::resolve_with(&live.doc, name.as_deref());
    apply_theme_ui(&live.doc, &live.theme, name.as_deref());
    live.ui_doc = themes::ui_doc(&live.doc, name.as_deref());
    install_visible(&live.doc, &live.ui_doc);
    visible::show(&live.current_page);
    live.redraw = true;
    // Other components: nothing moves on the switch.
    paneld_ui::motion::forget();
    // The old theme's fonts go with the old screen, at the next redraw.
    live.release_fonts = true;
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
    live.running.insert(key.clone(), call.name.clone());
    // A new run of the action clears its failed mark. Only a page
    // with a button for it changes.
    let mut shown = paneld_ui::shows_call(&call.name, &call.args);
    live.calls.retain(|_, c| {
        let cleared = c.name == call.name && c.state == paneld_ui::RunState::Failed;
        shown |= cleared && paneld_ui::shows_call(&c.name, &c.args);
        !cleared
    });
    let busy = live.actions.get(&call.name).and_then(|def| {
        let b = def.busy.as_ref()?;
        Some(paneld_ui::interpolate(&b.raw, b.span.file, b.span.start, &call_ctx(live, def, &call.args)))
    });
    live.calls.insert(
        key,
        paneld_ui::CallState {
            name: call.name.clone(),
            args: call.args.clone(),
            state: paneld_ui::RunState::Running,
            busy,
        },
    );
    if shown {
        live.redraw = true;
    }
    let _ = live.action_tx.send(call);
}

/// A call finished: off its buttons when it succeeded, `:failed` on
/// them until the action runs again when it did not. True when a
/// button on screen shows it.
fn call_done(live: &mut LiveState, key: &str, ok: bool) -> bool {
    let shown = live.calls.get(key).is_some_and(|c| paneld_ui::shows_call(&c.name, &c.args));
    if ok {
        live.calls.remove(key);
    } else if let Some(c) = live.calls.get_mut(key) {
        c.state = paneld_ui::RunState::Failed;
    }
    shown
}

/// The context an action's messages (`busy`, `done`, `failed`) read:
/// the page's, the call's args, and a remote kind's params.
fn call_ctx(live: &LiveState, def: &actions::ActionDef, args: &[paneld_ui::ActionArg]) -> paneld_ui::Context {
    let mut ctx = build_ctx(&live.source_values, &live.theme, &live.vars, Some(&live.alerts));
    for a in args {
        ctx.insert(a.name.clone(), a.value.clone());
    }
    // Bound last, as when the command runs: a kind's params win.
    def.bind(&mut ctx);
    ctx
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
    live.went_back = page == "back";
    true
}

/// A page's `nav` position: a slide to a page before the one shown
/// comes from the left.
fn nav_pos(doc: &paneld_config::parse::Document, page: &str) -> Option<i64> {
    use paneld_config::parse::{AssignTarget, Item};
    doc.items.iter().find_map(|item| match item {
        Item::Block(b) if b.kind.text == "page" && b.name.as_ref().is_some_and(|n| n.text == page) => {
            b.items.iter().find_map(|it| match it {
                Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "nav") => {
                    a.value.text.trim().parse::<i64>().ok()
                }
                _ => None,
            })
        }
        _ => None,
    })
}

/// Finish an action call (schema 5): `goto` on success, and the
/// `done` / `failed` message as a toast. Returns true when the page
/// changed.
fn action_done(
    live: &mut LiveState,
    screen: &paneld_ui::ui::Obj<'_>,
    name: &str,
    args: &[paneld_ui::ActionArg],
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
            let mut ctx = call_ctx(live, &def, args);
            ctx.insert("output".into(), paneld_ui::Value::String(field("stdout").trim().to_string()));
            ctx.insert("result".into(), result.clone());
            paneld_ui::interpolate(&d.raw, d.span.file, d.span.start, &ctx)
        })
    } else {
        Some(match &def.failed {
            Some(f) => {
                let mut ctx = call_ctx(live, &def, args);
                ctx.insert("result".into(), result.clone());
                paneld_ui::interpolate(&f.raw, f.span.file, f.span.start, &ctx)
            }
            None => {
                let err = field("stderr");
                let err = err.trim();
                if err.is_empty() {
                    match action_title(live, name, args) {
                        t if t.is_empty() => "Failed".to_string(),
                        t => format!("{t} failed"),
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
        paneld_ui::render_toast(&screen.top_layer(), &text, token, &live.theme);
        live.toast_until = Some(std::time::Instant::now() + TOAST_FOR);
    }
    match (ok, def.goto.as_deref()) {
        (true, Some(page)) => go_to(live, page),
        _ => false,
    }
}

/// How long a `done` / `failed` toast stays up.
const TOAST_FOR: std::time::Duration = std::time::Duration::from_secs(3);

/// What a theme sets beyond its tokens: node and class styles, and
/// the dialog backdrop (`dialog_backdrop`, default transparent).
fn apply_theme_ui(
    doc: &paneld_config::Document,
    tokens: &std::collections::BTreeMap<String, paneld_ui::Value>,
    theme: Option<&str>,
) {
    paneld_ui::set_styles(themes::styles(doc, theme));
    let backdrop = tokens
        .get("dialog_backdrop")
        .and_then(|v| paneld_ui::Backdrop::parse(&v.to_display_string()))
        .unwrap_or(paneld_ui::Backdrop::Transparent);
    paneld_ui::set_dialog_backdrop(backdrop);
}

/// The action's `title` (schema 5), filled in against the current
/// state and the call's args (as `busy`, `done` and `failed`); empty
/// without one. Dialogs and messages never show the action's own
/// name, which is for the config.
fn action_title(live: &LiveState, name: &str, args: &[paneld_ui::ActionArg]) -> String {
    let Some(def) = live.actions.get(name) else { return String::new() };
    let Some(t) = &def.title else { return String::new() };
    let ctx = call_ctx(live, def, args);
    let text = paneld_ui::interpolate(&t.raw, t.span.file, t.span.start, &ctx);
    // An expression that does not evaluate comes back as written: no
    // title then, not `{{ ... }}` on screen.
    if text.contains("{{") { String::new() } else { text.trim().to_string() }
}

/// A dialog over the page: the page stays on screen, under a backdrop
/// (theme `dialog_backdrop`) that takes the taps. The page's bindings
/// are dropped, so only the dialog's own act.
fn dialog_layer<'a>(screen: &paneld_ui::ui::Obj<'a>) -> paneld_ui::ui::Obj<'a> {
    paneld_ui::reset_action_bindings();
    screen.new_dialog_layer()
}

/// PIN entry modal: masked textarea in a msgbox plus a numeric
/// keyboard. The keyboard's OK sends `PinEntered`; Cancel sends a
/// `ConfirmReply { accept: false }` which drops the parked call.
fn show_pin_modal(
    screen: &paneld_ui::ui::Obj<'_>,
    title: &str,
    error: Option<&str>,
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    let layer = dialog_layer(screen);
    let box_ = layer.new_msgbox();
    box_.msgbox_add_title(&if title.is_empty() { "Enter PIN".to_string() } else { format!("PIN for {title}") });
    if let Some(e) = error {
        box_.msgbox_add_text(e);
    }
    let ta = box_.msgbox_content().new_textarea(true, 12);
    ta.set_size(paneld_ui::ui::Size::Percent(100), paneld_ui::ui::Size::Content);
    box_.msgbox_add_confirm_button("Cancel", false, sink.clone());
    // The keyboard takes the bottom half of the screen; keep the box
    // (and its Cancel) in the top half.
    box_.align_top_mid(8);
    let kb = layer.new_number_keyboard();
    kb.attach_keyboard_to(&ta);
    kb.on_submit_pin(sink.clone());
}

/// A dialog asking for `param` (schema 6 `ask`): a dropdown of its
/// `options`, a slider over `min`..`max` with its value shown, a
/// toggle, or a digit keypad, with Cancel and OK. The heading is the
/// action's `title` when it has one, else the param's label.
fn show_ask_modal(
    screen: &paneld_ui::ui::Obj<'_>,
    title: &str,
    param: &actions::ParamDef,
    options: &[paneld_ui::Value],
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    use paneld_ui::ui::{Size, ValueWidget};
    use paneld_ui::EventValue;
    let layer = dialog_layer(screen);
    let box_ = layer.new_msgbox();
    let label = param.label.clone().unwrap_or_else(|| param.name.clone());
    if title.is_empty() {
        box_.msgbox_add_title(&label);
    } else {
        box_.msgbox_add_title(title);
        box_.msgbox_add_text(&label);
    }
    let content = box_.msgbox_content();
    match param.ask.as_deref() {
        Some("slider") => {
            let (min, max) = (param.min.unwrap_or(0.0) as i32, param.max.unwrap_or(100.0) as i32);
            let shown = content.new_label(&min.to_string());
            let s = content.new_slider();
            s.set_slider_range(min, max);
            s.set_slider_value(min);
            // A fixed width: the dialog takes its contents' size.
            s.set_size(Size::Px(260), Size::Content);
            s.show_value_in(&shown);
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
            let kb = layer.new_number_keyboard();
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
    let layer = dialog_layer(screen);
    let box_ = layer.new_msgbox();
    box_.msgbox_add_text(message);
    box_.msgbox_add_confirm_button("OK", false, sink.clone());
}

/// An action's `confirm` question over the page; the dialog is the
/// only thing that reacts until the user answers.
fn show_confirm_modal(
    screen: &paneld_ui::ui::Obj<'_>,
    title: &str,
    message: &str,
    sink: &std::sync::mpsc::Sender<paneld_ui::UiEvent>,
) {
    let layer = dialog_layer(screen);
    let box_ = layer.new_msgbox();
    box_.msgbox_add_title(if title.is_empty() { "Confirm" } else { title });
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

    let (source, mut doc, mut diags) = match load(&config) {
        Ok(parts) => parts,
        Err(e) => {
            eprintln!("paneld check: {e}");
            return ExitCode::from(1);
        }
    };
    diags.extend(check(&doc, &source));
    // A broken theme folder nothing names: a warning (it is left out).
    let (diags, warnings) = paneld_config::split_warnings(&mut doc, &source, diags);
    for w in &warnings {
        eprintln!("warning: {}: {}", source.location(w.span), w.message);
    }

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
    if o.rotation != 0 && !mem.set_rotation(o.rotation) {
        eprintln!("paneld shot: could not rotate by {}", o.rotation);
        return ExitCode::from(70);
    }
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
        // The page asked for is the start: drawn as it is, no transition.
        live.instant = true;
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
    match setup_live_state(config, screen, state_override.as_deref(), Some(live)) {
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
            fresh.calls = std::mem::take(&mut live.calls);
            fresh.stale_events = std::mem::take(&mut live.stale_events);
            let (tx, rx) = std::sync::mpsc::channel();
            drop(tx);
            fresh.stale_events.push(std::mem::replace(&mut live.event_rx, rx));
            // Stop the old poll / stream workers; the new state runs
            // its own.
            live.sources_shutdown.stop();
            // A theme chosen over ctl stays if the new config still has
            // it and can draw its pages with it.
            let ctl_theme = live.ctl_theme.take().filter(|t| match paneld_config::check_theme_switch(&fresh.doc, t) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("paneld: {e}; back to the config's theme");
                    false
                }
            });
            *live = fresh;
            if ctl_theme.is_some() {
                live.ctl_theme = ctl_theme;
                apply_active_theme(live);
            }
            eprintln!("paneld: reloaded config");
            display::note_reload(&live.doc);
            raise_display_problem(live);
            Ok(())
        }
        Err(issues) => {
            eprintln!("paneld: reload failed; keeping previous config");
            Err(issues)
        }
    }
}

/// The config's display that did not open at start, if another one
/// did (schema 13): shown as an alert until a restart.
static DISPLAY_PROBLEM: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn raise_display_problem(live: &mut LiveState) {
    if let Some(Some(problem)) = DISPLAY_PROBLEM.get() {
        let now = std::time::SystemTime::now();
        if live.alerts.fire_rule("paneld", "display", "warning", "Display config not used", problem, "", now) {
            live.redraw = true;
        }
    }
}

/// The name of the theme in force, as `paneld ctl theme` reports it
/// (empty with no theme at all).
fn active_theme_name(live: &LiveState) -> String {
    active_theme(live)
        .map(str::to_string)
        .or_else(|| themes::active_theme_name(&live.doc))
        .unwrap_or_default()
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
        "theme" => {
            if let Some(name) = req.opt_arg("name") {
                paneld_config::check_theme_switch(&live.doc, &name).map_err(ctl::CtlError::msg)?;
                live.ctl_theme = Some(name);
                apply_active_theme(live);
            }
            Ok(Value::String(active_theme_name(live)))
        }
        "theme_reset" => {
            if live.ctl_theme.take().is_some() {
                apply_active_theme(live);
            }
            Ok(Value::String(active_theme_name(live)))
        }
        "status" => {
            let mut m = BTreeMap::new();
            m.insert("theme".to_string(), Value::String(active_theme_name(live)));
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
            // Motion (ADR 0023): animations running, and how long the
            // loop's last sleep could be (1000 ms when nothing moves).
            m.insert("animations".to_string(), Value::Int(paneld_ui::running_animations() as i64));
            m.insert("idle_ms".to_string(), Value::Int(paneld_ui::idle_hint_ms() as i64));
            // Full page renders since start: what the panel redrew.
            m.insert("renders".to_string(), Value::Int(RENDERS.load(std::sync::atomic::Ordering::Relaxed) as i64));
            // ADR 0024: LVGL objects on the screen and its top layer
            // (dialogs, toast), and the process's resident memory.
            let nodes = screen.descendant_count() + screen.top_layer().descendant_count();
            m.insert("nodes".to_string(), Value::Int(nodes as i64));
            if let Some(kb) = rss_kb() {
                m.insert("rss_kb".to_string(), Value::Int(kb));
            }
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
    clock::insert(&mut ctx);
    ctx
}

/// When a `sources.x.age` the page on screen shows is next drawn
/// again: on `ago()`'s steps (each minute under an hour, ...), not
/// each second, so a page showing an age costs what one with `ago()`
/// does.
fn age_due(live: &LiveState) -> Option<i64> {
    let names = visible::ages_on_page();
    if names.is_empty() {
        return None;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let values = live.source_values.lock().unwrap();
    let at = |v: &paneld_ui::Value| match v {
        paneld_ui::Value::Map(m) => match m.get("at") {
            Some(paneld_ui::Value::Int(t)) => Some(*t),
            _ => None,
        },
        _ => None,
    };
    names.iter().filter_map(|k| values.get(k).and_then(at)).map(|t| paneld_ui::ago_due(now, now - t)).min()
}

fn min_due(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// `VmRSS` of this process in KiB, for `ctl status` (`rss_kb`).
fn rss_kb() -> Option<i64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Full page renders since start, for `ctl status` (`renders`).
static RENDERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
        let Ok(Cmd::Shot(o)) = parse(&["shot", "-o", "x.png", "--rotation", "270"]) else { panic!() };
        assert_eq!(o.rotation, 270);
        assert!(parse(&["shot", "-o", "x.png", "--rotation", "45"]).is_err());
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
