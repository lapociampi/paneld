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
            // LVGL's own log (printf): checked when the daemon is dropped.
            .stdout(std::fs::File::create(dir.join("lvgl.log")).unwrap())
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
        // An LVGL error is a bug even when the test's checks pass (not
        // asserted while already failing: a second panic aborts).
        let log = std::fs::read_to_string(self.dir.join("lvgl.log")).unwrap_or_default();
        if !std::thread::panicking() {
            assert!(!log.contains("[Error]"), "LVGL logged an error:\n{log}");
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
    // The objects on screen (the shell, the page, its text) and memory.
    assert!(status_num(&d, "nodes") > 3, "{status}");
    assert!(status_num(&d, "rss_kb") > 1000, "{status}");
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

#[test]
fn a_visible_only_source_polls_while_its_page_is_on_screen() {
    let dir = tmp("visible");
    let count = |name: &str| dir.join(name);
    let d = Daemon::start(
        &dir,
        &format!(
            "source shown {{\n poll = 1s\n active = visible\n run = sh -c 'echo x >> \"$$0\"; wc -l < \"$$0\"' {}\n parse = number\n}}\n\
             source watched {{\n poll = 1s\n active = visible\n run = sh -c 'echo x >> \"$$0\"; wc -l < \"$$0\"' {}\n parse = number\n}}\n\
             alert many {{ when = {{{{ sources.watched.value > 1000 }}}}; title = many }}\n\
             page home {{ nav = 1; text {{ value = {{{{ sources.shown.value }}}} }} }}\n\
             page other {{ nav = 2; text {{ value = elsewhere }} }}\n",
            count("shown").display(),
            count("watched").display()
        ),
    );
    let runs = |name: &str| std::fs::read_to_string(count(name)).unwrap_or_default().lines().count();
    // On its page: it polls.
    let start = runs("shown");
    std::thread::sleep(Duration::from_millis(2300));
    assert!(runs("shown") >= start + 2, "did not poll on its page");
    // Elsewhere: it rests; the one an alert reads goes on.
    d.ok(&["goto", "other"]);
    std::thread::sleep(Duration::from_millis(300));
    let (hidden, watched) = (runs("shown"), runs("watched"));
    std::thread::sleep(Duration::from_millis(2300));
    assert_eq!(runs("shown"), hidden, "polled while not on screen");
    assert!(runs("watched") >= watched + 2, "an alert's source stopped");
    // Back on its page: polling again.
    d.ok(&["goto", "home"]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(runs("shown") > hidden, "did not resume on its page");
}

#[test]
fn reload_keeps_vars_the_screen_set_and_applies_new_defaults_to_the_rest() {
    let dir = tmp("reload-vars");
    let d = Daemon::start(&dir, "var tab: int = 0\nvar mode: string = \"a\"\nvar gone: int = 1\npage home { text { value = x } }\n");
    d.ok(&["set", "vars.tab=3"]);
    write_conf(&dir, "var tab: int = 0\nvar mode: string = \"b\"\nvar extra: bool = true\npage home { text { value = x } }\n");
    assert_eq!(d.ok(&["reload"]), "reloaded");
    assert_eq!(d.eval("vars.tab"), "3", "a value the screen set is kept");
    assert_eq!(d.eval("vars.mode"), "b", "an untouched var takes its new default");
    assert_eq!(d.eval("vars.extra"), "true");
    assert!(d.refused(&["eval", "vars.gone"]).contains("gone"));
    // A var whose type changed starts over.
    write_conf(&dir, "var tab: string = \"x\"\npage home { text { value = x } }\n");
    d.ok(&["reload"]);
    assert_eq!(d.eval("vars.tab"), "x");
}

#[test]
fn a_remote_kind_declares_an_alert_for_each_instance() {
    let dir = tmp("kind-alert");
    let d = Daemon::start(
        &dir,
        "remote_kind box {\n\
             param label: string\n\
             param heat: int\n\
             via = local\n\
             value temp { run = echo {{ heat }}; parse = number }\n\
             alert hot { when = {{ self.temp > 80 }}; severity = critical; title = {{ label }} is hot }\n\
         }\n\
         remote a: box { label = Rack; heat = 90 }\n\
         remote b: box { label = Desk; heat = 10 }\n\
         page home { text { value = x } }\n",
    );
    d.wait_for("alerts.count", "1");
    assert_eq!(d.eval("alerts.active[0].title"), "Rack is hot");
    assert_eq!(d.eval("alerts.active[0].rule"), "a.hot");
    assert_eq!(d.eval("alerts.worst"), "critical");
}

#[test]
fn reloads_free_the_fonts_of_the_old_screen() {
    let dir = tmp("fonts");
    std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/../paneld-ui/fonts/inter-regular.ttf"), dir.join("f.ttf")).unwrap();
    let page = |n: u32| {
        format!(
            "var a: int = {n}\npage home {{ text {{ value = \"a \u{2192}\"; font = roboto {} }} \
             text {{ value = b; font = f.ttf {} }} text {{ value = c; font = noto_sans {} bold }} }}\n",
            10 + n,
            10 + n,
            10 + n
        )
    };
    let d = Daemon::start(&dir, &page(0));
    // 4 fonts a config (roboto falls back to montserrat), 40 configs:
    // past the cap of 32 unless each reload frees the last one's.
    for n in 1..=40 {
        write_conf(&dir, &page(n));
        assert_eq!(d.ok(&["reload"]), "reloaded");
    }
    assert_eq!(d.eval("vars.a"), "40");
    assert!(!d.log().contains("fonts in use"), "{}", d.log());
}

#[test]
fn a_broken_theme_folder_keeps_the_last_good_config() {
    let dir = tmp("theme-dir");
    let theme = dir.join("themes/glass/theme.conf");
    std::fs::create_dir_all(theme.parent().unwrap()).unwrap();
    std::fs::write(&theme, "theme glass {\n  accent = #123456\n}\n").unwrap();
    let d = Daemon::start(&dir, "screen:theme = glass\npage home { text { value = x } }\n");
    assert_eq!(d.eval("theme.accent"), "#123456");

    std::fs::write(&theme, "theme glass {\n  accent = #654321\n}\n").unwrap();
    assert_eq!(d.ok(&["reload"]), "reloaded");
    assert_eq!(d.eval("theme.accent"), "#654321");

    // Broken: refused with the theme file and line; the old theme stays.
    std::fs::write(&theme, "theme glass {\n  accent = #654321\n  font = fonts/none.ttf 18\n}\n").unwrap();
    let why = d.refused(&["reload"]);
    assert!(why.contains("themes/glass/theme.conf:3:") && why.contains("not found"), "{why}");
    assert_eq!(d.eval("theme.accent"), "#654321");
}

#[test]
fn ctl_switches_the_theme_until_reset_and_over_reloads() {
    let dir = tmp("ctl-theme");
    let theme = dir.join("themes/glass/theme.conf");
    std::fs::create_dir_all(theme.parent().unwrap()).unwrap();
    // `glass` is named nowhere in the config: a folder is enough.
    std::fs::write(&theme, "theme glass { extends = dark\n accent = #44aaff }\n").unwrap();
    let d = Daemon::start(&dir, "page home { text { value = x } }\n");
    // No theme set: the shipped `light`.
    assert_eq!(d.ok(&["theme"]), "light");
    assert_eq!(d.eval("theme.background"), "#f5f5f5");
    // The shipped `dark`, by name.
    assert_eq!(d.ok(&["theme", "dark"]), "dark");
    assert_eq!(d.eval("theme.background"), "#121212");
    // A folder theme; it extends the shipped one.
    assert_eq!(d.ok(&["theme", "glass"]), "glass");
    assert_eq!(d.eval("theme.accent"), "#44aaff");
    assert_eq!(d.eval("theme.background"), "#121212");
    assert!(d.ok(&["status"]).contains("\"theme\":\"glass\""));
    // An unknown one is refused; the current one stays.
    let why = d.refused(&["theme", "nope"]);
    assert!(why.contains("no theme `nope`"), "{why}");
    assert_eq!(d.eval("theme.accent"), "#44aaff");
    // Kept over a reload while the theme is still there.
    assert_eq!(d.ok(&["reload"]), "reloaded");
    assert_eq!(d.ok(&["theme"]), "glass");
    // Editing the theme folder's file reloads it, like the config.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(&theme, "theme glass { extends = dark\n accent = #00ff00 }\n").unwrap();
    d.wait_for("theme.accent", "#00ff00");
    // A new theme folder is found without a reload by hand, once its
    // file is there.
    let neu = dir.join("themes/neu");
    std::fs::create_dir_all(&neu).unwrap();
    std::thread::sleep(Duration::from_millis(700));
    std::fs::write(neu.join("theme.conf"), "theme neu { accent = #abcdef }\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while d.ctl(&["theme", "neu"]).status.code() != Some(0) {
        assert!(Instant::now() < deadline, "theme `neu` never loaded: {}", d.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(d.eval("theme.accent"), "#abcdef");
    // Back to the config's.
    assert_eq!(d.ok(&["theme", "--reset"]), "light");
    assert_eq!(d.eval("theme.background"), "#f5f5f5");
}

#[test]
fn a_ctl_theme_wins_over_the_night_theme_until_reset() {
    let dir = tmp("ctl-night");
    let h = local_hour();
    let d = Daemon::start(
        &dir,
        &format!(
            "screen {{ theme = light; night {{ from = {:02}:00; to = {:02}:00; theme = dark }} }}\npage home {{ text {{ value = x }} }}\n",
            (h + 23) % 24,
            (h + 2) % 24
        ),
    );
    d.wait_for("theme.background", "#121212");
    assert_eq!(d.ok(&["theme"]), "dark");
    assert_eq!(d.ok(&["theme", "light"]), "light");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(d.eval("theme.background"), "#f5f5f5");
    // Reset: the night hours have it again.
    assert_eq!(d.ok(&["theme", "--reset"]), "dark");
    assert_eq!(d.eval("theme.background"), "#121212");
}

#[test]
fn theme_switches_free_the_fonts_of_the_old_theme() {
    let dir = tmp("theme-fonts");
    for n in 0..10u32 {
        let t = dir.join(format!("themes/t{n}"));
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("theme.conf"),
            format!("theme t{n} {{ font = inter {}; font_big = noto_sans {} bold; font_huge = montserrat {} }}\n", 12 + n, 20 + n, 30 + n),
        )
        .unwrap();
    }
    let d = Daemon::start(
        &dir,
        "page home { column { text { value = a }\n text { value = b; font = font_big }\n text { value = c; font = font_huge } } }\n",
    );
    // 3 or 4 fonts a theme, 40 switches: past the cap of 32 unless each
    // switch frees the last theme's.
    for i in 0..40u32 {
        assert_eq!(d.ok(&["theme", &format!("t{}", i % 10)]), format!("t{}", i % 10));
        // Let the redraw happen.
        d.eval("1");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!d.log().contains("fonts in use"), "{}", d.log());
}

#[test]
fn a_display_that_does_not_open_falls_back_and_says_where() {
    let dir = tmp("display");
    let conf = "display:backend = fbdev\ndisplay:device = /nonexistent/fb\npage home { text { value = x } }\n";
    let d = Daemon::start(&dir, conf);
    // The next backend opened; the config's display is an alert, with
    // the file and line of `backend`.
    d.wait_for("alerts.count", "1");
    let text = d.eval("alerts.active[0].text");
    assert!(text.contains("p.conf:3:") && text.contains("fbdev on /nonexistent/fb"), "{text}");
    assert!(d.log().contains("display: fbdev on /nonexistent/fb did not open"), "{}", d.log());
    // A reload keeps the alert and says that display keys need a restart.
    write_conf(&dir, &format!("display:rotation = 90\n{conf}"));
    assert_eq!(d.ok(&["reload"]), "reloaded");
    assert_eq!(d.eval("alerts.count"), "1");
    assert!(d.log().contains("display: rotation changed; restart paneld to apply"), "{}", d.log());
}

/// A number field of `ctl --json status`.
fn status_num(d: &Daemon, field: &str) -> i64 {
    let s = d.ok(&["--json", "status"]);
    let at = s.find(&format!("\"{field}\":")).unwrap_or_else(|| panic!("no `{field}` in {s}"));
    let rest = &s[at + field.len() + 3..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
    rest[..end].parse().unwrap()
}

/// Wait until nothing animates and the loop sleeps its full idle step.
fn wait_idle(d: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while (status_num(d, "animations"), status_num(d, "idle_ms")) != (0, 1000) {
        assert!(Instant::now() < deadline, "never idle: {}", d.ok(&["--json", "status"]));
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn animations_end_and_the_loop_sleeps_again_and_none_run_while_asleep() {
    let dir = tmp("motion");
    let log = dir.join("screen.log");
    let d = Daemon::start(
        &dir,
        &format!(
            "theme t {{ value_transition = 600ms; page_transition = fade 600ms }}\n\
             var v: int = 10\n\
             screen {{\n\
                 idle = 4s\n\
                 off = sh -c 'echo off >> \"$$0\"' {log}\n\
                 on = sh -c 'echo on >> \"$$0\"' {log}\n\
             }}\n\
             page home {{ nav = 1; bar {{ width = 200; value = {{{{ vars.v }}}} }} }}\n\
             page two {{ nav = 2; arc {{ value = {{{{ vars.v }}}} }} }}\n",
            log = log.display()
        ),
    );
    // Idle: the loop sleeps its whole idle step (IDLE_STEP_MS).
    wait_idle(&d);
    // A value change moves the bar: an animation, frames every 33 ms.
    d.ok(&["set", "vars.v=90"]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(status_num(&d, "animations") >= 1, "the bar did not animate");
    assert!(status_num(&d, "idle_ms") <= 33, "no frames while animating");
    // Then it ends and the loop sleeps as before.
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!((status_num(&d, "animations"), status_num(&d, "idle_ms")), (0, 1000));
    // The same for a page transition.
    d.ok(&["goto", "two"]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(status_num(&d, "animations") >= 1, "the page did not fade in");
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!((status_num(&d, "animations"), status_num(&d, "idle_ms")), (0, 1000));
    // Asleep: a change is not drawn; on waking it is drawn at its value
    // at once, with no animation (the banner of the waking alert too).
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while read() != "off\n" {
        assert!(Instant::now() < deadline, "the screen did not sleep: `{}`", read());
        std::thread::sleep(Duration::from_millis(100));
    }
    d.ok(&["set", "vars.v=20"]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!((status_num(&d, "animations"), status_num(&d, "idle_ms")), (0, 1000));
    d.ok(&["alert", "fire", "c", "severity=critical"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while read() != "off\non\n" {
        assert!(Instant::now() < deadline, "the screen did not wake: `{}`", read());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(status_num(&d, "animations"), 0, "something animated on waking");
    wait_idle(&d);
    // The banner is up; a theme switch or a reload (lines moved) does
    // not bring it in again.
    d.ok(&["theme", "dark"]);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(status_num(&d, "animations"), 0, "a theme switch replayed something");
    wait_idle(&d);
    let conf = std::fs::read_to_string(dir.join("p.conf")).unwrap();
    std::fs::write(dir.join("p.conf"), format!("# moved\n# down\n{conf}")).unwrap();
    d.ok(&["reload"]);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(status_num(&d, "animations"), 0, "a reload replayed something");
    wait_idle(&d);
}

#[test]
fn the_clock_redraws_only_pages_that_read_it_and_as_often_as_they_read() {
    let dir = tmp("clock");
    let d = Daemon::start(
        &dir,
        "page plain { nav = 1; text { value = no clock } }\n\
         page minutes { nav = 2; text { value = {{ now.weekday }}, {{ now.date }} {{ now.time }} } }\n\
         page seconds { nav = 3; text { value = {{ now.time }}:{{ now.second }} } }\n",
    );
    // `now` is the daemon's clock, in the same zone everywhere.
    let unix: i64 = d.eval("now.unix").parse().unwrap();
    let mine = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((unix - mine).abs() <= 2, "now.unix {unix}, the clock {mine}");
    assert_eq!(d.eval("ago(now.unix)"), "just now");
    assert_eq!(d.eval("size(now.iso_date) == 10 && size(now.time) == 5"), "true");
    let renders = || status_num(&d, "renders");
    // A page without the clock: not redrawn as time passes, and the
    // loop sleeps its whole idle step.
    wait_idle(&d);
    let before = renders();
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(renders(), before, "the clock redrew a page that does not read it");
    // Minutes: not redrawn within a minute (unless one ended meanwhile).
    d.ok(&["goto", "minutes"]);
    std::thread::sleep(Duration::from_millis(200));
    let (minute, before) = (d.eval("now.minute"), renders());
    std::thread::sleep(Duration::from_millis(2_000));
    let after = renders();
    if d.eval("now.minute") == minute {
        assert_eq!(after, before, "a page that reads the minute was redrawn within it");
    } else {
        assert_eq!(after, before + 1, "a new minute redraws the page once");
    }
    // Seconds: redrawn each second.
    d.ok(&["goto", "seconds"]);
    std::thread::sleep(Duration::from_millis(200));
    let before = renders();
    std::thread::sleep(Duration::from_millis(3_000));
    let n = renders() - before;
    assert!((2..=4).contains(&n), "{n} redraws in 3 s of a seconds clock");
    d.stop();
}
