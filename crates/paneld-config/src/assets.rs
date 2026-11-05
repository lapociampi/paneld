//! Asset files (fonts, icons, images) and the folder each must stay
//! inside (ADR 0021): a path is relative to the config folder, with no
//! `..`; the folders on the way may be symlinks the owner put there (a
//! theme folder, an icons folder kept elsewhere), the file must stay
//! inside the folder it is named in. Theme folders' asset paths are
//! rewritten to `themes/<name>/...` when they are loaded
//! ([`crate::load`]), so every asset is read relative to the config
//! folder.

use std::path::{Component, Path, PathBuf};

/// The folder theme folders live in, under the config folder.
pub const THEMES_DIR: &str = "themes";

/// The file a theme folder holds its `theme <name> { }` in.
pub const THEME_FILE: &str = "theme.conf";

/// `themes/<name>/`: what an asset path in a theme folder's file is
/// rewritten to start with.
pub fn theme_prefix(name: &str) -> String {
    format!("{THEMES_DIR}/{name}/")
}

/// The theme folder `rel` must stay inside (relative to the config
/// folder) when it is under `themes/<name>/`; `None` when only the
/// config folder bounds it.
pub fn root_of(rel: &str) -> Option<PathBuf> {
    let mut parts = Path::new(rel).components().filter(|c| !matches!(c, Component::CurDir));
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Component::Normal(t)), Some(Component::Normal(name)), Some(_)) if t == THEMES_DIR => {
            Some(Path::new(THEMES_DIR).join(name))
        }
        _ => None,
    }
}

/// The real path of `rel` (a `what`, e.g. "font file") under `dir`,
/// the config folder. `rel` is relative with no `..`; the folders on
/// its way are followed wherever they lead (the owner may link in a
/// theme folder or an icons folder kept elsewhere), but the file itself
/// must stay inside the folder it is named in: a symlink in a theme or
/// icons folder cannot reach out of it.
pub fn resolve(dir: &Path, rel: &str, what: &str) -> Result<PathBuf, String> {
    let path = Path::new(rel);
    if rel.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(format!("{what} `{rel}` must be a relative path inside the config folder"));
    }
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let folder_rel = path.parent().unwrap_or(Path::new(""));
    let folder = std::fs::canonicalize(dir.join(folder_rel))
        .map_err(|_| format!("{what} `{rel}` not found (relative to the config folder `{}`)", dir.display()))?;
    let full = std::fs::canonicalize(dir.join(path))
        .map_err(|_| format!("{what} `{rel}` not found (relative to the config folder `{}`)", dir.display()))?;
    if !full.starts_with(&folder) {
        let name = if folder_rel.as_os_str().is_empty() { "the config folder".to_string() } else { format!("its folder `{}`", folder_rel.display()) };
        return Err(format!("{what} `{rel}` leads outside {name} (to {})", full.display()));
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_stays_in_its_folder_which_may_be_linked_in() {
        assert_eq!(root_of("themes/glass/fonts/a.ttf"), Some(PathBuf::from("themes/glass")));
        assert_eq!(root_of("./themes/glass/x.png"), Some(PathBuf::from("themes/glass")));
        assert_eq!(root_of("themes/glass"), None);
        assert_eq!(root_of("fonts/a.ttf"), None);

        let base = std::env::temp_dir().join(format!("paneld-assets-{}", std::process::id()));
        let dir = base.join("conf");
        std::fs::create_dir_all(dir.join("themes/glass/fonts")).unwrap();
        std::fs::create_dir_all(dir.join("themes/dark")).unwrap();
        std::fs::write(dir.join("themes/glass/fonts/a.ttf"), b"x").unwrap();
        std::fs::write(dir.join("themes/dark/b.png"), b"x").unwrap();
        std::fs::write(dir.join("top.png"), b"x").unwrap();
        // A symlink inside the theme folder to another file of the
        // config folder: still out of the theme's folder.
        std::os::unix::fs::symlink(dir.join("top.png"), dir.join("themes/glass/top.png")).unwrap();
        std::os::unix::fs::symlink(dir.join("themes/dark/b.png"), dir.join("themes/glass/b.png")).unwrap();
        assert!(resolve(&dir, "themes/glass/fonts/a.ttf", "font file").is_ok());
        assert!(resolve(&dir, "top.png", "image").is_ok());
        let e = resolve(&dir, "themes/glass/top.png", "image").unwrap_err();
        assert!(e.contains("leads outside its folder `themes/glass`"), "{e}");
        assert!(resolve(&dir, "themes/glass/b.png", "image").unwrap_err().contains("leads outside"));
        assert!(resolve(&dir, "themes/glass/../dark/b.png", "image").unwrap_err().contains("relative path inside"));
        assert!(resolve(&dir, "/etc/passwd", "image").unwrap_err().contains("relative path inside"));
        assert!(resolve(&dir, "themes/none/x.png", "image").unwrap_err().contains("not found"));
        assert!(resolve(&dir, "themes/glass/nope.png", "image").unwrap_err().contains("not found"));
        // A theme folder or an icons folder kept elsewhere, linked in.
        std::fs::create_dir_all(base.join("shared/fonts")).unwrap();
        std::fs::write(base.join("shared/fonts/c.ttf"), b"x").unwrap();
        std::fs::write(base.join("shared/home.png"), b"x").unwrap();
        std::os::unix::fs::symlink(base.join("shared"), dir.join("themes/linked")).unwrap();
        std::os::unix::fs::symlink(base.join("shared"), dir.join("icons")).unwrap();
        assert!(resolve(&dir, "themes/linked/fonts/c.ttf", "font file").is_ok());
        assert!(resolve(&dir, "icons/home.png", "icon").is_ok());
        // ... but a file in it still cannot lead out of it.
        std::os::unix::fs::symlink(dir.join("top.png"), base.join("shared/out.png")).unwrap();
        assert!(resolve(&dir, "icons/out.png", "icon").unwrap_err().contains("leads outside its folder `icons`"));
        let _ = std::fs::remove_dir_all(base);
    }
}
