//! `background` and `shadow` values (schema 8.1): one parser for the
//! check and the renderer. Colors are left as written (a hex or a
//! theme color name); the caller resolves them.
//!
//! | Value | Form |
//! | --- | --- |
//! | color | `#rgb`, `#rrggbb`, `#rrggbbaa`, a theme color name |
//! | none | `none` or `transparent` |
//! | linear gradient | `linear <angle> <color> <color> [<color> <color>]` |
//! | radial gradient | `radial <color> <color> [<color> <color>]` |
//! | image | `<path>.png [cover \| contain \| fill \| tile]` |
//! | shadow | `none`, or `<size> [<color>] [<x> <y>]` |

/// Most color stops a gradient takes (`LV_GRADIENT_MAX_STOPS`).
pub const MAX_STOPS: usize = 4;

/// Largest shadow `size` in px: a software shadow costs about its
/// size squared per corner, every frame it is redrawn.
pub const MAX_SHADOW: i32 = 32;

/// The shadow color when a value names none: black at 38%.
pub const DEFAULT_SHADOW_COLOR: &str = "#00000060";

/// LVGL's decoded-image cache (`LV_CACHE_DEF_SIZE` in lv_conf.h): a
/// larger image would be decoded again on every redraw.
pub const IMAGE_CACHE_BYTES: u64 = 4 * 1024 * 1024;

/// The most pixels an image file may have to be drawn on a screen of
/// `screen_pixels`: twice the screen's (at least 800x480's), and no
/// more than the decoded-image cache holds (4 bytes a pixel). The
/// renderer and the check share it.
pub fn max_image_pixels(screen_pixels: u64) -> u64 {
    (2 * screen_pixels.max(800 * 480)).min(IMAGE_CACHE_BYTES / 4)
}

/// How an image background fills its node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Scaled to cover the node, cropped to it, centered (default).
    Cover,
    /// Scaled to fit inside the node, centered.
    Contain,
    /// Stretched to the node's size.
    Fill,
    /// At its own size, repeated from the top left.
    Tile,
}

impl Fit {
    pub fn parse(s: &str) -> Option<Fit> {
        Some(match s {
            "cover" => Fit::Cover,
            "contain" => Fit::Contain,
            "fill" => Fit::Fill,
            "tile" => Fit::Tile,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GradientKind {
    /// CSS angles: 0 bottom to top, 90 left to right, 180 top to
    /// bottom (the default look of a vertical fade), 270 right to left.
    Linear(u16),
    /// From the center out to the corners.
    Radial,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gradient<'a> {
    pub kind: GradientKind,
    /// 2 to [`MAX_STOPS`] colors, spread evenly, as written.
    pub stops: Vec<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Background<'a> {
    None,
    /// A hex color or a single bare name (a theme token: a color, or
    /// one holding another background value).
    Color(&'a str),
    Gradient(Gradient<'a>),
    Image { path: &'a str, fit: Fit },
}

/// A word that names a color: a hex, or a bare name (a theme token).
fn color_word(w: &str) -> bool {
    if w.starts_with('#') {
        return crate::value::parse_color(w).is_some();
    }
    is_name(w)
}

fn is_name(w: &str) -> bool {
    let b = w.as_bytes();
    !b.is_empty() && (b[0].is_ascii_alphabetic() || b[0] == b'_') && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// An image path: has a `/` or ends in `.png`, no `{{`.
fn image_word(w: &str) -> bool {
    !w.contains("{{") && (w.contains('/') || w.to_ascii_lowercase().ends_with(".png"))
}

/// Parse a `background` value (resolved, not an expression).
pub fn parse_background(text: &str) -> Result<Background<'_>, String> {
    let text = text.trim();
    let words: Vec<&str> = text.split_whitespace().collect();
    let Some(&first) = words.first() else {
        return Err("expected a background: a color, a gradient or an image path".to_string());
    };
    match first {
        "none" | "transparent" if words.len() == 1 => Ok(Background::None),
        "linear" | "radial" => {
            let (kind, colors) = if first == "linear" {
                let angle = words.get(1).map(|a| a.trim_end_matches("deg")).and_then(|a| a.parse::<i32>().ok());
                let Some(angle) = angle.filter(|a| (0..=360).contains(a)) else {
                    return Err(format!(
                        "a linear gradient is `linear <angle> <color> <color> ...` with the angle 0 to 360 \
                         (180: top to bottom), got `{text}`"
                    ));
                };
                (GradientKind::Linear((angle % 360) as u16), &words[2..])
            } else {
                (GradientKind::Radial, &words[1..])
            };
            if colors.len() < 2 || colors.len() > MAX_STOPS {
                return Err(format!("a gradient takes 2 to {MAX_STOPS} colors, got {} in `{text}`", colors.len()));
            }
            if let Some(bad) = colors.iter().find(|c| !color_word(c)) {
                return Err(format!("gradient stop `{bad}` is not a color (`#rrggbb[aa]` or a theme color name)"));
            }
            Ok(Background::Gradient(Gradient { kind, stops: colors.to_vec() }))
        }
        _ if words.len() == 1 && color_word(first) && !image_word(first) => Ok(Background::Color(first)),
        _ if image_word(first) => {
            if !first.to_ascii_lowercase().ends_with(".png") {
                return Err(format!("background image `{first}` must be a `.png` file"));
            }
            let fit = match words.get(1) {
                None => Fit::Cover,
                Some(w) => Fit::parse(w).ok_or_else(|| {
                    format!("background image fit `{w}`: expected `cover` (default), `contain`, `fill` or `tile`")
                })?,
            };
            if words.len() > 2 {
                return Err(format!("expected `<path>.png [cover|contain|fill|tile]`, got `{text}`"));
            }
            Ok(Background::Image { path: first, fit })
        }
        _ => Err(format!(
            "expected a background: a color (`#rrggbb[aa]`, a theme color name, `none`), \
             `linear <angle> <color> <color> ...`, `radial <color> <color> ...` or an image `<path>.png [fit]`, got `{text}`"
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shadow<'a> {
    /// Blur width in px, 1 to [`MAX_SHADOW`].
    pub size: i32,
    /// As written; [`DEFAULT_SHADOW_COLOR`] when none is.
    pub color: &'a str,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShadowValue<'a> {
    None,
    Shadow(Shadow<'a>),
    /// A bare theme token's name (`shadow = shadow`): its value is one.
    Token(&'a str),
}

fn px(w: &str) -> Option<i32> {
    w.strip_suffix("px").unwrap_or(w).parse::<i32>().ok()
}

/// Parse a `shadow` value: `none` (or `0`), `<size> [<color>] [<x>
/// <y>]`, or a theme token's name.
pub fn parse_shadow(text: &str) -> Result<ShadowValue<'_>, String> {
    let text = text.trim();
    let words: Vec<&str> = text.split_whitespace().collect();
    let usage = || format!("expected a shadow: `none`, or `<size> [<color>] [<x> <y>]` (e.g. `12 #00000060 0 4`), got `{text}`");
    match words.as_slice() {
        ["none"] => return Ok(ShadowValue::None),
        [name] if is_name(name) => return Ok(ShadowValue::Token(name)),
        [] => return Err(usage()),
        _ => {}
    }
    let size = px(words[0]).ok_or_else(usage)?;
    if !(0..=MAX_SHADOW).contains(&size) {
        return Err(format!("shadow size {size} is out of range: 0 to {MAX_SHADOW} px (software shadows cost their size squared)"));
    }
    let mut rest = &words[1..];
    let mut color = DEFAULT_SHADOW_COLOR;
    if let Some(c) = rest.first().filter(|w| px(w).is_none()) {
        if !color_word(c) {
            return Err(format!("shadow color `{c}` is not a color (`#rrggbb[aa]` or a theme color name)"));
        }
        color = c;
        rest = &rest[1..];
    }
    let (x, y) = match rest {
        [] => (0, 0),
        [x, y] => match (px(x), px(y)) {
            (Some(x), Some(y)) if x.abs() <= 64 && y.abs() <= 64 => (x, y),
            _ => return Err(format!("shadow offset `{x} {y}`: two px numbers, -64 to 64")),
        },
        _ => return Err(usage()),
    };
    if size == 0 {
        return Ok(ShadowValue::None);
    }
    Ok(ShadowValue::Shadow(Shadow { size, color, x, y }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backgrounds_parse() {
        assert_eq!(parse_background("#ff000080"), Ok(Background::Color("#ff000080")));
        assert_eq!(parse_background("surface"), Ok(Background::Color("surface")));
        assert_eq!(parse_background("none"), Ok(Background::None));
        assert_eq!(
            parse_background("linear 180 #000 accent #ffffff00"),
            Ok(Background::Gradient(Gradient { kind: GradientKind::Linear(180), stops: vec!["#000", "accent", "#ffffff00"] }))
        );
        assert_eq!(
            parse_background("linear 360deg #000 #fff"),
            Ok(Background::Gradient(Gradient { kind: GradientKind::Linear(0), stops: vec!["#000", "#fff"] }))
        );
        assert_eq!(
            parse_background("radial #fff #000"),
            Ok(Background::Gradient(Gradient { kind: GradientKind::Radial, stops: vec!["#fff", "#000"] }))
        );
        assert_eq!(parse_background("images/bg.png"), Ok(Background::Image { path: "images/bg.png", fit: Fit::Cover }));
        assert_eq!(parse_background("bg.PNG contain"), Ok(Background::Image { path: "bg.PNG", fit: Fit::Contain }));
        for (bad, msg) in [
            ("linear #000 #fff", "angle"),
            ("linear 400 #000 #fff", "angle"),
            ("linear 90 #000", "2 to 4 colors"),
            ("radial #0 #1 #2 #3 #4", "2 to 4 colors"),
            ("linear 90 #000 #zz", "not a color"),
            ("images/bg.jpg", ".png"),
            ("bg.png zoom", "fit `zoom`"),
            ("bg.png cover x", "expected"),
            ("#12", "expected a background"),
            ("", "expected a background"),
        ] {
            let e = parse_background(bad).unwrap_err();
            assert!(e.contains(msg), "{bad}: {e}");
        }
    }

    #[test]
    fn shadows_parse() {
        let s = |size, color, x, y| Ok(ShadowValue::Shadow(Shadow { size, color, x, y }));
        assert_eq!(parse_shadow("12"), s(12, DEFAULT_SHADOW_COLOR, 0, 0));
        assert_eq!(parse_shadow("8 #00000080 0 4"), s(8, "#00000080", 0, 4));
        assert_eq!(parse_shadow("8px accent"), s(8, "accent", 0, 0));
        assert_eq!(parse_shadow("8 -2 4"), s(8, DEFAULT_SHADOW_COLOR, -2, 4));
        assert_eq!(parse_shadow("none"), Ok(ShadowValue::None));
        assert_eq!(parse_shadow("0"), Ok(ShadowValue::None));
        assert_eq!(parse_shadow("shadow"), Ok(ShadowValue::Token("shadow")));
        for (bad, msg) in [("99", "out of range"), ("8 #zz", "not a color"), ("8 #000 4", "expected a shadow"), ("8 #000 1 200", "offset"), ("", "expected")] {
            let e = parse_shadow(bad).unwrap_err();
            assert!(e.contains(msg), "{bad}: {e}");
        }
    }
}
