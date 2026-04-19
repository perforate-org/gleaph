//! GQL tokenizer (lexer).
//!
//! Converts a GQL source string into a sequence of [`Spanned`] tokens.
//! Handles all GQL token types including:
//!
//! - Identifiers (Unicode `ID_Start` / `ID_Continue`, plus `_`)
//! - Delimited identifiers (double-quoted, backtick-quoted)
//! - String literals (single-quoted, with `@` no-escape prefix)
//! - Byte literals (`X'hex'`)
//! - Numeric literals (decimal, hex, octal, binary; `M`/`F`/`D` suffixes;
//!   underscore separators; scientific notation)
//! - Parameters (`$name`, `$$name`)
//! - All multi-character punctuation from GQL (edge patterns, simplified
//!   paths, comparison operators, etc.)
//! - Comments (`//`, `--`, `/* */`)

use crate::error::GqlError;
use crate::token::{Comment, CommentKind, Span, Spanned, Token};

/// Result type alias for the lexer.
pub type LexResult = Result<Vec<Spanned>, GqlError>;

/// The result of tokenization with comment preservation.
#[derive(Debug)]
pub struct TokenizeResult {
    /// The token stream (comments and whitespace excluded).
    pub tokens: Vec<Spanned>,
    /// All comments found in the source, in order of appearance.
    pub comments: Vec<Comment>,
}

/// Tokenizes a GQL source string into a list of spanned tokens.
///
/// Comments and whitespace are silently consumed. Returns a [`GqlError::Parse`]
/// on unrecognized characters or malformed literals.
pub fn tokenize(input: &str) -> LexResult {
    let mut lexer = Lexer::new(input);
    lexer.run()?;
    Ok(lexer.tokens)
}

/// Tokenizes a GQL source string, preserving comments alongside the token
/// stream.
///
/// This is the comment-preserving alternative to [`tokenize`]. The returned
/// [`TokenizeResult`] contains both the token stream and a list of all
/// comments found in the source.
pub fn tokenize_with_comments(input: &str) -> Result<TokenizeResult, GqlError> {
    let mut lexer = Lexer::new(input);
    lexer.run()?;
    Ok(TokenizeResult {
        tokens: lexer.tokens,
        comments: lexer.comments,
    })
}

/// Tokenizes and strips span information, returning bare tokens.
/// Convenience for tests and simple use cases.
pub fn tokenize_bare(input: &str) -> Result<Vec<Token>, GqlError> {
    Ok(tokenize(input)?.into_iter().map(|s| s.token).collect())
}

// ─────────────────────────────────────────────────────────────────────────────
// Lexer internals
// ─────────────────────────────────────────────────────────────────────────────

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Spanned>,
    comments: Vec<Comment>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            comments: Vec::new(),
        }
    }

    fn run(&mut self) -> Result<(), GqlError> {
        while self.pos < self.bytes.len() {
            self.skip_ws_and_comments()?;
            if self.pos >= self.bytes.len() {
                break;
            }
            self.next_token()?;
        }
        Ok(())
    }

    // ── Whitespace & comments ────────────────────────────────────────────

    fn skip_ws_and_comments(&mut self) -> Result<(), GqlError> {
        loop {
            // Skip whitespace.
            while self.pos < self.bytes.len() && is_whitespace(self.bytes[self.pos]) {
                self.pos += 1; // is_whitespace only matches single-byte ASCII
            }
            if self.pos >= self.bytes.len() {
                return Ok(());
            }
            // Line comment: // or --
            if self.pos + 1 < self.bytes.len() {
                let a = self.bytes[self.pos];
                let b = self.bytes[self.pos + 1];
                if (a == b'/' && b == b'/') || (a == b'-' && b == b'-') {
                    let comment_start = self.pos;
                    self.pos += 2; // skip delimiter
                    let text_start = self.pos;
                    while self.pos < self.bytes.len()
                        && self.bytes[self.pos] != b'\n'
                        && self.bytes[self.pos] != b'\r'
                    {
                        self.pos += 1;
                    }
                    self.comments.push(Comment {
                        span: Span {
                            start: comment_start,
                            end: self.pos,
                        },
                        kind: CommentKind::Line,
                        text: self.src[text_start..self.pos].to_string(),
                    });
                    continue;
                }
            }
            // Block comment: /* ... */ (supports nesting)
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'*'
            {
                let comment_start = self.pos;
                self.pos += 2; // skip /*
                let text_start = self.pos;
                let mut depth = 1u32;
                while self.pos + 1 < self.bytes.len() && depth > 0 {
                    if self.bytes[self.pos] == b'/' && self.bytes[self.pos + 1] == b'*' {
                        depth += 1;
                        self.pos += 2;
                    } else if self.bytes[self.pos] == b'*' && self.bytes[self.pos + 1] == b'/' {
                        depth -= 1;
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                    }
                }
                if depth > 0 {
                    return Err(self.err("unterminated block comment"));
                }
                // text_end is before the final */
                let text_end = if self.pos >= 2 {
                    self.pos - 2
                } else {
                    text_start
                };
                self.comments.push(Comment {
                    span: Span {
                        start: comment_start,
                        end: self.pos,
                    },
                    kind: CommentKind::Block,
                    text: self.src[text_start..text_end].to_string(),
                });
                continue;
            }
            break;
        }
        Ok(())
    }

    // ── Main dispatch ────────────────────────────────────────────────────

    fn next_token(&mut self) -> Result<(), GqlError> {
        let start = self.pos;
        let b = self.bytes[self.pos];

        match b {
            // ── Tokens starting with `<` ─────────────────────────────
            b'<' => self.lex_left_angle(start),

            // ── Tokens starting with `]` ─────────────────────────────
            b']' => self.lex_right_bracket(start),

            // ── Tokens starting with `-` ─────────────────────────────
            b'-' => self.lex_minus(start),

            // ── Tokens starting with `~` ─────────────────────────────
            b'~' => self.lex_tilde(start),

            // ── Tokens starting with `/` ─────────────────────────────
            b'/' => self.lex_slash(start),

            // ── Tokens starting with `|` ─────────────────────────────
            b'|' => self.lex_pipe(start),

            // ── Tokens starting with `$` ─────────────────────────────
            b'$' => self.lex_dollar(start),

            // ── Tokens starting with `.` ─────────────────────────────
            b'.' => {
                if self.peek(1) == Some(b'.') {
                    self.pos += 2;
                    self.emit(Token::RangeDots, start);
                } else {
                    self.pos += 1;
                    self.emit(Token::Dot, start);
                }
                Ok(())
            }

            // ── Tokens starting with `:` ─────────────────────────────
            b':' => {
                if self.peek(1) == Some(b':') {
                    self.pos += 2;
                    self.emit(Token::DoubleColon, start);
                } else {
                    self.pos += 1;
                    self.emit(Token::Colon, start);
                }
                Ok(())
            }

            // ── Tokens starting with `=` ─────────────────────────────
            b'=' => {
                if self.peek(1) == Some(b'>') {
                    self.pos += 2;
                    self.emit(Token::RightDoubleArrow, start);
                } else {
                    self.pos += 1;
                    self.emit(Token::Eq, start);
                }
                Ok(())
            }

            // ── Tokens starting with `>` ─────────────────────────────
            b'>' => {
                if self.peek(1) == Some(b'=') {
                    self.pos += 2;
                    self.emit(Token::Ge, start);
                } else {
                    self.pos += 1;
                    self.emit(Token::Gt, start);
                }
                Ok(())
            }

            // ── Simple single-character tokens ───────────────────────
            b'(' => {
                self.pos += 1;
                self.emit(Token::LParen, start);
                Ok(())
            }
            b')' => {
                self.pos += 1;
                self.emit(Token::RParen, start);
                Ok(())
            }
            b'[' => {
                self.pos += 1;
                self.emit(Token::LBracket, start);
                Ok(())
            }
            b'{' => {
                self.pos += 1;
                self.emit(Token::LBrace, start);
                Ok(())
            }
            b'}' => {
                self.pos += 1;
                self.emit(Token::RBrace, start);
                Ok(())
            }
            b',' => {
                self.pos += 1;
                self.emit(Token::Comma, start);
                Ok(())
            }
            b'*' => {
                self.pos += 1;
                self.emit(Token::Star, start);
                Ok(())
            }
            b'+' => {
                self.pos += 1;
                self.emit(Token::Plus, start);
                Ok(())
            }
            b'%' => {
                self.pos += 1;
                self.emit(Token::Percent, start);
                Ok(())
            }
            b'&' => {
                self.pos += 1;
                self.emit(Token::Ampersand, start);
                Ok(())
            }
            b'!' => {
                self.pos += 1;
                self.emit(Token::Bang, start);
                Ok(())
            }
            b'?' => {
                self.pos += 1;
                self.emit(Token::Question, start);
                Ok(())
            }

            // ── `@` — no-escape prefix or standalone ─────────────────
            b'@' => {
                if matches!(self.peek(1), Some(b'\'' | b'"' | b'`')) {
                    // No-escape prefix: `@'...'`, `@"..."`, @`...`
                    self.pos += 1; // skip @
                    let delim = self.bytes[self.pos];
                    self.lex_no_escape_string(start, delim)
                } else {
                    self.pos += 1;
                    self.emit(Token::At, start);
                    Ok(())
                }
            }

            // ── String & byte literals ───────────────────────────────
            b'\'' => self.lex_string(start),
            b'"' => self.lex_double_quoted(start),
            b'`' => self.lex_backtick(start),

            // ── Byte literal: X'...' or x'...' ──────────────────────
            b'X' | b'x' if self.peek(1) == Some(b'\'') => self.lex_bytes_literal(start),

            // ── Numeric literals ─────────────────────────────────────
            b'0'..=b'9' => self.lex_number(start),

            // ── Identifier ───────────────────────────────────────────
            _ => {
                let ch = self.current_char();
                if is_ident_start(ch) {
                    self.lex_ident(start)
                } else {
                    Err(GqlError::Parse(format!("unexpected character: '{ch}'")))
                }
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn emit(&mut self, token: Token, start: usize) {
        self.tokens.push(Spanned {
            token,
            span: Span {
                start,
                end: self.pos,
            },
        });
    }

    fn current_char(&self) -> char {
        // Safe: we only call this when pos < bytes.len().
        self.src[self.pos..].chars().next().unwrap_or('\0')
    }

    fn err(&self, msg: impl Into<String>) -> GqlError {
        GqlError::Parse(msg.into())
    }

    // ── Multi-character token lexers ─────────────────────────────────────

    /// Tokens starting with `<`: `<->`, `<-[`, `<-/`, `<-`, `<~[`, `<~/`, `<~`, `<=`, `<>`, `<`.
    fn lex_left_angle(&mut self, start: usize) -> Result<(), GqlError> {
        match self.peek(1) {
            Some(b'-') => match self.peek(2) {
                Some(b'>') => {
                    self.pos += 3;
                    self.emit(Token::LeftMinusRight, start);
                }
                Some(b'[') => {
                    self.pos += 3;
                    self.emit(Token::LeftArrowBracket, start);
                }
                Some(b'/') => {
                    self.pos += 3;
                    self.emit(Token::LeftMinusSlash, start);
                }
                _ => {
                    self.pos += 2;
                    self.emit(Token::LeftArrow, start);
                }
            },
            Some(b'~') => match self.peek(2) {
                Some(b'[') => {
                    self.pos += 3;
                    self.emit(Token::LeftArrowTildeBracket, start);
                }
                Some(b'/') => {
                    self.pos += 3;
                    self.emit(Token::LeftTildeSlash, start);
                }
                _ => {
                    self.pos += 2;
                    self.emit(Token::LeftArrowTilde, start);
                }
            },
            Some(b'=') => {
                self.pos += 2;
                self.emit(Token::Le, start);
            }
            Some(b'>') => {
                self.pos += 2;
                self.emit(Token::Ne, start);
            }
            _ => {
                self.pos += 1;
                self.emit(Token::Lt, start);
            }
        }
        Ok(())
    }

    /// Tokens starting with `]`: `]->`, `]~>`, `]-`, `]~`, `]`.
    fn lex_right_bracket(&mut self, start: usize) -> Result<(), GqlError> {
        match self.peek(1) {
            Some(b'-') => {
                if self.peek(2) == Some(b'>') {
                    self.pos += 3;
                    self.emit(Token::BracketRightArrow, start);
                } else {
                    self.pos += 2;
                    self.emit(Token::RightBracketMinus, start);
                }
            }
            Some(b'~') => {
                if self.peek(2) == Some(b'>') {
                    self.pos += 3;
                    self.emit(Token::BracketTildeRightArrow, start);
                } else {
                    self.pos += 2;
                    self.emit(Token::RightBracketTilde, start);
                }
            }
            _ => {
                self.pos += 1;
                self.emit(Token::RBracket, start);
            }
        }
        Ok(())
    }

    /// Tokens starting with `-`: `->`, `-[`, `-/`, `-`.
    fn lex_minus(&mut self, start: usize) -> Result<(), GqlError> {
        match self.peek(1) {
            Some(b'>') => {
                self.pos += 2;
                self.emit(Token::RightArrow, start);
            }
            Some(b'[') => {
                self.pos += 2;
                self.emit(Token::MinusLeftBracket, start);
            }
            Some(b'/') => {
                self.pos += 2;
                self.emit(Token::MinusSlash, start);
            }
            _ => {
                self.pos += 1;
                self.emit(Token::Minus, start);
            }
        }
        Ok(())
    }

    /// Tokens starting with `~`: `~>`, `~[`, `~/`, `~`.
    fn lex_tilde(&mut self, start: usize) -> Result<(), GqlError> {
        match self.peek(1) {
            Some(b'>') => {
                self.pos += 2;
                self.emit(Token::TildeRightArrow, start);
            }
            Some(b'[') => {
                self.pos += 2;
                self.emit(Token::TildeLeftBracket, start);
            }
            Some(b'/') => {
                self.pos += 2;
                self.emit(Token::TildeSlash, start);
            }
            _ => {
                self.pos += 1;
                self.emit(Token::Tilde, start);
            }
        }
        Ok(())
    }

    /// Tokens starting with `/`: `/->`, `/-`, `/~>`, `/~`, `/`.
    fn lex_slash(&mut self, start: usize) -> Result<(), GqlError> {
        match self.peek(1) {
            Some(b'-') => {
                if self.peek(2) == Some(b'>') {
                    self.pos += 3;
                    self.emit(Token::SlashMinusRight, start);
                } else {
                    self.pos += 2;
                    self.emit(Token::SlashMinus, start);
                }
            }
            Some(b'~') => {
                if self.peek(2) == Some(b'>') {
                    self.pos += 3;
                    self.emit(Token::SlashTildeRight, start);
                } else {
                    self.pos += 2;
                    self.emit(Token::SlashTilde, start);
                }
            }
            _ => {
                self.pos += 1;
                self.emit(Token::Slash, start);
            }
        }
        Ok(())
    }

    /// Tokens starting with `|`: `|+|`, `||`, `|`.
    fn lex_pipe(&mut self, start: usize) -> Result<(), GqlError> {
        if self.peek(1) == Some(b'+') && self.peek(2) == Some(b'|') {
            self.pos += 3;
            self.emit(Token::MultisetAlt, start);
        } else if self.peek(1) == Some(b'|') {
            self.pos += 2;
            self.emit(Token::Concat, start);
        } else {
            self.pos += 1;
            self.emit(Token::Pipe, start);
        }
        Ok(())
    }

    /// Tokens starting with `$`: `$$name` (substituted) or `$name` (general parameter).
    /// Both are GQL standard: `GENERAL_PARAMETER_REFERENCE` (`$name`) and
    /// `SUBSTITUTED_PARAMETER_REFERENCE` (`$$name`).
    fn lex_dollar(&mut self, start: usize) -> Result<(), GqlError> {
        if self.peek(1) == Some(b'$') {
            // Substituted parameter: $$name
            self.pos += 2;
            let name = self.read_ident_string();
            if name.is_empty() {
                return Err(self.err("expected parameter name after '$$'"));
            }
            self.emit(Token::SubstitutedParam(name), start);
        } else {
            // General parameter: $name
            self.pos += 1;
            let name = self.read_ident_string();
            if name.is_empty() {
                return Err(self.err("expected parameter name after '$'"));
            }
            self.emit(Token::Param(name), start);
        }
        Ok(())
    }

    // ── String literals ──────────────────────────────────────────────────

    /// Lexes a single-quoted string: `'hello'`.
    fn lex_string(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 1; // skip opening '
        let mut buf = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated string literal"));
            }
            let b = self.bytes[self.pos];
            match b {
                b'\'' => {
                    // Doubled quote ('') is an escape for literal '.
                    if self.peek(1) == Some(b'\'') {
                        buf.push('\'');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        self.emit(Token::StringLit(buf), start);
                        return Ok(());
                    }
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.read_escape_char()?;
                    buf.push(esc);
                }
                _ => {
                    let ch = self.current_char();
                    self.pos += ch.len_utf8();
                    buf.push(ch);
                }
            }
        }
    }

    /// Lexes a double-quoted identifier: `"my col"`.
    fn lex_double_quoted(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 1; // skip opening "
        let mut buf = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated double-quoted identifier"));
            }
            let b = self.bytes[self.pos];
            match b {
                b'"' => {
                    if self.peek(1) == Some(b'"') {
                        buf.push('"');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        self.emit(Token::QuotedIdent(buf), start);
                        return Ok(());
                    }
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.read_escape_char()?;
                    buf.push(esc);
                }
                _ => {
                    let ch = self.current_char();
                    self.pos += ch.len_utf8();
                    buf.push(ch);
                }
            }
        }
    }

    /// Lexes a backtick-quoted identifier: `` `my col` ``.
    fn lex_backtick(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 1; // skip opening `
        let mut buf = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated backtick-quoted identifier"));
            }
            let b = self.bytes[self.pos];
            match b {
                b'`' => {
                    if self.peek(1) == Some(b'`') {
                        buf.push('`');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        self.emit(Token::QuotedIdent(buf), start);
                        return Ok(());
                    }
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.read_escape_char()?;
                    buf.push(esc);
                }
                _ => {
                    let ch = self.current_char();
                    self.pos += ch.len_utf8();
                    buf.push(ch);
                }
            }
        }
    }

    /// Lexes a no-escape string/identifier: `@'raw text'`, `@"raw id"`, @\`raw id\`.
    /// No escape processing; doubled delimiters still produce a single delimiter.
    fn lex_no_escape_string(&mut self, start: usize, delim: u8) -> Result<(), GqlError> {
        self.pos += 1; // skip opening delimiter
        let mut buf = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated no-escape literal"));
            }
            let b = self.bytes[self.pos];
            if b == delim {
                if self.peek(1) == Some(delim) {
                    buf.push(delim as char);
                    self.pos += 2;
                } else {
                    self.pos += 1;
                    let token = if delim == b'\'' {
                        Token::StringLit(buf)
                    } else {
                        Token::QuotedIdent(buf)
                    };
                    self.emit(token, start);
                    return Ok(());
                }
            } else {
                let ch = self.current_char();
                self.pos += ch.len_utf8();
                buf.push(ch);
            }
        }
    }

    /// Reads a single escape sequence after `\` has been consumed.
    fn read_escape_char(&mut self) -> Result<char, GqlError> {
        if self.pos >= self.bytes.len() {
            return Err(self.err("unterminated escape sequence"));
        }
        let b = self.bytes[self.pos];
        self.pos += 1;
        match b {
            b'\\' => Ok('\\'),
            b'\'' => Ok('\''),
            b'"' => Ok('"'),
            b'`' => Ok('`'),
            b'n' => Ok('\n'),
            b'r' => Ok('\r'),
            b't' => Ok('\t'),
            b'b' => Ok('\u{08}'), // backspace
            b'f' => Ok('\u{0C}'), // form feed
            b'u' => {
                // \uXXXX — 4 hex digits
                let hex = self.read_hex_digits(4)?;
                char::from_u32(hex).ok_or_else(|| self.err("invalid unicode escape"))
            }
            b'U' => {
                // \UXXXXXX — 6 hex digits
                let hex = self.read_hex_digits(6)?;
                char::from_u32(hex).ok_or_else(|| self.err("invalid unicode escape"))
            }
            _ => Err(self.err(format!("unknown escape sequence: \\{}", b as char))),
        }
    }

    fn read_hex_digits(&mut self, count: usize) -> Result<u32, GqlError> {
        if self.pos + count > self.bytes.len() {
            return Err(self.err("unexpected end of unicode escape"));
        }
        let mut val = 0u32;
        for _ in 0..count {
            let b = self.bytes[self.pos];
            self.pos += 1;
            let digit =
                hex_val(b).ok_or_else(|| self.err("invalid hex digit in unicode escape"))?;
            val = val * 16 + digit as u32;
        }
        Ok(val)
    }

    // ── Byte literal ─────────────────────────────────────────────────────

    fn lex_bytes_literal(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 2; // skip X'
        let mut result = Vec::new();
        loop {
            // Skip spaces (GQL allows spaces between hex pairs).
            while self.pos < self.bytes.len() && self.bytes[self.pos] == b' ' {
                self.pos += 1;
            }
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated byte literal"));
            }
            if self.bytes[self.pos] == b'\'' {
                self.pos += 1;
                self.emit(Token::BytesLit(result), start);
                return Ok(());
            }
            // Read two hex digits.
            let hi = hex_val(self.bytes[self.pos])
                .ok_or_else(|| self.err("invalid hex digit in byte literal"))?;
            self.pos += 1;
            // Skip optional space between digits of a pair.
            while self.pos < self.bytes.len() && self.bytes[self.pos] == b' ' {
                self.pos += 1;
            }
            if self.pos >= self.bytes.len() {
                return Err(self.err("unterminated byte literal"));
            }
            let lo = hex_val(self.bytes[self.pos])
                .ok_or_else(|| self.err("invalid hex digit in byte literal"))?;
            self.pos += 1;
            result.push(hi << 4 | lo);
        }
    }

    // ── Numeric literals ─────────────────────────────────────────────────

    fn lex_number(&mut self, start: usize) -> Result<(), GqlError> {
        // Check for hex/octal/binary prefix.
        if self.bytes[self.pos] == b'0' && self.pos + 1 < self.bytes.len() {
            match self.bytes[self.pos + 1] {
                b'x' | b'X' => return self.lex_hex_int(start),
                b'o' | b'O' => return self.lex_octal_int(start),
                b'b' | b'B' => return self.lex_binary_int(start),
                _ => {}
            }
        }
        self.lex_decimal_number(start)
    }

    fn lex_hex_int(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 2; // skip 0x
        let digit_start = self.pos;
        self.eat_hex_digits_with_underscores();
        if self.pos == digit_start {
            return Err(self.err("expected hex digit after '0x'"));
        }
        let raw = self.collect_digits_without_underscores(start, self.pos);
        self.emit_integer(&raw[2..], 16, &raw, start);
        Ok(())
    }

    fn lex_octal_int(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 2; // skip 0o
        let digit_start = self.pos;
        while self.pos < self.bytes.len()
            && (matches!(self.bytes[self.pos], b'0'..=b'7') || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
        if self.pos == digit_start {
            return Err(self.err("expected octal digit after '0o'"));
        }
        let raw = self.collect_digits_without_underscores(start, self.pos);
        self.emit_integer(&raw[2..], 8, &raw, start);
        Ok(())
    }

    fn lex_binary_int(&mut self, start: usize) -> Result<(), GqlError> {
        self.pos += 2; // skip 0b
        let digit_start = self.pos;
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos] == b'0'
                || self.bytes[self.pos] == b'1'
                || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
        if self.pos == digit_start {
            return Err(self.err("expected binary digit after '0b'"));
        }
        let raw = self.collect_digits_without_underscores(start, self.pos);
        self.emit_integer(&raw[2..], 2, &raw, start);
        Ok(())
    }

    fn lex_decimal_number(&mut self, start: usize) -> Result<(), GqlError> {
        // Eat integer part.
        self.eat_decimal_digits_with_underscores();

        let mut has_fraction = false;
        let mut has_exponent = false;

        // Fraction part: `.` followed by digits (but not `..` which is RangeDots).
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'.' && self.peek(1) != Some(b'.')
        {
            has_fraction = true;
            self.pos += 1; // skip '.'
            self.eat_decimal_digits_with_underscores();
        }

        // Exponent part: e/E followed by optional +/- and digits.
        if self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'e' | b'E') {
            let saved = self.pos;
            self.pos += 1;
            if self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'+' | b'-') {
                self.pos += 1;
            }
            let digit_start = self.pos;
            self.eat_decimal_digits_with_underscores();
            if self.pos > digit_start {
                has_exponent = true;
            } else {
                // Not a valid exponent; backtrack.
                self.pos = saved;
            }
        }

        // Check for M/F/D suffix.
        let suffix = if self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b'M' | b'm' => {
                    self.pos += 1;
                    Some('M')
                }
                b'F' | b'f' => {
                    // Check it's not followed by ident chars (e.g. `42FROM`).
                    if !self.is_followed_by_ident_continue() {
                        self.pos += 1;
                        Some('F')
                    } else {
                        None
                    }
                }
                b'D' | b'd' => {
                    if !self.is_followed_by_ident_continue() {
                        self.pos += 1;
                        Some('D')
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        let raw = self.collect_digits_without_underscores(start, self.pos);
        // Strip suffix character from raw for ExactNumeric.
        let num_str = if suffix.is_some() {
            &raw[..raw.len() - 1]
        } else {
            &raw
        };

        match suffix {
            Some('M') => {
                self.emit(Token::ExactNumeric(num_str.to_string()), start);
            }
            Some('F' | 'D') => {
                // Approximate numeric — parse as f64.
                let v = num_str
                    .parse::<f64>()
                    .map_err(|_| self.err("invalid approximate numeric literal"))?;
                self.emit(Token::Float(v), start);
            }
            _ => {
                if has_fraction || has_exponent {
                    let v = num_str
                        .parse::<f64>()
                        .map_err(|_| self.err("invalid float literal"))?;
                    self.emit(Token::Float(v), start);
                } else {
                    self.emit_integer(num_str, 10, num_str, start);
                }
            }
        }
        Ok(())
    }

    fn eat_decimal_digits_with_underscores(&mut self) {
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
    }

    fn eat_hex_digits_with_underscores(&mut self) {
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_hexdigit() || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
    }

    /// Collects the raw source text from `start..end` and strips underscores.
    fn collect_digits_without_underscores(&self, start: usize, end: usize) -> String {
        self.src[start..end].replace('_', "")
    }

    fn emit_integer(&mut self, digits: &str, radix: u32, raw: &str, start: usize) {
        match i64::from_str_radix(digits, radix) {
            Ok(v) => self.emit(Token::Int(v), start),
            Err(_) => self.emit(Token::BigInt(raw.to_string()), start),
        }
    }

    fn is_followed_by_ident_continue(&self) -> bool {
        if self.pos + 1 >= self.bytes.len() {
            return false;
        }
        let next_ch = self.src[self.pos + 1..].chars().next().unwrap_or('\0');
        is_ident_continue(next_ch)
    }

    // ── Identifier ───────────────────────────────────────────────────────

    fn lex_ident(&mut self, start: usize) -> Result<(), GqlError> {
        let name = self.read_ident_string();
        self.emit(Token::Ident(name), start);
        Ok(())
    }

    /// Reads an identifier string starting at the current position.
    /// Returns empty string if current char is not an ident start.
    fn read_ident_string(&mut self) -> String {
        let ident_start = self.pos;
        if self.pos < self.bytes.len() {
            let ch = self.current_char();
            if is_ident_start(ch) {
                self.pos += ch.len_utf8();
                loop {
                    if self.pos >= self.bytes.len() {
                        break;
                    }
                    let ch = self.current_char();
                    if is_ident_continue(ch) {
                        self.pos += ch.len_utf8();
                    } else {
                        break;
                    }
                }
            }
        }
        self.src[ident_start..self.pos].to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Standalone helpers
// ─────────────────────────────────────────────────────────────────────────────

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Returns `true` if `c` is a valid first character of an identifier.
/// Uses Unicode `ID_Start` plus connector punctuation (`Pc` category, i.e. `_`).
fn is_ident_start(c: char) -> bool {
    c == '_' || unicode_id_start(c)
}

/// Returns `true` if `c` may appear after the first character of an identifier.
/// Uses Unicode `ID_Continue`.
fn is_ident_continue(c: char) -> bool {
    unicode_id_continue(c)
}

/// Approximation of Unicode ID_Start property.
fn unicode_id_start(c: char) -> bool {
    c.is_alphabetic()
}

/// Approximation of Unicode ID_Continue property.
fn unicode_id_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Returns `true` if the byte is GQL whitespace (ASCII subset; the full set
/// of Unicode whitespace is handled by checking the char when needed, but
/// for performance we fast-path ASCII).
fn is_whitespace(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C | 0x1C | 0x1D | 0x1E | 0x1F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(input: &str) -> Vec<Token> {
        tokenize_bare(input).unwrap()
    }

    // ── Basic punctuation ────────────────────────────────────────────────

    #[test]
    fn simple_punctuation() {
        assert_eq!(
            toks("( ) [ ] { } : , . * + % & ! ?"),
            vec![
                Token::LParen,
                Token::RParen,
                Token::LBracket,
                Token::RBracket,
                Token::LBrace,
                Token::RBrace,
                Token::Colon,
                Token::Comma,
                Token::Dot,
                Token::Star,
                Token::Plus,
                Token::Percent,
                Token::Ampersand,
                Token::Bang,
                Token::Question,
            ]
        );
    }

    // ── Multi-character operators ────────────────────────────────────────

    #[test]
    fn comparison_operators() {
        assert_eq!(
            toks("= <> <= >= < >"),
            vec![
                Token::Eq,
                Token::Ne,
                Token::Le,
                Token::Ge,
                Token::Lt,
                Token::Gt
            ]
        );
    }

    #[test]
    fn range_dots_and_double_colon() {
        assert_eq!(toks(".. ::"), vec![Token::RangeDots, Token::DoubleColon]);
    }

    #[test]
    fn right_double_arrow() {
        assert_eq!(toks("=>"), vec![Token::RightDoubleArrow]);
    }

    #[test]
    fn concat_and_multiset() {
        assert_eq!(
            toks("|| |+| |"),
            vec![Token::Concat, Token::MultisetAlt, Token::Pipe]
        );
    }

    // ── Arrow / edge tokens ──────────────────────────────────────────────

    #[test]
    fn arrow_tokens() {
        assert_eq!(
            toks("-> <- <->"),
            vec![Token::RightArrow, Token::LeftArrow, Token::LeftMinusRight]
        );
    }

    #[test]
    fn tilde_arrows() {
        assert_eq!(
            toks("~> <~"),
            vec![Token::TildeRightArrow, Token::LeftArrowTilde]
        );
    }

    #[test]
    fn bracket_edge_tokens() {
        assert_eq!(
            toks("-[ ]-> ]- ~[ ]~> ]~ <-[ <~["),
            vec![
                Token::MinusLeftBracket,
                Token::BracketRightArrow,
                Token::RightBracketMinus,
                Token::TildeLeftBracket,
                Token::BracketTildeRightArrow,
                Token::RightBracketTilde,
                Token::LeftArrowBracket,
                Token::LeftArrowTildeBracket,
            ]
        );
    }

    #[test]
    fn simplified_path_tokens() {
        assert_eq!(
            toks("-/ /- /-> ~/ /~ /~> <-/ <~/"),
            vec![
                Token::MinusSlash,
                Token::SlashMinus,
                Token::SlashMinusRight,
                Token::TildeSlash,
                Token::SlashTilde,
                Token::SlashTildeRight,
                Token::LeftMinusSlash,
                Token::LeftTildeSlash,
            ]
        );
    }

    // ── Identifiers ──────────────────────────────────────────────────────

    #[test]
    fn plain_ident() {
        assert_eq!(toks("hello"), vec![Token::Ident("hello".into())]);
    }

    #[test]
    fn ident_with_underscore() {
        assert_eq!(toks("_foo_bar"), vec![Token::Ident("_foo_bar".into())]);
    }

    #[test]
    fn double_quoted_ident() {
        assert_eq!(
            toks(r#""my col""#),
            vec![Token::QuotedIdent("my col".into())]
        );
    }

    #[test]
    fn backtick_ident() {
        assert_eq!(toks("`min`"), vec![Token::QuotedIdent("min".into())]);
    }

    #[test]
    fn doubled_quotes_in_ident() {
        assert_eq!(
            toks(r#""say ""hello"" ""#),
            vec![Token::QuotedIdent(r#"say "hello" "#.into())]
        );
    }

    // ── String literals ──────────────────────────────────────────────────

    #[test]
    fn simple_string() {
        assert_eq!(toks("'hello'"), vec![Token::StringLit("hello".into())]);
    }

    #[test]
    fn string_with_escape() {
        assert_eq!(
            toks(r"'line\nbreak'"),
            vec![Token::StringLit("line\nbreak".into())]
        );
    }

    #[test]
    fn string_with_doubled_quote() {
        assert_eq!(toks("'it''s'"), vec![Token::StringLit("it's".into())]);
    }

    #[test]
    fn unicode_escape_4() {
        assert_eq!(toks(r"'\u0041'"), vec![Token::StringLit("A".into())]);
    }

    #[test]
    fn unicode_escape_6() {
        assert_eq!(
            toks(r"'\U01F600'"),
            // U+1F600 = 😀
            vec![Token::StringLit("\u{1F600}".into())]
        );
    }

    #[test]
    fn no_escape_string() {
        assert_eq!(
            toks(r"@'raw\ntext'"),
            vec![Token::StringLit(r"raw\ntext".into())]
        );
    }

    #[test]
    fn unterminated_string_error() {
        assert!(tokenize_bare("'abc").is_err());
    }

    // ── Byte literals ────────────────────────────────────────────────────

    #[test]
    fn byte_literal() {
        assert_eq!(toks("X'4142'"), vec![Token::BytesLit(vec![0x41, 0x42])]);
    }

    #[test]
    fn byte_literal_lowercase() {
        assert_eq!(toks("x'de ad'"), vec![Token::BytesLit(vec![0xDE, 0xAD])]);
    }

    // ── Numeric literals ─────────────────────────────────────────────────

    #[test]
    fn decimal_integer() {
        assert_eq!(toks("42"), vec![Token::Int(42)]);
    }

    #[test]
    fn integer_with_underscores() {
        assert_eq!(toks("1_000_000"), vec![Token::Int(1_000_000)]);
    }

    #[test]
    fn hex_integer() {
        assert_eq!(toks("0xFF"), vec![Token::Int(255)]);
    }

    #[test]
    fn octal_integer() {
        assert_eq!(toks("0o77"), vec![Token::Int(63)]);
    }

    #[test]
    fn binary_integer() {
        assert_eq!(toks("0b1010"), vec![Token::Int(10)]);
    }

    #[test]
    fn float_literal() {
        #[allow(clippy::approx_constant)]
        let expected = vec![Token::Float(3.14)];
        assert_eq!(toks("3.14"), expected);
    }

    #[test]
    fn float_scientific() {
        assert_eq!(toks("1e10"), vec![Token::Float(1e10)]);
    }

    #[test]
    fn float_scientific_negative_exp() {
        assert_eq!(toks("2.5E-3"), vec![Token::Float(2.5e-3)]);
    }

    #[test]
    fn exact_numeric_m_suffix() {
        assert_eq!(toks("42M"), vec![Token::ExactNumeric("42".into())]);
    }

    #[test]
    fn exact_numeric_decimal_m() {
        assert_eq!(toks("3.14M"), vec![Token::ExactNumeric("3.14".into())]);
    }

    #[test]
    fn approximate_f_suffix() {
        assert_eq!(toks("42F"), vec![Token::Float(42.0)]);
    }

    #[test]
    fn approximate_d_suffix() {
        #[allow(clippy::approx_constant)]
        let expected = vec![Token::Float(3.14)];
        assert_eq!(toks("3.14D"), expected);
    }

    #[test]
    fn big_int() {
        let result = toks("99999999999999999999");
        assert!(matches!(&result[0], Token::BigInt(_)));
    }

    #[test]
    fn integer_before_range_dots() {
        assert_eq!(
            toks("1..3"),
            vec![Token::Int(1), Token::RangeDots, Token::Int(3)]
        );
    }

    #[test]
    fn float_and_range_dots_coexist() {
        assert_eq!(
            toks("1.5 1..3"),
            vec![
                Token::Float(1.5),
                Token::Int(1),
                Token::RangeDots,
                Token::Int(3)
            ]
        );
    }

    // ── Parameters ───────────────────────────────────────────────────────

    #[test]
    fn param() {
        assert_eq!(toks("$name"), vec![Token::Param("name".into())]);
    }

    #[test]
    fn substituted_param() {
        assert_eq!(toks("$$name"), vec![Token::SubstitutedParam("name".into())]);
    }

    // ── Comments ─────────────────────────────────────────────────────────

    #[test]
    fn line_comment_slash() {
        assert_eq!(
            toks("a // comment\nb"),
            vec![Token::Ident("a".into()), Token::Ident("b".into())]
        );
    }

    #[test]
    fn line_comment_dash() {
        assert_eq!(
            toks("a -- comment\nb"),
            vec![Token::Ident("a".into()), Token::Ident("b".into())]
        );
    }

    #[test]
    fn block_comment() {
        assert_eq!(
            toks("a /* block */ b"),
            vec![Token::Ident("a".into()), Token::Ident("b".into())]
        );
    }

    #[test]
    fn nested_block_comment() {
        assert_eq!(
            toks("a /* outer /* inner */ still comment */ b"),
            vec![Token::Ident("a".into()), Token::Ident("b".into())]
        );
    }

    #[test]
    fn unterminated_block_comment_errors() {
        let result = tokenize_bare("a /* block");
        assert!(result.is_err());
    }

    #[test]
    fn unterminated_nested_block_comment_errors() {
        let result = tokenize_bare("a /* outer /* inner */");
        assert!(result.is_err());
    }

    // ── Realistic queries ────────────────────────────────────────────────

    #[test]
    fn simple_match_return() {
        // (a:User)-[:KNOWS]->(b)
        // In GQL, `-[` and `]->` are single tokens.
        let tokens = toks("MATCH (a:User)-[:KNOWS]->(b) RETURN a.name, b.name");
        assert_eq!(tokens[0], Token::Ident("MATCH".into()));
        assert_eq!(tokens[1], Token::LParen);
        assert_eq!(tokens[2], Token::Ident("a".into()));
        assert_eq!(tokens[3], Token::Colon);
        assert_eq!(tokens[4], Token::Ident("User".into()));
        assert_eq!(tokens[5], Token::RParen);
        assert_eq!(tokens[6], Token::MinusLeftBracket); // -[
        assert_eq!(tokens[7], Token::Colon);
        assert_eq!(tokens[8], Token::Ident("KNOWS".into()));
        assert_eq!(tokens[9], Token::BracketRightArrow); // ]->
        assert_eq!(tokens[10], Token::LParen);
        assert_eq!(tokens[11], Token::Ident("b".into()));
        assert_eq!(tokens[12], Token::RParen);
    }

    #[test]
    fn match_with_bracket_edge() {
        let tokens = toks("MATCH (a)-[e:KNOWS]->(b)");
        assert_eq!(tokens[0], Token::Ident("MATCH".into()));
        assert_eq!(tokens[1], Token::LParen);
        assert_eq!(tokens[2], Token::Ident("a".into()));
        assert_eq!(tokens[3], Token::RParen);
        assert_eq!(tokens[4], Token::MinusLeftBracket);
        assert_eq!(tokens[5], Token::Ident("e".into()));
        assert_eq!(tokens[6], Token::Colon);
        assert_eq!(tokens[7], Token::Ident("KNOWS".into()));
        assert_eq!(tokens[8], Token::BracketRightArrow);
        assert_eq!(tokens[9], Token::LParen);
        assert_eq!(tokens[10], Token::Ident("b".into()));
        assert_eq!(tokens[11], Token::RParen);
    }

    #[test]
    fn match_undirected_tilde() {
        // (a)~[e]~(b) — `~[` and `]~` are single tokens.
        let tokens = toks("MATCH (a)~[e]~(b)");
        // MATCH ( a ) ~[ e ]~ ( b )
        // 0     1 2 3 4  5 6  7 8 9
        assert_eq!(tokens[4], Token::TildeLeftBracket);
        assert_eq!(tokens[6], Token::RightBracketTilde);
    }

    // ── Spans ────────────────────────────────────────────────────────────

    #[test]
    fn spans_are_correct() {
        let spanned = tokenize("MATCH (a)").unwrap();
        assert_eq!(spanned[0].span, Span { start: 0, end: 5 });
        assert_eq!(spanned[0].token, Token::Ident("MATCH".into()));
        assert_eq!(spanned[1].span, Span { start: 6, end: 7 });
        assert_eq!(spanned[1].token, Token::LParen);
        assert_eq!(spanned[2].span, Span { start: 7, end: 8 });
        assert_eq!(spanned[2].token, Token::Ident("a".into()));
        assert_eq!(spanned[3].span, Span { start: 8, end: 9 });
        assert_eq!(spanned[3].token, Token::RParen);
    }

    // ── Edge case: `-` vs `--` comment ───────────────────────────────────

    #[test]
    fn minus_not_confused_with_comment() {
        // Single minus is Token::Minus.
        assert_eq!(
            toks("a - b"),
            vec![
                Token::Ident("a".into()),
                Token::Minus,
                Token::Ident("b".into()),
            ]
        );
    }

    #[test]
    fn f_suffix_not_confused_with_ident() {
        // `42FROM` should be Int(42) + Ident("FROM"), not Float.
        assert_eq!(
            toks("42FROM"),
            vec![Token::Int(42), Token::Ident("FROM".into()),]
        );
    }

    #[test]
    fn at_sign_standalone() {
        assert_eq!(toks("@"), vec![Token::At]);
    }

    // ── Uncovered lexer paths ────────────────────────────────────────────

    #[test]
    fn multibyte_utf8_in_ident() {
        // Lines 128-133, 966-967 — multi-byte UTF-8 characters in identifiers
        let tokens = toks("café");
        assert_eq!(tokens, vec![Token::Ident("café".into())]);
    }

    #[test]
    fn unexpected_character() {
        // Line 306
        let result = tokenize_bare("§");
        assert!(result.is_err());
    }

    #[test]
    fn dollar_empty_name() {
        // Line 526 — $ with no following identifier
        let result = tokenize_bare("$");
        assert!(result.is_err());
    }

    #[test]
    fn double_dollar_empty_name() {
        // Line 518 — $$ with no following identifier
        let result = tokenize_bare("$$");
        assert!(result.is_err());
    }

    #[test]
    fn unterminated_double_quoted() {
        // Line 576
        let result = tokenize_bare("\"unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn escape_in_double_quoted() {
        // Lines 591-593 — escape sequences in double-quoted identifiers
        let tokens = toks("\"hello\\nworld\"");
        assert_eq!(tokens, vec![Token::QuotedIdent("hello\nworld".into())]);
    }

    #[test]
    fn unterminated_backtick() {
        // Line 610
        let result = tokenize_bare("`unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn doubled_backtick_inside() {
        // Lines 616-618 — doubled backtick produces single backtick
        let tokens = toks("`he``llo`");
        assert_eq!(tokens, vec![Token::QuotedIdent("he`llo".into())]);
    }

    #[test]
    fn escape_in_backtick() {
        // Lines 625-627 — escape sequences in backtick identifiers
        let tokens = toks("`col\\tname`");
        assert_eq!(tokens, vec![Token::QuotedIdent("col\tname".into())]);
    }

    #[test]
    fn unterminated_no_escape_literal() {
        // Line 645
        let result = tokenize_bare("@'unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn no_escape_doubled_delimiter() {
        // Lines 650-652 — doubled delimiter in no-escape string
        let tokens = toks("@'it''s'");
        assert_eq!(tokens, vec![Token::StringLit("it's".into())]);
    }

    #[test]
    fn no_escape_double_quoted_ident() {
        // Line 657 — no-escape with " delimiter → QuotedIdent
        let tokens = toks("@\"raw\"");
        assert_eq!(tokens, vec![Token::QuotedIdent("raw".into())]);
    }

    #[test]
    fn escape_backslash() {
        // Line 677 — \\ escape
        let tokens = toks(r"'\\'");
        assert_eq!(tokens, vec![Token::StringLit("\\".into())]);
    }

    #[test]
    fn escape_double_quote_in_string() {
        // Line 679 — \" escape in single-quoted string
        let tokens = toks(r#"'\"'"#);
        assert_eq!(tokens, vec![Token::StringLit("\"".into())]);
    }

    #[test]
    fn escape_backtick_in_string() {
        // Line 680 — \` escape in single-quoted string
        let tokens = toks(r"'\`'");
        assert_eq!(tokens, vec![Token::StringLit("`".into())]);
    }

    #[test]
    fn escape_carriage_return() {
        // Line 682 — \r escape
        let tokens = toks(r"'\r'");
        assert_eq!(tokens, vec![Token::StringLit("\r".into())]);
    }

    #[test]
    fn escape_backspace_formfeed() {
        // Lines 684-685 — \b and \f escape sequences
        let tokens = toks(r"'\b\f'");
        assert_eq!(tokens, vec![Token::StringLit("\u{08}\u{0C}".into())]);
    }

    #[test]
    fn escape_tab() {
        let tokens = toks(r"'\t'");
        assert_eq!(tokens, vec![Token::StringLit("\t".into())]);
    }

    #[test]
    fn escape_single_quote_in_string() {
        // Line 678 — \' escape
        let tokens = toks(r"'\''");
        assert_eq!(tokens, vec![Token::StringLit("'".into())]);
    }

    #[test]
    fn unknown_escape_error() {
        // Line 697
        let result = tokenize_bare("'\\z'");
        assert!(result.is_err());
    }

    #[test]
    fn unterminated_escape_at_eof() {
        // Line 673
        let result = tokenize_bare("'\\");
        assert!(result.is_err());
    }

    #[test]
    fn hex_escape_too_short() {
        // Line 703
        let result = tokenize_bare("'\\u");
        assert!(result.is_err());
    }

    #[test]
    fn unterminated_byte_literal() {
        // Line 726
        let result = tokenize_bare("X'");
        assert!(result.is_err());
    }

    #[test]
    fn byte_literal_with_spaces() {
        // Lines 739 — spaces between hex digits in byte literal
        let tokens = toks("X'A B'");
        assert_eq!(tokens, vec![Token::BytesLit(vec![0xAB])]);
    }

    #[test]
    fn byte_literal_space_then_eof() {
        // Line 742 — unterminated byte literal: space between digits then EOF
        let result = tokenize_bare("X'A ");
        assert!(result.is_err());
    }

    #[test]
    fn empty_hex_int() {
        // Line 771
        let result = tokenize_bare("0x");
        assert!(result.is_err());
    }

    #[test]
    fn empty_octal_int() {
        // Line 787
        let result = tokenize_bare("0o");
        assert!(result.is_err());
    }

    #[test]
    fn empty_binary_int() {
        // Line 803
        let result = tokenize_bare("0b");
        assert!(result.is_err());
    }

    #[test]
    fn exponent_backtrack() {
        // Lines 839-841 — 1e not followed by digit backtracks to plain int
        let tokens = toks("1e");
        // Should parse as Int(1) followed by Ident("e")
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], Token::Int(1));
    }

    #[test]
    fn double_suffix() {
        // Line 862 — D suffix for FLOAT64 literal
        let tokens = toks("42D");
        assert_eq!(tokens, vec![Token::Float(42.0)]);
    }

    #[test]
    fn d_followed_by_ident_is_not_suffix() {
        // Line 865 — D followed by ident char is NOT a suffix
        let tokens = toks("42Dfoo");
        // Should parse as Int(42) followed by Ident("Dfoo")
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], Token::Int(42));
    }

    #[test]
    fn input_ending_with_whitespace() {
        // Lines 65, 81 — early exits when input ends with whitespace
        let tokens = toks("1  ");
        assert_eq!(tokens, vec![Token::Int(1)]);
    }

    #[test]
    fn input_ending_with_comment() {
        // Line 65 — input ending with a comment
        let tokens = toks("1 // trailing comment");
        assert_eq!(tokens, vec![Token::Int(1)]);
    }

    // ── Comment preservation tests ──────────────────────────────────────

    #[test]
    fn comment_preservation_line_double_slash() {
        let result = tokenize_with_comments("// find people\nMATCH (n)").unwrap();
        assert_eq!(result.comments.len(), 1);
        let c = &result.comments[0];
        assert_eq!(c.kind, CommentKind::Line);
        assert_eq!(c.text, " find people");
        assert_eq!(c.span, Span { start: 0, end: 14 });
        // Tokens should still parse normally.
        assert_eq!(result.tokens.len(), 4); // MATCH ( n )
    }

    #[test]
    fn comment_preservation_line_double_dash() {
        let result = tokenize_with_comments("-- a note\n1").unwrap();
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.comments[0].kind, CommentKind::Line);
        assert_eq!(result.comments[0].text, " a note");
    }

    #[test]
    fn comment_preservation_block() {
        let result = tokenize_with_comments("/* block */ 42").unwrap();
        assert_eq!(result.comments.len(), 1);
        let c = &result.comments[0];
        assert_eq!(c.kind, CommentKind::Block);
        assert_eq!(c.text, " block ");
        assert_eq!(c.span, Span { start: 0, end: 11 });
    }

    #[test]
    fn comment_preservation_nested_block() {
        let result = tokenize_with_comments("/* outer /* inner */ end */ 1").unwrap();
        assert_eq!(result.comments.len(), 1);
        let c = &result.comments[0];
        assert_eq!(c.kind, CommentKind::Block);
        assert_eq!(c.text, " outer /* inner */ end ");
    }

    #[test]
    fn multiple_comments() {
        let result = tokenize_with_comments("// first\n/* second */ MATCH").unwrap();
        assert_eq!(result.comments.len(), 2);
        assert_eq!(result.comments[0].kind, CommentKind::Line);
        assert_eq!(result.comments[0].text, " first");
        assert_eq!(result.comments[1].kind, CommentKind::Block);
        assert_eq!(result.comments[1].text, " second ");
    }

    #[test]
    fn trailing_line_comment() {
        let result = tokenize_with_comments("1 // trailing").unwrap();
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.comments[0].text, " trailing");
        assert_eq!(result.tokens.len(), 1);
    }

    #[test]
    fn no_comments() {
        let result = tokenize_with_comments("MATCH (n) RETURN n").unwrap();
        assert!(result.comments.is_empty());
    }
}
