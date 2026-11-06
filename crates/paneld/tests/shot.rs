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
    let run = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .arg("shot")
        .arg("-c")
        .arg(config)
        .arg("-o")
        .arg(&out)
        .args(extra)
        // ADR 0024: every update in place is checked against a fresh
        // render (exit 3 and the first difference when they differ).
        .env("PANELD_VERIFY_UPDATES", "1")
        .output()
        .expect("run paneld shot");
    assert!(run.status.success(), "paneld shot failed: {}\n{}", run.status, String::from_utf8_lossy(&run.stderr));
    // LVGL logs with printf, on stdout: an error there is a bug even
    // when the picture looks right.
    let log = String::from_utf8_lossy(&run.stdout);
    assert!(!log.contains("[Error]"), "LVGL logged an error:\n{log}");
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
fn a_param_default_reads_the_params_before_it() {
    // `b` defaults from `a` as passed (5), not as defaulted (1) and
    // not null: a rect 5 * 20 = 100 px wide in the warning color.
    let d = tmp("param-order");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "component c {\n\
             param a: int = 1\n\
             param b: int = {{ a * 20 }}\n\
             rect { width = {{ b }}; height = 40; background = {{ theme.warning }} }\n\
         }\n\
         page home { c { a = 5 } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--size", "400x200", "--settle", "50"]);
    let warn = |p| close(p, [0xef, 0x6c, 0x00], 24);
    let width = (0..img.h).map(|y| (0..img.w).filter(|&x| warn(img.px(x, y))).count()).max().unwrap_or(0);
    assert!((96..=104).contains(&width), "rect is {width} px wide, want 100");
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
    let second = shot(&conf, &["--tap", "546,450", "--settle", "300"]);
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
fn a_named_on_change_call_reads_the_new_value_in_its_args() {
    // `level = {{ value }}` is evaluated when the toggle flips, with
    // its new state, not when the page is drawn (no value then), and
    // still sees `page` where it was drawn.
    let d = tmp("late-arg");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var on: bool = false\n\
         action set_on { param level: bool\n param where: string\n run = /bin/true\n set { on = {{ level && where == 'home' }} } }\n\
         page home {\n\
             toggle { on_change set_on { level = {{ value && event.value }}; where = {{ value ? page.name : '' }} } }\n\
             rect { width = 100; height = 40; background = {{ vars.on ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "38,77", "--settle", "300"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "the arg did not get the new value");
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
    let before = shot(&conf, &["--tap", "400,72", "--settle", "300"]);
    assert!(before.any(critical), "no critical alert drawn");
    let after = shot(&conf, &["--tap", "400,72", "--tap", "100,145", "--settle", "300"]);
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
    let before = shot(&conf, &["--tap", "400,72", "--settle", "300"]);
    assert!(banner(&before), "no banner for a due reminder");
    let done = shot(&conf, &["--tap", "400,72", "--tap", "67,163", "--settle", "300"]);
    assert!(!done.any(|p| close(p, warning, 16)), "reminder still listed after Done");
    let snoozed = shot(&conf, &["--tap", "400,72", "--tap", "307,163", "--settle", "300"]);
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
    let cancelled = shot(&conf, &["--tap", "400,72", "--tap", "100,145", "--tap", "351,282", "--settle", "300"]);
    assert!(banner(&cancelled), "ack ran without the confirm");
    let ok = shot(&conf, &["--tap", "400,72", "--tap", "100,145", "--tap", "463,282", "--settle", "300"]);
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
    shot(&src, &["--size", "120x120"]);
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
                        background = {{{{ h1.info == 7 && h1.status.info.status == 200 && h1.status.info.stderr == '' ? '#00ff00' : '#ff0000' }}}} }}\n\
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
             repeat y in {{ [1][5] }} { rect { width = 10; height = 10; background = #ff0000 } }\n\
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
fn a_button_runs_its_action() {
    // `action` on a button: a name, or `{{ tap }}` from a param; an
    // `on_tap` on the same button wins over it.
    let d = tmp("button-action");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var n: int = 0\n\
         action bump { set { n = {{ vars.n + 1 }} } }\n\
         action jump { set { n = {{ vars.n + 10 }} } }\n\
         component big {\n\
             param tap: action = none\n\
             button { width = 200; height = 60; label = go; action = {{ tap }} }\n\
         }\n\
         page home {\n\
             big { tap = bump }\n\
             button { width = 200; height = 60; label = jump; action = jump }\n\
             button { width = 200; height = 60; label = both; action = jump; on_tap = bump }\n\
             rect { width = 200; height = 40; background = {{ vars.n == 1 ? '#00ff00' : vars.n == 10 ? '#0000ff' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "100,90", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "action = {{ tap }} did nothing");
    let img = shot(&conf, &["--tap", "100,158", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0, 0xff], 16)), "action = jump did nothing");
    let img = shot(&conf, &["--tap", "100,226", "--settle", "100"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "on_tap did not win over action");
}

#[test]
fn a_button_shows_its_call_running_and_failed() {
    // `button:running` while the call runs, `button:failed` after it
    // failed, neither once it succeeds; a row button matches its args.
    let d = tmp("button-running");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t {\n\
             style button:running { background = #00ff00 }\n\
             style button:failed { background = #0000ff }\n\
         }\n\
         screen { theme = t }\n\
         action slow { param v: int = 0; run = sleep 2 }\n\
         action bad { run = false }\n\
         action good { run = true }\n\
         page home {\n\
             button { width = 200; height = 60; label = a; on_tap slow { v = 1 } }\n\
             button { width = 200; height = 60; label = b; on_tap slow { v = 2 } }\n\
             button { width = 200; height = 60; label = c; action = bad }\n\
             button { width = 200; height = 60; label = d; action = good }\n\
         }\n",
    )
    .unwrap();
    // Buttons at y 63, 131, 199, 267 under the shell's header, 60 high.
    let img = shot(&conf, &["--tap", "100,93", "--settle", "300"]);
    assert!(close(img.px(40, 70), [0, 0xff, 0], 16), "the running call's button is not :running");
    assert!(!close(img.px(40, 138), [0, 0xff, 0], 16), "another row's button shows :running");
    let img = shot(&conf, &["--tap", "100,229", "--settle", "500"]);
    assert!(img.any(|p| close(p, [0, 0, 0xff], 16)), "the failed call's button is not :failed");
    let img = shot(&conf, &["--tap", "100,297", "--settle", "500"]);
    assert!(!img.any(|p| close(p, [0, 0xff, 0], 16) || close(p, [0, 0, 0xff], 16)), "a good call left a state");
}

#[test]
fn a_running_button_shows_the_busy_text_or_a_spinner() {
    let d = tmp("button-busy");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t { critical = #ff00ff }\n\
         screen { theme = t }\n\
         action slow { busy = WORKING; run = sleep 2 }\n\
         action spin { run = sleep 2 }\n\
         action bad { run = false }\n\
         page home {\n\
             button { width = 200; height = 60; label = go; action = slow }\n\
             button { width = 200; height = 60; label = go; action = spin }\n\
             button { width = 200; height = 60; label = go; action = bad }\n\
         }\n",
    )
    .unwrap();
    let idle = shot(&conf, &["--settle", "100"]);
    let busy = shot(&conf, &["--tap", "100,93", "--settle", "300"]);
    // The busy text is wider than `go`: more text pixels in the button.
    let ink = |img: &Image, y0: usize| {
        // Inside the button (x 12..212), clear of its rounded corners.
        (y0 + 6..y0 + 54).flat_map(|y| (24..200).map(move |x| (x, y))).filter(|&(x, y)| img.px(x, y)[0] > 200 && img.px(x, y)[1] > 200).count()
    };
    assert!(ink(&busy, 63) > ink(&idle, 63) * 2, "no busy text");
    let spin = shot(&conf, &["--tap", "100,161", "--settle", "300"]);
    assert_ne!(ink(&spin, 131), ink(&idle, 131), "no spinner in place of the label");
    // A failed call's default look: a border in the theme's critical color.
    let failed = shot(&conf, &["--tap", "100,229", "--settle", "500"]);
    assert!(failed.any(|p| close(p, [0xff, 0, 0xff], 24)), "no critical border on the failed button");
}

#[test]
fn a_button_state_matches_its_own_row_and_its_component_call() {
    // Rows of maps tell their calls apart; a component call's own
    // `on_tap` shows on its root button.
    let d = tmp("button-rows");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t { style button:running { background = #00ff00 } }\n\
         screen { theme = t }\n\
         action slow { run = sleep 2 }\n\
         component tile { button { width = 200; height = 60; label = go } }\n\
         page home {\n\
             repeat vm in {{ [{'id': 'a'}, {'id': 'b'}] }} {\n\
                 button { width = 200; height = 60; label = {{ vm.id }}; on_tap { run = sleep 2 } }\n\
             }\n\
             tile { on_tap = slow }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "100,93", "--settle", "300"]);
    assert!(close(img.px(40, 70), [0, 0xff, 0], 16), "the tapped row is not :running");
    assert!(!close(img.px(40, 138), [0, 0xff, 0], 16), "the other row is :running too");
    let img = shot(&conf, &["--tap", "100,229", "--settle", "300"]);
    assert!(close(img.px(40, 206), [0, 0xff, 0], 16), "the component call's tap is not shown on its root");
}

#[test]
fn a_call_site_on_tap_wins_over_the_root_buttons_action() {
    let d = tmp("button-callsite");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var n: int = 0\n\
         action bump { set { n = {{ vars.n + 1 }} } }\n\
         action jump { set { n = {{ vars.n + 10 }} } }\n\
         component big {\n\
             param tap: action = none\n\
             button { width = 200; height = 60; label = go; action = {{ tap }} }\n\
         }\n\
         page home {\n\
             big { tap = bump; on_tap = jump }\n\
             rect { width = 200; height = 40; background = {{ vars.n == 10 ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "100,93", "--settle", "200"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "not only the call's on_tap ran");
}

#[test]
fn busy_and_done_read_the_calls_args() {
    let d = tmp("button-args");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "action slow { param v: string = \"\"; busy = {{ v }}; run = sleep 2 }\n\
         action quick { param v: string = \"\"; done = {{ v }}; run = true }\n\
         page home {\n\
             button { width = 200; height = 60; label = go; on_tap slow { v = MMMMMMMM } }\n\
             button { width = 200; height = 60; label = go; on_tap quick { v = MMMMMMMMMMMMMMMMMMMM } }\n\
         }\n",
    )
    .unwrap();
    let white = |p: [u8; 3]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let ink = |img: &Image, x0: usize, x1: usize, y0: usize, y1: usize| {
        (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).filter(|&(x, y)| white(img.px(x, y))).count()
    };
    let idle = shot(&conf, &["--settle", "100"]);
    let busy = shot(&conf, &["--tap", "100,93", "--settle", "300"]);
    assert!(ink(&busy, 24, 200, 69, 117) > ink(&idle, 24, 200, 69, 117) * 3, "busy did not show the call's arg");
    // The toast (bottom middle) is wide when it shows the arg.
    let done = shot(&conf, &["--tap", "100,161", "--settle", "300"]);
    assert!(ink(&done, 200, 600, 360, 420) > 150, "done did not show the call's arg");
}

#[test]
fn qr_codes_are_freed_without_lvgl_errors() {
    // LVGL 9.6.0's canvas destructor dropped `&draw_buf` (the field,
    // not the buffer) from the image cache, and logged "invalid magic"
    // when the pointer's low byte was under 0x20 (lvgl#10824, fixed on
    // release/v9.6). Many QR codes rebuilt a few times hit it.
    let d = tmp("qr-free");
    let conf = d.join("p.conf");
    let list: Vec<String> = (0..48).map(|i| i.to_string()).collect();
    std::fs::write(
        &conf,
        format!(
            "var n: int = 0\n\
             page home {{\n\
                 button {{ width = 100; height = 40; label = go; on_tap {{ set {{ n = {{{{ vars.n + 1 }}}} }} }} }}\n\
                 row {{ wrap = true; width = 100%\n\
                     repeat i in {{{{ [{}] }}}} {{ qr {{ size = 24; data = {{{{ string(i) + string(vars.n) }}}} }} }}\n\
                 }}\n\
             }}\n",
            list.join(", ")
        ),
    )
    .unwrap();
    let mut args = vec!["shot", "-c", conf.to_str().unwrap(), "-o", "/dev/null", "--settle", "50"];
    for _ in 0..6 {
        args.extend(["--tap", "50,83", "--settle", "50"]);
    }
    let out = Command::new(env!("CARGO_BIN_EXE_paneld")).args(&args).env("PANELD_VERIFY_UPDATES", "1").output().expect("run paneld shot");
    assert!(out.status.success());
    // LVGL logs with printf: on stdout.
    let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!log.contains("invalid magic"), "{log}");
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

#[test]
fn one_dialog_replaces_the_one_before() {
    // An asked param, then a confirm: the confirm replaces the ask
    // dialog. Stacked, two half-black backdrops would darken the page
    // twice.
    let d = tmp("one-dialog");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t { dialog_backdrop = #00000080 }\nscreen { theme = t }\n\
         action go { param m: int { ask = select; options = [5, 30] }\n confirm = Sure? }\n\
         page home { button { width = 200; height = 60; label = go; on_tap = go } }\n",
    )
    .unwrap();
    // Open the ask dialog, take its OK (the confirm then opens).
    let img = shot(&conf, &["--tap", "100,90", "--settle", "150", "--tap", "463,295", "--settle", "150"]);
    let [r, _, _] = img.px(780, 470);
    // The page background is about 0xf4; under one backdrop about 0x7a.
    assert!((0x6a..=0x8a).contains(&r), "background at {r:#x}: dialogs stacked, or none");
}

#[test]
fn state_styles_apply_and_literal_false_means_false() {
    let d = tmp("states");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t {\n\
             style button:disabled { background = #00ff00 }\n\
             style .card:disabled { background = #0000ff }\n\
         }\n\
         screen { theme = t }\n\
         var n: int = 0\n\
         page home {\n\
             button { width = 200; height = 60; label = off; enabled = false; on_tap { set { n = 1 } } }\n\
             rect { class = card; width = 100; height = 40; enabled = false }\n\
             rect { width = 100; height = 40; background = #ff0000; visible = false }\n\
             rect { width = 100; height = 40; background = {{ vars.n == 1 ? '#ff00ff' : '#000000' }} }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "100,90", "--settle", "300"]);
    // `:disabled` styles (LVGL lightens a disabled button a little).
    assert!(img.any(|[r, g, b]| g > 0xd0 && r < 0x90 && b < 0x90), "button:disabled not applied");
    assert!(img.any(|p| close(p, [0, 0, 0xff], 8)), ".card:disabled not applied");
    // `visible = false` written plainly hides; `enabled = false` takes no tap.
    assert!(!img.any(|p| close(p, [0xff, 0, 0], 8)), "visible = false still drawn");
    assert!(!img.any(|p| close(p, [0xff, 0, 0xff], 8)), "a disabled button took a tap");
}

#[test]
fn roller_and_button_row_selects_send_their_option() {
    let d = tmp("select-styles");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var a: string = \"\"\n\
         var b: string = \"y\"\n\
         page home {\n\
             title = Selects\n\
             row {\n\
                 gap = 16\n\
                 select { style = roller; width = 120; options = {{ ['x', 'y', 'z'] }}; value = {{ vars.a }}; on_change { set { a = {{ value }} } } }\n\
                 rect { width = 40; height = 40; background = {{ vars.a == 'y' ? '#00ff00' : '#ff0000' }} }\n\
                 select { style = buttons; width = 300; height = 60; options = {{ ['x', 'y', 'z'] }}; value = {{ vars.b }}; on_change { set { b = {{ value }} } } }\n\
                 rect { width = 40; height = 40; background = {{ vars.b == 'x' ? '#0000ff' : '#ff00ff' }} }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--tap", "91,170", "--settle", "400", "--tap", "284,113", "--settle", "300"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "the roller did not send `y`");
    assert!(img.any(|p| close(p, [0, 0, 0xff], 16)), "the button row did not send `x`");
}

#[test]
fn an_area_chart_fills_under_its_line() {
    let d = tmp("area");
    let conf = |ty: &str| {
        let c = d.join(format!("{ty}.conf"));
        std::fs::write(
            &c,
            format!(
                "page home {{ chart {{ type = {ty}; width = 600; height = 300; min = 0; max = 10\n\
                     series {{ values = {{{{ [8, 8, 8] }}}}; color = #ff0000 }} }} }}\n"
            ),
        )
        .unwrap();
        c
    };
    // Below the line, in the middle of the chart.
    let tinted = |img: &Image| {
        let [r, g, b] = img.px(300, 300);
        r > 0xe0 && (0x80..0xe0).contains(&g) && (0x80..0xe0).contains(&b)
    };
    assert!(tinted(&shot(&conf("area"), &["--settle", "50"])), "no fill under an area chart");
    assert!(!tinted(&shot(&conf("line"), &["--settle", "50"])), "a line chart got a fill");
}

#[test]
fn button_rows_fire_once_per_change_and_values_match_options_first() {
    let d = tmp("select-fixes");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var n: int = 0\n\
         var c: string = \"x\"\n\
         var iv: int = 5\n\
         page home {\n\
             title = S\n\
             select { style = buttons; width = 300; height = 50; options = {{ ['x', 'y', ''] }}; value = {{ vars.c }}; on_change { set { n = {{ vars.n + 1 }}; c = {{ value }} } } }\n\
             select { style = roller; width = 120; options = {{ [1, 2, 5, 10] }}; value = {{ vars.iv }} }\n\
             rect { width = 40; height = 40; background = {{ vars.n == 1 ? '#00ff00' : vars.n > 1 ? '#ff0000' : '#0000ff' }} }\n\
         }\n",
    )
    .unwrap();
    // Tap `y` twice: one change. (Each button is a third of the row.)
    let img = shot(&conf, &["--tap", "160,90", "--settle", "200", "--tap", "160,90", "--settle", "200"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "not exactly one change");
}

#[test]
fn colors_take_alpha_and_theme_names() {
    let d = tmp("alpha");
    let conf = d.join("p.conf");
    // `#ffffff80` over black is mid gray; `accent` and `track` are the
    // theme's; `border = 4 #ff0000` keeps its color on a one-line block.
    std::fs::write(
        &conf,
        "theme t { accent = #00ff00; track = #0000ff }\n\
         page home {\n\
             column { padding = 0; background = #000000; width = 300; height = 200\n\
                 rect { width = 100; height = 40; background = #ffffff80 }\n\
                 rect { width = 100; height = 40; background = accent }\n\
                 rect { width = 100; height = 40; background = track }\n\
                 rect { width = 100; height = 40; background = #000000; border = 4 #ff0000 }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &[]);
    assert!(img.any(|p| close(p, [0x80, 0x80, 0x80], 6)), "no half white over black");
    assert!(img.any(|p| close(p, [0, 0xff, 0], 10)), "theme accent not drawn");
    assert!(img.any(|p| close(p, [0, 0, 0xff], 10)), "theme extra token not drawn");
    assert!(img.any(|p| close(p, [0xff, 0, 0], 10)), "border color lost to a comment");

    // A translucent chart series is drawn faded; an opaque one of the
    // same color beside it stays full red.
    let conf = d.join("c.conf");
    std::fs::write(
        &conf,
        "page home { chart { width = 100%; height = 200; type = bar\n\
             series { values = {{ [5, 5, 5] }}; color = #0000ff60 }\n\
             series { values = {{ [5, 5, 5] }}; color = #0000ff } } }\n",
    )
    .unwrap();
    let img = shot(&conf, &[]);
    assert!(img.any(|p| close(p, [0, 0, 0xff], 10)), "opaque series drawn translucent");
    assert!(img.any(|p| p[2] > 0xe0 && p[0] > 0x80 && p[0] < 0xd0 && p[0] == p[1]), "translucent series not drawn");
    let conf = d.join("e.conf");
    std::fs::write(
        &conf,
        "page home { chart { width = 100%; height = 200; type = bar\n\
             series { values = {{ [5, 5, 5] }}; color = #ff000060 } } }\n",
    )
    .unwrap();
    let img = shot(&conf, &[]);
    assert!(!img.any(|p| close(p, [0xff, 0, 0], 40)), "series alpha dropped");
    assert!(img.any(|p| p[0] > 0xe0 && p[1] > 0x80 && p[1] < 0xd0 && p[1] == p[2]), "translucent series not drawn");
}

/// The smallest box (x0, y0, x1, y1, inclusive) holding every pixel
/// that matches `f`.
fn bbox(img: &Image, f: impl Fn([u8; 3]) -> bool) -> Option<(usize, usize, usize, usize)> {
    let mut b: Option<(usize, usize, usize, usize)> = None;
    for y in 0..img.h {
        for x in 0..img.w {
            if f(img.px(x, y)) {
                b = Some(match b {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
    }
    b
}

fn near(a: usize, b: usize, tol: usize) -> bool {
    a.abs_diff(b) <= tol
}

#[test]
fn an_arc_draws_its_track_and_indicator_with_no_knob_and_a_press_keeps_its_value() {
    let d = tmp("arc");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var tapped: bool = false\n\
         page home {\n\
             arc { width = 200; height = 200; value = 25; thickness = 20\n\
                   track = #00ff00; color = #ff0000\n\
                   on_hold { set { tapped = true } } }\n\
             rect { width = 20; height = 20; background = {{ vars.tapped ? '#ffff00' : '#000000' }} }\n\
         }\n",
    )
    .unwrap();
    let red = |p| close(p, [0xff, 0, 0], 16);
    let green = |p| close(p, [0, 0xff, 0], 16);
    let img = shot(&conf, &["--settle", "50"]);
    let (r, g) = (bbox(&img, red).expect("no indicator"), bbox(&img, green).expect("no track"));
    // 25% of 135..45 (270 degrees) ends at 202.5 degrees: the
    // indicator stays left of the middle, the track goes around.
    let (gx0, gy0, gx1, _) = g;
    let cx = (gx0 + gx1) / 2;
    assert!(r.2 < cx, "indicator reaches the right half: {r:?}");
    assert!(gx1 > cx + 80, "track does not reach the right: {g:?}");
    // No knob: LVGL draws one in its primary blue.
    assert!(!img.any(|p| close(p, [0x21, 0x96, 0xf3], 40)), "a knob is drawn");
    // A press on the track at 3 o'clock drags nothing, though the
    // `on_hold` makes the arc clickable (a short tap: no re-render).
    let at = format!("{},{}", gx1 - 10, gy0 + 100);
    let after = shot(&conf, &["--tap", &at, "--settle", "100"]);
    assert_eq!(bbox(&after, red), Some(r), "the press moved the value");
    let held = shot(&conf, &["--hold", &at, "--settle", "100"]);
    assert!(held.any(|p| close(p, [0xff, 0xff, 0], 16)), "the hold did not run on_hold");
}

#[test]
fn stack_children_sit_at_their_anchor_and_offset() {
    let d = tmp("stack");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             stack { width = 200; height = 200; background = #000000\n\
                 rect { anchor = center; width = 40; height = 40; background = #ff0000; border = 0; radius = 0 }\n\
                 rect { anchor = bottom_right; width = 30; height = 30; background = #0000ff; border = 0; radius = 0 }\n\
                 rect { x = 20; y = 10; width = 30; height = 30; background = #00ff00; border = 0; radius = 0 }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let s = bbox(&img, |p| p == [0, 0, 0]).expect("no stack");
    let r = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no centered child");
    let b = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no bottom_right child");
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 8)).expect("no offset child");
    assert!(near((r.0 + r.2) / 2, (s.0 + s.2) / 2, 2) && near((r.1 + r.3) / 2, (s.1 + s.3) / 2, 2), "center: {r:?} in {s:?}");
    assert!(near(b.2, s.2, 1) && near(b.3, s.3, 1), "bottom_right: {b:?} in {s:?}");
    assert!(near(g.0, s.0 + 20, 1) && near(g.1, s.1 + 10, 1), "top_left + (20, 10): {g:?} in {s:?}");
}

#[test]
fn padding_takes_two_or_four_sizes() {
    let d = tmp("padding");
    let conf = d.join("p.conf");
    let child = |c: &str| format!("rect {{ width = 20; height = 20; background = {c}; border = 0; radius = 0 }}");
    std::fs::write(
        &conf,
        format!(
            "page home {{\n\
                 row {{ gap = 20\n\
                     column {{ background = #ff0000; radius = 0; padding = 10 30; {} }}\n\
                     column {{ background = #0000ff; radius = 0; padding = 1 2 3 4; {} }}\n\
                 }}\n\
             }}\n",
            child("#00ff00"),
            child("#ffff00")
        ),
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let r = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no first column");
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 8)).expect("no first child");
    assert_eq!((r.2 - r.0 + 1, r.3 - r.1 + 1), (80, 40), "padding = 10 30: {r:?}");
    assert_eq!((g.0 - r.0, g.1 - r.1), (30, 10), "first child at {g:?} in {r:?}");
    let b = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no second column");
    let y = bbox(&img, |p| close(p, [0xff, 0xff, 0], 8)).expect("no second child");
    assert_eq!((b.2 - b.0 + 1, b.3 - b.1 + 1), (26, 24), "padding = 1 2 3 4: {b:?}");
    assert_eq!((y.0 - b.0, y.1 - b.1), (4, 1), "second child at {y:?} in {b:?}");
}

#[test]
fn grid_takes_a_track_list_gaps_and_spanning_cells() {
    let d = tmp("grid-tracks");
    let conf = d.join("p.conf");
    let rect = |c: &str, extra: &str| format!("rect {{ {extra} background = {c}; border = 0; radius = 0 }}");
    std::fs::write(
        &conf,
        format!(
            "page home {{\n\
                 grid {{ width = 400; padding = 0; columns = [100, 1fr, content]; rows = [50, 50]\n\
                        row_gap = 10; column_gap = 0\n\
                     {}\n{}\n{}\n\
                 }}\n\
             }}\n",
            rect("#ff0000", "cell = 1 0 2 2;"),
            rect("#00ff00", ""),
            rect("#0000ff", ""),
        ),
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let r = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no spanning cell");
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 8)).expect("no first free cell");
    let b = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no second free cell");
    let size = |(x0, y0, x1, y1): (usize, usize, usize, usize)| (x1 - x0 + 1, y1 - y0 + 1);
    // The free children fill column 0 (100 px), row by row.
    assert_eq!(size(g), (100, 50), "first free cell {g:?}");
    assert_eq!(size(b), (100, 50), "second free cell {b:?}");
    assert_eq!((b.0, b.1), (g.0, g.3 + 1 + 10), "second free cell below the first, 10 px apart");
    // The spanning cell takes the other 300 px and both rows.
    assert_eq!((r.0, r.1), (g.2 + 1, g.1), "spanning cell at {r:?}");
    assert_eq!(size(r), (300, 110), "spanning cell {r:?}");
}

#[test]
fn a_vertical_bar_fills_from_the_bottom_over_its_track() {
    let d = tmp("bar");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             bar { direction = vertical; width = 20; height = 200; value = 25; radius = 0\n\
                   track = #0000ff; color = #ff0000 }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let t = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no track");
    let i = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no indicator");
    assert!(i.3 - i.1 > i.2 - i.0, "indicator not upright: {i:?}");
    assert!(t.1 < i.1 && near(i.3, t.3, 2), "indicator not at the bottom of the track: {i:?} in {t:?}");
    assert!(near(i.3 - i.1 + 1, 50, 6), "indicator not a quarter high: {i:?}");
}

#[test]
fn text_overflow_dots_keeps_one_line_where_wrap_breaks() {
    let d = tmp("overflow");
    let conf = d.join("p.conf");
    let value = "\"aaaa bbbb cccc dddd eeee ffff gggg\"";
    std::fs::write(
        &conf,
        format!(
            "page home {{\n\
                 text {{ width = 120; overflow = dots; color = #ff0000; value = {value} }}\n\
                 text {{ width = 120; color = #0000ff; value = {value} }}\n\
             }}\n"
        ),
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let dots = bbox(&img, |p| p[0] > 0xc0 && p[1] < 0x60 && p[2] < 0x60).expect("no dots text");
    let wrap = bbox(&img, |p| p[2] > 0xc0 && p[0] < 0x60 && p[1] < 0x60).expect("no wrapped text");
    let h = |b: (usize, usize, usize, usize)| b.3 - b.1 + 1;
    assert!(h(dots) < 24, "dots text is more than a line: {dots:?}");
    assert!(h(wrap) > 2 * h(dots), "wrapped text did not wrap: {wrap:?}");
    assert!(dots.2 - dots.0 < 120 && wrap.2 - wrap.0 < 120, "text wider than its width");
}

#[test]
fn a_chart_has_no_white_box_unless_given_a_background() {
    let d = tmp("chart-look");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             column { background = #000000; padding = 20; radius = 0\n\
                 chart { width = 200; height = 100; series { values = {{ [1, 3, 2, 5] }}; color = #ff0000 } }\n\
                 chart { width = 200; height = 100; background = #00ff00; series { values = {{ [1, 2] }} } }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let col = bbox(&img, |p| p == [0, 0, 0]).expect("no column");
    let white = (col.1..=col.3).any(|y| (col.0..=col.2).any(|x| img.px(x, y).iter().all(|&c| c > 0xf0)));
    assert!(!white, "a chart drew a white box");
    assert!(img.any(|p| close(p, [0xff, 0, 0], 16)), "no series");
    // Division lines in a faint `muted` over the black.
    let faint = (col.1..=col.3).any(|y| (col.0..=col.2).any(|x| close(img.px(x, y), [0x1d, 0x1d, 0x1d], 8)));
    assert!(faint, "no faint division lines");
    assert!(img.any(|p| close(p, [0, 0xff, 0], 8)), "a chart's own background lost");
}

#[test]
fn align_self_places_a_child_across_its_column_and_a_row_wraps() {
    let d = tmp("align-self");
    let conf = d.join("p.conf");
    let rect = |c: &str, extra: &str| format!("rect {{ {extra} height = 20; background = {c}; border = 0; radius = 0 }}");
    std::fs::write(
        &conf,
        format!(
            "page home {{\n\
                 column {{ width = 200; padding = 0; background = #000000; radius = 0\n\
                     {}\n{}\n{}\n\
                 }}\n\
                 row {{ width = 200; padding = 0; gap = 0; wrap = true\n\
                     repeat i in {{{{ [1, 2, 3] }}}} {{ {} }}\n\
                 }}\n\
             }}\n",
            rect("#ff0000", "width = 40; align_self = end;"),
            rect("#00ff00", "width = 40; align_self = center;"),
            rect("#0000ff", "align_self = stretch;"),
            rect("#ffff00", "width = 80;"),
        ),
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let col = bbox(&img, |p| p == [0, 0, 0]).expect("no column");
    let r = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no end child");
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 8)).expect("no center child");
    let b = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no stretch child");
    assert!(near(r.2, col.2, 1), "end: {r:?} in {col:?}");
    assert!(near((g.0 + g.2) / 2, (col.0 + col.2) / 2, 2), "center: {g:?} in {col:?}");
    assert!(near(b.0, col.0, 1) && near(b.2, col.2, 1), "stretch: {b:?} in {col:?}");
    // Two 80 px children fit in 200; the third wraps to a second line.
    let y = bbox(&img, |p| close(p, [0xff, 0xff, 0], 8)).expect("no row");
    assert!(y.3 - y.1 + 1 >= 40, "the row did not wrap: {y:?}");
}

#[test]
fn align_stretch_fills_the_cross_axis_unless_a_child_sets_its_size() {
    let d = tmp("align-stretch");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             column { width = 200; padding = 0; align = stretch; background = #000000; radius = 0\n\
                 rect { height = 20; background = #ff0000; border = 0; radius = 0 }\n\
                 rect { width = 40; height = 20; background = #00ff00; border = 0; radius = 0 }\n\
                 text { value = abc; align_self = end; color = #0000ff }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let col = bbox(&img, |p| p == [0, 0, 0]).expect("no column");
    // Its own `align_self` wins: its own width, at the end (stretched,
    // the text would sit on the left).
    let b = bbox(&img, |p| p[2] > 0xc0 && p[0] < 0x60 && p[1] < 0x60).expect("no align_self child");
    assert!(b.0 > col.0 + 100 && near(b.2, col.2, 3), "align_self lost to align = stretch: {b:?} in {col:?}");
    let r = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no stretched child");
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 8)).expect("no sized child");
    assert!(near(r.0, col.0, 1) && near(r.2, col.2, 1), "not stretched: {r:?} in {col:?}");
    assert_eq!(g.2 - g.0 + 1, 40, "a set width was stretched: {g:?}");
}

#[test]
fn a_full_circle_arc_draws_all_round_from_any_start() {
    let d = tmp("arc-full");
    for (start, end) in [(0, 360), (90, 450)] {
        let conf = d.join(format!("p{start}.conf"));
        std::fs::write(
            &conf,
            format!(
                "page home {{ arc {{ width = 200; height = 200; value = 0; thickness = 10\n\
                     start = {start}; end = {end}; track = #00ff00 }} }}\n"
            ),
        )
        .unwrap();
        let img = shot(&conf, &["--settle", "50"]);
        let g = bbox(&img, |p| close(p, [0, 0xff, 0], 16)).expect("no track");
        let (cx, cy) = ((g.0 + g.2) / 2, (g.1 + g.3) / 2);
        let on = |x: usize, y: usize| (0..6).any(|i| close(img.px(x, y + i), [0, 0xff, 0], 40) || close(img.px(x + i, y), [0, 0xff, 0], 40));
        // Top, bottom, left and right of the ring all drawn.
        assert!(on(cx, g.1) && on(cx, g.3 - 5) && on(g.0, cy) && on(g.2 - 5, cy), "{start}..{end} is not a full circle: {g:?}");
    }
}

#[test]
fn a_line_chart_draws_no_dot_on_each_point() {
    let d = tmp("chart-dots");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home { chart { width = 300; height = 200; min = 0; max = 10\n\
             series { values = {{ [5, 5, 5, 5] }}; color = #ff0000 } } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let r = bbox(&img, |p| close(p, [0xff, 0, 0], 60)).expect("no line");
    let high = r.3 - r.1 + 1;
    assert!(high <= 4, "a flat line is {high} px high: dots drawn");
}

#[test]
fn dots_text_takes_more_lines_with_a_height_from_a_class() {
    let d = tmp("dots-class");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t { style .two { height = 40 } }\n\
         page home { text { class = two; width = 120; overflow = dots; color = #ff0000\n\
             value = \"aaaa bbbb cccc dddd eeee ffff gggg hhhh\" } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let r = bbox(&img, |p| p[0] > 0xc0 && p[1] < 0x60 && p[2] < 0x60).expect("no text");
    assert!(r.3 - r.1 + 1 > 24, "dots text kept to one line despite its class height: {r:?}");
}

fn count(img: &Image, f: impl Fn([u8; 3]) -> bool) -> usize {
    (0..img.h).map(|y| (0..img.w).filter(|&x| f(img.px(x, y))).count()).sum()
}

#[test]
fn an_icon_draws_in_its_color_at_its_size() {
    let d = tmp("icon");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home { row { gap = 20\n\
             icon { name = home; size = 32; color = #ff0000 }\n\
             icon { name = {{ 'bell' }}; size = 64; color = #00ff00 }\n\
             icon { name = {{ 'not_an_icon' }}; color = #0000ff } } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let red = bbox(&img, |p| close(p, [0xff, 0, 0], 40)).expect("built-in icon not drawn");
    assert!((20..=40).contains(&(red.2 - red.0 + 1)), "a 32 px icon is {red:?}");
    // Above 32 px the glyph is scaled up.
    let green = bbox(&img, |p| close(p, [0, 0xff, 0], 40)).expect("scaled icon not drawn");
    assert!(green.3 - green.1 + 1 > 50, "a 64 px icon is {green:?}");
    assert!(green.0 > red.2, "icons overlap: {red:?} {green:?}");
    // An unknown name (from an expression) draws nothing.
    assert!(!img.any(|p| close(p, [0, 0, 0xff], 40)));
}

#[test]
fn a_button_icon_goes_before_its_label() {
    let d = tmp("icon-button");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home { column {\n\
             button { label = Save; background = #ff0000 }\n\
             button { label = Save; icon = save; background = #00ff00 }\n\
             button { icon = play; background = #0000ff } } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let plain = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no label button");
    let both = bbox(&img, |p| close(p, [0, 0xff, 0], 8)).expect("no icon + label button");
    let icon = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no icon button");
    assert!(both.2 - both.0 > plain.2 - plain.0 + 10, "icon + label not wider: {both:?} vs {plain:?}");
    // The icon is drawn in the button's text color (white), at its
    // left, before the label.
    let white = |x: usize| (both.1..both.3).any(|y| close(img.px(x, y), [0xff, 0xff, 0xff], 40));
    let first = (both.0..both.2).find(|&x| white(x)).expect("nothing drawn on the button");
    assert!(first < both.0 + 24, "icon not first: content starts at {first} in {both:?}");
    assert!((icon.1..icon.3).any(|y| (icon.0..icon.2).any(|x| close(img.px(x, y), [0xff, 0xff, 0xff], 40))), "icon-only button empty");
}

#[test]
fn a_theme_icons_folder_png_replaces_a_built_in() {
    let d = tmp("icon-theme");
    // A 16x16 PNG of our own: a shot of a filled page.
    let src = d.join("src.conf");
    std::fs::write(&src, "page home { rect { width = 100%; height = 100%; background = #ff00ff } }\n").unwrap();
    std::fs::create_dir_all(d.join("icons")).unwrap();
    let made = shot(&src, &["--size", "16x16"]);
    assert_eq!((made.w, made.h), (16, 16));
    std::fs::rename(src.with_extension("png"), d.join("icons/home.png")).unwrap();
    let body = "page home { row { gap = 20\n\
                    icon { name = home; size = 40; color = #00ff00 }\n\
                    icon { name = settings; size = 32; color = #ff0000 } } }\n";
    let conf = d.join("p.conf");
    std::fs::write(&conf, format!("theme t {{ icons = icons }}\n{body}")).unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    // The PNG, sized to 40 px and tinted: a full green square.
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 40)).expect("theme icon not drawn");
    // (Its edges are blended with the page when scaled up.)
    let (w, h) = (g.2 - g.0 + 1, g.3 - g.1 + 1);
    assert!((37..=40).contains(&w) && (37..=40).contains(&h), "theme icon box {g:?}");
    assert!(count(&img, |p| close(p, [0, 0xff, 0], 40)) > 1300, "theme icon not filled");
    assert!(!img.any(|p| close(p, [0xff, 0, 0xff], 40)), "theme icon not recolored");
    // A name the folder lacks is still the built-in.
    assert!(img.any(|p| close(p, [0xff, 0, 0], 40)), "built-in fallback not drawn");

    // Without the folder: the built-in glyph, far from a full square.
    let conf = d.join("q.conf");
    std::fs::write(&conf, body).unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let n = count(&img, |p| close(p, [0, 0xff, 0], 40));
    assert!(n > 100 && n < 1200, "built-in home icon: {n} px");
}

#[test]
fn the_shell_nav_shows_page_icons() {
    let d = tmp("icon-nav");
    let width = |icons: bool| {
        let conf = d.join(format!("p{icons}.conf"));
        let icon = |n: &str| if icons { format!("icon = {n}; ") } else { String::new() };
        std::fs::write(
            &conf,
            format!(
                "theme t {{ accent = #ff00ff }}\n\
                 page home {{ title = Home; {}nav = 1 }}\n\
                 page two {{ title = Two; {}nav = 2 }}\n",
                icon("home"),
                icon("wifi")
            ),
        )
        .unwrap();
        let img = shot(&conf, &["--settle", "50"]);
        let b = bbox(&img, |p| close(p, [0xff, 0, 0xff], 8)).expect("no current nav button");
        assert!(b.1 > 400, "nav not at the bottom: {b:?}");
        let c = nav_content(&img, b);
        c.1 - c.0
    };
    // The icon goes before the title.
    let (with, without) = (width(true), width(false));
    assert!(with > without + 14, "nav content with an icon {with} px, without {without} px");
}

#[test]
fn a_tap_on_a_buttons_row_or_big_icon_reaches_the_button() {
    let d = tmp("icon-tap");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home { column { gap = 20\n\
             button { background = #ff0000; on_tap { goto = two }; icon { name = power; size = 96 } }\n\
             button { background = #0000ff; on_tap { goto = two }\n\
                      row { padding = 30; text { value = XXXXXXXX } } } } }\n\
         page two { rect { width = 100; height = 40; background = #00ff00 } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let red = bbox(&img, |p| close(p, [0xff, 0, 0], 8)).expect("no icon button");
    let blue = bbox(&img, |p| close(p, [0, 0, 0xff], 8)).expect("no row button");
    // The scaled icon takes the button's text color (white), not
    // LVGL's default dark one.
    let on_red = |f: &dyn Fn([u8; 3]) -> bool| (red.1..red.3).any(|y| (red.0..red.2).any(|x| f(img.px(x, y))));
    assert!(on_red(&|p| close(p, [0xff, 0xff, 0xff], 40)), "big icon not white");
    assert!(!on_red(&|p| p.iter().all(|&c| c < 0x60)), "big icon drawn dark");
    let green = |p| close(p, [0, 0xff, 0], 16);
    for b in [red, blue] {
        let at = format!("{},{}", (b.0 + b.2) / 2, (b.1 + b.3) / 2);
        let after = shot(&conf, &["--tap", &at, "--settle", "300"]);
        assert!(after.any(green), "a tap at {at} on {b:?} did not reach its button");
    }
}

#[test]
fn the_shell_nav_shares_the_width_and_shows_icons_only_when_crowded() {
    let d = tmp("icon-nav-fit");
    let names = ["Home Overview Dashboard", "Network and Firewall", "Media Library", "Power and UPS", "Storage Arrays", "Settings", "Mail"];
    let icons = ["home", "wifi", "video", "power", "drive", "settings", "envelope"];
    let nav = |n: usize, with_icons: bool| {
        let conf = d.join(format!("p{n}{with_icons}.conf"));
        let mut text = String::from("theme t { accent = #ff00ff; muted = #00ffff }\n");
        for i in 0..n {
            let icon = if with_icons { format!("icon = {}; ", icons[i]) } else { String::new() };
            text += &format!("page p{i} {{ title = {}; {icon}nav = {} }}\n", names[i], i + 1);
        }
        std::fs::write(&conf, text).unwrap();
        let img = shot(&conf, &["--settle", "50"]);
        let cur = bbox(&img, |p| close(p, [0xff, 0, 0xff], 8)).expect("no current nav button");
        let rest = bbox(&img, |p| close(p, [0, 0xff, 0xff], 8)).expect("no other nav buttons");
        let content = nav_content(&img, cur);
        (img.w, cur, rest, content)
    };
    for n in [3, 5, 7] {
        let (w, cur, rest, content) = nav(n, false);
        // Every button on screen, sharing the width; titles cut short
        // stay inside their button.
        assert!(rest.2 < w - 4 && cur.0 > 2, "{n} pages: nav overflows: {cur:?} {rest:?}");
        let share = (w - 16) / n;
        assert!((cur.2 - cur.0 + 1).abs_diff(share) < 12, "{n} pages: button {cur:?}, share {share}");
        assert!(content.0 > cur.0 && content.1 < cur.2, "{n} pages: title spills: {content:?} in {cur:?}");
    }
    // 5 pages with icons: icon and title; 7: just the icon.
    let (_, cur5, _, content5) = nav(5, true);
    assert!(content5.1 - content5.0 > 40, "5 pages: no title beside the icon: {content5:?} in {cur5:?}");
    let (_, cur7, _, content7) = nav(7, true);
    assert!(content7.1 - content7.0 < 24, "7 pages: more than the icon: {content7:?} in {cur7:?}");
}

/// The x extent of the white content (icon, title) across the middle
/// of nav button `b`, away from its rounded corners.
fn nav_content(img: &Image, b: (usize, usize, usize, usize)) -> (usize, usize) {
    let mid = (b.1 + b.3) / 2;
    (b.0 + 4..b.2 - 4)
        .filter(|&x| (mid - 8..mid + 8).any(|y| close(img.px(x, y), [0xff, 0xff, 0xff], 60)))
        .fold(None, |r: Option<(usize, usize)>, x| Some(r.map_or((x, x), |(a, _)| (a, x))))
        .expect("nav button empty")
}

/// The box of what `node` (a `text` or `icon`, colored pure red) draws,
/// alone on a page.
fn red_box(label: &str, node: &str) -> ((usize, usize, usize, usize), usize) {
    let d = tmp(label);
    let conf = d.join("p.conf");
    std::fs::write(&conf, format!("page home {{ {node} }}\n")).unwrap();
    let img = shot(&conf, &["--size", "800x300", "--settle", "50"]);
    let red = |p: [u8; 3]| p[0] as i32 - p[1] as i32 > 90 && p[0] as i32 - p[2] as i32 > 90;
    let b = bbox(&img, red).unwrap_or_else(|| panic!("{label}: nothing red drawn"));
    let filled = (b.1..=b.3).flat_map(|y| (b.0..=b.2).map(move |x| (x, y))).filter(|&(x, y)| red(img.px(x, y))).count();
    (b, filled)
}

/// Width, height and red pixels of a line of text in `font`.
fn text_box(label: &str, font: &str) -> (usize, usize, usize) {
    let (b, ink) = red_box(label, &format!("text {{ value = \"Quick brown fox 12:45\"; color = #ff0000; font = {font} }}"));
    (b.2 - b.0 + 1, b.3 - b.1 + 1, ink)
}

#[test]
fn ttf_families_weights_and_sizes_draw_differently() {
    let (roboto, _, roboto_ink) = text_box("roboto", "roboto 20");
    let (inter, _, _) = text_box("inter", "inter 20");
    assert!(roboto.abs_diff(inter) >= 4, "roboto 20 {roboto} px, inter 20 {inter} px");
    // Roboto Bold keeps nearly the widths of Regular, with more ink.
    let (_, _, bold_ink) = text_box("rbold", "roboto 20 bold");
    assert!(bold_ink * 10 > roboto_ink * 13, "bold ink {bold_ink}, regular {roboto_ink}");
    let (open, _, _) = text_box("open", "open_sans 20");
    let (bold, _, _) = text_box("bold", "open_sans 20 bold");
    assert!(bold > open + 4, "bold {bold} px not wider than regular {open} px");
    let (_, h32, _) = text_box("h32", "noto_sans 32");
    let (_, h48, _) = text_box("h48", "noto_sans 48");
    assert!(h48 * 10 > h32 * 13, "48 px is {h48} high, 32 px {h32}");
    // Any size: 37 is no bitmap size.
    let (w37, _, _) = text_box("w37", "montserrat 37");
    let (w32, _, _) = text_box("w32", "montserrat 32");
    assert!(w37 > w32 + 10, "37 px {w37}, 32 px {w32}");
}

#[test]
fn a_symbol_inside_ttf_text_comes_from_the_bitmap_fallback() {
    // `home` (U+F015) is not in the TTF: drawn from Montserrat 20, the
    // same glyph as the icon, not a placeholder box.
    let (glyph, filled) = red_box("sym", "text { value = \"\u{f015}\"; color = #ff0000; font = roboto 20 }");
    let (icon, _) = red_box("icon", "icon { name = home; size = 20; color = #ff0000 }");
    let size = |b: (usize, usize, usize, usize)| (b.2 - b.0, b.3 - b.1);
    assert_eq!(size(glyph), size(icon));
    let area = (glyph.2 - glyph.0 + 1) * (glyph.3 - glyph.1 + 1);
    assert!(filled * 10 > area * 3, "an outline, not the glyph: {filled} of {area}");
}

#[test]
fn an_unreadable_font_file_falls_back_to_montserrat() {
    // From an expression, so the check cannot catch it.
    let (b, _) = red_box("nofile", "text { value = Hi; color = #ff0000; font = {{ \"fonts/none.ttf 20\" }} }");
    assert!(b.3 - b.1 > 8, "{b:?}");
}

#[test]
fn an_arrow_in_roboto_comes_from_montserrat() {
    // Roboto has no arrows: drawn from Montserrat 20 (TTF), not as a
    // placeholder box.
    let (roboto, filled) = red_box("arrow", "text { value = \"\u{2192}\"; color = #ff0000; font = roboto 20 }");
    let (montserrat, _) = red_box("arrow_m", "text { value = \"\u{2192}\"; color = #ff0000; font = montserrat 20 }");
    let size = |b: (usize, usize, usize, usize)| (b.2 - b.0, b.3 - b.1);
    assert_eq!(size(roboto), size(montserrat));
    // A glyph no font has (a snowman) draws as the placeholder box.
    let (boxed, boxed_filled) = red_box("arrow_x", "text { value = \"\u{2603}\"; color = #ff0000; font = roboto 20 }");
    assert_ne!((size(roboto), filled), (size(boxed), boxed_filled), "the arrow drew as a placeholder");
}

/// A config folder with theme folder `themes/side` (ADR 0021): its
/// `shell` puts the navigation in a blue column on the left, its
/// `tile` is a green box holding the folder's `icons/home.png` (a
/// magenta square, drawn tinted red), its text font is `font`. The
/// config's own `tile` is plain text; `night` adds night hours that
/// switch to theme `plain`, which has no components.
fn theme_folder(label: &str, font: &str, night: bool) -> PathBuf {
    let d = tmp(label);
    let src = d.join("src.conf");
    std::fs::write(&src, "page home { rect { width = 100%; height = 100%; background = #ff00ff } }\n").unwrap();
    shot(&src, &["--size", "16x16"]);
    let side = d.join("themes/side");
    std::fs::create_dir_all(side.join("icons")).unwrap();
    std::fs::create_dir_all(side.join("fonts")).unwrap();
    std::fs::rename(src.with_extension("png"), side.join("icons/home.png")).unwrap();
    let inter = Path::new(env!("CARGO_MANIFEST_DIR")).join("../paneld-ui/fonts/inter-regular.ttf");
    std::fs::copy(inter, side.join("fonts/f.ttf")).unwrap();
    std::fs::write(
        side.join("theme.conf"),
        format!(
            "theme side {{\n\
                 font  = {font}\n\
                 icons = icons\n\
                 component shell {{\n\
                     row {{\n\
                         width = 100%; height = 100%; padding = 0; gap = 0; radius = 0; border = 0; background = #000000\n\
                         column {{\n\
                             width = 120; height = 100%; padding = 0; gap = 0; radius = 0; border = 0; background = #0000ff\n\
                             repeat p in {{{{ nav }}}} {{\n\
                                 button {{ width = 120; height = 60; label = {{{{ p.title }}}}; on_tap {{ goto = {{{{ p.name }}}} }} }}\n\
                             }}\n\
                         }}\n\
                         column {{ grow = 1; height = 100%; padding = 10; gap = 10; radius = 0; border = 0; slot {{ }} }}\n\
                     }}\n\
                 }}\n\
                 component tile {{\n\
                     param label: string\n\
                     row {{ width = 200; height = 60; padding = 10; radius = 0; border = 0; background = #00ff00\n\
                         icon {{ name = home; size = 40; color = #ff0000 }} }}\n\
                 }}\n\
             }}\n"
        ),
    )
    .unwrap();
    let night = if night {
        let o = Command::new("date").arg("+%H").output().unwrap();
        let h: u32 = String::from_utf8_lossy(&o.stdout).trim().parse().unwrap();
        format!("theme plain {{ }}\nscreen {{ night {{ from = {:02}:00; to = {:02}:00; theme = plain }} }}\n", (h + 23) % 24, (h + 2) % 24)
    } else {
        String::new()
    };
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "screen:theme = side\n{night}\
             component tile {{ param label: string\n text {{ value = {{{{ label }}}}; color = #00ff00 }} }}\n\
             page home {{ title = One; nav = 1\n\
                 tile {{ label = config }}\n\
                 text {{ value = \"Quick brown fox 12:45\"; color = #ffff00 }} }}\n\
             page two {{ title = Two; nav = 2\n rect {{ width = 100; height = 40; background = #ff8000 }} }}\n"
        ),
    )
    .unwrap();
    conf
}

#[test]
fn a_theme_folder_replaces_the_shell_and_a_component_with_its_own_assets() {
    let conf = theme_folder("tdir", "fonts/f.ttf 20", false);
    let check = Command::new(env!("CARGO_BIN_EXE_paneld")).arg("check").arg("-c").arg(&conf).output().unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    let img = shot(&conf, &["--settle", "100"]);
    let blue = |p| close(p, [0, 0, 0xff], 16);
    let green = |p| close(p, [0, 0xff, 0], 16);
    let red = |p| close(p, [0xff, 0, 0], 40);
    // The theme's shell: navigation in a column on the left, the full
    // height; no bottom bar.
    let b = bbox(&img, blue).expect("no blue nav column");
    assert!(b.0 < 2 && b.2 > 110 && b.2 < 125 && b.3 > 470, "nav column {b:?}");
    // The theme's tile, with the theme folder's icon PNG (a full square,
    // not the built-in glyph).
    let g = bbox(&img, green).expect("theme tile not drawn");
    assert!(g.0 >= 120 && g.2 - g.0 > 190, "tile {g:?}");
    assert!(count(&img, red) > 1300, "theme icon PNG not drawn: {} px", count(&img, red));
    // The theme's font file: drawn as Inter, the same text as the
    // built-in `inter 20` and not as Montserrat.
    let yellow = |p: [u8; 3]| p[0] > 150 && p[1] > 150 && p[2] < 90;
    let width = |img: &Image| {
        let b = bbox(img, yellow).expect("no text");
        b.2 - b.0
    };
    let file = width(&img);
    let builtin = width(&shot(&theme_folder("tdir-inter", "inter 20", false), &["--settle", "100"]));
    let montserrat = width(&shot(&theme_folder("tdir-mont", "montserrat 20", false), &["--settle", "100"]));
    assert_eq!(file, builtin, "theme font file not used");
    assert!(file.abs_diff(montserrat) >= 4, "inter {file} px, montserrat {montserrat} px");

    // The left nav works: page two.
    let two = shot(&conf, &["--tap", "60,90", "--settle", "300"]);
    assert!(two.any(|p| close(p, [0xff, 0x80, 0], 16)) && !two.any(green), "nav did not switch to page two");
}

#[test]
fn the_night_theme_brings_its_own_components() {
    // Night hours now: theme `plain`, which has no components, so the
    // config's `tile` and the library's shell are back.
    let conf = theme_folder("tnight", "fonts/f.ttf 20", true);
    let img = shot(&conf, &["--settle", "200"]);
    assert!(!img.any(|p| close(p, [0, 0, 0xff], 16)), "the side theme's shell under the night theme");
    assert!(count(&img, |p| close(p, [0xff, 0, 0], 40)) < 50, "the side theme's tile under the night theme");
    // The config's tile: green text, not a 200 px box.
    let g = bbox(&img, |p| close(p, [0, 0xff, 0], 40)).expect("config tile not drawn");
    assert!(g.2 - g.0 < 150, "config tile {g:?}");
}

#[test]
fn a_broken_theme_folder_is_a_check_error_with_its_file_and_line() {
    let conf = theme_folder("tbroken", "fonts/none.ttf 20", false);
    let check = Command::new(env!("CARGO_BIN_EXE_paneld")).arg("check").arg("-c").arg(&conf).output().unwrap();
    assert!(!check.status.success());
    let err = String::from_utf8_lossy(&check.stderr);
    assert!(err.contains("themes/side/theme.conf:2:") && err.contains("font file `themes/side/fonts/none.ttf` not found"), "{err}");
}

#[test]
fn the_shipped_light_and_dark_themes_draw_by_name() {
    let d = tmp("shipped");
    let look = |theme: &str| {
        let conf = d.join(format!("{theme}.conf"));
        std::fs::write(
            &conf,
            format!(
                "screen:theme = {theme}\n\
                 page home {{ title = One; nav = 1\n text {{ value = \"Quick brown fox\"; font = roboto 32 }} }}\n\
                 page two {{ title = Two; nav = 2 }}\n"
            ),
        )
        .unwrap();
        shot(&conf, &["--settle", "100"])
    };
    // Light: today's built-in default look; dark text on light grey.
    let light = look("light");
    assert!(close(light.px(400, 300), [0xf5, 0xf5, 0xf5], 8), "light page {:?}", light.px(400, 300));
    assert!(close(light.px(400, 20), [0xff, 0xff, 0xff], 8), "light header");
    let ink = |img: &Image, f: &dyn Fn([u8; 3]) -> bool| (60..160).flat_map(|y| (0..400).map(move |x| (x, y))).filter(|&(x, y)| f(img.px(x, y))).count();
    assert!(ink(&light, &|p| p[0] < 0x40) > 300, "light body text not dark");
    // Dark: light text on near black, the header a step lighter.
    let dark = look("dark");
    assert!(close(dark.px(400, 300), [0x12, 0x12, 0x12], 8), "dark page {:?}", dark.px(400, 300));
    assert!(close(dark.px(400, 20), [0x1e, 0x1e, 0x1e], 8), "dark header");
    assert!(ink(&dark, &|p| p[0] > 0xc0) > 300, "dark body text not light");
}

#[test]
fn rotation_90_draws_the_upright_page_turned_into_the_panel_and_turns_taps() {
    // The tap test's page on an 800x480 panel mounted turned 90
    // degrees clockwise: the page lays out at 480x800, the panel's
    // buffer gets it turned, and a touch (in panel coordinates) on
    // the button's place still presses it.
    let d = tmp("rot");
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
    let upright = shot(&conf, &["--size", "480x800", "--settle", "50"]);
    let panel = shot(&conf, &["--size", "800x480", "--rotation", "90", "--settle", "50"]);
    assert_eq!((panel.w, panel.h), (800, 480));
    // Logical (x, y) shows at panel (y, 479 - x).
    let mut differ = 0;
    for y in 0..800 {
        for x in 0..480 {
            if !close(upright.px(x, y), panel.px(y, 479 - x), 8) {
                differ += 1;
            }
        }
    }
    assert!(differ < 480 * 800 / 100, "{differ} pixels differ from the upright page turned");
    let green = |p| close(p, [0, 0xff, 0], 16);
    assert!(!panel.any(green), "green before the tap");
    // The button's middle at logical (100, 120) is panel (120, 379).
    let after = shot(&conf, &["--size", "800x480", "--rotation", "90", "--tap", "120,379", "--settle", "100"]);
    assert!(after.any(green), "the turned tap missed the button");
}

/// Write an RGBA PNG of `w` x `h` with pixels from `f` (stored
/// deflate blocks: big enough for test images).
fn write_png(path: &Path, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) {
    fn crc(data: &[u8]) -> u32 {
        let mut c = 0xffff_ffffu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 == 1 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&f(x, y));
        }
    }
    let mut z = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, c) in chunks.iter().enumerate() {
        z.push(u8::from(i + 1 == chunks.len()));
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc(&body).to_be_bytes());
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    std::fs::write(path, out).unwrap();
}

#[test]
fn gradients_change_across_the_node_with_alpha_stops() {
    let d = tmp("grad");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home {\n\
             rect { width = 100%; height = 120; border = 0; radius = 0; background = linear 90 #ff0000 #0000ff }\n\
             rect { width = 100%; height = 120; border = 0; radius = 0; background = linear 180 #00ff00 #00ff0000 }\n\
             rect { width = 200; height = 120; border = 0; radius = 0; background = radial #ffffff #000000 }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--size", "800x600", "--settle", "50"]);
    // The first rect: find its rows by the reddest pixel on the left.
    let y = (0..600).find(|&y| close(img.px(14, y), [0xff, 0, 0], 40)).expect("no red at the left edge");
    let (l, r) = (img.px(14, y + 10), img.px(785, y + 10));
    assert!(l[0] > 200 && l[2] < 60, "left {l:?}");
    assert!(r[2] > 200 && r[0] < 60, "right {r:?}");
    let mid = img.px(400, y + 10);
    assert!(mid[0] > 80 && mid[2] > 80, "middle {mid:?}");
    // The second: green at the top fading to the page (#f5f5f5): the
    // last stop's alpha is 0.
    let g = (y + 120..600).find(|&yy| close(img.px(400, yy), [0, 0xff, 0], 40)).expect("no green");
    let bottom = img.px(400, g + 115);
    assert!(close(bottom, [0xf5, 0xf5, 0xf5], 30), "bottom of the fade {bottom:?}");
    // The radial: white in the middle, dark in the corners.
    let top = (g + 120..600).find(|&yy| img.px(14, yy)[0] < 120).expect("no radial");
    let center = img.px(14 + 100, top + 60);
    let corner = img.px(16, top + 2);
    assert!(center[0] > 220, "radial center {center:?}");
    assert!(corner[0] < 100, "radial corner {corner:?}");
}

#[test]
fn image_backgrounds_cover_contain_and_sit_behind_the_page() {
    let d = tmp("bgimg");
    // 200x100: red left half, blue right half.
    write_png(&d.join("a.png"), 200, 100, |x, _| if x < 100 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
    write_png(&d.join("bg.png"), 16, 16, |_, _| [0, 160, 80, 255]);
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t { background_image = bg.png }\n\
         screen { theme = t }\n\
         page home {\n\
             row { gap = 0; padding = 0\n\
                 rect { width = 100; height = 100; border = 0; radius = 0; background = a.png }\n\
                 rect { width = 100; height = 100; border = 0; radius = 0; background = a.png contain }\n\
                 rect { width = 100; height = 100; border = 0; radius = 0; background = a.png fill }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let red = |p| close(p, [0xff, 0, 0], 40);
    let blue = |p| close(p, [0, 0, 0xff], 40);
    let x0 = (0..800).find(|&x| (0..480).any(|y| red(img.px(x, y)))).expect("no image drawn");
    let count = |x: usize, f: &dyn Fn([u8; 3]) -> bool| (0..480).filter(|&y| f(img.px(x, y))).count();
    // Cover: the middle 100x100 of the image, so red then blue, full height.
    assert!((95..=100).contains(&count(x0 + 25, &red)), "cover red {}", count(x0 + 25, &red));
    assert!((95..=100).contains(&count(x0 + 75, &blue)), "cover blue {}", count(x0 + 75, &blue));
    // Contain: 100x50, centered: half the height.
    assert!((45..=55).contains(&count(x0 + 125, &red)), "contain red {}", count(x0 + 125, &red));
    assert!((45..=55).contains(&count(x0 + 175, &blue)), "contain blue {}", count(x0 + 175, &blue));
    // Fill: the whole image squeezed.
    assert!((95..=100).contains(&count(x0 + 225, &red)), "fill red");
    assert!((95..=100).contains(&count(x0 + 275, &blue)), "fill blue");
    // The theme's background image behind the page (the shell's body).
    assert!(close(img.px(600, 300), [0, 160, 80], 8), "page background {:?}", img.px(600, 300));
    // Contain leaves the rect's own (transparent) background: the page shows.
    let y_top = (0..480).find(|&y| red(img.px(x0 + 25, y))).unwrap();
    assert!(close(img.px(x0 + 125, y_top + 5), [0, 160, 80], 8), "{:?}", img.px(x0 + 125, y_top + 5));
}

#[test]
fn a_shadow_draws_outside_the_node() {
    let d = tmp("shadow");
    let conf = d.join("p.conf");
    let page = |shadow: &str| {
        format!(
            "page home {{\n\
                 rect {{ width = 120; height = 80; border = 0; radius = 8; background = #ff00ff; {shadow} }}\n\
             }}\n"
        )
    };
    std::fs::write(&conf, page("shadow = 16 #000000 0 8")).unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let magenta = |p| close(p, [0xff, 0, 0xff], 8);
    let (x0, y0) = (0..480)
        .find_map(|y| (0..800).find(|&x| magenta(img.px(x, y))).map(|x| (x, y)))
        .expect("no rect");
    // Below the rect (offset 8 down): darker than the page.
    let below = img.px(x0 + 60, y0 + 80 + 6);
    assert!(below[0] < 0xd0, "no shadow under the rect: {below:?}");
    std::fs::write(&conf, page("shadow = none")).unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    assert!(close(img.px(x0 + 60, y0 + 80 + 6), [0xf5, 0xf5, 0xf5], 4), "shadow = none drew one");
}

#[test]
fn a_background_image_lies_over_a_gradient_or_image_background() {
    let d = tmp("bglayers");
    // Left half transparent, right half green.
    write_png(&d.join("over.png"), 40, 20, |x, _| if x < 20 { [0, 0, 0, 0] } else { [0, 200, 0, 255] });
    write_png(&d.join("under.png"), 8, 8, |_, _| [200, 0, 200, 255]);
    let conf = d.join("p.conf");
    let write = |background: &str| {
        std::fs::write(
            &conf,
            format!(
                "theme t {{ background = {background}\n background_image = over.png }}\n\
                 screen {{ theme = t }}\n\
                 page home {{ text {{ value = hi }} }}\n"
            ),
        )
        .unwrap();
    };
    write("linear 180 #ff0000 #0000ff");
    let img = shot(&conf, &["--settle", "50"]);
    let (top, bottom) = (img.px(100, 80), img.px(100, 470));
    assert!(top[0] > 150 && top[2] < 120, "gradient top under the image {top:?}");
    assert!(bottom[2] > 150 && bottom[0] < 120, "gradient bottom under the image {bottom:?}");
    assert!(close(img.px(700, 300), [0, 200, 0], 8), "image half {:?}", img.px(700, 300));
    write("under.png");
    let img = shot(&conf, &["--settle", "50"]);
    assert!(close(img.px(100, 300), [200, 0, 200], 8), "image background under {:?}", img.px(100, 300));
    assert!(close(img.px(700, 300), [0, 200, 0], 8), "image half {:?}", img.px(700, 300));
}

/// A page with a bar (red, with an `enter`, which must not stop its
/// value from moving), an arc (green) and a bar chart (magenta) on
/// `vars.v`, and a button that sets it from 10 to 90, under a theme
/// with `tokens`.
fn motion_conf(label: &str, tokens: &str) -> PathBuf {
    let d = tmp(label);
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "theme t {{ {tokens} }}\n\
             var v: int = 10\n\
             page home {{\n\
                 button {{ width = 200; height = 60; label = go; on_tap {{ set {{ v = 90 }} }} }}\n\
                 row {{ gap = 20\n\
                     bar {{ width = 300; height = 20; value = {{{{ vars.v }}}}; color = #ff0000; enter = fade }}\n\
                     arc {{ width = 120; height = 120; thickness = 12; value = {{{{ vars.v }}}}; color = #00ff00 }}\n\
                     chart {{ width = 60; height = 120; type = bar; min = 0; max = 100\n\
                              series {{ values = {{{{ [vars.v] }}}}; color = #ff00ff }} }}\n\
                 }}\n\
             }}\n"
        ),
    )
    .unwrap();
    conf
}

/// Pixels of the bar, the arc and the chart.
fn gauges(img: &Image) -> [usize; 3] {
    [
        count(img, |p| close(p, [0xff, 0, 0], 16)),
        count(img, |p| close(p, [0, 0xff, 0], 16)),
        count(img, |p| close(p, [0xff, 0, 0xff], 16)),
    ]
}

#[test]
fn value_changes_move_from_the_last_value_and_end_on_the_new_one() {
    let conf = motion_conf("motion-value", "value_transition = 1000ms");
    let before = gauges(&shot(&conf, &["--settle", "50"]));
    let mid_img = shot(&conf, &["--tap", "100,95", "--settle", "20", "--wait", "200"]);
    // A value change redraws the page in place: no page transition.
    assert!(close(mid_img.px(30, 80), [0x21, 0x96, 0xf3], 12), "the page faded on a value change: {:?}", mid_img.px(30, 80));
    let mid = gauges(&mid_img);
    let end = gauges(&shot(&conf, &["--tap", "100,95", "--settle", "20", "--wait", "1300"]));
    for i in 0..3 {
        let (b, m, e) = (before[i], mid[i], end[i]);
        assert!(b * 3 < e, "gauge {i}: {b} px at 10, {e} px at 90");
        // Under way: well past the start, not yet at the end.
        assert!(m > b + (e - b) / 5 && m + (e - b) / 10 < e, "gauge {i} mid-way: {b} < {m} < {e}");
    }
    // The same after the change with motion off: at the end at once.
    let conf = motion_conf("motion-none", "value_transition = 1000ms; motion = none");
    let mid = gauges(&shot(&conf, &["--tap", "100,95", "--settle", "20", "--wait", "200"]));
    assert_eq!(mid, end, "motion = none still animates");
}

/// Two pages: green on the left of `home` with a button to `two`,
/// blue on the right of `two`.
fn pages_conf(label: &str, tokens: &str) -> PathBuf {
    let d = tmp(label);
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        format!(
            "theme t {{ {tokens} }}\n\
             page home {{ row {{ gap = 200\n\
                 button {{ width = 160; height = 60; label = go; on_tap {{ goto = two }} }}\n\
                 rect {{ width = 100; height = 100; background = #00ff00 }} }} }}\n\
             page two {{ row {{ gap = 400\n\
                 text {{ value = two }}\n\
                 rect {{ width = 100; height = 100; background = #0000ff }} }} }}\n"
        ),
    )
    .unwrap();
    conf
}

#[test]
fn navigation_fades_or_slides_the_page_in() {
    let green = |p| close(p, [0, 0xff, 0], 16);
    let blue = |p| close(p, [0, 0, 0xff], 16);
    let tap = ["--tap", "90,90", "--settle", "20", "--wait"];
    // `none`: the new page at once.
    let conf = pages_conf("tr-none", "page_transition = none");
    let img = shot(&conf, &[&tap[..], &["200"]].concat());
    assert!(img.any(blue) && !img.any(green), "`none` did not switch at once");
    // `fade`: under way both pages show, blended; then only the new one.
    let conf = pages_conf("tr-fade", "page_transition = fade 1000ms");
    let g = bbox(&shot(&conf, &["--settle", "50"]), green).expect("no green on home");
    let (gx, gy) = ((g.0 + g.2) / 2, (g.1 + g.3) / 2);
    let mid = shot(&conf, &[&tap[..], &["200"]].concat());
    let p = mid.px(gx, gy);
    assert!(!green(p) && p[0] > 0x20 && p[0] < 0xd0, "not fading at {gx},{gy}: {p:?}");
    let end = shot(&conf, &[&tap[..], &["1300"]].concat());
    assert!(end.any(blue) && !end.any(green), "fade did not end on the new page");
    // `slide` needs `motion = full`: under the default `reduced` it
    // switches at once.
    let conf = pages_conf("tr-slide-reduced", "page_transition = slide 1000ms");
    let img = shot(&conf, &[&tap[..], &["200"]].concat());
    assert!(img.any(blue) && !img.any(green), "a slide ran under `reduced`");
    // Under `full` the old page moves out to the left, the new one in.
    let conf = pages_conf("tr-slide", "page_transition = slide 1000ms; motion = full");
    let mid = shot(&conf, &[&tap[..], &["200"]].concat());
    let moved = bbox(&mid, green).expect("old page gone mid-slide");
    assert!(moved.0 + 100 < g.0, "old page did not slide left: {moved:?} vs {g:?}");
    let end = shot(&conf, &[&tap[..], &["1300"]].concat());
    assert!(end.any(blue) && !end.any(green), "slide did not end on the new page");
}

#[test]
fn the_alert_banner_comes_in_with_the_theme_transition() {
    let d = tmp("banner-enter");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "theme t { banner_transition = fade 1000ms }\n\
         var on: bool = false\n\
         alert hot { when = {{ vars.on }}; severity = critical; title = Hot }\n\
         page home { button { width = 200; height = 60; label = go; on_tap { set { on = true } } } }\n",
    )
    .unwrap();
    let critical = [0xd3, 0x2f, 0x2f];
    // The banner sits under the header, before the body.
    let mid = shot(&conf, &["--tap", "100,95", "--settle", "20", "--wait", "200"]);
    let end = shot(&conf, &["--tap", "100,95", "--settle", "20", "--wait", "1300"]);
    let y = (0..end.h).find(|&y| close(end.px(700, y), critical, 16)).expect("no banner");
    let p = mid.px(700, y + 4);
    assert!(!close(p, critical, 16) && p[1] < 0xe0, "banner not fading in: {p:?}");
    // A rebuild during the entrance (another var set) carries it on
    // from where it was, not at full opacity.
    let d2 = tmp("banner-rebuild");
    let conf2 = d2.join("p.conf");
    std::fs::write(
        &conf2,
        "theme t { banner_transition = fade 1000ms }\n\
         var on: bool = false\n\
         var n: int = 0\n\
         alert hot { when = {{ vars.on }}; severity = critical; title = Hot }\n\
         page home { row { gap = 20\n\
             button { width = 200; height = 60; label = go; on_tap { set { on = true } } }\n\
             button { width = 200; height = 60; background = #00ffff; label = {{ string(vars.n) }}; on_tap { set { n = {{ vars.n + 1 }} } } } } }\n",
    )
    .unwrap();
    let shown = shot(&conf2, &["--tap", "100,95", "--settle", "1300"]);
    let c = bbox(&shown, |p| close(p, [0, 0xff, 0xff], 16)).expect("no second button");
    let at = format!("{},{}", (c.0 + c.2) / 2, (c.1 + c.3) / 2);
    // (`--settle` is one setting for every step: the gap goes in `--wait`.)
    let cut = shot(&conf2, &["--settle", "20", "--tap", "100,95", "--wait", "60", "--tap", &at, "--wait", "40"]);
    let p = cut.px(700, y + 4);
    assert!(!close(p, critical, 16) && p[1] < 0xe0, "a rebuild ended the entrance at once: {p:?}");
    // At once with `motion = none`.
    std::fs::write(&conf, std::fs::read_to_string(&conf).unwrap().replace("fade 1000ms", "fade 1000ms; motion = none")).unwrap();
    let img = shot(&conf, &["--tap", "100,95", "--settle", "20", "--wait", "200"]);
    assert!(close(img.px(700, y + 4), critical, 16), "banner faded under `motion = none`");
}

/// Whether a pixel in rows `ys` passes `f`.
fn any_in_rows(img: &Image, ys: std::ops::Range<usize>, f: impl Fn([u8; 3]) -> bool) -> bool {
    ys.into_iter().any(|y| (0..img.w).any(|x| f(img.px(x, y))))
}

#[test]
fn now_renders_in_local_time() {
    let d = tmp("now");
    let conf = d.join("p.conf");
    // The green square shows only when `now.time` agrees with its
    // fields and the date fields are there.
    std::fs::write(
        &conf,
        "page home {\n\
             text { value = {{ now.weekday }}, {{ now.date }} {{ now.time }} }\n\
             rect {\n\
                 width = 40; height = 40; background = #00ff00\n\
                 visible = {{ now.time == (now.hour < 10 ? '0' : '') + string(now.hour) + ':' + (now.minute < 10 ? '0' : '') + string(now.minute) && size(now.iso_date) == 10 && now.month_name.startsWith(now.month_short) && now.weekday.startsWith(now.weekday_short) }}\n\
             }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 8)), "now.time did not render as its fields say");
}

#[test]
fn inputs_and_dialogs_take_the_theme_accent_in_a_dark_theme() {
    let d = tmp("accent");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "screen:theme = t\n\
         theme t {\n\
             extends = dark\n\
             accent = #e040fb\n\
             style .alt:checked { color = #00ff00 }\n\
         }\n\
         action sure { confirm = Sure?; run = true }\n\
         page home {\n\
             column {\n\
                 row { toggle { value = true }  toggle { value = true; class = alt }  toggle { value = false } }\n\
                 slider { width = 300; value = 60 }\n\
                 button { label = Go; action = sure }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let accent = |p| close(p, [0xe0, 0x40, 0xfb], 10);
    let lvgl_blue = |p| close(p, [0x21, 0x96, 0xf3], 24) || close(p, [0x1e, 0x88, 0xe5], 24);
    let img = shot(&conf, &["--settle", "50"]);
    // The toggle that is on, and a slider's indicator, in the accent.
    assert!(any_in_rows(&img, 100..140, accent), "the toggle is not in the accent");
    assert!(any_in_rows(&img, 150..180, accent), "the slider is not in the accent");
    // A `:checked` style reaches the toggle's on color.
    assert!(any_in_rows(&img, 100..140, |p| close(p, [0, 0xff, 0], 8)), "the toggle's :checked style did not apply");
    assert!(!img.any(lvgl_blue), "LVGL's blue is still drawn");
    // The confirm dialog's buttons too.
    // The confirm dialog: its box in `surface`, its buttons in the
    // accent (the page under it is dimmed).
    let img = shot(&conf, &["--settle", "50", "--tap", "60,208"]);
    assert!(any_in_rows(&img, 225..260, |p| close(p, [0x1e, 0x1e, 0x1e], 4)), "the dialog is not on `surface`");
    assert!(any_in_rows(&img, 265..300, accent), "the dialog's buttons are not in the accent");
    assert!(!img.any(lvgl_blue), "a dialog drew LVGL's blue");
}

#[test]
fn a_seconds_clock_does_not_close_an_open_dropdown() {
    let d = tmp("open-list");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "page home { column {\n\
             text { value = {{ now.second }} }\n\
             select { width = 140; options = {{ ['a', 'b', 'c'] }}; value = b }\n\
         } }\n",
    )
    .unwrap();
    // The list (its chosen row in the accent) is under the select.
    let open = |img: &Image| any_in_rows(img, 160..290, |p| close(p, [0x21, 0x96, 0xf3], 12));
    // Open, then two and a half seconds of the clock: still open.
    let img = shot(&conf, &["--tap", "100,135", "--wait", "2500"]);
    assert!(open(&img), "the clock's redraw closed the list");
    // Picked: the list closes and the page is redrawn.
    let img = shot(&conf, &["--tap", "100,135", "--wait", "1200", "--tap", "100,188", "--wait", "1200"]);
    assert!(!open(&img), "the list stayed open after a pick");
}

#[test]
fn only_a_scroll_scrolls_and_other_blocks_clip() {
    let d = tmp("clip");
    let conf = d.join("p.conf");
    // Text in the box's own blue, overflowing it: anything else drawn
    // inside is a scrollbar.
    std::fs::write(
        &conf,
        "page home {\n\
             column { width = 200; height = 60; padding = 0; radius = 0; border = 0; background = #0000ff; color = #0000ff\n\
                 text { value = one } text { value = two } text { value = three } text { value = four } text { value = five } }\n\
             scroll { width = 200; height = 60; grow = 0; padding = 0; radius = 0; border = 0; background = #00ff00; color = #00ff00\n\
                 text { value = one } text { value = two } text { value = three } text { value = four } text { value = five } }\n\
         }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let inside = |c: [u8; 3]| {
        let b = bbox(&img, |p| close(p, c, 8)).expect("box not drawn");
        (b.1 + 2..b.3 - 2).flat_map(|y| (b.0 + 2..b.2 - 2).map(move |x| (x, y))).any(|(x, y)| !close(img.px(x, y), c, 8))
    };
    assert!(!inside([0, 0, 0xff]), "a column drew a scrollbar");
    assert!(inside([0, 0xff, 0]), "a scroll lost its scrollbar");
}

#[test]
fn text_on_the_accent_reads_and_on_accent_sets_it() {
    let d = tmp("on-accent");
    let conf = d.join("p.conf");
    let page = "page home { button { width = 200; height = 60; label = WWWWWW } }\n";
    // A pale accent: black text, not white (1.48:1).
    std::fs::write(&conf, format!("screen:theme = t\ntheme t {{ extends = dark; accent = #5eead4 }}\n{page}")).unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    let b = bbox(&img, |p| close(p, [0x5e, 0xea, 0xd4], 8)).expect("no button");
    let on_button = |f: &dyn Fn([u8; 3]) -> bool| (b.1..b.3).any(|y| (b.0..b.2).any(|x| f(img.px(x, y))));
    assert!(on_button(&|p| p.iter().all(|&c| c < 0x30)), "no dark text on a pale accent");
    assert!(!on_button(&|p| p == [0xff, 0xff, 0xff]), "white text on a pale accent");
    // The theme's `on_accent` wins.
    std::fs::write(&conf, format!("screen:theme = t\ntheme t {{ extends = dark; accent = #5eead4; on_accent = #ff0000 }}\n{page}")).unwrap();
    let img = shot(&conf, &["--settle", "50"]);
    assert!(img.any(|p| close(p, [0xff, 0, 0], 40)), "on_accent not used");
}

/// One 30x30 square per check in a row, green when its expression
/// holds and red when not (ADR 0024 phase 0: scopes are layered, not
/// copied). Returns how many green and red squares the picture shows.
fn scope_checks(taps: &[&str]) -> (usize, usize) {
    let d = tmp("scopes");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var k: int = 3\n\
         component ok {\n\
             param c: bool\n\
             rect { width = 30; height = 30; background = {{ c ? '#00ff00' : '#ff0000' }} }\n\
         }\n\
         # A param spelled like a global hides it inside only.\n\
         component shadow {\n\
             param vars: string\n\
             param sources: int = 7\n\
             ok { c = {{ vars == 'mine' && sources == 7 }} }\n\
         }\n\
         component inner {\n\
             param a: int\n\
             param b: int\n\
             # Its own `a` wins over the caller's; the caller's other\n\
             # names are not in scope (`paneld check`).\n\
             ok { c = {{ a == 5 && b == 2 }} }\n\
         }\n\
         component outer {\n\
             param a: int\n\
             param outer_only: int = 9\n\
             row {\n\
                 gap = 4\n\
                 inner { a = 5; b = {{ a + 1 }} }\n\
                 ok { c = {{ a == 1 }} }\n\
             }\n\
         }\n\
         component wrap {\n\
             param v: int = 9\n\
             row {\n\
                 gap = 4\n\
                 ok { c = {{ v == 9 }} }\n\
                 slot { }\n\
             }\n\
         }\n\
         component live {\n\
             param p: int\n\
             ok { c = {{ p == vars.k && vars.k == (page.name == 'home' ? vars.k : -1) }} }\n\
         }\n\
         page home {\n\
             row {\n\
                 gap = 4\n\
                 shadow { vars = mine }\n\
                 ok { c = {{ vars.k >= 3 }} }\n\
                 repeat vars in {{ [1] }} {\n\
                     ok { c = {{ vars == 1 }} }\n\
                 }\n\
                 ok { c = {{ vars.k >= 3 }} }\n\
                 outer { a = 1 }\n\
                 repeat v in {{ [7] }} {\n\
                     wrap { ok { c = {{ v == 7 }} } }\n\
                 }\n\
                 ok { c = {{ [1, 2].all(vars, vars > 0) && vars.k >= 3 }} }\n\
                 live { p = {{ vars.k }} }\n\
                 ok { c = {{ vars.k == 3 }} }\n\
                 button { width = 60; height = 30; label = up; on_tap { set { k = 4 } } }\n\
             }\n\
         }\n",
    )
    .unwrap();
    let mut args = vec!["--size", "800x120", "--settle", "100"];
    for t in taps {
        args.extend(["--tap", t]);
    }
    let img = shot(&conf, &args);
    // Squares do not share columns: count runs of columns with a
    // green (red) pixel anywhere.
    let col = |x: usize, c: [u8; 3]| (0..img.h).any(|y| close(img.px(x, y), c, 16));
    let runs = |c: [u8; 3]| (0..img.w).filter(|&x| col(x, c) && (x == 0 || !col(x - 1, c))).count();
    let (green, red) = (runs([0, 0xff, 0]), runs([0xff, 0, 0]));
    (green, red)
}

#[test]
fn params_and_repeat_bindings_hide_outer_names_only_inside() {
    // shadow, after shadow, repeat `vars`, after it, inner, outer, wrap
    // body, slot, macro, live, the last check: 11 squares.
    assert_eq!(scope_checks(&[]), (11, 0));
}

#[test]
fn a_var_set_after_a_component_call_reaches_its_params_and_body() {
    // After `k = 4` the last check (`vars.k == 3`) turns red; `live`
    // sees the new value through its param and directly.
    assert_eq!(scope_checks(&["515,99"]), (10, 1));
}

// ---- ADR 0024: pages updated in place ----

/// `paneld shot` with `PANELD_PERF` (and verify): what it printed.
fn shot_perf(config: &Path, extra: &[&str]) -> String {
    let run = Command::new(env!("CARGO_BIN_EXE_paneld"))
        .arg("shot")
        .arg("-c")
        .arg(config)
        .arg("-o")
        .arg(config.with_extension("png"))
        .args(extra)
        .env("PANELD_VERIFY_UPDATES", "1")
        .env("PANELD_PERF", "1")
        .output()
        .expect("run paneld shot");
    let err = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(run.status.success(), "paneld shot failed: {}\n{err}", run.status);
    err
}

/// `name=value` fields of a perf line.
fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split_whitespace().find_map(|w| w.strip_prefix(name).and_then(|r| r.strip_prefix('='))).unwrap_or("")
}

#[test]
fn a_value_change_updates_only_what_shows_it() {
    let d = tmp("in-place");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "source cpu { run = echo 1; poll = 1h }\n\
         page home { column {\n\
             text { value = CPU {{ sources.cpu.value }} }\n\
             text { value = one }\n\
             rect { width = 300; height = 100; background = #336699 }\n\
             text { value = {{ ago(sources.cpu.at) }} }\n\
         } }\n",
    )
    .unwrap();
    // A second apart, so the same output is a new `at`.
    let log = shot_perf(&conf, &["--no-sources", "--source", "cpu=42", "--wait", "1100", "--source", "cpu=42", "--wait", "1100", "--source", "cpu=50"]);
    let lines: Vec<&str> = log.lines().filter(|l| !l.starts_with("perf event")).collect();
    // Drawn once; every change after is an update, with no object made
    // (every one kept), none built.
    assert_eq!(lines.iter().filter(|l| l.starts_with("perf build")).count(), 0, "{log}");
    let updates: Vec<&&str> = lines.iter().filter(|l| l.starts_with("perf update")).collect();
    assert_eq!(updates.len(), 3, "{log}");
    assert!(updates.iter().all(|l| field(l, "made") == "0"), "{log}");
    // The same output again: only its `at` moved; its reader still
    // reads `just now`, and no `value` reader is looked at.
    assert!(!field(updates[1], "changes").contains("cpu.value") && field(updates[1], "changes").contains("cpu.at"), "{}", updates[1]);
    assert_eq!(field(updates[1], "reapplied"), "0", "{}", updates[1]);
    assert!(field(updates[2], "changes").contains("cpu.value"), "{}", updates[2]);
    assert_eq!(field(updates[2], "reapplied"), "1", "{}", updates[2]);
    // Each frame after an update redraws a line of text, not the page.
    for u in &updates {
        let at = lines.iter().position(|l| l == *u).unwrap();
        let frame = lines[at..].iter().find(|l| l.starts_with("perf t=")).expect("a frame after the update");
        let px: u64 = field(frame, "px").parse().unwrap();
        assert!(px < 20_000, "{px} px redrawn for a label: {frame}");
    }
}

#[test]
fn the_page_takes_taps_after_a_dialog_closes() {
    // The page's bindings are its objects': a dialog over it adds its
    // own and takes them along when it goes (ADR 0024).
    let d = tmp("after-dialog");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var a: bool = false\nvar b: bool = false\n\
         page home {\n\
             button { width = 200; height = 60; label = ask; on_tap { set { a = true }; confirm = Sure? } }\n\
             button { width = 200; height = 60; label = b; on_tap { set { b = true } } }\n\
             rect { width = 100; height = 40; background = {{ vars.a && vars.b ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    // Under the shell's title bar: `ask`, OK in the dialog, then `b`.
    let img = shot(&conf, &["--tap", "100,95", "--settle", "200", "--tap", "463,282", "--settle", "200", "--tap", "100,160", "--settle", "200"]);
    assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "the page took no tap after the dialog");
}

#[test]
fn a_grid_child_shown_again_takes_its_place_as_in_a_fresh_render() {
    let d = tmp("grid-visible");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var show: bool = true\n\
         page home { grid { columns = 2\n\
             rect { width = 50; height = 50; background = #ff0000 }\n\
             rect { width = 50; height = 50; background = #00ff00; visible = {{ vars.show }} }\n\
             rect { width = 50; height = 50; background = #0000ff }\n\
         } }\n",
    )
    .unwrap();
    // `--verify` compares each step's picture with a fresh render.
    let img = shot(&conf, &["--verify", "--set", "show=false"]);
    let blue = bbox(&img, |p| close(p, [0, 0, 0xff], 16)).unwrap();
    let red = bbox(&img, |p| close(p, [0xff, 0, 0], 16)).unwrap();
    assert!(blue.1 < red.3, "the blue cell did not move up beside the red one");
    shot(&conf, &["--verify", "--set", "show=false", "--set", "show=true"]);
}

#[test]
fn a_running_button_and_its_end_look_as_a_fresh_render() {
    let d = tmp("busy-verify");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "action slow { busy = wait; run = sleep 0.6 }\n\
         action spin { run = sleep 0.6 }\n\
         page home {\n\
             button { width = 200; height = 60; label = go; on_tap = slow }\n\
             button { width = 200; height = 60; icon = refresh; label = go; on_tap = spin }\n\
         }\n",
    )
    .unwrap();
    shot(&conf, &["--verify", "--tap", "100,95", "--tap", "100,160", "--settle", "100", "--wait", "1200"]);
}

/// The configs random changes are checked over: the shipped shell and
/// alerts page, the examples, and the rack config when it is next to
/// this repo (read only, nothing of it runs).
fn fuzz_configs() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out: Vec<PathBuf> = ["homelab", "smart-home", "system"].iter().map(|e| root.join("examples").join(e).join("paneld.conf")).collect();
    let rack = root.join("../paneld-configs/paneld.conf");
    if rack.is_file() {
        out.push(rack);
    }
    out
}

/// The pages of `conf` (`page <name>` at the start of a line), and the
/// shipped alerts page.
fn pages_of(conf: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(conf).unwrap();
    let mut pages: Vec<String> =
        text.lines().filter_map(|l| l.strip_prefix("page ")).filter_map(|r| r.split_whitespace().next()).map(str::to_string).collect();
    pages.push("alerts".into());
    pages
}

#[test]
fn random_changes_update_every_page_as_a_fresh_render_would_draw_it() {
    // A minimal config: the shipped shell and alerts page.
    let d = tmp("fuzz");
    let lib = d.join("p.conf");
    std::fs::write(&lib, "var x: int = 0\npage home { nav = 1; text { value = {{ vars.x }} } }\n").unwrap();
    let mut configs = vec![lib];
    configs.extend(fuzz_configs());
    for conf in configs {
        for page in pages_of(&conf) {
            for seed in ["5", "17", "40"] {
                let out = d.join(format!("fuzz-{page}-{seed}.png"));
                let run = Command::new(env!("CARGO_BIN_EXE_paneld"))
                    .arg("shot")
                    .arg("-c")
                    .arg(&conf)
                    .arg("-o")
                    .arg(&out)
                    .args(["--no-sources", "--settle", "50", "--verify", "--page", &page, "--fuzz", "80", "--seed", seed])
                    .env("PANELD_VERIFY_UPDATES", "1")
                    .output()
                    .expect("run paneld shot");
                assert!(
                    run.status.success(),
                    "{} page {page} seed {seed}: {}\n{}",
                    conf.display(),
                    run.status,
                    String::from_utf8_lossy(&run.stderr)
                );
            }
        }
    }
}

#[test]
fn a_spinner_shown_again_by_an_update_keeps_turning() {
    // Review: setting a node up again deleted LVGL's own animations; the
    // verify dump counts those that run for good (a spinner's turn).
    let d = tmp("spin-again");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var show: bool = true\nvar c: int = 0\n\
         page home { column {\n\
             spinner { width = 60; height = 60; visible = {{ vars.show }} }\n\
             text { value = {{ vars.c }} }\n\
         } }\n",
    )
    .unwrap();
    shot(&conf, &["--set", "show=false", "--set", "show=true", "--set", "c=2"]);
}

#[test]
fn a_node_set_up_again_by_its_call_keeps_what_it_shows_in_step() {
    // Review: a root set up again because its call's own keys changed
    // kept the fingerprint from before, so a later change back to it
    // was skipped and the old color stayed.
    let d = tmp("fp-forced");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var a: int = 0\n\
         component card {\n\
             param p: int = 0\n\
             rect { width = 100; height = 50; background = {{ p > 150 ? '#ff0000' : '#00ff00' }} }\n\
         }\n\
         page home { column {\n\
             card { p = {{ vars.a }}; opacity = {{ vars.a > 100 ? 0.5 : 1 }} }\n\
         } }\n",
    )
    .unwrap();
    let img = shot(&conf, &["--set", "a=1", "--set", "a=50", "--set", "a=200", "--set", "a=120"]);
    let green = |p: [u8; 3]| p[1] > 0x80 && p[0] < 0x80 && p[2] < 0x80;
    let red = |p: [u8; 3]| p[0] > 0x80 && p[1] < 0x80 && p[2] < 0x80;
    assert!(img.any(green) && !img.any(red), "the card stayed red at a = 120");
}

#[test]
fn an_ago_in_a_component_arg_is_timed() {
    // Review: an `ago()` evaluated in a component arg was never
    // scheduled. Verify compares the page's `ago()` timing with a
    // fresh render's after each update.
    let d = tmp("ago-arg");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "source x { run = echo 1; poll = 1h }\n\
         component age_of { param t: string = \"\"; text { value = {{ t }} } }\n\
         page home { column { age_of { t = {{ ago(sources.x.at) }} } } }\n\
         page two { title = {{ ago(sources.x.at) }}; column { text { value = x } } }\n",
    )
    .unwrap();
    shot(&conf, &["--no-sources", "--source", "x=1", "--source", "x=2"]);
    // In a page title (`page`, `nav`), the same.
    shot(&conf, &["--no-sources", "--page", "two", "--source", "x=1", "--source", "x=2"]);
}

#[test]
fn a_var_set_while_the_page_is_updated_is_shown() {
    // Review: the vars were told apart before the page's context was
    // taken; a `set` by the action worker in between was drawn without
    // its change (the rect kept red; verify said so under load).
    // `PANELD_TEST_DELAY_MS` holds the loop there long enough for it.
    let d = tmp("var-race");
    let conf = d.join("p.conf");
    std::fs::write(
        &conf,
        "var on: bool = false\n\
         page home {\n\
             toggle { on_change { set { on = {{ event.value }} }; confirm = Sure? } }\n\
             rect { width = 100; height = 40; background = {{ vars.on ? '#00ff00' : '#ff0000' }} }\n\
         }\n",
    )
    .unwrap();
    for _ in 0..3 {
        let run = Command::new(env!("CARGO_BIN_EXE_paneld"))
            .args(["shot", "-c", conf.to_str().unwrap(), "-o", d.join("p.png").to_str().unwrap()])
            .args(["--tap", "38,77", "--tap", "463,282", "--settle", "200"])
            .env("PANELD_VERIFY_UPDATES", "1")
            .env("PANELD_TEST_DELAY_MS", "30")
            .output()
            .expect("run paneld shot");
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
        let img = decode(&std::fs::read(d.join("p.png")).unwrap());
        assert!(img.any(|p| close(p, [0, 0xff, 0], 16)), "the set was not drawn");
    }
}
