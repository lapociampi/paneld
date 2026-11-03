//! Command runner (ADR 0013). Executes commands argv-mode, with a
//! clean environment and the child in its own process group so a
//! timeout kills the command and anything it spawned. The input is
//! always interpreted as an argv list -- the raw text is tokenised
//! with POSIX-style single and double quotes so secrets and other
//! interpolated values reach the command as argv items, never as
//! shell syntax. `app:max_running` (a global cap on concurrent
//! commands) is not enforced yet.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub use nix::sys::signal::Signal;
use nix::sys::signal::killpg;
pub use nix::unistd::Pid;

#[derive(Debug)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// stdout or stderr went past its cap; the command was killed and
    /// the run counts as failed (`output too large`, ADR 0013).
    pub too_large: bool,
}

impl RunResult {
    /// Exit 0, not timed out, output within limits.
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && !self.too_large
    }
}

/// ADR 0013: stderr is kept up to 64 KiB.
pub const MAX_STDERR: usize = 64 * 1024;

/// Read `r` to EOF keeping at most `cap` bytes. Reading past `cap`
/// raises `too_large`; the bytes beyond it are dropped but the pipe is
/// still drained so the child can never block on it.
fn read_capped(mut r: impl Read, cap: usize, too_large: &AtomicBool) -> String {
    let mut buf = Vec::with_capacity(cap.min(4096));
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len());
                if n > room {
                    buf.extend_from_slice(&chunk[..room]);
                    too_large.store(true, Ordering::SeqCst);
                } else {
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[derive(Debug)]
pub struct RunError {
    pub message: String,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Daemon-wide command defaults read from the top-level `app` block
/// (ADR 0013). Per-source / per-action keys override these for one
/// command.
#[derive(Debug, Clone, Copy)]
pub struct AppDefaults {
    pub timeout: Duration,
    pub max_output: usize,
}

impl Default for AppDefaults {
    fn default() -> Self {
        // ADR 0013 defaults.
        Self {
            timeout: Duration::from_secs(10),
            max_output: 1024 * 1024,
        }
    }
}

impl AppDefaults {
    /// Read the ADR 0013 app defaults from `doc`. Accepts both the
    /// `app:timeout = ...` qualified form and the `app { timeout =
    /// ... }` block form. Missing keys fall back to [`Default`].
    pub fn from_doc(doc: &paneld_config::parse::Document) -> Self {
        use paneld_config::parse::{AssignTarget, Item};
        let mut out = Self::default();
        for item in &doc.items {
            match item {
                Item::Assign(a) => {
                    if let AssignTarget::Qualified { category, key } = &a.target {
                        if category.text == "app" {
                            apply(&mut out, key.text.as_str(), &a.value.text);
                        }
                    }
                }
                Item::Block(b) if b.kind.text == "app" => {
                    for inner in &b.items {
                        if let Item::Assign(a) = inner {
                            if let AssignTarget::Simple { key } = &a.target {
                                apply(&mut out, key.text.as_str(), &a.value.text);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

fn apply(out: &mut AppDefaults, key: &str, raw: &str) {
    match key {
        "timeout" => {
            if let Some(d) = paneld_config::value::parse_duration(raw) {
                out.timeout = d;
            }
        }
        "max_output" => {
            if let Some(n) = paneld_config::value::parse_bytes(raw) {
                out.max_output = n as usize;
            }
        }
        _ => {}
    }
}

/// What the command is run as (source `foo` vs action `bar`). Used
/// to set `PANELD_SOURCE` / `PANELD_ACTION` and nothing else.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Source,
    Action,
    /// Neither (the screen's `on` / `off` / `dim`).
    Other,
}

/// Options for a run. Everything in here is derived from the config
/// (or the daemon's own defaults). The runner itself never looks at
/// the process environment it was started in.
#[derive(Debug, Clone)]
pub struct Opts<'a> {
    pub timeout: Duration,
    pub max_bytes: usize,
    pub kind: Kind,
    /// Name of the invoking source or action, exposed as the matching
    /// `PANELD_SOURCE` / `PANELD_ACTION` env var.
    pub name: &'a str,
    /// Working directory. ADR 0013 uses the main config file's
    /// directory by default; the caller passes it in. `None` leaves
    /// the child in the daemon's cwd (back-compat for tests).
    pub cwd: Option<&'a Path>,
    /// Extra environment pairs from the source or action's `env { }`
    /// block. The clean base env (PATH, HOME, LANG, PANELD) is still
    /// applied first and these can override it.
    pub env: &'a [(String, String)],
}


/// Tokenise a command template into an argv list using POSIX-ish
/// single/double quote rules: single quotes are literal, double quotes
/// allow `\` escaping of `"`, `\\` and `$`, and whitespace outside
/// quotes splits tokens. No variable expansion, no globbing, no
/// backticks. A `{{ ... }}` expression is copied into its token whole,
/// even across spaces or quotes, so it can be interpolated per argv
/// item afterwards (see [`command_argv`]).
pub fn tokenize(cmd: &str) -> Result<Vec<String>, RunError> {
    let err = |m: &str| RunError { message: m.into() };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut i = 0;
    // Copy the `{{ ... }}` at `i` whole (the interpolator finds its
    // end the same way) and return the index after it.
    let take_expr = |i: usize, cur: &mut String| -> Result<usize, RunError> {
        let end = paneld_config::cel::expr_end(cmd, i).ok_or_else(|| err("unterminated `{{`"))?;
        cur.push_str(&cmd[i..end]);
        Ok(end)
    };
    while let Some(c) = cmd[i..].chars().next() {
        let rest = &cmd[i..];
        if rest.starts_with("{{") {
            in_tok = true;
            i = take_expr(i, &mut cur)?;
            continue;
        }
        i += c.len_utf8();
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if in_tok {
                    out.push(std::mem::take(&mut cur));
                    in_tok = false;
                }
            }
            '\'' | '"' => {
                in_tok = true;
                loop {
                    let Some(c2) = cmd[i..].chars().next() else {
                        return Err(err(if c == '"' { "unterminated double quote" } else { "unterminated single quote" }));
                    };
                    if cmd[i..].starts_with("{{") {
                        i = take_expr(i, &mut cur)?;
                        continue;
                    }
                    i += c2.len_utf8();
                    if c2 == c {
                        break;
                    }
                    // In double quotes `\` escapes `"`, `\`, `$`, a
                    // backtick and a newline; anything else stays.
                    if c == '"' && c2 == '\\' {
                        if let Some(next) = cmd[i..].chars().next().filter(|n| matches!(n, '"' | '\\' | '$' | '`' | '\n')) {
                            cur.push(next);
                            i += next.len_utf8();
                            continue;
                        }
                    }
                    cur.push(c2);
                }
            }
            '\\' => {
                in_tok = true;
                if cmd[i..].starts_with("{{") {
                    // `\{{` stays as is: the interpolator turns it into
                    // a literal `{{`.
                    cur.push_str("\\{{");
                    i += 2;
                    continue;
                }
                // Backslash escapes the next character outside quotes.
                // A trailing bare backslash is kept as a literal, same
                // as most POSIX shells.
                match cmd[i..].chars().next() {
                    Some(next) => {
                        cur.push(next);
                        i += next.len_utf8();
                    }
                    None => cur.push('\\'),
                }
            }
            c => {
                in_tok = true;
                cur.push(c);
            }
        }
    }
    if in_tok {
        out.push(cur);
    }
    if out.is_empty() {
        return Err(err("empty command"));
    }
    Ok(out)
}

/// Split a `run` template into argv and interpolate each item on its
/// own with `interp`, so a value with spaces or quotes stays one argv
/// item (the "inputs reach commands as argv items" rule).
pub fn command_argv(
    template: &str,
    interp: impl Fn(&str) -> String,
) -> Result<Vec<String>, RunError> {
    let argv: Vec<String> = tokenize(template)?
        .into_iter()
        .map(|t| if t.contains("{{") { interp(&t) } else { t })
        .collect();
    if argv[0].is_empty() {
        return Err(RunError { message: "the command name is empty".into() });
    }
    Ok(argv)
}

/// Run `argv`. The child gets a clean environment, its own process
/// group, stdin from `/dev/null`, stdout and stderr captured up to
/// their caps, and is killed as a group once `opts.timeout` elapses.
pub fn exec(argv: &[String], opts: Opts<'_>) -> Result<RunResult, RunError> {
    let program = &argv[0];
    let args = &argv[1..];
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(clean_env(&opts))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Put the child in its own process group so a timeout can
        // SIGTERM / SIGKILL the whole group (ADR 0013): everything the
        // command itself spawned dies with it. 0 makes the child pid
        // the group id.
        .process_group(0);
    if let Some(cwd) = opts.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().map_err(|e| RunError {
        message: format!("spawn `{program}`: {e}"),
    })?;
    // `.process_group(0)` made the child its own group leader, so its
    // pid is also the group id for `killpg`.
    let pgid = Pid::from_raw(child.id() as i32);

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // ADR 0013: stdout up to `max_output`, stderr up to 64 KiB; more
    // fails the run. The readers only raise the flag; the wait loop
    // below does the kill, while the child is still unreaped.
    let too_large = Arc::new(AtomicBool::new(false));
    let out_handle = {
        let flag = too_large.clone();
        let cap = opts.max_bytes;
        thread::spawn(move || read_capped(stdout, cap, &flag))
    };
    let err_handle = {
        let flag = too_large.clone();
        let cap = opts.max_bytes.min(MAX_STDERR);
        thread::spawn(move || read_capped(stderr, cap, &flag))
    };

    let start = Instant::now();
    let (exit_code, timed_out) = loop {
        if too_large.load(Ordering::SeqCst) && matches!(child.try_wait(), Ok(None)) {
            let _ = killpg(pgid, Signal::SIGKILL);
            let _ = child.wait();
            break (None, false);
        }
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) => {
                if start.elapsed() >= opts.timeout {
                    let _ = killpg(pgid, Signal::SIGTERM);
                    let hard = Instant::now();
                    while hard.elapsed() < Duration::from_secs(2) {
                        if let Ok(Some(_)) = child.try_wait() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                    }
                    if matches!(child.try_wait(), Ok(None)) {
                        let _ = killpg(pgid, Signal::SIGKILL);
                    }
                    let _ = child.wait();
                    break (None, true);
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(RunError {
                    message: format!("waiting on child: {e}"),
                });
            }
        }
    };

    let stdout_s = out_handle.join().unwrap_or_default();
    let stderr_s = err_handle.join().unwrap_or_default();

    Ok(RunResult {
        exit_code,
        stdout: stdout_s,
        stderr: stderr_s,
        timed_out,
        too_large: too_large.load(Ordering::SeqCst),
    })
}

/// Send `sig` to the group whose leader is `pid`. The caller is
/// responsible for having spawned the child with `process_group(0)`.
/// Errors (including `ESRCH` for an already-gone group) are
/// swallowed, matching the one-shot `exec` path.
pub fn kill_group(pid: Pid, sig: Signal) {
    let _ = killpg(pid, sig);
}

/// Build a [`Command`] primed with the ADR 0013 base environment
/// (clean env, PATH, HOME, LANG, PANELD, PANELD_SOURCE / _ACTION),
/// stdin from /dev/null, stdout and stderr piped, and the child in
/// its own process group. The caller spawns, drains output, and
/// kills on their own terms -- used by [`exec`] for one-shots and by
/// `sources.rs` for long-lived stream children.
pub fn prepared_command(argv: &[String], opts: &Opts<'_>) -> Result<Command, RunError> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(clean_env(opts))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(cwd) = opts.cwd {
        command.current_dir(cwd);
    }
    Ok(command)
}

fn clean_env(opts: &Opts<'_>) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    env.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
    env.push(("LANG".into(), "C.UTF-8".into()));
    env.push(("PANELD".into(), "1".into()));
    if let Ok(h) = std::env::var("HOME") {
        env.push(("HOME".into(), h.into()));
    }
    match opts.kind {
        Kind::Source if !opts.name.is_empty() => {
            env.push(("PANELD_SOURCE".into(), opts.name.into()));
        }
        Kind::Action if !opts.name.is_empty() => {
            env.push(("PANELD_ACTION".into(), opts.name.into()));
        }
        _ => {}
    }
    for (k, v) in opts.env {
        env.push((k.clone().into(), v.clone().into()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokenize a literal command (no `{{ }}`) and run it.
    fn exec(cmd: &str, o: Opts<'_>) -> Result<RunResult, RunError> {
        super::exec(&tokenize(cmd)?, o)
    }

    #[test]
    fn expressions_stay_one_token_and_interpolate_per_item() {
        assert_eq!(
            tokenize("curl -H 'X: {{ a }}' {{ b == 'x y' ? 1 : 2 }} --n={{ c }}").unwrap(),
            vec!["curl", "-H", "X: {{ a }}", "{{ b == 'x y' ? 1 : 2 }}", "--n={{ c }}"]
        );
        // A value with spaces and quotes is one argv item.
        let argv = command_argv("/bin/echo {{ name }} --tag={{ name }}", |t| {
            t.replace("{{ name }}", "web 1; rm -rf /")
        })
        .unwrap();
        assert_eq!(argv, vec!["/bin/echo", "web 1; rm -rf /", "--tag=web 1; rm -rf /"]);
        assert!(command_argv("{{ x }} a", |_| String::new()).is_err());
    }

    fn opts(timeout_ms: u64, max_bytes: usize) -> Opts<'static> {
        Opts {
            timeout: Duration::from_millis(timeout_ms),
            max_bytes,
            kind: Kind::Other,
            name: "",
            cwd: None,
            env: &[],
        }
    }

    #[test]
    fn tokenize_cuts_expressions_where_the_interpolator_does() {
        // Review round 6: a `}}` inside a string or a nested map ended
        // the expression early.
        assert_eq!(tokenize(r#"echo {{ x == "}}" ? 1 : 2 }}"#).unwrap(), ["echo", r#"{{ x == "}}" ? 1 : 2 }}"#]);
        assert_eq!(tokenize("echo {{ {'a': {'b': 1}} }}").unwrap(), ["echo", "{{ {'a': {'b': 1}} }}"]);
        assert_eq!(tokenize("echo {{ a").unwrap_err().message, "unterminated `{{`");
        assert_eq!(tokenize(r"echo \{{ a }}").unwrap(), ["echo", r"\{{", "a", "}}"]);
        assert_eq!(tokenize("echo caf\u{e9}{{ x }}\u{65e5}").unwrap(), ["echo", "caf\u{e9}{{ x }}\u{65e5}"]);
    }

    #[test]
    fn echo_captures_stdout() {
        let r = exec("/bin/echo hello", opts(2000, 4096)).unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stdout.trim(), "hello");
        assert!(!r.timed_out);
    }

    #[test]
    fn nonzero_exit_is_surfaced() {
        let r = exec("/bin/sh -c 'exit 7'", opts(2000, 4096)).unwrap();
        assert_eq!(r.exit_code, Some(7));
    }

    #[test]
    fn timeout_fires_and_kills_group() {
        // Prove ADR 0013: the group kill must reach descendants, not
        // just the direct child. Shell forks a `sleep 30`, writes
        // the sleep's pid to a tmpfile, then waits on it. Once the
        // runner times out it kills the whole group. After `exec`
        // returns we confirm the grandchild is gone with kill(_, 0).
        use nix::errno::Errno;
        use nix::sys::signal::kill;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let pid_file = std::env::temp_dir().join(format!(
            "paneld-group-kill-{}-{nanos}.pid",
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&pid_file);
        let cmd = format!(
            "/bin/sh -c 'sleep 30 & echo $! > {}; wait'",
            pid_file.display()
        );
        // 500ms timeout gives the shell time to spawn and record.
        let r = exec(&cmd, opts(500, 4096)).unwrap();
        assert!(
            r.timed_out,
            "parent exited cleanly? exit_code={:?} stdout={:?} stderr={:?}",
            r.exit_code, r.stdout, r.stderr
        );

        let pid_str = std::fs::read_to_string(&pid_file).expect("pid file written");
        let grandchild: i32 = pid_str.trim().parse().expect("valid pid");
        let _ = std::fs::remove_file(&pid_file);

        // Give the kernel a moment to deliver and reap.
        let gone = (0..40).any(|_| {
            thread::sleep(Duration::from_millis(50));
            matches!(kill(Pid::from_raw(grandchild), None), Err(Errno::ESRCH))
        });
        assert!(
            gone,
            "grandchild pid {grandchild} outlived its parent's group kill",
        );
    }

    #[test]
    fn stderr_is_captured() {
        let r = exec("/bin/sh -c 'echo oops 1>&2'", opts(2000, 4096)).unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stderr.trim(), "oops");
    }

    #[test]
    fn output_is_capped() {
        let r = exec(
            "/bin/sh -c 'yes a | head -c 10000'",
            opts(2000, 128),
        )
        .unwrap();
        assert!(r.stdout.len() <= 128, "got {} bytes", r.stdout.len());
        assert!(r.too_large);
        assert!(!r.succeeded());
    }

    #[test]
    fn output_within_cap_is_not_flagged() {
        let r = exec("/bin/echo hi", opts(2000, 128)).unwrap();
        assert!(!r.too_large);
        assert!(r.succeeded());
    }

    #[test]
    fn stderr_is_capped_at_64k_even_with_a_larger_stdout_cap() {
        // ADR 0013: stdout up to max_output, stderr up to 64 KiB.
        let r = exec(
            "/bin/sh -c 'head -c 100000 /dev/zero | tr \"\\\\0\" e 1>&2'",
            opts(5000, 1024 * 1024),
        )
        .unwrap();
        assert_eq!(r.stderr.len(), MAX_STDERR);
        assert!(r.too_large);
    }

    #[test]
    fn endless_output_fails_fast_not_at_the_timeout() {
        // Past the cap the run is killed right away rather than
        // waiting out the timeout.
        let t0 = Instant::now();
        let r = exec("/bin/sh -c 'yes'", opts(10_000, 4096)).unwrap();
        assert!(r.too_large);
        assert!(!r.timed_out);
        assert!(t0.elapsed() < Duration::from_secs(5), "took {:?}", t0.elapsed());
    }

    #[test]
    fn env_is_clean_and_includes_paneld_markers() {
        // Set a secret-looking var in the harness, then confirm the
        // child never sees it.
        std::env::set_var("PANELD_RUNNER_LEAK_TEST", "nope");
        let o = Opts {
            timeout: Duration::from_secs(2),
            max_bytes: 4096,
            kind: Kind::Source,
            name: "cpu",
            cwd: None,
            env: &[],
        };
        let r = exec(
            "/bin/sh -c 'echo $PANELD:$PANELD_SOURCE:$PANELD_RUNNER_LEAK_TEST'",
            o,
        )
        .unwrap();
        assert_eq!(r.stdout.trim(), "1:cpu:");
        std::env::remove_var("PANELD_RUNNER_LEAK_TEST");
    }

    #[test]
    fn extra_env_pairs_are_passed_through() {
        let env = [("GREETING".to_string(), "hi".to_string())];
        let o = Opts {
            timeout: Duration::from_secs(2),
            max_bytes: 4096,
            kind: Kind::Action,
            name: "noop",
            cwd: None,
            env: &env,
        };
        let r = exec("/bin/sh -c 'echo $GREETING $PANELD_ACTION'", o).unwrap();
        assert_eq!(r.stdout.trim(), "hi noop");
    }

    #[test]
    fn argv_mode_does_not_run_a_shell() {
        // `; rm -rf /` must land as a literal argv item, not as a
        // separate command. If the runner ever regresses to sh -c
        // this test would run the `rm` and probably break the
        // harness, so it stays as a load-bearing safety net.
        let r = exec("/bin/echo foo; whoami", opts(2000, 4096)).unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stdout.trim(), "foo; whoami");
    }

    #[test]
    fn tokenize_handles_quotes_and_escapes() {
        assert_eq!(
            tokenize(r#"a 'b c' "d e"#).unwrap_err().message,
            "unterminated double quote"
        );
        assert_eq!(
            tokenize(r#"a 'b c' "d e""#).unwrap(),
            vec!["a", "b c", "d e"]
        );
        assert_eq!(
            tokenize(r#"path\ with\ spaces --flag=value"#).unwrap(),
            vec!["path with spaces", "--flag=value"]
        );
    }

    #[test]
    fn tokenize_rejects_empty() {
        assert!(tokenize("").is_err());
        assert!(tokenize("   \t\n").is_err());
    }

    #[test]
    fn tokenize_preserves_non_ascii_bytes() {
        // Interpolated secrets, titles and filenames may carry
        // non-ASCII UTF-8. The tokenizer must not round-trip bytes
        // through `as char`, which produces mojibake for anything
        // outside Latin-1.
        assert_eq!(
            tokenize("/bin/echo cafe\u{301} caf\u{00e9}").unwrap(),
            vec!["/bin/echo", "cafe\u{301}", "caf\u{00e9}"]
        );
        assert_eq!(
            tokenize("curl -H 'X-Name: \u{65e5}\u{672c}\u{8a9e}'").unwrap(),
            vec!["curl", "-H", "X-Name: \u{65e5}\u{672c}\u{8a9e}"]
        );
        assert_eq!(
            tokenize("curl -H \"X-Name: \\\"\u{00e9}\\\"\"").unwrap(),
            vec!["curl", "-H", "X-Name: \"\u{00e9}\""]
        );
    }

    #[test]
    fn non_ascii_argv_round_trips_through_exec() {
        // Prove the fix at the exec boundary, not just at the
        // tokenizer: a non-ASCII argv item must come back out of the
        // child verbatim.
        let r = exec("/bin/echo caf\u{00e9}", opts(2000, 4096)).unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stdout.trim(), "caf\u{00e9}");
    }
}
