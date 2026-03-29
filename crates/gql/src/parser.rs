//! GQL parser — recursive-descent parser producing AST nodes.
//!
//! # Usage
//!
//! ```rust
//! use gleaph_gql::parser::parse;
//! let program = parse("MATCH (n) RETURN n").unwrap();
//! ```

mod clause;
mod ddl;
mod expr;
pub mod helpers;
mod pattern;
mod session;
mod statement;
mod types;

use crate::ast::GqlProgram;
use crate::error::GqlError;
use crate::lexer;
use crate::token::Comment;
pub use helpers::{Parser, is_reserved_keyword};

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
}
