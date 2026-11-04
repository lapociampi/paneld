//! AST and recursive-descent parser.
//!
//! The parser consumes the lexer's tokens and produces a [`Document`].
//! It is structural: it recognizes assignments, blocks (named or not),
//! and `repeat` loops, but does not know any item kind (`source`,
//! `page`, `component`, ...). The schema check pass (Phase 4) is where
//! those meanings are attached.
//!
//! Error recovery: on an unexpected token the parser emits a
//! diagnostic and skips forward to the next separator (`;`, newline,
//! or `}` at the current nesting level), so `paneld check` can report
//! many errors in one run (ADR 0009).

use crate::lex::LexError;
use crate::{SourceMap, Span, Token, TokenKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    pub fn new(span: Span, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

impl From<LexError> for Diagnostic {
    fn from(e: LexError) -> Self {
        Diagnostic {
            span: e.span,
            message: e.message,
        }
    }
}

/// A parsed document: the items of one file (or the merged items of
/// a file plus its includes once the loader has run).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Assign(Assign),
    Block(Block),
    Repeat(Repeat),
    Param(Param),
    Var(VarDecl),
}

impl Item {
    pub fn span(&self) -> Span {
        match self {
            Item::Assign(a) => a.span,
            Item::Block(b) => b.span,
            Item::Repeat(r) => r.span,
            Item::Param(p) => p.span,
            Item::Var(v) => v.span,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub text: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawValue {
    /// Text of the value as it appeared in the file, trailing spaces
    /// trimmed. Interpretation (number, duration, color, string,
    /// list, expression) happens in a later pass.
    pub text: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expr {
    /// The expression body between `{{` and `}}`, with the delimiters
    /// stripped. CEL parsing is a later phase.
    pub body: String,
    /// The full span including the `{{` and `}}` delimiters.
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assign {
    pub target: AssignTarget,
    pub value: RawValue,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssignTarget {
    /// `key = value`
    Simple { key: Ident },
    /// `$name = value`
    Var { name: Ident },
    /// `category:key = value`
    Qualified { category: Ident, key: Ident },
    /// `kind[name]:key = value`
    Indexed {
        kind: Ident,
        name: Ident,
        key: Ident,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: Ident,
    /// For `kind name { ... }` the name is set; for a plain `kind { ... }`
    /// it is `None`. Also used for `on_tap action { arg = ... }` style
    /// calls inside UI nodes.
    pub name: Option<Ident>,
    /// `remote device1: linux { }`: the kind after the `:` (schema 17).
    pub of: Option<Ident>,
    pub items: Vec<Item>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repeat {
    pub binding: Ident,
    pub source: Expr,
    pub items: Vec<Item>,
    pub span: Span,
}

/// `param <name>: <type> [= <default>] [{ <options> }]`.
///
/// Used inside `action` and `component` bodies. The spec marks `param`
/// as a repeatable keyword with this specific grammar — different from
/// an ordinary assignment or block, since the type sits after a `:` and
/// a default and an options block are both optional.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: Ident,
    pub ty: Ident,
    pub default: Option<RawValue>,
    pub options: Option<OptionsBlock>,
    pub span: Span,
}

/// The `{ ... }` options block of a `param`. Items inside are the same
/// shape as any other block body (`ask = select`, `options = [5, 30]`,
/// ...). Kept separate from [`Block`] so Phase 4's kind-dispatched
/// table of `Block.kind` is not polluted with a synthetic `"param"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsBlock {
    pub items: Vec<Item>,
    /// Span covering just `{...}` (not the preceding `param name: type`).
    pub span: Span,
}

/// `var <name>: <type> = <default>` at the top level.
///
/// Section 3 of `docs/schema.md`: UI state actions can `set`, typed so
/// expressions check at load. The default is mandatory in the spec.
/// Semantically distinct from [`Param`] (which lives inside action and
/// component bodies and may have an options block), so it gets its own
/// AST node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VarDecl {
    pub name: Ident,
    pub ty: Ident,
    pub default: RawValue,
    pub span: Span,
}

/// Parse a token slice into a [`Document`] plus a list of diagnostics.
pub fn parse(tokens: &[Token], source: &SourceMap) -> (Document, Vec<Diagnostic>) {
    let mut p = Parser::new(tokens, source);
    let items = p.parse_items(true);
    (Document { items }, p.diagnostics)
}

struct Parser<'t> {
    tokens: &'t [Token],
    pos: usize,
    source: &'t SourceMap,
    diagnostics: Vec<Diagnostic>,
}

impl<'t> Parser<'t> {
    fn new(tokens: &'t [Token], source: &'t SourceMap) -> Self {
        Self {
            tokens,
            pos: 0,
            source,
            diagnostics: Vec::new(),
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos];
        if t.kind != TokenKind::Eof {
            self.pos += 1;
        }
        t
    }

    fn slice(&self, span: Span) -> &str {
        self.source.slice(span)
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(span, message));
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek().kind, TokenKind::Semi | TokenKind::Newline) {
            self.pos += 1;
        }
    }

    /// Parse items until Eof (top level) or an unbalanced `}` (block body).
    fn parse_items(&mut self, at_top: bool) -> Vec<Item> {
        let mut items = Vec::new();
        loop {
            self.skip_separators();
            match self.peek().kind {
                TokenKind::Eof => break,
                TokenKind::RBrace if !at_top => break,
                _ => {}
            }
            match self.parse_item() {
                Some(it) => items.push(it),
                None => self.recover_to_separator(at_top),
            }
        }
        items
    }

    fn parse_item(&mut self) -> Option<Item> {
        let tok = *self.peek();
        match tok.kind {
            TokenKind::DollarIdent => Some(Item::Assign(self.parse_var_def()?)),
            TokenKind::Ident => {
                let text = self.slice(tok.span);
                match text {
                    "repeat" => Some(Item::Repeat(self.parse_repeat()?)),
                    "param" => Some(Item::Param(self.parse_param()?)),
                    "var" => Some(Item::Var(self.parse_var_decl()?)),
                    _ => self.parse_ident_item(),
                }
            }
            _ => {
                self.error(
                    tok.span,
                    format!(
                        "expected an identifier at the start of an item, got {}",
                        describe(tok.kind)
                    ),
                );
                None
            }
        }
    }

    /// `param <name>: <type> [= <default>] [{ <options> }]`.
    fn parse_param(&mut self) -> Option<Param> {
        let param_kw = self.bump(); // the `param` ident
        let name = self.expect_ident("expected a parameter name after `param`")?;
        self.expect(TokenKind::Colon, "expected `:` after the parameter name")?;
        let ty = self.expect_ident("expected a type name after `:`")?;

        let mut end_span = ty.span;
        let mut default = None;
        let mut options = None;

        if self.peek().kind == TokenKind::Equals {
            let eq = self.bump();
            let v = self.expect_raw_value(eq.span)?;
            end_span = v.span;
            default = Some(v);
        }
        if self.peek().kind == TokenKind::LBrace {
            let opts = self.parse_options_block()?;
            end_span = opts.span;
            options = Some(opts);
        }

        Some(Param {
            name,
            ty,
            default,
            options,
            span: param_kw.span.join(end_span),
        })
    }

    /// `var <name>: <type> = <default>`.
    fn parse_var_decl(&mut self) -> Option<VarDecl> {
        let var_kw = self.bump();
        let name = self.expect_ident("expected a variable name after `var`")?;
        self.expect(TokenKind::Colon, "expected `:` after the variable name")?;
        let ty = self.expect_ident("expected a type name after `:`")?;
        let eq = self.expect(
            TokenKind::Equals,
            "expected `=` and a default value for the `var` declaration",
        )?;
        let default = self.expect_raw_value(eq.span)?;
        let span = var_kw.span.join(default.span);
        Some(VarDecl {
            name,
            ty,
            default,
            span,
        })
    }

    /// Parse a `{ items }` body as an [`OptionsBlock`]. Caller has
    /// already verified the next token is `{`.
    fn parse_options_block(&mut self) -> Option<OptionsBlock> {
        let lbrace = self.expect(TokenKind::LBrace, "expected `{`")?;
        let items = self.parse_items(false);
        let rbrace = self.expect(TokenKind::RBrace, "expected `}` to close block")?;
        Some(OptionsBlock {
            items,
            span: lbrace.span.join(rbrace.span),
        })
    }

    /// After a leading `$name` token: expect `= value`.
    fn parse_var_def(&mut self) -> Option<Assign> {
        let dollar = self.bump();
        let name_text = &self.slice(dollar.span)[1..]; // strip the `$`
        let name_span = Span::new(dollar.span.file, dollar.span.start + 1, dollar.span.end);
        let name = Ident {
            text: name_text.to_string(),
            span: name_span,
        };
        let eq = self.expect(TokenKind::Equals, "expected `=` after `$name`")?;
        let value = self.expect_raw_value(eq.span)?;
        let span = dollar.span.join(value.span);
        Some(Assign {
            target: AssignTarget::Var { name },
            value,
            span,
        })
    }

    /// After a leading Ident: branch on what comes next.
    fn parse_ident_item(&mut self) -> Option<Item> {
        let first = self.bump();
        let first_ident = Ident {
            text: self.slice(first.span).to_string(),
            span: first.span,
        };

        match self.peek().kind {
            TokenKind::Equals => {
                let eq = self.bump();
                let value = self.expect_raw_value(eq.span)?;
                let span = first.span.join(value.span);
                Some(Item::Assign(Assign {
                    target: AssignTarget::Simple { key: first_ident },
                    value,
                    span,
                }))
            }
            TokenKind::Colon => {
                self.bump();
                let key = self.expect_ident("expected a key name after `:`")?;
                let eq = self.expect(TokenKind::Equals, "expected `=` after `category:key`")?;
                let value = self.expect_raw_value(eq.span)?;
                let span = first.span.join(value.span);
                Some(Item::Assign(Assign {
                    target: AssignTarget::Qualified {
                        category: first_ident,
                        key,
                    },
                    value,
                    span,
                }))
            }
            TokenKind::LBracket => {
                self.bump();
                let name = self.expect_ident("expected a name inside `[ ]`")?;
                self.expect(TokenKind::RBracket, "expected `]`")?;
                self.expect(TokenKind::Colon, "expected `:` after `[name]`")?;
                let key = self.expect_ident("expected a key name after `:`")?;
                let eq = self.expect(TokenKind::Equals, "expected `=` after `kind[name]:key`")?;
                let value = self.expect_raw_value(eq.span)?;
                let span = first.span.join(value.span);
                Some(Item::Assign(Assign {
                    target: AssignTarget::Indexed {
                        kind: first_ident,
                        name,
                        key,
                    },
                    value,
                    span,
                }))
            }
            TokenKind::LBrace => {
                let block = self.parse_block(first_ident, None)?;
                Some(Item::Block(block))
            }
            TokenKind::Ident => {
                // `kind name { ... }` named block.
                let name_tok = self.bump();
                let name = Ident {
                    text: self.slice(name_tok.span).to_string(),
                    span: name_tok.span,
                };
                // `kind name: of { ... }` (a remote instance).
                let of = if self.peek().kind == TokenKind::Colon {
                    self.bump();
                    Some(self.expect_ident("expected a kind name after `:`")?)
                } else {
                    None
                };
                let mut block = self.parse_block(first_ident, Some(name))?;
                block.of = of;
                Some(Item::Block(block))
            }
            TokenKind::Selector => {
                // `style <selector> { ... }` theme sub-block. The
                // selector's raw text comes from the Selector token; it
                // is used as the block's name, and the schema check
                // validates its shape.
                let name_tok = self.bump();
                let name = Ident {
                    text: self.slice(name_tok.span).to_string(),
                    span: name_tok.span,
                };
                let block = self.parse_block(first_ident, Some(name))?;
                Some(Item::Block(block))
            }
            TokenKind::DollarIdent => {
                // Allow `kind $name { ... }`? Not in the spec; reject.
                let bad = self.peek().span;
                self.error(
                    bad,
                    "an item name cannot be a `$variable`; use a bare identifier",
                );
                None
            }
            _ => {
                let bad = *self.peek();
                self.error(
                    bad.span,
                    format!(
                        "expected `=`, `:`, `[`, `{{` or a name after `{}`, got {}",
                        first_ident.text,
                        describe(bad.kind)
                    ),
                );
                None
            }
        }
    }

    fn parse_block(&mut self, kind: Ident, name: Option<Ident>) -> Option<Block> {
        self.expect(TokenKind::LBrace, "expected `{`")?;
        let items = self.parse_items(false);
        let rbrace = self.expect(TokenKind::RBrace, "expected `}` to close block")?;
        let span = kind.span.join(rbrace.span);
        Some(Block {
            kind,
            name,
            of: None,
            items,
            span,
        })
    }

    /// `repeat <binding> in <expr> { items }`.
    fn parse_repeat(&mut self) -> Option<Repeat> {
        let repeat_kw = self.bump(); // the `repeat` ident
        let binding = self.expect_ident("expected a binding name after `repeat`")?;
        let in_tok = self.expect_ident("expected `in` after the repeat binding")?;
        if in_tok.text != "in" {
            self.error(in_tok.span, "expected `in` after the repeat binding");
            return None;
        }
        let expr_tok = *self.peek();
        if expr_tok.kind != TokenKind::Expression {
            self.error(
                expr_tok.span,
                "expected a `{{ ... }}` expression after `in`",
            );
            return None;
        }
        self.bump();
        let body = expr_body(self.slice(expr_tok.span));
        let source = Expr {
            body,
            span: expr_tok.span,
        };
        self.expect(TokenKind::LBrace, "expected `{` after the repeat expression")?;
        let items = self.parse_items(false);
        let rbrace = self.expect(TokenKind::RBrace, "expected `}` to close `repeat`")?;
        let span = repeat_kw.span.join(rbrace.span);
        Some(Repeat {
            binding,
            source,
            items,
            span,
        })
    }

    fn expect(&mut self, kind: TokenKind, message: &str) -> Option<Token> {
        let tok = *self.peek();
        if tok.kind == kind {
            self.pos += 1;
            Some(tok)
        } else {
            self.error(
                tok.span,
                format!("{message}; got {}", describe(tok.kind)),
            );
            None
        }
    }

    fn expect_ident(&mut self, message: &str) -> Option<Ident> {
        let tok = *self.peek();
        if tok.kind == TokenKind::Ident {
            self.pos += 1;
            Some(Ident {
                text: self.slice(tok.span).to_string(),
                span: tok.span,
            })
        } else {
            self.error(
                tok.span,
                format!("{message}; got {}", describe(tok.kind)),
            );
            None
        }
    }

    fn expect_raw_value(&mut self, after_eq: Span) -> Option<RawValue> {
        let tok = *self.peek();
        if tok.kind == TokenKind::RawValue {
            self.pos += 1;
            Some(RawValue {
                text: self.slice(tok.span).to_string(),
                span: tok.span,
            })
        } else {
            // In practice the lexer always emits a RawValue after an Equals.
            // Defensive: synthesize an empty value with a span at the `=`.
            self.error(after_eq, "expected a value after `=`");
            Some(RawValue {
                text: String::new(),
                span: Span::new(after_eq.file, after_eq.end, after_eq.end),
            })
        }
    }

    /// Skip tokens until the next record-separator (`;`, newline) or
    /// closing brace at the current depth. Used to resync after a
    /// parse error so one run reports as many problems as it can.
    fn recover_to_separator(&mut self, at_top: bool) {
        let mut depth: i32 = 0;
        loop {
            let k = self.peek().kind;
            match k {
                TokenKind::Eof => return,
                TokenKind::LBrace => {
                    depth += 1;
                    self.pos += 1;
                }
                TokenKind::RBrace => {
                    if depth == 0 {
                        if at_top {
                            // Stray `}` at top level: consume so we do
                            // not loop forever.
                            self.pos += 1;
                        }
                        return;
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                TokenKind::Semi | TokenKind::Newline if depth == 0 => {
                    self.pos += 1;
                    return;
                }
                _ => {
                    self.pos += 1;
                }
            }
        }
    }
}

/// Strip the leading `{{` and trailing `}}` from an expression's raw
/// slice and trim the ends. Caller guarantees the slice starts with
/// `{{` and ends with `}}`.
fn expr_body(raw: &str) -> String {
    let inner = raw
        .strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
        .unwrap_or(raw);
    inner.trim().to_string()
}

fn describe(k: TokenKind) -> &'static str {
    match k {
        TokenKind::Ident => "an identifier",
        TokenKind::DollarIdent => "a `$name`",
        TokenKind::Equals => "`=`",
        TokenKind::Colon => "`:`",
        TokenKind::Semi => "`;`",
        TokenKind::LBrace => "`{`",
        TokenKind::RBrace => "`}`",
        TokenKind::LBracket => "`[`",
        TokenKind::RBracket => "`]`",
        TokenKind::Newline => "a newline",
        TokenKind::RawValue => "a value",
        TokenKind::Expression => "a `{{ ... }}` expression",
        TokenKind::Selector => "a style selector",
        TokenKind::Eof => "end of input",
    }
}

/// Keys that make a nameless `on_tap { }` / `on_hold { }` an action
/// of its own (schema 5, "Calling an action", inline form) rather
/// than a plain `goto` / `set` shortcut.
pub const INLINE_ACTION_ONLY_KEYS: &[&str] = &["run", "pin", "confirm", "then", "else", "refresh"];

impl Block {
    /// `true` for a nameless handler block that must run as an action
    /// (and so through the PIN / confirm gates).
    pub fn is_inline_action(&self) -> bool {
        self.name.is_none()
            && self.items.iter().any(|it| {
                matches!(it, Item::Assign(a)
                    if matches!(&a.target, AssignTarget::Simple { key }
                        if INLINE_ACTION_ONLY_KEYS.contains(&key.text.as_str())))
            })
    }

    /// The action name an inline handler block runs under. Unique per
    /// block position; the `@` keeps it out of the config's own
    /// identifier space.
    pub fn inline_action_name(&self) -> String {
        format!("@inline:{}:{}", self.span.file.0, self.span.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tokenize;
    use std::path::PathBuf;

    fn parse_str(src: &str) -> (Document, Vec<Diagnostic>, SourceMap) {
        let mut map = SourceMap::new();
        let id = map.add(PathBuf::from("t.conf"), src.to_string());
        let (toks, lex_errs) = tokenize(id, src);
        assert!(lex_errs.is_empty(), "unexpected lex errors: {lex_errs:?}");
        let (doc, diags) = parse(&toks, &map);
        (doc, diags, map)
    }

    fn parse_ok(src: &str) -> Document {
        let (doc, diags, _) = parse_str(src);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        doc
    }

    #[test]
    fn simple_assignment_item() {
        let doc = parse_ok("include = foo.conf\n");
        assert_eq!(doc.items.len(), 1);
        let Item::Assign(a) = &doc.items[0] else {
            panic!("expected Assign, got {:?}", doc.items[0])
        };
        match &a.target {
            AssignTarget::Simple { key } => assert_eq!(key.text, "include"),
            other => panic!("expected Simple target, got {other:?}"),
        }
        assert_eq!(a.value.text, "foo.conf");
    }

    #[test]
    fn var_def() {
        let doc = parse_ok("$pve = https://host:8006\n");
        let Item::Assign(a) = &doc.items[0] else {
            panic!("expected Assign")
        };
        let AssignTarget::Var { name } = &a.target else {
            panic!("expected Var target, got {:?}", a.target)
        };
        assert_eq!(name.text, "pve");
        assert_eq!(a.value.text, "https://host:8006");
    }

    #[test]
    fn qualified_assignment() {
        let doc = parse_ok("screen:idle = 2m\n");
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        let AssignTarget::Qualified { category, key } = &a.target else {
            panic!("got {:?}", a.target)
        };
        assert_eq!(category.text, "screen");
        assert_eq!(key.text, "idle");
        assert_eq!(a.value.text, "2m");
    }

    #[test]
    fn indexed_assignment() {
        let doc = parse_ok("theme[dark]:accent = #44aaff\n");
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        let AssignTarget::Indexed { kind, name, key } = &a.target else {
            panic!("got {:?}", a.target)
        };
        assert_eq!(kind.text, "theme");
        assert_eq!(name.text, "dark");
        assert_eq!(key.text, "accent");
        assert_eq!(a.value.text, "#44aaff");
    }

    #[test]
    fn unnamed_block_with_assigns() {
        let doc = parse_ok("screen { idle = 2m\nbrightness = 80\n}");
        let Item::Block(b) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(b.kind.text, "screen");
        assert!(b.name.is_none());
        assert_eq!(b.items.len(), 2);
    }

    #[test]
    fn named_block_at_top_level() {
        let doc = parse_ok(
            "source cpu_temp {\n    poll = 30s\n    run = cat /sys/x\n}",
        );
        let Item::Block(b) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(b.kind.text, "source");
        assert_eq!(b.name.as_ref().unwrap().text, "cpu_temp");
        assert_eq!(b.items.len(), 2);
    }

    #[test]
    fn nested_blocks() {
        let doc = parse_ok(
            "page home {\n    grid {\n        columns = 3\n    }\n}",
        );
        let Item::Block(outer) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(outer.kind.text, "page");
        assert_eq!(outer.name.as_ref().unwrap().text, "home");
        let Item::Block(inner) = &outer.items[0] else {
            panic!()
        };
        assert_eq!(inner.kind.text, "grid");
    }

    #[test]
    fn semicolon_separates_assignments() {
        let doc = parse_ok("text { value = Hi; color = red }");
        let Item::Block(b) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(b.items.len(), 2);
    }

    #[test]
    fn repeat_block() {
        let doc = parse_ok("page x {\nrepeat it in {{ list }} {\n  text { value = Y }\n}\n}");
        let Item::Block(page) = &doc.items[0] else {
            panic!()
        };
        let Item::Repeat(r) = &page.items[0] else {
            panic!()
        };
        assert_eq!(r.binding.text, "it");
        assert_eq!(r.source.body, "list");
        assert_eq!(r.items.len(), 1);
    }

    #[test]
    fn empty_block() {
        let doc = parse_ok("screen { }");
        let Item::Block(b) = &doc.items[0] else {
            panic!()
        };
        assert!(b.items.is_empty());
    }

    #[test]
    fn empty_value_allowed() {
        let doc = parse_ok("k =\n");
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(a.value.text, "");
    }

    #[test]
    fn value_with_embedded_expression() {
        let doc = parse_ok("label = CPU {{ x }} C\n");
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(a.value.text, "CPU {{ x }} C");
    }

    #[test]
    fn multiple_items_at_top() {
        let doc = parse_ok("include = a\ninclude = b\n$pve = foo\n");
        assert_eq!(doc.items.len(), 3);
    }

    #[test]
    fn error_recovery_continues_at_next_record() {
        // The first line has no `=` after the ident, which is an error.
        // The parser should still produce the second line's item.
        let (doc, diags, _) = parse_str("foo oops bar\ninclude = good\n");
        // The `foo oops` + an ident `bar` could parse as a named block header
        // but there is no `{` so it is an error.
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        // The good line still gets parsed.
        let has_good = doc.items.iter().any(|it| matches!(
            it,
            Item::Assign(a) if matches!(&a.target, AssignTarget::Simple { key } if key.text == "include")
                && a.value.text == "good"
        ));
        assert!(has_good, "parser did not recover; items: {:?}", doc.items);
    }

    #[test]
    fn missing_closing_brace_errors_at_eof() {
        let (_, diags, _) = parse_str("screen {\nidle = 2m\n");
        assert!(!diags.is_empty());
        assert!(
            diags.iter().any(|d| d.message.contains("expected `}`")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn repeat_without_expression_errors() {
        let (_, diags, _) = parse_str("repeat x in nope { }");
        assert!(diags.iter().any(|d| d.message.contains("expression")));
    }

    #[test]
    fn item_spans_cover_the_whole_item() {
        let src = "include = foo.conf\n";
        let (doc, _, map) = parse_str(src);
        let Item::Assign(a) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(map.slice(a.span), "include = foo.conf");
    }

    #[test]
    fn param_simple() {
        let doc = parse_ok("param vm: string\n");
        let Item::Param(p) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(p.name.text, "vm");
        assert_eq!(p.ty.text, "string");
        assert!(p.default.is_none());
        assert!(p.options.is_none());
    }

    #[test]
    fn param_with_default() {
        let doc = parse_ok("param warn: number = 1e9\n");
        let Item::Param(p) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(p.ty.text, "number");
        assert_eq!(p.default.as_ref().unwrap().text, "1e9");
    }

    #[test]
    fn param_with_options_block() {
        let doc = parse_ok(
            "param minutes: int { ask = select; options = [5, 30]; label = Minutes }",
        );
        let Item::Param(p) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(p.ty.text, "int");
        let opts = p.options.as_ref().unwrap();
        assert_eq!(opts.items.len(), 3);
    }

    #[test]
    fn param_with_default_and_options() {
        let doc = parse_ok("param mode: string = fast { options = [fast, slow] }\nx = 1\n");
        let Item::Param(p) = &doc.items[0] else { panic!() };
        assert_eq!(p.default.as_ref().unwrap().text, "fast");
        assert_eq!(p.options.as_ref().unwrap().items.len(), 1);
        assert_eq!(doc.items.len(), 2);
        // Review round 9: a default that starts with a brace.
        let doc = parse_ok("param body: string = {\"k\": 1}\n");
        let Item::Param(p) = &doc.items[0] else { panic!() };
        assert_eq!(p.default.as_ref().unwrap().text, "{\"k\": 1}");
        assert!(p.options.is_none());
    }

    #[test]
    fn param_options_span_covers_only_the_braces() {
        let src = "param minutes: int { ask = select }\n";
        let (doc, _, map) = parse_str(src);
        let Item::Param(p) = &doc.items[0] else {
            panic!()
        };
        let opts = p.options.as_ref().unwrap();
        assert_eq!(map.slice(opts.span), "{ ask = select }");
    }

    #[test]
    fn var_top_level_declaration() {
        let doc = parse_ok("var vm: string = \"\"\n");
        let Item::Var(v) = &doc.items[0] else {
            panic!("expected Var, got {:?}", doc.items[0])
        };
        assert_eq!(v.name.text, "vm");
        assert_eq!(v.ty.text, "string");
        assert_eq!(v.default.text, "\"\"");
    }

    #[test]
    fn var_int_default() {
        let doc = parse_ok("var tab: int = 0\n");
        let Item::Var(v) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(v.ty.text, "int");
        assert_eq!(v.default.text, "0");
    }

    #[test]
    fn var_without_default_errors() {
        let (_, diags, _) = parse_str("var tab: int\n");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("expected `=`") || d.message.contains("default")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn block_span_covers_from_kind_to_closing_brace() {
        let src = "page home {\n}\n";
        let (doc, _, map) = parse_str(src);
        let Item::Block(b) = &doc.items[0] else {
            panic!()
        };
        assert_eq!(map.slice(b.span), "page home {\n}");
    }
}
