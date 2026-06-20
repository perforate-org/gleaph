//! GQL parser — recursive-descent parser producing AST nodes.
//!
//! # Usage
//!
//! ```rust
//! use gleaph_gql::parser::parse;
//! let program = parse("MATCH (n) RETURN n").unwrap();
//! ```

use crate::ast::GqlProgram;
use crate::error::GqlError;
use crate::lexer;
use crate::token::Comment;

mod clause;
mod ddl;
mod expr;
pub mod helpers;
mod pattern;
mod session;
mod statement;
mod types;

pub use helpers::{Parser, is_prereserved_keyword, is_reserved_keyword};

/// The result of parsing with comment preservation.
#[derive(Debug)]
pub struct ParseResult {
    /// The parsed program AST.
    pub program: GqlProgram,
    /// All comments found in the source, in order of appearance.
    pub comments: Vec<Comment>,
}

/// Parses a GQL source string into a [`GqlProgram`] AST.
///
/// This is the main entry point. It tokenizes the input then runs the
/// recursive-descent parser. Comments are discarded.
pub fn parse(input: &str) -> Result<GqlProgram, GqlError> {
    let tokens = lexer::tokenize(input)?;
    let mut parser = Parser::new(&tokens);
    let program = parser.parse_program()?;
    if !parser.at_end() {
        return Err(parser.expected("end of input"));
    }
    Ok(program)
}

/// Parses a GQL source string, preserving comments alongside the AST.
///
/// This is the comment-preserving alternative to [`parse`]. Comments can
/// be correlated to AST nodes by comparing their [`Span`](crate::token::Span)
/// positions.
pub fn parse_with_comments(input: &str) -> Result<ParseResult, GqlError> {
    let result = lexer::tokenize_with_comments(input)?;
    let mut parser = Parser::new(&result.tokens);
    let program = parser.parse_program()?;
    if !parser.at_end() {
        return Err(parser.expected("end of input"));
    }
    Ok(ParseResult {
        program,
        comments: result.comments,
    })
}

/// Parses a single GQL expression from a source string.
/// Useful for testing and REPL-like contexts.
pub fn parse_expr(input: &str) -> Result<crate::ast::Expr, GqlError> {
    let tokens = lexer::tokenize(input)?;
    let mut parser = Parser::new(&tokens);
    let expr = parser.parse_expr()?;
    if !parser.at_end() {
        return Err(parser.expected("end of input"));
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{CommentKind, Span};

    #[test]
    fn parse_with_comments_preserves_comments() {
        let result = parse_with_comments("// find people\nMATCH (n) RETURN n").unwrap();
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.comments[0].kind, CommentKind::Line);
        assert_eq!(result.comments[0].text, " find people");
        assert_eq!(result.comments[0].span, Span { start: 0, end: 14 });
        // The AST should parse successfully.
        assert!(
            !result.program.session_activity.is_empty()
                || result.program.transaction_activity.is_some()
        );
    }

    #[test]
    fn parse_with_comments_block_comment() {
        let result = parse_with_comments("MATCH /* graph pattern */ (n) RETURN n").unwrap();
        assert_eq!(result.comments.len(), 1);
        assert_eq!(result.comments[0].kind, CommentKind::Block);
        assert_eq!(result.comments[0].text, " graph pattern ");
    }

    #[test]
    fn parse_with_comments_no_comments() {
        let result = parse_with_comments("MATCH (n) RETURN n").unwrap();
        assert!(result.comments.is_empty());
    }

    #[test]
    fn parse_without_comments_still_works() {
        // The original parse() should still work fine.
        let program = parse("// comment\nMATCH (n) RETURN n").unwrap();
        assert!(program.transaction_activity.is_some());
    }

    // ── Recursion-depth guard (stack-overflow DoS hardening) ─────────────
    //
    // A recursive-descent parser fed deeply nested input would otherwise
    // overflow the (wasm) stack and trap the canister. The guard converts that
    // into a bounded parse error. Each negative case uses far more nesting than
    // `Parser::MAX_RECURSION_DEPTH`.

    fn assert_depth_error(input: &str) {
        match parse(input) {
            Err(GqlError::Parse(msg)) => assert!(
                msg.contains("nesting depth"),
                "expected a nesting-depth error, got: {msg}"
            ),
            Err(other) => panic!("expected a nesting-depth parse error, got: {other:?}"),
            Ok(_) => panic!("deeply nested input should be rejected, not parsed"),
        }
    }

    #[test]
    fn deeply_nested_parentheses_are_rejected_not_overflowing() {
        let depth = 10_000;
        let input = format!("RETURN {}1{}", "(".repeat(depth), ")".repeat(depth));
        assert_depth_error(&input);
    }

    #[test]
    fn deeply_nested_not_chain_is_rejected_not_overflowing() {
        let input = format!("RETURN {}true", "NOT ".repeat(10_000));
        assert_depth_error(&input);
    }

    #[test]
    fn deeply_nested_subqueries_are_rejected_not_overflowing() {
        let input = format!(
            "RETURN {}1{}",
            "VALUE { RETURN ".repeat(2_000),
            " }".repeat(2_000)
        );
        // Either the subquery guard or the expression guard fires first; both
        // are nesting-depth errors.
        assert_depth_error(&input);
    }

    #[test]
    fn moderately_nested_expression_still_parses() {
        // Comfortably below MAX_RECURSION_DEPTH: must not be falsely rejected.
        let depth = Parser::MAX_RECURSION_DEPTH / 4;
        let input = format!("RETURN {}1{}", "(".repeat(depth), ")".repeat(depth));
        assert!(
            parse(&input).is_ok(),
            "nesting below the limit must still parse"
        );
    }
}
