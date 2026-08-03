//! Gleaph-specific extension DDL: `CREATE INDEX` / `DROP INDEX` (ADR 0009 §4, ADR 0012).
//!
//! This crate is the single parser owner for the vendor index syntax. It intentionally does not
//! extend the general-purpose `gleaph-gql` grammar; Router and migration tooling consume the
//! parsed, Gleaph-specific statement through this boundary.

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::index::IndexedPropertyKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexDdlStatement {
    Create {
        index_name: String,
        if_not_exists: bool,
        target: IndexTarget,
    },
    Drop {
        index_name: String,
        if_exists: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexTarget {
    pub kind: IndexedPropertyKind,
    pub label: String,
    pub property: String,
    pub edge_direction: Option<EdgeDirection>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IndexDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
    #[error(
        "edge FOR pattern must use bracket form -[var:Label]-; slash form is not supported in CREATE INDEX"
    )]
    SlashEdgePatternNotSupported,
}

/// Returns `None` when the query is not index DDL (caller should use standard GQL parsing).
pub fn try_parse(query: &str) -> Option<Result<IndexDdlStatement, IndexDdlParseError>> {
    let raw_trimmed = query.trim();
    let raw_upper = raw_trimmed.to_ascii_uppercase();
    if raw_upper.starts_with("CREATE INDEX") || raw_upper.starts_with("DROP INDEX") {
        return Some(parse(raw_trimmed));
    }
    let lexical = gleaph_gql::lexer::tokenize_with_comments(query).ok()?;
    let is_index = first_two_idents_are(&lexical.tokens, "CREATE", "INDEX")
        || first_two_idents_are(&lexical.tokens, "DROP", "INDEX");
    if !is_index {
        return None;
    }

    // Preserve the exact caller bytes for checksum purposes while replacing comments only in the
    // parser's private input. Keeping newlines avoids accidentally joining two adjacent tokens.
    let mut parse_bytes = query.as_bytes().to_vec();
    for comment in lexical.comments {
        for byte in &mut parse_bytes[comment.span.start..comment.span.end] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    let parse_input =
        String::from_utf8(parse_bytes).expect("comment replacement preserves UTF-8 boundaries");
    Some(parse(&parse_input))
}

fn ident_is(token: &gleaph_gql::token::Token, expected: &str) -> bool {
    matches!(token, gleaph_gql::token::Token::Ident(value) if value.eq_ignore_ascii_case(expected))
}

fn first_two_idents_are(tokens: &[gleaph_gql::token::Spanned], first: &str, second: &str) -> bool {
    tokens
        .first()
        .is_some_and(|token| ident_is(&token.token, first))
        && tokens
            .get(1)
            .is_some_and(|token| ident_is(&token.token, second))
}

fn parse(query: &str) -> Result<IndexDdlStatement, IndexDdlParseError> {
    let mut cur = Cursor::new(query);
    cur.skip_ws();
    if cur.consume_ascii_ci("CREATE") {
        cur.expect_ascii_ci("INDEX")?;
        let index_name = cur.parse_ident()?;
        cur.skip_ws();
        let if_not_exists = cur.try_consume_ascii_ci("IF NOT EXISTS");
        if if_not_exists {
            cur.skip_ws();
        }
        cur.expect_ascii_ci("FOR")?;
        let (kind, label, edge_direction) = parse_for_pattern(&mut cur)?;
        cur.skip_ws();
        cur.expect_ascii_ci("ON")?;
        let property = parse_on_property(&mut cur)?;
        cur.skip_ws();
        cur.try_consume(';');
        cur.skip_ws();
        if !cur.is_eof() {
            return Err(IndexDdlParseError::TrailingInput);
        }
        Ok(IndexDdlStatement::Create {
            index_name,
            if_not_exists,
            target: IndexTarget {
                kind,
                label,
                property,
                edge_direction,
            },
        })
    } else if cur.consume_ascii_ci("DROP") {
        cur.expect_ascii_ci("INDEX")?;
        let index_name = cur.parse_ident()?;
        cur.skip_ws();
        let if_exists = cur.try_consume_ascii_ci("IF EXISTS");
        cur.skip_ws();
        cur.try_consume(';');
        cur.skip_ws();
        if !cur.is_eof() {
            return Err(IndexDdlParseError::TrailingInput);
        }
        Ok(IndexDdlStatement::Drop {
            index_name,
            if_exists,
        })
    } else {
        Err(IndexDdlParseError::Expected(
            "CREATE INDEX or DROP INDEX".into(),
        ))
    }
}

fn parse_for_pattern(
    cur: &mut Cursor<'_>,
) -> Result<(IndexedPropertyKind, String, Option<EdgeDirection>), IndexDdlParseError> {
    cur.expect('(')?;
    cur.skip_ws();
    if cur.peek() == Some(')') {
        cur.expect(')')?;
        cur.skip_ws();
        reject_slash_edge_pattern(cur)?;
        let (label, direction) = parse_for_edge_pattern(cur)?;
        cur.skip_ws();
        cur.expect('(')?;
        cur.skip_ws();
        cur.expect(')')?;
        Ok((IndexedPropertyKind::Edge, label, Some(direction)))
    } else {
        let _var = cur.parse_ident()?;
        cur.expect(':')?;
        let label = cur.parse_ident()?;
        cur.skip_ws();
        cur.expect(')')?;
        Ok((IndexedPropertyKind::Vertex, label, None))
    }
}

/// Slash edge patterns (`-/L/->`, `~/L/~`, …) bind no edge variable; `ON (e.prop)` requires
/// bracket form.
fn reject_slash_edge_pattern(cur: &mut Cursor<'_>) -> Result<(), IndexDdlParseError> {
    let saved = cur.pos;
    cur.skip_ws();
    let slash = match cur.peek() {
        Some('-') => {
            cur.pos += 1;
            cur.skip_ws();
            cur.peek() == Some('/')
        }
        Some('~') => {
            cur.pos += 1;
            cur.skip_ws();
            cur.peek() == Some('/')
        }
        Some('<') => {
            cur.pos += 1;
            cur.skip_ws();
            match cur.peek() {
                Some('~') | Some('-') => {
                    cur.pos += 1;
                    cur.skip_ws();
                    cur.peek() == Some('/')
                }
                _ => false,
            }
        }
        _ => false,
    };
    cur.pos = saved;
    if slash {
        Err(IndexDdlParseError::SlashEdgePatternNotSupported)
    } else {
        Ok(())
    }
}

/// Parses the `[var:Label]` filler shared by every edge-pattern arrow form.
fn parse_bracket_label(cur: &mut Cursor<'_>) -> Result<String, IndexDdlParseError> {
    cur.expect('[')?;
    let label = parse_edge_pattern_filler(cur)?;
    cur.skip_ws();
    cur.expect(']')?;
    cur.skip_ws();
    Ok(label)
}

fn parse_for_edge_pattern(
    cur: &mut Cursor<'_>,
) -> Result<(String, EdgeDirection), IndexDdlParseError> {
    if cur.try_consume('<') {
        if cur.try_consume('~') {
            let label = parse_bracket_label(cur)?;
            cur.expect('~')?;
            return Ok((label, EdgeDirection::LeftOrUndirected));
        }
        cur.expect('-')?;
        let label = parse_bracket_label(cur)?;
        if cur.try_consume('-') {
            if cur.try_consume('>') {
                return Ok((label, EdgeDirection::LeftOrRight));
            }
            return Ok((label, EdgeDirection::PointingLeft));
        }
        return Err(IndexDdlParseError::Expected("]- or ]->".into()));
    }
    if cur.try_consume('~') {
        let label = parse_bracket_label(cur)?;
        cur.expect('~')?;
        if cur.try_consume('>') {
            return Ok((label, EdgeDirection::UndirectedOrRight));
        }
        return Ok((label, EdgeDirection::Undirected));
    }
    cur.expect('-')?;
    let label = parse_bracket_label(cur)?;
    if cur.try_consume('-') {
        if cur.try_consume('>') {
            return Ok((label, EdgeDirection::PointingRight));
        }
        return Ok((label, EdgeDirection::AnyDirection));
    }
    Err(IndexDdlParseError::Expected("edge closing token".into()))
}

fn parse_edge_pattern_filler(cur: &mut Cursor<'_>) -> Result<String, IndexDdlParseError> {
    let _var = cur.parse_ident()?;
    cur.expect(':')?;
    cur.parse_ident()
}

fn parse_on_property(cur: &mut Cursor<'_>) -> Result<String, IndexDdlParseError> {
    cur.expect('(')?;
    cur.skip_ws();
    let _var = cur.parse_ident()?;
    cur.expect('.')?;
    let mut parts = vec![cur.parse_ident()?];
    while cur.try_consume('.') {
        parts.push(cur.parse_ident()?);
    }
    cur.skip_ws();
    cur.expect(')')?;
    Ok(parts.join("."))
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

    fn try_consume_ascii_ci(&mut self, word: &str) -> bool {
        let saved = self.pos;
        self.skip_ws();
        if self.consume_ascii_ci(word) {
            true
        } else {
            self.pos = saved;
            false
        }
    }

    fn expect_ascii_ci(&mut self, word: &str) -> Result<(), IndexDdlParseError> {
        if self.consume_ascii_ci(word) {
            Ok(())
        } else {
            Err(IndexDdlParseError::Expected(word.to_string()))
        }
    }

    fn expect(&mut self, ch: char) -> Result<(), IndexDdlParseError> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(IndexDdlParseError::Expected(ch.to_string()))
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

    fn parse_ident(&mut self) -> Result<String, IndexDdlParseError> {
        self.skip_ws();
        let start = self.pos;
        let first = self
            .peek()
            .ok_or_else(|| IndexDdlParseError::Expected("identifier".into()))?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return Err(IndexDdlParseError::Expected("identifier".into()));
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
            .map_err(|_| IndexDdlParseError::Expected("identifier".into()))?;
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vertex_and_edge_create_index() {
        let Some(Ok(IndexDdlStatement::Create {
            index_name,
            if_not_exists,
            target,
        })) = try_parse("CREATE INDEX person_age FOR (n:Person) ON (n.age)")
        else {
            panic!("expected vertex index");
        };
        assert_eq!(index_name, "person_age");
        assert!(!if_not_exists);
        assert_eq!(target.kind, IndexedPropertyKind::Vertex);
        assert_eq!(target.label, "Person");
        assert_eq!(target.property, "age");

        let Some(Ok(IndexDdlStatement::Create { target, .. })) =
            try_parse("CREATE INDEX knows_weight FOR ()-[e:KNOWS]->() ON (e.weight)")
        else {
            panic!("expected edge index");
        };
        assert_eq!(target.kind, IndexedPropertyKind::Edge);
        assert_eq!(target.edge_direction, Some(EdgeDirection::PointingRight));
    }

    #[test]
    fn preserves_all_bracket_edge_directions_and_if_not_exists() {
        let cases = [
            (
                "CREATE INDEX w FOR () <-[e:KNOWS]- () ON (e.weight)",
                EdgeDirection::PointingLeft,
            ),
            (
                "CREATE INDEX w FOR () <-[e:KNOWS]-> () ON (e.weight)",
                EdgeDirection::LeftOrRight,
            ),
            (
                "CREATE INDEX w FOR () ~[e:KNOWS]~ () ON (e.weight)",
                EdgeDirection::Undirected,
            ),
            (
                "CREATE INDEX w FOR () ~[e:KNOWS]~> () ON (e.weight)",
                EdgeDirection::UndirectedOrRight,
            ),
            (
                "CREATE INDEX w FOR () <~[e:KNOWS]~ () ON (e.weight)",
                EdgeDirection::LeftOrUndirected,
            ),
            (
                "CREATE INDEX w FOR ()-[e:KNOWS]-() ON (e.weight)",
                EdgeDirection::AnyDirection,
            ),
        ];
        for (ddl, direction) in cases {
            let Some(Ok(IndexDdlStatement::Create { target, .. })) = try_parse(ddl) else {
                panic!("expected edge index: {ddl}");
            };
            assert_eq!(target.edge_direction, Some(direction), "ddl: {ddl}");
        }
        let Some(Ok(IndexDdlStatement::Create { if_not_exists, .. })) =
            try_parse("CREATE INDEX w IF NOT EXISTS FOR (n:Person) ON (n.age)")
        else {
            panic!("expected IF NOT EXISTS");
        };
        assert!(if_not_exists);
    }

    #[test]
    fn preserves_nested_edge_property_and_rejects_slash_syntax() {
        let Some(Ok(IndexDdlStatement::Create { target, .. })) =
            try_parse("CREATE INDEX affinity FOR ()-[e:AFFINITY]-() ON (e.stats.score)")
        else {
            panic!("expected edge index");
        };
        assert_eq!(target.property, "stats.score");

        let Some(Err(error)) =
            try_parse("CREATE INDEX affinity FOR ()-/AFFINITY/->() ON (e.score)")
        else {
            panic!("expected slash syntax rejection");
        };
        assert_eq!(error, IndexDdlParseError::SlashEdgePatternNotSupported);
    }

    #[test]
    fn parses_drop_and_ignores_non_index_queries() {
        let Some(Ok(IndexDdlStatement::Drop {
            index_name,
            if_exists,
        })) = try_parse("DROP INDEX person_age IF EXISTS;")
        else {
            panic!("expected drop index");
        };
        assert_eq!(index_name, "person_age");
        assert!(if_exists);
        assert!(try_parse("MATCH (n) RETURN n").is_none());
    }

    #[test]
    fn accepts_leading_and_trailing_comments_without_changing_statement_shape() {
        let Some(Ok(IndexDdlStatement::Create { target, .. })) = try_parse(
            "// leading\n/* between */ CREATE INDEX person_age FOR (n:Person) ON (n.age) // trailing",
        ) else {
            panic!("expected commented index");
        };
        assert_eq!(target.kind, IndexedPropertyKind::Vertex);
        assert_eq!(target.label, "Person");
        assert_eq!(target.property, "age");

        let Some(Ok(IndexDdlStatement::Create { index_name, .. })) =
            try_parse("CREATE /* between */ INDEX person_age FOR (n:Person) ON (n.age)")
        else {
            panic!("expected comment between CREATE and INDEX");
        };
        assert_eq!(index_name, "person_age");
    }
}
