//! Parser utilities: the [`Parser`] struct and token-matching helpers.

use crate::error::GqlError;
use crate::token::{Span, Spanned, Token};

/// A recursive-descent parser over a token stream.
///
/// The parser tracks a position within the spanned token slice and provides
/// methods for peeking, advancing, and expecting tokens.
pub struct Parser<'a> {
    tokens: &'a [Spanned],
    pos: usize,
}

impl<'a> Parser<'a> {
    /// Creates a new parser over the given token stream.
    pub fn new(tokens: &'a [Spanned]) -> Self {
        Self { tokens, pos: 0 }
    }

    // ── Position ─────────────────────────────────────────────────────────

    /// Returns the current position (token index).
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Saves the current position for backtracking.
    pub fn save(&self) -> usize {
        self.pos
    }

    /// Restores a previously saved position.
    pub fn restore(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Returns `true` if all tokens have been consumed.
    pub fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// Returns how many tokens remain.
    pub fn remaining(&self) -> usize {
        self.tokens.len().saturating_sub(self.pos)
    }

    /// Returns a [`Span`] covering from the token at `saved_pos` to the last
    /// consumed token (i.e. the token just before `self.pos`).
    ///
    /// Typical usage:
    /// ```ignore
    /// let start = self.save();
    /// // ... parse something ...
    /// let span = self.span_since(start);
    /// ```
    pub fn span_since(&self, saved_pos: usize) -> Span {
        let start = self
            .tokens
            .get(saved_pos)
            .map(|s| s.span.start)
            .unwrap_or(0);
        let end = if self.pos > 0 {
            self.tokens[self.pos - 1].span.end
        } else {
            start
        };
        Span { start, end }
    }

    // ── Peek ─────────────────────────────────────────────────────────────

    /// Returns the current token without advancing, or `None` at end.
    pub fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|s| &s.token)
    }

    /// Returns the token at `offset` positions ahead, or `None`.
    pub fn peek_ahead(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset).map(|s| &s.token)
    }

    /// Returns the span of the current token, or a zero-width span at the end.
    pub fn current_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|s| s.span)
            .unwrap_or_else(|| {
                self.tokens
                    .last()
                    .map(|s| Span {
                        start: s.span.end,
                        end: s.span.end,
                    })
                    .unwrap_or(Span { start: 0, end: 0 })
            })
    }

    // ── Advance ──────────────────────────────────────────────────────────

    /// Advances past the current token and returns it.
    /// Panics if at end — callers should check with `peek()` or `at_end()` first.
    pub fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos].token;
        self.pos += 1;
        tok
    }

    /// Advances and returns the current spanned token.
    pub fn advance_spanned(&mut self) -> &Spanned {
        let s = &self.tokens[self.pos];
        self.pos += 1;
        s
    }

    // ── Keyword matching ─────────────────────────────────────────────────

    /// Returns `true` if the current token is an identifier matching `kw`
    /// (case-insensitive).
    pub fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    /// Returns `true` if the token at `offset` ahead is a keyword match.
    pub fn at_keyword_ahead(&self, offset: usize, kw: &str) -> bool {
        matches!(self.peek_ahead(offset), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    /// Consumes the current token if it matches keyword `kw` (case-insensitive).
    /// Returns `true` if consumed.
    pub fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.at_keyword(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Expects and consumes a keyword, or returns an error.
    pub fn expect_keyword(&mut self, kw: &str) -> Result<(), GqlError> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(self.expected(&format!("'{kw}'")))
        }
    }

    // ── Token matching ───────────────────────────────────────────────────

    /// Returns `true` if the current token matches exactly.
    pub fn at_token(&self, tok: &Token) -> bool {
        self.peek() == Some(tok)
    }

    /// Consumes the current token if it matches exactly. Returns `true` if consumed.
    pub fn eat_token(&mut self, tok: &Token) -> bool {
        if self.at_token(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Expects and consumes an exact token, or returns an error.
    pub fn expect_token(&mut self, tok: &Token) -> Result<(), GqlError> {
        if self.eat_token(tok) {
            Ok(())
        } else {
            Err(self.expected(&format!("'{}'", token_display(tok))))
        }
    }

    // ── Identifier ───────────────────────────────────────────────────────

    /// Returns `true` if the current token is any identifier (including quoted).
    pub fn at_ident(&self) -> bool {
        matches!(self.peek(), Some(Token::Ident(_) | Token::QuotedIdent(_)))
    }

    /// Consumes and returns an identifier (quoted or unquoted).
    pub fn expect_ident(&mut self) -> Result<String, GqlError> {
        match self.peek() {
            Some(Token::Ident(s) | Token::QuotedIdent(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.expected("identifier")),
        }
    }

    /// Consumes an identifier that is NOT a reserved keyword.
    /// Quoted identifiers always pass.
    pub fn expect_ident_non_reserved(&mut self) -> Result<String, GqlError> {
        match self.peek() {
            Some(Token::QuotedIdent(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            Some(Token::Ident(s)) if !is_reserved_keyword(s) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.expected("identifier (non-reserved)")),
        }
    }

    /// Consumes an identifier if present. Quoted identifiers always match.
    /// Unquoted identifiers match only if they are not reserved keywords.
    pub fn eat_ident_non_reserved(&mut self) -> Option<String> {
        match self.peek() {
            Some(Token::QuotedIdent(s)) => {
                let s = s.clone();
                self.pos += 1;
                Some(s)
            }
            Some(Token::Ident(s)) if !is_reserved_keyword(s) => {
                let s = s.clone();
                self.pos += 1;
                Some(s)
            }
            _ => None,
        }
    }

    // ── Numeric ──────────────────────────────────────────────────────────

    /// Consumes an unsigned integer literal. Returns the value as u64.
    pub fn expect_unsigned_int(&mut self) -> Result<u64, GqlError> {
        match self.peek() {
            Some(Token::Int(v)) if *v >= 0 => {
                let v = *v as u64;
                self.pos += 1;
                Ok(v)
            }
            _ => Err(self.expected("unsigned integer")),
        }
    }

    // ── Comma-separated lists ────────────────────────────────────────────

    /// Parses a comma-separated list of items using the given parser function.
    /// Requires at least one item.
    pub fn comma_list<T>(
        &mut self,
        mut parse_item: impl FnMut(&mut Self) -> Result<T, GqlError>,
    ) -> Result<Vec<T>, GqlError> {
        let mut items = vec![parse_item(self)?];
        while self.eat_token(&Token::Comma) {
            items.push(parse_item(self)?);
        }
        Ok(items)
    }

    // ── Error helpers ────────────────────────────────────────────────────

    /// Creates an "expected X" parse error at the current position.
    pub fn expected(&self, what: &str) -> GqlError {
        let got = match self.peek() {
            Some(tok) => format!("'{}'", token_display(tok)),
            None => "end of input".to_string(),
        };
        GqlError::Parse(format!("expected {what}, got {got}"))
    }

    /// Creates a parse error with a custom message.
    pub fn error(&self, msg: impl Into<String>) -> GqlError {
        GqlError::Parse(msg.into())
    }
}

/// Returns a human-readable description of a token for error messages.
fn token_display(tok: &Token) -> String {
    match tok {
        Token::Ident(s) => s.clone(),
        Token::QuotedIdent(s) => format!("\"{s}\""),
        Token::StringLit(s) => format!("'{s}'"),
        Token::Int(v) => v.to_string(),
        Token::BigInt(v) => v.clone(),
        Token::Float(v) => v.to_string(),
        Token::ExactNumeric(v) => format!("{v}M"),
        Token::BytesLit(_) => "X'...'".to_string(),
        Token::Param(s) => format!("${s}"),
        Token::SubstitutedParam(s) => format!("$${s}"),
        Token::LParen => "(".into(),
        Token::RParen => ")".into(),
        Token::LBracket => "[".into(),
        Token::RBracket => "]".into(),
        Token::LBrace => "{".into(),
        Token::RBrace => "}".into(),
        Token::Colon => ":".into(),
        Token::Comma => ",".into(),
        Token::Dot => ".".into(),
        Token::Star => "*".into(),
        Token::Minus => "-".into(),
        Token::Plus => "+".into(),
        Token::Slash => "/".into(),
        Token::Percent => "%".into(),
        Token::Pipe => "|".into(),
        Token::Ampersand => "&".into(),
        Token::Bang => "!".into(),
        Token::Tilde => "~".into(),
        Token::Question => "?".into(),
        Token::At => "@".into(),
        Token::Eq => "=".into(),
        Token::Lt => "<".into(),
        Token::Gt => ">".into(),
        Token::Ne => "<>".into(),
        Token::Le => "<=".into(),
        Token::Ge => ">=".into(),
        Token::RangeDots => "..".into(),
        Token::DoubleColon => "::".into(),
        Token::RightDoubleArrow => "=>".into(),
        Token::Concat => "||".into(),
        Token::MultisetAlt => "|+|".into(),
        Token::RightArrow => "->".into(),
        Token::LeftArrow => "<-".into(),
        Token::LeftMinusRight => "<->".into(),
        Token::TildeRightArrow => "~>".into(),
        Token::LeftArrowTilde => "<~".into(),
        Token::MinusLeftBracket => "-[".into(),
        Token::BracketRightArrow => "]->".into(),
        Token::RightBracketMinus => "]-".into(),
        Token::TildeLeftBracket => "~[".into(),
        Token::BracketTildeRightArrow => "]~>".into(),
        Token::RightBracketTilde => "]~".into(),
        Token::LeftArrowBracket => "<-[".into(),
        Token::LeftArrowTildeBracket => "<~[".into(),
        Token::MinusSlash => "-/".into(),
        Token::SlashMinus => "/-".into(),
        Token::SlashMinusRight => "/->".into(),
        Token::TildeSlash => "~/".into(),
        Token::SlashTilde => "/~".into(),
        Token::SlashTildeRight => "/~>".into(),
        Token::LeftMinusSlash => "<-/".into(),
        Token::LeftTildeSlash => "<~/".into(),
    }
}

/// GQL reserved keywords (ISO/IEC 39075 §21.3).
///
/// This is the full set from GQL. Non-reserved words (like GRAPH, EDGE, etc.)
/// are NOT included — they can be used as identifiers without quoting.
pub fn is_reserved_keyword(s: &str) -> bool {
    // Compare uppercase for case-insensitive matching.
    matches!(
        s.to_ascii_uppercase().as_str(),
        "ABS"
            | "ACOS"
            | "ALL"
            | "ALL_DIFFERENT"
            | "AND"
            | "ANY"
            | "ARRAY"
            | "AS"
            | "ASC"
            | "ASCENDING"
            | "ASIN"
            | "AT"
            | "ATAN"
            | "AVG"
            | "BIG"
            | "BIGINT"
            | "BINARY"
            | "BOOL"
            | "BOOLEAN"
            | "BOTH"
            | "BTRIM"
            | "BY"
            | "BYTE_LENGTH"
            | "BYTES"
            | "CALL"
            | "CARDINALITY"
            | "CASE"
            | "CAST"
            | "CEIL"
            | "CEILING"
            | "CHAR"
            | "CHAR_LENGTH"
            | "CHARACTER_LENGTH"
            | "CHARACTERISTICS"
            | "CLOSE"
            | "COALESCE"
            | "COLLECT_LIST"
            | "COMMIT"
            | "COPY"
            | "COS"
            | "COSH"
            | "COT"
            | "COUNT"
            | "CREATE"
            | "CURRENT_DATE"
            | "CURRENT_GRAPH"
            | "CURRENT_PROPERTY_GRAPH"
            | "CURRENT_SCHEMA"
            | "CURRENT_TIME"
            | "CURRENT_TIMESTAMP"
            | "DATE"
            | "DATETIME"
            | "DAY"
            | "DEC"
            | "DECIMAL"
            | "DEGREES"
            | "DELETE"
            | "DESC"
            | "DESCENDING"
            | "DETACH"
            | "DISTINCT"
            | "DOUBLE"
            | "DROP"
            | "DURATION"
            | "DURATION_BETWEEN"
            | "ELEMENT_ID"
            | "ELSE"
            | "END"
            | "EXCEPT"
            | "EXISTS"
            | "EXP"
            | "FILTER"
            | "FINISH"
            | "FLOAT"
            | "FLOAT16"
            | "FLOAT32"
            | "FLOAT64"
            | "FLOAT128"
            | "FLOAT256"
            | "FLOOR"
            | "FOR"
            | "FROM"
            | "GROUP"
            | "HAVING"
            | "HOME_GRAPH"
            | "HOME_PROPERTY_GRAPH"
            | "HOME_SCHEMA"
            | "HOUR"
            | "IF"
            | "IN"
            | "INSERT"
            | "INT"
            | "INT8"
            | "INT16"
            | "INT32"
            | "INT64"
            | "INT128"
            | "INT256"
            | "INTEGER"
            | "INTEGER8"
            | "INTEGER16"
            | "INTEGER32"
            | "INTEGER64"
            | "INTEGER128"
            | "INTEGER256"
            | "INTERSECT"
            | "INTERVAL"
            | "IS"
            | "LEADING"
            | "LEFT"
            | "LET"
            | "LIKE"
            | "LIMIT"
            | "LIST"
            | "LN"
            | "LOCAL"
            | "LOCAL_DATETIME"
            | "LOCAL_TIME"
            | "LOCAL_TIMESTAMP"
            | "LOG"
            | "LOG10"
            | "LOWER"
            | "LTRIM"
            | "MATCH"
            | "MAX"
            | "MIN"
            | "MINUTE"
            | "MOD"
            | "MONTH"
            | "NEXT"
            | "NODETACH"
            | "NORMALIZE"
            | "NOT"
            | "NOTHING"
            | "NULL"
            | "NULLS"
            | "NULLIF"
            | "OCTET_LENGTH"
            | "OF"
            | "OFFSET"
            | "OPTIONAL"
            | "OR"
            | "ORDER"
            | "OTHERWISE"
            | "PARAMETER"
            | "PARAMETERS"
            | "PATH"
            | "PATH_LENGTH"
            | "PATHS"
            | "PERCENTILE_CONT"
            | "PERCENTILE_DISC"
            | "POWER"
            | "PRECISION"
            | "PROPERTY_EXISTS"
            | "RADIANS"
            | "REAL"
            | "RECORD"
            | "REMOVE"
            | "REPLACE"
            | "RESET"
            | "RETURN"
            | "RIGHT"
            | "ROLLBACK"
            | "RTRIM"
            | "SAME"
            | "SCHEMA"
            | "SECOND"
            | "SELECT"
            | "SESSION"
            | "SESSION_USER"
            | "SET"
            | "SIGNED"
            | "SIN"
            | "SINH"
            | "SIZE"
            | "SKIP"
            | "SMALL"
            | "SMALLINT"
            | "SQRT"
            | "START"
            | "STDDEV_POP"
            | "STDDEV_SAMP"
            | "STRING"
            | "SUM"
            | "TAN"
            | "TANH"
            | "THEN"
            | "TIME"
            | "TIMESTAMP"
            | "TRAILING"
            | "TRIM"
            | "TYPED"
            | "UBIGINT"
            | "UINT"
            | "UINT8"
            | "UINT16"
            | "UINT32"
            | "UINT64"
            | "UINT128"
            | "UINT256"
            | "UNION"
            | "UNSIGNED"
            | "UPPER"
            | "USE"
            | "USMALLINT"
            | "VALUE"
            | "VARBINARY"
            | "VARCHAR"
            | "VARIABLE"
            | "WHEN"
            | "WHERE"
            | "WITH"
            | "XOR"
            | "YEAR"
            | "YIELD"
            | "ZONED"
            | "ZONED_DATETIME"
            | "ZONED_TIME"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{Span, Spanned, Token};

    /// Helper to create a Spanned token with a simple span.
    fn spanned(token: Token, start: usize, end: usize) -> Spanned {
        Spanned {
            token,
            span: Span { start, end },
        }
    }

    fn make_parser(tokens: &[Spanned]) -> Parser<'_> {
        Parser::new(tokens)
    }

    // ── pos() ─────────────────────────────────────────────────────────────

    #[test]
    fn test_pos_initial() {
        let tokens = [spanned(Token::Ident("x".into()), 0, 1)];
        let p = make_parser(&tokens);
        assert_eq!(p.pos(), 0);
    }

    #[test]
    fn test_pos_after_advance() {
        let tokens = [
            spanned(Token::Ident("a".into()), 0, 1),
            spanned(Token::Ident("b".into()), 2, 3),
        ];
        let mut p = make_parser(&tokens);
        p.advance();
        assert_eq!(p.pos(), 1);
    }

    // ── remaining() ───────────────────────────────────────────────────────

    #[test]
    fn test_remaining_full() {
        let tokens = [
            spanned(Token::Ident("a".into()), 0, 1),
            spanned(Token::Ident("b".into()), 2, 3),
        ];
        let p = make_parser(&tokens);
        assert_eq!(p.remaining(), 2);
    }

    #[test]
    fn test_remaining_empty() {
        let tokens = [];
        let p = make_parser(&tokens);
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn test_remaining_after_advance() {
        let tokens = [
            spanned(Token::Ident("a".into()), 0, 1),
            spanned(Token::Ident("b".into()), 2, 3),
        ];
        let mut p = make_parser(&tokens);
        p.advance();
        assert_eq!(p.remaining(), 1);
    }

    // ── current_span() ──────────────────────────────────────────────────

    #[test]
    fn test_current_span_at_token() {
        let tokens = [spanned(Token::Ident("x".into()), 5, 10)];
        let p = make_parser(&tokens);
        let sp = p.current_span();
        assert_eq!(sp.start, 5);
        assert_eq!(sp.end, 10);
    }

    #[test]
    fn test_current_span_at_end_with_tokens() {
        // When at end but tokens exist, should return zero-width span at end of last token.
        let tokens = [spanned(Token::Ident("x".into()), 5, 10)];
        let mut p = make_parser(&tokens);
        p.advance();
        let sp = p.current_span();
        assert_eq!(sp.start, 10);
        assert_eq!(sp.end, 10);
    }

    #[test]
    fn test_current_span_empty_tokens() {
        // When no tokens at all, should return Span { start: 0, end: 0 }.
        let tokens = [];
        let p = make_parser(&tokens);
        let sp = p.current_span();
        assert_eq!(sp.start, 0);
        assert_eq!(sp.end, 0);
    }

    // ── advance_spanned() ────────────────────────────────────────────────

    #[test]
    fn test_advance_spanned() {
        let tokens = [
            spanned(Token::Ident("hello".into()), 0, 5),
            spanned(Token::Comma, 5, 6),
        ];
        let mut p = make_parser(&tokens);
        let s = p.advance_spanned();
        assert_eq!(s.token, Token::Ident("hello".into()));
        assert_eq!(s.span.start, 0);
        assert_eq!(s.span.end, 5);
        assert_eq!(p.pos(), 1);
    }

    // ── expect_ident_non_reserved() ──────────────────────────────────────

    #[test]
    fn test_expect_ident_non_reserved_unquoted() {
        let tokens = [spanned(Token::Ident("myVar".into()), 0, 5)];
        let mut p = make_parser(&tokens);
        assert_eq!(p.expect_ident_non_reserved().unwrap(), "myVar");
    }

    #[test]
    fn test_expect_ident_non_reserved_quoted() {
        let tokens = [spanned(Token::QuotedIdent("SELECT".into()), 0, 8)];
        let mut p = make_parser(&tokens);
        // Quoted identifiers always pass even if the name is a reserved word.
        assert_eq!(p.expect_ident_non_reserved().unwrap(), "SELECT");
    }

    #[test]
    fn test_expect_ident_non_reserved_fails_on_reserved() {
        let tokens = [spanned(Token::Ident("MATCH".into()), 0, 5)];
        let mut p = make_parser(&tokens);
        assert!(p.expect_ident_non_reserved().is_err());
    }

    #[test]
    fn test_expect_ident_non_reserved_fails_on_non_ident() {
        let tokens = [spanned(Token::Comma, 0, 1)];
        let mut p = make_parser(&tokens);
        assert!(p.expect_ident_non_reserved().is_err());
    }

    // ── token_display() coverage ─────────────────────────────────────────

    #[test]
    fn test_token_display_quoted_ident() {
        assert_eq!(token_display(&Token::QuotedIdent("foo".into())), "\"foo\"");
    }

    #[test]
    fn test_token_display_string_lit() {
        assert_eq!(token_display(&Token::StringLit("hello".into())), "'hello'");
    }

    #[test]
    fn test_token_display_bigint() {
        assert_eq!(
            token_display(&Token::BigInt("99999999999999999999".into())),
            "99999999999999999999"
        );
    }

    #[test]
    fn test_token_display_exact_numeric() {
        assert_eq!(token_display(&Token::ExactNumeric("3.14".into())), "3.14M");
    }

    #[test]
    fn test_token_display_bytes_lit() {
        assert_eq!(token_display(&Token::BytesLit(vec![0x41, 0x42])), "X'...'");
    }

    #[test]
    fn test_token_display_param() {
        assert_eq!(token_display(&Token::Param("name".into())), "$name");
    }

    #[test]
    fn test_token_display_substituted_param() {
        assert_eq!(
            token_display(&Token::SubstitutedParam("val".into())),
            "$$val"
        );
    }

    #[test]
    fn test_token_display_punctuation() {
        // Test all punctuation tokens that were uncovered.
        assert_eq!(token_display(&Token::LBracket), "[");
        assert_eq!(token_display(&Token::RBracket), "]");
        assert_eq!(token_display(&Token::LBrace), "{");
        assert_eq!(token_display(&Token::Star), "*");
        assert_eq!(token_display(&Token::Slash), "/");
        assert_eq!(token_display(&Token::Percent), "%");
        assert_eq!(token_display(&Token::Pipe), "|");
        assert_eq!(token_display(&Token::Ampersand), "&");
        assert_eq!(token_display(&Token::Bang), "!");
        assert_eq!(token_display(&Token::Tilde), "~");
        assert_eq!(token_display(&Token::Question), "?");
        assert_eq!(token_display(&Token::At), "@");
        assert_eq!(token_display(&Token::Ne), "<>");
        assert_eq!(token_display(&Token::Le), "<=");
        assert_eq!(token_display(&Token::RangeDots), "..");
        assert_eq!(token_display(&Token::DoubleColon), "::");
        assert_eq!(token_display(&Token::RightDoubleArrow), "=>");
        assert_eq!(token_display(&Token::Concat), "||");
        assert_eq!(token_display(&Token::MultisetAlt), "|+|");
        assert_eq!(token_display(&Token::RightArrow), "->");
        assert_eq!(token_display(&Token::LeftArrow), "<-");
        assert_eq!(token_display(&Token::LeftMinusRight), "<->");
        assert_eq!(token_display(&Token::TildeRightArrow), "~>");
        assert_eq!(token_display(&Token::LeftArrowTilde), "<~");
        assert_eq!(token_display(&Token::MinusLeftBracket), "-[");
        assert_eq!(token_display(&Token::BracketRightArrow), "]->");
        assert_eq!(token_display(&Token::RightBracketMinus), "]-");
        assert_eq!(token_display(&Token::TildeLeftBracket), "~[");
        assert_eq!(token_display(&Token::BracketTildeRightArrow), "]~>");
        assert_eq!(token_display(&Token::RightBracketTilde), "]~");
        assert_eq!(token_display(&Token::LeftArrowBracket), "<-[");
        assert_eq!(token_display(&Token::LeftArrowTildeBracket), "<~[");
        assert_eq!(token_display(&Token::MinusSlash), "-/");
        assert_eq!(token_display(&Token::SlashMinus), "/-");
        assert_eq!(token_display(&Token::SlashMinusRight), "/->");
        assert_eq!(token_display(&Token::TildeSlash), "~/");
        assert_eq!(token_display(&Token::SlashTilde), "/~");
        assert_eq!(token_display(&Token::SlashTildeRight), "/~>");
        assert_eq!(token_display(&Token::LeftMinusSlash), "<-/");
        assert_eq!(token_display(&Token::LeftTildeSlash), "<~/");
    }

    // ── save / restore ──────────────────────────────────────────────────

    #[test]
    fn test_save_restore() {
        let tokens = [
            spanned(Token::Ident("a".into()), 0, 1),
            spanned(Token::Ident("b".into()), 2, 3),
        ];
        let mut p = make_parser(&tokens);
        let saved = p.save();
        p.advance();
        assert_eq!(p.pos(), 1);
        p.restore(saved);
        assert_eq!(p.pos(), 0);
    }

    // ── error helpers ────────────────────────────────────────────────────

    #[test]
    fn test_expected_at_end() {
        let tokens = [];
        let p = make_parser(&tokens);
        let err = p.expected("something");
        match err {
            GqlError::Parse(msg) => assert!(msg.contains("end of input")),
            _ => panic!("expected Parse error"),
        }
    }

    #[test]
    fn test_error_custom() {
        let tokens = [];
        let p = make_parser(&tokens);
        let err = p.error("custom message");
        match err {
            GqlError::Parse(msg) => assert_eq!(msg, "custom message"),
            _ => panic!("expected Parse error"),
        }
    }

    // ── expect_token error paths (exercises token_display via error) ────

    #[test]
    fn test_expect_token_error_shows_got_token() {
        let tokens = [spanned(Token::Ident("foo".into()), 0, 3)];
        let mut p = make_parser(&tokens);
        let err = p.expect_token(&Token::LParen).unwrap_err();
        match err {
            GqlError::Parse(msg) => {
                assert!(msg.contains("'('"), "msg: {msg}");
                assert!(msg.contains("'foo'"), "msg: {msg}");
            }
            _ => panic!("expected Parse error"),
        }
    }
}
