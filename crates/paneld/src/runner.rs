//! Command runner (ADR 0013). Executes commands argv-mode, with a
//! clean environment and the child in its own process group so a
//! timeout kills the command and anything it spawned. The input is
//! always interpreted as an argv list -- the raw text is tokenised
//! with POSIX-style single and double quotes so secrets and other
//! interpolated values reach the command as argv items, never as
//! shell syntax. The `app:timeout`, `app:max_output`, `app:max_running`
//! and `cwd` handles come in a later phase; defaults are enforced
//! here.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
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
    /// A caller that doesn't set either env var (today only the
    /// runner's own tests).
    #[allow(dead_code)]
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


/// Tokenise `cmd` into an argv list using POSIX-ish single/double
/// quote rules: single quotes are literal, double quotes allow `\`
/// escaping of `"`, `\\` and `$`, and whitespace outside quotes
/// splits tokens. No variable expansion, no globbing, no backticks:
/// the caller already interpolated everything it wanted interpolated.
pub fn tokenize(cmd: &str) -> Result<Vec<String>, RunError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_tok = false;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' if !in_tok => {}
            ' ' | '\t' | '\n' | '\r' => {
                out.push(std::mem::take(&mut cur));
                in_tok = false;
            }
            '\'' => {
                in_tok = true;
                let mut closed = false;
                for c2 in chars.by_ref() {
                    if c2 == '\'' {
                        closed = true;
                        break;
                    }
                    cur.push(c2);
                }
                if !closed {
                    return Err(RunError {
                        message: "unterminated single quote".into(),
                    });
                }
            }
            '"' => {
                in_tok = true;
                let mut closed = false;
                while let Some(c2) = chars.next() {
                    if c2 == '"' {
                        closed = true;
                        break;
                    }
                    if c2 == '\\' {
                        if let Some(&next) = chars.peek() {
                            if matches!(next, '"' | '\\' | '$' | '`' | '\n') {
                                cur.push(next);
                                chars.next();
                                continue;
                            }
                        }
                    }
                    cur.push(c2);
                }
                if !closed {
                    return Err(RunError {
                        message: "unterminated double quote".into(),
                    });
                }
            }
            '\\' => {
                in_tok = true;
                // Backslash escapes the next character outside quotes.
                // A trailing bare backslash is kept as a literal, same
                // as most POSIX shells.
                match chars.next() {
                    Some(next) => cur.push(next),
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
        return Err(RunError {
            message: "empty command".into(),
        });
    }
    Ok(out)
}

/// Run `cmd` as argv (parsed with [`tokenize`]). The child gets a
/// clean environment, its own process group, stdin from `/dev/null`,
/// stdout and stderr captured up to `opts.max_bytes`, and is killed
/// as a group once `opts.timeout` elapses.
pub fn exec(cmd: &str, opts: Opts<'_>) -> Result<RunResult, RunError> {
    let argv = tokenize(cmd)?;
    exec_argv(&argv, opts)
}

fn exec_argv(argv: &[String], opts: Opts<'_>) -> Result<RunResult, RunError> {
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

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let max_bytes = opts.max_bytes;
    let out_handle = thread::spawn(move || {
        let mut buf = Vec::with_capacity(4096);
        let _ = stdout.by_ref().take(max_bytes as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });
    let err_handle = thread::spawn(move || {
        let mut buf = Vec::with_capacity(4096);
        let _ = stderr.by_ref().take(max_bytes as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    });

    let start = Instant::now();
    let (exit_code, timed_out) = loop {
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
pub fn prepared_command(cmd: &str, opts: &Opts<'_>) -> Result<Command, RunError> {
    let argv = tokenize(cmd)?;
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
