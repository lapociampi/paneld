//! Hand-written lexer for the hypr-style config format.
//!
//! The lexer is modal: it starts in `Normal` mode and flips to `Value`
//! mode after an `=`. In `Normal` mode it emits structural tokens
//! (idents, braces, `=`, `:`, `;`). In `Value` mode it reads everything
//! up to the record terminator (unquoted `;`, `}`, or newline not after
//! `\`) as a single raw `Value` token.
//!
//! `"..."` segments and `{{ ... }}` expressions inside a value mask the
//! terminators: a `;` inside quotes or `{{ }}` does not end the value.
//! CEL inside `{{ }}` is NOT parsed here (that is left to the CEL
//! crate in a later phase); the lexer only finds the matching `}}` by
//! tracking brace depth and quoted strings.
//!
//! `$name` references inside a value are kept as part of the raw span;
//! variable substitution happens in a later pass that preserves the
//! mapping from substituted bytes back to the original span (ADR 0009).
//!
//! `#` starts a comment to end of line. Inside a value it is treated
//! as literal when it is the first non-whitespace character of the
//! value (so `color = #44aaff` works) or when it is not preceded by
//! whitespace (so `foo#bar` is literal). `##` is always a literal `#`
//! and is unescaped at value interpretation time.

use crate::{FileId, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// `[A-Za-z_][A-Za-z0-9_]*`
    Ident,
    /// `$` followed by an identifier.
    DollarIdent,
    /// `=`
    Equals,
    /// `:`
    Colon,
    /// `;`
    Semi,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// A raw newline that ended a record. Newlines consumed by a `\`
    /// continuation do not emit this token.
    Newline,
    /// A raw value span, from the first non-whitespace character after
    /// `=` to the record terminator. Interpretation (into a number,
    /// duration, color, string, expression, ...) is done later.
    RawValue,
    /// A `{{ ... }}` expression in Normal mode. Inside a value the
    /// expression bytes are embedded in the `RawValue` span; this
    /// standalone token covers only the Normal-mode case (currently
    /// only `repeat <ident> in {{ ... }}`).
    Expression,
    /// A theme-style selector that follows the `style` keyword inside
    /// a `theme { }` body, running up to the opening `{` of its block.
    /// Carries the raw selector text (`.card`, `button:pressed`, `*`,
    /// ...); shape validation is done in the schema check.
    Selector,
    /// End of input.
    Eof,
}

impl TokenKind {
    pub fn is_structural(self) -> bool {
        matches!(
            self,
            TokenKind::Equals
                | TokenKind::Colon
                | TokenKind::Semi
                | TokenKind::LBrace
                | TokenKind::RBrace
                | TokenKind::LBracket
                | TokenKind::RBracket
                | TokenKind::Newline
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    pub span: Span,
    pub message: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Value,
    /// Scan the next lexeme as a theme-style selector (`.card`,
    /// `button:pressed`, ...), running up to the opening `{` of the
    /// block. Flipped on from Normal when the lexer emits an `Ident`
    /// whose text is `style` and the next non-whitespace byte isn't a
    /// character that would start an ordinary assignment (`=`, `:`,
    /// `[`). Flipped back to Normal right after the selector token is
    /// produced.
    Selector,
}

pub struct Lexer<'src> {
    src: &'src [u8],
    file: FileId,
    pos: usize,
    mode: Mode,
    /// On a `param` line: its default value ends before a ` {` that
    /// opens the options block (`param m: int = 5 { min = 1 }`).
    param_line: bool,
}

impl<'src> Lexer<'src> {
    pub fn new(file: FileId, src: &'src str) -> Self {
        Self {
            src: src.as_bytes(),
            file,
            pos: 0,
            mode: Mode::Normal,
            param_line: false,
        }
    }

    /// Return the next token, or a lex error. After `Eof` the lexer
    /// keeps returning `Eof`.
    pub fn next_token(&mut self) -> Result<Token, LexError> {
        match self.mode {
            Mode::Normal => self.next_normal(),
            Mode::Value => self.next_value(),
            Mode::Selector => self.next_selector(),
        }
    }

    fn make_span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start as u32, end as u32)
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    #[inline]
    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    /// Starting at `{{`, advance past the matching `}}`. Tracks nested
    /// braces and skips over `"..."` / `'...'` string literals, which
    /// may contain unmatched braces (e.g. `"}}"` inside the expression).
    /// CEL is not parsed here; that is the parser's job later.
    fn scan_expression(&mut self) -> Result<(), LexError> {
        debug_assert_eq!(self.peek(), Some(b'{'));
        debug_assert_eq!(self.peek_at(1), Some(b'{'));
        let expr_open = self.pos;
        self.pos += 2;
        let mut depth: u32 = 0;
        loop {
            match self.peek() {
                None => {
                    return Err(LexError {
                        span: self.make_span(expr_open, self.pos),
                        message: "unterminated `{{` expression".to_string(),
                    });
                }
                Some(q @ b'"') | Some(q @ b'\'') => {
                    self.pos += 1;
                    loop {
                        match self.peek() {
                            None => {
                                return Err(LexError {
                                    span: self.make_span(expr_open, self.pos),
                                    message: "unterminated string in `{{ }}` expression"
                                        .to_string(),
                                });
                            }
                            Some(b'\\') => {
                                self.pos += 1;
                                if self.peek().is_some() {
                                    self.pos += 1;
                                }
                            }
                            Some(c) if c == q => {
                                self.pos += 1;
                                break;
                            }
                            _ => {
                                self.pos += 1;
                            }
                        }
                    }
                }
                Some(b'{') => {
                    depth = depth.saturating_add(1);
                    self.pos += 1;
                }
                Some(b'}') => {
                    if depth > 0 {
                        depth -= 1;
                        self.pos += 1;
                    } else if self.peek_at(1) == Some(b'}') {
                        self.pos += 2;
                        return Ok(());
                    } else {
                        self.pos += 1;
                    }
                }
                _ => {
                    self.pos += 1;
                }
            }
        }
    }

    fn next_normal(&mut self) -> Result<Token, LexError> {
        // Skip horizontal whitespace, line continuations, and comments.
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\r') => {
                    self.pos += 1;
                }
                Some(b'\\') if matches!(self.peek_at(1), Some(b'\n'))
                    || (self.peek_at(1) == Some(b'\r') && self.peek_at(2) == Some(b'\n')) =>
                {
                    // `\<newline>` continuation is allowed in normal mode too, so
                    // long block headers can wrap.
                    self.pos += 1;
                    if self.peek() == Some(b'\r') {
                        self.pos += 1;
                    }
                    self.pos += 1; // consume \n
                }
                Some(b'#') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }

        let start = self.pos;
        let b = match self.peek() {
            None => {
                return Ok(Token {
                    kind: TokenKind::Eof,
                    span: self.make_span(start, start),
                });
            }
            Some(b) => b,
        };

        match b {
            b'\n' => {
                self.pos += 1;
                self.param_line = false;
                Ok(Token {
                    kind: TokenKind::Newline,
                    span: self.make_span(start, self.pos),
                })
            }
            b'{' if self.peek_at(1) == Some(b'{') => {
                // Standalone `{{ ... }}` in Normal mode. Only `repeat <ident>
                // in {{ expr }}` needs this today, but the lexer is agnostic
                // to position.
                self.scan_expression()?;
                Ok(Token {
                    kind: TokenKind::Expression,
                    span: self.make_span(start, self.pos),
                })
            }
            b'{' => {
                self.pos += 1;
                self.param_line = false;
                Ok(Token {
                    kind: TokenKind::LBrace,
                    span: self.make_span(start, self.pos),
                })
            }
            b'}' => {
                self.pos += 1;
                Ok(Token {
                    kind: TokenKind::RBrace,
                    span: self.make_span(start, self.pos),
                })
            }
            b'[' => {
                self.pos += 1;
                Ok(Token {
                    kind: TokenKind::LBracket,
                    span: self.make_span(start, self.pos),
                })
            }
            b']' => {
                self.pos += 1;
                Ok(Token {
                    kind: TokenKind::RBracket,
                    span: self.make_span(start, self.pos),
                })
            }
            b':' => {
                self.pos += 1;
                Ok(Token {
                    kind: TokenKind::Colon,
                    span: self.make_span(start, self.pos),
                })
            }
            b';' => {
                self.pos += 1;
                self.param_line = false;
                Ok(Token {
                    kind: TokenKind::Semi,
                    span: self.make_span(start, self.pos),
                })
            }
            b'=' => {
                self.pos += 1;
                self.mode = Mode::Value;
                Ok(Token {
                    kind: TokenKind::Equals,
                    span: self.make_span(start, self.pos),
                })
            }
            b'$' => {
                self.pos += 1;
                let name_start = self.pos;
                while let Some(c) = self.peek() {
                    if is_ident_cont(c) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if self.pos == name_start {
                    return Err(LexError {
                        span: self.make_span(start, self.pos),
                        message: "expected variable name after `$`".to_string(),
                    });
                }
                Ok(Token {
                    kind: TokenKind::DollarIdent,
                    span: self.make_span(start, self.pos),
                })
            }
            c if is_ident_start(c) => {
                // `a.b` is one identifier: a remote member
                // (`on_tap device1.restart { }`, schema 17). The schema
                // check decides where a dotted name is allowed.
                loop {
                    while let Some(c) = self.peek() {
                        if is_ident_cont(c) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    let dotted = self.peek() == Some(b'.')
                        && self.src.get(self.pos + 1).copied().is_some_and(is_ident_start);
                    if !dotted {
                        break;
                    }
                    self.pos += 1;
                }
                let span = self.make_span(start, self.pos);
                if &self.src[start..self.pos] == b"param" {
                    self.param_line = true;
                }
                // Contextual trigger: the `style` keyword inside a theme
                // body introduces a selector-headed sub-block
                // (`style .card { ... }`, `style button:pressed { ... }`).
                // The lexer flips to Selector mode so the next token
                // carries the raw selector text up to the `{`. The flip
                // is skipped if the ident is used in an ordinary
                // assignment position (`style = foo`, `style:x = ...`,
                // `style[x]:y = ...`), identified by the next
                // non-whitespace byte being `=`, `:`, or `[`.
                if &self.src[start..self.pos] == b"style" {
                    let mut look = self.pos;
                    while let Some(b) = self.src.get(look).copied() {
                        if b == b' ' || b == b'\t' {
                            look += 1;
                        } else {
                            break;
                        }
                    }
                    let next = self.src.get(look).copied();
                    if !matches!(next, Some(b'=') | Some(b':') | Some(b'[')) {
                        self.mode = Mode::Selector;
                    }
                }
                Ok(Token {
                    kind: TokenKind::Ident,
                    span,
                })
            }
            _ => {
                // Advance by one full UTF-8 character so the error span
                // and message are both correct and never slice through a
                // multi-byte sequence (which would panic in SourceMap::slice).
                let rest = &self.src[self.pos..];
                let ch = std::str::from_utf8(rest)
                    .ok()
                    .and_then(|s| s.chars().next());
                let (ch_text, n) = match ch {
                    Some(c) => (c.to_string(), c.len_utf8()),
                    None => ("?".to_string(), 1),
                };
                self.pos += n;
                Err(LexError {
                    span: self.make_span(start, self.pos),
                    message: format!("unexpected character `{ch_text}`"),
                })
            }
        }
    }

    fn next_value(&mut self) -> Result<Token, LexError> {
        // Spec: "Spaces around `=` ... are ignored." Trim leading horizontal
        // whitespace. The resulting `start` is the first byte of the value.
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }

        let start = self.pos;
        let mut end = start;
        // Depth of unmatched `{` inside the value. A `}` at depth 0
        // terminates the value (closing the enclosing block); a `}`
        // at depth > 0 just decrements. This lets a value hold an
        // awk / jq script with its own braces (`run = awk '{print}'`)
        // without needing to double-quote the whole value.
        let mut brace_depth: u32 = 0;
        loop {
            match self.peek() {
                None => break,
                Some(b'\n') => break,
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => break,
                Some(b';') if brace_depth == 0 => break,
                Some(b'}') if brace_depth == 0 => break,
                Some(b'}') => {
                    brace_depth -= 1;
                    self.pos += 1;
                    end = self.pos;
                }
                Some(b'#') => {
                    // `##` is a literal `#`.
                    if self.peek_at(1) == Some(b'#') {
                        self.pos += 2;
                        end = self.pos;
                        continue;
                    }
                    // `#` starts an in-value comment only when preceded by
                    // in-value whitespace (not at value start).
                    let preceded_by_ws = self.pos > start
                        && matches!(
                            self.src.get(self.pos - 1),
                            Some(b' ') | Some(b'\t')
                        );
                    if preceded_by_ws {
                        break;
                    }
                    self.pos += 1;
                    end = self.pos;
                }
                Some(b'\\') => {
                    // `\\` is a literal backslash pair; eat both bytes so a
                    // following `{{` opens an expression rather than being
                    // elided by a stray single-`\{{` escape.
                    if self.peek_at(1) == Some(b'\\') {
                        self.pos += 2;
                        end = self.pos;
                        continue;
                    }
                    // `\{{` escapes the expression opener per config-format.md.
                    // Consume the backslash so the following `{{` is left as
                    // literal text in the value span (interpretation strips
                    // the backslash later).
                    if self.peek_at(1) == Some(b'{') && self.peek_at(2) == Some(b'{') {
                        self.pos += 3;
                        end = self.pos;
                        continue;
                    }
                    // `\<newline>` continues the value on the next line.
                    let after_cr = if self.peek_at(1) == Some(b'\r') { 2 } else { 1 };
                    if self.peek_at(after_cr) == Some(b'\n') {
                        self.pos += after_cr + 1;
                        end = self.pos;
                        continue;
                    }
                    self.pos += 1;
                    end = self.pos;
                }
                Some(b'"') => {
                    let quote_start = self.pos;
                    self.pos += 1;
                    loop {
                        match self.peek() {
                            None => {
                                return Err(LexError {
                                    span: self.make_span(quote_start, self.pos),
                                    message: "unterminated quoted string in value".to_string(),
                                });
                            }
                            Some(b'\\') => {
                                self.pos += 1;
                                if self.peek().is_some() {
                                    self.pos += 1;
                                }
                            }
                            Some(b'"') => {
                                self.pos += 1;
                                break;
                            }
                            _ => {
                                self.pos += 1;
                            }
                        }
                    }
                    end = self.pos;
                }
                Some(b'{') if self.peek_at(1) == Some(b'{') => {
                    self.scan_expression()?;
                    end = self.pos;
                }
                Some(b'{')
                    if self.param_line
                        && brace_depth == 0
                        // Some default read already: `= {"k": 1}` is
                        // a default that starts with a brace.
                        && end > start
                        && matches!(self.src.get(self.pos.wrapping_sub(1)), Some(b' ') | Some(b'\t')) =>
                {
                    // The param's options block, not part of the default.
                    break;
                }
                Some(b'{') => {
                    brace_depth = brace_depth.saturating_add(1);
                    self.pos += 1;
                    end = self.pos;
                }
                Some(b'\'') => {
                    // A `'...'` single-quoted string inside the value
                    // keeps literal `{` / `}` / `;` from affecting
                    // termination. Shell-style single quotes have no
                    // escape sequence; the next `'` ends the span.
                    let quote_start = self.pos;
                    self.pos += 1;
                    loop {
                        match self.peek() {
                            None => {
                                return Err(LexError {
                                    span: self.make_span(quote_start, self.pos),
                                    message: "unterminated single-quoted string in value"
                                        .to_string(),
                                });
                            }
                            Some(b'\'') => {
                                self.pos += 1;
                                break;
                            }
                            _ => {
                                self.pos += 1;
                            }
                        }
                    }
                    end = self.pos;
                }
                _ => {
                    self.pos += 1;
                    end = self.pos;
                }
            }
        }

        // Trim trailing in-value whitespace, matching the spec's "spaces at
        // both ends of the value are ignored". The main loop never advances
        // `end` across a terminator, so this scan stays inside the value.
        let mut trimmed_end = end;
        while trimmed_end > start {
            let b = self.src[trimmed_end - 1];
            if b == b' ' || b == b'\t' {
                trimmed_end -= 1;
            } else {
                break;
            }
        }

        self.mode = Mode::Normal;
        Ok(Token {
            kind: TokenKind::RawValue,
            span: self.make_span(start, trimmed_end),
        })
    }

    /// Scan the body of a theme-style selector (`.card`,
    /// `button:pressed`, `*`, ...). The selector runs from the current
    /// position up to the next `{`, with leading and trailing
    /// whitespace trimmed. An empty selector is accepted (`style { }`
    /// means "every node"). If `{` is not found, we emit an error and
    /// return a Selector token covering what we read, which lets the
    /// parser recover.
    fn next_selector(&mut self) -> Result<Token, LexError> {
        // Trim leading horizontal whitespace. A newline before the
        // selector is allowed (the user may write `style<newline>.card`
        // -- unusual but we don't want to crash). An inline `#` comment
        // is not supported in selector text; `#` is a legal CSS id
        // character and we don't want to swallow it.
        while matches!(self.peek(), Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')) {
            self.pos += 1;
        }
        let start = self.pos;
        let mut end = start;
        loop {
            match self.peek() {
                None => {
                    self.mode = Mode::Normal;
                    return Err(LexError {
                        span: self.make_span(start, self.pos),
                        message: "expected `{` after `style` selector".to_string(),
                    });
                }
                Some(b'{') => break,
                Some(b'\n') => {
                    // A newline inside a selector is a hard error -- the
                    // body-opening `{` must be on the same line.
                    self.mode = Mode::Normal;
                    return Err(LexError {
                        span: self.make_span(start, self.pos),
                        message: "a `style` selector must be followed by `{` on the same line"
                            .to_string(),
                    });
                }
                Some(_) => {
                    self.pos += 1;
                    end = self.pos;
                }
            }
        }
        // Trim trailing whitespace from the selector span.
        while end > start {
            let b = self.src[end - 1];
            if b == b' ' || b == b'\t' || b == b'\r' {
                end -= 1;
            } else {
                break;
            }
        }
        self.mode = Mode::Normal;
        Ok(Token {
            kind: TokenKind::Selector,
            span: self.make_span(start, end),
        })
    }
}

/// Lex a whole source file, collecting tokens and errors. Returns every
/// token up to and including `Eof`. On a lex error the lexer recovers to
/// the next line and continues, so one run reports as many errors as it
/// can find.
pub fn tokenize(file: FileId, src: &str) -> (Vec<Token>, Vec<LexError>) {
    let mut lx = Lexer::new(file, src);
    let mut toks = Vec::new();
    let mut errs = Vec::new();
    loop {
        match lx.next_token() {
            Ok(t) => {
                let is_eof = t.kind == TokenKind::Eof;
                toks.push(t);
                if is_eof {
                    break;
                }
            }
            Err(e) => {
                errs.push(e);
                // The lexer methods always advance past the offending bytes
                // before returning Err (an unterminated string or `{{` goes
                // all the way to EOF; a bad char consumes exactly that
                // char), so we only need to flip back to Normal mode and
                // keep going. Multi-error collection in one pass.
                lx.mode = Mode::Normal;
            }
        }
    }
    (toks, errs)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceMap;
    use std::path::PathBuf;

    /// Lex `src` and return `(kind, slice)` pairs for every token except
    /// the final `Eof`.
    fn lex(src: &str) -> Vec<(TokenKind, String)> {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, errs) = tokenize(id, src);
        assert!(errs.is_empty(), "unexpected lex errors: {errs:?}");
        toks.into_iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .map(|t| (t.kind, map.slice(t.span).to_string()))
            .collect()
    }

    fn lex_with_errors(src: &str) -> (Vec<(TokenKind, String)>, Vec<LexError>) {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, errs) = tokenize(id, src);
        let out = toks
            .into_iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .map(|t| (t.kind, map.slice(t.span).to_string()))
            .collect();
        (out, errs)
    }

    use TokenKind::*;

    #[test]
    fn simple_assignment() {
        assert_eq!(
            lex("key = value"),
            vec![
                (Ident, "key".into()),
                (Equals, "=".into()),
                (RawValue, "value".into()),
            ]
        );
    }

    #[test]
    fn block_with_name() {
        assert_eq!(
            lex("source cpu_temp { poll = 30s }"),
            vec![
                (Ident, "source".into()),
                (Ident, "cpu_temp".into()),
                (LBrace, "{".into()),
                (Ident, "poll".into()),
                (Equals, "=".into()),
                (RawValue, "30s".into()),
                (RBrace, "}".into()),
            ]
        );
    }

    #[test]
    fn category_colon_key() {
        assert_eq!(
            lex("screen:idle = 2m"),
            vec![
                (Ident, "screen".into()),
                (Colon, ":".into()),
                (Ident, "idle".into()),
                (Equals, "=".into()),
                (RawValue, "2m".into()),
            ]
        );
    }

    #[test]
    fn indexed_colon_key() {
        assert_eq!(
            lex("theme[dark]:accent = #44aaff"),
            vec![
                (Ident, "theme".into()),
                (LBracket, "[".into()),
                (Ident, "dark".into()),
                (RBracket, "]".into()),
                (Colon, ":".into()),
                (Ident, "accent".into()),
                (Equals, "=".into()),
                (RawValue, "#44aaff".into()),
            ]
        );
    }

    #[test]
    fn dollar_var_def_and_ref() {
        let out = lex("$pve = https://host:8006/api\ninclude = foo.conf");
        assert_eq!(out[0], (DollarIdent, "$pve".into()));
        assert_eq!(out[1], (Equals, "=".into()));
        assert_eq!(out[2], (RawValue, "https://host:8006/api".into()));
        assert_eq!(out[3], (Newline, "\n".into()));
        assert_eq!(out[4], (Ident, "include".into()));
        assert_eq!(out[5], (Equals, "=".into()));
        assert_eq!(out[6], (RawValue, "foo.conf".into()));
    }

    #[test]
    fn semi_separates_assignments_on_one_line() {
        assert_eq!(
            lex("text { value = Hi; color = red }"),
            vec![
                (Ident, "text".into()),
                (LBrace, "{".into()),
                (Ident, "value".into()),
                (Equals, "=".into()),
                (RawValue, "Hi".into()),
                (Semi, ";".into()),
                (Ident, "color".into()),
                (Equals, "=".into()),
                (RawValue, "red".into()),
                (RBrace, "}".into()),
            ]
        );
    }

    #[test]
    fn trailing_spaces_in_value_are_trimmed() {
        assert_eq!(
            lex("k = foo   \n"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "foo".into()),
                (Newline, "\n".into()),
            ]
        );
    }

    #[test]
    fn comment_on_its_own_line_is_skipped() {
        assert_eq!(
            lex("# a comment\nk = v"),
            vec![
                (Newline, "\n".into()),
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "v".into()),
            ]
        );
    }

    #[test]
    fn inline_comment_after_value_with_space() {
        // Space before `#` makes it a comment.
        assert_eq!(
            lex("k = foo # trailing"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "foo".into()),
            ]
        );
    }

    #[test]
    fn hash_in_value_without_leading_space_is_literal() {
        // No space before `#`, and `#` is first in value: literal (color).
        assert_eq!(
            lex("color = #44aaff"),
            vec![
                (Ident, "color".into()),
                (Equals, "=".into()),
                (RawValue, "#44aaff".into()),
            ]
        );
        // No space before `#` in the middle: still literal.
        assert_eq!(
            lex("k = foo#bar"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "foo#bar".into()),
            ]
        );
    }

    #[test]
    fn double_hash_in_value_is_literal_text() {
        // ## is literal; the span still contains ##, interpretation unescapes.
        assert_eq!(
            lex("k = a##b"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "a##b".into()),
            ]
        );
    }

    #[test]
    fn quoted_value_keeps_terminators_inside() {
        assert_eq!(
            lex(r#"k = "foo; bar } #baz""#),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, r#""foo; bar } #baz""#.into()),
            ]
        );
    }

    #[test]
    fn quoted_value_handles_backslash_escapes() {
        assert_eq!(
            lex(r#"k = "a\"b""#),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, r#""a\"b""#.into()),
            ]
        );
    }

    #[test]
    fn expression_in_value_masks_terminators() {
        // The `;` and `}` inside `{{ }}` do not end the value.
        assert_eq!(
            lex("k = {{ a; b.c }}"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "{{ a; b.c }}".into()),
            ]
        );
    }

    #[test]
    fn expression_in_value_handles_nested_braces() {
        assert_eq!(
            lex("k = {{ f({\"a\": 1}) }}"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "{{ f({\"a\": 1}) }}".into()),
            ]
        );
    }

    #[test]
    fn expression_followed_by_text() {
        assert_eq!(
            lex("label = CPU {{ x }} C"),
            vec![
                (Ident, "label".into()),
                (Equals, "=".into()),
                (RawValue, "CPU {{ x }} C".into()),
            ]
        );
    }

    #[test]
    fn backslash_newline_continues_value() {
        let out = lex("k = foo \\\n    bar");
        assert_eq!(out[0], (Ident, "k".into()));
        assert_eq!(out[1], (Equals, "=".into()));
        assert_eq!(out[2].0, RawValue);
        assert_eq!(out[2].1, "foo \\\n    bar");
    }

    #[test]
    fn backslash_cr_newline_continues_value() {
        let out = lex("k = foo \\\r\n    bar");
        assert_eq!(out[2].0, RawValue);
        assert_eq!(out[2].1, "foo \\\r\n    bar");
    }

    #[test]
    fn empty_value() {
        assert_eq!(
            lex("k ="),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "".into()),
            ]
        );
    }

    #[test]
    fn newlines_are_emitted_as_tokens() {
        let out = lex("a = 1\nb = 2");
        assert_eq!(
            out,
            vec![
                (Ident, "a".into()),
                (Equals, "=".into()),
                (RawValue, "1".into()),
                (Newline, "\n".into()),
                (Ident, "b".into()),
                (Equals, "=".into()),
                (RawValue, "2".into()),
            ]
        );
    }

    #[test]
    fn unterminated_quoted_string_is_an_error() {
        let (_, errs) = lex_with_errors(r#"k = "hello"#);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unterminated quoted string"));
    }

    #[test]
    fn unterminated_expression_is_an_error() {
        let (_, errs) = lex_with_errors("k = {{ a + b");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unterminated `{{`"));
    }

    #[test]
    fn dollar_without_name_is_an_error() {
        let (_, errs) = lex_with_errors("$ = foo");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("expected variable name"));
    }

    #[test]
    fn eof_is_terminal() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), "k=1".to_string());
        let mut lx = Lexer::new(id, "k=1");
        // Consume everything.
        for _ in 0..5 {
            let t = lx.next_token().unwrap();
            if t.kind == TokenKind::Eof {
                let t2 = lx.next_token().unwrap();
                assert_eq!(t2.kind, TokenKind::Eof);
                return;
            }
        }
        panic!("lexer did not reach EOF");
    }

    #[test]
    fn backslash_newline_in_normal_mode() {
        // `kind name { ... }` header wrapped over two lines.
        let out = lex("source \\\n    cpu_temp {\n}");
        assert_eq!(
            out,
            vec![
                (Ident, "source".into()),
                (Ident, "cpu_temp".into()),
                (LBrace, "{".into()),
                (Newline, "\n".into()),
                (RBrace, "}".into()),
            ]
        );
    }

    #[test]
    fn spans_point_at_original_bytes() {
        let src = "key = value\n";
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, _) = tokenize(id, src);
        assert_eq!(toks[0].kind, TokenKind::Ident);
        assert_eq!(toks[0].span.start, 0);
        assert_eq!(toks[0].span.end, 3);
        assert_eq!(toks[1].kind, TokenKind::Equals);
        assert_eq!(toks[1].span.start, 4);
        assert_eq!(toks[1].span.end, 5);
        assert_eq!(toks[2].kind, TokenKind::RawValue);
        assert_eq!(toks[2].span.start, 6);
        assert_eq!(toks[2].span.end, 11);
    }

    #[test]
    fn recovery_after_lex_error_continues() {
        let (toks, errs) = lex_with_errors("$ = foo\nk = v");
        // One error on line 1, then line 2 lexes cleanly.
        assert_eq!(errs.len(), 1);
        // After recovery to the newline, the second line's tokens appear.
        assert!(
            toks.iter().any(|(k, s)| *k == TokenKind::Ident && s == "k"),
            "expected `k` ident after recovery, got {toks:?}"
        );
    }

    #[test]
    fn backslash_escapes_double_brace_in_value_without_close() {
        // Should NOT be an "unterminated `{{`" error; whole string is literal.
        assert_eq!(
            lex(r"k = \{{ hi"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, r"\{{ hi".into()),
            ]
        );
    }

    #[test]
    fn backslash_escapes_double_brace_followed_by_safe_text() {
        // After `\{{`, the braces are literal. A later `}` is NOT an
        // escape (there is no `\}}` counter-escape in the spec), so a
        // trailing `}}` would end the value. Here the trailing text has
        // no `}`, so the whole run is one value.
        assert_eq!(
            lex(r"k = \{{ not an expr"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, r"\{{ not an expr".into()),
            ]
        );
    }

    #[test]
    fn crlf_terminates_value_without_leaking_cr() {
        let out = lex("k = foo\r\nbar = 1\r\n");
        // out[0] k, out[1] =, out[2] value, out[3] newline, ...
        assert_eq!(out[2], (RawValue, "foo".into()));
        assert_eq!(out[6], (RawValue, "1".into()));
    }

    #[test]
    fn non_ascii_at_normal_position_errors_at_char_boundary() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), "\u{20ac} = 1".to_string());
        let (toks, errs) = tokenize(id, "\u{20ac} = 1");
        assert_eq!(errs.len(), 1);
        // The span must be a valid UTF-8 slice (does not panic).
        let s = map.slice(errs[0].span);
        assert_eq!(s, "\u{20ac}");
        assert!(errs[0].message.contains('\u{20ac}'));
        // After recovery the Value "1" still lands in the stream.
        assert!(toks.iter().any(|t| t.kind == TokenKind::RawValue
            && map.slice(t.span) == "1"));
    }

    #[test]
    fn utf8_value_passes_through() {
        assert_eq!(
            lex("k = caf\u{e9}"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "caf\u{e9}".into()),
            ]
        );
    }

    #[test]
    fn string_in_expression_contains_doubled_close() {
        let out = lex(r#"k = {{ "}}" }}"#);
        assert_eq!(out[2], (RawValue, r#"{{ "}}" }}"#.into()));
    }

    #[test]
    fn single_quoted_string_in_expression_contains_double_open() {
        assert_eq!(
            lex("k = {{ '{{' }}"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "{{ '{{' }}".into()),
            ]
        );
    }

    #[test]
    fn empty_value_followed_by_semicolon() {
        let out = lex("k = ; foo = 1");
        assert_eq!(out[2], (RawValue, "".into()));
        assert_eq!(out[3], (Semi, ";".into()));
    }

    #[test]
    fn hash_hash_in_middle_after_space_is_literal_hash() {
        assert_eq!(
            lex("k = foo ##bar"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "foo ##bar".into()),
            ]
        );
    }

    #[test]
    fn glob_in_value() {
        assert_eq!(
            lex("include = pages/*.conf"),
            vec![
                (Ident, "include".into()),
                (Equals, "=".into()),
                (RawValue, "pages/*.conf".into()),
            ]
        );
    }

    #[test]
    fn balanced_braces_in_value() {
        // `}` at depth 0 still terminates the value, so the outer
        // block closes correctly; `}` inside an open `{...}` just
        // decrements depth.
        assert_eq!(
            lex("source s { run = awk /x/ {print 1} /tmp/f\n}"),
            vec![
                (Ident, "source".into()),
                (Ident, "s".into()),
                (LBrace, "{".into()),
                (Ident, "run".into()),
                (Equals, "=".into()),
                (RawValue, "awk /x/ {print 1} /tmp/f".into()),
                (Newline, "\n".into()),
                (RBrace, "}".into()),
            ]
        );
    }

    #[test]
    fn single_quoted_string_in_value_protects_braces_and_semicolons() {
        assert_eq!(
            lex("run = awk '/x/ {print; next}' /tmp/f\n"),
            vec![
                (Ident, "run".into()),
                (Equals, "=".into()),
                (RawValue, "awk '/x/ {print; next}' /tmp/f".into()),
                (Newline, "\n".into()),
            ]
        );
    }

    #[test]
    fn consecutive_newlines_each_emit_a_token() {
        let out = lex("a=1\n\nb=2");
        assert_eq!(out.iter().filter(|(k, _)| *k == Newline).count(), 2);
    }

    #[test]
    fn backslash_at_eof_is_literal() {
        assert_eq!(
            lex("k = foo\\"),
            vec![
                (Ident, "k".into()),
                (Equals, "=".into()),
                (RawValue, "foo\\".into()),
            ]
        );
    }

    #[test]
    fn trailing_comment_with_crlf() {
        let out = lex("k = foo # tr\r\n");
        assert_eq!(out[2], (RawValue, "foo".into()));
    }

    #[test]
    fn unterminated_expression_error_span_starts_at_opener() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), "k = {{ a".to_string());
        let (_, errs) = tokenize(id, "k = {{ a");
        assert_eq!(errs.len(), 1);
        // Error span must start at the `{{`, not after it.
        assert_eq!(map.slice(errs[0].span), "{{ a");
    }

    #[test]
    fn unterminated_string_error_span_starts_at_quote() {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), r#"k = "hi"#.to_string());
        let (_, errs) = tokenize(id, r#"k = "hi"#);
        assert_eq!(errs.len(), 1);
        assert_eq!(map.slice(errs[0].span), r#""hi"#);
    }

    #[test]
    fn expression_in_normal_mode_is_a_token() {
        assert_eq!(
            lex("repeat item in {{ sources.vms.value }} {\n}"),
            vec![
                (Ident, "repeat".into()),
                (Ident, "item".into()),
                (Ident, "in".into()),
                (Expression, "{{ sources.vms.value }}".into()),
                (LBrace, "{".into()),
                (Newline, "\n".into()),
                (RBrace, "}".into()),
            ]
        );
    }

    #[test]
    fn unterminated_expression_in_normal_mode_is_an_error() {
        let (_, errs) = lex_with_errors("repeat x in {{ foo");
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("unterminated"));
    }

    #[test]
    fn token_kind_is_structural() {
        assert!(TokenKind::Equals.is_structural());
        assert!(TokenKind::LBrace.is_structural());
        assert!(TokenKind::Newline.is_structural());
        assert!(!TokenKind::Ident.is_structural());
        assert!(!TokenKind::RawValue.is_structural());
        assert!(!TokenKind::DollarIdent.is_structural());
        assert!(!TokenKind::Eof.is_structural());
    }
}
