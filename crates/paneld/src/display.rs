//! Pick a display + input backend from the config's `display` block
//! (schema 13, ADR 0012). Without `backend` the daemon looks at the
//! machine: an SDL window on a desktop, DRM/KMS when a card has a
//! connected screen, fbdev when `/dev/fb0` exists, else SDL. Without
//! `touch` it attaches the first touchscreen in
//! `/proc/bus/input/devices`. `rotation` turns the picture and the
//! touch input with it.

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_ui::{Ui, Window};

#[derive(Clone, Debug)]
pub struct DisplayConfig {
    /// `None`: picked from the machine ([`pick_backend`]).
    pub backend: Option<Backend>,
    /// Display device path (DRM card or fbdev).
    pub device: Option<String>,
    /// Touch input (evdev). SDL ignores it.
    pub touch: Touch,
    /// The panel's size, unrotated. Only SDL uses it (DRM and fbdev
    /// take the display's own mode).
    pub width: i32,
    pub height: i32,
    /// How far the panel is mounted turned clockwise: 0, 90, 180 or 270.
    pub rotation: i32,
    #[cfg_attr(not(feature = "sdl"), allow(dead_code))]
    pub title: String,
    /// Where `backend` (else `device`) is set, as `file:line:col`, for
    /// the error shown when it does not open.
    pub origin: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sdl,
    Drm,
    Fbdev,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Touch {
    /// The first touchscreen found ([`find_touchscreen`]).
    Auto,
    /// `touch = none`: no touch input.
    Off,
    Path(String),
}

/// What the machine offers, for [`pick_backend`].
#[derive(Clone, Debug, Default)]
pub struct Probe {
    /// Backends compiled into this binary.
    pub sdl: bool,
    pub drm: bool,
    /// A desktop session (`WAYLAND_DISPLAY` or `DISPLAY` is set).
    pub desktop: bool,
    /// A DRM card with a connected connector, e.g. `/dev/dri/card0`.
    pub drm_card: Option<String>,
    /// `/dev/fb0` exists.
    pub fbdev: bool,
}

impl Probe {
    pub fn system() -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self {
            sdl: cfg!(feature = "sdl"),
            drm: cfg!(feature = "drm"),
            desktop: set("WAYLAND_DISPLAY") || set("DISPLAY"),
            drm_card: drm_card_in_sysfs(),
            fbdev: std::path::Path::new("/dev/fb0").exists(),
        }
    }
}

/// The backends to try when the config names none, in this order:
/// SDL when built with it and a desktop session runs; DRM when built
/// with it and a card has a connected screen; fbdev when `/dev/fb0`
/// exists; SDL when built with it. With none of these, the first
/// compiled device backend (which then fails to open and says so).
///
/// A desktop session is only `WAYLAND_DISPLAY` or `DISPLAY` being set:
/// a desktop build started over `ssh -X` picks SDL.
pub fn auto_order(p: &Probe) -> Vec<Backend> {
    let mut v = Vec::new();
    if p.sdl && p.desktop {
        v.push(Backend::Sdl);
    }
    if p.drm && p.drm_card.is_some() {
        v.push(Backend::Drm);
    }
    if p.fbdev {
        v.push(Backend::Fbdev);
    }
    if p.sdl && !v.contains(&Backend::Sdl) {
        v.push(Backend::Sdl);
    }
    if v.is_empty() {
        v.push(if p.drm { Backend::Drm } else { Backend::Fbdev });
    }
    v
}

/// The backend picked when the config names none.
pub fn pick_backend(p: &Probe) -> Backend {
    auto_order(p)[0]
}

/// One backend to try, with its device (`None` for SDL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub backend: Backend,
    pub device: Option<String>,
    /// Named by the config's `backend` / `device`.
    pub from_config: bool,
}

impl std::fmt::Display for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.backend {
            Backend::Sdl => "sdl",
            Backend::Drm => "drm",
            Backend::Fbdev => "fbdev",
        };
        match &self.device {
            Some(d) => write!(f, "{name} on {d}"),
            None => write!(f, "{name}"),
        }
    }
}

fn default_device(b: Backend, p: &Probe) -> Option<String> {
    match b {
        Backend::Sdl => None,
        Backend::Drm => Some(p.drm_card.clone().unwrap_or_else(|| "/dev/dri/card0".into())),
        Backend::Fbdev => Some("/dev/fb0".into()),
    }
}

/// What to try, in order: the config's `backend` / `device` first if
/// it sets either, then [`auto_order`]. Each is tried once.
pub fn plan(cfg: &DisplayConfig, p: &Probe) -> Vec<Attempt> {
    let mut v: Vec<Attempt> = Vec::new();
    if cfg.backend.is_some() || cfg.device.is_some() {
        let backend = cfg.backend.unwrap_or_else(|| pick_backend(p));
        let device = match backend {
            Backend::Sdl => None,
            _ => cfg.device.clone().or_else(|| default_device(backend, p)),
        };
        v.push(Attempt { backend, device, from_config: true });
    }
    for backend in auto_order(p) {
        let a = Attempt { backend, device: default_device(backend, p), from_config: false };
        if !v.iter().any(|t| t.backend == a.backend && t.device == a.device) {
            v.push(a);
        }
    }
    v
}

/// What [`DisplayConfig::open`] opened.
pub struct Opened {
    pub window: Window,
    /// The config's display did not open and another one did: the
    /// error to show, with the config location.
    pub problem: Option<String>,
}

/// The message for a config display that did not open when `used`
/// did.
pub fn problem_text(origin: Option<&str>, failed: &Attempt, used: &Attempt) -> String {
    let at = origin.map(|o| format!("{o}: ")).unwrap_or_default();
    format!("{at}display `{failed}` did not open; using {used} instead")
}

/// The first card (by number) with a connector whose `status` is
/// `connected`, from `/sys/class/drm` entries as (name, status), e.g.
/// ("card0-DSI-1", "connected"). LVGL's DRM driver takes that card's
/// first connected connector.
pub fn connected_card<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<String> {
    entries
        .into_iter()
        .filter(|(_, status)| status.trim() == "connected")
        .filter_map(|(name, _)| {
            let (card, _connector) = name.split_once('-')?;
            let n: u32 = card.strip_prefix("card")?.parse().ok()?;
            Some((n, card.to_string()))
        })
        .min()
        .map(|(_, card)| format!("/dev/dri/{card}"))
}

fn drm_card_in_sysfs() -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    let entries: Vec<(String, String)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            Some((name, status))
        })
        .collect();
    let card = connected_card(entries.iter().map(|(n, s)| (n.as_str(), s.as_str())))?;
    std::path::Path::new(&card).exists().then_some(card)
}

/// One device of `/proc/bus/input/devices` that looks like a
/// touchscreen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    pub name: String,
    /// e.g. `/dev/input/event0`.
    pub path: String,
}

const EV_KEY: u32 = 1;
const EV_ABS: u32 = 3;
const ABS_X: u32 = 0;
const ABS_Y: u32 = 1;
const ABS_MT_POSITION_X: u32 = 0x35;
const ABS_MT_POSITION_Y: u32 = 0x36;
const BTN_TOUCH: u32 = 0x14a;
const INPUT_PROP_POINTER: u32 = 0;
const INPUT_PROP_DIRECT: u32 = 1;

/// Is `bit` set in a `B:` bitmap of `/proc/bus/input/devices`: hex
/// words of the kernel's `long`, the highest first.
fn bit_set(words: &str, bit: u32, word_bits: u32) -> bool {
    let words: Vec<&str> = words.split_whitespace().collect();
    let idx = (bit / word_bits) as usize;
    if idx >= words.len() {
        return false;
    }
    let word = words[words.len() - 1 - idx];
    u64::from_str_radix(word, 16).is_ok_and(|w| (w >> (bit % word_bits)) & 1 == 1)
}

/// The touchscreens in `/proc/bus/input/devices` text, best first: a
/// device with absolute X/Y (single or multi-touch) and `BTN_TOUCH`,
/// not a touchpad (`INPUT_PROP_POINTER`); one marked as on the screen
/// (`INPUT_PROP_DIRECT`) before one that is not. Mice and keyboards
/// have no absolute axes and are skipped. `word_bits` is the kernel's
/// `long` size (64 on arm64 and x86_64).
pub fn find_touchscreens(text: &str, word_bits: u32) -> Vec<InputDevice> {
    let mut found: Vec<(bool, InputDevice)> = Vec::new();
    for block in text.split("\n\n") {
        let (mut name, mut event) = (String::new(), None);
        let (mut ev, mut key, mut abs, mut prop) = ("", "", "", "");
        for line in block.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                name = n.trim().trim_matches('"').to_string();
            } else if let Some(h) = line.strip_prefix("H: Handlers=") {
                event = h.split_whitespace().find(|w| w.starts_with("event") && w[5..].parse::<u32>().is_ok());
            } else if let Some(b) = line.strip_prefix("B: ") {
                let (k, v) = b.split_once('=').unwrap_or((b, ""));
                match k {
                    "EV" => ev = v,
                    "KEY" => key = v,
                    "ABS" => abs = v,
                    "PROP" => prop = v,
                    _ => {}
                }
            }
        }
        let Some(event) = event else { continue };
        let has = |m: &str, bit| bit_set(m, bit, word_bits);
        let xy = (has(abs, ABS_MT_POSITION_X) && has(abs, ABS_MT_POSITION_Y)) || (has(abs, ABS_X) && has(abs, ABS_Y));
        let touch = has(ev, EV_ABS) && has(ev, EV_KEY) && xy && has(key, BTN_TOUCH);
        if touch && !has(prop, INPUT_PROP_POINTER) {
            found.push((has(prop, INPUT_PROP_DIRECT), InputDevice { name, path: format!("/dev/input/{event}") }));
        }
    }
    // Stable: file order within each group.
    found.sort_by_key(|(direct, _)| !direct);
    found.into_iter().map(|(_, d)| d).collect()
}

/// The first touchscreen on this machine, if any.
pub fn find_touchscreen() -> Option<InputDevice> {
    let text = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    find_touchscreens(&text, usize::BITS).into_iter().next()
}

impl DisplayConfig {
    pub fn from_doc(doc: &Document) -> Self {
        Self::from_doc_at(doc, None)
    }

    /// [`DisplayConfig::from_doc`], noting where `backend` / `device`
    /// are set when `source` is given.
    pub fn from_doc_at(doc: &Document, source: Option<&paneld_config::SourceMap>) -> Self {
        let mut cfg = Self::default();
        let block = find_display_block(doc);
        if let Some(b) = block {
            if let Some(src) = source {
                cfg.origin = find_span(b, "backend").or_else(|| find_span(b, "device")).map(|span| {
                    let l = src.location(span);
                    format!("{}:{}:{}", l.path.display(), l.line, l.column)
                });
            }
            if let Some(v) = find_simple(b, "backend") {
                cfg.backend = parse_backend(v.trim());
            }
            cfg.device = find_simple(b, "device").map(paneld_config::value::parse_string);
            if let Some(t) = find_simple(b, "touch").map(paneld_config::value::parse_string) {
                cfg.touch = match t.as_str() {
                    "none" => Touch::Off,
                    "" => Touch::Auto,
                    _ => Touch::Path(t),
                };
            }
            if let Some(w) = find_simple(b, "width").and_then(|v| v.trim().parse().ok()).filter(|&w: &i32| w > 0) {
                cfg.width = w;
            }
            if let Some(h) = find_simple(b, "height").and_then(|v| v.trim().parse().ok()).filter(|&h: &i32| h > 0) {
                cfg.height = h;
            }
            if let Some(r) = find_simple(b, "rotation").and_then(|v| v.trim().parse().ok()) {
                if matches!(r, 0 | 90 | 180 | 270) {
                    cfg.rotation = r;
                }
            }
        }
        cfg
    }

    /// Open the display, rotate it and attach touch input. Tries
    /// [`plan`] in order and logs each step; `None` only if nothing
    /// opens.
    pub fn open(&self, ui: &Ui) -> Option<Opened> {
        let probe = Probe::system();
        let attempts = plan(self, &probe);
        let mut failed: Option<&Attempt> = None;
        for a in &attempts {
            let how = if a.from_config { "set in the config" } else { "picked" };
            eprintln!("paneld: display: trying {a} ({how})");
            match self.open_one(ui, a) {
                Some(window) => {
                    eprintln!("paneld: display: {a}");
                    let problem = failed.map(|f| problem_text(self.origin.as_deref(), f, a));
                    if let Some(p) = &problem {
                        eprintln!("paneld: {p}");
                    }
                    if a.backend != Backend::Sdl {
                        self.rotate(&window);
                        self.attach_touch(ui);
                    }
                    return Some(Opened { window, problem });
                }
                None => {
                    eprintln!("paneld: display: {a} did not open");
                    if a.from_config {
                        failed = Some(a);
                    }
                }
            }
        }
        None
    }

    fn open_one(&self, ui: &Ui, a: &Attempt) -> Option<Window> {
        let dev = a.device.as_deref().unwrap_or_default();
        match a.backend {
            #[cfg(feature = "sdl")]
            Backend::Sdl => {
                // A desktop window opens upright at the rotated size.
                let (w, h) = if self.rotation % 180 == 90 { (self.height, self.width) } else { (self.width, self.height) };
                Some(ui.open_sdl_window(w, h, &self.title))
            }
            #[cfg(not(feature = "sdl"))]
            Backend::Sdl => {
                eprintln!("paneld: display: this build doesn't include the SDL backend");
                None
            }
            #[cfg(feature = "drm")]
            Backend::Drm => ui.try_open_drm_display(dev),
            #[cfg(not(feature = "drm"))]
            Backend::Drm => {
                let _ = dev;
                eprintln!("paneld: display: this build doesn't include the DRM backend");
                None
            }
            Backend::Fbdev => ui.try_open_fbdev_display(dev),
        }
    }

    fn rotate(&self, w: &Window) {
        if self.rotation == 0 {
            return;
        }
        if w.set_rotation(self.rotation) {
            eprintln!("paneld: display: rotated by {}", self.rotation);
        } else {
            eprintln!("paneld: display: could not rotate by {}; drawing unrotated", self.rotation);
        }
    }

    /// The `display` keys that differ from `other`: they apply only
    /// at start.
    pub fn changed_keys(&self, other: &Self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.backend != other.backend {
            v.push("backend");
        }
        if self.device != other.device {
            v.push("device");
        }
        if self.touch != other.touch {
            v.push("touch");
        }
        if (self.width, self.height) != (other.width, other.height) {
            v.push("width/height");
        }
        if self.rotation != other.rotation {
            v.push("rotation");
        }
        v
    }

    fn attach_touch(&self, ui: &Ui) {
        let (path, what) = match &self.touch {
            Touch::Off => {
                eprintln!("paneld: touch: off (`touch = none`)");
                return;
            }
            Touch::Path(p) => (p.clone(), "set in the config".to_string()),
            Touch::Auto => match find_touchscreen() {
                Some(d) => (d.path, format!("found: {}", d.name)),
                None => {
                    eprintln!("paneld: touch: no touchscreen found; no touch input (set `display:touch`)");
                    return;
                }
            },
        };
        if ui.add_evdev_pointer(&path) {
            eprintln!("paneld: touch: {path} ({what})");
        } else {
            eprintln!("paneld: touch: `{path}` could not be opened ({what})");
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            backend: None,
            device: None,
            touch: Touch::Auto,
            width: 800,
            height: 480,
            rotation: 0,
            title: "paneld".to_string(),
            origin: None,
        }
    }
}

fn parse_backend(s: &str) -> Option<Backend> {
    match s {
        "sdl" => Some(Backend::Sdl),
        "drm" => Some(Backend::Drm),
        "fbdev" => Some(Backend::Fbdev),
        _ => None,
    }
}

fn find_display_block(doc: &Document) -> Option<&Block> {
    for item in &doc.items {
        if let Item::Block(b) = item {
            if b.kind.text == "display" {
                return Some(b);
            }
        }
    }
    None
}

fn find_span(node: &Block, key: &str) -> Option<paneld_config::Span> {
    node.items.iter().find_map(|item| {
        let Item::Assign(a) = item else { return None };
        let AssignTarget::Simple { key: k } = &a.target else { return None };
        (k.text == key).then_some(a.span)
    })
}

/// The display as read at start, for [`note_reload`].
static STARTED: std::sync::OnceLock<DisplayConfig> = std::sync::OnceLock::new();

/// Remember the display the daemon started with.
pub fn set_started(cfg: DisplayConfig) {
    let _ = STARTED.set(cfg);
}

/// After a reload: log the `display` keys that changed since start
/// (they need a restart). Returns them.
pub fn note_reload(doc: &Document) -> Vec<&'static str> {
    let Some(started) = STARTED.get() else { return Vec::new() };
    let changed = DisplayConfig::from_doc(doc).changed_keys(started);
    if !changed.is_empty() {
        eprintln!("paneld: display: {} changed; restart paneld to apply", changed.join(", "));
    }
    changed
}

fn find_simple<'a>(node: &'a Block, key: &str) -> Option<&'a str> {
    for item in &node.items {
        let Item::Assign(a) = item else { continue };
        let AssignTarget::Simple { key: k } = &a.target else {
            continue;
        };
        if k.text == key {
            return Some(a.value.text.as_str());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(src: &str) -> DisplayConfig {
        let mut map = paneld_config::SourceMap::new();
        let id = map.add(std::path::PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = paneld_config::lex::tokenize(id, src);
        let (mut doc, diags) = paneld_config::parse(&toks, &map);
        assert!(diags.is_empty(), "{diags:?}");
        // As the daemon reads it: options merged, last one wins.
        paneld_config::apply_overrides(&mut doc);
        DisplayConfig::from_doc(&doc)
    }

    #[test]
    fn without_a_display_block_everything_is_picked() {
        let c = cfg("page home { }\n");
        assert_eq!(c.backend, None);
        assert_eq!((c.width, c.height, c.rotation), (800, 480, 0));
        assert_eq!((c.device, c.touch), (None, Touch::Auto));
    }

    #[test]
    fn the_block_sets_backend_devices_size_and_rotation() {
        let c = cfg("display {\n backend = fbdev\n device = /dev/fb1\n touch = /dev/input/event3\n width = 1024\n height = 600\n rotation = 270\n}\n");
        assert_eq!(c.backend, Some(Backend::Fbdev));
        assert_eq!(c.device.as_deref(), Some("/dev/fb1"));
        assert_eq!(c.touch, Touch::Path("/dev/input/event3".into()));
        assert_eq!((c.width, c.height, c.rotation), (1024, 600, 270));
        assert_eq!(cfg("display { backend = drm }\n").backend, Some(Backend::Drm));
        assert_eq!(cfg("display { device = \"/dev/fb 1\" }\n").device.as_deref(), Some("/dev/fb 1"));
        assert_eq!(cfg("display { touch = none }\n").touch, Touch::Off);
    }

    #[test]
    fn later_options_win_and_bad_values_keep_the_default() {
        let c = cfg("display { backend = fbdev; width = 100 }\ndisplay:backend = drm\ndisplay { width = 200 }\n");
        assert_eq!(c.backend, Some(Backend::Drm));
        assert_eq!(c.width, 200);
        let c = cfg("display { backend = vga; width = wide; height = -5; rotation = 45 }\n");
        assert_eq!(c.backend, None);
        assert_eq!((c.width, c.height, c.rotation), (800, 480, 0));
    }

    #[test]
    fn the_backend_is_picked_in_order() {
        let pi = Probe { drm: true, drm_card: Some("/dev/dri/card0".into()), fbdev: true, ..Probe::default() };
        assert_eq!(pick_backend(&pi), Backend::Drm);
        // No screen on any card (or drm not built): fbdev.
        assert_eq!(pick_backend(&Probe { drm_card: None, ..pi.clone() }), Backend::Fbdev);
        assert_eq!(pick_backend(&Probe { drm: false, ..pi.clone() }), Backend::Fbdev);
        // A desktop build on a desktop: a window, even with a card.
        let pc = Probe { sdl: true, desktop: true, ..pi.clone() };
        assert_eq!(pick_backend(&pc), Backend::Sdl);
        // The same build on a console: the screen.
        assert_eq!(pick_backend(&Probe { desktop: false, ..pc.clone() }), Backend::Drm);
        // Nothing found: SDL if built, else the device backend.
        assert_eq!(pick_backend(&Probe { sdl: true, ..Probe::default() }), Backend::Sdl);
        assert_eq!(pick_backend(&Probe { drm: true, ..Probe::default() }), Backend::Drm);
        assert_eq!(pick_backend(&Probe::default()), Backend::Fbdev);
        // A device build never picks SDL, desktop or not.
        assert_eq!(pick_backend(&Probe { desktop: true, ..pi }), Backend::Drm);
    }

    #[test]
    fn the_card_is_the_first_with_a_connected_screen() {
        // Pi 4: card0 is the 3D-only V3D, card1 has HDMI and DSI.
        let pi4 = [
            ("card1-HDMI-A-1", "disconnected\n"),
            ("card1-DSI-1", "connected\n"),
            ("card1-HDMI-A-2", "disconnected\n"),
            ("renderD128", ""),
            ("version", ""),
        ];
        assert_eq!(connected_card(pi4).as_deref(), Some("/dev/dri/card1"));
        let two = [("card2-DP-1", "connected"), ("card10-eDP-1", "connected"), ("card0-HDMI-A-1", "unknown")];
        assert_eq!(connected_card(two).as_deref(), Some("/dev/dri/card2"));
        assert_eq!(connected_card([("card0-HDMI-A-1", "disconnected")]), None);
    }

    // /proc/bus/input/devices of a Pi 3B+ (arm64) with the Waveshare
    // 5" DSI panel (ft5x06 touch on i2c 10-0038), a USB mouse and a
    // USB keyboard, plus a laptop touchpad.
    const DEVICES_64: &str = r#"I: Bus=0003 Vendor=046d Product=c077 Version=0111
N: Name="Logitech USB Optical Mouse"
P: Phys=usb-3f980000.usb-1.3/input0
S: Sysfs=/devices/platform/soc/3f980000.usb/usb1/1-1/1-1.3/1-1.3:1.0/0003:046D:C077.0001/input/input0
U: Uniq=
H: Handlers=mouse0 event0
B: PROP=0
B: EV=17
B: KEY=ff0000 0 0 0 0
B: REL=903
B: MSC=10

I: Bus=0003 Vendor=413c Product=2113 Version=0111
N: Name="Dell KB216 Wired Keyboard"
P: Phys=usb-3f980000.usb-1.2/input0
S: Sysfs=/devices/platform/soc/3f980000.usb/usb1/1-1/1-1.2/1-1.2:1.0/0003:413C:2113.0002/input/input1
U: Uniq=
H: Handlers=sysrq kbd leds event1
B: PROP=0
B: EV=120013
B: KEY=1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe
B: MSC=10
B: LED=7

I: Bus=0018 Vendor=0002 Product=0007 Version=01b1
N: Name="SynPS/2 Synaptics TouchPad"
P: Phys=isa0060/serio1/input0
S: Sysfs=/devices/platform/i8042/serio1/input/input5
U: Uniq=
H: Handlers=mouse1 event2
B: PROP=5
B: EV=b
B: KEY=e520 10000 0 0 0 0
B: ABS=660800011000003

I: Bus=0018 Vendor=0000 Product=0000 Version=0000
N: Name="10-0038 generic ft5x06 (79)"
P: Phys=
S: Sysfs=/devices/platform/soc/3f205000.i2c/i2c-11/i2c-10/10-0038/input/input3
U: Uniq=
H: Handlers=event3
B: PROP=2
B: EV=b
B: KEY=400 0 0 0 0 0
B: ABS=2608000000000003
"#;

    #[test]
    fn the_touchscreen_is_found_and_mice_keyboards_touchpads_are_not() {
        let found = find_touchscreens(DEVICES_64, 64);
        assert_eq!(
            found,
            vec![InputDevice { name: "10-0038 generic ft5x06 (79)".into(), path: "/dev/input/event3".into() }]
        );
        // Only the mouse and the keyboard: nothing.
        let first_two: String = DEVICES_64.split("\n\n").take(2).collect::<Vec<_>>().join("\n\n");
        assert!(find_touchscreens(&first_two, 64).is_empty());
    }

    #[test]
    fn a_32_bit_kernel_and_a_goodix_panel_without_prop_direct() {
        // 32-bit `long`: the same bitmaps split in more words; a
        // Goodix controller (Pi 7" / Waveshare DSI) without PROP, and a
        // second, direct one that wins over it.
        let text = r#"I: Bus=0018 Vendor=0416 Product=038f Version=1060
N: Name="Goodix Capacitive TouchScreen"
P: Phys=input/ts
H: Handlers=kbd event4
B: PROP=0
B: EV=b
B: KEY=400 0 0 0 0 0 0 0 0 0 0
B: ABS=2658000 3

I: Bus=0018 Vendor=0000 Product=0000 Version=0000
N: Name="10-0038 generic ft5x06 (79)"
H: Handlers=event7
B: PROP=2
B: EV=b
B: KEY=400 0 0 0 0 0 0 0 0 0 0
B: ABS=2608000 3
"#;
        let found = find_touchscreens(text, 32);
        let paths: Vec<&str> = found.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["/dev/input/event7", "/dev/input/event4"]);
        // Read with the wrong word size, BTN_TOUCH is not where it
        // looks: nothing, rather than a wrong device.
        assert!(find_touchscreens(text, 64).is_empty());
        // Single-touch only (ABS_X/ABS_Y, no MT axes) still counts.
        let st = "N: Name=\"ADS7846 Touchscreen\"\nH: Handlers=event0\nB: PROP=0\nB: EV=b\nB: KEY=400 0 0 0 0 0\nB: ABS=1000003\n";
        assert_eq!(find_touchscreens(st, 64).len(), 1);
    }

    #[test]
    fn a_device_that_cannot_open_leaves_nothing_behind() {
        let ui = paneld_ui::Ui::init().unwrap();
        assert!(ui.try_open_fbdev_display("/nonexistent/fb").is_none());
        assert!(ui.try_open_fbdev_display("/nonexistent/fb").is_none(), "a second try fails the same way");
    }

    fn at(backend: Backend, device: Option<&str>, from_config: bool) -> Attempt {
        Attempt { backend, device: device.map(str::to_string), from_config }
    }

    #[test]
    fn the_plan_tries_the_config_first_then_the_auto_order() {
        let pi = Probe { drm: true, drm_card: Some("/dev/dri/card0".into()), fbdev: true, ..Probe::default() };
        // Nothing set: drm, then fbdev.
        let auto = vec![at(Backend::Drm, Some("/dev/dri/card0"), false), at(Backend::Fbdev, Some("/dev/fb0"), false)];
        assert_eq!(plan(&DisplayConfig::default(), &pi), auto);
        // A wrong card in the config: tried, then the auto order.
        let c = DisplayConfig { backend: Some(Backend::Drm), device: Some("/dev/dri/card1".into()), ..DisplayConfig::default() };
        let mut want = vec![at(Backend::Drm, Some("/dev/dri/card1"), true)];
        want.extend(auto.clone());
        assert_eq!(plan(&c, &pi), want);
        // The config naming what the auto order would try anyway: once.
        let c = DisplayConfig { backend: Some(Backend::Fbdev), ..DisplayConfig::default() };
        assert_eq!(plan(&c, &pi), vec![at(Backend::Fbdev, Some("/dev/fb0"), true), auto[0].clone()]);
        // Only `device`: the picked backend on that device.
        let c = DisplayConfig { device: Some("/dev/dri/card2".into()), ..DisplayConfig::default() };
        assert_eq!(plan(&c, &pi)[0], at(Backend::Drm, Some("/dev/dri/card2"), true));
        // `backend = sdl` in a device build: tried (and fails), then the screen.
        let c = DisplayConfig { backend: Some(Backend::Sdl), ..DisplayConfig::default() };
        assert_eq!(plan(&c, &pi)[0], at(Backend::Sdl, None, true));
        assert_eq!(plan(&c, &pi).len(), 3);
        // A desktop build on a console with a screen: drm, fbdev, sdl.
        let pc = Probe { sdl: true, ..pi.clone() };
        let order: Vec<Backend> = plan(&DisplayConfig::default(), &pc).iter().map(|a| a.backend).collect();
        assert_eq!(order, [Backend::Drm, Backend::Fbdev, Backend::Sdl]);
    }

    #[test]
    fn a_config_display_that_fails_is_reported_with_its_location() {
        let mut map = paneld_config::SourceMap::new();
        let src = "page home { }\ndisplay {\n  rotation = 90\n  backend = drm\n}\n";
        let id = map.add(std::path::PathBuf::from("/etc/paneld/paneld.conf"), src.to_string());
        let (toks, _) = paneld_config::lex::tokenize(id, src);
        let (mut doc, _) = paneld_config::parse(&toks, &map);
        paneld_config::apply_overrides(&mut doc);
        let c = DisplayConfig::from_doc_at(&doc, Some(&map));
        assert_eq!(c.origin.as_deref(), Some("/etc/paneld/paneld.conf:4:3"));
        let msg = problem_text(
            c.origin.as_deref(),
            &at(Backend::Drm, Some("/dev/dri/card1"), true),
            &at(Backend::Fbdev, Some("/dev/fb0"), false),
        );
        assert_eq!(msg, "/etc/paneld/paneld.conf:4:3: display `drm on /dev/dri/card1` did not open; using fbdev on /dev/fb0 instead");
    }

    #[test]
    fn changed_display_keys_are_named() {
        let a = cfg("display { rotation = 90 }\n");
        assert!(a.changed_keys(&a.clone()).is_empty());
        let b = cfg("display { rotation = 180; backend = fbdev }\n");
        assert_eq!(b.changed_keys(&a), ["backend", "rotation"]);
    }
}
