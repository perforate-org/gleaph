//! Gleaph-specific extension DDL for indexes and uniqueness constraints.
//!
//! This crate is the single parser owner for the vendor `CREATE INDEX` / `DROP INDEX` syntax
//! (ADR 0009 §4, ADR 0012) and `CREATE CONSTRAINT` / `DROP CONSTRAINT` syntax (ADR 0030). It
//! intentionally does not extend the general-purpose `gleaph-gql` grammar; Router and migration
//! tooling consume parsed, Gleaph-specific statements through separate entrypoints at this
//! boundary.

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::index::IndexedPropertyKind;
use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorIndexKind, VectorMetric};

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

/// Gleaph-specific `CREATE TEXT INDEX` / `DROP TEXT INDEX` DDL (plan 0297 `backfill-pull`;
/// ADR 0059 §Text build kind).
///
/// Grammar (recorded decision, 2026-08-26):
/// `CREATE TEXT INDEX [IF NOT EXISTS] <name> FOR (<var>:<Label>) ON (<same var>.<prop>)`
/// and `DROP TEXT INDEX <name> [IF EXISTS]`.
///
/// Deliberately separate from [`IndexDdlStatement`]: a text declaration routes through the
/// text-canister backfill lifecycle, never the property-posting build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextIndexDdlStatement {
    Create {
        index_name: String,
        if_not_exists: bool,
        label: String,
        property: String,
    },
    Drop {
        index_name: String,
        if_exists: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextIndexDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
    #[error("the ON variable must match the FOR pattern variable")]
    VariableMismatch,
    #[error("edge patterns are not supported in CREATE TEXT INDEX")]
    EdgePatternUnsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexTarget {
    pub kind: IndexedPropertyKind,
    pub label: String,
    pub property: String,
    pub edge_direction: Option<EdgeDirection>,
}

/// Gleaph-specific vertex vector-index DDL.
///
/// This is deliberately separate from [`IndexDdlStatement`]: property-index migration consumers
/// must not accidentally acquire vector catalog semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VectorIndexDdlStatement {
    Create {
        index_name: String,
        if_not_exists: bool,
        target: VectorIndexTarget,
    },
}

/// The complete, immutable shape declared by `CREATE VECTOR INDEX`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VectorIndexTarget {
    pub label: String,
    pub embedding_name: String,
    pub dims: u16,
    pub metric: VectorMetric,
    pub encoding: VectorEncoding,
    pub kind: VectorIndexKind,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VectorIndexDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
    #[error("the ON variable must match the FOR pattern variable")]
    VariableMismatch,
    #[error("edge patterns are not supported in CREATE VECTOR INDEX")]
    EdgePatternUnsupported,
    #[error("missing required OPTIONS key: {0}")]
    MissingOption(String),
    #[error("duplicate OPTIONS key: {0}")]
    DuplicateOption(String),
    #[error("unsupported OPTIONS key: {0}")]
    UnsupportedOption(String),
    #[error("invalid value for OPTIONS key {option}: {value}")]
    InvalidOptionValue { option: String, value: String },
    #[error("dimensions must be a positive u16 integer")]
    InvalidDimensions,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstraintDdlStatement {
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
pub enum ConstraintDdlParseError {
    #[error("expected {0}")]
    Expected(String),
    #[error("unexpected trailing input")]
    TrailingInput,
    #[error("the REQUIRE variable must match the FOR pattern variable")]
    VariableMismatch,
    #[error("edge uniqueness constraints are not supported in the first cut (ADR 0030)")]
    EdgeConstraintUnsupported,
}

/// Returns `None` when the query is not index DDL (caller should use standard GQL parsing).
///
/// A query may chain several statements with `NEXT`, mirroring the migration payload convention;
/// each statement keeps its optional trailing semicolon and `NEXT` is the only chain operator.
pub fn try_parse(query: &str) -> Option<Result<Vec<IndexDdlStatement>, IndexDdlParseError>> {
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

/// Returns `None` when the query is not `CREATE VECTOR INDEX` DDL.
///
/// Vector DDL has its own AST because its target names a vertex embedding field and its complete
/// physical shape, rather than an ordinary property-index target.
pub fn try_parse_vector(
    query: &str,
) -> Option<Result<VectorIndexDdlStatement, VectorIndexDdlParseError>> {
    // `gleaph_gql` tokenization deliberately follows its ISO literal rules, while this vendor DDL
    // contract uses quoted option values. Strip comments locally before recognition so comments
    // do not make an otherwise valid vector declaration look like ordinary GQL.
    let stripped = strip_vector_comments(query);
    let trimmed = stripped.input.trim();
    if !starts_with_vector_create(trimmed) {
        return None;
    }
    if stripped.unterminated_block_comment {
        return Some(Err(VectorIndexDdlParseError::Expected(
            "closing block comment".into(),
        )));
    }
    Some(parse_vector(trimmed))
}

/// Returns `None` when the query is not text-index DDL.
///
/// Grammar: `CREATE TEXT INDEX [IF NOT EXISTS] <name> FOR (<var>:<Label>) ON (<same var>.<prop>)`
/// and `DROP TEXT INDEX <name> [IF EXISTS]`, mirroring the vendor property-index target shape
/// (`CREATE INDEX person_age FOR (n:Person) ON (n.age)`). The analyzer is creation-fixed by the
/// Router TEXT catalog (v0 production pipeline) and therefore not part of the syntax.
pub fn try_parse_text(
    query: &str,
) -> Option<Result<TextIndexDdlStatement, TextIndexDdlParseError>> {
    let trimmed = query.trim();
    if !starts_with_text_ddl(trimmed) {
        return None;
    }
    Some(parse_text(trimmed))
}

fn parse_text(query: &str) -> Result<TextIndexDdlStatement, TextIndexDdlParseError> {
    let mut cur = Cursor::new(query);
    cur.skip_ws();
    if cur.consume_ascii_ci("CREATE") {
        cur.expect_ascii_ci("TEXT")?;
        cur.expect_ascii_ci("INDEX")?;
        let index_name = cur.parse_ident()?;
        let if_not_exists = cur.try_consume_ascii_ci("IF NOT EXISTS");
        cur.expect_ascii_ci("FOR")?;
        let (variable, label) = parse_text_vertex_pattern(&mut cur)?;
        cur.expect_ascii_ci("ON")?;
        let (on_variable, property) = parse_text_property_ref(&mut cur)?;
        if on_variable != variable {
            return Err(TextIndexDdlParseError::VariableMismatch);
        }
        cur.try_consume(';');
        cur.skip_ws();
        if !cur.is_eof() {
            return Err(TextIndexDdlParseError::TrailingInput);
        }
        Ok(TextIndexDdlStatement::Create {
            index_name,
            if_not_exists,
            label,
            property,
        })
    } else if cur.consume_ascii_ci("DROP") {
        cur.expect_ascii_ci("TEXT")?;
        cur.expect_ascii_ci("INDEX")?;
        let index_name = cur.parse_ident()?;
        let if_exists = cur.try_consume_ascii_ci("IF EXISTS");
        cur.try_consume(';');
        cur.skip_ws();
        if !cur.is_eof() {
            return Err(TextIndexDdlParseError::TrailingInput);
        }
        Ok(TextIndexDdlStatement::Drop {
            index_name,
            if_exists,
        })
    } else {
        Err(TextIndexDdlParseError::Expected(
            "CREATE TEXT INDEX or DROP TEXT INDEX".into(),
        ))
    }
}

/// Word-boundary-precise recognition so `CREATE TEXT INDEXED …` stays ordinary GQL instead of
/// surfacing as a confusing text-DDL parse error.
fn starts_with_text_ddl(input: &str) -> bool {
    let mut cur = Cursor::new(input);
    (cur.consume_ascii_ci("CREATE") || cur.consume_ascii_ci("DROP"))
        && cur.consume_ascii_ci("TEXT")
        && cur.consume_ascii_ci("INDEX")
}

/// Parses `(var:Label)`. An edge pattern (`()-[..]-()`) is rejected as unsupported; the TEXT
/// catalog only declares vertex-property targets.
fn parse_text_vertex_pattern(
    cur: &mut Cursor<'_>,
) -> Result<(String, String), TextIndexDdlParseError> {
    cur.expect('(')?;
    cur.skip_ws();
    if cur.peek() == Some(')') {
        return Err(TextIndexDdlParseError::EdgePatternUnsupported);
    }
    let var = cur.parse_ident()?;
    cur.expect(':')?;
    let label = cur.parse_ident()?;
    cur.skip_ws();
    cur.expect(')')?;
    Ok((var, label))
}

/// Parses `(var.property)` — the parenthesized single-segment form shared with the vendor
/// `CREATE INDEX` ON clause. Nested dotted paths are not part of the v0 text contract.
fn parse_text_property_ref(
    cur: &mut Cursor<'_>,
) -> Result<(String, String), TextIndexDdlParseError> {
    cur.expect('(')?;
    cur.skip_ws();
    let var = cur.parse_ident()?;
    cur.expect('.')?;
    let property = cur.parse_ident()?;
    cur.skip_ws();
    cur.expect(')')?;
    Ok((var, property))
}

/// Returns `None` when the query is not constraint DDL (caller should use standard GQL parsing).
pub fn try_parse_constraint(
    query: &str,
) -> Option<Result<ConstraintDdlStatement, ConstraintDdlParseError>> {
    let trimmed = query.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("CREATE CONSTRAINT") || upper.starts_with("DROP CONSTRAINT") {
        Some(parse_constraint(trimmed))
    } else {
        None
    }
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

struct StrippedVectorComments {
    input: String,
    unterminated_block_comment: bool,
}

fn starts_with_vector_create(input: &str) -> bool {
    let mut cur = Cursor::new(input);
    cur.consume_ascii_ci("CREATE")
        && cur.consume_ascii_ci("VECTOR")
        && cur.consume_ascii_ci("INDEX")
}

fn strip_vector_comments(query: &str) -> StrippedVectorComments {
    let bytes = query.as_bytes();
    let mut output = bytes.to_vec();
    let mut pos = 0;
    let mut string_quote = None;
    let mut unterminated_block_comment = false;
    while pos < bytes.len() {
        if string_quote.is_some_and(|quote| bytes[pos] == quote) {
            string_quote = None;
            pos += 1;
            continue;
        }
        if string_quote.is_some() {
            pos += 1;
            continue;
        }
        if bytes[pos] == b'"' || bytes[pos] == b'\'' {
            string_quote = Some(bytes[pos]);
            pos += 1;
            continue;
        }
        if pos + 1 >= bytes.len() {
            pos += 1;
            continue;
        }
        let line_comment = (bytes[pos] == b'/' && bytes[pos + 1] == b'/')
            || (bytes[pos] == b'-' && bytes[pos + 1] == b'-');
        if line_comment {
            let start = pos;
            pos += 2;
            while pos < bytes.len() && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            replace_comment_bytes(&mut output[start..pos]);
            continue;
        }
        if bytes[pos] == b'/' && bytes[pos + 1] == b'*' {
            let start = pos;
            pos += 2;
            let mut depth = 1;
            while pos + 1 < bytes.len() && depth > 0 {
                if bytes[pos] == b'/' && bytes[pos + 1] == b'*' {
                    depth += 1;
                    pos += 2;
                } else if bytes[pos] == b'*' && bytes[pos + 1] == b'/' {
                    depth -= 1;
                    pos += 2;
                } else {
                    pos += 1;
                }
            }
            if depth > 0 {
                unterminated_block_comment = true;
                pos = bytes.len();
            }
            replace_comment_bytes(&mut output[start..pos]);
            continue;
        }
        pos += 1;
    }
    StrippedVectorComments {
        input: String::from_utf8(output).expect("comment replacement preserves UTF-8 boundaries"),
        unterminated_block_comment,
    }
}

fn replace_comment_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        if *byte != b'\n' && *byte != b'\r' {
            *byte = b' ';
        }
    }
}

fn parse(query: &str) -> Result<Vec<IndexDdlStatement>, IndexDdlParseError> {
    let mut cur = Cursor::new(query);
    let mut statements = Vec::new();
    loop {
        statements.push(parse_statement(&mut cur)?);
        if !cur.try_consume_ascii_ci("NEXT") {
            break;
        }
    }
    cur.skip_ws();
    if !cur.is_eof() {
        return Err(IndexDdlParseError::TrailingInput);
    }
    Ok(statements)
}

fn parse_vector(query: &str) -> Result<VectorIndexDdlStatement, VectorIndexDdlParseError> {
    let mut cur = Cursor::new(query);
    cur.skip_ws();
    cur.expect_ascii_ci("CREATE")?;
    cur.expect_ascii_ci("VECTOR")?;
    cur.expect_ascii_ci("INDEX")?;
    let index_name = cur.parse_ident()?;
    let if_not_exists = if cur.try_consume_ascii_ci("IF") {
        cur.expect_ascii_ci("NOT")?;
        cur.expect_ascii_ci("EXISTS")?;
        true
    } else {
        false
    };
    cur.expect_ascii_ci("FOR")?;
    let (variable, label) = parse_vector_vertex_pattern(&mut cur)?;
    cur.expect_ascii_ci("ON")?;
    let (on_variable, embedding_name) = parse_vector_embedding_ref(&mut cur)?;
    if on_variable != variable {
        return Err(VectorIndexDdlParseError::VariableMismatch);
    }
    cur.expect_ascii_ci("OPTIONS")?;
    let (dims, metric, encoding, kind) = parse_vector_options(&mut cur)?;
    cur.try_consume(';');
    cur.skip_ws();
    if !cur.is_eof() {
        return Err(VectorIndexDdlParseError::TrailingInput);
    }
    Ok(VectorIndexDdlStatement::Create {
        index_name,
        if_not_exists,
        target: VectorIndexTarget {
            label,
            embedding_name,
            dims,
            metric,
            encoding,
            kind,
        },
    })
}

fn parse_vector_vertex_pattern(
    cur: &mut Cursor<'_>,
) -> Result<(String, String), VectorIndexDdlParseError> {
    cur.expect('(')?;
    cur.skip_ws();
    if cur.peek() == Some(')') {
        return Err(VectorIndexDdlParseError::EdgePatternUnsupported);
    }
    let variable = cur.parse_ident()?;
    cur.expect(':')?;
    let label = cur.parse_ident()?;
    cur.expect(')')?;
    Ok((variable, label))
}

fn parse_vector_embedding_ref(
    cur: &mut Cursor<'_>,
) -> Result<(String, String), VectorIndexDdlParseError> {
    let variable = cur.parse_ident()?;
    cur.expect('.')?;
    let embedding_name = cur.parse_ident()?;
    Ok((variable, embedding_name))
}

fn parse_vector_options(
    cur: &mut Cursor<'_>,
) -> Result<(u16, VectorMetric, VectorEncoding, VectorIndexKind), VectorIndexDdlParseError> {
    cur.expect('{')?;
    let mut options = ParsedVectorOptions::default();
    parse_vector_option_map(cur, &mut options, true)?;

    Ok((
        options
            .dims
            .ok_or_else(|| VectorIndexDdlParseError::MissingOption("dimensions".into()))?,
        options
            .metric
            .ok_or_else(|| VectorIndexDdlParseError::MissingOption("metric".into()))?,
        options
            .encoding
            .ok_or_else(|| VectorIndexDdlParseError::MissingOption("encoding".into()))?,
        options
            .kind
            .ok_or_else(|| VectorIndexDdlParseError::MissingOption("algorithm".into()))?,
    ))
}

#[derive(Default)]
struct ParsedVectorOptions {
    dims: Option<u16>,
    metric: Option<VectorMetric>,
    encoding: Option<VectorEncoding>,
    kind: Option<VectorIndexKind>,
}

fn parse_vector_option_map(
    cur: &mut Cursor<'_>,
    options: &mut ParsedVectorOptions,
    allow_index_config: bool,
) -> Result<(), VectorIndexDdlParseError> {
    let mut index_config_seen = false;

    loop {
        cur.skip_ws();
        if cur.try_consume('}') {
            break;
        }
        let option = cur.parse_ident()?.to_ascii_lowercase();
        cur.expect(':')?;
        if option == "indexconfig" && allow_index_config {
            if index_config_seen {
                return Err(VectorIndexDdlParseError::DuplicateOption(option));
            }
            index_config_seen = true;
            cur.expect('{')?;
            parse_vector_option_map(cur, options, false)?;
        } else {
            parse_vector_option(cur, options, option)?;
        }

        cur.skip_ws();
        if cur.try_consume('}') {
            break;
        }
        cur.expect(',')?;
        cur.skip_ws();
        if cur.peek() == Some('}') {
            return Err(VectorIndexDdlParseError::Expected("option name".into()));
        }
    }

    Ok(())
}

fn parse_vector_option(
    cur: &mut Cursor<'_>,
    options: &mut ParsedVectorOptions,
    option: String,
) -> Result<(), VectorIndexDdlParseError> {
    match option.as_str() {
        "dimensions" => {
            if options.dims.is_some() {
                return Err(VectorIndexDdlParseError::DuplicateOption(option));
            }
            options.dims = Some(parse_vector_dimensions(cur)?);
        }
        "metric" | "similarity_function" => {
            if options.metric.is_some() {
                return Err(VectorIndexDdlParseError::DuplicateOption(option));
            }
            let value = parse_vector_option_string(cur)?;
            options.metric = Some(match value.as_str() {
                "l2_squared" => VectorMetric::L2Squared,
                "cosine" => VectorMetric::Cosine,
                _ => {
                    return Err(VectorIndexDdlParseError::InvalidOptionValue { option, value });
                }
            });
        }
        "encoding" => {
            if options.encoding.is_some() {
                return Err(VectorIndexDdlParseError::DuplicateOption(option));
            }
            let value = parse_vector_option_string(cur)?;
            options.encoding = Some(match value.as_str() {
                "f32" => VectorEncoding::F32,
                "i8" => VectorEncoding::I8,
                _ => {
                    return Err(VectorIndexDdlParseError::InvalidOptionValue { option, value });
                }
            });
        }
        "algorithm" => {
            if options.kind.is_some() {
                return Err(VectorIndexDdlParseError::DuplicateOption(option));
            }
            let value = parse_vector_option_string(cur)?;
            options.kind = Some(match value.as_str() {
                "ivf_flat" => VectorIndexKind::IvfFlat,
                _ => {
                    return Err(VectorIndexDdlParseError::InvalidOptionValue { option, value });
                }
            });
        }
        _ => return Err(VectorIndexDdlParseError::UnsupportedOption(option)),
    }
    Ok(())
}

fn parse_vector_dimensions(cur: &mut Cursor<'_>) -> Result<u16, VectorIndexDdlParseError> {
    cur.skip_ws();
    let start = cur.pos;
    while matches!(cur.peek(), Some(ch) if ch.is_ascii_digit()) {
        cur.pos += 1;
    }
    if start == cur.pos {
        return Err(VectorIndexDdlParseError::InvalidDimensions);
    }
    let value = std::str::from_utf8(&cur.bytes[start..cur.pos])
        .expect("ASCII digits are valid UTF-8")
        .parse::<u16>()
        .map_err(|_| VectorIndexDdlParseError::InvalidDimensions)?;
    if value == 0 {
        return Err(VectorIndexDdlParseError::InvalidDimensions);
    }
    Ok(value)
}

fn parse_vector_option_string(cur: &mut Cursor<'_>) -> Result<String, VectorIndexDdlParseError> {
    cur.skip_ws();
    let quote = match cur.peek() {
        Some(quote @ ('"' | '\'')) => {
            cur.pos += 1;
            quote
        }
        _ => return Err(VectorIndexDdlParseError::Expected("quoted string".into())),
    };
    let start = cur.pos;
    while let Some(ch) = cur.peek() {
        if ch == quote {
            let value = std::str::from_utf8(&cur.bytes[start..cur.pos])
                .expect("string token is a UTF-8 substring")
                .to_owned();
            cur.pos += 1;
            return Ok(value);
        }
        cur.pos += 1;
    }
    Err(VectorIndexDdlParseError::Expected("closing quote".into()))
}

fn parse_constraint(query: &str) -> Result<ConstraintDdlStatement, ConstraintDdlParseError> {
    let mut cur = Cursor::new(query);
    cur.skip_ws();
    if cur.consume_ascii_ci("CREATE") {
        cur.expect_ascii_ci("CONSTRAINT")?;
        let constraint_name = cur.parse_ident()?;
        let if_not_exists = cur.try_consume_ascii_ci("IF NOT EXISTS");
        cur.expect_ascii_ci("FOR")?;
        let (var, label) = parse_constraint_vertex_pattern(&mut cur)?;
        cur.expect_ascii_ci("REQUIRE")?;
        let (required_var, property) = parse_constraint_property_ref(&mut cur)?;
        if required_var != var {
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
fn parse_constraint_vertex_pattern(
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

fn parse_constraint_property_ref(
    cur: &mut Cursor<'_>,
) -> Result<(String, String), ConstraintDdlParseError> {
    let var = cur.parse_ident()?;
    cur.expect('.')?;
    let property = cur.parse_ident()?;
    Ok((var, property))
}

fn parse_statement(cur: &mut Cursor<'_>) -> Result<IndexDdlStatement, IndexDdlParseError> {
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
        let (kind, label, edge_direction) = parse_for_pattern(cur)?;
        cur.skip_ws();
        cur.expect_ascii_ci("ON")?;
        let property = parse_on_property(cur)?;
        cur.skip_ws();
        cur.try_consume(';');
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
    Ok(cur.parse_ident()?)
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

struct CursorExpected(String);

impl From<CursorExpected> for IndexDdlParseError {
    fn from(error: CursorExpected) -> Self {
        Self::Expected(error.0)
    }
}

impl From<CursorExpected> for ConstraintDdlParseError {
    fn from(error: CursorExpected) -> Self {
        Self::Expected(error.0)
    }
}

impl From<CursorExpected> for VectorIndexDdlParseError {
    fn from(error: CursorExpected) -> Self {
        Self::Expected(error.0)
    }
}

impl From<CursorExpected> for TextIndexDdlParseError {
    fn from(error: CursorExpected) -> Self {
        Self::Expected(error.0)
    }
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

    fn expect_ascii_ci(&mut self, word: &str) -> Result<(), CursorExpected> {
        if self.consume_ascii_ci(word) {
            Ok(())
        } else {
            Err(CursorExpected(word.to_string()))
        }
    }

    fn expect(&mut self, ch: char) -> Result<(), CursorExpected> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(CursorExpected(ch.to_string()))
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

    fn parse_ident(&mut self) -> Result<String, CursorExpected> {
        self.skip_ws();
        let start = self.pos;
        let first = self
            .peek()
            .ok_or_else(|| CursorExpected("identifier".into()))?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return Err(CursorExpected("identifier".into()));
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
            .map_err(|_| CursorExpected("identifier".into()))?;
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(query: &str) -> IndexDdlStatement {
        let statements = try_parse(query).expect("index DDL").expect("parse");
        assert_eq!(statements.len(), 1, "expected one statement: {query}");
        statements.into_iter().next().expect("one statement")
    }

    #[test]
    fn parses_vertex_and_edge_create_index() {
        let IndexDdlStatement::Create {
            index_name,
            if_not_exists,
            target,
        } = parse_one("CREATE INDEX person_age FOR (n:Person) ON (n.age)")
        else {
            panic!("expected vertex index");
        };
        assert_eq!(index_name, "person_age");
        assert!(!if_not_exists);
        assert_eq!(target.kind, IndexedPropertyKind::Vertex);
        assert_eq!(target.label, "Person");
        assert_eq!(target.property, "age");

        let IndexDdlStatement::Create { target, .. } =
            parse_one("CREATE INDEX knows_weight FOR ()-[e:KNOWS]->() ON (e.weight)")
        else {
            panic!("expected edge index");
        };
        assert_eq!(target.kind, IndexedPropertyKind::Edge);
        assert_eq!(target.edge_direction, Some(EdgeDirection::PointingRight));
    }

    #[test]
    fn parses_next_chained_create_index_sequence_in_order() {
        let statements = try_parse(
            "CREATE INDEX a FOR (n:Person) ON (n.age)\nNEXT CREATE INDEX b FOR (n:Post) ON (n.demo_id);\nNEXT CREATE INDEX c FOR ()-[e:KNOWS]-() ON (e.weight)",
        )
        .expect("index DDL")
        .expect("parse");
        assert_eq!(statements.len(), 3);
        let names: Vec<_> = statements
            .iter()
            .map(|statement| match statement {
                IndexDdlStatement::Create { index_name, .. } => index_name.as_str(),
                _ => panic!("expected creates"),
            })
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        let IndexDdlStatement::Create { target, .. } = &statements[1] else {
            panic!("expected create");
        };
        assert_eq!(target.property, "demo_id");

        // A trailing semicolon after the final statement is tolerated, as in single-statement DDL.
        let statements = try_parse("CREATE INDEX a FOR (n:Person) ON (n.age);")
            .expect("index DDL")
            .expect("parse");
        assert_eq!(statements.len(), 1);
    }

    #[test]
    fn rejects_semicolon_without_next_and_malformed_chains() {
        // `;` is only a trailing terminator; NEXT is the only chain operator.
        assert_eq!(
            try_parse("CREATE INDEX a FOR (n:Person) ON (n.age); CREATE INDEX b FOR (n:Post) ON (n.demo_id)")
                .expect("index DDL")
                .expect_err("semicolon-only chain"),
            IndexDdlParseError::TrailingInput
        );
        // A dangling NEXT has no following statement.
        assert!(
            try_parse("CREATE INDEX a FOR (n:Person) ON (n.age) NEXT")
                .expect("index DDL")
                .is_err()
        );
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
            let IndexDdlStatement::Create { target, .. } = parse_one(ddl) else {
                panic!("expected edge index: {ddl}");
            };
            assert_eq!(target.edge_direction, Some(direction), "ddl: {ddl}");
        }
        let IndexDdlStatement::Create { if_not_exists, .. } =
            parse_one("CREATE INDEX w IF NOT EXISTS FOR (n:Person) ON (n.age)")
        else {
            panic!("expected IF NOT EXISTS");
        };
        assert!(if_not_exists);
    }

    #[test]
    fn preserves_nested_edge_property_and_rejects_slash_syntax() {
        let IndexDdlStatement::Create { target, .. } =
            parse_one("CREATE INDEX affinity FOR ()-[e:AFFINITY]-() ON (e.stats.score)")
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
        let IndexDdlStatement::Drop {
            index_name,
            if_exists,
        } = parse_one("DROP INDEX person_age IF EXISTS;")
        else {
            panic!("expected drop index");
        };
        assert_eq!(index_name, "person_age");
        assert!(if_exists);
        assert!(try_parse("MATCH (n) RETURN n").is_none());
    }

    #[test]
    fn accepts_leading_and_trailing_comments_without_changing_statement_shape() {
        let IndexDdlStatement::Create { target, .. } = parse_one(
            "// leading\n/* between */ CREATE INDEX person_age FOR (n:Person) ON (n.age) // trailing",
        ) else {
            panic!("expected commented index");
        };
        assert_eq!(target.kind, IndexedPropertyKind::Vertex);
        assert_eq!(target.label, "Person");
        assert_eq!(target.property, "age");

        let IndexDdlStatement::Create { index_name, .. } =
            parse_one("CREATE /* between */ INDEX person_age FOR (n:Person) ON (n.age)")
        else {
            panic!("expected comment between CREATE and INDEX");
        };
        assert_eq!(index_name, "person_age");
    }

    fn parse_vector_one(query: &str) -> VectorIndexDdlStatement {
        try_parse_vector(query)
            .expect("vector index DDL")
            .expect("parse")
    }

    #[test]
    fn parses_complete_neo4j_shaped_vector_index() {
        assert_eq!(
            parse_vector_one(
                r#"// leading comment
                   CREATE VECTOR INDEX document_embedding IF NOT EXISTS
                   FOR (d:Document)
                   ON d.embedding
                   OPTIONS {
                     DIMENSIONS: 768,
                     MeTrIc: "cosine",
                     encoding: "i8",
                     algorithm: "ivf_flat"
                   }; // trailing comment"#
            ),
            VectorIndexDdlStatement::Create {
                index_name: "document_embedding".into(),
                if_not_exists: true,
                target: VectorIndexTarget {
                    label: "Document".into(),
                    embedding_name: "embedding".into(),
                    dims: 768,
                    metric: VectorMetric::Cosine,
                    encoding: VectorEncoding::I8,
                    kind: VectorIndexKind::IvfFlat,
                },
            }
        );
    }

    #[test]
    fn vector_index_recognition_accepts_trivia_between_header_and_optional_keywords() {
        let statement = parse_vector_one(
            r#"CREATE
               /* before vector */ VECTOR // before index
               INDEX document_embedding IF
               /* before not */ NOT -- before exists
               EXISTS FOR (d:Document) ON d.embedding
               OPTIONS { dimensions: 768, metric: "cosine", encoding: "i8", algorithm: "ivf_flat" }"#,
        );
        let VectorIndexDdlStatement::Create {
            index_name,
            if_not_exists,
            ..
        } = statement;
        assert_eq!(index_name, "document_embedding");
        assert!(if_not_exists);
    }

    #[test]
    fn vector_index_recognition_requires_exact_keyword_boundaries() {
        assert!(try_parse_vector("MATCH (n) RETURN n").is_none());
        assert!(try_parse_vector("CREATE INDEX x FOR (n:N) ON (n.p)").is_none());
        assert!(try_parse_vector("CREATE VECTOR INDEXED x").is_none());
    }

    #[test]
    fn vector_index_rejects_unterminated_block_comment() {
        assert_eq!(
            try_parse_vector("CREATE VECTOR INDEX document_embedding /* unterminated")
                .expect("vector index DDL")
                .expect_err("unterminated block comment"),
            VectorIndexDdlParseError::Expected("closing block comment".into())
        );
    }

    #[test]
    fn parses_neo4j_shaped_index_config_with_similarity_alias_and_single_quoted_values() {
        assert_eq!(
            parse_vector_one(
                r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
                   OPTIONS { indexConfig: {
                     dimensions: 768,
                     similarity_function: 'cosine',
                     encoding: 'i8',
                     algorithm: 'ivf_flat'
                   } }"#
            ),
            VectorIndexDdlStatement::Create {
                index_name: "document_embedding".into(),
                if_not_exists: false,
                target: VectorIndexTarget {
                    label: "Document".into(),
                    embedding_name: "embedding".into(),
                    dims: 768,
                    metric: VectorMetric::Cosine,
                    encoding: VectorEncoding::I8,
                    kind: VectorIndexKind::IvfFlat,
                },
            }
        );
    }

    #[test]
    fn vector_index_rejects_metric_alias_conflicts() {
        assert_eq!(
            try_parse_vector(
                r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
                   OPTIONS { indexConfig: {
                     dimensions: 768,
                     metric: "cosine",
                     similarity_function: "cosine",
                     encoding: "i8",
                     algorithm: "ivf_flat"
                   } }"#
            )
            .expect("vector index DDL")
            .expect_err("metric alias conflict"),
            VectorIndexDdlParseError::DuplicateOption("similarity_function".into())
        );
    }

    #[test]
    fn vector_index_comment_stripping_preserves_single_quoted_values() {
        assert_eq!(
            try_parse_vector(
                r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
                   OPTIONS {
                     dimensions: 768,
                     metric: 'not--valid',
                     encoding: 'i8',
                     algorithm: 'ivf_flat'
                   }"#
            )
            .expect("vector index DDL")
            .expect_err("invalid metric"),
            VectorIndexDdlParseError::InvalidOptionValue {
                option: "metric".into(),
                value: "not--valid".into(),
            }
        );
    }

    #[test]
    fn vector_index_requires_exact_vertex_shape_and_all_options() {
        let missing_option = try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
               OPTIONS { dimensions: 768, metric: "l2_squared", encoding: "f32" }"#,
        )
        .expect("vector index DDL")
        .expect_err("missing algorithm");
        assert_eq!(
            missing_option,
            VectorIndexDdlParseError::MissingOption("algorithm".into())
        );

        let edge_pattern = try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding FOR ()-[e:REL]->() ON e.embedding
               OPTIONS { dimensions: 768, metric: "l2_squared", encoding: "f32", algorithm: "ivf_flat" }"#,
        )
        .expect("vector index DDL")
        .expect_err("edge pattern");
        assert_eq!(
            edge_pattern,
            VectorIndexDdlParseError::EdgePatternUnsupported
        );

        let variable_mismatch = try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON e.embedding
               OPTIONS { dimensions: 768, metric: "l2_squared", encoding: "f32", algorithm: "ivf_flat" }"#,
        )
        .expect("vector index DDL")
        .expect_err("variable mismatch");
        assert_eq!(
            variable_mismatch,
            VectorIndexDdlParseError::VariableMismatch
        );
    }

    #[test]
    fn vector_index_rejects_invalid_or_duplicated_options() {
        let duplicate = try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
               OPTIONS { dimensions: 768, dimensions: 512, metric: "l2_squared", encoding: "f32", algorithm: "ivf_flat" }"#,
        )
        .expect("vector index DDL")
        .expect_err("duplicate dimensions");
        assert_eq!(
            duplicate,
            VectorIndexDdlParseError::DuplicateOption("dimensions".into())
        );

        let unknown = try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
               OPTIONS { dimensions: 768, metric: "l2_squared", encoding: "f32", algorithm: "ivf_flat", nlist: 1 }"#,
        )
        .expect("vector index DDL")
        .expect_err("unknown option");
        assert_eq!(
            unknown,
            VectorIndexDdlParseError::UnsupportedOption("nlist".into())
        );

        let invalid_dimensions = try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding FOR (d:Document) ON d.embedding
               OPTIONS { dimensions: 0, metric: "l2_squared", encoding: "f32", algorithm: "ivf_flat" }"#,
        )
        .expect("vector index DDL")
        .expect_err("zero dimensions");
        assert_eq!(
            invalid_dimensions,
            VectorIndexDdlParseError::InvalidDimensions
        );
    }

    fn parse_constraint_ok(query: &str) -> ConstraintDdlStatement {
        try_parse_constraint(query)
            .expect("constraint DDL")
            .expect("parse")
    }

    #[test]
    fn parses_create_constraint_with_and_without_if_not_exists() {
        assert_eq!(
            parse_constraint_ok(
                "CREATE CONSTRAINT user_email IF NOT EXISTS FOR (n:User) REQUIRE n.email IS UNIQUE;"
            ),
            ConstraintDdlStatement::Create {
                constraint_name: "user_email".into(),
                if_not_exists: true,
                label: "User".into(),
                property: "email".into(),
            }
        );
        assert_eq!(
            parse_constraint_ok("CREATE CONSTRAINT c FOR (u:Account) REQUIRE u.handle IS UNIQUE"),
            ConstraintDdlStatement::Create {
                constraint_name: "c".into(),
                if_not_exists: false,
                label: "Account".into(),
                property: "handle".into(),
            }
        );
    }

    #[test]
    fn parses_drop_constraint_with_and_without_if_exists() {
        assert_eq!(
            parse_constraint_ok("DROP CONSTRAINT user_email IF EXISTS"),
            ConstraintDdlStatement::Drop {
                constraint_name: "user_email".into(),
                if_exists: true,
            }
        );
        assert_eq!(
            parse_constraint_ok("DROP CONSTRAINT user_email;"),
            ConstraintDdlStatement::Drop {
                constraint_name: "user_email".into(),
                if_exists: false,
            }
        );
    }

    #[test]
    fn constraint_rejects_edge_pattern_and_variable_mismatch() {
        let edge_error = try_parse_constraint(
            "CREATE CONSTRAINT c FOR ()-[r:KNOWS]-() REQUIRE r.weight IS UNIQUE",
        )
        .expect("constraint DDL")
        .expect_err("edge constraint");
        assert_eq!(
            edge_error,
            ConstraintDdlParseError::EdgeConstraintUnsupported
        );

        let mismatch_error =
            try_parse_constraint("CREATE CONSTRAINT c FOR (n:User) REQUIRE m.email IS UNIQUE")
                .expect("constraint DDL")
                .expect_err("variable mismatch");
        assert_eq!(mismatch_error, ConstraintDdlParseError::VariableMismatch);
    }

    #[test]
    fn constraint_rejects_trailing_input_and_incomplete_syntax() {
        let trailing_error = try_parse_constraint(
            "CREATE CONSTRAINT c FOR (n:User) REQUIRE n.email IS UNIQUE trailing",
        )
        .expect("constraint DDL")
        .expect_err("trailing input");
        assert_eq!(trailing_error, ConstraintDdlParseError::TrailingInput);

        let syntax_error =
            try_parse_constraint("CREATE CONSTRAINT c FOR (n:User) REQUIRE n.email IS")
                .expect("constraint DDL")
                .expect_err("missing UNIQUE");
        assert_eq!(
            syntax_error,
            ConstraintDdlParseError::Expected("UNIQUE".into())
        );
    }

    #[test]
    fn constraint_recognition_stays_separate_from_index_ddl() {
        assert!(try_parse_constraint("MATCH (n) RETURN n").is_none());
        assert!(try_parse_constraint("CREATE INDEX x FOR (n:N) ON (n.p)").is_none());
        assert!(try_parse("CREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE").is_none());
        assert!(
            try_parse_constraint("CREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE").is_some()
        );
    }

    #[test]
    fn constraint_does_not_accept_index_comment_or_next_extensions() {
        assert!(
            try_parse_constraint("// leading\nCREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE")
                .is_none()
        );

        assert!(
            try_parse_constraint(
                "CREATE /* between */ CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE"
            )
            .is_none()
        );

        let next_error = try_parse_constraint(
            "CREATE CONSTRAINT c FOR (n:N) REQUIRE n.p IS UNIQUE NEXT DROP CONSTRAINT c",
        )
        .expect("constraint DDL")
        .expect_err("NEXT is index-only");
        assert_eq!(next_error, ConstraintDdlParseError::TrailingInput);
    }

    #[test]
    fn text_ddl_parses_for_on_target_and_rejects_other_statements() {
        let parsed =
            try_parse_text("CREATE TEXT INDEX docs FOR (v:Person) ON (v.bio);").expect("text DDL");
        assert_eq!(
            parsed.expect("parse"),
            TextIndexDdlStatement::Create {
                index_name: "docs".into(),
                if_not_exists: false,
                label: "Person".into(),
                property: "bio".into(),
            }
        );
        // Case-insensitive keywords, optional semicolon.
        assert!(try_parse_text("create text index docs for (v:person) on (v.bio)").is_some());
        // Other vendor DDL and plain GQL are not text DDL.
        assert!(try_parse_text("CREATE INDEX x FOR (n:N) ON (n.p)").is_none());
        assert!(try_parse_text("DROP INDEX x IF EXISTS").is_none());
        assert!(try_parse_text("CREATE VECTOR INDEX v FOR (n:N) ON (n.e)").is_none());
        assert!(try_parse_text("MATCH (n) RETURN n").is_none());
        // Keyword boundaries: a longer identifier is not the TEXT INDEX header.
        assert!(try_parse_text("CREATE TEXT INDEXED x FOR (n:N) ON (n.p)").is_none());
        // Trailing junk rejects.
        let trailing =
            try_parse_text("CREATE TEXT INDEX docs FOR (v:Person) ON (v.bio) DROP TEXT INDEX x")
                .expect("recognized")
                .expect_err("trailing input");
        assert_eq!(trailing, TextIndexDdlParseError::TrailingInput);
    }

    #[test]
    fn text_ddl_parses_if_not_exists_between_name_and_for() {
        let parsed =
            try_parse_text("CREATE TEXT INDEX docs IF NOT EXISTS FOR (v:Person) ON (v.bio);")
                .expect("text DDL")
                .expect("parse");
        assert_eq!(
            parsed,
            TextIndexDdlStatement::Create {
                index_name: "docs".into(),
                if_not_exists: true,
                label: "Person".into(),
                property: "bio".into(),
            }
        );
    }

    #[test]
    fn text_ddl_parses_drop_with_and_without_if_exists() {
        let parsed = try_parse_text("DROP TEXT INDEX docs IF EXISTS;").expect("text DDL");
        assert_eq!(
            parsed.expect("parse"),
            TextIndexDdlStatement::Drop {
                index_name: "docs".into(),
                if_exists: true,
            }
        );
        assert_eq!(
            try_parse_text("drop text index docs")
                .expect("text DDL")
                .expect("parse"),
            TextIndexDdlStatement::Drop {
                index_name: "docs".into(),
                if_exists: false,
            }
        );
    }

    #[test]
    fn text_ddl_rejects_variable_mismatch_and_edge_patterns() {
        let mismatch = try_parse_text("CREATE TEXT INDEX docs FOR (n:Person) ON (m.bio)")
            .expect("text DDL")
            .expect_err("variable mismatch");
        assert_eq!(mismatch, TextIndexDdlParseError::VariableMismatch);

        let edge = try_parse_text("CREATE TEXT INDEX docs FOR ()-[r:KNOWS]->() ON (r.weight)")
            .expect("text DDL")
            .expect_err("edge pattern");
        assert_eq!(edge, TextIndexDdlParseError::EdgePatternUnsupported);
    }
}
