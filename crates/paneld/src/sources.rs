//! Resolve top-level `source` blocks into evaluated [`Value`]s.
//!
//! Scope today (first live-data milestone): a one-shot resolution at
//! daemon startup. For each `source NAME { run = ...; parse = ...;
//! value = {{ ... }} }` in the config:
//!
//! 1. Execute `run` through [`crate::runner`] with a timeout.
//! 2. Parse stdout per `parse` (`number`, `line`, `string`, `json`).
//! 3. If a `value = {{ ... }}` is set, evaluate it with `out` bound
//!    to the parsed stdout. Otherwise `out` itself is the value.
//!
//! The result is a `BTreeMap<String, Value>` ready to splice into an
//! [`paneld_ui::Context`] under the key `sources`, so the renderer
//! can look up `sources.cpu_temp.value` naturally.
//!
//! Polling, source history, streaming / on-demand sources and secret
//! substitution come in later phases.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
// Sends wake the UI thread's pump, which waits in `poll`.
use paneld_ui::WakingSender as Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_config::value::parse_duration;
use paneld_ui::{Context, Value};

use crate::runner;

/// A fresh value for a named source, broadcast each time a polling
/// worker runs its `run` command.
#[derive(Debug)]
pub struct SourceUpdate {
    pub name: String,
    pub value: Value,
    /// Only `ok` / `error` changed (a run with no result, a stream that
    /// stopped or came back): the main loop keeps the rest of the last
    /// value, records no history and sends no alert event.
    pub status: bool,
    /// When the run that produced it started: the main loop drops an
    /// update older than the one it shows (a slow run overtaken by a
    /// newer one, as after a reload).
    pub started: Instant,
    /// Only a run started (`running` turned true); nothing else changed.
    pub began: bool,
}

/// How many runs of each source are under way (a poll and a refresh
/// can overlap; a stream counts while its command lives): schema 4's
/// `running`.
static RUNNING: Mutex<BTreeMap<String, usize>> = Mutex::new(BTreeMap::new());

/// Counts a run of a source while it is held.
pub struct Run(String);

impl Run {
    /// A run of `name` starts: `running` is true until it is dropped.
    /// `tx`, when given, hears of it so a page showing `running` is
    /// redrawn (the end comes with the run's own update).
    pub fn start(name: &str, tx: Option<&Sender<SourceUpdate>>) -> Run {
        *RUNNING.lock().unwrap_or_else(|e| e.into_inner()).entry(name.to_string()).or_default() += 1;
        if let Some(tx) = tx {
            let _ = tx.send(SourceUpdate { name: name.to_string(), value: Value::Null, status: false, started: Instant::now(), began: true });
        }
        Run(name.to_string())
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let mut r = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = r.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                r.remove(&self.0);
            }
        }
    }
}

/// The sources with a run under way now (schema 4's `running`): one
/// snapshot, so a context and the changes told from it agree.
pub fn running_names() -> std::collections::BTreeSet<String> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect()
}

/// Schema 4's fields that follow the clock and the runs, put into each
/// source map as a context is built: `running`, and `age` (whole
/// seconds since `at`, for a source that has a result).
pub fn add_live_fields(sources: &mut BTreeMap<String, Value>, now_unix: i64, running: &std::collections::BTreeSet<String>) {
    for (name, v) in sources.iter_mut() {
        let Value::Map(m) = v else { continue };
        m.insert("running".into(), Value::Bool(running.contains(name)));
        if let Some(Value::Int(at)) = m.get("at") {
            let age = now_unix.saturating_sub(*at).max(0);
            m.insert("age".into(), Value::Int(age));
        }
    }
}

/// Resolve every top-level `source` block in `doc`, producing the
/// map that lives under `sources` in a [`Context`].
///
/// Writes a one-line warning to stderr for each source that failed to
/// run; that source gets a [`not_ok`] value with the reason.
pub fn resolve_all(
    doc: &Document,
    ctx: &Context,
    base_cwd: &std::path::Path,
    app_defaults: runner::AppDefaults,
) -> BTreeMap<String, Value> {
    // All at once (within `app:max_running`): the wait is the slowest
    // source, not the sum of them.
    let blocks: Vec<(&str, &Block)> = doc
        .items
        .iter()
        .filter_map(|item| match item {
            // Stream sources are long-lived children started by
            // `spawn_pollers`; resolving them as a one-shot would just
            // time out and log a spurious error on every reload.
            Item::Block(b) if b.kind.text == "source" && !is_stream(b) => {
                b.name.as_ref().map(|n| (n.text.as_str(), b))
            }
            _ => None,
        })
        .collect();
    std::thread::scope(|scope| {
        let runs: Vec<_> = blocks
            .iter()
            .map(|&(name, b)| (name, scope.spawn(move || resolve_one(b, ctx, base_cwd, app_defaults))))
            .collect();
        let mut out = BTreeMap::new();
        for (name, run) in runs {
            match run.join() {
                Ok(Ok(v)) => {
                    out.insert(name.to_string(), v);
                }
                Ok(Err(e)) => {
                    eprintln!("paneld: source `{name}`: {e}");
                    out.insert(name.to_string(), not_ok(&e));
                }
                Err(_) => {
                    eprintln!("paneld: source `{name}`: the run panicked");
                    out.insert(name.to_string(), not_ok("the run panicked"));
                }
            }
        }
        out
    })
}

/// `out` through the source's `value = {{ ... }}` (or as it is),
/// masked again: `value` can put masked pieces back together.
fn transformed(b: &Block, out: &Value, mask: &crate::secrets::Mask) -> Value {
    match find_raw(b, "value") {
        Some((raw, span)) => mask.apply_value(
            eval_value_transform(b, raw, span, out).unwrap_or_else(|e| Value::String(format!("[eval error: {e}]"))),
        ),
        None => out.clone(),
    }
}

/// The value of a source that has no result (yet): `ok = false`,
/// `value = null` and the reason in `error`, so pages and alerts can
/// test `ok` for every declared source.
pub fn not_ok(error: &str) -> Value {
    Value::Map(BTreeMap::from([
        ("value".to_string(), Value::Null),
        ("ok".to_string(), Value::Bool(false)),
        ("error".to_string(), Value::String(error.to_string())),
    ]))
}

/// A [`not_ok`] value: the run produced no result (it failed to
/// start, timed out, or its output did not parse), as opposed to a
/// command that ran and exited non-zero.
pub fn is_failure(v: &Value) -> bool {
    matches!(v, Value::Map(m) if m.get("ok") == Some(&Value::Bool(false)) && !m.contains_key("exit"))
}

/// Give every source of `doc` missing from `values` a [`not_ok`]
/// value: stream sources before their first line, and sources added
/// by a reload before their first run.
pub fn fill_not_run(doc: &Document, values: &mut BTreeMap<String, Value>) {
    for name in collect_blocks(doc).into_keys() {
        values.entry(name).or_insert_with(|| not_ok("not run yet"));
    }
}

/// [`resolve_all`] on a thread of its own, each value sent on `tx` as
/// it would come from a poll. A reload uses it so the screen keeps
/// the previous values meanwhile instead of waiting.
pub fn resolve_all_in_background(
    doc: Document,
    ctx: Context,
    base_cwd: std::path::PathBuf,
    app_defaults: runner::AppDefaults,
    tx: impl Into<Sender<SourceUpdate>>,
) {
    let tx = tx.into();
    thread::spawn(move || {
        let started = Instant::now();
        for (name, value) in resolve_all(&doc, &ctx, &base_cwd, app_defaults) {
            if tx.send(SourceUpdate { status: is_failure(&value), name, value, started, began: false }).is_err() {
                return;
            }
        }
    });
}

/// Spawn a background worker for each source that declares a
/// `poll = DURATION`. Each worker sleeps for that interval, re-runs
/// the source's command, and emits a [`SourceUpdate`] on `tx`.
/// Sources without a `poll` key never re-emit; they stay at whatever
/// value [`resolve_all`] produced. The sender is cloned per worker,
/// so the caller can keep its own clone to hand to other producers
/// (the action `refresh` step reuses it).
pub fn spawn_pollers(
    doc: &Document,
    ctx: &Context,
    tx: impl Into<Sender<SourceUpdate>>,
    base_cwd: &std::path::Path,
    app_defaults: runner::AppDefaults,
) -> ShutdownHandle {
    let tx: Sender<SourceUpdate> = tx.into();
    let shutdown = Arc::new(AtomicBool::new(false));
    let stream_pgids: Arc<Mutex<Vec<runner::Pid>>> = Arc::new(Mutex::new(Vec::new()));
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "source" {
            continue;
        }
        let Some(name) = &b.name else { continue };
        let streaming = is_stream(b);

        if streaming {
            // ADR 0013: stream source. Long-lived child whose stdout
            // feeds one event per line; restart with backoff on exit.
            let block_clone = b.clone();
            let name_clone = name.text.clone();
            let tx_clone: Sender<SourceUpdate> = tx.clone();
            let ctx_clone = ctx.clone();
            let shutdown_clone = shutdown.clone();
            let pgids_clone = stream_pgids.clone();
            let cwd = resolve_cwd(b, base_cwd);
            thread::spawn(move || {
                stream_loop(
                    block_clone,
                    name_clone,
                    tx_clone,
                    ctx_clone,
                    shutdown_clone,
                    pgids_clone,
                    cwd,
                )
            });
            continue;
        }

        let Some(poll_raw) = find_simple(b, "poll") else {
            continue;
        };
        let Some(poll) = parse_duration(poll_raw.trim()) else {
            eprintln!(
                "paneld: source `{}`: unparseable poll `{}`; polling disabled",
                name.text, poll_raw
            );
            continue;
        };
        // Clone the source block owned so the worker thread can read
        // it without holding a borrow of the document. Secrets and
        // theme tokens live in the cloned ctx; they don't change at
        // runtime in v1.
        let block_clone = b.clone();
        let name_clone = name.text.clone();
        let tx_clone: Sender<SourceUpdate> = tx.clone();
        let ctx_clone = ctx.clone();
        let shutdown_clone = shutdown.clone();
        let cwd = resolve_cwd(b, base_cwd);
        thread::spawn(move || {
            poll_loop(
                block_clone,
                name_clone,
                poll,
                tx_clone,
                ctx_clone,
                shutdown_clone,
                cwd,
                app_defaults,
            )
        });
    }
    ShutdownHandle { flag: shutdown, stream_pgids }
}

/// Resolve a block's effective cwd per ADR 0013. `base_cwd` is the
/// main config file's directory. A `cwd = <path>` on the block
/// overrides: an absolute path is used as-is, a relative one joins
/// onto `base_cwd`.
fn resolve_cwd(b: &Block, base_cwd: &std::path::Path) -> std::path::PathBuf {
    let cwd = find_simple(b, "cwd").map(paneld_config::value::parse_string);
    match cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            let p = std::path::Path::new(raw);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base_cwd.join(p)
            }
        }
        None => base_cwd.to_path_buf(),
    }
}

/// `true` for a source block carrying `stream = true` (or an
/// equivalent truthy spelling).
fn is_stream(b: &Block) -> bool {
    find_simple(b, "stream")
        .map(|s| matches!(s.trim(), "true" | "yes" | "1"))
        .unwrap_or(false)
}

/// Returned by [`spawn_pollers`]. Calling `stop` flips the shared
/// flag AND SIGTERMs every live stream child's process group, so a
/// stream whose child is between lines wakes up and sees the flag
/// (its stdout pipe closes). Pollers notice the flag between sleeps.
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
    /// Live stream pgids; each `stream_loop` registers on spawn and
    /// removes on child exit.
    stream_pgids: Arc<Mutex<Vec<runner::Pid>>>,
}

impl ShutdownHandle {
    /// A handle with nothing to stop (no source runs: `paneld shot
    /// --no-sources`).
    pub fn idle() -> ShutdownHandle {
        ShutdownHandle { flag: Arc::new(AtomicBool::new(false)), stream_pgids: Arc::new(Mutex::new(Vec::new())) }
    }

    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        // Pollers wait on the visibility signal: wake them to see it.
        crate::visible::notify();
        let pgids = {
            let g = self.stream_pgids.lock().unwrap();
            g.clone()
        };
        for pgid in pgids {
            runner::kill_group(pgid, runner::Signal::SIGTERM);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn poll_loop(
    block: Block,
    name: String,
    interval: Duration,
    tx: Sender<SourceUpdate>,
    ctx: Context,
    shutdown: Arc<AtomicBool>,
    cwd: std::path::PathBuf,
    app_defaults: runner::AppDefaults,
) {
    let max_delay = backoff_ceiling(interval);
    let mut delay = interval;
    // `active = visible`: only while a page that shows it is on
    // screen; back on screen, at once if the last run is 5s old.
    let when_visible = find_simple(&block, "active").map(str::trim) == Some("visible");
    let mut next = Instant::now() + delay;
    let mut last_run = Instant::now();
    let mut was_shown = true;
    while !shutdown.load(Ordering::SeqCst) {
        let (seen_shown, generation) = crate::visible::is_shown(&name);
        let shown = !when_visible || seen_shown;
        let back = shown && !was_shown && last_run.elapsed() >= Duration::from_secs(5);
        was_shown = shown;
        if !shown || (Instant::now() < next && !back) {
            // Until it is due, or (hidden) until what is shown changes;
            // a page change and a shutdown wake it at once.
            let until = if shown { next } else { Instant::now() + Duration::from_secs(3600) };
            crate::visible::wait(until, generation);
            continue;
        }
        last_run = Instant::now();
        let run = Run::start(&name, Some(&tx));
        let result = resolve_one(&block, &ctx, &cwd, app_defaults);
        drop(run);
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match result {
            Ok(v) => {
                delay = interval;
                next = Instant::now() + delay;
                if tx.send(SourceUpdate { name: name.clone(), value: v, status: false, started: last_run, began: false }).is_err() {
                    // Receiver dropped; the daemon is shutting down.
                    return;
                }
            }
            Err(e) => {
                eprintln!("paneld: source `{name}`: {e}");
                delay = next_backoff(delay, max_delay);
                next = Instant::now() + delay;
                // `ok` turns false; the main loop keeps the last value.
                if tx.send(SourceUpdate { name: name.clone(), value: not_ok(&e), status: true, started: last_run, began: false }).is_err() {
                    return;
                }
            }
        }
    }
}

/// Stream source worker. Spawns the child, reads stdout line by line,
/// sends each parsed line as a [`SourceUpdate`], and restarts with
/// exponential backoff ([`RESTART_FLOOR`] doubling to
/// [`RESTART_CEILING`], reset after [`RESET_AFTER`] of successful
/// run) when the child exits. Each live child's pgid is registered
/// in the shared `pgids` list so [`ShutdownHandle::stop`] can
/// SIGTERM a quiet one and break it out of its blocking stdout read.
fn stream_loop(
    block: Block,
    name: String,
    tx: Sender<SourceUpdate>,
    ctx: Context,
    shutdown: Arc<AtomicBool>,
    pgids: Arc<Mutex<Vec<runner::Pid>>>,
    cwd: std::path::PathBuf,
) {
    const RESTART_FLOOR: Duration = Duration::from_secs(1);
    const RESTART_CEILING: Duration = Duration::from_secs(60);
    const RESET_AFTER: Duration = RESTART_CEILING;
    let mut delay = RESTART_FLOOR;
    let mask = crate::secrets::Mask::from_ctx(&ctx);

    let mut ctx = ctx;
    crate::remotes::bind(&block, &mut ctx);
    while !shutdown.load(Ordering::SeqCst) {
        // A restarted stream reads the clock as of its start.
        crate::clock::insert(&mut ctx);
        let request = match crate::remotes::request(&block, &ctx, Duration::from_secs(10), true) {
            Ok(Some(r)) => Ok(r),
            Ok(None) => {
                eprintln!("paneld: source `{name}`: missing `run`; stream disabled");
                return;
            }
            Err(e) => Err(runner::RunError { message: e }),
        };
        let env_pairs: Vec<(String, String)> = collect_env(&block)
            .into_iter()
            .map(|(k, raw, span)| (k, paneld_ui::interpolate(&raw, span.file, span.start, &ctx)))
            .collect();
        let stdin_bytes = request.as_ref().ok().and_then(|r| r.stdin.clone());
        let opts = runner::Opts {
            timeout: Duration::MAX,
            max_bytes: usize::MAX,
            kind: runner::Kind::Source,
            name: &name,
            cwd: Some(&cwd),
            env: &env_pairs,
            stdin: stdin_bytes.as_deref(),
        };
        let mut command = match request.and_then(|r| runner::prepared_command(&r.argv, &opts)) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(SourceUpdate { name: name.clone(), value: not_ok(&mask.apply(&e.to_string())), status: true, started: Instant::now(), began: false });
                eprintln!("paneld: source `{name}`: {}; retrying in {:?}", mask.apply(&e.to_string()), delay);
                sleep_or_shutdown(delay, &shutdown);
                delay = next_backoff(delay, RESTART_CEILING);
                continue;
            }
        };
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(SourceUpdate {
                    name: name.clone(),
                    value: not_ok(&format!("spawn: {}", mask.apply(&e.to_string()))),
                    status: true,
                    started: Instant::now(),
                    began: false,
                });
                eprintln!("paneld: source `{name}`: spawn: {}; retrying in {:?}", mask.apply(&e.to_string()), delay);
                sleep_or_shutdown(delay, &shutdown);
                delay = next_backoff(delay, RESTART_CEILING);
                continue;
            }
        };
        // Running while the command lives (dropped with this pass).
        let _run = Run::start(&name, None);
        // Running again: healthy until it says otherwise.
        let up = Value::Map(BTreeMap::from([("ok".to_string(), Value::Bool(true)), ("error".to_string(), Value::String(String::new()))]));
        if tx.send(SourceUpdate { name: name.clone(), value: up, status: true, started: Instant::now(), began: false }).is_err() {
            runner::kill_group(runner::Pid::from_raw(child.id() as i32), runner::Signal::SIGTERM);
            let _ = child.wait();
            return;
        }
        if let (Some(data), Some(mut pipe)) = (stdin_bytes, child.stdin.take()) {
            thread::spawn(move || {
                use std::io::Write;
                let _ = pipe.write_all(&data);
            });
        }
        let pid = runner::Pid::from_raw(child.id() as i32);
        // Register this child's pgid so ShutdownHandle::stop can
        // deliver SIGTERM even while we're blocked in read_line.
        {
            let mut g = pgids.lock().unwrap();
            g.push(pid);
        }
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                eprintln!("paneld: source `{name}`: no stdout; retrying");
                let _ = child.kill();
                let _ = child.wait();
                unregister_pgid(&pgids, pid);
                sleep_or_shutdown(delay, &shutdown);
                delay = next_backoff(delay, RESTART_CEILING);
                continue;
            }
        };
        // Discard stderr so the child's pipe doesn't fill up and
        // block it; a dedicated drainer thread keeps it reading.
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let r = BufReader::new(stderr);
                for _ in r.lines().map_while(Result::ok) {}
            });
        }

        let parse_mode = find_simple(&block, "parse")
            .unwrap_or("text")
            .trim()
            .to_string();
        let started = Instant::now();
        let mut reader = BufReader::new(stdout);
        let mut sent_any = false;
        let mut dropped: u64 = 0;
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            let line = match read_line_capped(&mut reader, STREAM_LINE_CAP) {
                Ok(Line::Eof) => break,
                Ok(Line::TooLong) => {
                    // The first one, then every 100th: a misbehaving
                    // child must not flood the journal.
                    dropped += 1;
                    if dropped == 1 || dropped % 100 == 0 {
                        eprintln!(
                            "paneld: source `{name}`: dropped {dropped} line(s) longer than \
                             {STREAM_LINE_CAP} bytes"
                        );
                    }
                    continue;
                }
                Ok(Line::Text(l)) => l,
                Err(e) => {
                    eprintln!("paneld: source `{name}`: read: {e}");
                    break;
                }
            };
            let Some(parsed) = parse_output(&line, &parse_mode) else {
                eprintln!("paneld: source `{name}`: cannot parse `{}` as {parse_mode}", mask.apply(&line));
                continue;
            };
            let parsed = mask.apply_value(parsed);
            // `value` can put masked pieces back together: mask it too.
            let value = mask.apply_value(finish_stream_value(&block, &ctx, parsed));
            if tx.send(SourceUpdate { name: name.clone(), value, status: false, started: Instant::now(), began: false }).is_err() {
                // Receiver dropped: daemon is going down.
                runner::kill_group(pid, runner::Signal::SIGTERM);
                let _ = child.wait();
                unregister_pgid(&pgids, pid);
                return;
            }
            sent_any = true;
        }

        // Child's stdout EOF (or shutdown-triggered SIGTERM from the
        // handle). Reap the child and drop its pgid from the
        // registry.
        if shutdown.load(Ordering::SeqCst) {
            runner::kill_group(pid, runner::Signal::SIGTERM);
        }
        let _ = child.wait();
        unregister_pgid(&pgids, pid);
        if shutdown.load(Ordering::SeqCst) {
            return;
        }

        if sent_any && started.elapsed() >= RESET_AFTER {
            delay = RESTART_FLOOR;
        }
        if tx.send(SourceUpdate { name: name.clone(), value: not_ok("the stream stopped; restarting"), status: true, started: Instant::now(), began: false }).is_err() {
            return;
        }
        eprintln!(
            "paneld: source `{name}`: stream exited after {:?}; restarting in {:?}",
            started.elapsed(),
            delay
        );
        sleep_or_shutdown(delay, &shutdown);
        delay = next_backoff(delay, RESTART_CEILING);
    }
}

/// ADR 0013: each stream line up to 64 KiB; longer lines are dropped
/// with a warning.
const STREAM_LINE_CAP: usize = 64 * 1024;

enum Line {
    Text(String),
    TooLong,
    Eof,
}

/// Read one `\n`-terminated line holding at most `cap` bytes. A
/// longer line is consumed through its newline and reported as
/// `TooLong`, so memory stays bounded whatever the child writes.
fn read_line_capped(r: &mut impl BufRead, cap: usize) -> std::io::Result<Line> {
    let mut buf: Vec<u8> = Vec::new();
    let mut too_long = false;
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            // EOF: a final unterminated line still counts.
            if buf.is_empty() && !too_long {
                return Ok(Line::Eof);
            }
            return Ok(finish_line(buf, too_long, cap));
        }
        let (chunk, done) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (&avail[..i], Some(i + 1)),
            None => (avail, None),
        };
        // One spare byte for a `\r` that turns out to end the line.
        if !too_long {
            if buf.len() + chunk.len() > cap + 1 {
                too_long = true;
                buf = Vec::new();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let used = done.unwrap_or(avail.len());
        r.consume(used);
        if done.is_some() {
            return Ok(finish_line(buf, too_long, cap));
        }
    }
}

/// Drop a `\r` before the line end, then apply the cap.
fn finish_line(mut buf: Vec<u8>, too_long: bool, cap: usize) -> Line {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if too_long || buf.len() > cap {
        Line::TooLong
    } else {
        Line::Text(String::from_utf8_lossy(&buf).into_owned())
    }
}

fn unregister_pgid(pgids: &Arc<Mutex<Vec<runner::Pid>>>, pid: runner::Pid) {
    let mut g = pgids.lock().unwrap();
    g.retain(|p| p.as_raw() != pid.as_raw());
}

fn sleep_or_shutdown(d: Duration, shutdown: &Arc<AtomicBool>) {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(100).min(deadline - Instant::now()));
    }
}

/// Build the public source Value for one streamed line, following
/// the same `value = {{ ... }}` + `ok = true` shape as [`resolve_one`].
/// `ctx` is unused for now: event-mode alerts see `event = <this
/// value>` on their side, and `value = ...` on a stream source
/// evaluates against `out` only (same as `resolve_one`).
fn finish_stream_value(b: &Block, _ctx: &Context, out: Value) -> Value {
    let value = if let Some((raw, span)) = find_raw(b, "value") {
        eval_value_transform(b, raw, span, &out).unwrap_or(out)
    } else {
        out
    };
    let mut m = BTreeMap::new();
    m.insert("value".to_string(), value);
    m.insert("ok".to_string(), Value::Bool(true));
    m.insert("error".to_string(), Value::String(String::new()));
    Value::Map(m)
}

/// ADR 0013: on failure, the next run waits `interval`, then
/// `2*interval`, then `4*interval`, up to `max(interval, 5m)`.
fn next_backoff(current: Duration, ceiling: Duration) -> Duration {
    std::cmp::min(current.saturating_mul(2), ceiling)
}

fn backoff_ceiling(interval: Duration) -> Duration {
    std::cmp::max(interval, Duration::from_secs(5 * 60))
}

/// Collect all top-level `source` blocks keyed by name, so the
/// action worker can look up a block by its name when applying a
/// `refresh = NAME` step without re-walking the document.
pub fn collect_blocks(doc: &Document) -> BTreeMap<String, Block> {
    let mut out = BTreeMap::new();
    for item in &doc.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "source" {
            continue;
        }
        if let Some(name) = &b.name {
            out.insert(name.text.clone(), b.clone());
        }
    }
    out
}

/// Resolve one `source` block. Returns the [`Value::Map`] stored
/// under its name in the sources map; `.value` is set so the
/// idiomatic `sources.X.value` works. `.ok` reflects the command's
/// exit code (missing today -> `false`).
pub fn resolve_one(
    b: &Block,
    ctx: &Context,
    base_cwd: &std::path::Path,
    app_defaults: runner::AppDefaults,
) -> Result<Value, String> {
    let parse_mode = find_simple(b, "parse").unwrap_or("text").trim().to_string();
    // Built before the params are bound, so no param can hide the
    // secrets from it.
    let mask = crate::secrets::Mask::from_ctx(ctx);
    // A remote member's params (schema 17) are plain names here.
    let mut bound = ctx.clone();
    crate::clock::insert(&mut bound);
    crate::remotes::bind(b, &mut bound);
    let ctx = &bound;
    let timeout = find_simple(b, "timeout")
        .and_then(paneld_config::value::parse_duration)
        .unwrap_or(app_defaults.timeout);

    // Interpolate secrets, theme tokens and other context values into
    // the command before executing: split first, then interpolate each
    // argv item, so a value with spaces stays one argument. A remote
    // member's command is built around it (ADR 0018).
    // A filled-in URL can hold a secret: mask what the error says.
    let request = crate::remotes::request(b, ctx, timeout, false)
        .map_err(|e| mask.apply(&e))?
        .ok_or_else(|| "missing `run`".to_string())?;
    let name = b.name.as_ref().map(|n| n.text.as_str()).unwrap_or("");
    // `env { KEY = value }` on the source becomes extra env pairs on
    // top of the clean base env (ADR 0013). Each value is
    // interpolated with the same ctx the run string uses.
    let env_pairs: Vec<(String, String)> = collect_env(b)
        .into_iter()
        .map(|(k, raw, span)| (k, paneld_ui::interpolate(&raw, span.file, span.start, ctx)))
        .collect();
    let cwd = resolve_cwd(b, base_cwd);
    let opts = runner::Opts {
        timeout,
        max_bytes: app_defaults.max_output,
        kind: runner::Kind::Source,
        name,
        cwd: Some(&cwd),
        env: &env_pairs,
        stdin: request.stdin.as_deref(),
    };
    // What the command prints becomes a value the screen shows: mask
    // secrets (see `secrets::Mask`). Parsed output is masked after
    // parsing, where JSON escapes are undone and numbers are left
    // alone; the raw `stdout` / `stderr` as text.
    let mut r = runner::exec(&request.argv, opts).map_err(|e| mask.apply(&format!("run: {e}")))?;
    let http_status = crate::remotes::take_http_status(b, r.exit_code, &mut r.stderr);
    if r.timed_out {
        return Err(if crate::remotes::is_remote(b) { "timed out; it may still have run".into() } else { "timed out".into() });
    }
    if r.too_large {
        return Err("output too large".into());
    }
    let ok = r.exit_code == Some(0);

    // A failed run's output (an HTTP error page, a usage message) need
    // not parse: it still gets its map, with `error`, `exit` and
    // `status`, and a null value.
    let out = match parse_output(&r.stdout, &parse_mode).map(|v| mask.apply_value(v)) {
        Some(v) => Some(v),
        None if !ok => None,
        None => return Err(format!("could not parse output as `{parse_mode}`")),
    };

    // If the source has `value = {{ ... }}`, evaluate it with `out`
    // in scope and use the result as the public value. Otherwise
    // `out` is the value itself.
    // `value` can put masked pieces back together: mask it too.
    let value = if let Some(out) = &out {
        transformed(b, out, &mask)
    } else {
        Value::Null
    };
    let mut m: BTreeMap<String, Value> = BTreeMap::new();
    m.insert("value".into(), value);
    m.insert("ok".into(), Value::Bool(ok));
    m.insert("exit".into(), r.exit_code.map(|n| Value::Int(n as i64)).unwrap_or(Value::Null));
    m.insert("stdout".into(), Value::String(mask.apply(&r.stdout)));
    m.insert("stderr".into(), Value::String(mask.apply(&r.stderr)));
    // Schema 4: `error` is always there, empty after a success.
    let error = if ok {
        String::new()
    } else {
        crate::remotes::explain(b, r.exit_code, &r.stderr).unwrap_or_else(|| match r.stderr.trim() {
            "" => format!("exit {}", r.exit_code.map_or("?".to_string(), |c| c.to_string())),
            e => e.to_string(),
        })
    };
    m.insert("error".into(), Value::String(mask.apply(&error)));
    if let Some(status) = http_status {
        m.insert("status".into(), Value::Int(status));
    }
    Ok(Value::Map(m))
}

fn parse_output(text: &str, mode: &str) -> Option<Value> {
    let t = text.trim();
    match mode {
        "text" => Some(Value::String(t.to_string())),
        "lines" => Some(Value::List(
            t.lines().map(|l| Value::String(l.to_string())).collect(),
        )),
        "number" => t.parse::<f64>().ok().map(Value::Double),
        "json" => parse_json(t),
        _ => None,
    }
}

/// Evaluate the `value = {{ ... }}` transform with `out` bound.
fn eval_value_transform(
    b: &Block,
    raw: &str,
    span: paneld_config::source::Span,
    out: &Value,
) -> Result<Value, String> {
    use paneld_config::cel;
    use paneld_config::value::{classify, ValueShape};

    // `out`, and a remote member's params (schema 17).
    let mut ctx: Context = Context::new();
    crate::clock::insert(&mut ctx);
    crate::remotes::bind(b, &mut ctx);
    ctx.insert("out".into(), out.clone());

    match classify(raw) {
        ValueShape::Literal(text) => Ok(Value::String(text.to_string())),
        ValueShape::Expression(body) => {
            let (expr, diags) = cel::parse_fragment(body, span.file, span.start + 2);
            if !diags.is_empty() {
                return Err(diags[0].message.clone());
            }
            let expr = expr.ok_or_else(|| "empty expression".to_string())?;
            paneld_ui::eval(&expr, &ctx).map_err(|e| e.message)
        }
        ValueShape::Interp(_) => {
            // Mixed-literal `value` on a source is unusual; stringify
            // the fragments.
            let (fragments, diags) = cel::parse_interp(raw, span.file, span.start);
            if !diags.is_empty() {
                return Err(diags[0].message.clone());
            }
            let joined = fragments
                .iter()
                .map(|e| {
                    paneld_ui::eval(e, &ctx)
                        .map(|v| v.to_display_string())
                        .unwrap_or_else(|e| format!("[{}]", e.message))
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(Value::String(joined))
        }
    }
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

fn find_raw<'a>(
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

/// Collect every `env { KEY = value }` entry on this block. Returned
/// tuples carry the key, the raw value text and its span so the
/// caller can interpolate against its own ctx.
fn collect_env(
    node: &Block,
) -> Vec<(String, String, paneld_config::source::Span)> {
    let mut out = Vec::new();
    for item in &node.items {
        let Item::Block(b) = item else { continue };
        if b.kind.text != "env" {
            continue;
        }
        for inner in &b.items {
            let Item::Assign(a) = inner else { continue };
            let AssignTarget::Simple { key } = &a.target else {
                continue;
            };
            out.push((key.text.clone(), a.value.text.clone(), a.value.span));
        }
    }
    out
}

// -- JSON parse ----------------------------------------------------------

/// Minimal JSON reader for `parse = json` sources. Enough to turn a
/// typical `jq` or REST payload into [`Value::Map`] / [`Value::List`]
/// trees. No streaming, no error recovery; a malformed input returns
/// `None` and the source is treated as failed.
/// Deepest nesting `parse_json` accepts. The parser recurses, and ctl
/// clients and network APIs are not trusted to keep their documents
/// shallow: a stack overflow would abort the whole daemon.
const JSON_MAX_DEPTH: usize = 64;

pub fn parse_json(s: &str) -> Option<Value> {
    let mut p = JsonParser { bytes: s.as_bytes(), pos: 0, depth: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return None;
    }
    Some(v)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            b'{' | b'[' if self.depth >= JSON_MAX_DEPTH => None,
            b'{' => {
                self.depth += 1;
                let v = self.parse_object();
                self.depth -= 1;
                v
            }
            b'[' => {
                self.depth += 1;
                let v = self.parse_array();
                self.depth -= 1;
                v
            }
            b'"' => self.parse_string().map(Value::String),
            b't' | b'f' => self.parse_bool(),
            b'n' => self.parse_null(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_object(&mut self) -> Option<Value> {
        self.bump()?; // '{'
        let mut m: BTreeMap<String, Value> = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Some(Value::Map(m));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bump()? != b':' {
                return None;
            }
            let v = self.parse_value()?;
            m.insert(key, v);
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b'}' => return Some(Value::Map(m)),
                _ => return None,
            }
        }
    }

    fn parse_array(&mut self) -> Option<Value> {
        self.bump()?; // '['
        let mut items: Vec<Value> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Some(Value::List(items));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b']' => return Some(Value::List(items)),
                _ => return None,
            }
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if self.bump()? != b'"' {
            return None;
        }
        // Bytes, decoded as UTF-8 at the end: text is not one char per
        // byte.
        let mut out: Vec<u8> = Vec::new();
        let push = |out: &mut Vec<u8>, c: char| out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        loop {
            match self.bump()? {
                b'"' => return Some(String::from_utf8_lossy(&out).into_owned()),
                b'\\' => match self.bump()? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'u' => {
                        let hi = self.hex4()?;
                        if (0xd800..0xdc00).contains(&hi) {
                            // A surrogate pair: `😀`. Only a
                            // low half is taken with it; anything else
                            // is read on its own next.
                            let back = self.pos;
                            let lo = if self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u") {
                                self.pos += 2;
                                match self.hex4() {
                                    Some(lo) if (0xdc00..0xe000).contains(&lo) => Some(lo),
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            match lo {
                                Some(lo) => push(&mut out, char::from_u32(0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)).unwrap_or('\u{fffd}')),
                                None => {
                                    self.pos = back;
                                    push(&mut out, '\u{fffd}');
                                }
                            }
                        } else {
                            push(&mut out, char::from_u32(hi).unwrap_or('\u{fffd}'));
                        }
                    }
                    _ => return None,
                },
                b => out.push(b),
            }
        }
    }

    /// Four hex digits after `\u`.
    fn hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.pos..self.pos + 4)?;
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let n = u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
        self.pos += 4;
        Some(n)
    }

    fn parse_bool(&mut self) -> Option<Value> {
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"true") {
            self.pos += 4;
            Some(Value::Bool(true))
        } else if self.bytes.get(self.pos..self.pos + 5) == Some(b"false") {
            self.pos += 5;
            Some(Value::Bool(false))
        } else {
            None
        }
    }

    fn parse_null(&mut self) -> Option<Value> {
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"null") {
            self.pos += 4;
            Some(Value::Null)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Value> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_double = false;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            is_double = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_double = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        if is_double {
            text.parse::<f64>().ok().map(Value::Double)
        } else {
            // Past i64 (a long id): a number, not a failed document.
            text.parse::<i64>().ok().map(Value::Int).or_else(|| text.parse::<f64>().ok().map(Value::Double))
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn running_and_age_follow_the_runs_and_the_clock() {
        let mut m = BTreeMap::from([
            ("a".to_string(), Value::Map(BTreeMap::from([("at".to_string(), Value::Int(1000))]))),
            ("b".to_string(), not_ok("not run yet")),
        ]);
        let field = |m: &BTreeMap<String, Value>, s: &str, f: &str| match &m[s] {
            Value::Map(x) => x.get(f).cloned(),
            _ => None,
        };
        let run = Run::start("a", None);
        let overlapping = Run::start("a", None);
        add_live_fields(&mut m, 1090, &running_names());
        assert_eq!(field(&m, "a", "running"), Some(Value::Bool(true)));
        assert_eq!(field(&m, "a", "age"), Some(Value::Int(90)));
        assert_eq!(field(&m, "b", "running"), Some(Value::Bool(false)));
        assert_eq!(field(&m, "b", "age"), None, "no result, no age");
        drop(run);
        add_live_fields(&mut m, 1100, &running_names());
        assert_eq!(field(&m, "a", "running"), Some(Value::Bool(true)), "a refresh still runs");
        drop(overlapping);
        add_live_fields(&mut m, 1100, &running_names());
        assert_eq!(field(&m, "a", "running"), Some(Value::Bool(false)));
        assert_eq!(field(&m, "a", "age"), Some(Value::Int(100)));
    }
    use super::*;

    fn parse_doc(src: &str) -> paneld_config::parse::Document {
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
    fn a_failed_run_keeps_its_map_when_its_output_does_not_parse() {
        let doc = parse_doc("source s { run = /bin/sh -c 'echo \"<html>busy</html>\"; exit 3'\n parse = json }\nsource t { run = /bin/echo nope\n parse = json }\n");
        let mut blocks = collect_blocks(&doc);
        let v = resolve_one(&blocks.remove("s").unwrap(), &Context::new(), std::path::Path::new("/"), runner::AppDefaults::default()).unwrap();
        let Value::Map(m) = v else { panic!() };
        assert_eq!(m["ok"], Value::Bool(false));
        assert_eq!(m["value"], Value::Null);
        assert_eq!(m["exit"], Value::Int(3));
        assert_eq!(m["error"], Value::String("exit 3".into()));
        // A run that worked but printed something else is still an error.
        assert!(resolve_one(&blocks.remove("t").unwrap(), &Context::new(), std::path::Path::new("/"), runner::AppDefaults::default()).is_err());
    }

    #[test]
    fn a_stream_that_cannot_start_says_so() {
        let doc = parse_doc("source s { run = /nonexistent/bin\n stream = true }\n");
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = spawn_pollers(&doc, &Context::new(), tx, std::path::Path::new("."), runner::AppDefaults::default());
        let u = rx.recv_timeout(Duration::from_secs(5)).expect("a status");
        handle.stop();
        assert!(u.status);
        let Value::Map(m) = &u.value else { panic!() };
        assert_eq!(m["ok"], Value::Bool(false));
        assert!(matches!(&m["error"], Value::String(e) if e.starts_with("spawn:")), "{m:?}");
    }

    #[test]
    fn sources_without_a_result_are_not_ok() {
        let doc = parse_doc(
            "source good { run = /bin/echo 1 }\nsource gone { run = /nonexistent/x }\nsource s { run = /bin/cat\n stream = true }\n",
        );
        let mut values = resolve_all(&doc, &Context::new(), std::path::Path::new("/"), runner::AppDefaults::default());
        fill_not_run(&doc, &mut values);
        let field = |n: &str, k: &str| match &values[n] {
            Value::Map(m) => m.get(k).cloned(),
            _ => panic!(),
        };
        assert_eq!(field("good", "ok"), Some(Value::Bool(true)));
        assert_eq!(field("gone", "ok"), Some(Value::Bool(false)));
        assert!(matches!(field("gone", "error"), Some(Value::String(e)) if e.starts_with("run:")));
        assert_eq!(field("s", "ok"), Some(Value::Bool(false)));
        assert_eq!(field("s", "error"), Some(Value::String("not run yet".into())));
        assert_eq!(field("s", "value"), Some(Value::Null));
    }

    #[test]
    fn secrets_are_masked_after_parsing() {
        // Review round 7: the raw text held `a\/b-tok` (JSON escape),
        // so masking it missed the secret, and masking `1234` inside a
        // number broke the JSON.
        let doc = parse_doc(
            "source s { run = /bin/echo '{\"user\": \"a\\/b-tok\", \"ts\": 1762341234}'\n parse = json }\n\
             source j { run = /bin/echo '{\"a\": \"98765\", \"b\": \"432\"}'\n parse = json\n value = {{ out.a + out.b }} }\n",
        );
        let mut blocks = collect_blocks(&doc);
        let block = blocks.remove("s").unwrap();
        let mut ctx = Context::new();
        let secrets = BTreeMap::from([
            ("t".to_string(), Value::String("a/b-tok".into())),
            ("p".to_string(), Value::String("1234".into())),
        ]);
        ctx.insert("secrets".into(), Value::Map(secrets));
        let v = resolve_one(&block, &ctx, std::path::Path::new("/"), runner::AppDefaults::default()).unwrap();
        let Value::Map(m) = v else { panic!() };
        let Value::Map(value) = &m["value"] else { panic!("{:?}", m["value"]) };
        assert_eq!(value["user"], Value::String("****".into()));
        assert_eq!(value["ts"], Value::String("****".into()), "a number holding the secret's digits");
        // Pieces joined back by `value` are masked again.
        let mut ctx2 = Context::new();
        ctx2.insert("secrets".into(), Value::Map(BTreeMap::from([("t".to_string(), Value::String("98765432".into()))])));
        let j = resolve_one(&blocks.remove("j").unwrap(), &ctx2, std::path::Path::new("/"), runner::AppDefaults::default()).unwrap();
        let Value::Map(m) = j else { panic!() };
        assert_eq!(m["value"], Value::String("****".into()));
    }

    #[test]
    fn json_nesting_is_bounded() {
        let ok = format!("{}1{}", "[".repeat(64), "]".repeat(64));
        assert!(parse_json(&ok).is_some());
        let deep = format!("{}1{}", "[".repeat(65), "]".repeat(65));
        assert_eq!(parse_json(&deep), None);
        // Would overflow the stack without the limit.
        assert_eq!(parse_json(&"[".repeat(200_000)), None);
        assert_eq!(parse_json(&"{\"a\":".repeat(200_000)), None);
    }

    #[test]
    fn stream_source_emits_one_update_per_line_and_restarts() {
        // Child prints two lines, exits. The stream worker emits two
        // SourceUpdates, then restarts the child; since restart floor
        // is 1s we only need to assert the first two came through.
        use std::sync::mpsc::channel;
        let src = concat!(
            "source lines {\n",
            "  run = /bin/sh -c \"printf 'a\\nb\\n'\"\n",
            "  stream = true\n",
            "  parse = text\n",
            "}\n",
        );
        let doc = parse_doc(src);
        let (tx, rx) = channel();
        let handle = spawn_pollers(&doc, &Context::new(), tx, std::path::Path::new("."), runner::AppDefaults::default());

        // The first update says the stream is up (status only).
        let up = rx.recv_timeout(Duration::from_secs(5)).expect("up");
        assert!(up.status && matches!(&up.value, Value::Map(m) if m["ok"] == Value::Bool(true)));
        let mut got = Vec::new();
        while got.len() < 2 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(u) if u.status => {}
                Ok(u) => got.push(u),
                Err(e) => panic!("stream update timed out: {e}"),
            }
        }
        handle.stop();

        assert_eq!(got.len(), 2);
        for (u, expected) in got.iter().zip(["a", "b"].iter()) {
            assert_eq!(u.name, "lines");
            let Value::Map(ref m) = u.value else {
                panic!("expected map, got {:?}", u.value);
            };
            assert_eq!(m["value"], Value::String((*expected).into()));
            assert_eq!(m["ok"], Value::Bool(true));
        }
    }

    #[test]
    fn stream_source_json_parses_each_line_as_json() {
        use std::sync::mpsc::channel;
        let src = concat!(
            "source events {\n",
            "  run = /bin/sh -c \"printf '{\\\"title\\\":\\\"a\\\"}\\n{\\\"title\\\":\\\"b\\\"}\\n'\"\n",
            "  stream = true\n",
            "  parse = json\n",
            "}\n",
        );
        let doc = parse_doc(src);
        let (tx, rx) = channel();
        let handle = spawn_pollers(&doc, &Context::new(), tx, std::path::Path::new("."), runner::AppDefaults::default());

        let next = || loop {
            let u = rx.recv_timeout(Duration::from_secs(5)).expect("a line");
            if !u.status {
                return u;
            }
        };
        let u1 = next();
        let u2 = next();
        handle.stop();

        let title = |u: &SourceUpdate| -> String {
            let Value::Map(ref m) = u.value else { panic!() };
            let Value::Map(ref vm) = m["value"] else { panic!() };
            let Value::String(ref s) = vm["title"] else { panic!() };
            s.clone()
        };
        assert_eq!(title(&u1), "a");
        assert_eq!(title(&u2), "b");
    }

    fn lines(input: &[u8], cap: usize) -> Vec<String> {
        let mut r = std::io::BufReader::with_capacity(7, input);
        let mut out = Vec::new();
        loop {
            match read_line_capped(&mut r, cap).unwrap() {
                Line::Eof => return out,
                Line::TooLong => out.push("<too long>".into()),
                Line::Text(t) => out.push(t),
            }
        }
    }

    #[test]
    fn stream_lines_are_capped_and_long_ones_dropped() {
        let long = "x".repeat(40);
        let input = format!("a\r\n{long}\nbb\nlast");
        assert_eq!(
            lines(input.as_bytes(), 16),
            vec!["a", "<too long>", "bb", "last"]
        );
        // Exactly at the cap is fine, also with CRLF; a final
        // unterminated line loses its `\r` too.
        assert_eq!(lines(b"0123456789abcdef\n", 16), vec!["0123456789abcdef"]);
        assert_eq!(lines(b"0123456789abcdef\r\n", 16), vec!["0123456789abcdef"]);
        assert_eq!(lines(b"0123456789abcdefg\r\n", 16), vec!["<too long>"]);
        assert_eq!(lines(b"end\r", 16), vec!["end"]);
        assert!(lines(b"", 16).is_empty());
    }

    #[test]
    fn stream_shutdown_wakes_a_quiet_child() {
        // Reviewer round 3 blocker: a stream child that goes silent
        // (sleep after one line) must be killed by ShutdownHandle::
        // stop, not just left blocked in BufReader::lines().
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use std::sync::mpsc::channel;
        let src = concat!(
            "source quiet {\n",
            "  run = /bin/sh -c \"printf 'first\\n'; sleep 300\"\n",
            "  stream = true\n",
            "  parse = text\n",
            "}\n",
        );
        let doc = parse_doc(src);
        let (tx, rx) = channel();
        let handle = spawn_pollers(&doc, &Context::new(), tx, std::path::Path::new("."), runner::AppDefaults::default());

        // First line arrives, confirming the child is up.
        let first = loop {
            let u = rx.recv_timeout(Duration::from_secs(5)).expect("first line");
            if !u.status {
                break u;
            }
        };
        assert_eq!(first.name, "quiet");

        // Capture the live pgid for a liveness check post-stop.
        let pgid = {
            let g = handle.stream_pgids.lock().unwrap();
            assert_eq!(g.len(), 1, "one stream pgid should be registered");
            g[0]
        };

        // Trigger shutdown. The child was in `sleep 300`; stop must
        // SIGTERM its group, the reader unblocks, the loop exits.
        handle.stop();

        let gone = (0..40).any(|_| {
            thread::sleep(Duration::from_millis(50));
            matches!(kill(pgid, None), Err(Errno::ESRCH))
        });
        assert!(gone, "stream child pgid {pgid} outlived stop()");
    }

    #[test]
    fn poll_backoff_doubles_until_ceiling() {
        let interval = Duration::from_secs(5);
        let ceiling = backoff_ceiling(interval);
        assert_eq!(ceiling, Duration::from_secs(300));
        let mut d = interval;
        d = next_backoff(d, ceiling);
        assert_eq!(d, Duration::from_secs(10));
        d = next_backoff(d, ceiling);
        assert_eq!(d, Duration::from_secs(20));
        for _ in 0..20 {
            d = next_backoff(d, ceiling);
        }
        assert_eq!(d, ceiling);
    }

    #[test]
    fn poll_backoff_ceiling_is_at_least_interval() {
        // An interval of 10m is already its own ceiling (the schema
        // says `max(interval, 5m)`).
        let interval = Duration::from_secs(600);
        assert_eq!(backoff_ceiling(interval), interval);
    }

    #[test]
    fn parse_output_number() {
        assert_eq!(parse_output("42", "number"), Some(Value::Double(42.0)));
        assert_eq!(parse_output("  2.5\n", "number"), Some(Value::Double(2.5)));
        assert_eq!(parse_output("abc", "number"), None);
    }

    #[test]
    fn parse_output_lines_splits_each_line() {
        assert_eq!(
            parse_output("one\ntwo\n", "lines"),
            Some(Value::List(vec![
                Value::String("one".into()),
                Value::String("two".into()),
            ]))
        );
    }

    #[test]
    fn parse_output_json_object() {
        let v = parse_output(r#"{"a": 1, "b": "two"}"#, "json").unwrap();
        if let Value::Map(m) = v {
            assert_eq!(m.get("a"), Some(&Value::Int(1)));
            assert_eq!(m.get("b"), Some(&Value::String("two".into())));
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn parse_output_json_array_of_objects() {
        let v = parse_output(r#"[{"name": "x"}, {"name": "y"}]"#, "json").unwrap();
        if let Value::List(items) = v {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected list");
        }
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn text_survives_whole() {
        let v = parse_json(r#"{"a": "été é日 😀 \b\f"}"#);
        let Some(Value::Map(m)) = v else { panic!("{v:?}") };
        assert_eq!(m["a"], Value::String("été é日 😀 \u{8}\u{c}".into()));
        // A lone surrogate becomes the replacement character.
        assert_eq!(parse_json(r#""\ud800x""#), Some(Value::String("\u{fffd}x".into())));
        assert_eq!(parse_json(r#""\ud800A""#), Some(Value::String("\u{fffd}A".into())));
        // A lone high half, then a whole pair: the pair survives.
        assert_eq!(parse_json(r#""\ud800😀""#), Some(Value::String("\u{fffd}😀".into())));
        assert_eq!(parse_json(r#""\udc00x""#), Some(Value::String("\u{fffd}x".into())));
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_json("-12"), Some(Value::Int(-12)));
        assert_eq!(parse_json("1.5e3"), Some(Value::Double(1500.0)));
        assert_eq!(parse_json("2E-2"), Some(Value::Double(0.02)));
        assert_eq!(parse_json("1e+2"), Some(Value::Double(100.0)));
        assert_eq!(parse_json("12345678901234567890123"), Some(Value::Double(1.2345678901234568e22)));
        assert_eq!(parse_json("-"), None);
    }

    #[test]
    fn structure() {
        assert_eq!(parse_json(" [ ] "), Some(Value::List(vec![])));
        assert_eq!(parse_json("{ }"), Some(Value::Map(BTreeMap::new())));
        assert_eq!(
            parse_json(r#"{"a": [1, {"b": null}], "c": true, "d": false}"#),
            Some(Value::Map(BTreeMap::from([
                ("a".into(), Value::List(vec![Value::Int(1), Value::Map(BTreeMap::from([("b".into(), Value::Null)]))])),
                ("c".into(), Value::Bool(true)),
                ("d".into(), Value::Bool(false)),
            ])))
        );
        for bad in [
            "", "{", "[1,", "[1 2]", r#"{"a" 1}"#, r#"{"a": 1,}"#, "{a: 1}", "[1] x", "tru", "nul",
            r#""\x""#, r#""\u12""#, r#""\u+041""#, r#""open"#, "@",
        ] {
            assert_eq!(parse_json(bad), None, "accepted `{bad}`");
        }
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = |n: usize| "[".repeat(n) + &"]".repeat(n);
        assert!(parse_json(&deep(JSON_MAX_DEPTH)).is_some());
        assert_eq!(parse_json(&deep(JSON_MAX_DEPTH + 1)), None);
        assert_eq!(parse_json(&deep(100_000)), None, "no stack overflow");
    }
}
