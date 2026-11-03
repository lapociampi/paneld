//! A small CEL (Common Expression Language) parser.
//!
//! CEL (<https://cel.dev>) is the expression language embedded in
//! `{{ ... }}` values in paneld configs. This module parses one CEL
//! fragment (the text between `{{` and `}}`) into an AST, with spans
//! translated back into the enclosing source file so diagnostics
//! point at the exact character that failed.
//!
//! Scope: parse the subset of CEL that paneld configs actually need
//! (literals, identifiers, field / index access, function and method
//! calls, unary `-` / `!`, binary arithmetic / comparison / logical,
//! ternary `a ? b : c`, list and map literals). Full type checking
//! -- method lookup, numeric coercion rules, overload resolution --
//! is left to the engine; here we only parse and surface shape
//! errors and `sources.<name>` cross-references.
//!
//! The parser is intentionally hand-written to stay consistent with
//! ADR 0009 (no parser-generator dependency).

use crate::parse::Diagnostic;
use crate::source::{FileId, Span};

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    /// `42`, `0x2a`
    IntLit(i64),
    /// `3.14`, `1e9`
    DoubleLit(f64),
    /// `"hi"`, `'hi'`
    StringLit(String),
    /// `true` / `false`
    BoolLit(bool),
    /// `null`
    Null,
    /// A bare identifier (`sources`, `item`, `now`, ...).
    Ident(String),
    /// Field access: `a.b`.
    Field(Box<Expr>, String),
    /// Index: `a[b]`.
    Index(Box<Expr>, Box<Expr>),
    /// Free function call: `f(a, b)`.
    Call(String, Vec<Expr>),
    /// Method call: `a.m(b, c)`.
    Method(Box<Expr>, String, Vec<Expr>),
    /// `[a, b, c]`.
    List(Vec<Expr>),
    /// `{k: v, ...}`.
    Map(Vec<(Expr, Expr)>),
    /// `-x`, `!x`.
    Unary(UnaryOp, Box<Expr>),
    /// `a + b`, `a && b`, `a in b`.
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    /// `a ? b : c`.
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    In,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse one CEL expression body (the bytes between `{{` and `}}`),
/// returning the AST plus any diagnostics.
///
/// `base` is the byte offset of the first byte of `src` inside the
/// enclosing source file; `file` names that file. Spans in the result
/// are translated into the enclosing file's coordinates.
pub fn parse_fragment(src: &str, file: FileId, base: u32) -> (Option<Expr>, Vec<Diagnostic>) {
    let mut parser = Parser::new(src, file, base);
    let expr = parser.parse_expression();
    // Any tokens left over after the top expression are a trailing
    // garbage error.
    if expr.is_some() && parser.peek().kind != Tk::Eof {
        let tok = *parser.peek();
        let lex = parser.token_lexeme(&tok);
        parser.diags.push(Diagnostic::new(
            tok.span,
            format!("unexpected `{lex}` after expression"),
        ));
    }
    (expr, parser.diags)
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tk {
    Ident,
    IntLit,
    DoubleLit,
    StringLit,
    True,
    False,
    Null,
    In,
    LParen,
    RParen,
    LBrack,
    RBrack,
    LBrace,
    RBrace,
    Comma,
    Dot,
    Colon,
    Question,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Amp2,
    Pipe2,
    Eq2,
    BangEq,
    Lt,
    Le,
    Gt,
    Ge,
    Eof,
    /// Lex error marker; the parser surfaces this as a diagnostic.
    Bad,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Tk,
    span: Span,
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    file: FileId,
    base: u32,
    diags: Vec<Diagnostic>,
    last_slice_text: String,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, file: FileId, base: u32) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            file,
            base,
            diags: Vec::new(),
            last_slice_text: String::new(),
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, self.base + start as u32, self.base + end as u32)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Token {
        self.skip_ws();
        let start = self.pos;
        let Some(b) = self.peek() else {
            return Token {
                kind: Tk::Eof,
                span: self.span(start, start),
            };
        };
        match b {
            b'(' => self.single(Tk::LParen),
            b')' => self.single(Tk::RParen),
            b'[' => self.single(Tk::LBrack),
            b']' => self.single(Tk::RBrack),
            b'{' => self.single(Tk::LBrace),
            b'}' => self.single(Tk::RBrace),
            b',' => self.single(Tk::Comma),
            b'.' => {
                // `.5` isn't allowed in CEL (numbers must start with a
                // digit), so `.` is always a field accessor.
                self.single(Tk::Dot)
            }
            b':' => self.single(Tk::Colon),
            b'?' => self.single(Tk::Question),
            b'+' => self.single(Tk::Plus),
            b'-' => self.single(Tk::Minus),
            b'*' => self.single(Tk::Star),
            b'/' => self.single(Tk::Slash),
            b'%' => self.single(Tk::Percent),
            b'!' => {
                if self.peek_at(1) == Some(b'=') {
                    self.pos += 2;
                    Token {
                        kind: Tk::BangEq,
                        span: self.span(start, self.pos),
                    }
                } else {
                    self.single(Tk::Bang)
                }
            }
            b'=' if self.peek_at(1) == Some(b'=') => {
                self.pos += 2;
                Token {
                    kind: Tk::Eq2,
                    span: self.span(start, self.pos),
                }
            }
            b'<' => {
                if self.peek_at(1) == Some(b'=') {
                    self.pos += 2;
                    Token {
                        kind: Tk::Le,
                        span: self.span(start, self.pos),
                    }
                } else {
                    self.single(Tk::Lt)
                }
            }
            b'>' => {
                if self.peek_at(1) == Some(b'=') {
                    self.pos += 2;
                    Token {
                        kind: Tk::Ge,
                        span: self.span(start, self.pos),
                    }
                } else {
                    self.single(Tk::Gt)
                }
            }
            b'&' if self.peek_at(1) == Some(b'&') => {
                self.pos += 2;
                Token {
                    kind: Tk::Amp2,
                    span: self.span(start, self.pos),
                }
            }
            b'|' if self.peek_at(1) == Some(b'|') => {
                self.pos += 2;
                Token {
                    kind: Tk::Pipe2,
                    span: self.span(start, self.pos),
                }
            }
            b'"' | b'\'' => self.scan_string(start, b),
            b'0'..=b'9' => self.scan_number(start),
            c if c.is_ascii_alphabetic() || c == b'_' => self.scan_ident(start),
            _ => {
                // Advance past the single bad byte (or UTF-8 char) so
                // the parser can recover.
                let rest = &self.src[self.pos..];
                let n = rest
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1);
                let span = self.span(start, start + n);
                self.diags.push(Diagnostic::new(
                    span,
                    format!("unexpected character `{}` in expression", &self.src[start..start + n]),
                ));
                self.pos += n;
                Token {
                    kind: Tk::Bad,
                    span,
                }
            }
        }
    }

    fn single(&mut self, kind: Tk) -> Token {
        let start = self.pos;
        self.pos += 1;
        Token {
            kind,
            span: self.span(start, self.pos),
        }
    }

    fn scan_ident(&mut self, start: usize) -> Token {
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        let kind = match text {
            "true" => Tk::True,
            "false" => Tk::False,
            "null" => Tk::Null,
            "in" => Tk::In,
            _ => Tk::Ident,
        };
        self.last_slice_text = text.to_string();
        Token {
            kind,
            span: self.span(start, self.pos),
        }
    }

    fn scan_number(&mut self, start: usize) -> Token {
        // Integer or double. CEL ints can be hex (`0x...`) or decimal.
        // Doubles have a `.` or an `e`/`E` exponent. The parser later
        // converts the lexeme by re-reading its bytes.
        let mut is_double = false;
        // Optional `0x` prefix.
        if self.peek() == Some(b'0') && matches!(self.peek_at(1), Some(b'x') | Some(b'X')) {
            self.pos += 2;
            while let Some(b) = self.peek() {
                if b.is_ascii_hexdigit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            // Optional `u` suffix for unsigned, which we accept but
            // treat as int.
            if self.peek() == Some(b'u') || self.peek() == Some(b'U') {
                self.pos += 1;
            }
        } else {
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.peek() == Some(b'.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit())
            {
                is_double = true;
                self.pos += 1; // '.'
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            if matches!(self.peek(), Some(b'e') | Some(b'E')) {
                is_double = true;
                self.pos += 1;
                if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                    self.pos += 1;
                }
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            if self.peek() == Some(b'u') || self.peek() == Some(b'U') {
                self.pos += 1;
            }
        }
        self.last_slice_text = self.src[start..self.pos].to_string();
        Token {
            kind: if is_double { Tk::DoubleLit } else { Tk::IntLit },
            span: self.span(start, self.pos),
        }
    }

    fn scan_string(&mut self, start: usize, quote: u8) -> Token {
        self.pos += 1; // opening quote
        let body_start = self.pos;
        loop {
            match self.peek() {
                None => {
                    let span = self.span(start, self.pos);
                    self.diags.push(Diagnostic::new(
                        span,
                        "unterminated string in expression".to_string(),
                    ));
                    return Token {
                        kind: Tk::Bad,
                        span,
                    };
                }
                Some(b'\\') => {
                    self.pos += 1;
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                }
                Some(c) if c == quote => {
                    let body = self.src[body_start..self.pos].to_string();
                    self.pos += 1; // closing quote
                    self.last_slice_text = body;
                    return Token {
                        kind: Tk::StringLit,
                        span: self.span(start, self.pos),
                    };
                }
                Some(_) => {
                    self.pos += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    lx: Lexer<'a>,
    cur: Token,
    cur_text: String,
    src: &'a str,
    diags: Vec<Diagnostic>,
    /// If a `scan_string` populated `last_slice_text` for the current
    /// token, this holds that body (unescaped later during AST
    /// construction).
    cur_string_body: Option<String>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, file: FileId, base: u32) -> Self {
        let mut lx = Lexer::new(src, file, base);
        let cur = lx.next_token();
        let cur_text = lx.last_slice_text.clone();
        let cur_string_body = if cur.kind == Tk::StringLit {
            Some(cur_text.clone())
        } else {
            None
        };
        let diags = std::mem::take(&mut lx.diags);
        Self {
            lx,
            cur,
            cur_text,
            src,
            diags,
            cur_string_body,
        }
    }

    fn peek(&self) -> &Token {
        &self.cur
    }

    fn bump(&mut self) -> Token {
        let prev = self.cur;
        self.cur = self.lx.next_token();
        self.cur_text = self.lx.last_slice_text.clone();
        self.cur_string_body = if self.cur.kind == Tk::StringLit {
            Some(self.cur_text.clone())
        } else {
            None
        };
        self.diags.append(&mut self.lx.diags);
        prev
    }

    fn expect(&mut self, kind: Tk, what: &str) -> Option<Token> {
        if self.cur.kind == kind {
            Some(self.bump())
        } else {
            let tok = self.cur;
            self.diags.push(Diagnostic::new(
                tok.span,
                format!("expected {what}, got `{}`", self.token_lexeme(&tok)),
            ));
            None
        }
    }

    fn token_lexeme(&self, tok: &Token) -> String {
        let local_start = (tok.span.start - self.lx.base) as usize;
        let local_end = (tok.span.end - self.lx.base) as usize;
        if local_end <= self.src.len() {
            self.src[local_start..local_end].to_string()
        } else {
            String::new()
        }
    }

    fn parse_expression(&mut self) -> Option<Expr> {
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Option<Expr> {
        let cond = self.parse_or()?;
        if self.cur.kind == Tk::Question {
            let qmark = self.bump();
            let then_br = self.parse_or()?;
            self.expect(Tk::Colon, "`:` in ternary")?;
            let else_br = self.parse_ternary()?;
            let span = cond.span.join(else_br.span);
            let _ = qmark;
            return Some(Expr {
                kind: ExprKind::Conditional(Box::new(cond), Box::new(then_br), Box::new(else_br)),
                span,
            });
        }
        Some(cond)
    }

    fn parse_or(&mut self) -> Option<Expr> {
        let mut left = self.parse_and()?;
        while self.cur.kind == Tk::Pipe2 {
            self.bump();
            let right = self.parse_and()?;
            let span = left.span.join(right.span);
            left = Expr {
                kind: ExprKind::Binary(BinaryOp::Or, Box::new(left), Box::new(right)),
                span,
            };
        }
        Some(left)
    }

    fn parse_and(&mut self) -> Option<Expr> {
        let mut left = self.parse_relation()?;
        while self.cur.kind == Tk::Amp2 {
            self.bump();
            let right = self.parse_relation()?;
            let span = left.span.join(right.span);
            left = Expr {
                kind: ExprKind::Binary(BinaryOp::And, Box::new(left), Box::new(right)),
                span,
            };
        }
        Some(left)
    }

    fn parse_relation(&mut self) -> Option<Expr> {
        let mut left = self.parse_addition()?;
        loop {
            let op = match self.cur.kind {
                Tk::Lt => BinaryOp::Lt,
                Tk::Le => BinaryOp::Le,
                Tk::Gt => BinaryOp::Gt,
                Tk::Ge => BinaryOp::Ge,
                Tk::Eq2 => BinaryOp::Eq,
                Tk::BangEq => BinaryOp::Ne,
                Tk::In => BinaryOp::In,
                _ => break,
            };
            self.bump();
            let right = self.parse_addition()?;
            let span = left.span.join(right.span);
            left = Expr {
                kind: ExprKind::Binary(op, Box::new(left), Box::new(right)),
                span,
            };
        }
        Some(left)
    }

    fn parse_addition(&mut self) -> Option<Expr> {
        let mut left = self.parse_multiplication()?;
        loop {
            let op = match self.cur.kind {
                Tk::Plus => BinaryOp::Add,
                Tk::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.parse_multiplication()?;
            let span = left.span.join(right.span);
            left = Expr {
                kind: ExprKind::Binary(op, Box::new(left), Box::new(right)),
                span,
            };
        }
        Some(left)
    }

    fn parse_multiplication(&mut self) -> Option<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.cur.kind {
                Tk::Star => BinaryOp::Mul,
                Tk::Slash => BinaryOp::Div,
                Tk::Percent => BinaryOp::Mod,
                _ => break,
            };
            self.bump();
            let right = self.parse_unary()?;
            let span = left.span.join(right.span);
            left = Expr {
                kind: ExprKind::Binary(op, Box::new(left), Box::new(right)),
                span,
            };
        }
        Some(left)
    }

    fn parse_unary(&mut self) -> Option<Expr> {
        match self.cur.kind {
            Tk::Minus => {
                let op_tok = self.bump();
                let inner = self.parse_unary()?;
                let span = op_tok.span.join(inner.span);
                Some(Expr {
                    kind: ExprKind::Unary(UnaryOp::Neg, Box::new(inner)),
                    span,
                })
            }
            Tk::Bang => {
                let op_tok = self.bump();
                let inner = self.parse_unary()?;
                let span = op_tok.span.join(inner.span);
                Some(Expr {
                    kind: ExprKind::Unary(UnaryOp::Not, Box::new(inner)),
                    span,
                })
            }
            _ => self.parse_member(),
        }
    }

    fn parse_member(&mut self) -> Option<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            match self.cur.kind {
                Tk::Dot => {
                    self.bump();
                    let ident_tok = self.cur;
                    if ident_tok.kind != Tk::Ident {
                        self.diags.push(Diagnostic::new(
                            ident_tok.span,
                            "expected a field name after `.`".to_string(),
                        ));
                        return None;
                    }
                    let name = self.cur_text.clone();
                    self.bump();
                    // Method call?
                    if self.cur.kind == Tk::LParen {
                        self.bump();
                        let args = self.parse_arg_list()?;
                        let rparen = self.expect(Tk::RParen, "`)`")?;
                        let span = e.span.join(rparen.span);
                        e = Expr {
                            kind: ExprKind::Method(Box::new(e), name, args),
                            span,
                        };
                    } else {
                        let span = e.span.join(ident_tok.span);
                        e = Expr {
                            kind: ExprKind::Field(Box::new(e), name),
                            span,
                        };
                    }
                }
                Tk::LBrack => {
                    self.bump();
                    let index = self.parse_expression()?;
                    let rbrack = self.expect(Tk::RBrack, "`]`")?;
                    let span = e.span.join(rbrack.span);
                    e = Expr {
                        kind: ExprKind::Index(Box::new(e), Box::new(index)),
                        span,
                    };
                }
                _ => break,
            }
        }
        Some(e)
    }

    fn parse_primary(&mut self) -> Option<Expr> {
        let tok = self.cur;
        match tok.kind {
            Tk::IntLit => {
                let text = self.cur_text.clone();
                self.bump();
                let v = parse_int_literal(&text).ok_or_else(|| {
                    self.diags.push(Diagnostic::new(
                        tok.span,
                        format!("invalid integer literal `{text}`"),
                    ));
                });
                let kind = match v {
                    Ok(n) => ExprKind::IntLit(n),
                    Err(()) => ExprKind::IntLit(0),
                };
                Some(Expr { kind, span: tok.span })
            }
            Tk::DoubleLit => {
                let text = self.cur_text.clone();
                self.bump();
                let v = text
                    .trim_end_matches(['u', 'U'])
                    .parse::<f64>()
                    .map_err(|_| {
                        self.diags.push(Diagnostic::new(
                            tok.span,
                            format!("invalid number literal `{text}`"),
                        ));
                    });
                let kind = match v {
                    Ok(d) => ExprKind::DoubleLit(d),
                    Err(()) => ExprKind::DoubleLit(0.0),
                };
                Some(Expr { kind, span: tok.span })
            }
            Tk::StringLit => {
                let body = self.cur_string_body.clone().unwrap_or_default();
                self.bump();
                Some(Expr {
                    kind: ExprKind::StringLit(unescape(&body)),
                    span: tok.span,
                })
            }
            Tk::True => {
                self.bump();
                Some(Expr {
                    kind: ExprKind::BoolLit(true),
                    span: tok.span,
                })
            }
            Tk::False => {
                self.bump();
                Some(Expr {
                    kind: ExprKind::BoolLit(false),
                    span: tok.span,
                })
            }
            Tk::Null => {
                self.bump();
                Some(Expr {
                    kind: ExprKind::Null,
                    span: tok.span,
                })
            }
            Tk::Ident => {
                let name = self.cur_text.clone();
                self.bump();
                // Function call?
                if self.cur.kind == Tk::LParen {
                    self.bump();
                    let args = self.parse_arg_list()?;
                    let rparen = self.expect(Tk::RParen, "`)`")?;
                    let span = tok.span.join(rparen.span);
                    Some(Expr {
                        kind: ExprKind::Call(name, args),
                        span,
                    })
                } else {
                    Some(Expr {
                        kind: ExprKind::Ident(name),
                        span: tok.span,
                    })
                }
            }
            Tk::LParen => {
                self.bump();
                let e = self.parse_expression()?;
                let rparen = self.expect(Tk::RParen, "`)`")?;
                let span = tok.span.join(rparen.span);
                Some(Expr { kind: e.kind, span })
            }
            Tk::LBrack => {
                self.bump();
                let mut items = Vec::new();
                if self.cur.kind != Tk::RBrack {
                    loop {
                        let item = self.parse_expression()?;
                        items.push(item);
                        if self.cur.kind == Tk::Comma {
                            self.bump();
                            if self.cur.kind == Tk::RBrack {
                                break;
                            }
                            continue;
                        }
                        break;
                    }
                }
                let rbrack = self.expect(Tk::RBrack, "`]`")?;
                let span = tok.span.join(rbrack.span);
                Some(Expr {
                    kind: ExprKind::List(items),
                    span,
                })
            }
            Tk::LBrace => {
                self.bump();
                let mut items = Vec::new();
                if self.cur.kind != Tk::RBrace {
                    loop {
                        let k = self.parse_expression()?;
                        self.expect(Tk::Colon, "`:` between map key and value")?;
                        let v = self.parse_expression()?;
                        items.push((k, v));
                        if self.cur.kind == Tk::Comma {
                            self.bump();
                            if self.cur.kind == Tk::RBrace {
                                break;
                            }
                            continue;
                        }
                        break;
                    }
                }
                let rbrace = self.expect(Tk::RBrace, "`}`")?;
                let span = tok.span.join(rbrace.span);
                Some(Expr {
                    kind: ExprKind::Map(items),
                    span,
                })
            }
            Tk::Minus | Tk::Bang => self.parse_unary(),
            Tk::Eof => {
                self.diags.push(Diagnostic::new(
                    tok.span,
                    "empty expression".to_string(),
                ));
                None
            }
            _ => {
                let lex = self.token_lexeme(&tok);
                self.diags.push(Diagnostic::new(
                    tok.span,
                    format!("unexpected `{lex}` in expression"),
                ));
                self.bump();
                None
            }
        }
    }

    fn parse_arg_list(&mut self) -> Option<Vec<Expr>> {
        let mut args = Vec::new();
        if self.cur.kind == Tk::RParen {
            return Some(args);
        }
        loop {
            let a = self.parse_expression()?;
            args.push(a);
            if self.cur.kind == Tk::Comma {
                self.bump();
                continue;
            }
            break;
        }
        Some(args)
    }
}

fn parse_int_literal(text: &str) -> Option<i64> {
    let t = text.trim_end_matches(['u', 'U']);
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<i64>().ok()
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some(other) => {
                // Preserve unknown escapes verbatim; CEL has more
                // escape forms (`\u...`, `\x...`, `\<octal>`) that we
                // leave for a later pass.
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Semantic walk: cross-reference `sources.<name>` against the config's
// collected source names, and surface unknown-identifier errors for a
// small, well-known set of bare roots.
// ---------------------------------------------------------------------------

/// Walk a parsed CEL expression and append diagnostics for any
/// `sources.<name>` whose `<name>` is not in `known_sources`. Other
/// identifier roots are left alone -- they may be component / action
/// params, `item` from a `repeat`, or `theme` tokens that a theme
/// adds freely.
/// The span of the first bare identifier `name` in `expr`, if any.
pub fn find_ident(expr: &Expr, name: &str) -> Option<Span> {
    match &expr.kind {
        ExprKind::Ident(n) if n == name => Some(expr.span),
        ExprKind::Ident(_)
        | ExprKind::IntLit(_)
        | ExprKind::DoubleLit(_)
        | ExprKind::StringLit(_)
        | ExprKind::BoolLit(_)
        | ExprKind::Null => None,
        ExprKind::Field(inner, _) | ExprKind::Unary(_, inner) => find_ident(inner, name),
        ExprKind::Index(a, b) | ExprKind::Binary(_, a, b) => {
            find_ident(a, name).or_else(|| find_ident(b, name))
        }
        ExprKind::Conditional(a, b, c) => find_ident(a, name)
            .or_else(|| find_ident(b, name))
            .or_else(|| find_ident(c, name)),
        ExprKind::Call(_, args) | ExprKind::List(args) => {
            args.iter().find_map(|a| find_ident(a, name))
        }
        ExprKind::Method(inner, _, args) => find_ident(inner, name)
            .or_else(|| args.iter().find_map(|a| find_ident(a, name))),
        ExprKind::Map(pairs) => pairs
            .iter()
            .find_map(|(k, v)| find_ident(k, name).or_else(|| find_ident(v, name))),
    }
}

pub fn walk_source_refs(
    expr: &Expr,
    known_sources: &std::collections::HashSet<String>,
    diags: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Field(inner, name) => {
            if let ExprKind::Ident(root) = &inner.kind {
                if root == "sources" && !known_sources.contains(name) {
                    diags.push(Diagnostic::new(
                        expr.span,
                        format!("unknown source `{name}` in `sources.{name}`"),
                    ));
                }
            }
            walk_source_refs(inner, known_sources, diags);
        }
        ExprKind::Method(inner, _, args) => {
            walk_source_refs(inner, known_sources, diags);
            for a in args {
                walk_source_refs(a, known_sources, diags);
            }
        }
        ExprKind::Index(inner, idx) => {
            // `sources["name"]` is a sometimes-used CEL idiom equivalent
            // to `sources.name`. Cross-ref the key when the base is the
            // literal `sources` identifier.
            if let (ExprKind::Ident(root), ExprKind::StringLit(name)) =
                (&inner.kind, &idx.kind)
            {
                if root == "sources" && !known_sources.contains(name) {
                    diags.push(Diagnostic::new(
                        expr.span,
                        format!("unknown source `{name}` in `sources[\"{name}\"]`"),
                    ));
                }
            }
            walk_source_refs(inner, known_sources, diags);
            walk_source_refs(idx, known_sources, diags);
        }
        ExprKind::Call(_, args) => {
            for a in args {
                walk_source_refs(a, known_sources, diags);
            }
        }
        ExprKind::Unary(_, inner) => walk_source_refs(inner, known_sources, diags),
        ExprKind::Binary(_, l, r) => {
            walk_source_refs(l, known_sources, diags);
            walk_source_refs(r, known_sources, diags);
        }
        ExprKind::Conditional(c, t, e) => {
            walk_source_refs(c, known_sources, diags);
            walk_source_refs(t, known_sources, diags);
            walk_source_refs(e, known_sources, diags);
        }
        ExprKind::List(items) => {
            for i in items {
                walk_source_refs(i, known_sources, diags);
            }
        }
        ExprKind::Map(items) => {
            for (k, v) in items {
                walk_source_refs(k, known_sources, diags);
                walk_source_refs(v, known_sources, diags);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Interp walker: extract each `{{ ... }}` from an interpolated raw
// value, and parse it. Used by the schema check so a value like
// `curl /{{ vm }}/stop` validates its one expression body.
// ---------------------------------------------------------------------------

/// Scan `raw` for `{{ ... }}` fragments, parse each one, and return
/// their ASTs plus any diagnostics. `base` is the byte offset of
/// `raw` in the enclosing source file. `\{{` is treated as an
/// escaped literal (same rule as the config lexer).
pub fn parse_interp(
    raw: &str,
    file: FileId,
    base: u32,
) -> (Vec<Expr>, Vec<Diagnostic>) {
    let mut out = Vec::new();
    let mut diags = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\\') {
            // Literal `\\` pair: two bytes, not an escape of the next
            // opener. A following `{{` would start an expression.
            i += 2;
            continue;
        }
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'{')
            && bytes.get(i + 2) == Some(&b'{')
        {
            // Escaped opener; skip it.
            i += 3;
            continue;
        }
        if bytes[i] == b'{' && bytes.get(i + 1) == Some(&b'{') {
            let expr_start = i + 2;
            let mut j = expr_start;
            let mut depth: u32 = 0;
            let mut closed = false;
            while j < bytes.len() {
                match bytes[j] {
                    b'"' | b'\'' => {
                        let q = bytes[j];
                        j += 1;
                        while j < bytes.len() {
                            if bytes[j] == b'\\' {
                                j += 2;
                                continue;
                            }
                            if bytes[j] == q {
                                j += 1;
                                break;
                            }
                            j += 1;
                        }
                    }
                    b'{' => {
                        depth = depth.saturating_add(1);
                        j += 1;
                    }
                    b'}' => {
                        if depth > 0 {
                            depth -= 1;
                            j += 1;
                        } else if bytes.get(j + 1) == Some(&b'}') {
                            // Found closing `}}`.
                            let body = &raw[expr_start..j];
                            let (e, mut ds) =
                                parse_fragment(body, file, base + expr_start as u32);
                            if let Some(e) = e {
                                out.push(e);
                            }
                            diags.append(&mut ds);
                            j += 2;
                            i = j;
                            closed = true;
                            break;
                        } else {
                            j += 1;
                        }
                    }
                    _ => j += 1,
                }
            }
            if !closed {
                // Reached end without closing `}}`. The outer config
                // lexer should have caught this; emit a soft error
                // anyway.
                let span = Span::new(
                    file,
                    base + i as u32,
                    base + bytes.len() as u32,
                );
                diags.push(Diagnostic::new(
                    span,
                    "unterminated `{{` expression".to_string(),
                ));
                break;
            }
        } else {
            i += 1;
        }
    }
    (out, diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceMap;
    use std::path::PathBuf;

    fn fid() -> FileId {
        let mut map = SourceMap::new();
        map.add(PathBuf::from("t"), String::new())
    }

    fn parse(s: &str) -> (Option<Expr>, Vec<Diagnostic>) {
        parse_fragment(s, fid(), 0)
    }

    #[test]
    fn parse_simple_arith() {
        let (e, d) = parse("1 + 2 * 3");
        assert!(d.is_empty(), "{d:?}");
        let e = e.unwrap();
        match &e.kind {
            ExprKind::Binary(BinaryOp::Add, _, _) => {}
            k => panic!("not add: {k:?}"),
        }
    }

    #[test]
    fn parse_field_chain() {
        let (e, d) = parse("sources.cpu_temp.value");
        assert!(d.is_empty(), "{d:?}");
        let e = e.unwrap();
        match &e.kind {
            ExprKind::Field(_, name) => assert_eq!(name, "value"),
            k => panic!("not field: {k:?}"),
        }
    }

    #[test]
    fn parse_ternary() {
        let (e, d) = parse("value > warn ? theme.warning : theme.text");
        assert!(d.is_empty(), "{d:?}");
        matches!(e.unwrap().kind, ExprKind::Conditional(_, _, _));
    }

    #[test]
    fn parse_call_and_method() {
        let (e, d) = parse("string(round(value, 1))");
        assert!(d.is_empty(), "{d:?}");
        matches!(e.unwrap().kind, ExprKind::Call(_, _));
    }

    #[test]
    fn parse_unary_neg() {
        let (e, d) = parse("-x");
        assert!(d.is_empty(), "{d:?}");
        matches!(e.unwrap().kind, ExprKind::Unary(UnaryOp::Neg, _));
    }

    #[test]
    fn parse_list_and_in() {
        let (e, d) = parse("x in [1, 2, 3]");
        assert!(d.is_empty(), "{d:?}");
        matches!(e.unwrap().kind, ExprKind::Binary(BinaryOp::In, _, _));
    }

    #[test]
    fn parse_map() {
        let (e, d) = parse("{\"a\": 1, \"b\": 2}");
        assert!(d.is_empty(), "{d:?}");
        matches!(e.unwrap().kind, ExprKind::Map(_));
    }

    #[test]
    fn parse_double_with_e() {
        let (e, d) = parse("1e9");
        assert!(d.is_empty(), "{d:?}");
        match e.unwrap().kind {
            ExprKind::DoubleLit(v) => assert!((v - 1e9).abs() < 1e-6),
            k => panic!("not double: {k:?}"),
        }
    }

    #[test]
    fn parse_hex_int() {
        let (e, d) = parse("0x2a");
        assert!(d.is_empty(), "{d:?}");
        match e.unwrap().kind {
            ExprKind::IntLit(v) => assert_eq!(v, 42),
            k => panic!("not int: {k:?}"),
        }
    }

    #[test]
    fn parse_string_literal() {
        let (e, d) = parse(r#""hi\nworld""#);
        assert!(d.is_empty(), "{d:?}");
        match e.unwrap().kind {
            ExprKind::StringLit(s) => assert_eq!(s, "hi\nworld"),
            k => panic!("not str: {k:?}"),
        }
    }

    #[test]
    fn missing_close_paren_errors() {
        let (_, d) = parse("f(1, 2");
        assert!(!d.is_empty());
    }

    #[test]
    fn trailing_garbage_errors() {
        let (_, d) = parse("1 + 2 banana");
        assert!(!d.is_empty());
    }

    #[test]
    fn unknown_source_ref_flagged() {
        let (e, _) = parse("sources.ghost.value");
        let e = e.unwrap();
        let mut known = std::collections::HashSet::new();
        known.insert("cpu_temp".to_string());
        let mut diags = Vec::new();
        walk_source_refs(&e, &known, &mut diags);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("unknown source `ghost`"));
    }

    #[test]
    fn known_source_ref_not_flagged() {
        let (e, _) = parse("sources.cpu_temp.value");
        let e = e.unwrap();
        let mut known = std::collections::HashSet::new();
        known.insert("cpu_temp".to_string());
        let mut diags = Vec::new();
        walk_source_refs(&e, &known, &mut diags);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn interp_parses_each_fragment() {
        let (exprs, d) = parse_interp("CPU {{ x }} temp {{ y + 1 }}", fid(), 0);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(exprs.len(), 2);
    }

    #[test]
    fn interp_skips_escaped_opener() {
        let (exprs, d) = parse_interp(r"\{{ x }} {{ y }}", fid(), 0);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(exprs.len(), 1);
    }
}
