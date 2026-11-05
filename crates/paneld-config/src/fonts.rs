//! Font values (ADR 0015): `<family> <size> [bold]`, where the family
//! is a built-in name or a path to a `.ttf` file. Shared by the check
//! and the renderer, so both read a value the same way.

/// A built-in family, embedded in the binary as TTF (regular, bold).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Builtin {
    Roboto,
    OpenSans,
    NotoSans,
    Montserrat,
    Inter,
}

/// Names a config may write, with the family each one means. `sans`
/// is the default family under another name.
pub const FAMILIES: &[(&str, Builtin)] = &[
    ("roboto", Builtin::Roboto),
    ("sans", Builtin::Roboto),
    ("open_sans", Builtin::OpenSans),
    ("noto_sans", Builtin::NotoSans),
    ("montserrat", Builtin::Montserrat),
    ("inter", Builtin::Inter),
];

/// The theme tokens that hold a font value; a node's `font` may also
/// name one bare (`font = font_big`).
pub const TOKENS: &[&str] = &["font_small", "font", "font_big", "font_huge"];

/// Sizes in px a font value may ask for. Above 200 one glyph's bitmap
/// alone is 40 KB, and the screen is 480 px high.
pub const MIN_SIZE: u16 = 6;
pub const MAX_SIZE: u16 = 200;

/// A `.ttf` file is read whole into memory; larger ones are refused.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    Builtin(Builtin),
    /// A `.ttf` path, relative to the config folder.
    File(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Font {
    pub family: Family,
    pub size: u16,
    pub bold: bool,
}

/// Parse a font value. The error says what is wrong, for a check
/// diagnostic or a log line.
pub fn parse(text: &str) -> Result<Font, String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let (name, size, rest) = match parts.as_slice() {
        [name, size, rest @ ..] if rest.len() <= 1 => (*name, *size, rest),
        _ => return Err(format!("expected a font `<family> <size> [bold]` (`roboto 20`, `inter 16 bold`), got `{text}`")),
    };
    let bold = match rest {
        [] => false,
        ["bold"] => true,
        [w] => return Err(format!("expected `bold` or nothing after the font size, got `{w}`")),
        _ => unreachable!(),
    };
    let size = match size.strip_suffix("px").unwrap_or(size).parse::<u16>() {
        Ok(n) if (MIN_SIZE..=MAX_SIZE).contains(&n) => n,
        _ => return Err(format!("expected a font size from {MIN_SIZE} to {MAX_SIZE} px, got `{size}`")),
    };
    let family = family(name)?;
    if bold && matches!(family, Family::File(_)) {
        return Err(format!("`bold` only applies to a built-in family; name the bold file instead of `{name}`"));
    }
    Ok(Font { family, size, bold })
}

/// A family name or a `.ttf` path. A path is relative and stays inside
/// the config folder (no `..`, not absolute), since a value can come
/// from an expression.
pub fn family(name: &str) -> Result<Family, String> {
    if let Some((_, b)) = FAMILIES.iter().find(|(n, _)| *n == name) {
        return Ok(Family::Builtin(*b));
    }
    if name.to_ascii_lowercase().ends_with(".ttf") {
        let path = std::path::Path::new(name);
        let inside = path.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
        if !inside {
            return Err(format!("font file `{name}` must be a relative path inside the config folder"));
        }
        return Ok(Family::File(name.to_string()));
    }
    let hint = match suggest(name) {
        Some(s) => format!("; did you mean `{s}`?"),
        None => String::new(),
    };
    Err(format!(
        "unknown font family `{name}`: built in are roboto (also sans), open_sans, noto_sans, montserrat, inter, or a `.ttf` path{hint}"
    ))
}

/// The real path of font file `rel` under `dir` (the config folder):
/// a file that, with symlinks resolved, is still inside `dir`.
pub fn resolve_file(dir: &std::path::Path, rel: &str) -> Result<std::path::PathBuf, String> {
    let root = std::fs::canonicalize(if dir.as_os_str().is_empty() { std::path::Path::new(".") } else { dir })
        .map_err(|e| format!("font file `{rel}`: config folder {}: {e}", dir.display()))?;
    let full = std::fs::canonicalize(root.join(rel))
        .map_err(|_| format!("font file `{rel}` not found (relative to the config folder `{}`)", root.display()))?;
    if !full.starts_with(&root) {
        return Err(format!("font file `{rel}` leads outside the config folder (to {})", full.display()));
    }
    if !full.is_file() {
        return Err(format!("font file `{rel}` is not a file"));
    }
    Ok(full)
}

/// The bytes of a font file, at most [`MAX_FILE_BYTES`] (one more is
/// read to tell a larger file, which is refused).
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("font file {}: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("font file {} is larger than {} MiB", path.display(), MAX_FILE_BYTES >> 20));
    }
    Ok(bytes)
}

fn suggest(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase().replace('-', "_");
    FAMILIES
        .iter()
        .map(|(n, _)| (crate::icons::distance(&name, n), *n))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_family_size_and_weight() {
        let f = parse("roboto 20").unwrap();
        assert_eq!((f.family, f.size, f.bold), (Family::Builtin(Builtin::Roboto), 20, false));
        let f = parse("  montserrat   32 bold ").unwrap();
        assert_eq!((f.family, f.size, f.bold), (Family::Builtin(Builtin::Montserrat), 32, true));
        assert_eq!(parse("sans 16").unwrap().family, Family::Builtin(Builtin::Roboto));
        assert_eq!(parse("open_sans 18px").unwrap().size, 18);
        assert_eq!(parse("fonts/Inter.ttf 18").unwrap().family, Family::File("fonts/Inter.ttf".into()));
    }

    #[test]
    fn a_font_file_stays_inside_the_config_folder() {
        let base = std::env::temp_dir().join(format!("paneld-fonts-{}", std::process::id()));
        let dir = base.join("conf");
        std::fs::create_dir_all(dir.join("fonts")).unwrap();
        std::fs::write(base.join("outside.ttf"), b"x").unwrap();
        std::fs::write(dir.join("fonts/in.ttf"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("outside.ttf"), dir.join("fonts/link.ttf")).unwrap();
        assert!(resolve_file(&dir, "fonts/in.ttf").is_ok());
        assert!(resolve_file(&dir, "fonts/link.ttf").unwrap_err().contains("outside the config folder"));
        assert!(resolve_file(&dir, "fonts/none.ttf").unwrap_err().contains("not found"));
        std::fs::write(dir.join("big.ttf"), vec![0u8; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(read_file(&dir.join("big.ttf")).unwrap_err().contains("larger than"));
        assert_eq!(read_file(&dir.join("fonts/in.ttf")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse("roboto").unwrap_err().contains("<family> <size>"));
        assert!(parse("roboto 20 italic").unwrap_err().contains("`italic`"));
        assert!(parse("roboto 2").unwrap_err().contains("from 6 to 200"));
        assert!(parse("roboto 900").is_err());
        assert!(parse("robto 20").unwrap_err().contains("did you mean `roboto`"));
        assert!(parse("open-sans 20").unwrap_err().contains("`open_sans`"));
        assert!(parse("../x.ttf 20").unwrap_err().contains("inside the config folder"));
        assert!(parse("/etc/x.ttf 20").is_err());
        assert!(parse("fonts/Inter.ttf 18 bold").unwrap_err().contains("bold file"));
    }
}
