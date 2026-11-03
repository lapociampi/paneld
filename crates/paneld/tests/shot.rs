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
