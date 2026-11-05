//! Loader: open the root config file, follow `include`s, substitute
//! `$vars`, and return one merged [`Document`] plus every diagnostic.
//!
//! Design notes:
//! - Includes are resolved relative to the file that wrote them. A `*`
//!   or `?` in the final path segment is a glob (matched by [`glob_match`]
//!   below); globs load in sorted order and matching zero files is not
//!   an error (same as hyprlang). A plain path that does not exist is
//!   an error.
//! - Cycles are detected via a canonical-path stack and reported with
//!   a diagnostic at the including site; the cycle edge is skipped.
//! - `$vars` are substituted after parsing in a single forward pass:
//!   a variable must be defined before its use (ADR 0009 / spec).
//!   `$$` escapes to a literal `$`. Nested references in a variable's
//!   value are expanded at definition time, which is why a variable
//!   can refer to earlier variables but not to itself or to later ones.
//! - ADR 0009 asks for a span map from substituted bytes back to the
//!   defining variable so a CEL error inside an expanded expression
//!   can point at both the use and the definition. That mapping is
//!   deferred: for now diagnostics about the expanded text point at
//!   the use site only, and the file:line:col information still works.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::lex::tokenize;
use crate::parse::{parse, AssignTarget, Block, Diagnostic, Document, Item, RawValue};
use crate::{SourceMap, Span};

#[derive(Debug)]
pub enum LoadError {
    Io { path: PathBuf, source: io::Error },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io { path, source } => write!(f, "{}: {}", path.display(), source),
        }
    }
}

impl std::error::Error for LoadError {}

/// Load the root config file and resolve its includes and `$vars`.
///
/// Returns the populated [`SourceMap`] (so a caller can resolve any
/// diagnostic span to a file:line:col), the merged [`Document`], and
/// every diagnostic produced along the way (lex errors, parse errors,
/// missing includes, undefined variables).
pub fn load(root: &Path) -> Result<(SourceMap, Document, Vec<Diagnostic>), LoadError> {
    let mut source = SourceMap::new();
    let mut diagnostics = Vec::new();
    let mut stack = Vec::new();
    let items = load_file(root, &mut source, &mut diagnostics, &mut stack, None)?;
    let mut doc = Document { items };
    let vars = substitute_vars(&mut doc, &mut diagnostics);
    let dir = root.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let folders = load_theme_folders(&mut doc, dir, &vars, &mut source, &mut diagnostics);
    crate::library::merge(&mut doc, &mut source, &mut diagnostics);
    // Folder themes last: with no `screen:theme`, the active theme is
    // the config's first, else the shipped `light`, never a folder's.
    let (mut items, last): (Vec<Item>, Vec<Item>) = doc.items.into_iter().partition(|it| {
        !matches!(it, Item::Block(b) if b.kind.text == "theme" && b.name.as_ref().is_some_and(|n| folders.contains(&n.text)))
    });
    items.extend(last);
    doc.items = items;
    Ok((source, doc, diagnostics))
}

fn load_file(
    path: &Path,
    source: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    stack: &mut Vec<PathBuf>,
    include_span: Option<Span>,
) -> Result<Vec<Item>, LoadError> {
    let canon = fs::canonicalize(path).map_err(|e| LoadError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if stack.iter().any(|p| p == &canon) {
        if let Some(sp) = include_span {
            diagnostics.push(Diagnostic::new(
                sp,
                format!("include cycle: `{}` is already being loaded", canon.display()),
            ));
        }
        return Ok(Vec::new());
    }
    stack.push(canon.clone());

    let text = fs::read_to_string(&canon).map_err(|e| LoadError::Io {
        path: canon.clone(),
        source: e,
    })?;
    let file_id = source.add(canon.clone(), text.clone());
    let (toks, lex_errs) = tokenize(file_id, &text);
    diagnostics.extend(lex_errs.into_iter().map(Diagnostic::from));
    let (doc, parse_diags) = parse(&toks, source);
    diagnostics.extend(parse_diags);

    // Walk the top-level items, expanding `include = ...` entries in place.
    let mut out = Vec::new();
    let base = canon
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    for item in doc.items {
        if let Item::Assign(ref a) = item {
            if let AssignTarget::Simple { ref key } = a.target {
                if key.text == "include" {
                    expand_include(&a.value, &base, source, diagnostics, stack, &mut out);
                    continue;
                }
            }
        }
        out.push(item);
    }

    stack.pop();
    Ok(out)
}

fn expand_include(
    value: &RawValue,
    base: &Path,
    source: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
    stack: &mut Vec<PathBuf>,
    out: &mut Vec<Item>,
) {
    // `include = "my file.conf"`: the quotes are not part of the path.
    let unquoted = crate::value::parse_string(&value.text);
    let pat = unquoted.trim();
    if pat.is_empty() {
        diagnostics.push(Diagnostic::new(value.span, "empty include path"));
        return;
    }
    let path = base.join(pat);
    let final_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let has_glob_overall = contains_glob(pat);
    let has_glob_in_final = contains_glob(final_name);
    if has_glob_overall && !has_glob_in_final {
        diagnostics.push(Diagnostic::new(
            value.span,
            format!(
                "include `{pat}`: glob patterns are only supported in the final path segment"
            ),
        ));
        return;
    }
    let matches = match expand_glob(&path) {
        Ok(ms) => ms,
        Err(e) => {
            diagnostics.push(Diagnostic::new(
                value.span,
                format!("include `{pat}`: {e}"),
            ));
            return;
        }
    };
    if matches.is_empty() {
        if !has_glob_in_final {
            diagnostics.push(Diagnostic::new(
                value.span,
                format!("include `{pat}`: file not found"),
            ));
        }
        return;
    }
    for m in matches {
        match load_file(&m, source, diagnostics, stack, Some(value.span)) {
            Ok(items) => out.extend(items),
            Err(LoadError::Io { path, source }) => {
                diagnostics.push(Diagnostic::new(
                    value.span,
                    format!("include `{}`: {}", path.display(), source),
                ));
            }
        }
    }
}

fn contains_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Resolve a path that may have a glob in its final segment. Globs in
/// earlier segments are not supported in v1 and would be matched
/// literally (and probably not resolve); callers should not rely on
/// that behavior.
fn expand_glob(path: &Path) -> io::Result<Vec<PathBuf>> {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return Ok(Vec::new()),
    };
    if !contains_glob(name) {
        if path.exists() {
            return Ok(vec![path.to_path_buf()]);
        }
        return Ok(Vec::new());
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut matches = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let fname = entry.file_name();
        let fname_str = match fname.to_str() {
            Some(s) => s,
            None => continue,
        };
        if glob_match(name, fname_str) {
            matches.push(entry.path());
        }
    }
    matches.sort();
    Ok(matches)
}

/// Minimal glob matcher. Supports `*` (any run of characters) and `?`
/// (exactly one character). Not `**`, no character classes, no escapes:
/// paneld config paths are plain. Matches whole strings, case sensitive.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    // Both pattern and text are iterated by Unicode scalar value so a
    // `?` matches one full codepoint rather than one byte of a
    // multi-byte UTF-8 sequence.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, ti));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// --- theme folders (ADR 0021) ---

/// The themes the config names, each with where: the screen's and the
/// night's `theme`, `screen:theme`, and every theme's `extends`.
fn theme_refs(items: &[Item]) -> Vec<(String, Span)> {
    fn in_screen(items: &[Item], out: &mut Vec<(String, Span)>) {
        for it in items {
            match it {
                Item::Assign(a) => {
                    if let AssignTarget::Simple { key } = &a.target {
                        if key.text == "theme" {
                            out.push((a.value.text.trim().to_string(), a.value.span));
                        }
                    }
                }
                Item::Block(b) => in_screen(&b.items, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    for it in items {
        match it {
            Item::Block(b) if b.kind.text == "screen" => in_screen(&b.items, &mut out),
            Item::Block(b) if b.kind.text == "theme" => {
                for i in &b.items {
                    if let Item::Assign(a) = i {
                        if matches!(&a.target, AssignTarget::Simple { key } if key.text == "extends") {
                            out.push((a.value.text.trim().to_string(), a.value.span));
                        }
                    }
                }
            }
            Item::Assign(a) => {
                let theme_key = match &a.target {
                    AssignTarget::Qualified { category, key } => category.text == "screen" && key.text == "theme",
                    // `theme[glass]:accent = ...` changes a theme that may
                    // live in a folder.
                    AssignTarget::Indexed { kind, name, key } if kind.text == "theme" => {
                        out.push((name.text.clone(), name.span));
                        key.text == "extends"
                    }
                    _ => false,
                };
                if theme_key {
                    out.push((a.value.text.trim().to_string(), a.value.span));
                }
            }
            _ => {}
        }
    }
    out
}

/// Load `<dir>/themes/<name>/theme.conf` for every theme the config
/// names but does not define, and for the themes those extend. A
/// theme defined in the config wins over a folder of the same name; a
/// name with neither is left to the check ("unknown theme").
fn load_theme_folders(
    doc: &mut Document,
    dir: &Path,
    vars: &HashMap<String, String>,
    source: &mut SourceMap,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<String> {
    let mut loaded = Vec::new();
    let mut tried: Vec<String> = Vec::new();
    // Every theme folder, named or not: each is checked with the config,
    // so `paneld ctl theme <name>` can switch to any of them. The named
    // ones first (popped first), so a problem is said where it is named.
    let mut listed: Vec<String> = fs::read_dir(dir.join(crate::assets::THEMES_DIR))
        .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    listed.sort();
    let mut todo: Vec<(String, Option<Span>)> = listed.into_iter().rev().map(|n| (n, None)).collect();
    todo.extend(theme_refs(&doc.items).into_iter().map(|(n, s)| (n, Some(s))));
    while let Some((name, span)) = todo.pop() {
        let defined = doc.items.iter().any(|it| {
            matches!(it, Item::Block(b) if b.kind.text == "theme" && b.name.as_ref().is_some_and(|n| n.text == name))
        });
        if defined || tried.contains(&name) || !crate::icons::is_file_name(&name) {
            continue;
        }
        tried.push(name.clone());
        let path = dir.join(crate::assets::THEMES_DIR).join(&name).join(crate::assets::THEME_FILE);
        if !path.is_file() {
            continue;
        }
        let first_file = source.files().count();
        let mut items = match load_file(&path, source, diagnostics, &mut Vec::new(), span) {
            Ok(items) => items,
            Err(e) => {
                // Unreadable: said at the reference, else at the config's start.
                let at = span.unwrap_or_else(|| Span::new(crate::source::FileId::default(), 0, 0));
                diagnostics.push(Diagnostic::new(at, format!("theme `{name}`: {e}")));
                continue;
            }
        };
        if span.is_none() {
            source.mark_unused_theme(first_file, &name);
        }
        // Where to say what is wrong with the file as a whole.
        let file_start = fs::canonicalize(&path)
            .ok()
            .and_then(|c| source.files().find(|(_, p)| *p == c).map(|(id, _)| Span::new(id, 0, 0)));
        let span = span.or(file_start).unwrap_or_else(|| Span::new(crate::source::FileId::default(), 0, 0));
        let mut vars = vars.clone();
        substitute_items(&mut items, &mut vars, diagnostics);
        let rel = format!("{}/{name}/{}", crate::assets::THEMES_DIR, crate::assets::THEME_FILE);
        let mut found = false;
        for item in items {
            match item {
                Item::Block(mut b) if b.kind.text == "theme" && b.name.as_ref().is_some_and(|n| n.text == name) && !found => {
                    found = true;
                    rewrite_theme_assets(&mut b, &crate::assets::theme_prefix(&name));
                    todo.extend(theme_refs(std::slice::from_ref(&Item::Block(b.clone()))).into_iter().map(|(n, s)| (n, Some(s))));
                    doc.items.push(Item::Block(b));
                    loaded.push(name.clone());
                }
                Item::Block(b) if b.kind.text == "theme" => {
                    let at = b.name.as_ref().map_or(b.kind.span, |n| n.span);
                    diagnostics.push(Diagnostic::new(
                        at,
                        format!("`{rel}` holds one `theme {name} {{ }}` (the folder's name); another theme goes in its own folder"),
                    ));
                }
                other => {
                    let (at, what) = match &other {
                        Item::Block(b) => (b.kind.span, format!("`{}`", b.kind.text)),
                        Item::Assign(a) => (a.span, "a setting".to_string()),
                        Item::Repeat(r) => (r.span, "`repeat`".to_string()),
                        Item::Param(p) => (p.span, "`param`".to_string()),
                        Item::Var(v) => (v.span, "`var`".to_string()),
                    };
                    diagnostics.push(Diagnostic::new(
                        at,
                        format!("`{rel}` holds only `theme {name} {{ }}`; {what} belongs in the config"),
                    ));
                }
            }
        }
        if !found {
            diagnostics.push(Diagnostic::new(span, format!("`{rel}` does not define `theme {name} {{ }}`")));
        }
    }
    loaded
}

/// Make the asset paths of a theme loaded from its folder relative to
/// the config folder by putting `prefix` (`themes/<name>/`) in front:
/// font files (a token or a `font` that is a `<file>.ttf <size>` font), the
/// `icons` folder, `background_image` (and `background` when it is a
/// path), and every literal `src` in its components (an expression is
/// left alone: its value, a param say, is relative to the config folder).
fn rewrite_theme_assets(theme: &mut Block, prefix: &str) {
    fn font_file(text: &str) -> bool {
        !text.contains("{{")
            && matches!(crate::fonts::parse(text), Ok(crate::fonts::Font { family: crate::fonts::Family::File(_), .. }))
    }
    fn rewrite(items: &mut [Item], prefix: &str, top: bool) {
        for it in items {
            match it {
                Item::Assign(a) => {
                    let AssignTarget::Simple { key } = &a.target else { continue };
                    let v = a.value.text.trim();
                    // Only paths written here: a value from an expression
                    // (a component param, say) is the caller's, relative
                    // to the config folder.
                    let path_key = !v.contains("{{")
                        && match key.text.as_str() {
                            "src" => !top,
                            "icons" => top && v != "builtin",
                            "background_image" => top,
                            "background" => top && !v.starts_with('#') && (v.contains('/') || v.contains('.')),
                            _ => false,
                        };
                    // A font file: in a token (`font_title = fonts/x.ttf 20`) or a `font`.
                    let font_key = top || key.text == "font";
                    if (path_key && !v.is_empty()) || (font_key && font_file(v)) {
                        a.value.text = format!("{prefix}{}", v.trim_start_matches("./"));
                    }
                }
                Item::Block(b) => rewrite(&mut b.items, prefix, false),
                Item::Repeat(r) => rewrite(&mut r.items, prefix, false),
                Item::Param(_) | Item::Var(_) => {}
            }
        }
    }
    rewrite(&mut theme.items, prefix, true);
}

/// Split the diagnostics of a load and check into errors and warnings:
/// a problem in a theme folder nothing names (ADR 0021) is a warning,
/// and that theme is taken out of `doc`, so `paneld ctl theme` cannot
/// switch to it. Everything else is an error.
pub fn split_warnings(doc: &mut Document, source: &SourceMap, diags: Vec<Diagnostic>) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    let (mut errors, mut warnings) = (Vec::new(), Vec::new());
    let mut broken: Vec<String> = Vec::new();
    for mut d in diags {
        match source.unused_theme(d.span.file) {
            Some(name) => {
                d.message = format!("{} (theme `{name}` is named nowhere: left out)", d.message);
                if !broken.iter().any(|b| b == name) {
                    broken.push(name.to_string());
                }
                warnings.push(d);
            }
            None => errors.push(d),
        }
    }
    doc.items.retain(|it| {
        !matches!(it, Item::Block(b) if b.kind.text == "theme" && b.name.as_ref().is_some_and(|n| broken.contains(&n.text)))
    });
    (errors, warnings)
}

// --- $var substitution ---

fn substitute_vars(doc: &mut Document, diagnostics: &mut Vec<Diagnostic>) -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = HashMap::new();
    substitute_items(&mut doc.items, &mut vars, diagnostics);
    vars
}

fn substitute_items(
    items: &mut [Item],
    vars: &mut HashMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for item in items {
        match item {
            Item::Assign(a) => match &a.target {
                AssignTarget::Var { name } => {
                    // The right-hand side can refer to earlier variables.
                    let expanded = expand_vars_text(&a.value.text, a.value.span, vars, diagnostics);
                    a.value.text = expanded.clone();
                    vars.insert(name.text.clone(), expanded);
                }
                _ => {
                    a.value.text =
                        expand_vars_text(&a.value.text, a.value.span, vars, diagnostics);
                }
            },
            Item::Block(b) => substitute_items(&mut b.items, vars, diagnostics),
            Item::Repeat(r) => {
                r.source.body =
                    expand_vars_text(&r.source.body, r.source.span, vars, diagnostics);
                substitute_items(&mut r.items, vars, diagnostics);
            }
            Item::Param(p) => {
                if let Some(d) = &mut p.default {
                    d.text = expand_vars_text(&d.text, d.span, vars, diagnostics);
                }
                if let Some(opts) = &mut p.options {
                    substitute_items(&mut opts.items, vars, diagnostics);
                }
            }
            Item::Var(v) => {
                v.default.text =
                    expand_vars_text(&v.default.text, v.default.span, vars, diagnostics);
            }
        }
    }
}

/// Scan `text` for `$name` references and expand them using `vars`.
/// `$$` yields a literal `$`. Unknown variables produce a diagnostic
/// anchored at `span` and are left untouched in the output so that
/// downstream processing (schema, CEL) does not see a surprise.
fn expand_vars_text(
    text: &str,
    span: Span,
    vars: &HashMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'$' {
            // Copy one UTF-8 character.
            let ch_len = utf8_len(b);
            let end = (i + ch_len).min(bytes.len());
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        // b == `$`
        if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        let name_start = i + 1;
        let mut j = name_start;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        if j == name_start {
            // Lone `$` or `$` followed by a non-ident char: emit literally.
            out.push('$');
            i += 1;
            continue;
        }
        let name = &text[name_start..j];
        if let Some(v) = vars.get(name) {
            out.push_str(v);
        } else {
            diagnostics.push(Diagnostic::new(
                span,
                format!("undefined variable `${name}`"),
            ));
            // Preserve the original text so later passes see something
            // the user wrote and can report sensibly on top of it.
            out.push_str(&text[i..j]);
        }
        i = j;
    }
    out
}

fn utf8_len(first_byte: u8) -> usize {
    // Lead-byte lookup. Continuation bytes (0x80..0xC0) should not be seen
    // as a start, but defaulting them to 1 keeps the loop advancing.
    match first_byte {
        0..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xFF => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Create a temporary directory under the scratchpad. The directory
    /// is unique per test and cleaned up afterwards.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            // Use a project-local temp root rather than /tmp so test writes
            // stay inside the workspace and the harness's shared-scratch
            // rules do not block them.
            let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
                .join("testdata");
            let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
            let pid = std::process::id();
            let p = base.join(format!("paneld-load-{label}-{pid}-{n}"));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, text: &str) -> PathBuf {
            let p = self.0.join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, text).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn glob_match_cases() {
        assert!(glob_match("*.conf", "a.conf"));
        assert!(glob_match("*.conf", "foo.bar.conf"));
        assert!(!glob_match("*.conf", "a.txt"));
        assert!(glob_match("a*b", "aXYZb"));
        assert!(glob_match("a*b", "ab"));
        assert!(!glob_match("a*b", "a"));
        assert!(glob_match("?.conf", "x.conf"));
        assert!(!glob_match("?.conf", "xy.conf"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        // `?` matches one Unicode scalar, not one byte of a multi-byte
        // UTF-8 sequence.
        assert!(glob_match("?.conf", "\u{00e9}.conf")); // "é.conf"
        assert!(glob_match("*.conf", "\u{00e9}\u{00e9}.conf"));
    }

    #[test]
    fn loads_single_file() {
        let d = TempDir::new("single");
        let root = d.write("root.conf", "include = child.conf\n");
        let _ = d.write("child.conf", "k = v\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(doc.items.len(), 1 + crate::library::item_count());
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(a.value.text, "v");
    }

    #[test]
    fn missing_non_glob_include_errors() {
        let d = TempDir::new("missing");
        let root = d.write("root.conf", "include = nope.conf\n");
        let (_, _, diags) = load(&root).unwrap();
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("file not found"), "{diags:?}");
    }

    #[test]
    fn missing_glob_include_is_silent() {
        let d = TempDir::new("missingglob");
        let root = d.write("root.conf", "include = *.nothing\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(doc.items.len(), crate::library::item_count());
    }

    #[test]
    fn glob_loads_in_sorted_order() {
        let d = TempDir::new("sorted");
        let root = d.write("root.conf", "include = pages/*.conf\n");
        let _ = d.write("pages/b.conf", "k = second\n");
        let _ = d.write("pages/a.conf", "k = first\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(doc.items.len(), 2 + crate::library::item_count());
        let Item::Assign(a0) = &doc.items[0] else {
            panic!()
        };
        let Item::Assign(a1) = &doc.items[1] else {
            panic!()
        };
        assert_eq!(a0.value.text, "first");
        assert_eq!(a1.value.text, "second");
    }

    #[test]
    fn include_cycle_errors_without_recursing_forever() {
        let d = TempDir::new("cycle");
        let root = d.write("root.conf", "include = other.conf\n");
        let _ = d.write("other.conf", "include = root.conf\nk = v\n");
        let (_, _, diags) = load(&root).unwrap();
        assert!(
            diags.iter().any(|d| d.message.contains("cycle")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn var_substitution_in_values() {
        let d = TempDir::new("vars");
        let root = d.write(
            "root.conf",
            "$pve = https://host\n$ep = $pve/api\nk = get $ep/nodes\n",
        );
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        let Item::Assign(a) = &doc.items[2] else {
            panic!()
        };
        assert_eq!(a.value.text, "get https://host/api/nodes");
    }

    #[test]
    fn undefined_variable_is_a_diagnostic_but_text_is_preserved() {
        let d = TempDir::new("undef");
        let root = d.write("root.conf", "k = $missing/path\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(
            diags.iter().any(|d| d.message.contains("undefined variable `$missing`")),
            "diags: {diags:?}"
        );
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(a.value.text, "$missing/path");
    }

    #[test]
    fn double_dollar_escapes_to_literal() {
        let d = TempDir::new("dd");
        let root = d.write("root.conf", "k = cost $$5 for $$total\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(a.value.text, "cost $5 for $total");
    }

    #[test]
    fn var_used_before_definition_is_undefined() {
        let d = TempDir::new("fwd");
        let root = d.write("root.conf", "k = $pve\n$pve = x\n");
        let (_, _, diags) = load(&root).unwrap();
        assert!(
            diags.iter().any(|d| d.message.contains("undefined variable `$pve`")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn stray_dollar_without_name_is_literal() {
        let d = TempDir::new("stray");
        let root = d.write("root.conf", "k = price $\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(a.value.text, "price $");
    }

    #[test]
    fn non_final_segment_glob_errors() {
        let d = TempDir::new("midglob");
        let root = d.write("root.conf", "include = */x.conf\n");
        let (_, _, diags) = load(&root).unwrap();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("final path segment")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn include_cycle_through_two_paths_detects() {
        let d = TempDir::new("cycle2");
        // Two files include each other.
        let root = d.write("root.conf", "include = ./sub/../other.conf\n");
        let _ = d.write(
            "other.conf",
            "include = root.conf\nk = v\n",
        );
        fs::create_dir_all(d.path().join("sub")).unwrap();
        let (_, _, diags) = load(&root).unwrap();
        assert!(
            diags.iter().any(|d| d.message.contains("cycle")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn errors_from_included_file_report_against_that_file() {
        let d = TempDir::new("inclerr");
        let root = d.write("root.conf", "include = bad.conf\n");
        let bad = d.write("bad.conf", "oops no equals\n");
        let bad_canon = fs::canonicalize(&bad).unwrap();
        let (map, _, diags) = load(&root).unwrap();
        assert!(!diags.is_empty());
        let any_in_bad = diags.iter().any(|dg| map.path(dg.span.file) == bad_canon.as_path());
        assert!(any_in_bad, "diags: {diags:?}");
    }

    #[test]
    fn a_missing_main_file_is_an_io_error_with_its_path() {
        let d = TempDir::new("nomain");
        let Err(e) = load(&d.path().join("absent.conf")) else { panic!("loaded a missing file") };
        let LoadError::Io { path, .. } = &e;
        assert!(path.ends_with("absent.conf"));
        assert!(e.to_string().contains("absent.conf"), "{e}");
        let _: &dyn std::error::Error = &e;
    }

    #[test]
    fn an_empty_or_unreadable_include_is_a_diagnostic() {
        let d = TempDir::new("badinc");
        let root = d.write("root.conf", "include = \"\"\ninclude = adir\n");
        fs::create_dir_all(d.path().join("adir")).unwrap();
        let (_, _, diags) = load(&root).unwrap();
        let msgs: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
        assert!(msgs.iter().any(|m| m.contains("empty include path")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("include `") && m.contains("adir")), "{msgs:?}");
    }

    #[test]
    fn a_quoted_include_path_may_hold_spaces() {
        let d = TempDir::new("spaces");
        let root = d.write("root.conf", "include = \"my file.conf\"\n");
        d.write("my file.conf", "x = 1\n");
        let (_, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        assert!(matches!(&doc.items[0], Item::Assign(_)));
    }

    #[test]
    fn a_glob_in_a_missing_folder_is_a_diagnostic() {
        let d = TempDir::new("globdir");
        let root = d.write("root.conf", "include = nowhere/*.conf\n");
        let (_, _, diags) = load(&root).unwrap();
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert!(diags[0].message.starts_with("include `nowhere/*.conf`:"), "{diags:?}");
    }

    #[test]
    fn includes_resolve_from_the_including_file() {
        let d = TempDir::new("nested");
        let root = d.write("root.conf", "include = sub/a.conf\n");
        d.write("sub/a.conf", "include = b.conf\n");
        d.write("sub/b.conf", "x = 1\n");
        let (map, doc, diags) = load(&root).unwrap();
        assert!(diags.is_empty(), "{diags:?}");
        let Item::Assign(a) = &doc.items[0] else { panic!() };
        assert!(map.location(a.span).to_string().contains("b.conf:1"), "{}", map.location(a.span));
    }

    #[test]
    fn a_file_that_is_not_utf8_is_an_io_error() {
        let d = TempDir::new("latin1");
        let root = d.path().join("root.conf");
        fs::write(&root, b"k = caf\xe9\n").unwrap();
        assert!(load(&root).is_err());
    }
}