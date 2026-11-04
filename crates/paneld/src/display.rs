//! Pick a display + input backend from the config's `display` block
//! (ADR 0012). On a Linux device the daemon prefers DRM/KMS, falls
//! back to fbdev; on a PC it uses SDL. The config can override.
//!
//! The three backends are compiled into every Linux build; the
//! `display { backend }` option chooses which one runs.

use paneld_config::parse::{AssignTarget, Block, Document, Item};
use paneld_ui::{Ui, Window};

#[derive(Clone, Debug)]
pub struct DisplayConfig {
    pub backend: Backend,
    /// Display device path (DRM card or fbdev).
    pub device: Option<String>,
    /// Touch input device path (evdev). SDL ignores this.
    pub touch: Option<String>,
    pub width: i32,
    pub height: i32,
    #[cfg_attr(not(feature = "sdl"), allow(dead_code))]
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sdl,
    Drm,
    Fbdev,
}

impl DisplayConfig {
    pub fn from_doc(doc: &Document) -> Self {
        let mut cfg = Self::default();
        let block = find_display_block(doc);
        if let Some(b) = block {
            if let Some(v) = find_simple(b, "backend") {
                cfg.backend = parse_backend(v.trim()).unwrap_or(cfg.backend);
            }
            cfg.device = find_simple(b, "device").map(paneld_config::value::parse_string);
            cfg.touch = find_simple(b, "touch").map(paneld_config::value::parse_string);
            if let Some(w) = find_simple(b, "width").and_then(|v| v.trim().parse().ok()) {
                cfg.width = w;
            }
            if let Some(h) = find_simple(b, "height").and_then(|v| v.trim().parse().ok()) {
                cfg.height = h;
            }
        }
        cfg
    }

    /// Open the display + attach input. Returns `None` if the chosen
    /// backend's device isn't openable; the caller decides whether
    /// to fall back.
    pub fn open(&self, ui: &Ui) -> Option<Window> {
        match self.backend {
            #[cfg(feature = "sdl")]
            Backend::Sdl => Some(ui.open_sdl_window(self.width, self.height, &self.title)),
            #[cfg(not(feature = "sdl"))]
            Backend::Sdl => {
                eprintln!("paneld: this build doesn't include the SDL backend");
                None
            }
            #[cfg(feature = "drm")]
            Backend::Drm => {
                let dev = self.device.as_deref().unwrap_or("/dev/dri/card1");
                let w = ui.open_drm_display(dev)?;
                if let Some(touch) = self.touch.as_deref() {
                    if !ui.add_evdev_pointer(touch) {
                        eprintln!("paneld: evdev `{touch}` could not be opened");
                    }
                }
                Some(w)
            }
            #[cfg(not(feature = "drm"))]
            Backend::Drm => {
                eprintln!("paneld: this build doesn't include the DRM backend");
                None
            }
            Backend::Fbdev => {
                let dev = self.device.as_deref().unwrap_or("/dev/fb0");
                let w = ui.open_fbdev_display(dev)?;
                if let Some(touch) = self.touch.as_deref() {
                    if !ui.add_evdev_pointer(touch) {
                        eprintln!("paneld: evdev `{touch}` could not be opened");
                    }
                }
                Some(w)
            }
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        // SDL by default so paneld "just works" on a PC without a
        // display block. A later phase may auto-detect DRM on the
        // Pi, but config-driven keeps the surface explicit.
        Self {
            backend: Backend::Sdl,
            device: None,
            touch: None,
            width: 800,
            height: 480,
            title: "paneld".to_string(),
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
    fn without_a_display_block_it_is_an_sdl_window() {
        let c = cfg("page home { }\n");
        assert_eq!(c.backend, Backend::Sdl);
        assert_eq!((c.width, c.height), (800, 480));
        assert_eq!((c.device, c.touch), (None, None));
    }

    #[test]
    fn the_block_sets_backend_devices_and_size() {
        let c = cfg("display {\n backend = fbdev\n device = /dev/fb1\n touch = /dev/input/event3\n width = 1024\n height = 600\n}\n");
        assert_eq!(c.backend, Backend::Fbdev);
        assert_eq!(c.device.as_deref(), Some("/dev/fb1"));
        assert_eq!(c.touch.as_deref(), Some("/dev/input/event3"));
        assert_eq!((c.width, c.height), (1024, 600));
        assert_eq!(cfg("display { backend = drm }\n").backend, Backend::Drm);
        assert_eq!(cfg("display { device = \"/dev/fb 1\" }\n").device.as_deref(), Some("/dev/fb 1"));
    }

    #[test]
    fn later_options_win_and_bad_values_keep_the_default() {
        let c = cfg("display { backend = fbdev; width = 100 }\ndisplay:backend = drm\ndisplay { width = 200 }\n");
        assert_eq!(c.backend, Backend::Drm);
        assert_eq!(c.width, 200);
        let c = cfg("display { backend = vga; width = wide }\n");
        assert_eq!(c.backend, Backend::Sdl);
        assert_eq!(c.width, 800);
    }

    #[test]
    fn a_device_that_cannot_open_is_none() {
        let ui = paneld_ui::Ui::init().unwrap();
        let c = DisplayConfig { backend: Backend::Fbdev, device: Some("/nonexistent/fb".into()), ..DisplayConfig::default() };
        assert!(c.open(&ui).is_none());
    }
}
