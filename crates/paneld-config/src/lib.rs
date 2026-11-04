//! Config parser and schema check for paneld.
//!
//! The config format is documented in `docs/config-format.md` and
//! `docs/schema.md`. The parser is hand-written (ADR 0009): a lexer
//! producing tokens with spans, then a recursive-descent parser.

pub mod cel;
pub mod lex;
pub mod library;
pub mod load;
pub mod overrides;
pub mod parse;
pub mod remote;
pub mod schema;
pub mod source;
pub mod value;

pub use lex::{LexError, Lexer, Token, TokenKind};
pub use load::{load, LoadError};
pub use overrides::apply_overrides;
pub use parse::{
    parse, Assign, AssignTarget, Block, Diagnostic, Document, Expr, Ident, Item, OptionsBlock,
    Param, RawValue, Repeat, VarDecl,
};
pub use schema::check;
pub use source::{FileId, Location, SourceMap, Span};
