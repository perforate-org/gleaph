//! Gleaph extension DDL: `CREATE CONSTRAINT` / `DROP CONSTRAINT` (ADR 0030).
//!
//! First cut: vertex single-property uniqueness only.
//!
//! ```text
//! CREATE CONSTRAINT <name> [IF NOT EXISTS] FOR (n:Label) REQUIRE n.prop IS UNIQUE
//! DROP CONSTRAINT <name> [IF EXISTS]
//! ```

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConstraintDdlStatement {
    Create {
        constraint_name: String,
        if_not_exists: bool,
        label: String,
        property: String,
    },
    Drop {
        constraint_name: String,
        if_exists: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ConstraintDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
    #[error("the REQUIRE variable must match the FOR pattern variable")]
    VariableMismatch,
    #[error("edge uniqueness constraints are not supported in the first cut (ADR 0030)")]
    EdgeConstraintUnsupported,
}

/// Returns `None` when the query is not constraint DDL (caller should use standard GQL parse).
pub(crate) fn try_parse(
    query: &str,
) -> Option<Result<ConstraintDdlStatement, ConstraintDdlParseError>> {
    let trimmed = query.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("CREATE CONSTRAINT") || upper.starts_with("DROP CONSTRAINT") {
        Some(parse(trimmed))
    } else {
        None
    }
}

fn parse(query: &str) -> Result<ConstraintDdlStatement, ConstraintDdlParseError> {
    let mut cur = Cursor::new(query);
    cur.skip_ws();
    if cur.consume_ascii_ci("CREATE") {
        cur.expect_ascii_ci("CONSTRAINT")?;
        let constraint_name = cur.parse_ident()?;
        let if_not_exists = cur.try_consume_ascii_ci("IF NOT EXISTS");
        cur.expect_ascii_ci("FOR")?;
        let (var, label) = parse_for_vertex_pattern(&mut cur)?;
        cur.expect_ascii_ci("REQUIRE")?;
        let (req_var, property) = parse_property_ref(&mut cur)?;
        if req_var != var {
            return Err(ConstraintDdlParseError::VariableMismatch);
        }
        cur.expect_ascii_ci("IS")?;
        cur.expect_ascii_ci("UNIQUE")?;
        cur.try_consume(';');
        cur.skip_ws();
        if !cur.is_eof() {
            return Err(ConstraintDdlParseError::TrailingInput);
        }
        Ok(ConstraintDdlStatement::Create {
            constraint_name,
            if_not_exists,
            label,
            property,
        })
    } else if cur.consume_ascii_ci("DROP") {
        cur.expect_ascii_ci("CONSTRAINT")?;
        let constraint_name = cur.parse_ident()?;
        let if_exists = cur.try_consume_ascii_ci("IF EXISTS");
        cur.try_consume(';');
        cur.skip_ws();
        if !cur.is_eof() {
            return Err(ConstraintDdlParseError::TrailingInput);
        }
        Ok(ConstraintDdlStatement::Drop {
            constraint_name,
            if_exists,
        })
    } else {
        Err(ConstraintDdlParseError::Expected(
            "CREATE CONSTRAINT or DROP CONSTRAINT".into(),
        ))
    }
}

/// Parses `(var:Label)`. An edge pattern (`()-[..]-()`) is rejected as unsupported.
fn parse_for_vertex_pattern(
    cur: &mut Cursor<'_>,
) -> Result<(String, String), ConstraintDdlParseError> {
    cur.expect('(')?;
    cur.skip_ws();
    if cur.peek() == Some(')') {
        return Err(ConstraintDdlParseError::EdgeConstraintUnsupported);
    }
    let var = cur.parse_ident()?;
    cur.expect(':')?;
    let label = cur.parse_ident()?;
    cur.skip_ws();
    cur.expect(')')?;
    Ok((var, label))
}

fn parse_property_ref(cur: &mut Cursor<'_>) -> Result<(String, String), ConstraintDdlParseError> {
    let var = cur.parse_ident()?;
    cur.expect('.')?;
    let property = cur.parse_ident()?;
    Ok((var, property))
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

    fn expect_ascii_ci(&mut self, word: &str) -> Result<(), ConstraintDdlParseError> {
        if self.consume_ascii_ci(word) {
            Ok(())
        } else {
            Err(ConstraintDdlParseError::Expected(word.to_string()))
        }
    }

    fn expect(&mut self, ch: char) -> Result<(), ConstraintDdlParseError> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(ConstraintDdlParseError::Expected(ch.to_string()))
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

    fn parse_ident(&mut self) -> Result<String, ConstraintDdlParseError> {
        self.skip_ws();
        let start = self.pos;
        let first = self
            .peek()
            .ok_or_else(|| ConstraintDdlParseError::Expected("identifier".into()))?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return Err(ConstraintDdlParseError::Expected("identifier".into()));
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
            .map_err(|_| ConstraintDdlParseError::Expected("identifier".into()))?;
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(query: &str) -> ConstraintDdlStatement {
        parse(query).expect("parse")
    }

    #[test]
    fn create_vertex_unique_constraint() {
        assert_eq!(
            parse_ok(
                "CREATE CONSTRAINT user_email IF NOT EXISTS FOR (n:User) REQUIRE n.email IS UNIQUE;"
            ),
            ConstraintDdlStatement::Create {
                constraint_name: "user_email".into(),
                if_not_exists: true,
                label: "User".into(),
                property: "email".into(),
            }
        );
    }

    #[test]
    fn create_without_if_not_exists() {
        assert_eq!(
            parse_ok("CREATE CONSTRAINT c FOR (u:Account) REQUIRE u.handle IS UNIQUE"),
            ConstraintDdlStatement::Create {
                constraint_name: "c".into(),
                if_not_exists: false,
                label: "Account".into(),
                property: "handle".into(),
            }
        );
    }

    #[test]
    fn drop_constraint_if_exists() {
        assert_eq!(
            parse_ok("DROP CONSTRAINT user_email IF EXISTS"),
            ConstraintDdlStatement::Drop {
                constraint_name: "user_email".into(),
                if_exists: true,
            }
        );
    }

    #[test]
    fn edge_constraint_is_unsupported() {
        let err = parse("CREATE CONSTRAINT c FOR ()-[r:KNOWS]-() REQUIRE r.weight IS UNIQUE")
            .unwrap_err();
        assert_eq!(err, ConstraintDdlParseError::EdgeConstraintUnsupported);
    }

    #[test]
    fn variable_mismatch_is_rejected() {
        let err = parse("CREATE CONSTRAINT c FOR (n:User) REQUIRE m.email IS UNIQUE").unwrap_err();
        assert_eq!(err, ConstraintDdlParseError::VariableMismatch);
    }

    #[test]
    fn non_constraint_query_returns_none() {
        assert!(try_parse("MATCH (n) RETURN n").is_none());
        assert!(try_parse("CREATE INDEX x FOR (n:N) ON (n.p)").is_none());
    }

    #[test]
    fn try_parse_detects_create_constraint() {
        assert!(try_parse("CREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE").is_some());
    }
}
