//! Gleaph extension DDL: `CREATE EDGE LABEL ... { <property> <scalar> INLINE }` (ADR 0034 Slice 20).
//!
//! A Router-owned parser for the standalone scalar inline edge-property schema statement.
//! Non-INLINE statements continue through the generic GQL parser unchanged.

use crate::facade::stable::edge_payload_profiles::InlineScalarType;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InlineEdgeScalarSchema {
    pub edge_label: String,
    pub property: String,
    pub scalar_type: InlineScalarType,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EdgePayloadDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
    #[error("unrecognised scalar type `{0}`")]
    UnrecognisedScalarType(String),
    #[error("CREATE EDGE LABEL ... INLINE accepts exactly one property declaration")]
    MultipleFields,
}

/// Returns `None` when the query is not the scalar inline edge-label DDL shape.
pub(crate) fn try_parse(
    query: &str,
) -> Option<Result<InlineEdgeScalarSchema, EdgePayloadDdlParseError>> {
    let trimmed = query.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("CREATE EDGE LABEL") {
        Some(parse(trimmed))
    } else {
        None
    }
}

fn parse(query: &str) -> Result<InlineEdgeScalarSchema, EdgePayloadDdlParseError> {
    let mut cur = Cursor::new(query);
    cur.skip_ws();
    cur.expect_ascii_ci("CREATE")?;
    cur.expect_ascii_ci("EDGE")?;
    cur.expect_ascii_ci("LABEL")?;
    let edge_label = cur.parse_ident()?;
    cur.skip_ws();
    cur.expect('{')?;
    cur.skip_ws();

    let property = cur.parse_ident()?;
    cur.skip_ws();
    let scalar_name = cur.parse_ident()?;
    cur.skip_ws();
    cur.expect_ascii_ci("INLINE")?;
    cur.skip_ws();

    // Reject a second field / comma.
    if cur.try_consume(',') {
        return Err(EdgePayloadDdlParseError::MultipleFields);
    }
    if cur
        .peek()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
    {
        // A second identifier means a second field declaration.
        return Err(EdgePayloadDdlParseError::MultipleFields);
    }

    cur.skip_ws();
    cur.expect('}')?;
    cur.skip_ws();
    cur.try_consume(';');
    cur.skip_ws();
    if !cur.is_eof() {
        return Err(EdgePayloadDdlParseError::TrailingInput);
    }

    let scalar_type = InlineScalarType::from_ddl_name(&scalar_name).ok_or(
        EdgePayloadDdlParseError::UnrecognisedScalarType(scalar_name),
    )?;

    Ok(InlineEdgeScalarSchema {
        edge_label,
        property,
        scalar_type,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<char> {
        if self.is_eof() {
            None
        } else {
            Some(self.bytes[self.pos] as char)
        }
    }

    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn consume_ascii_ci(&mut self, word: &str) -> bool {
        self.skip_ws();
        let word_bytes = word.as_bytes();
        if self.bytes[self.pos..].len() < word_bytes.len() {
            return false;
        }
        for (i, b) in word_bytes.iter().enumerate() {
            if !self.bytes[self.pos + i].eq_ignore_ascii_case(b) {
                return false;
            }
        }
        let next = self.pos + word_bytes.len();
        if next < self.bytes.len() {
            let tail = self.bytes[next] as char;
            if tail.is_ascii_alphanumeric() || tail == '_' {
                return false;
            }
        }
        self.pos = next;
        true
    }

    fn expect_ascii_ci(&mut self, word: &str) -> Result<(), EdgePayloadDdlParseError> {
        if self.consume_ascii_ci(word) {
            Ok(())
        } else {
            Err(EdgePayloadDdlParseError::Expected(word.to_string()))
        }
    }

    fn try_consume(&mut self, ch: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, ch: char) -> Result<(), EdgePayloadDdlParseError> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(EdgePayloadDdlParseError::Expected(ch.to_string()))
        }
    }

    fn parse_ident(&mut self) -> Result<String, EdgePayloadDdlParseError> {
        self.skip_ws();
        let start = self.pos;
        let first = self
            .peek()
            .ok_or_else(|| EdgePayloadDdlParseError::Expected("identifier".into()))?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return Err(EdgePayloadDdlParseError::Expected("identifier".into()));
        }
        self.pos += 1;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| EdgePayloadDdlParseError::Expected("identifier".into()))?;
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(query: &str) -> InlineEdgeScalarSchema {
        parse(query).expect("parse")
    }

    #[test]
    fn float32_inline() {
        assert_eq!(
            parse_ok("CREATE EDGE LABEL ROAD { distance FLOAT32 INLINE }"),
            InlineEdgeScalarSchema {
                edge_label: "ROAD".into(),
                property: "distance".into(),
                scalar_type: InlineScalarType::F32,
            }
        );
    }

    #[test]
    fn case_insensitive_keywords_and_type() {
        assert_eq!(
            parse_ok("create edge label road { distance float32 inline }"),
            InlineEdgeScalarSchema {
                edge_label: "road".into(),
                property: "distance".into(),
                scalar_type: InlineScalarType::F32,
            }
        );
    }

    #[test]
    fn all_scalar_types_parse() {
        for (type_name, expected) in [
            ("UINT8", InlineScalarType::U8),
            ("UINT16", InlineScalarType::U16),
            ("UINT32", InlineScalarType::U32),
            ("UINT64", InlineScalarType::U64),
            ("INT8", InlineScalarType::I8),
            ("INT16", InlineScalarType::I16),
            ("INT32", InlineScalarType::I32),
            ("INT64", InlineScalarType::I64),
            ("UINT128", InlineScalarType::U128),
            ("INT128", InlineScalarType::I128),
            ("FLOAT16", InlineScalarType::F16),
            ("FLOAT32", InlineScalarType::F32),
            ("FLOAT64", InlineScalarType::F64),
            ("FIXED32", InlineScalarType::Fixed32),
            ("FIXED64", InlineScalarType::Fixed64),
        ] {
            let q = format!("CREATE EDGE LABEL L {{ p {type_name} INLINE }}");
            let got = parse_ok(&q);
            assert_eq!(
                got.scalar_type, expected,
                "{type_name} should parse to {expected:?}"
            );
        }
    }

    #[test]
    fn trailing_semicolon_accepted() {
        assert!(try_parse("CREATE EDGE LABEL ROAD { distance FLOAT32 INLINE };").is_some());
    }

    #[test]
    fn non_edge_payload_ddl_returns_none() {
        assert!(try_parse("MATCH (n) RETURN n").is_none());
        assert!(try_parse("CREATE INDEX x FOR (n:N) ON (n.p)").is_none());
        assert!(try_parse("CREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE").is_none());
    }

    #[test]
    fn missing_inline_rejected() {
        let err = parse("CREATE EDGE LABEL ROAD { distance FLOAT32 }").unwrap_err();
        assert!(matches!(err, EdgePayloadDdlParseError::Expected(_)));
    }

    #[test]
    fn multiple_fields_rejected() {
        let err = parse("CREATE EDGE LABEL ROAD { distance FLOAT32 INLINE, time UINT32 INLINE }")
            .unwrap_err();
        assert_eq!(err, EdgePayloadDdlParseError::MultipleFields);
    }

    #[test]
    fn unknown_type_rejected() {
        let err = parse("CREATE EDGE LABEL ROAD { distance FOO INLINE }").unwrap_err();
        assert_eq!(
            err,
            EdgePayloadDdlParseError::UnrecognisedScalarType("FOO".into())
        );
    }

    #[test]
    fn trailing_statement_rejected() {
        let err = parse(
            "CREATE EDGE LABEL ROAD { distance FLOAT32 INLINE } CREATE EDGE LABEL X { y UINT8 INLINE }"
        )
        .unwrap_err();
        assert_eq!(err, EdgePayloadDdlParseError::TrailingInput);
    }

    #[test]
    fn missing_braces_rejected() {
        let err = parse("CREATE EDGE LABEL ROAD distance FLOAT32 INLINE").unwrap_err();
        assert!(matches!(err, EdgePayloadDdlParseError::Expected(_)));
    }

    #[test]
    fn empty_body_rejected() {
        let err = parse("CREATE EDGE LABEL ROAD {}").unwrap_err();
        assert!(matches!(err, EdgePayloadDdlParseError::Expected(_)));
    }
}
