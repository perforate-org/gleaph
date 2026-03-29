use gleaph_types::GleaphError;
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{escaped_transform, is_not, tag, take_while},
    character::complete::{char, satisfy},
    combinator::{map, opt, value},
    sequence::{delimited, pair},
};

/// A single lexical token produced by [`tokenize`].
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// An identifier or keyword (e.g. `MATCH`, `User`, `a`).
    Ident(String),
    /// A backtick-quoted identifier (e.g. `` `min` ``, `` `my prop` ``).
    /// Never treated as a keyword, allowing reserved words to be used as identifiers.
    QuotedIdent(String),
    /// A signed 64-bit integer literal.
    Int(i64),
    /// An integer literal too large for i64 (stored as raw string for parser to promote).
    BigInt(String),
    /// A 64-bit floating-point literal.
    Float(f64),
    /// A single-quoted string literal (GQL/SQL standard). Escape sequences resolved.
    String(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    RangeDots,
    Star,
    Minus,
    Plus,
    Slash,
    Percent,
    Pipe2,
    Pipe,
    Ampersand,
    Bang,
    /// `->` edge arrow.
    ArrowRight,
    /// `<-` edge arrow.
    ArrowLeft,
    /// A query parameter reference: `$name`.
    Param(String),
    /// `~` undirected edge connector (GQL standard).
    Tilde,
    /// A binary literal: `X'4142'` → raw bytes.
    Bytes(Vec<u8>),
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Converts a GQL query string into a flat list of [`Token`]s.
///
/// Returns [`GleaphError::ParseError`] on unrecognised characters or malformed
/// literals, and [`GleaphError::UnsupportedFeature`] for syntax that is
/// intentionally unsupported syntax that is not yet part of the phase subset.
pub fn tokenize(input: &str) -> Result<Vec<Token>, GleaphError> {
    let mut rest = input;
    let mut tokens = Vec::new();

    while !rest.is_empty() {
        rest = trim_ascii_whitespace(rest);
        if rest.is_empty() {
            break;
        }

        let bytes = rest.as_bytes();
        let parsed = match bytes[0] {
            b'-' if bytes.get(1) == Some(&b'>') => Ok((&rest[2..], Token::ArrowRight)),
            b'<' if bytes.get(1) == Some(&b'-') => Ok((&rest[2..], Token::ArrowLeft)),
            b'.' if bytes.get(1) == Some(&b'.') => Ok((&rest[2..], Token::RangeDots)),
            b'<' if bytes.get(1) == Some(&b'>') => Ok((&rest[2..], Token::Ne)),
            b'<' if bytes.get(1) == Some(&b'=') => Ok((&rest[2..], Token::Le)),
            b'>' if bytes.get(1) == Some(&b'=') => Ok((&rest[2..], Token::Ge)),
            b'|' if bytes.get(1) == Some(&b'|') => Ok((&rest[2..], Token::Pipe2)),
            b'&' => Ok((&rest[1..], Token::Ampersand)),
            b'|' => Ok((&rest[1..], Token::Pipe)),
            b'(' => Ok((&rest[1..], Token::LParen)),
            b')' => Ok((&rest[1..], Token::RParen)),
            b'[' => Ok((&rest[1..], Token::LBracket)),
            b']' => Ok((&rest[1..], Token::RBracket)),
            b'{' => Ok((&rest[1..], Token::LBrace)),
            b'}' => Ok((&rest[1..], Token::RBrace)),
            b':' => Ok((&rest[1..], Token::Colon)),
            b',' => Ok((&rest[1..], Token::Comma)),
            b'.' => Ok((&rest[1..], Token::Dot)),
            b'*' => Ok((&rest[1..], Token::Star)),
            b'-' => Ok((&rest[1..], Token::Minus)),
            b'+' => Ok((&rest[1..], Token::Plus)),
            b'/' => Ok((&rest[1..], Token::Slash)),
            b'%' => Ok((&rest[1..], Token::Percent)),
            b'=' => Ok((&rest[1..], Token::Eq)),
            b'<' => Ok((&rest[1..], Token::Lt)),
            b'>' => Ok((&rest[1..], Token::Gt)),
            b'!' => Ok((&rest[1..], Token::Bang)),
            b'~' => Ok((&rest[1..], Token::Tilde)),
            b'X' | b'x' if bytes.get(1) == Some(&b'\'') => lex_bytes_literal(rest),
            b'\'' => lex_string_single(rest),
            b'"' => lex_double_quoted_ident(rest),
            b'`' => lex_backtick_ident(rest),
            b'0'..=b'9' => lex_number(rest),
            b'$' => lex_param(rest),
            _ if is_ident_start(bytes[0] as char) => lex_ident(rest),
            _ => Err(nom::Err::Error(nom::error::Error::new(
                rest,
                nom::error::ErrorKind::Tag,
            ))),
        };

        match parsed {
            Ok((next, token)) => {
                tokens.push(token);
                rest = next;
            }
            Err(nom::Err::Error(e)) | Err(nom::Err::Failure(e)) => {
                let got = e.input.chars().next().unwrap_or('\0');
                if got == '\0' {
                    return Err(GleaphError::ParseError("unexpected end of input".into()));
                }
                return Err(GleaphError::ParseError(format!(
                    "unexpected character: {got}"
                )));
            }
            Err(nom::Err::Incomplete(_)) => {
                return Err(GleaphError::ParseError("incomplete input".into()));
            }
        }
    }

    Ok(tokens)
}

fn trim_ascii_whitespace(input: &str) -> &str {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\n' | b'\r' | b'\t') {
        i += 1;
    }
    &input[i..]
}

#[allow(dead_code)]
fn lex_token(input: &str) -> IResult<&str, Token> {
    alt((
        alt((
            value(Token::ArrowRight, tag("->")),
            value(Token::ArrowLeft, tag("<-")),
            value(Token::RangeDots, tag("..")),
            value(Token::Ne, tag("<>")),
            value(Token::Le, tag("<=")),
            value(Token::Ge, tag(">=")),
            value(Token::Pipe2, tag("||")),
            value(Token::Ampersand, tag("&")),
            value(Token::Pipe, tag("|")),
            value(Token::LParen, tag("(")),
            value(Token::RParen, tag(")")),
            value(Token::LBracket, tag("[")),
            value(Token::RBracket, tag("]")),
            value(Token::LBrace, tag("{")),
            value(Token::RBrace, tag("}")),
        )),
        alt((
            value(Token::Colon, tag(":")),
            value(Token::Comma, tag(",")),
            value(Token::Dot, tag(".")),
            value(Token::Star, tag("*")),
            value(Token::Minus, tag("-")),
            value(Token::Plus, tag("+")),
            value(Token::Slash, tag("/")),
            value(Token::Percent, tag("%")),
            value(Token::Eq, tag("=")),
            value(Token::Lt, tag("<")),
            value(Token::Gt, tag(">")),
            value(Token::Bang, tag("!")),
            value(Token::Tilde, tag("~")),
        )),
        lex_bytes_literal,
        lex_string_single,
        lex_double_quoted_ident,
        lex_number,
        lex_param,
        lex_backtick_ident,
        lex_ident,
    ))
    .parse(input)
}

/// Lexes a double-quoted identifier: `"my col"` → `Token::QuotedIdent("my col")`.
/// GQL/SQL standard: double quotes delimit identifiers, not strings.
fn lex_double_quoted_ident(input: &str) -> IResult<&str, Token> {
    map(
        delimited(
            char('"'),
            map(
                opt(escaped_transform(
                    is_not("\\\""),
                    '\\',
                    alt((
                        value("\\", char('\\')),
                        value("\"", char('"')),
                        value("\n", char('n')),
                        value("\t", char('t')),
                        value("\r", char('r')),
                    )),
                )),
                |s| s.unwrap_or_default(),
            ),
            char('"'),
        ),
        Token::QuotedIdent,
    )
    .parse(input)
}

/// Lexes a single-quoted string literal: `'hello'` → `Token::String("hello")`.
/// Same escape sequences as double-quoted strings (`\\`, `\'`, `\n`, `\t`, `\r`).
fn lex_string_single(input: &str) -> IResult<&str, Token> {
    map(
        delimited(
            char('\''),
            map(
                opt(escaped_transform(
                    is_not("\\'"),
                    '\\',
                    alt((
                        value("\\", char('\\')),
                        value("'", char('\'')),
                        value("\n", char('n')),
                        value("\t", char('t')),
                        value("\r", char('r')),
                    )),
                )),
                |s| s.unwrap_or_default(),
            ),
            char('\''),
        ),
        Token::String,
    )
    .parse(input)
}

/// Lexes a binary literal: `X'4142'` → `Token::Bytes(vec![0x41, 0x42])`.
/// The hex string must have an even number of characters.
fn lex_bytes_literal(input: &str) -> IResult<&str, Token> {
    let bytes = input.as_bytes();
    if bytes.len() < 3 || (bytes[0] != b'X' && bytes[0] != b'x') || bytes[1] != b'\'' {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Tag,
        )));
    }
    let rest = &input[2..];
    let end = rest.find('\'').ok_or_else(|| {
        nom::Err::Failure(nom::error::Error::new(input, nom::error::ErrorKind::Tag))
    })?;
    let hex_str = &rest[..end];
    if !hex_str.len().is_multiple_of(2) {
        return Err(nom::Err::Failure(nom::error::Error::new(
            input,
            nom::error::ErrorKind::HexDigit,
        )));
    }
    let mut result = Vec::with_capacity(hex_str.len() / 2);
    let hex_bytes = hex_str.as_bytes();
    let mut i = 0;
    while i < hex_bytes.len() {
        let hi = hex_digit(hex_bytes[i]).ok_or_else(|| {
            nom::Err::Failure(nom::error::Error::new(
                input,
                nom::error::ErrorKind::HexDigit,
            ))
        })?;
        let lo = hex_digit(hex_bytes[i + 1]).ok_or_else(|| {
            nom::Err::Failure(nom::error::Error::new(
                input,
                nom::error::ErrorKind::HexDigit,
            ))
        })?;
        result.push(hi << 4 | lo);
        i += 2;
    }
    Ok((&rest[end + 1..], Token::Bytes(result)))
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Lexes a backtick-quoted identifier: `` `min` `` → `Token::QuotedIdent("min")`.
/// Any characters except backtick are allowed inside. Backtick itself is not escapable.
fn lex_backtick_ident(input: &str) -> IResult<&str, Token> {
    map(
        delimited(char('`'), take_while(|c| c != '`'), char('`')),
        |s: &str| Token::QuotedIdent(s.to_string()),
    )
    .parse(input)
}

fn lex_number(input: &str) -> IResult<&str, Token> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Digit,
        )));
    }

    // Hex literal: 0x... or 0X...
    if bytes[0] == b'0' && bytes.len() > 1 && (bytes[1] == b'x' || bytes[1] == b'X') {
        let mut i = 2;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i == 2 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                input,
                nom::error::ErrorKind::Digit,
            )));
        }
        let hex_str = &input[2..i];
        return match i64::from_str_radix(hex_str, 16) {
            Ok(v) => Ok((&input[i..], Token::Int(v))),
            Err(_) => Ok((&input[i..], Token::BigInt(input[..i].to_string()))),
        };
    }

    // Octal literal: 0o... or 0O...
    if bytes[0] == b'0' && bytes.len() > 1 && (bytes[1] == b'o' || bytes[1] == b'O') {
        let mut i = 2;
        while i < bytes.len() && matches!(bytes[i], b'0'..=b'7') {
            i += 1;
        }
        if i == 2 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                input,
                nom::error::ErrorKind::Digit,
            )));
        }
        let oct_str = &input[2..i];
        return match i64::from_str_radix(oct_str, 8) {
            Ok(v) => Ok((&input[i..], Token::Int(v))),
            Err(_) => Ok((&input[i..], Token::BigInt(input[..i].to_string()))),
        };
    }

    // Binary literal: 0b... or 0B...
    if bytes[0] == b'0' && bytes.len() > 1 && (bytes[1] == b'b' || bytes[1] == b'B') {
        let mut i = 2;
        while i < bytes.len() && (bytes[i] == b'0' || bytes[i] == b'1') {
            i += 1;
        }
        if i == 2 {
            return Err(nom::Err::Failure(nom::error::Error::new(
                input,
                nom::error::ErrorKind::Digit,
            )));
        }
        let bin_str = &input[2..i];
        return match i64::from_str_radix(bin_str, 2) {
            Ok(v) => Ok((&input[i..], Token::Int(v))),
            Err(_) => Ok((&input[i..], Token::BigInt(input[..i].to_string()))),
        };
    }

    let mut i = 1usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }

    let mut has_fraction = false;
    if i < bytes.len() && bytes[i] == b'.' {
        // `..` belongs to path range syntax, not a float literal.
        if i + 1 < bytes.len() && bytes[i + 1] == b'.' {
            return match input[..i].parse::<i64>() {
                Ok(n) => Ok((&input[i..], Token::Int(n))),
                Err(_) => Ok((&input[i..], Token::BigInt(input[..i].to_string()))),
            };
        }
        has_fraction = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }

    // Scientific notation: e/E followed by optional +/- and digits
    let mut has_exponent = false;
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let j = i + 1;
        let k = if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j + 1
        } else {
            j
        };
        if k < bytes.len() && bytes[k].is_ascii_digit() {
            has_exponent = true;
            i = k;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }

    let num = &input[..i];
    let rest = &input[i..];
    if has_fraction || has_exponent {
        let v = num.parse::<f64>().map_err(|_| {
            nom::Err::Failure(nom::error::Error::new(input, nom::error::ErrorKind::Float))
        })?;
        Ok((rest, Token::Float(v)))
    } else {
        match num.parse::<i64>() {
            Ok(v) => Ok((rest, Token::Int(v))),
            Err(_) => Ok((rest, Token::BigInt(num.to_string()))),
        }
    }
}

fn lex_param(input: &str) -> IResult<&str, Token> {
    // `$name` → Token::Param("name")
    let (rest, _) = char('$')(input)?;
    let (rest, name) =
        nom::combinator::recognize(pair(satisfy(is_ident_start), take_while(is_ident_continue)))
            .parse(rest)?;
    Ok((rest, Token::Param(name.to_string())))
}

fn lex_ident(input: &str) -> IResult<&str, Token> {
    let (rest, ident) =
        nom::combinator::recognize(pair(satisfy(is_ident_start), take_while(is_ident_continue)))
            .parse(input)?;
    Ok((rest, Token::Ident(ident.to_string())))
}

/// Returns `true` if `c` is a valid first character of an identifier.
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Returns `true` if `c` may appear after the first character of an identifier.
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Returns `true` if `s` is a GQL reserved keyword (case-insensitive).
pub fn is_reserved_keyword(s: &str) -> bool {
    matches!(
        s.to_ascii_uppercase().as_str(),
        "MATCH"
            | "WHERE"
            | "RETURN"
            | "ORDER"
            | "BY"
            | "LIMIT"
            | "CREATE"
            | "DELETE"
            | "SET"
            | "REMOVE"
            | "OPTIONAL"
            | "OR"
            | "NOT"
            | "XOR"
            | "IN"
            | "IS"
            | "DISTINCT"
            | "CASE"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "DETACH"
            | "EXISTS"
            | "WITH"
            | "COUNT"
            | "SUM"
            | "AVG"
            | "MIN"
            | "MAX"
            | "COLLECT"
            | "COLLECT_LIST"
            | "MERGE"
            | "COALESCE"
            | "NULLIF"
            | "GROUP"
            | "HAVING"
            | "OFFSET"
            | "UNION"
            | "ALL"
            | "EXCEPT"
            | "INTERSECT"
            | "INSERT"
            | "LABELS"
            | "PROPERTIES"
            | "TYPE"
            | "ID"
            | "UPPER"
            | "LOWER"
            | "TRIM"
            | "SUBSTRING"
            | "SIZE"
            | "ABS"
            | "FLOOR"
            | "CEIL"
            | "TOSTRING"
            | "TOINTEGER"
            | "TOFLOAT"
            | "ANY"
            | "SHORTEST"
            | "PATH"
            | "PATHS"
            | "AND"
            | "AS"
            | "ASC"
            | "DESC"
            | "TRUE"
            | "FALSE"
            | "NULL"
            | "NO"
            | "BINDINGS"
            | "OTHERWISE"
            | "FINISH"
            | "CAST"
            | "LABELED"
            | "SOURCE"
            | "DESTINATION"
            | "FILTER"
            | "LET"
            | "DIRECTED"
            | "UNKNOWN"
            | "PROPERTY_EXISTS"
            | "ALL_DIFFERENT"
            | "SAME"
            | "ELEMENT_ID"
            | "WALK"
            | "TRAIL"
            | "SIMPLE"
            | "ACYCLIC"
            | "FOR"
            | "ORDINALITY"
            | "NEXT"
            | "YIELD"
            | "SELECT"
            | "CALL"
            | "USE"
            | "GRAPH"
            | "DROP"
            | "SCHEMA"
            | "NONE"
            | "KEEP"
            | "DATE"
            | "TIME"
            | "DATETIME"
            | "DURATION"
            | "DESCRIBE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unterminated_string_literal() {
        let err = tokenize("MATCH (a)-[:X]->(b) WHERE a.name = 'abc").unwrap_err();
        assert!(matches!(err, GleaphError::ParseError(_)));
    }

    #[test]
    fn double_quotes_produce_quoted_ident() {
        let tokens = tokenize(r#""my col""#).unwrap();
        assert_eq!(tokens, vec![Token::QuotedIdent("my col".into())]);
    }

    #[test]
    fn single_quotes_produce_string() {
        let tokens = tokenize("'hello'").unwrap();
        assert_eq!(tokens, vec![Token::String("hello".into())]);
    }

    #[test]
    fn tokenizes_new_punctuation_variants() {
        let tokens = tokenize("a+b/c%2 || !x").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Ident("a".into()),
                Token::Plus,
                Token::Ident("b".into()),
                Token::Slash,
                Token::Ident("c".into()),
                Token::Percent,
                Token::Int(2),
                Token::Pipe2,
                Token::Bang,
                Token::Ident("x".into()),
            ]
        );
    }

    #[test]
    fn tokenizes_float_without_breaking_range_dots() {
        let tokens = tokenize("1.5 1..3").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Float(1.5),
                Token::Int(1),
                Token::RangeDots,
                Token::Int(3)
            ]
        );
    }

    #[test]
    fn tokenizes_bytes_literal() {
        let tokens = tokenize("X'4142'").unwrap();
        assert_eq!(tokens, vec![Token::Bytes(vec![0x41, 0x42])]);
    }

    #[test]
    fn tokenizes_bytes_in_property_map() {
        let tokens = tokenize("{payload: X'DEAD'}").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::LBrace,
                Token::Ident("payload".into()),
                Token::Colon,
                Token::Bytes(vec![0xDE, 0xAD]),
                Token::RBrace,
            ]
        );
    }
}
