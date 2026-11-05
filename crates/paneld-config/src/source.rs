//! Source files and spans.
//!
//! Every token and AST node carries a `Span` (file id + byte range).
//! Line and column are computed only when an error is printed, keeping
//! the hot path cheap. See ADR 0009.

use std::fmt;
use std::path::{Path, PathBuf};

/// Opaque id of a source file inside a `SourceMap`.
///
/// The inner value is only writable from this module, so a `FileId`
/// handed to a `SourceMap` method is always one the map produced.
/// `Default` returns a sentinel id for diagnostics that don't need
/// a real source location (e.g. an interpolation at runtime).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileId(pub(crate) u32);

/// A byte range inside a specific source file.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end, "span start {start} > end {end}");
        Self { file, start, end }
    }

    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Return the smallest span that covers both `self` and `other`.
    ///
    /// This is a union: any gap between the two is included. The two
    /// spans must be in the same file.
    pub fn join(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// A source file loaded into memory.
struct File {
    path: PathBuf,
    text: String,
    /// Byte offset of the start of each line (line 1 is `line_starts[0]`).
    line_starts: Vec<u32>,
}

/// Human-friendly location: file path, 1-indexed line, 1-indexed column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.path.display(), self.line, self.column)
    }
}

/// All source files the parser has seen in one run.
#[derive(Default)]
pub struct SourceMap {
    files: Vec<File>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, path: PathBuf, text: String) -> FileId {
        debug_assert!(
            self.files.len() < u32::MAX as usize,
            "too many source files"
        );
        let line_starts = compute_line_starts(&text);
        let id = FileId(self.files.len() as u32);
        self.files.push(File {
            path,
            text,
            line_starts,
        });
        id
    }

    pub fn path(&self, id: FileId) -> &Path {
        &self.files[id.0 as usize].path
    }

    /// The folder of the first file added (the root config): where a
    /// relative asset path in the config is read from.
    pub fn root_dir(&self) -> Option<&Path> {
        self.files.first().map(|f| f.path.parent().unwrap_or(Path::new("")))
    }

    pub fn text(&self, id: FileId) -> &str {
        &self.files[id.0 as usize].text
    }

    pub fn slice(&self, span: Span) -> &str {
        let text = self.text(span.file);
        &text[span.start as usize..span.end as usize]
    }

    /// Resolve the start of `span` to a `Location`.
    pub fn location(&self, span: Span) -> Location {
        self.location_at(span.file, span.start)
    }

    /// Resolve both ends of `span` to `Location`s. Useful for diagnostics
    /// that highlight a range inside one file.
    pub fn location_range(&self, span: Span) -> (Location, Location) {
        (
            self.location_at(span.file, span.start),
            self.location_at(span.file, span.end),
        )
    }

    fn location_at(&self, file: FileId, offset: u32) -> Location {
        let file = &self.files[file.0 as usize];
        let (line, column) = line_col(&file.line_starts, &file.text, offset);
        Location {
            path: file.path.clone(),
            line,
            column,
        }
    }
}

fn compute_line_starts(text: &str) -> Vec<u32> {
    debug_assert!(text.len() <= u32::MAX as usize, "source file too large");
    let mut starts = Vec::with_capacity(text.len() / 32 + 1);
    starts.push(0);
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i as u32 + 1);
        }
    }
    starts
}

fn line_col(line_starts: &[u32], text: &str, offset: u32) -> (u32, u32) {
    let line_idx = match line_starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i - 1,
    };
    let line_start = line_starts[line_idx] as usize;
    let off = (offset as usize).min(text.len());
    let col_bytes = &text.as_bytes()[line_start..off];
    // Count characters, not bytes, so the column number lines up with
    // what a text editor shows for the user.
    let column = std::str::from_utf8(col_bytes)
        .map(|s| s.chars().count() as u32)
        .unwrap_or(col_bytes.len() as u32);
    (line_idx as u32 + 1, column + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_resolves_line_and_column() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("test.conf"), "ab\ncde\nfghij".to_string());
        let loc = |o: u32| map.location(Span::new(id, o, o));
        assert_eq!(loc(0), Location { path: "test.conf".into(), line: 1, column: 1 });
        assert_eq!(loc(1), Location { path: "test.conf".into(), line: 1, column: 2 });
        assert_eq!(loc(3), Location { path: "test.conf".into(), line: 2, column: 1 });
        assert_eq!(loc(7), Location { path: "test.conf".into(), line: 3, column: 1 });
        assert_eq!(loc(11), Location { path: "test.conf".into(), line: 3, column: 5 });
    }

    #[test]
    fn location_counts_utf8_chars_not_bytes() {
        let mut map = SourceMap::new();
        // 'è' is two bytes in UTF-8.
        let id = map.add(PathBuf::from("t.conf"), "cafè!".to_string());
        let loc = map.location(Span::new(id, 5, 5)); // byte offset of '!'
        assert_eq!(loc.column, 5);
    }

    #[test]
    fn slice_returns_span_text() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), "key = value".to_string());
        assert_eq!(map.slice(Span::new(id, 0, 3)), "key");
        assert_eq!(map.slice(Span::new(id, 6, 11)), "value");
    }

    #[test]
    fn span_join_covers_both() {
        let f = FileId(0);
        let a = Span::new(f, 2, 5);
        let b = Span::new(f, 10, 12);
        assert_eq!(a.join(b), Span::new(f, 2, 12));
        assert_eq!(b.join(a), Span::new(f, 2, 12));
    }

    #[test]
    fn location_at_end_of_file_is_valid() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), "abc".to_string());
        let loc = map.location(Span::new(id, 3, 3));
        assert_eq!(loc, Location { path: "t.conf".into(), line: 1, column: 4 });
    }

    #[test]
    fn empty_file_has_valid_location() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), String::new());
        let loc = map.location(Span::new(id, 0, 0));
        assert_eq!(loc, Location { path: "t.conf".into(), line: 1, column: 1 });
    }

    #[test]
    fn location_range_resolves_both_ends() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), "abc\nde".to_string());
        let (start, end) = map.location_range(Span::new(id, 2, 5));
        assert_eq!(start, Location { path: "t.conf".into(), line: 1, column: 3 });
        assert_eq!(end, Location { path: "t.conf".into(), line: 2, column: 2 });
    }

    #[test]
    fn multi_file_source_map_dispatches_by_file_id() {
        let mut map = SourceMap::new();
        let a = map.add(PathBuf::from("a.conf"), "aaa".to_string());
        let b = map.add(PathBuf::from("b.conf"), "bbbb".to_string());
        assert_ne!(a, b);
        assert_eq!(map.slice(Span::new(a, 0, 3)), "aaa");
        assert_eq!(map.slice(Span::new(b, 0, 4)), "bbbb");
        assert_eq!(map.path(a), Path::new("a.conf"));
        assert_eq!(map.path(b), Path::new("b.conf"));
    }

    #[test]
    fn location_displays_as_path_line_col() {
        let loc = Location { path: PathBuf::from("x/y.conf"), line: 10, column: 3 };
        assert_eq!(format!("{loc}"), "x/y.conf:10:3");
    }
}
