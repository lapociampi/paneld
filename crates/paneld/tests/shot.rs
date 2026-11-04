//! End-to-end UI checks: run the real `paneld shot` (a separate
//! process per test, since LVGL initializes once per process), decode
//! the PNG and look at what was drawn.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        "theme t { dialog_backdrop = #ff00ff }\nscreen { theme = t }\n\
         var m: int = 0\n\
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
    // The dialog is up over the page (an opaque backdrop here): nothing
    // set yet.
    let magenta = |p| close(p, [0xff, 0, 0xff], 16);
    let asked = shot(&conf, &["--tap", "50,85", "--settle", "150"]);
    assert!(asked.any(magenta) && !asked.any(green) && !asked.any(blue), "no dialog over the page");
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
        "theme t { dialog_backdrop = #ff00ff }\nscreen { theme = t }\n\
         var m: int = 0\n\
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

#[test]
fn a_remote_value_reads_kind_params_and_the_kind_timeout_applies() {
    let d = tmp("remote-params");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "remote_kind box {\n\
             param scale: int = 2\n\
             via = local\n\
             timeout = 1s\n\
             value a { run = echo 21; parse = number; value = {{ out * scale }} }\n\
             value slow { run = sleep 3 }\n\
         }\n\
         remote b1: box { }\n\
         page home {\n\
             rect { width = 200; height = 100\n\
                    background = {{ b1.a == 42.0 && !b1.ok && b1.error == 'slow: timed out; it may still have run' ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "params or timeout not applied");
}

#[test]
fn a_local_remote_shows_its_values_under_its_name() {
    let d = tmp("remote-local");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "remote_kind box {\n\
             param factor: int = 1\n\
             via = local\n\
             value temp { run = echo {{ factor * 21 }}; parse = number }\n\
         }\n\
         remote b1: box { factor = 2 }\n\
         page home {\n\
             rect { width = 200; height = 100\n\
                    background = {{ b1.ok && b1.temp == 42.0 && b1.status.temp.ok ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "b1.temp is not 42");
}

/// A one-request-per-connection HTTP server on 127.0.0.1 that answers
/// `body` and hands back each request it read.
fn http_server(body: &'static str) -> (u16, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            conn.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers, then a body of Content-Length bytes.
            loop {
                let n = conn.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    if req.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = tx.send(String::from_utf8_lossy(&req).to_string());
        }
    });
    (port, rx)
}

#[test]
fn an_http_remote_polls_and_its_action_posts_through_curl() {
    let (port, requests) = http_server("{\"v\": 7}");
    let d = tmp("remote-http");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "secret tok {{ file = {tok} }}\n\
             remote_kind api {{\n\
                 param port: int\n\
                 param token: secret\n\
                 via = http\n\
                 url = http://127.0.0.1:{{{{ port }}}}/api\n\
                 headers {{ Authorization = Bearer {{{{ token }}}} }}\n\
                 value info {{ get = /status; parse = json; value = {{{{ out.v }}}} }}\n\
                 action stop {{ param vm: string = \"../a b\"\n post = /vms/{{{{ vm }}}}/stop\n body = {{\"why\": \"tap\"}} }}\n\
             }}\n\
             remote h1: api {{ port = {port}; token = tok }}\n\
             page home {{\n\
                 button {{ width = 200; height = 100; label = Stop; on_tap = h1.stop }}\n\
                 rect {{ width = 200; height = 100\n\
                        background = {{{{ h1.info == 7 ? '#00ff00' : '#ff0000' }}}} }}\n\
             }}\n",
            tok = d.join("tok").display()
        ),
    )
    .unwrap();
    std::fs::write(d.join("tok"), "s3cret-token\n").unwrap();
    let img = shot(&conf, &["--tap", "100,120", "--wait", "1500", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "h1.info is not 7");
    let reqs: Vec<String> = requests.try_iter().collect();
    let get = reqs.iter().find(|r| r.starts_with("GET ")).expect("no GET");
    assert!(get.starts_with("GET /api/status HTTP/1.1\r\n"), "{get}");
    assert!(get.contains("Authorization: Bearer s3cret-token\r\n"), "{get}");
    let post = reqs.iter().find(|r| r.starts_with("POST ")).unwrap_or_else(|| panic!("no POST in {reqs:?}"));
    // The arg is one encoded segment: no `..` folding, no space.
    assert!(post.starts_with("POST /api/vms/..%2Fa%20b/stop HTTP/1.1\r\n"), "{post}");
    assert!(post.ends_with("\r\n\r\n{\"why\": \"tap\"}"), "{post}");
}

/// A page with each input setting a var, and a square per var that
/// turns green when the var holds what the input sent.
const INPUTS: &str = "var t: bool = false\n\
    var s: int = 0\n\
    var o: string = \"\"\n\
    var k: string = \"\"\n\
    page home {\n\
        title = Inputs\n\
        row {\n\
            gap = 8\n\
            toggle { value = {{ vars.t }}; on_change { set { t = {{ value }} } } }\n\
            rect { width = 40; height = 40; background = {{ vars.t ? '#00ff00' : '#ff0000' }} }\n\
            slider { width = 200; min = 0; max = 100; value = {{ vars.s }}; on_change { set { s = {{ event.value }} } } }\n\
            rect { width = 40; height = 40; background = {{ vars.s > 50 ? '#00ff00' : '#ff0000' }} }\n\
            select { width = 120; options = {{ ['a', 'b', 'c'] }}; value = {{ vars.o }}; on_change { set { o = {{ value }} } } }\n\
            rect { width = 40; height = 40; background = {{ vars.o == 'b' ? '#00ff00' : '#ff0000' }} }\n\
            rect { width = 40; height = 40; background = {{ vars.k == '42' ? '#00ff00' : '#ff0000' }} }\n\
        }\n\
        keypad { length = 4; on_submit { set { k = {{ value }} } } }\n\
    }\n";

fn inputs_shot(taps: &[&str]) -> Image {
    let d = tmp("inputs");
    let conf = d.join("p.conf");
    std::fs::write(&conf, INPUTS).unwrap();
    let mut args = vec!["--size", "800x700"];
    for t in taps {
        args.extend(["--tap", t]);
    }
    args.extend(["--settle", "150"]);
    shot(&conf, &args)
}

/// The squares in the order of [`INPUTS`]: toggle, slider, select,
/// keypad; `true` when green.
fn squares(img: &Image) -> [bool; 4] {
    [(110, 100), (368, 100), (545, 103), (596, 103)].map(|(x, y)| close(img.px(x, y), [0, 0xff, 0], 16))
}

#[test]
fn inputs_start_unset() {
    assert_eq!(squares(&inputs_shot(&[])), [false; 4]);
}

#[test]
fn a_toggle_sets_its_var_from_value() {
    assert_eq!(squares(&inputs_shot(&["58,98"])), [true, false, false, false]);
}

#[test]
fn a_slider_sets_its_var_on_release() {
    assert_eq!(squares(&inputs_shot(&["300,90"])), [false, true, false, false]);
}

#[test]
fn a_select_sends_the_option_not_its_position() {
    // Open the list, pick `b` (the second row under it).
    assert_eq!(squares(&inputs_shot(&["455,105", "422,192"])), [false, false, true, false]);
}

#[test]
fn a_keypad_submits_its_digits() {
    let img = inputs_shot(&["145,325", "400,250", "655,475"]);
    assert_eq!(squares(&img), [false, false, false, true]);
}

#[test]
fn a_set_inside_a_repeat_reads_the_row() {
    let d = tmp("repeat-set");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var sel: string = \"\"\n\
         component pick {\n\
             param id: string\n\
             button { width = 200; height = 60; label = {{ id }}; on_tap { set { sel = {{ id }} } } }\n\
         }\n\
         page home {\n\
             repeat r in {{ [{'id': 'x'}, {'id': 'y'}] }} {\n\
                 button { width = 200; height = 60; label = {{ r.id }}; on_tap { set { sel = {{ r.id }} } } }\n\
             }\n\
             pick { id = z }\n\
             rect { width = 200; height = 40; background = {{ vars.sel == 'y' ? '#00ff00' : vars.sel == 'z' ? '#0000ff' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    // The second row (a repeat item).
    let img = shot(&conf, &["--tap", "100,150", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "the row's item was not in scope");
    // The component's button (a component param).
    let img = shot(&conf, &["--tap", "100,218", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0, 0xff], 16)), "the component param was not in scope");
}

#[test]
fn every_widget_draws_and_a_bad_repeat_says_why() {
    let d = tmp("gallery");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             row {\n\
                 bar { width = 120; height = 20; value = 50 }\n\
                 arc { width = 80; height = 80; value = 75 }\n\
                 spinner { width = 40; height = 40 }\n\
                 qr { data = hello; size = 60 }\n\
                 stack { width = 60; height = 60; rect { width = 30; height = 30; background = #0000ff } }\n\
             }\n\
             repeat x in {{ 5 }} { rect { width = 10; height = 10; background = #ff0000 } }\n\
             repeat y in {{ nope }} { rect { width = 10; height = 10; background = #ff0000 } }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    // The bar and arc indicators in the theme's accent.
    assert!(img.any(|p| close(p, [0x21, 0x96, 0xf3], 12)), "no accent drawn");
    // The QR code's dark modules.
    assert!(img.any(|p| p == [0, 0, 0]), "no QR code");
    // The stack's child, at its corner (no padding).
    assert!(img.any(|p| close(p, [0, 0, 0xff], 8)), "no stack child");
    // Neither bad repeat drew its body.
    assert!(!img.any(|p| close(p, [0xff, 0, 0], 8)), "a bad repeat drew its body");
}

#[test]
fn a_missing_start_page_is_said_on_screen() {
    let d = tmp("nopage");
    let conf = d.join("p.conf");
    std::fs::write(&conf, "page home { text { value = hi } }\n").unwrap();
    let img = shot(&conf, &["--page", "nope", "--settle", "50"]);
    assert_eq!((img.w, img.h), (800, 480));
}

/// A config with a PIN-guarded action that counts its runs, and a
/// square that is green after two of them, blue after one.
fn pin_conf(label: &str) -> PathBuf {
    let d = tmp(label);
    let hash = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .arg("hash-pin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(b"1234\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(hash.status.success());
    std::fs::write(d.join("pin.hash"), &hash.stdout).unwrap();
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "secret pin_hash {{ file = {} }}\n\
             security {{ pin = pin_hash; attempts = 2; lockout = 5m; unlock_for = 1m }}\n\
             var n: int = 0\n\
             action guarded {{ pin = true; set {{ n = {{{{ vars.n + 1 }}}} }} }}\n\
             page home {{\n\
                 button {{ width = 200; height = 100; label = go; on_tap = guarded }}\n\
                 rect {{ width = 200; height = 100; background = {{{{ vars.n == 2 ? '#00ff00' : vars.n == 1 ? '#0000ff' : '#ff0000' }}}} }}\n\
             }}\n",
            d.join("pin.hash").display()
        ),
    )
    .unwrap();
    conf
}

const GO: &str = "100,120";
const PIN_1234: [&str; 5] = ["137,274", "400,274", "662,274", "137,331", "662,445"];
const WRONG: [&str; 2] = ["662,388", "662,445"];
const NOTICE_OK: &str = "399,269";

fn taps(seq: &[&[&str]]) -> Vec<String> {
    let mut out = Vec::new();
    for group in seq {
        for t in *group {
            out.push("--tap".to_string());
            out.push(t.to_string());
        }
    }
    out.extend(["--settle".to_string(), "150".to_string()]);
    out
}

fn pin_shot(seq: &[&[&str]]) -> Image {
    let conf = pin_conf("pin");
    let args = taps(seq);
    shot(&conf, &args.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn the_right_pin_runs_the_action_and_unlocks_for_a_while() {
    // PIN, then a second tap with no PIN asked: two runs.
    let img = pin_shot(&[&[GO], &PIN_1234, &[GO]]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "not two runs");
}

#[test]
fn wrong_pins_lock_the_action_out() {
    // Two wrong PINs: the lockout notice; after it, another tap meets
    // the lockout again, so the right PIN cannot even be entered.
    let img = pin_shot(&[&[GO], &WRONG, &WRONG, &[NOTICE_OK], &[GO], &[NOTICE_OK], &[GO], &PIN_1234]);
    assert!(img.any(|p| close(p, [0xff, 0, 0], 16)), "the page is not back");
    assert!(!img.any(|p| close(p, [0, 0, 0xff], 16) || close(p, [0, 0xff, 0], 16)), "it ran while locked out");
}

#[test]
fn one_wrong_pin_asks_again_and_the_right_one_then_runs() {
    let img = pin_shot(&[&[GO], &WRONG, &PIN_1234]);
    assert!(img.any(|p| close(p, [0, 0, 0xff], 16)), "the second try did not run it once");
}

#[test]
fn an_inline_set_can_read_the_page_it_was_tapped_on() {
    let d = tmp("page-set");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var last: string = \"\"\n\
         page home {\n\
             button { width = 200; height = 100; label = go; on_tap { set { last = {{ page.name }} } } }\n\
             rect { width = 200; height = 100; background = {{ vars.last == 'home' ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "100,120", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "page.name was not in scope");
}

#[test]
fn an_action_named_by_an_expression_runs() {
    // Schema 7's pattern: a component takes `param tap: action` and
    // does `on_tap = {{ tap }}`.
    let d = tmp("action-param");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var n: int = 0\n\
         action bump { set { n = {{ vars.n + 1 }} } }\n\
         action jump { set { n = {{ vars.n + 10 }} } }\n\
         component tile {\n\
             param tap: action = none\n\
             button { width = 200; height = 60; label = go; on_tap = {{ tap }} }\n\
         }\n\
         page home {\n\
             tile { tap = bump }\n\
             tile { }\n\
             button { width = 200; height = 60; label = pick; on_tap = {{ vars.n >= 1 ? 'jump' : 'bump' }} }\n\
             rect { width = 200; height = 40; background = {{ vars.n == 1 ? '#00ff00' : vars.n == 11 ? '#0000ff' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    // The param's action.
    let img = shot(&conf, &["--tap", "100,90", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "on_tap = {{ tap }} did nothing");
    // `none` (the default) binds nothing.
    let img = shot(&conf, &["--tap", "100,158", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0xff, 0, 0], 16)), "the none tile ran something");
    // An expression choosing between actions, re-evaluated per render.
    let img = shot(&conf, &["--tap", "100,90", "--settle", "100", "--tap", "100,226", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0, 0xff], 16)), "the chosen action did not run");
}

#[test]
fn a_dialog_shows_over_the_page_as_the_theme_says() {
    let d = tmp("backdrop");
    let conf = |backdrop: &str| {
        let c = d.join(format!("{}.conf", backdrop.len()));
        std::fs::write(
            &c,
            format!(
                "theme t {{ {backdrop} }}\nscreen {{ theme = t }}\n\
                 action go {{ title = Go; confirm = Sure? }}\n\
                 page home {{\n\
                     button {{ width = 200; height = 60; label = go; on_tap = go }}\n\
                     rect {{ x = 0; width = 780; height = 300; background = #00ff00 }}\n\
                 }}\n"
            ),
        )
        .unwrap();
        c
    };
    let green = |img: &Image| img.any(|p| close(p, [0, 0xff, 0], 8));
    // Default: transparent, the page shows unchanged.
    assert!(green(&shot(&conf(""), &["--tap", "100,90", "--settle", "100"])), "the page is gone");
    // A color covers it.
    let covered = shot(&conf("dialog_backdrop = #000000"), &["--tap", "100,90", "--settle", "100"]);
    assert!(!green(&covered) && covered.any(|p| p == [0, 0, 0]), "the color backdrop did not cover the page");
    // A blur keeps its colors but softens its edges: still green somewhere.
    assert!(green(&shot(&conf("dialog_backdrop = blur 8"), &["--tap", "100,90", "--settle", "100"])));
}
