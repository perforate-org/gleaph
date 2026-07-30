//! Heap runtime support for prepared GQL programs.
//!
//! The source and durable metadata for prepared queries are owned by the
//! caller (currently the Router). This crate only parses source into an AST
//! record for the current canister instance; it deliberately does not expose
//! a stable-memory catalog or persist an AST.

use gleaph_gql::ast::GqlProgram;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::token::Comment;

/// Failure while compiling a prepared GQL source string.
#[derive(Debug, thiserror::Error)]
pub enum PreparedQueryError {
    /// The source could not be parsed as GQL.
    #[error("parse error: {0}")]
    Parse(String),
    /// Prepared queries must contain a transaction statement body.
    #[error("prepared GQL must be a transaction with a statement body")]
    MissingStatementBlock,
}

/// A parsed prepared program kept in the current canister's heap.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedQueryRecord {
    /// Parsed GQL program.
    pub program: GqlProgram,
    /// Whether execution must use the update path.
    pub requires_write_path: bool,
}

/// A validated prepared source with comments retained for metadata generation.
#[derive(Debug)]
pub struct PreparedQuerySource {
    /// Parsed GQL program.
    pub program: GqlProgram,
    /// All source comments, including non-documentation comments.
    pub comments: Vec<Comment>,
    /// Whether execution must use the update path.
    pub requires_write_path: bool,
}

/// Parse and validate a prepared-query source string.
pub fn compile_prepared_source(source: &str) -> Result<GqlProgram, PreparedQueryError> {
    Ok(parse_prepared_source(source)?.program)
}

/// Parse, validate, and retain comments from a prepared-query source string.
pub fn parse_prepared_source(source: &str) -> Result<PreparedQuerySource, PreparedQueryError> {
    let parsed = parser::parse_with_comments(source)
        .map_err(|e| PreparedQueryError::Parse(e.to_string()))?;
    let program = parsed.program;
    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or(PreparedQueryError::MissingStatementBlock)?;
    if tx.body.is_none() {
        return Err(PreparedQueryError::MissingStatementBlock);
    }
    let requires_write_path = classify_program(&program).requires_write_path();
    Ok(PreparedQuerySource {
        program,
        comments: parsed.comments,
        requires_write_path,
    })
}

/// Parse, validate, and classify a prepared-query source string for runtime use.
pub fn prepare(source: &str) -> Result<PreparedQueryRecord, PreparedQueryError> {
    let parsed = parse_prepared_source(source)?;
    Ok(PreparedQueryRecord {
        program: parsed.program,
        requires_write_path: parsed.requires_write_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_rejects_program_without_body() {
        let err = compile_prepared_source("").expect_err("empty");
        assert!(matches!(
            err,
            PreparedQueryError::MissingStatementBlock | PreparedQueryError::Parse(_)
        ));
    }

    #[test]
    fn prepare_classifies_write_path() {
        let got = prepare("MATCH (n:PrepRuntime) RETURN n NEXT INSERT (m:PrepRuntime {k: 1})")
            .expect("prepare");
        assert!(got.requires_write_path);
        assert_eq!(
            got.program
                .transaction_activity
                .as_ref()
                .and_then(|t| t.body.as_ref())
                .map(|b| b.iter_statements().count()),
            Some(2)
        );
    }

    #[test]
    fn prepare_classifies_read_only_path() {
        let got = prepare("MATCH (n:PrepRuntimeReadOnly) RETURN n").expect("prepare");
        assert!(!got.requires_write_path);
    }

    #[test]
    fn parse_prepared_source_retains_comments() {
        let parsed = parse_prepared_source(
            "/// query docs\nMATCH (n:PrepRuntimeComments) RETURN n /* implementation note */",
        )
        .expect("parse");
        assert_eq!(parsed.comments.len(), 2);
        assert_eq!(parsed.program.doc_comments.len(), 1);
        assert_eq!(parsed.program.doc_comments[0].text, "query docs");
    }
}
