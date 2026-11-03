//! `paneld ctl` over a Unix socket (ADR 0007, 0014).
//!
//! Wire format (ADR 0014): one JSON object per line, both ways. On
//! connect the daemon sends `{"paneld": "<version>", "protocol": 1}`.
//! A request is `{"id": 1, "cmd": "eval", "args": {"expr": "..."}}`;
//! its reply carries the same `id` and either `"ok": true, "result":
//! ...` or `"ok": false, "error": {"message", "file", "line",
//! "column"}`. A connection may send several requests.
//!
//! `ping` is answered here; everything that needs the live state goes
//! to the main loop as a [`Request`] and is answered from there.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use paneld_ui::Value;

pub const PROTOCOL: i64 = 1;

/// What the main loop gets from the ctl listener (and the config
/// watcher).
pub enum ControlMsg {
    /// From the config file watcher; no one waits for an answer.
    Reload,
    /// A ctl request, answered on `reply`.
    Request(Request),
}

pub struct Request {
    pub cmd: String,
    pub args: BTreeMap<String, Value>,
    pub reply: Sender<Reply>,
}

pub type Reply = Result<Value, CtlError>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CtlError {
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl CtlError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Self::default() }
    }
}

impl Request {
    /// A string argument, or an error naming it.
    pub fn arg(&self, name: &str) -> Result<String, CtlError> {
        match self.args.get(name) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Ok(v.to_display_string()),
            None => Err(CtlError::msg(format!("`{}` needs `{name}`", self.cmd))),
        }
    }

    pub fn opt_arg(&self, name: &str) -> Option<String> {
        self.arg(name).ok()
    }
}

/// Spawn a background thread that polls the config file's mtime
/// every `interval` and sends [`ControlMsg::Reload`] on `tx` when
/// it changes.
pub fn watch_config(path: &Path, interval: Duration, tx: Sender<ControlMsg>) {
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
    // A directory of our own in /tmp: a shared `/tmp/paneld.sock`
    // could be bound first by another user, and `ctl` would talk to
    // them.
    fallback_dir().join("paneld.sock")
}

fn fallback_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/paneld-{}", current_uid()))
}

fn current_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0)
}

/// The fallback directory must be ours and closed to others, or a
/// local user could have planted it.
fn check_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::symlink_metadata(dir)?;
    if !m.is_dir() || m.uid() != current_uid() || m.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a private directory of this user", dir.display()),
        ));
    }
    Ok(())
}

/// Start a background thread that listens on `path` and forwards
/// requests on `tx`. The socket file is unlinked on bind so a
/// leftover from a crashed daemon doesn't block startup; it is made
/// owner and group only (ADR 0007).
pub fn serve(path: &Path, tx: Sender<ControlMsg>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        if parent == fallback_dir() {
            check_private_dir(parent)?;
        } else {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    // A socket someone answers on belongs to a running daemon: do not
    // take it over. One nobody answers on is left from a crash.
    if UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("another paneld is listening on {}", path.display()),
        ));
    }
    let _ = std::fs::remove_file(path);
    // Bind with no bits for others, so the socket is never open to
    // them before the chmod below. Only "other" bits are masked: files
    // other threads create meanwhile keep working modes.
    use nix::sys::stat::{umask, Mode};
    let old = umask(Mode::from_bits_truncate(0o007));
    umask(old | Mode::from_bits_truncate(0o007));
    let bound = UnixListener::bind(path);
    umask(old);
    let listener = bound?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    let path = path.to_path_buf();
    thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    // One thread per client, bounded: a script that
                    // opens connections and never closes them must
                    // not exhaust threads on a 1 GB Pi.
                    if CLIENTS.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
                        CLIENTS.fetch_sub(1, Ordering::SeqCst);
                        let _ = writeln!(&stream, "{}", reply_line(&Value::Null, &Err(CtlError::msg("too many ctl clients"))));
                        continue;
                    }
                    let tx = tx.clone();
                    thread::spawn(move || {
                        handle_client(stream, tx);
                        CLIENTS.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) => eprintln!("paneld ctl: accept on {}: {e}", path.display()),
            }
        }
    });
    Ok(())
}

/// Longest request line; a longer one ends the connection.
const MAX_LINE: u64 = 64 * 1024;
/// Clients served at once.
const MAX_CLIENTS: usize = 16;
/// A client silent this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
static CLIENTS: AtomicUsize = AtomicUsize::new(0);

fn handle_client(stream: UnixStream, tx: Sender<ControlMsg>) {
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let greeting = format!(
        r#"{{"paneld":{},"protocol":{PROTOCOL}}}"#,
        json_string(env!("CARGO_PKG_VERSION"))
    );
    if writeln!(&stream, "{greeting}").is_err() {
        return;
    }
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.by_ref().take(MAX_LINE + 1).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.len() as u64 > MAX_LINE {
            let _ = writeln!(&stream, "{}", reply_line(&Value::Null, &Err(CtlError::msg("request line too long"))));
            return;
        }
        if line.trim().is_empty() {
            continue;
        }
        let reply = answer(&line, &tx);
        if writeln!(&stream, "{reply}").is_err() {
            return;
        }
    }
}

/// Turn one request line into its reply line.
fn answer(line: &str, tx: &Sender<ControlMsg>) -> String {
    let Some(Value::Map(req)) = crate::sources::parse_json(line.trim()) else {
        return reply_line(&Value::Null, &Err(CtlError::msg("not a JSON object")));
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let cmd = match req.get("cmd") {
        Some(Value::String(c)) => c.clone(),
        _ => return reply_line(&id, &Err(CtlError::msg("missing `cmd`"))),
    };
    let args = match req.get("args") {
        Some(Value::Map(m)) => m.clone(),
        None | Some(Value::Null) => BTreeMap::new(),
        Some(_) => return reply_line(&id, &Err(CtlError::msg("`args` must be an object"))),
    };
    if cmd == "ping" {
        return reply_line(&id, &Ok(Value::String("pong".into())));
    }
    let (reply_tx, reply_rx) = mpsc::channel();
    if tx.send(ControlMsg::Request(Request { cmd, args, reply: reply_tx })).is_err() {
        return reply_line(&id, &Err(CtlError::msg("daemon is shutting down")));
    }
    // A reload can take a moment (it runs every source once).
    let reply = reply_rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| Err(CtlError::msg("no answer from the daemon")));
    reply_line(&id, &reply)
}

fn reply_line(id: &Value, reply: &Reply) -> String {
    match reply {
        Ok(v) => format!(r#"{{"id":{},"ok":true,"result":{}}}"#, to_json(id), to_json(v)),
        Err(e) => {
            let mut err = format!(r#"{{"message":{}"#, json_string(&e.message));
            if let Some(f) = &e.file {
                err.push_str(&format!(r#","file":{}"#, json_string(f)));
            }
            if let Some(l) = e.line {
                err.push_str(&format!(r#","line":{l}"#));
            }
            if let Some(c) = e.column {
                err.push_str(&format!(r#","column":{c}"#));
            }
            err.push('}');
            format!(r#"{{"id":{},"ok":false,"error":{err}}}"#, to_json(id))
        }
    }
}

/// Serialise a [`Value`] as JSON. Non-finite numbers become `null`.
pub fn to_json(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Double(d) if d.is_finite() => {
            let s = d.to_string();
            if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
        }
        Value::Double(_) => "null".into(),
        Value::String(s) => json_string(s),
        Value::List(items) => {
            format!("[{}]", items.iter().map(to_json).collect::<Vec<_>>().join(","))
        }
        Value::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}:{}", json_string(k), to_json(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn json_string(s: &str) -> String {
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

/// Build the request for `paneld ctl <args>`: `(cmd, args)`.
pub fn parse_cli(args: &[String]) -> Result<(String, BTreeMap<String, Value>), String> {
    let s = |v: &str| Value::String(v.to_string());
    let mut a = BTreeMap::new();
    let Some(cmd) = args.first() else {
        return Err("missing command (ping, reload, check, eval, set, goto, alert, status, exit)".into());
    };
    let rest = &args[1..];
    let cmd = match cmd.as_str() {
        "ping" | "reload" | "check" | "status" | "exit" => {
            if let Some(x) = rest.first() {
                return Err(format!("unexpected argument `{x}`"));
            }
            cmd.clone()
        }
        "eval" => {
            if rest.is_empty() {
                return Err("`eval` needs an expression, e.g. paneld ctl eval 'sources.cpu.value'".into());
            }
            a.insert("expr".into(), s(&rest.join(" ")));
            "eval".into()
        }
        "goto" => {
            let [page] = rest else { return Err("`goto` needs one page name".into()) };
            a.insert("page".into(), s(page));
            "goto".into()
        }
        "set" => {
            let [pair] = rest else { return Err("`set` needs `vars.<name>=<value>`".into()) };
            let (k, v) = pair.split_once('=').ok_or("`set` needs `vars.<name>=<value>`")?;
            let k = k.trim();
            a.insert("var".into(), s(k.strip_prefix("vars.").unwrap_or(k)));
            a.insert("value".into(), s(v));
            "set".into()
        }
        "alert" => {
            let (verb, id, extra) = match rest {
                [verb, id, extra @ ..] => (verb.as_str(), id, extra),
                _ => return Err("`alert fire|resolve|ack|snooze <id> ...`".into()),
            };
            a.insert("id".into(), s(id));
            match verb {
                "fire" => {
                    for kv in extra {
                        let (k, v) = kv.split_once('=').ok_or(format!("expected key=value, got `{kv}`"))?;
                        if !matches!(k, "severity" | "title" | "text" | "link") {
                            return Err(format!("unknown alert field `{k}`"));
                        }
                        a.insert(k.into(), s(v));
                    }
                    "alert_fire".into()
                }
                "resolve" if extra.is_empty() => "alert_resolve".into(),
                "resolve" => return Err("`alert resolve` takes only the id".into()),
                "ack" if extra.is_empty() => "alert_ack".into(),
                "snooze" => {
                    match extra {
                        [] => {}
                        [d] => {
                            a.insert("for".into(), s(d));
                        }
                        _ => return Err("`alert snooze <id> [duration]`".into()),
                    }
                    "alert_snooze".into()
                }
                other => return Err(format!("unknown alert verb `{other}` (fire, resolve, ack, snooze)")),
            }
        }
        "reminder" => {
            let [verb, id] = rest else { return Err("`reminder done <name>`".into()) };
            if verb != "done" {
                return Err(format!("unknown reminder verb `{verb}` (done)"));
            }
            a.insert("id".into(), s(id));
            "reminder_done".into()
        }
        other => return Err(format!("unknown command `{other}`")),
    };
    Ok((cmd, a))
}

/// Run `paneld ctl <args>` against the socket at `path`. Prints the
/// result for people, or the raw reply with `json`. Exit status: 0 ok,
/// 1 the daemon reported an error, 2 usage or connection problem.
pub fn call(path: &Path, args: &[String], json: bool) -> i32 {
    let (cmd, a) = match parse_cli(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("paneld ctl: {e}");
            return 2;
        }
    };
    let stream = match UnixStream::connect(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("paneld ctl: cannot connect to {}: {e}", path.display());
            return 2;
        }
    };
    let mut reader = BufReader::new(&stream);
    let mut greeting = String::new();
    if reader.read_line(&mut greeting).is_err() {
        eprintln!("paneld ctl: no greeting from the daemon");
        return 2;
    }
    match crate::sources::parse_json(greeting.trim()) {
        Some(Value::Map(g)) if g.get("protocol") == Some(&Value::Int(PROTOCOL)) => {}
        _ => {
            eprintln!("paneld ctl: the daemon speaks another protocol: {}", greeting.trim());
            return 2;
        }
    }
    let request = format!(
        r#"{{"id":1,"cmd":{},"args":{}}}"#,
        json_string(&cmd),
        to_json(&Value::Map(a))
    );
    if writeln!(&stream, "{request}").is_err() {
        eprintln!("paneld ctl: write failed");
        return 2;
    }
    let mut reply = String::new();
    if reader.read_line(&mut reply).is_err() || reply.is_empty() {
        eprintln!("paneld ctl: no reply");
        return 2;
    }
    if json {
        print!("{reply}");
    }
    let Some(Value::Map(r)) = crate::sources::parse_json(reply.trim()) else {
        eprintln!("paneld ctl: bad reply: {}", reply.trim());
        return 2;
    };
    if r.get("ok") == Some(&Value::Bool(true)) {
        if !json {
            print_for_people(&cmd, r.get("result").unwrap_or(&Value::Null));
        }
        0
    } else {
        if !json {
            if let Some(Value::Map(e)) = r.get("error") {
                let msg = e.get("message").map(|m| m.to_display_string()).unwrap_or_default();
                match (e.get("file"), e.get("line"), e.get("column")) {
                    (Some(f), Some(l), Some(c)) => eprintln!(
                        "{}:{}:{}: {msg}",
                        f.to_display_string(),
                        l.to_display_string(),
                        c.to_display_string()
                    ),
                    _ => eprintln!("paneld ctl: {msg}"),
                }
            }
        }
        1
    }
}

/// A result as a person wants to read it: scalars plain, `check`
/// issues one per line like `paneld check`, the rest as JSON.
fn print_for_people(cmd: &str, result: &Value) {
    match (cmd, result) {
        ("check", Value::List(issues)) if issues.is_empty() => println!("ok"),
        ("check", Value::List(issues)) => {
            for i in issues {
                let Value::Map(m) = i else { continue };
                let get = |k: &str| m.get(k).map(Value::to_display_string);
                match (get("file"), get("line"), get("column")) {
                    (Some(f), Some(l), Some(c)) => {
                        println!("{f}:{l}:{c}: {}", get("message").unwrap_or_default())
                    }
                    _ => println!("{}", get("message").unwrap_or_default()),
                }
            }
        }
        (_, Value::Null) => {}
        (_, v @ (Value::String(_) | Value::Int(_) | Value::Double(_) | Value::Bool(_))) => {
            println!("{}", v.to_display_string())
        }
        (_, v) => println!("{}", to_json(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(a: &[&str]) -> Result<(String, BTreeMap<String, Value>), String> {
        parse_cli(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn json_round_trips_through_the_reader() {
        let mut m = BTreeMap::new();
        m.insert("s".to_string(), Value::String("he said \"hi\"\n".into()));
        m.insert("n".to_string(), Value::Int(-3));
        m.insert("d".to_string(), Value::Double(2.0));
        m.insert("l".to_string(), Value::List(vec![Value::Bool(true), Value::Null]));
        let v = Value::Map(m);
        assert_eq!(crate::sources::parse_json(&to_json(&v)), Some(v));
        assert_eq!(to_json(&Value::Double(f64::NAN)), "null");
    }

    #[test]
    fn socket_survives_hostile_clients() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("paneld-ctl-hostile-{}", std::process::id()));
        let path = dir.join("s.sock");
        let (tx, _rx) = mpsc::channel();
        serve(&path, tx).expect("serve");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o660);
        let talk = |line: &[u8]| {
            let s = UnixStream::connect(&path).unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut greeting = String::new();
            r.read_line(&mut greeting).unwrap();
            // The server may hang up mid-write: write on the side.
            let line = line.to_vec();
            let mut w = s.try_clone().unwrap();
            let writer = thread::spawn(move || {
                let _ = w.write_all(&line);
            });
            let mut reply = String::new();
            r.read_line(&mut reply).unwrap();
            drop(s);
            let _ = writer.join();
            reply
        };
        let mut deep = "[".repeat(200_000).into_bytes();
        deep.push(b'\n');
        // Over the line cap: refused before parsing.
        assert!(talk(&deep).contains("too long"), "long line");
        let mut nested = "[".repeat(10_000).into_bytes();
        nested.push(b'\n');
        assert!(talk(&nested).contains("not a JSON object"), "deep JSON");
        assert!(talk(b"{\"id\":1,\"cmd\":\"ping\"}\n").contains("pong"), "still serving");
        // A second daemon must not take the socket over.
        let (tx2, _rx2) = mpsc::channel();
        assert_eq!(serve(&path, tx2).unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
        assert!(talk(b"{\"id\":2,\"cmd\":\"ping\"}\n").contains("pong"), "first daemon lost its socket");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_dir_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("paneld-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        check_private_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        check_private_dir(&dir).unwrap();
        // Opened up (as a planted one would be): refused.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_private_dir(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_builds_requests() {
        assert_eq!(cli(&["ping"]).unwrap().0, "ping");
        let (c, a) = cli(&["eval", "sources.cpu.value", ">", "70"]).unwrap();
        assert_eq!(c, "eval");
        assert_eq!(a["expr"], Value::String("sources.cpu.value > 70".into()));
        let (c, a) = cli(&["set", "vars.tab=2"]).unwrap();
        assert_eq!((c.as_str(), &a["var"], &a["value"]), ("set", &Value::String("tab".into()), &Value::String("2".into())));
        let (c, a) = cli(&["alert", "fire", "disk", "severity=critical", "title=Disk full"]).unwrap();
        assert_eq!(c, "alert_fire");
        assert_eq!(a["title"], Value::String("Disk full".into()));
        assert_eq!(cli(&["alert", "resolve", "disk"]).unwrap().0, "alert_resolve");
        assert!(cli(&["alert", "fire", "x", "color=red"]).is_err());
        assert!(cli(&["goto"]).is_err());
        assert!(cli(&["frobnicate"]).is_err());
        let (c, a) = cli(&["reminder", "done", "restore_test"]).unwrap();
        assert_eq!((c.as_str(), &a["id"]), ("reminder_done", &Value::String("restore_test".into())));
        assert!(cli(&["reminder", "skip", "x"]).is_err());
    }

    #[test]
    fn replies_follow_adr_0014() {
        assert_eq!(
            reply_line(&Value::Int(7), &Ok(Value::Double(48.2))),
            r#"{"id":7,"ok":true,"result":48.2}"#
        );
        let e = CtlError {
            message: "bad".into(),
            file: Some("a.conf".into()),
            line: Some(3),
            column: Some(5),
        };
        assert_eq!(
            reply_line(&Value::Int(1), &Err(e)),
            r#"{"id":1,"ok":false,"error":{"message":"bad","file":"a.conf","line":3,"column":5}}"#
        );
    }

    #[test]
    fn a_connection_gets_a_greeting_and_several_answers() {
        let dir = std::env::temp_dir().join(format!("paneld-ctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let (tx, rx) = mpsc::channel();
        serve(&sock, tx).unwrap();
        // A fake main loop answering `eval` with the expression text.
        thread::spawn(move || {
            for msg in rx {
                if let ControlMsg::Request(r) = msg {
                    let expr = r.arg("expr");
                    let _ = r.reply.send(expr.map(Value::String));
                }
            }
        });
        let stream = UnixStream::connect(&sock).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains(r#""protocol":1"#), "{line}");
        writeln!(&stream, r#"{{"id":1,"cmd":"ping"}}"#).unwrap();
        writeln!(&stream, r#"{{"id":2,"cmd":"eval","args":{{"expr":"1+1"}}}}"#).unwrap();
        writeln!(&stream, r#"{{"id":3,"cmd":"eval"}}"#).unwrap();
        let mut replies = Vec::new();
        for _ in 0..3 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            replies.push(line.trim().to_string());
        }
        assert_eq!(replies[0], r#"{"id":1,"ok":true,"result":"pong"}"#);
        assert_eq!(replies[1], r#"{"id":2,"ok":true,"result":"1+1"}"#);
        assert!(replies[2].starts_with(r#"{"id":3,"ok":false"#), "{}", replies[2]);
        let mode = std::fs::metadata(&sock).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o660);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
