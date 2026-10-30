//! Minimal command runner (ADR 0013). Executes a single command via
//! `sh -c`, capturing stdout and stderr with a timeout and an output
//! cap. Argv-mode exec, secret substitution, env masking and
//! concurrency control come in later phases.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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

/// Run `cmd` through `sh -c`. The daemon kills the child once
/// `timeout` elapses and truncates stdout / stderr to `max_bytes`.
pub fn exec(cmd: &str, timeout: Duration, max_bytes: usize) -> Result<RunResult, RunError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunError { message: format!("spawn `{cmd}`: {e}") })?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    // Drain both pipes concurrently so a child writing a lot to one
    // of them can't deadlock the daemon waiting on the other. Caps
    // are enforced inside each reader.
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
                if start.elapsed() >= timeout {
                    let _ = child.kill();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_captures_stdout() {
        let r = exec("echo hello", Duration::from_secs(2), 4096).unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stdout.trim(), "hello");
        assert!(!r.timed_out);
    }

    #[test]
    fn nonzero_exit_is_surfaced() {
        let r = exec("exit 7", Duration::from_secs(2), 4096).unwrap();
        assert_eq!(r.exit_code, Some(7));
    }

    #[test]
    fn timeout_fires() {
        let r = exec("sleep 2", Duration::from_millis(100), 4096).unwrap();
        assert!(r.timed_out);
        assert!(r.exit_code.is_none());
    }

    #[test]
    fn stderr_is_captured() {
        let r = exec("echo oops 1>&2", Duration::from_secs(2), 4096).unwrap();
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stderr.trim(), "oops");
    }

    #[test]
    fn output_is_capped() {
        // `yes | head` is used to flood stdout; `yes` is killed by
        // SIGPIPE once `head` has consumed enough, so the final
        // pipeline exit code reflects `head` -- either 0, or 141
        // (SIGPIPE) depending on which end the shell samples. We
        // only care that our cap held.
        let r = exec("yes a | head -c 10000", Duration::from_secs(2), 128).unwrap();
        assert!(r.stdout.len() <= 128, "got {} bytes", r.stdout.len());
    }
}
