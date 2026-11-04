//! End-to-end checks of the running daemon: start the real binary
//! headless (SDL's `dummy` video driver), talk to it with `paneld
//! ctl` over its own socket, and look at what it answers and what it
//! writes. Each test gets its own folder, socket and state file.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static N: AtomicUsize = AtomicUsize::new(0);

fn tmp(label: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "daemon-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A running daemon on `dir/p.conf`, stopped when dropped.
struct Daemon {
    dir: PathBuf,
    child: Child,
}

impl Daemon {
    /// Write `conf` (with `app:state` pointed into the folder and a
    /// small SDL window) and start the daemon on it.
    fn start(dir: &Path, conf: &str) -> Daemon {
        write_conf(dir, conf);
        Self::start_existing(dir)
    }

    fn start_existing(dir: &Path) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_paneld"))
            .arg("-c")
            .arg(dir.join("p.conf"))
            .env("SDL_VIDEODRIVER", "dummy")
            .env("PANELD_SOCK", dir.join("ctl.sock"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("log")).unwrap())
            .spawn()
            .expect("start paneld");
        let d = Daemon { dir: dir.to_path_buf(), child };
        let deadline = Instant::now() + Duration::from_secs(10);
        while d.ctl(&["ping"]).status.code() != Some(0) {
            assert!(Instant::now() < deadline, "the daemon did not answer: {}", d.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        d
    }

    fn ctl(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_paneld"))
            .arg("ctl")
            .args(args)
            .env("PANELD_SOCK", self.dir.join("ctl.sock"))
            .output()
            .expect("run paneld ctl")
    }

    /// `ctl <args>`, which must succeed; its output, trimmed.
    fn ok(&self, args: &[&str]) -> String {
        let o = self.ctl(args);
        assert_eq!(o.status.code(), Some(0), "ctl {args:?}: {}{}", text(&o.stdout), text(&o.stderr));
        text(&o.stdout).trim().to_string()
    }

    /// `ctl <args>`, which must fail with exit 1 (the daemon said no);
    /// what it said.
    fn refused(&self, args: &[&str]) -> String {
        let o = self.ctl(args);
        assert_eq!(o.status.code(), Some(1), "ctl {args:?} should be refused: {}", text(&o.stdout));
        format!("{}{}", text(&o.stdout), text(&o.stderr))
    }

    fn eval(&self, expr: &str) -> String {
        self.ok(&["eval", expr])
    }

    /// Wait until `expr` evaluates to `want` (sources and alerts move
    /// on their own).
    fn wait_for(&self, expr: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let o = self.ctl(&["eval", expr]);
            if text(&o.stdout).trim() == want {
                return;
            }
            assert!(Instant::now() < deadline, "`{expr}` stayed `{}`, not `{want}`: {}", text(&o.stdout).trim(), self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    /// `ctl exit`, and wait for the process to end.
    fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.ctl(&["exit"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            self.shutdown();
        }
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn write_conf(dir: &Path, conf: &str) {
    let full = format!(
        "display {{ backend = sdl; width = 320; height = 240 }}\napp {{ state = {} }}\n{conf}",
        dir.join("state").display()
    );
    std::fs::write(dir.join("p.conf"), full).unwrap();
}

#[test]
fn ctl_reads_and_changes_the_running_state() {
    let dir = tmp("basics");
    let d = Daemon::start(
        &dir,
        "var n: int = 0\n\
         var on: bool = false\n\
         source c { run = echo 5; parse = number }\n\
         page home { title = Home; nav = 1; text { value = {{ vars.n }} } }\n\
         page other { title = Other; nav = 2; text { value = x } }\n",
    );
    // status: the page, vars and sources as JSON.
    let status = d.ok(&["--json", "status"]);
    assert!(status.contains("\"page\":\"home\""), "{status}");
    assert!(status.contains("\"vars\":{\"n\":0,\"on\":false}"), "{status}");
    assert!(status.contains("\"value\":5.0"), "{status}");
    // eval against the live context.
    assert_eq!(d.eval("sources.c.value + 1"), "6");
    assert_eq!(d.eval("vars.on ? 'yes' : 'no'"), "no");
    assert!(d.refused(&["eval", "nope.x"]).contains("unknown identifier `nope`"));
    assert!(d.refused(&["eval", "1 +"]).contains("empty expression"));
    assert!(d.refused(&["eval", "(1"]).contains("paneld ctl:"));
    // set: typed, checked, expressions allowed.
    assert_eq!(d.ok(&["set", "vars.n=3"]), "3");
    assert_eq!(d.ok(&["set", "n={{ vars.n * 2 }}"]), "6");
    assert_eq!(d.eval("vars.n"), "6");
    assert_eq!(d.ok(&["set", "vars.on=true"]), "true");
    assert!(d.refused(&["set", "vars.n=abc"]).contains("does not fit var `n` (int)"));
    assert!(d.refused(&["set", "vars.nope=1"]).contains("no var `nope`"));
    // goto: a page, `back`, and a missing one.
    assert_eq!(d.ok(&["goto", "other"]), "other");
    assert!(d.ok(&["--json", "status"]).contains("\"page\":\"other\""));
    assert_eq!(d.ok(&["goto", "back"]), "home");
    assert!(d.refused(&["goto", "nope"]).contains("no page `nope`"));
    // check on a good config: no issues.
    let check = d.ok(&["--json", "check"]);
    assert!(check.contains("\"result\":[]"), "{check}");
    d.stop();
    assert!(!dir.join("ctl.sock").exists(), "the socket outlived the daemon");
}

#[test]
fn alerts_fired_over_ctl_go_through_ack_snooze_and_resolve() {
    let dir = tmp("alerts");
    let d = Daemon::start(&dir, "page home { text { value = x } }\n");
    assert_eq!(d.eval("alerts.count"), "0");
    assert_eq!(d.ok(&["alert", "fire", "disk", "severity=critical", "title=Disk full", "text=sda1 at 99%"]), "ctl:disk");
    assert_eq!(d.eval("alerts.count"), "1");
    assert_eq!(d.eval("alerts.worst"), "critical");
    assert_eq!(d.eval("alerts.active[0].title"), "Disk full");
    assert_eq!(d.eval("alerts.active[0].text"), "sda1 at 99%");
    // The banner shows it until it is acknowledged.
    assert_eq!(d.eval("len(alerts.shown)"), "1");
    assert_eq!(d.ok(&["alert", "ack", "disk"]), "ok");
    assert_eq!(d.eval("alerts.active[0].acked"), "true");
    assert_eq!(d.eval("len(alerts.shown)"), "0");
    // A second one, snoozed.
    d.ok(&["alert", "fire", "temp", "severity=warning"]);
    assert_eq!(d.eval("len(alerts.shown)"), "1");
    assert_eq!(d.ok(&["alert", "snooze", "temp", "1h"]), "ok");
    assert_eq!(d.eval("len(alerts.shown)"), "0");
    assert_eq!(d.eval("alerts.count"), "2");
    // Resolving moves it to the history.
    assert_eq!(d.ok(&["alert", "resolve", "disk"]), "resolved");
    assert_eq!(d.eval("alerts.count"), "1");
    assert_eq!(d.eval("alerts.recent[0].title"), "Disk full");
    assert!(d.refused(&["alert", "resolve", "disk"]).contains("no active ctl alert `disk`"));
    assert!(d.refused(&["alert", "fire", "x", "severity=loud"]).contains("use critical, warning or info"));
    assert!(d.refused(&["alert", "ack", "nope"]).contains("nope"));
}

#[test]
fn an_acknowledged_rule_alert_stays_acknowledged_after_a_restart() {
    let dir = tmp("restart");
    let conf = "source c { run = echo 5; parse = number }\n\
                alert high { when = {{ sources.c.value > 1 }}; severity = warning; title = High }\n\
                page home { text { value = x } }\n";
    let d = Daemon::start(&dir, conf);
    d.wait_for("alerts.count", "1");
    let id = d.eval("alerts.active[0].id");
    d.ok(&["alert", "ack", &id]);
    assert_eq!(d.eval("alerts.active[0].acked"), "true", "ack of {id}");
    d.stop();
    assert!(dir.join("state").exists(), "no state file written");

    let d = Daemon::start_existing(&dir);
    d.wait_for("alerts.count", "1");
    assert_eq!(d.eval("alerts.active[0].acked"), "true", "the ack was lost: {}", d.log());
}

#[test]
fn reload_applies_a_good_config_and_keeps_the_last_good_one() {
    let dir = tmp("reload");
    let d = Daemon::start(&dir, "var a: int = 1\npage home { text { value = x } }\n");
    assert_eq!(d.eval("vars.a"), "1");

    write_conf(&dir, "var a: int = 1\nvar b: string = \"new\"\npage home { text { value = x } }\n");
    assert_eq!(d.ok(&["reload"]), "reloaded");
    assert_eq!(d.eval("vars.b"), "new");

    // A broken config: refused with file and line, the old one runs on.
    write_conf(&dir, "var a: int = 1\npage home { nosuch { } }\n");
    let why = d.refused(&["reload"]);
    assert!(why.contains("p.conf:4"), "{why}");
    assert_eq!(d.eval("vars.b"), "new");
    let issues = d.ok(&["--json", "check"]);
    assert!(issues.contains("nosuch"), "{issues}");
    assert!(issues.contains("\"line\":4"), "{issues}");
}

#[test]
fn saving_the_config_reloads_it() {
    let dir = tmp("watch");
    let d = Daemon::start(&dir, "var a: int = 1\npage home { text { value = x } }\n");
    // The watcher compares modification times: make sure this one is
    // later than the first write.
    std::thread::sleep(Duration::from_millis(1100));
    write_conf(&dir, "var a: int = 2\npage home { text { value = x } }\n");
    d.wait_for("vars.a", "2");
}

#[test]
fn a_remote_is_reachable_over_ctl_by_its_name() {
    let dir = tmp("remote");
    let d = Daemon::start(
        &dir,
        "remote_kind box {\n param n: int\n via = local\n value v { run = echo {{ n }}; parse = number }\n}\n\
         remote b1: box { n = 7 }\n\
         page home { text { value = {{ b1.v }} } }\n",
    );
    d.wait_for("b1.v", "7");
    assert_eq!(d.eval("b1.ok"), "true");
    assert_eq!(d.eval("b1.status.v.ok"), "true");
    // Remote members are not plain sources.
    assert_eq!(d.eval("'b1.v' in sources"), "false");
    let status = d.ok(&["--json", "status"]);
    assert!(status.contains("\"b1.v\""), "{status}");
}

#[test]
fn a_config_that_never_loaded_leaves_the_daemon_stopped() {
    let dir = tmp("bad");
    write_conf(&dir, "page home { nosuch { } }\n");
    let out = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .arg("-c")
        .arg(dir.join("p.conf"))
        .env("SDL_VIDEODRIVER", "dummy")
        .env("PANELD_SOCK", dir.join("ctl.sock"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("p.conf:3"), "{err}");
    assert!(err.contains("refusing to apply"), "{err}");
}

#[test]
fn ctl_prints_for_people_and_reports_its_own_problems() {
    let dir = tmp("cli");
    let d = Daemon::start(&dir, "var n: int = 0\npage home { text { value = x } }\n");
    // For people: scalars plain, `check` as `ok`, maps as JSON.
    assert_eq!(d.ok(&["check"]), "ok");
    assert_eq!(d.ok(&["eval", "'a' + 'b'"]), "ab");
    assert_eq!(d.ok(&["eval", "{'k': [1, true]}"]), "{\"k\":[1,true]}");
    assert_eq!(d.ok(&["eval", "null"]), "");
    // A reload that fails says where, like `paneld check`.
    write_conf(&dir, "var n: int = 0\npage home { nosuch { } }\n");
    let o = d.ctl(&["reload"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("p.conf:4:"), "{}", text(&o.stderr));
    let o = d.ctl(&["check"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o.stdout).contains("p.conf:4:"), "{}", text(&o.stdout));
    // Usage problems are exit 2, before any connection.
    for bad in [&["nope"][..], &["status", "x"], &["eval"], &["goto"], &["set", "x"], &["alert", "x"], &["alert", "poke", "a"],
                &["alert", "fire", "a", "color=red"], &["alert", "resolve", "a", "b"], &["alert", "snooze", "a", "1h", "2h"],
                &["reminder", "skip", "a"], &["reminder"]] {
        let o = d.ctl(bad);
        assert_eq!(o.status.code(), Some(2), "ctl {bad:?}: {}", text(&o.stderr));
        assert!(text(&o.stderr).starts_with("paneld ctl: "), "{bad:?}");
    }
    d.stop();
    // Nobody listening: exit 2 and the socket path.
    let o = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .args(["ctl", "status"])
        .env("PANELD_SOCK", dir.join("ctl.sock"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("cannot connect to"), "{}", text(&o.stderr));
}

#[test]
fn ctl_refuses_a_socket_that_speaks_something_else() {
    use std::io::Write;
    let dir = tmp("impostor");
    let sock = dir.join("ctl.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        for greeting in ["hello\n", "{\"protocol\": 99}\n"] {
            let (mut c, _) = listener.accept().unwrap();
            let _ = c.write_all(greeting.as_bytes());
        }
    });
    for _ in 0..2 {
        let o = Command::new(env!("CARGO_BIN_EXE_paneld"))
            .args(["ctl", "status"])
            .env("PANELD_SOCK", &sock)
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2));
        assert!(text(&o.stderr).contains("speaks another protocol"), "{}", text(&o.stderr));
    }
    server.join().unwrap();
}

#[test]
fn a_second_daemon_does_not_take_over_a_live_socket() {
    let dir = tmp("second");
    let d = Daemon::start(&dir, "var n: int = 1\npage home { text { value = x } }\n");
    let second = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .arg("-c")
        .arg(dir.join("p.conf"))
        .env("SDL_VIDEODRIVER", "dummy")
        .env("PANELD_SOCK", dir.join("ctl.sock"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Give it time to try the socket, then make sure the first one
    // still answers, and stop the second.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(d.eval("vars.n"), "1");
    let mut second = second;
    let _ = second.kill();
    let out = second.wait_with_output().unwrap();
    assert!(text(&out.stderr).contains("cannot bind ctl socket"), "{}", text(&out.stderr));
}

#[test]
fn a_poll_that_stops_answering_keeps_its_last_value_and_says_why() {
    let dir = tmp("timeout");
    let flag = dir.join("slow");
    let d = Daemon::start(
        &dir,
        &format!(
            "source c {{\n poll = 1s\n timeout = 1s\n run = sh -c 'if [ -e \"$$0\" ]; then sleep 3; fi; echo 5' {}\n parse = number\n}}\n\
             page home {{ text {{ value = x }} }}\n",
            flag.display()
        ),
    );
    d.wait_for("sources.c.ok", "true");
    assert_eq!(d.eval("sources.c.value"), "5");
    let at = d.eval("sources.c.at");
    std::fs::write(&flag, "").unwrap();
    d.wait_for("sources.c.ok", "false");
    assert_eq!(d.eval("sources.c.error"), "timed out");
    assert_eq!(d.eval("sources.c.value"), "5", "the last value is kept");
    assert_eq!(d.eval("sources.c.at"), at, "and so is when it was taken");
    // Answering again clears it.
    std::fs::remove_file(&flag).unwrap();
    d.wait_for("sources.c.ok", "true");
    assert_eq!(d.eval("sources.c.error == ''"), "true");
}

/// The local hour, from `date`, so a night window can be put around now.
fn local_hour() -> u32 {
    let o = Command::new("date").arg("+%H").output().unwrap();
    text(&o.stdout).trim().parse().unwrap()
}

#[test]
fn the_screen_dims_at_night_sleeps_when_idle_and_wakes_for_a_critical_alert() {
    let dir = tmp("screen");
    let log = dir.join("screen.log");
    let h = local_hour();
    let (from, to) = ((h + 23) % 24, (h + 2) % 24);
    let d = Daemon::start(
        &dir,
        &format!(
            "theme night_theme {{ accent = #123456 }}\n\
             screen {{\n\
                 idle = 1s\n\
                 brightness = 40\n\
                 off = sh -c 'echo off >> \"$$0\"' {log}\n\
                 on = sh -c 'echo on >> \"$$0\"' {log}\n\
                 dim = sh -c 'echo \"dim $$1\" >> \"$$0\"' {log} {{{{ level }}}}\n\
                 night {{ from = {from:02}:00; to = {to:02}:00; theme = night_theme; brightness = 20 }}\n\
             }}\n\
             page home {{ text {{ value = x }} }}\n",
            log = log.display()
        ),
    );
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let wait_log = |want: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        // A prefix: after a wake it idles off again a second later.
        while !read().starts_with(want) {
            assert!(Instant::now() < deadline, "screen log `{}`, not `{want}`", read());
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // Night: its brightness and theme; then off after a second idle.
    wait_log("dim 20\noff\n");
    assert_eq!(d.eval("theme.accent"), "#123456");
    // A critical alert wakes it (warnings do not).
    d.ok(&["alert", "fire", "w", "severity=warning"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(read(), "dim 20\noff\n");
    d.ok(&["alert", "fire", "c", "severity=critical"]);
    wait_log("dim 20\noff\non\ndim 20\n");
}
