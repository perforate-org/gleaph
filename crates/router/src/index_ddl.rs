//! Gleaph extension DDL: `CREATE INDEX` / `DROP INDEX` (ADR 0009 §4, ADR 0012).

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::index::IndexedPropertyKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexDdlStatement {
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
pub(crate) struct IndexTarget {
    pub kind: IndexedPropertyKind,
    pub label: String,
    pub property: String,
    pub edge_direction: Option<EdgeDirection>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum IndexDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
}

/// Returns `None` when the query is not index DDL (caller should use standard GQL parse).
pub(crate) fn try_parse(query: &str) -> Option<Result<IndexDdlStatement, IndexDdlParseError>> {
    let trimmed = query.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("CREATE INDEX") || upper.starts_with("DROP INDEX") {
        Some(parse(trimmed))
    } else {
        None
    }
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

fn parse_for_edge_pattern(
    cur: &mut Cursor<'_>,
) -> Result<(String, EdgeDirection), IndexDdlParseError> {
    if cur.try_consume('<') {
        if cur.try_consume('~') {
            cur.expect('[')?;
            let label = parse_edge_pattern_filler(cur)?;
            cur.skip_ws();
            cur.expect(']')?;
            cur.skip_ws();
            cur.expect('~')?;
            return Ok((label, EdgeDirection::LeftOrUndirected));
        }
        cur.expect('-')?;
        cur.expect('[')?;
        let label = parse_edge_pattern_filler(cur)?;
        cur.skip_ws();
        cur.expect(']')?;
        cur.skip_ws();
        if cur.try_consume('-') {
            if cur.try_consume('>') {
                return Ok((label, EdgeDirection::LeftOrRight));
            }
            return Ok((label, EdgeDirection::PointingLeft));
        }
        return Err(IndexDdlParseError::Expected("]- or ]->".into()));
    }
    if cur.try_consume('~') {
        cur.expect('[')?;
        let label = parse_edge_pattern_filler(cur)?;
        cur.skip_ws();
        cur.expect(']')?;
        cur.skip_ws();
        cur.expect('~')?;
        if cur.try_consume('>') {
            return Ok((label, EdgeDirection::UndirectedOrRight));
        }
        return Ok((label, EdgeDirection::Undirected));
    }
    cur.expect('-')?;
    cur.expect('[')?;
    let label = parse_edge_pattern_filler(cur)?;
    cur.skip_ws();
    cur.expect(']')?;
    cur.skip_ws();
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
    let property = cur.parse_ident()?;
    cur.skip_ws();
    cur.expect(')')?;
    Ok(property)
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
            Err(IndexDdlParseError::Expected(
                match ch {
                    '(' => "(",
                    ')' => ")",
                    ':' => ":",
                    '.' => ".",
                    '-' => "-",
                    '[' => "[",
                    ']' => "]",
                    _ => "character",
                }
                .to_string(),
            ))
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

    fn parse_ok(query: &str) -> IndexDdlStatement {
        parse(query).expect("parse")
    }

    #[test]
    fn create_vertex_index() {
        assert_eq!(
            parse_ok("CREATE INDEX person_age IF NOT EXISTS FOR (n:Person) ON (n.age);"),
            IndexDdlStatement::Create {
                index_name: "person_age".into(),
                if_not_exists: true,
                target: IndexTarget {
                    kind: IndexedPropertyKind::Vertex,
                    label: "Person".into(),
                    property: "age".into(),
                    edge_direction: None,
                },
            }
        );
    }

    #[test]
    fn create_edge_index() {
        assert_eq!(
            parse_ok("CREATE INDEX knows_weight FOR ()-[e:KNOWS]-() ON (e.weight)"),
            IndexDdlStatement::Create {
                index_name: "knows_weight".into(),
                if_not_exists: false,
                target: IndexTarget {
                    kind: IndexedPropertyKind::Edge,
                    label: "KNOWS".into(),
                    property: "weight".into(),
                    edge_direction: Some(EdgeDirection::AnyDirection),
                },
            }
        );
    }

    #[test]
    fn create_edge_index_pointing_right() {
        assert_eq!(
            parse_ok("CREATE INDEX w FOR ()-[e:KNOWS]->() ON (e.weight)"),
            IndexDdlStatement::Create {
                index_name: "w".into(),
                if_not_exists: false,
                target: IndexTarget {
                    kind: IndexedPropertyKind::Edge,
                    label: "KNOWS".into(),
                    property: "weight".into(),
                    edge_direction: Some(EdgeDirection::PointingRight),
                },
            }
        );
    }

    #[test]
    fn drop_index_if_exists() {
        assert_eq!(
            parse_ok("DROP INDEX person_age IF EXISTS"),
            IndexDdlStatement::Drop {
                index_name: "person_age".into(),
                if_exists: true,
            }
        );
    }

    #[test]
    fn non_index_query_returns_none() {
        assert!(try_parse("MATCH (n) RETURN n").is_none());
    }

    #[test]
    fn try_parse_detects_create_index() {
        assert!(try_parse("CREATE INDEX x FOR (n:N) ON (n.p)").is_some());
    }
}
