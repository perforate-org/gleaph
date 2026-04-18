//! Token type definitions for the GQL lexer.
//!
//! Tokens are the output of [`crate::lexer::tokenize`] and the input to the parser.
//! Keywords are not pre-classified; they appear as [`Token::Ident`] and the parser
//! matches them case-insensitively.

/// A position in source text (byte offset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the token start.
    pub start: usize,
    /// Byte offset past the last byte of the token.
    pub end: usize,
}

impl Span {
    /// A zero-width dummy span for use in tests and synthetic AST nodes.
    pub const DUMMY: Span = Span { start: 0, end: 0 };

    /// Merges two spans into the smallest span that covers both.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

impl Default for Span {
    fn default() -> Self {
        Span::DUMMY
    }
}

/// A token with its source span.
#[derive(Clone, Debug, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}

// ─────────────────────────────────────────────────────────────────────────────
// Comments
// ─────────────────────────────────────────────────────────────────────────────

/// A source comment preserved from lexical analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    /// Location of the comment in the source text (includes delimiters).
    pub span: Span,
    /// Whether this is a line comment or a block comment.
    pub kind: CommentKind,
    /// The comment body text with delimiters stripped
    /// (`//`/`--` prefix or `/* */` wrapper removed).
    pub text: String,
}

/// The kind of source comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    /// A line comment introduced by `//` or `--`.
    Line,
    /// A block comment delimited by `/* ... */`.
    Block,
}

/// A single lexical token produced by the GQL lexer.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    // ── Identifiers & literals ───────────────────────────────────────────
    /// An unquoted identifier or keyword (e.g. `MATCH`, `User`, `a`).
    /// Keywords are matched case-insensitively by the parser.
    Ident(String),

    /// A delimited identifier: double-quoted (`"my col"`) or backtick-quoted
    /// (`` `my col` ``). Never treated as a keyword.
    QuotedIdent(String),

    /// A single-quoted string literal with escape sequences resolved.
    StringLit(String),

    /// A binary literal: `X'4142'` → raw bytes.
    BytesLit(Vec<u8>),

    /// A signed 64-bit integer literal (decimal, hex, octal, or binary).
    Int(i64),

    /// An integer literal too large for i64 (stored as raw digit string for
    /// the parser to promote to Int128/Int256/Uint256 etc.).
    BigInt(String),

    /// A 64-bit floating-point literal. Includes values with fraction/exponent
    /// and no suffix, or with `F`/`D` (approximate) suffix.
    Float(f64),

    /// An exact numeric literal with `M` suffix (stored as the raw decimal
    /// string without the suffix, for precise `Decimal` construction).
    ExactNumeric(String),

    /// A query parameter reference: `$name`.
    Param(String),

    /// A substituted parameter reference: `$$name`.
    SubstitutedParam(String),

    // ── Single-character punctuation ─────────────────────────────────────
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]` — only emitted when not part of a multi-char token like `]->`.
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `:`
    Colon,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `*`
    Star,
    /// `-` — only when not part of `->`, `-[`, `-/`, `<->`, etc.
    Minus,
    /// `+`
    Plus,
    /// `/` — only when not part of `/-`, `/->`, `/~`, `/~>`.
    Slash,
    /// `%`
    Percent,
    /// `|` — only when not part of `||` or `|+|`.
    Pipe,
    /// `&`
    Ampersand,
    /// `!`
    Bang,
    /// `~` — only when not part of `~>`, `~[`, `~/`.
    Tilde,
    /// `?`
    Question,
    /// `@`
    At,
    /// `=` — only when not part of `=>`.
    Eq,

    // ── Comparison operators ─────────────────────────────────────────────
    /// `<` — only when not part of `<-`, `<~`, `<=`, `<>`, `<->`, etc.
    Lt,
    /// `>`
    Gt,
    /// `<>`
    Ne,
    /// `<=`
    Le,
    /// `>=`
    Ge,

    // ── Multi-character operators ────────────────────────────────────────
    /// `..`
    RangeDots,
    /// `::`
    DoubleColon,
    /// `=>`
    RightDoubleArrow,
    /// `||`
    Concat,
    /// `|+|`
    MultisetAlt,

    // ── Arrow / edge tokens ──────────────────────────────────────────────
    /// `->`
    RightArrow,
    /// `<-`
    LeftArrow,
    /// `<->`
    LeftMinusRight,
    /// `~>`
    TildeRightArrow,
    /// `<~`
    LeftArrowTilde,

    // ── Bracket edge tokens ──────────────────────────────────────────────
    /// `-[`
    MinusLeftBracket,
    /// `]->`
    BracketRightArrow,
    /// `]-`
    RightBracketMinus,
    /// `~[`
    TildeLeftBracket,
    /// `]~>`
    BracketTildeRightArrow,
    /// `]~`
    RightBracketTilde,
    /// `<-[`
    LeftArrowBracket,
    /// `<~[`
    LeftArrowTildeBracket,

    // ── Simplified path tokens ───────────────────────────────────────────
    /// `-/`
    MinusSlash,
    /// `/-`
    SlashMinus,
    /// `/->`
    SlashMinusRight,
    /// `~/`
    TildeSlash,
    /// `/~`
    SlashTilde,
    /// `/~>`
    SlashTildeRight,
    /// `<-/`
    LeftMinusSlash,
    /// `<~/`
    LeftTildeSlash,
}

impl Token {
    /// Returns `true` if this token is an identifier (quoted or unquoted).
    pub fn is_ident(&self) -> bool {
        matches!(self, Token::Ident(_) | Token::QuotedIdent(_))
    }

    /// If this token is an [`Ident`](Token::Ident), checks whether it matches
    /// the given keyword (case-insensitive). Returns `false` for all other
    /// token variants.
    pub fn is_keyword(&self, kw: &str) -> bool {
        match self {
            Token::Ident(s) => s.eq_ignore_ascii_case(kw),
            _ => false,
        }
    }

    /// Returns the identifier string if this is an `Ident` or `QuotedIdent`.
    pub fn as_ident_str(&self) -> Option<&str> {
        match self {
            Token::Ident(s) | Token::QuotedIdent(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_ident_unquoted() {
        assert!(Token::Ident("foo".into()).is_ident());
    }
    #[test]
    fn is_ident_quoted() {
        assert!(Token::QuotedIdent("bar".into()).is_ident());
    }
    #[test]
    fn is_ident_non_ident() {
        assert!(!Token::Star.is_ident());
    }
    #[test]
    fn is_keyword_match() {
        assert!(Token::Ident("MATCH".into()).is_keyword("match"));
        assert!(Token::Ident("match".into()).is_keyword("MATCH"));
    }
    #[test]
    fn is_keyword_no_match() {
        assert!(!Token::Ident("RETURN".into()).is_keyword("MATCH"));
        assert!(!Token::Star.is_keyword("MATCH"));
    }
    #[test]
    fn as_ident_str_some() {
        assert_eq!(Token::Ident("foo".into()).as_ident_str(), Some("foo"));
        assert_eq!(Token::QuotedIdent("bar".into()).as_ident_str(), Some("bar"));
    }
    #[test]
    fn as_ident_str_none() {
        assert_eq!(Token::Star.as_ident_str(), None);
    }
}
