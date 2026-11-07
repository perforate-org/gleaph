//! Lexical highlighting for GQL source.
//!
//! The lexer already owns every token's byte span; this module gives those spans a UI-neutral
//! kind so an editor or console can colour GQL without re-tokenising it and without this crate
//! carrying a UI's palette or span types. A host maps [`HighlightKind`] onto its own theme.
//!
//! ```ignore
//! for (range, kind) in gleaph_gql::highlight::highlight(source)? {
//!     theme.paint(kind, &source[range]);
//! }
//! ```

use std::ops::Range;

use crate::error::GqlError;
use crate::lexer::tokenize_with_comments;
use crate::token::Token;

/// What a highlighted range is, as colouring needs it.
///
/// The kinds are the lexical distinctions a reader can see: reserved words, names, literals,
/// parameters and comments. They are deliberately not parser concepts - a highlight pass must
/// work on source that does not parse yet, which is the state source is in while it is typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HighlightKind {
    /// A reserved or prereserved GQL word (`MATCH`, `RETURN`, `NOT`, ...).
    Keyword,
    /// An unquoted identifier.
    Identifier,
    /// A delimited identifier (`"my col"`, `` `my col` ``).
    QuotedIdentifier,
    /// A string literal, including a bytes literal.
    String,
    /// An integer, exact numeric or float literal.
    Number,
    /// A parameter reference (`$name` or `$$name`).
    Parameter,
    /// Brackets, separators and operators.
    Punctuation,
    /// A line or block comment, including its delimiters.
    Comment,
}

/// Classify `source`'s tokens for highlighting.
///
/// Returns byte ranges in source order, non-overlapping, each one equal to the token or comment
/// text it came from. Keywords are matched case-insensitively against the ISO/IEC 39075 reserved
/// and prereserved sets.
///
/// Lexing is the only failure mode: source that does not lex yet (an unclosed string, an unknown
/// character) returns the lexer's error. Highlighting a draft therefore asks the host to keep the
/// previous spans until the source lexes again, rather than inventing them.
pub fn highlight(source: &str) -> Result<Vec<(Range<usize>, HighlightKind)>, GqlError> {
    let lexed = tokenize_with_comments(source)?;
    let mut spans: Vec<(Range<usize>, HighlightKind)> =
        Vec::with_capacity(lexed.tokens.len() + lexed.comments.len());
    for spanned in &lexed.tokens {
        let kind = match &spanned.token {
            Token::Ident(name) => {
                if gleaph_gql_keywords::is_reserved_keyword(name)
                    || gleaph_gql_keywords::is_prereserved_keyword(name)
                {
                    HighlightKind::Keyword
                } else {
                    HighlightKind::Identifier
                }
            }
            Token::QuotedIdent(_) => HighlightKind::QuotedIdentifier,
            Token::StringLit(_) | Token::BytesLit(_) => HighlightKind::String,
            Token::Int(_) | Token::BigInt(_) | Token::Float(_) | Token::ExactNumeric(_) => {
                HighlightKind::Number
            }
            Token::Param(_) | Token::SubstitutedParam(_) => HighlightKind::Parameter,
            _ => HighlightKind::Punctuation,
        };
        spans.push((spanned.span.start..spanned.span.end, kind));
    }
    for comment in &lexed.comments {
        spans.push((comment.span.start..comment.span.end, HighlightKind::Comment));
    }
    spans.sort_by_key(|(range, _)| range.start);
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<(String, HighlightKind)> {
        highlight(source)
            .expect("source lexes")
            .into_iter()
            .map(|(range, kind)| (source[range].to_owned(), kind))
            .collect()
    }

    #[test]
    fn every_range_slices_back_to_its_own_token() {
        // The invariant that makes a highlight safe to paint: a range is exactly the text the
        // lexer classified, so a host can slice the source with it.
        let source = "MATCH (n:Person)-[e:KNOWS]->(m) WHERE n.age >= 30 RETURN n.name, m.name";
        let spans = highlight(source).expect("source lexes");
        assert!(spans.len() > 10);
        for (range, _) in &spans {
            assert!(range.start < range.end, "a span is never empty");
            assert!(range.end <= source.len());
            assert!(!source[range.clone()].is_empty());
        }
        for pair in spans.windows(2) {
            assert!(
                pair[0].0.end <= pair[1].0.start,
                "spans are sorted and never overlap"
            );
        }
    }

    #[test]
    fn reserved_words_are_keywords_and_names_are_identifiers() {
        let found = kinds("MATCH (n:Person) RETURN n");
        assert!(found.contains(&("MATCH".to_owned(), HighlightKind::Keyword)));
        assert!(found.contains(&("RETURN".to_owned(), HighlightKind::Keyword)));
        assert!(found.contains(&("n".to_owned(), HighlightKind::Identifier)));
        assert!(found.contains(&("Person".to_owned(), HighlightKind::Identifier)));
    }

    #[test]
    fn keywords_match_case_insensitively() {
        let found = kinds("match (n) return n");
        assert!(found.contains(&("match".to_owned(), HighlightKind::Keyword)));
        assert!(found.contains(&("return".to_owned(), HighlightKind::Keyword)));
    }

    #[test]
    fn literals_parameters_and_quoted_identifiers_get_their_own_kinds() {
        let found = kinds("RETURN \"my col\", 'text', 42, 1.5, X'4142', $value, $$value");
        assert!(found.contains(&("\"my col\"".to_owned(), HighlightKind::QuotedIdentifier)));
        assert!(found.contains(&("'text'".to_owned(), HighlightKind::String)));
        assert!(found.contains(&("42".to_owned(), HighlightKind::Number)));
        assert!(found.contains(&("1.5".to_owned(), HighlightKind::Number)));
        assert!(found.contains(&("X'4142'".to_owned(), HighlightKind::String)));
        assert!(found.contains(&("$value".to_owned(), HighlightKind::Parameter)));
        assert!(found.contains(&("$$value".to_owned(), HighlightKind::Parameter)));
    }

    #[test]
    fn comments_are_highlighted_including_their_delimiters() {
        let found = kinds("MATCH (n) // a line\nRETURN n /* a block */");
        assert!(found.contains(&("// a line".to_owned(), HighlightKind::Comment)));
        assert!(found.contains(&("/* a block */".to_owned(), HighlightKind::Comment)));
    }

    #[test]
    fn source_that_does_not_lex_yet_reports_the_lexer_error() {
        // Highlighting a draft is allowed to fail; inventing spans for unlexable source is not.
        assert!(highlight("MATCH 'unterminated").is_err());
        assert!(highlight("MATCH ^").is_err());
    }
}
