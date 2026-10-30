//! `paneld ctl` over a Unix socket (ADR 0007, 0014).
//!
//! The daemon's listener thread accepts connections, reads one line
//! of JSON per request, and emits a [`ControlMsg`] on the main
//! loop's channel. The reply is a one-line JSON string.
//!
//! Scope today: `ping`, `exit`, `reload`. Status queries, source
//! snapshots, action triggers over ctl come in later phases. No
//! auth, no size cap on reply bodies (the surface is small).
//!
//! JSON shape is minimal and hand-rolled -- no `serde` dependency.
//! The daemon only parses `"cmd": "<name>"` out of the request and
//! writes `{"ok": true, "msg": "..."}` or `{"ok": false, "msg":
//! "..."}` back.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

/// Commands the ctl listener posts to the main loop.
#[derive(Debug)]
pub enum ControlMsg {
    Exit,
    Reload,
}

/// Spawn a background thread that polls the config file's mtime
/// every `interval` and sends [`ControlMsg::Reload`] on `tx` when
/// it changes. Zero dependency; the device build will swap this
/// for an inotify-based watch later.
pub fn watch_config(path: &Path, interval: std::time::Duration, tx: Sender<ControlMsg>) {
    let path = path.to_path_buf();
    let mut last = mtime(&path);
    thread::spawn(move || loop {
        thread::sleep(interval);
        let now = mtime(&path);
        if now != last {
            last = now;
            if tx.send(ControlMsg::Reload).is_err() {
                return;
            }
        }
    });
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Default socket path. Can be overridden via the `PANELD_SOCK`
/// env var; the device build would use `/run/paneld/<name>.sock`
/// per ADR 0014.
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("PANELD_SOCK") {
        return PathBuf::from(p);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("paneld.sock");
    }
    PathBuf::from("/tmp/paneld.sock")
}

/// Start a background thread that listens on `path` and forwards
/// [`ControlMsg`] on `tx`. Returns immediately. The socket file is
/// unlinked on bind so a leftover from a crashed daemon doesn't
/// block startup.
pub fn serve(path: &Path, tx: Sender<ControlMsg>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = UnixListener::bind(path)?;
    let path = path.to_path_buf();
    thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let tx = tx.clone();
                    thread::spawn(move || handle_client(stream, tx));
                }
                Err(e) => {
                    eprintln!("paneld ctl: accept on {}: {e}", path.display());
                }
            }
        }
    });
    Ok(())
}

fn handle_client(stream: UnixStream, tx: Sender<ControlMsg>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let cmd = extract_cmd(&line).unwrap_or_default();
    let reply = match cmd.as_str() {
        "ping" => reply_ok("pong"),
        "exit" => {
            let _ = tx.send(ControlMsg::Exit);
            reply_ok("exiting")
        }
        "reload" => {
            let _ = tx.send(ControlMsg::Reload);
            reply_ok("reloading")
        }
        other => reply_err(&format!("unknown command `{other}`")),
    };
    let stream = reader.into_inner();
    // Best-effort write of the reply; a half-closed client is a
    // benign case (`paneld ctl exit` disconnects after reading).
    let _ = (&stream).write_all(reply.as_bytes());
    let _ = (&stream).write_all(b"\n");
}

/// Scan a request body for `"cmd": "..."` and return the value.
/// The daemon-side format is a single JSON object per line, with
/// `cmd` always present. A full JSON parser is overkill for the
/// three-command surface today.
fn extract_cmd(line: &str) -> Option<String> {
    let s = line.trim();
    let key_at = s.find("\"cmd\"")?;
    let after_key = &s[key_at + "\"cmd\"".len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?.trim_start();
    let rest = after_colon.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn reply_ok(msg: &str) -> String {
    format!(r#"{{"ok":true,"msg":{}}}"#, json_escape(msg))
}

fn reply_err(msg: &str) -> String {
    format!(r#"{{"ok":false,"msg":{}}}"#, json_escape(msg))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Client side (`paneld ctl ...`)
// ---------------------------------------------------------------------------

/// Send `cmd` to the daemon socket at `path` and print the reply.
/// Returns 0 on an `ok: true` reply, 1 otherwise.
pub fn call(path: &Path, cmd: &str) -> i32 {
    let stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("paneld ctl: cannot connect to {}: {e}", path.display());
            return 2;
        }
    };
    let request = format!(r#"{{"cmd":{}}}"#, json_escape(cmd));
    if let Err(e) = (&stream).write_all(request.as_bytes()).and_then(|_| (&stream).write_all(b"\n"))
    {
        eprintln!("paneld ctl: write: {e}");
        return 2;
    }
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    if let Err(e) = reader.read_line(&mut reply) {
        eprintln!("paneld ctl: read: {e}");
        return 2;
    }
    print!("{reply}");
    if reply.contains(r#""ok":true"#) {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_cmd_simple() {
        assert_eq!(
            extract_cmd(r#"{"cmd": "ping"}"#),
            Some("ping".into())
        );
        assert_eq!(
            extract_cmd(r#" {  "cmd":"reload"} "#),
            Some("reload".into())
        );
        assert_eq!(extract_cmd(r#"{"other": "x"}"#), None);
    }

    #[test]
    fn json_escape_handles_quotes_and_control() {
        assert_eq!(json_escape(r#"he said "hi""#), r#""he said \"hi\"""#);
        assert_eq!(json_escape("a\nb"), r#""a\nb""#);
    }
}
