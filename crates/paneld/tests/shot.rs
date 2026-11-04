//! End-to-end UI checks: run the real `paneld shot` (a separate
//! process per test, since LVGL initializes once per process), decode
//! the PNG and look at what was drawn.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static N: AtomicUsize = AtomicUsize::new(0);

fn tmp(label: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "shot-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Image {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

impl Image {
    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    fn any(&self, f: impl Fn([u8; 3]) -> bool) -> bool {
        (0..self.h).any(|y| (0..self.w).any(|x| f(self.px(x, y))))
    }
}

/// Decoder for exactly what `paneld shot` writes: RGB8, filter 0,
/// zlib with stored blocks only.
fn decode(png: &[u8]) -> Image {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let (mut w, mut h, mut idat) = (0usize, 0usize, Vec::new());
    let mut i = 8;
    while i < png.len() {
        let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
        let kind = &png[i + 4..i + 8];
        let data = &png[i + 8..i + 8 + len];
        match kind {
            b"IHDR" => {
                w = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                h = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
            }
            b"IDAT" => idat.extend_from_slice(data),
            _ => {}
        }
        i += 12 + len;
    }
    let mut raw = Vec::new();
    let mut p = 2; // zlib header
    loop {
        let last = idat[p] & 1 == 1;
        let len = u16::from_le_bytes([idat[p + 1], idat[p + 2]]) as usize;
        raw.extend_from_slice(&idat[p + 5..p + 5 + len]);
        p += 5 + len;
        if last {
            break;
        }
    }
    let row = w * 3;
    let mut rgb = Vec::with_capacity(row * h);
    for y in 0..h {
        let start = y * (row + 1);
        assert_eq!(raw[start], 0, "filter type");
        rgb.extend_from_slice(&raw[start + 1..start + 1 + row]);
    }
    Image { w, h, rgb }
}

fn shot(config: &Path, extra: &[&str]) -> Image {
    let out = config.with_extension("png");
    let status = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .arg("shot")
        .arg("-c")
        .arg(config)
        .arg("-o")
        .arg(&out)
        .args(extra)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("run paneld shot");
    assert!(status.success(), "paneld shot failed: {status}");
    decode(&std::fs::read(&out).unwrap())
}

fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
    a.iter().zip(b).all(|(&x, y)| (x as i32 - y as i32).abs() <= tol)
}

#[test]
fn shot_has_the_requested_size() {
    let d = tmp("size");
    let conf = d.join("p.conf");
    std::fs::write(&conf, "page home { text { value = hi } }\n").unwrap();
    let img = shot(&conf, &["--size", "320x200", "--settle", "50"]);
    assert_eq!((img.w, img.h), (320, 200));
}

#[test]
fn component_number_param_drives_the_warning_color() {
    // The docs example: a literal `warn = 70` arg must compare as a
    // number and pick the built-in `theme.warning` color.
    let d = tmp("warn");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "component tile {\n\
             param value: number\n\
             param warn: number = 1e9\n\
             text { value = {{ string(value) }}; font = montserrat 32\n\
                    color = {{ value > warn ? theme.warning : theme.text }} }\n\
         }\n\
         page home { tile { value = 80; warn = 70 } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    assert!(
        img.any(|p| close(p, [0xef, 0x6c, 0x00], 24)),
        "no warning-colored pixels drawn"
    );
}

#[test]
fn grid_places_repeated_children_in_separate_columns() {
    // Three tiles in a 3-column grid. Before the fix every child sat in
    // cell (0, 0), so the right two thirds of the row stayed empty.
    let d = tmp("grid");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             grid {\n\
                 columns = 3\n\
                 repeat n in {{ [1, 2, 3] }} {\n\
                     column { background = #000000; height = 60\n\
                              text { value = x } }\n\
                 }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--size", "600x200", "--settle", "50"]);
    let dark_in = |x0: usize, x1: usize| {
        (0..img.h).any(|y| (x0..x1).any(|x| close(img.px(x, y), [0, 0, 0], 8)))
    };
    assert!(dark_in(30, 170), "first column empty");
    assert!(dark_in(230, 370), "second column empty");
    assert!(dark_in(430, 570), "third column empty");
}

#[test]
fn a_tap_runs_the_inline_set_and_re_renders() {
    let d = tmp("tap");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var on: bool = false\n\
         page home {\n\
             button { width = 200; height = 100; label = go\n\
                      on_tap { set { on = true } } }\n\
             rect { width = 200; height = 100\n\
                    background = {{ vars.on ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let green = |p| close(p, [0, 0xff, 0], 16);
    let before = shot(&conf, &["--settle", "50"]);
    assert!(!before.any(green), "green before the tap");
    // The button sits below the shell header (about 50 px).
    let after = shot(&conf, &["--tap", "100,120", "--settle", "100"]);
    assert!(after.any(green), "tap did not set the var and re-render");
}

#[test]
fn size_props_take_theme_tokens_and_expressions() {
    // `theme.tap_min` is 48 in the built-in theme; height is computed.
    let d = tmp("tokens");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             rect { width = {{ theme.tap_min }}; height = {{ 30 + 20 }}\n\
                    background = #0000ff }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let blue = |x: usize, y: usize| close(img.px(x, y), [0, 0, 0xff], 8);
    let widest = (0..img.h)
        .map(|y| (0..img.w).filter(|&x| blue(x, y)).count())
        .max()
        .unwrap();
    let tallest = (0..img.w)
        .map(|x| (0..img.h).filter(|&y| blue(x, y)).count())
        .max()
        .unwrap();
    // Border and rounded corners eat a few pixels of the fill.
    assert!((40..=48).contains(&widest), "width {widest}");
    assert!((42..=50).contains(&tallest), "height {tallest}");
}

#[test]
fn shell_shows_the_alert_banner_and_switches_pages() {
    let d = tmp("shell");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "source cpu { poll = 30s; run = /bin/echo 90; parse = number }\n\
         alert hot { when = {{ sources.cpu.value > 80 }}; severity = critical; title = Hot }\n\
         page home { title = One; nav = 1\n\
                     rect { width = 100; height = 40; background = #00ff00 } }\n\
         page two { title = Two; nav = 2\n\
                    rect { width = 100; height = 40; background = #0000ff } }\n",
    )
    .unwrap();
    let critical = |p| close(p, [0xd3, 0x2f, 0x2f], 16);
    let green = |p| close(p, [0, 0xff, 0], 16);
    let blue = |p| close(p, [0, 0, 0xff], 16);

    let first = shot(&conf, &["--settle", "100"]);
    assert!(first.any(critical), "no critical alert banner");
    assert!(first.any(green) && !first.any(blue), "not on page one");

    // Nav buttons sit evenly spaced in the bottom bar; the second is
    // in the right half of the screen.
    let second = shot(&conf, &["--tap", "546,450", "--settle", "100"]);
    assert!(second.any(blue) && !second.any(green), "nav did not switch to page two");
}

#[test]
fn action_goto_and_done_failed_toasts() {
    let d = tmp("done");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "action good { run = /bin/true; done = ok; goto = two }\n\
         action bad { run = /bin/false }\n\
         page one {\n\
             button { width = 200; height = 60; label = good; on_tap = good }\n\
             button { width = 200; height = 60; label = bad; on_tap = bad }\n\
         }\n\
         page two { rect { width = 100; height = 40; background = #0000ff } }\n",
    )
    .unwrap();
    let ok_green = |p| close(p, [0x2e, 0x7d, 0x32], 12);
    let failed_red = |p| close(p, [0xd3, 0x2f, 0x2f], 12);
    let blue = |p| close(p, [0, 0, 0xff], 16);

    // Buttons stack below the shell header: good at y ~65..125,
    // bad at y ~133..193.
    let good = shot(&conf, &["--tap", "100,95", "--settle", "300"]);
    assert!(good.any(blue), "goto did not switch to page two");
    assert!(good.any(ok_green), "no success toast");

    let bad = shot(&conf, &["--tap", "100,163", "--settle", "300"]);
    assert!(bad.any(failed_red), "no failure toast");
    assert!(!bad.any(blue), "a failed action must not goto");
}

#[test]
fn a_toast_lets_taps_through() {
    // Review round 6: the toast pill swallowed taps for 3 s.
    let d = tmp("toast");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var hit: bool = false\n\
         action bad { run = /bin/false }\n\
         page one {\n\
             rect { width = 100; height = 20; background = {{ vars.hit ? '#00ff00' : '#ff0000' }} }\n\
             button { width = 200; height = 40; label = bad; on_tap = bad }\n\
             button { width = 100%; height = 300; label = under; on_tap { set { hit = true } } }\n\
         }\n",
    )
    .unwrap();
    let green = |p| close(p, [0, 0xff, 0], 16);
    let failed_red = |p| close(p, [0xd3, 0x2f, 0x2f], 12);
    let toast = shot(&conf, &["--tap", "100,113", "--settle", "300"]);
    assert!(toast.any(failed_red), "no failure toast to tap through");
    // The pill sits at the bottom middle, over the big button.
    let through = shot(&conf, &["--tap", "100,113", "--settle", "300", "--tap", "400,386", "--settle", "150"]);
    assert!(through.any(green), "the tap on the toast did not reach the button under it");
}

#[test]
fn inline_handler_confirm_gates_its_set() {
    // Review round 5: `on_tap { set { ... }; confirm = ... }` ran the
    // set without asking.
    let d = tmp("inline");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var on: bool = false\n\
         page home {\n\
             button { width = 200; height = 60; label = go\n\
                      on_tap { set { on = true }; confirm = Sure? } }\n\
             rect { width = 100; height = 40\n\
                    background = {{ vars.on ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let green = |p| close(p, [0, 0xff, 0], 16);
    // Tap only: the confirm modal is up, nothing set yet.
    let asked = shot(&conf, &["--tap", "100,95", "--settle", "100"]);
    assert!(!asked.any(green), "set ran before the confirm");
    // Tap, then OK (right footer button of the centered modal).
    let done = shot(&conf, &["--tap", "100,95", "--tap", "463,282", "--settle", "200"]);
    assert!(done.any(green), "set did not run after OK");
}

#[test]
fn on_change_handlers_go_through_the_gates() {
    // Review round 6: `on_change { set; pin }` set the var with no PIN.
    let d = tmp("change");
    let write = |handler: &str| {
        let conf = d.join(format!("{}.conf", handler.len()));
        std::fs::write(
            &conf,
            format!(
                "var on: bool = false\n\
                 page home {{\n\
                     toggle {{ on_change {{ set {{ on = {{{{ event.value }}}} }}; {handler} }} }}\n\
                     rect {{ width = 100; height = 40\n\
                            background = {{{{ vars.on ? '#00ff00' : '#ff0000' }}}} }}\n\
                 }}\n"
            ),
        )
        .unwrap();
        conf
    };
    let green = |p| close(p, [0, 0xff, 0], 16);
    let red = |p| close(p, [0xff, 0, 0], 16);
    // No PIN is configured, so a PIN-gated call is refused outright;
    // dismiss the notice and look at the page.
    let pinned = shot(&write("pin = true"), &["--tap", "38,77", "--tap", "399,269", "--settle", "150"]);
    assert!(pinned.any(red) && !pinned.any(green), "set ran without the PIN");
    let conf = write("confirm = Sure?");
    let asked = shot(&conf, &["--tap", "38,77", "--settle", "150"]);
    assert!(!asked.any(green), "set ran before the confirm");
    let done = shot(&conf, &["--tap", "38,77", "--tap", "463,282", "--settle", "200"]);
    assert!(done.any(green), "set did not run after OK, or event.value was lost");
}

#[test]
fn acknowledging_takes_an_alert_off_the_banner() {
    let d = tmp("ack");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "alert hot { when = {{ true }}; severity = critical; title = Hot }\n\
         page home { text { value = body } }\n",
    )
    .unwrap();
    let critical = |p| close(p, [0xd3, 0x2f, 0x2f], 16);
    // The banner (just under the header) opens the alerts page; its
    // first card's first button is Acknowledge.
    let before = shot(&conf, &["--tap", "400,72", "--settle", "100"]);
    assert!(before.any(critical), "no critical alert drawn");
    let after = shot(&conf, &["--tap", "400,72", "--tap", "100,145", "--settle", "100"]);
    // The title text stays critical-colored; the full-width banner must
    // be gone: check a pixel in the banner row away from any text.
    assert!(close(before.px(700, 72), [0xd3, 0x2f, 0x2f], 16), "no banner before the ack");
    assert!(!close(after.px(700, 72), [0xd3, 0x2f, 0x2f], 16), "banner still shown after ack");
}

#[test]
fn a_due_reminder_is_an_alert_until_done_and_snooze_1w_works() {
    let d = tmp("reminder");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "reminder restore { once = 2020-01-01; title = Test a restore; severity = warning }\n\
         page home { text { value = body } }\n",
    )
    .unwrap();
    let warning = [0xef, 0x6c, 0x00];
    let banner = |img: &Image| close(img.px(700, 72), warning, 16);
    // Banner opens the alerts page: Done, Snooze 1d, Snooze 1w.
    let before = shot(&conf, &["--tap", "400,72", "--settle", "100"]);
    assert!(banner(&before), "no banner for a due reminder");
    let done = shot(&conf, &["--tap", "400,72", "--tap", "67,163", "--settle", "100"]);
    assert!(!done.any(|p| close(p, warning, 16)), "reminder still listed after Done");
    let snoozed = shot(&conf, &["--tap", "400,72", "--tap", "307,163", "--settle", "100"]);
    assert!(!banner(&snoozed), "banner still shown after Snooze 1w");
    assert!(snoozed.any(|p| close(p, warning, 16)), "snoozed reminder must stay listed");
}

#[test]
fn builtin_actions_take_the_gates_of_an_override() {
    // Review round 6: `action[alert_ack]:pin|confirm` was ignored.
    let d = tmp("ackgate");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "alert hot { when = {{ true }}; severity = critical; title = Hot }\n\
         action[alert_ack]:confirm = Acknowledge?\n\
         page home { text { value = body } }\n",
    )
    .unwrap();
    let banner = |img: &Image| close(img.px(700, 72), [0xd3, 0x2f, 0x2f], 16);
    // Acknowledge, then Cancel: still on the banner.
    let cancelled = shot(&conf, &["--tap", "400,72", "--tap", "100,145", "--tap", "351,282", "--settle", "150"]);
    assert!(banner(&cancelled), "ack ran without the confirm");
    let ok = shot(&conf, &["--tap", "400,72", "--tap", "100,145", "--tap", "463,282", "--settle", "150"]);
    assert!(!banner(&ok), "ack did not run after OK");
}

#[test]
fn a_resolved_alert_is_listed_under_resolved() {
    let d = tmp("history");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var hot: bool = true\n\
         alert hot { when = {{ vars.hot }}; severity = warning; title = Hot }\n\
         action cool { set { hot = false }; goto = alerts }\n\
         page home { button { label = cool; on_tap = cool } }\n",
    )
    .unwrap();
    let warning = |p| close(p, [0xef, 0x6c, 0x00], 16);
    // Banner opens the alerts page while it fires.
    let firing = shot(&conf, &["--tap", "400,72", "--settle", "150"]);
    assert!(firing.any(warning), "no firing alert");
    // Cool down (the button sits under the banner); the action then
    // opens the alerts page.
    let resolved = shot(&conf, &["--tap", "40,125", "--settle", "200"]);
    assert!(!resolved.any(warning), "alert still drawn as firing");
    let text = |p: [u8; 3]| p[0] < 0x40 && p[1] < 0x40 && p[2] < 0x40;
    // The `Resolved` list draws text below the empty-state line.
    assert!((100..200).any(|y| (0..300).any(|x| text(resolved.px(x, y)))), "no resolved entry drawn");
}

#[test]
fn chart_draws_each_series_in_its_color() {
    let d = tmp("chart");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home { chart { width = 100%; height = 200\n\
             series { values = {{ [1, 3, 2, 5] }}; color = #00ff00 }\n\
             series { values = {{ [{'value': 4}, {'value': 1}] }}; color = #ff0000 } } }\n",
    )
    .unwrap();
    let img = shot(&conf, &[]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 40)), "no green series");
    assert!(img.any(|p| close(p, [0xff, 0, 0], 40)), "no red series from {{value}} maps");
    // The red one has 2 of 4 points: drawn against the right edge only.
    let red_x: Vec<usize> = (0..img.w).filter(|&x| (60..270).any(|y| close(img.px(x, y), [0xff, 0, 0], 40))).collect();
    assert!(red_x.first().is_some_and(|&x| x > img.w / 3), "red series starts at x {:?}", red_x.first());
}

#[test]
fn image_reads_a_png_next_to_the_config_and_line_draws() {
    let d = tmp("image");
    // A PNG of our own: a shot of a magenta page.
    let src = d.join("src.conf");
    std::fs::write(&src, "page home { rect { width = 100%; height = 100%; background = #ff00ff } }\n").unwrap();
    shot(&src, &["--size", "120x80"]);
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             image { src = src.png; width = 240; height = 160; fit = fill }\n\
             line { points = {{ [[0, 0], [200, 30]] }}; thickness = 6; color = #00ff00; width = 210; height = 40 }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &[]);
    let magenta = |p| close(p, [0xff, 0, 0xff], 40);
    assert!((60..230).any(|y| (0..260).any(|x| magenta(img.px(x, y)))), "image not drawn");
    assert!(img.any(|p| close(p, [0, 0xff, 0], 40)), "line not drawn");

    // Review round 8: a screen-sized PNG decodes past the old 1 MiB
    // cache and was not drawn; a huge one must be refused unread.
    shot(&src, &["--size", "800x480"]);
    let mut huge = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    huge.extend(8000u32.to_be_bytes());
    huge.extend(8000u32.to_be_bytes());
    std::fs::write(d.join("huge.png"), huge).unwrap();
    let conf = d.join("q.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             image { src = src.png; width = 100%; height = 200; fit = fill }\n\
             image { src = huge.png; width = 100; height = 100 }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &[]);
    assert!((60..260).any(|y| magenta(img.px(400, y))), "screen-sized image not drawn");
}

#[test]
fn a_param_is_asked_for_and_checked() {
    let d = tmp("ask");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var m: int = 0\n\
         action pause {\n\
             param minutes: int { ask = select; options = [5, 30]; max = 10 }\n\
             set { m = {{ minutes }} }\n\
         }\n\
         page home {\n\
             button { label = Pause; on_tap = pause }\n\
             rect { width = 100; height = 20\n\
                    background = {{ vars.m == 5 ? '#00ff00' : vars.m == 0 ? '#0000ff' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let green = |p| close(p, [0, 0xff, 0], 16);
    let blue = |p| close(p, [0, 0, 0xff], 16);
    // The dialog is up: nothing set yet.
    let asked = shot(&conf, &["--tap", "50,85", "--settle", "150"]);
    assert!(!asked.any(green) && !asked.any(blue), "no dialog over the page");
    // OK takes the first option, 5.
    let ok = shot(&conf, &["--tap", "50,85", "--settle", "150", "--tap", "463,295", "--settle", "200"]);
    assert!(ok.any(green), "5 was not set");
    // Cancel: nothing.
    let cancelled = shot(&conf, &["--tap", "50,85", "--settle", "150", "--tap", "351,295", "--settle", "200"]);
    assert!(cancelled.any(blue), "cancel changed the var");
    // 30 is an option but above `max`: refused, the notice says so.
    let refused = shot(&conf, &[
        "--tap", "50,85", "--settle", "150", "--tap", "400,240", "--settle", "150",
        "--tap", "400,327", "--settle", "150", "--tap", "463,295", "--settle", "200",
        "--tap", "399,269", "--settle", "150",
    ]);
    assert!(refused.any(blue), "a value above max was set");
}

#[test]
fn an_empty_keypad_is_asked_again() {
    // Review round 9: OK on an empty keypad ran the action with "".
    let d = tmp("askkeypad");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var m: int = 0\n\
         action setn { param n: int { ask = keypad }; set { m = {{ n }} } }\n\
         page home {\n\
             button { label = Set; on_tap = setn }\n\
             rect { width = 100; height = 20\n\
                    background = {{ vars.m == 4 ? '#00ff00' : '#0000ff' }} }\n\
         }\n",
    )
    .unwrap();
    let green = |p| close(p, [0, 0xff, 0], 16);
    let blue = |p| close(p, [0, 0, 0xff], 16);
    // Keyboard OK (bottom right) with nothing typed: still asking.
    let empty = shot(&conf, &["--tap", "40,85", "--settle", "150", "--tap", "662,445", "--settle", "200"]);
    assert!(!empty.any(blue) && !empty.any(green), "the dialog went away on an empty OK");
    // Then 4 and OK.
    let four = shot(&conf, &[
        "--tap", "40,85", "--settle", "150", "--tap", "662,445", "--settle", "200",
        "--tap", "136,331", "--tap", "662,445", "--settle", "200",
    ]);
    assert!(four.any(green), "4 was not set after asking again");
}

#[test]
fn idle_screen_goes_black_and_the_waking_tap_does_nothing_else() {
    let d = tmp("sleep");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var n: int = 0\n\
         screen { idle = 1s }\n\
         page home {\n\
             button { width = 200; height = 80; label = add\n\
                      on_tap { set { n = {{ vars.n + 1 }} } } }\n\
             rect { width = 100; height = 40\n\
                    background = {{ vars.n == 1 ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let black = |p| close(p, [0, 0, 0], 4);
    let green = |p| close(p, [0, 0xff, 0], 16);
    let asleep = shot(&conf, &["--settle", "100", "--wait", "1500"]);
    assert!((0..asleep.h).all(|y| black(asleep.px(400, y))), "screen not blacked out");
    // First tap wakes only; the second counts once.
    let woke = shot(&conf, &["--settle", "100", "--wait", "1500", "--tap", "100,100", "--tap", "100,100"]);
    assert!(!black(woke.px(400, 240)), "did not wake");
    assert!(woke.any(green), "the waking tap was not absorbed (or the second was lost)");
}

#[test]
fn theme_classes_call_props_min_height_and_scroll() {
    let d = tmp("styles");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t {\n\
             style .card { background = #3355aa; radius = 12; padding = 10 }\n\
             style .wide { width = 300 }\n\
         }\n\
         screen { theme = t }\n\
         component tile {\n\
             param label: string\n\
             column { class = card; text { value = {{ label }}; color = #ffffff } }\n\
         }\n\
         page home {\n\
             title = Home\n\
             row {\n\
                 gap = 8\n\
                 tile { label = plain }\n\
                 tile { label = called; class = wide; min_height = 120; background = #aa3333 }\n\
             }\n\
             scroll {\n\
                 height = 150\n\
                 repeat i in {{ [1,2,3,4,5,6,7,8,9,10] }} { rect { width = 200; height = 30; background = #44aa44 } }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    // `.card` from the theme.
    assert!(close(img.px(45, 110), [0x33, 0x55, 0xaa], 8), "{:?}", img.px(45, 110));
    // The call's `class`, `min_height` and `background` reach the root.
    assert!(close(img.px(380, 190), [0xaa, 0x33, 0x33], 8), "{:?}", img.px(380, 190));
    // The row itself is transparent: the page background shows.
    assert!(close(img.px(500, 110), [0xf5, 0xf5, 0xf5], 4), "{:?}", img.px(500, 110));
    // The scroll keeps its height: the rows past it are clipped.
    assert!(img.any(|p| close(p, [0x44, 0xaa, 0x44], 8)));
    assert!(!(0..800).any(|x| close(img.px(x, 440), [0x44, 0xaa, 0x44], 8)), "scroll overflowed");
}
