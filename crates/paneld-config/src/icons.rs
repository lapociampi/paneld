//! Built-in icons (schema 8.3): LVGL's symbol glyphs, which the
//! compiled Montserrat fonts carry, under readable names. One table
//! for the check (an unknown name is an error) and the renderer.

/// Every built-in icon: its name and the glyph's code point in the
/// Montserrat fonts (`LV_SYMBOL_*` in LVGL's `lv_symbol_def.h`).
pub const BUILTIN: &[(&str, char)] = &[
    ("audio", '\u{f001}'),
    ("video", '\u{f008}'),
    ("list", '\u{f00b}'),
    ("ok", '\u{f00c}'),
    ("close", '\u{f00d}'),
    ("power", '\u{f011}'),
    ("settings", '\u{f013}'),
    ("home", '\u{f015}'),
    ("download", '\u{f019}'),
    ("drive", '\u{f01c}'),
    ("refresh", '\u{f021}'),
    ("mute", '\u{f026}'),
    ("volume", '\u{f027}'),
    ("volume_max", '\u{f028}'),
    ("image", '\u{f03e}'),
    ("tint", '\u{f043}'),
    ("prev", '\u{f048}'),
    ("play", '\u{f04b}'),
    ("pause", '\u{f04c}'),
    ("stop", '\u{f04d}'),
    ("next", '\u{f051}'),
    ("eject", '\u{f052}'),
    ("left", '\u{f053}'),
    ("right", '\u{f054}'),
    ("plus", '\u{f067}'),
    ("minus", '\u{f068}'),
    ("eye_open", '\u{f06e}'),
    ("eye_close", '\u{f070}'),
    ("warning", '\u{f071}'),
    ("shuffle", '\u{f074}'),
    ("up", '\u{f077}'),
    ("down", '\u{f078}'),
    ("loop", '\u{f079}'),
    ("directory", '\u{f07b}'),
    ("upload", '\u{f093}'),
    ("call", '\u{f095}'),
    ("cut", '\u{f0c4}'),
    ("copy", '\u{f0c5}'),
    ("save", '\u{f0c7}'),
    ("bars", '\u{f0c9}'),
    ("envelope", '\u{f0e0}'),
    ("charge", '\u{f0e7}'),
    ("paste", '\u{f0ea}'),
    ("bell", '\u{f0f3}'),
    ("keyboard", '\u{f11c}'),
    ("gps", '\u{f124}'),
    ("file", '\u{f158}'),
    ("wifi", '\u{f1eb}'),
    ("battery_full", '\u{f240}'),
    ("battery_3", '\u{f241}'),
    ("battery_2", '\u{f242}'),
    ("battery_1", '\u{f243}'),
    ("battery_empty", '\u{f244}'),
    ("usb", '\u{f287}'),
    ("bluetooth", '\u{f293}'),
    ("trash", '\u{f2ed}'),
    ("edit", '\u{f304}'),
    ("backspace", '\u{f55a}'),
    ("sd_card", '\u{f7c2}'),
    ("new_line", '\u{f8a2}'),
    ("bullet", '\u{2022}'),
];

/// The Montserrat sizes compiled in (`lv_conf.h`); an icon takes the
/// nearest one, and above the largest it is scaled up to [`MAX_SIZE`].
pub const FONT_SIZES: &[u16] = &[12, 14, 16, 20, 24, 32];

/// The largest icon `size` drawn: the 32 px glyph scaled up 4 times.
pub const MAX_SIZE: u16 = 128;

/// The glyph of a built-in icon.
pub fn glyph(name: &str) -> Option<char> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// A name that can be a file `icons/<name>.png`: letters, digits, `_`
/// and `-` only, so an expression cannot reach out of the folder.
pub fn is_file_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The built-in name closest to `name`, for a "did you mean" hint:
/// one that starts with it (`battery` -> `battery_full`), else the
/// nearest within 2 edits.
pub fn suggest(name: &str) -> Option<&'static str> {
    if name.len() >= 3 {
        if let Some((n, _)) = BUILTIN.iter().find(|(n, _)| n.starts_with(name)) {
            return Some(n);
        }
    }
    let mut best: Option<(usize, &'static str)> = None;
    for (n, _) in BUILTIN {
        let d = distance(name, n);
        if d <= 2 && best.map_or(true, |(b, _)| d < b) {
            best = Some((d, n));
        }
    }
    best.map(|(_, n)| n)
}

/// Levenshtein distance (short ASCII names).
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = if ca == b[j] { prev } else { 1 + prev.min(row[j]).min(row[j + 1]) };
            prev = cur;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_snake_case_and_suggested() {
        for (i, (n, _)) in BUILTIN.iter().enumerate() {
            assert!(n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{n}");
            assert!(BUILTIN[..i].iter().all(|(m, _)| m != n), "{n} twice");
        }
        assert_eq!(glyph("home"), Some('\u{f015}'));
        assert_eq!(suggest("hom"), Some("home"));
        assert_eq!(suggest("setings"), Some("settings"));
        assert_eq!(suggest("battery"), Some("battery_full"));
        assert_eq!(suggest("zzzzzzzz"), None);
        assert!(is_file_name("my-icon_2") && !is_file_name("../x") && !is_file_name(""));
    }

    #[test]
    fn schema_md_lists_every_built_in() {
        let doc = include_str!("../../../docs/schema.md");
        let line = doc.lines().find(|l| l.starts_with("Built-in icons:")).expect("no list in schema.md");
        let listed: Vec<&str> = line.split('`').skip(1).step_by(2).collect();
        let mut table: Vec<&str> = BUILTIN.iter().map(|(n, _)| *n).collect();
        let mut sorted = listed.clone();
        sorted.sort_unstable();
        table.sort_unstable();
        assert_eq!(sorted, table);
    }
}
