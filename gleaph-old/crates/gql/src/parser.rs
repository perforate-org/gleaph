use crate::ast::{
    AggFunc, AggregateExpr, BinaryOp, CaseExpr, CaseWhenThen, CmpOp, ConstraintDef, ConstraintKind,
    CreateStmt, DeleteStmt, Direction, EdgeCreate, EdgePattern, EdgeTypeDef, Expr,
    GraphTypeDefinition, LabelExpr, Limit, MatchChain, MatchClause, MergeStmt, NodeCreate,
    NodePattern, NodeTypeDef, OrderBy, OrderByItem, PathLength, PatternElement, PropertyDef,
    QueryStmt, RemoveClause, RemoveItem, RemoveStmt, ReturnClause, ReturnItem, SetClause, SetItem,
    SetOp, SetStmt, Statement, TruthValue, TypeExpr, UnaryOp, ValueType, WhereClause, WithClause,
    parse_value_type,
};
use crate::lexer::{Token, is_reserved_keyword, tokenize};
use gleaph_types::{GleaphError, Value};
use nom::{
    IResult, Parser,
    branch::alt,
    combinator::{map, opt},
    error::{ErrorKind, ParseError as NomParseError},
    sequence::separated_pair,
};

type Tokens<'a> = &'a [Token];
type TResult<'a, T> = IResult<Tokens<'a>, T, PError<'a>>;

#[derive(Clone, Debug)]
enum PError<'a> {
    Nom { input: Tokens<'a>, _kind: ErrorKind },
    Gleaph(GleaphError),
}

impl<'a> NomParseError<Tokens<'a>> for PError<'a> {
    fn from_error_kind(input: Tokens<'a>, kind: ErrorKind) -> Self {
        Self::Nom { input, _kind: kind }
    }

    fn append(input: Tokens<'a>, kind: ErrorKind, _other: Self) -> Self {
        Self::Nom { input, _kind: kind }
    }
}

fn perr<'a>(input: Tokens<'a>, kind: ErrorKind) -> nom::Err<PError<'a>> {
    nom::Err::Error(PError::Nom { input, _kind: kind })
}

fn fail<'a, T>(err: GleaphError) -> TResult<'a, T> {
    Err(nom::Err::Failure(PError::Gleaph(err)))
}

fn token<'a, F>(pred: F) -> impl FnMut(Tokens<'a>) -> TResult<'a, Token>
where
    F: Fn(&Token) -> bool,
{
    move |input: Tokens<'a>| match input.split_first() {
        Some((tok, rest)) if pred(tok) => Ok((rest, tok.clone())),
        _ => Err(perr(input, ErrorKind::Tag)),
    }
}

fn kw<'a>(word: &'static str) -> impl FnMut(Tokens<'a>) -> TResult<'a, ()> {
    move |input: Tokens<'a>| {
        let (rest, _) =
            token(move |t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(word)))(input)?;
        Ok((rest, ()))
    }
}

fn punct<'a>(want: Token) -> impl FnMut(Tokens<'a>) -> TResult<'a, ()> {
    move |input: Tokens<'a>| match input.split_first() {
        Some((tok, rest)) if tok == &want => Ok((rest, ())),
        _ => Err(perr(input, ErrorKind::Tag)),
    }
}

fn ident<'a>(input: Tokens<'a>) -> TResult<'a, String> {
    match input.split_first() {
        Some((Token::Ident(s) | Token::QuotedIdent(s), rest)) => Ok((rest, s.clone())),
        _ => Err(perr(input, ErrorKind::Tag)),
    }
}

fn ident_non_kw<'a>(input: Tokens<'a>) -> TResult<'a, String> {
    match input.split_first() {
        // Backtick-quoted identifiers are never keywords, always valid.
        Some((Token::QuotedIdent(s), rest)) => Ok((rest, s.clone())),
        Some((Token::Ident(s), rest)) if !is_reserved_keyword(s) => Ok((rest, s.clone())),
        _ => Err(perr(input, ErrorKind::Verify)),
    }
}

fn starts_kw(input: Tokens<'_>, word: &str) -> bool {
    matches!(input.first(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
}

fn starts_edge(input: Tokens<'_>) -> bool {
    matches!(
        input.first(),
        Some(Token::Minus) | Some(Token::ArrowLeft) | Some(Token::Tilde)
    ) || matches!(
        (input.first(), input.get(1)),
        (Some(Token::Lt), Some(Token::Tilde))
    )
}

fn parse_u32_limit_value<'a>(input: Tokens<'a>) -> TResult<'a, u32> {
    match input.split_first() {
        Some((Token::Int(v), rest)) if (0..=u32::MAX as i64).contains(v) => Ok((rest, *v as u32)),
        Some((Token::Int(_), _)) => fail(GleaphError::ParseError(
            "LIMIT exceeds maximum supported value".into(),
        )),
        _ => fail(GleaphError::ParseError(
            "LIMIT expects a non-negative integer".into(),
        )),
    }
}

fn parse_hop_count<'a>(input: Tokens<'a>, which: &'static str) -> TResult<'a, u32> {
    match input.split_first() {
        Some((Token::Int(v), rest)) if *v >= 1 => Ok((rest, *v as u32)),
        Some((Token::Int(_), _)) => fail(GleaphError::ValidationError(format!(
            "variable-length path {which} must be >= 1"
        ))),
        _ => fail(GleaphError::ParseError(format!(
            "expected {which} hop count after {}",
            if which == "min" { "*" } else { ".." }
        ))),
    }
}

fn parse_statement_nom<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (mut input, mut stmt) = parse_simple_statement_nom(input)?;
    let mut branches = 1usize;
    loop {
        let (i, op) = parse_set_op(input)?;
        let Some(op) = op else { break };
        branches += 1;
        if branches > 16 {
            return fail(GleaphError::ValidationError(
                "compound query exceeds MAX_UNION_BRANCHES (16)".into(),
            ));
        }
        let (i, right) = parse_simple_statement_nom(i)?;
        stmt = Statement::Compound {
            op,
            left: Box::new(stmt),
            right: Box::new(right),
        };
        input = i;
    }
    Ok((input, stmt))
}

fn parse_simple_statement_nom<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    // §15.2: OPTIONAL CALL { ... } — optional inline subquery
    if starts_kw(input, "OPTIONAL")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("CALL")))
    {
        let (input, _) = kw("OPTIONAL")(input)?;
        return parse_call_statement_with_optional(input, true);
    }
    if starts_kw(input, "OPTIONAL") {
        return optional_match_led_statement(input);
    }
    if starts_kw(input, "FOR") {
        return parse_for_statement(input);
    }
    if starts_kw(input, "SELECT") {
        return parse_select_statement(input);
    }
    if starts_kw(input, "CALL") {
        return parse_call_statement_with_optional(input, false);
    }
    // §12: DESCRIBE GRAPH TYPE name
    if starts_kw(input, "DESCRIBE")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("GRAPH")))
        && input
            .get(2)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("TYPE")))
    {
        return parse_describe_graph_type(input);
    }
    // §16.2: USE [GRAPH] name
    if starts_kw(input, "USE") {
        return parse_use_graph_statement(input);
    }
    // §12: CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE name (before CREATE GRAPH to avoid consuming TYPE as graph name)
    if starts_kw(input, "CREATE") && lookahead_graph_type(input) {
        return parse_create_graph_type_statement(input);
    }
    // §12: CREATE [PROPERTY] GRAPH name (before generic CREATE which handles CREATE (...))
    if starts_kw(input, "CREATE") && lookahead_graph(input) {
        return parse_create_graph_statement(input);
    }
    // §12: CREATE SCHEMA name
    if starts_kw(input, "CREATE")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("SCHEMA")))
    {
        return parse_create_schema_statement(input);
    }
    // §12: DROP [PROPERTY] GRAPH TYPE / DROP [PROPERTY] GRAPH / DROP SCHEMA / DROP INDEX / DROP CONSTRAINT
    if starts_kw(input, "DROP") {
        if lookahead_drop_graph_type(input) {
            return parse_drop_graph_type_statement(input);
        }
        if input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("SCHEMA")))
        {
            return parse_drop_schema_statement(input);
        }
        if input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("INDEX")))
        {
            return parse_drop_index_statement(input);
        }
        if input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("CONSTRAINT")))
        {
            return parse_drop_constraint_statement(input);
        }
        if lookahead_drop_graph(input) {
            return parse_drop_graph_statement(input);
        }
    }
    // SHOW ...
    if starts_kw(input, "SHOW") {
        return parse_show_statement(input);
    }
    // GRANT ... ON GRAPH TO ...
    if starts_kw(input, "GRANT") {
        return parse_grant_statement(input);
    }
    // REVOKE ACCESS ON GRAPH FROM ...
    if starts_kw(input, "REVOKE") {
        return parse_revoke_statement(input);
    }
    // ANALYZE
    if starts_kw(input, "ANALYZE") {
        return parse_analyze_statement(input);
    }
    // SET TYPE CHECK STRICT|WARNING (§18.9 Phase 3)
    if starts_kw(input, "SET")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("TYPE")))
        && input
            .get(2)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("CHECK")))
    {
        return parse_set_type_check_statement(input);
    }
    // CREATE INDEX ON ...
    if starts_kw(input, "CREATE")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("INDEX")))
    {
        return parse_create_index_statement(input);
    }
    // CREATE CONSTRAINT name ON (:Label) ASSERT property IS UNIQUE|NOT NULL
    if starts_kw(input, "CREATE")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("CONSTRAINT")))
    {
        return parse_create_constraint_statement(input);
    }
    if starts_kw(input, "MERGE") {
        return merge_statement(input);
    }
    // Bare RETURN (no MATCH): wrap in a QueryStmt with no match entries and no WHERE.
    if starts_kw(input, "RETURN") {
        let (i, _) = kw("RETURN")(input)?;
        let (i, return_clause) = parse_return_clause(i)?;
        let stmt = Statement::Query(crate::ast::QueryStmt {
            match_mode: None,
            match_clauses: vec![],
            where_clause: None,
            with_clauses: vec![],
            return_clause,
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
        });
        return Ok((i, stmt));
    }
    alt((match_led_statement, insert_statement, bare_delete_statement)).parse(input)
}

fn parse_set_op<'a>(input: Tokens<'a>) -> TResult<'a, Option<SetOp>> {
    if starts_kw(input, "UNION") {
        let (input, _) = kw("UNION")(input)?;
        if starts_kw(input, "ALL") {
            let (input, _) = kw("ALL")(input)?;
            Ok((input, Some(SetOp::UnionAll)))
        } else {
            Ok((input, Some(SetOp::Union)))
        }
    } else if starts_kw(input, "EXCEPT") {
        let (input, _) = kw("EXCEPT")(input)?;
        Ok((input, Some(SetOp::Except)))
    } else if starts_kw(input, "INTERSECT") {
        let (input, _) = kw("INTERSECT")(input)?;
        Ok((input, Some(SetOp::Intersect)))
    } else if starts_kw(input, "OTHERWISE") {
        let (input, _) = kw("OTHERWISE")(input)?;
        Ok((input, Some(SetOp::Otherwise)))
    } else if starts_kw(input, "NEXT") {
        let (input, _) = kw("NEXT")(input)?;
        // §16.14: optional YIELD clause between NEXT stages.
        let (input, yield_cols) = if starts_kw(input, "YIELD") {
            let (i, _) = kw("YIELD")(input)?;
            if matches!(i.first(), Some(Token::Star)) {
                // YIELD * — pass all bindings (same as omitted).
                (&i[1..], None)
            } else {
                // Consume a comma-separated identifier list.
                let mut cols = Vec::new();
                let mut j = i;
                while let Some(Token::Ident(s) | Token::QuotedIdent(s)) = j.first() {
                    cols.push(s.clone());
                    j = &j[1..];
                    if matches!(j.first(), Some(Token::Comma)) {
                        j = &j[1..];
                    } else {
                        break;
                    }
                }
                (j, Some(cols))
            }
        } else {
            (input, None)
        };
        Ok((input, Some(SetOp::Next(yield_cols))))
    } else {
        Ok((input, None))
    }
}

fn bare_delete_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DELETE")(input)?;
    let _ = input;
    fail(GleaphError::ValidationError(
        "DELETE requires a preceding MATCH clause in Phase 2".into(),
    ))
}

fn insert_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    if starts_kw(input, "CREATE") {
        return fail(GleaphError::ParseError(
            "CREATE is reserved for DDL (CREATE GRAPH, CREATE GRAPH TYPE, CREATE SCHEMA). Use INSERT for data mutations.".into(),
        ));
    }
    let (mut input, _) = kw("INSERT")(input)?;
    let mut patterns = Vec::new();
    loop {
        let (i, left) = parse_node_pattern(input)?;
        if starts_edge(i) {
            let (i, edge) = parse_edge_pattern(i)?;
            let (i, right) = parse_node_pattern(i)?;
            patterns.push(CreateStmt::Edge(Box::new(EdgeCreate { left, edge, right })));
            input = i;
        } else {
            patterns.push(CreateStmt::Node(NodeCreate { node: left }));
            input = i;
        }
        if matches!(input.first(), Some(Token::Comma)) {
            input = &input[1..];
        } else {
            break;
        }
    }
    Ok((input, Statement::Create(patterns)))
}

fn merge_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("MERGE")(input)?;
    let (i, left) = parse_node_pattern(input)?;
    let (input, create) = if starts_edge(i) {
        let (i, edge) = parse_edge_pattern(i)?;
        let (i, right) = parse_node_pattern(i)?;
        (
            i,
            CreateStmt::Edge(Box::new(EdgeCreate { left, edge, right })),
        )
    } else {
        (i, CreateStmt::Node(NodeCreate { node: left }))
    };
    // Optional: ON CREATE SET items
    let (input, on_create_set) = if starts_kw(input, "ON")
        && matches!(input.get(1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("CREATE"))
        && matches!(input.get(2), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("SET"))
    {
        let (i, _) = kw("ON")(input)?;
        let (i, _) = kw("CREATE")(i)?;
        let (i, _) = kw("SET")(i)?;
        let (i, clause) = parse_set_clause(i)?;
        (i, clause.items)
    } else {
        (input, Vec::new())
    };
    // Optional: ON MATCH SET items
    let (input, on_match_set) = if starts_kw(input, "ON")
        && matches!(input.get(1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("MATCH"))
        && matches!(input.get(2), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("SET"))
    {
        let (i, _) = kw("ON")(input)?;
        let (i, _) = kw("MATCH")(i)?;
        let (i, _) = kw("SET")(i)?;
        let (i, clause) = parse_set_clause(i)?;
        (i, clause.items)
    } else {
        (input, Vec::new())
    };
    Ok((
        input,
        Statement::Merge(MergeStmt {
            create,
            on_create_set,
            on_match_set,
        }),
    ))
}

/// Try to consume an opening `{` or `(` for GQL `OPTIONAL { ... }` / `OPTIONAL ( ... )` syntax.
/// Returns `(remaining, braced)` where `braced` is `true` if a brace/paren was consumed.
fn opt_optional_brace_open(input: Tokens) -> (Tokens, bool) {
    if matches!(input.first(), Some(Token::LBrace) | Some(Token::LParen)) {
        (&input[1..], true)
    } else {
        (input, false)
    }
}

/// Consume the closing `}` or `)` for a braced OPTIONAL block.
fn consume_optional_brace_close<'a>(input: Tokens<'a>) -> TResult<'a, ()> {
    if matches!(input.first(), Some(Token::RBrace | Token::RParen)) {
        Ok((&input[1..], ()))
    } else {
        Err(perr(input, ErrorKind::Tag))
    }
}

fn match_led_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    match_led_statement_with_first_optional(input, false)
}

fn optional_match_led_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    match_led_statement_with_first_optional(input, true)
}

fn match_led_statement_with_first_optional<'a>(
    input: Tokens<'a>,
    first_optional: bool,
) -> TResult<'a, Statement> {
    let (input, first_optional_parsed, first_optional_braced) = if first_optional {
        let (i, _) = kw("OPTIONAL")(input)?;
        // GQL standard: OPTIONAL MATCH ... or OPTIONAL { MATCH ... } or OPTIONAL ( MATCH ... )
        let (i, braced) = opt_optional_brace_open(i);
        let (i, _) = kw("MATCH")(i)?;
        (i, true, braced)
    } else {
        let (i, _) = kw("MATCH")(input)?;
        (i, false, false)
    };
    // §16.4: Parse optional match mode (REPEATABLE ELEMENTS | DIFFERENT EDGES).
    let (input, match_mode) = if starts_kw(input, "REPEATABLE")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("ELEMENTS")))
    {
        let (i, _) = kw("REPEATABLE")(input)?;
        let (i, _) = kw("ELEMENTS")(i)?;
        (i, Some(crate::ast::MatchMode::RepeatableElements))
    } else if starts_kw(input, "DIFFERENT")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("EDGES")))
    {
        let (i, _) = kw("DIFFERENT")(input)?;
        let (i, _) = kw("EDGES")(i)?;
        (i, Some(crate::ast::MatchMode::DifferentEdges))
    } else {
        (input, None)
    };
    let mut input = input;
    let (
        i2,
        (shortest, shortest_mode, path_variable, path_mode, match_clause, any_paths, keep_clause),
    ) = parse_match_entry_clause(input)?;
    input = i2;
    // If the first OPTIONAL used braces, consume the closing brace after the match clause.
    if first_optional_braced {
        let (i, _) = consume_optional_brace_close(input)?;
        input = i;
    }
    let mut match_entries = vec![crate::ast::MatchEntry {
        optional: first_optional_parsed,
        shortest,
        shortest_mode,
        path_variable,
        path_mode,
        pattern: match_clause.clone(),
        any_paths,
        keep_clause,
    }];
    while starts_kw(input, "OPTIONAL") {
        let (i, _) = kw("OPTIONAL")(input)?;
        let (i, braced) = opt_optional_brace_open(i);
        let (i, _) = kw("MATCH")(i)?;
        let (i, (shortest, shortest_mode, path_variable, path_mode, m, any_paths, keep_clause)) =
            parse_match_entry_clause(i)?;
        let i = if braced {
            let (i, _) = consume_optional_brace_close(i)?;
            i
        } else {
            i
        };
        match_entries.push(crate::ast::MatchEntry {
            optional: true,
            shortest,
            shortest_mode,
            path_variable,
            path_mode,
            pattern: m,
            any_paths,
            keep_clause,
        });
        input = i;
    }

    let where_clause = if starts_kw(input, "WHERE") {
        let (i, _) = kw("WHERE")(input)?;
        let (i, where_clause) = parse_where_clause(i)?;
        input = i;
        Some(where_clause)
    } else {
        None
    };

    let mut with_clauses = Vec::new();
    while starts_kw(input, "WITH") {
        let (i, _) = kw("WITH")(input)?;
        let (i, mut with_clause) = parse_with_clause(i)?;
        input = i;

        // Parse optional follow-on MATCH/OPTIONAL MATCH clauses (WITH … MATCH … continuation).
        while starts_kw(input, "MATCH") || starts_kw(input, "OPTIONAL") {
            let (i, optional, braced) = if starts_kw(input, "OPTIONAL") {
                let (i, _) = kw("OPTIONAL")(input)?;
                let (i, braced) = opt_optional_brace_open(i);
                let (i, _) = kw("MATCH")(i)?;
                (i, true, braced)
            } else {
                let (i, _) = kw("MATCH")(input)?;
                (i, false, false)
            };
            let (i, (shortest, shortest_mode, path_variable, path_mode, m, any_paths, keep_clause)) =
                parse_match_entry_clause(i)?;
            let i = if braced {
                let (i, _) = consume_optional_brace_close(i)?;
                i
            } else {
                i
            };
            with_clause.match_clauses.push(crate::ast::MatchEntry {
                optional,
                shortest,
                shortest_mode,
                path_variable,
                path_mode,
                pattern: m,
                any_paths,
                keep_clause,
            });
            input = i;
        }
        // Parse optional WHERE after the follow-on MATCHes.
        if !with_clause.match_clauses.is_empty() && starts_kw(input, "WHERE") {
            let (i, _) = kw("WHERE")(input)?;
            let (i, w) = parse_where_clause(i)?;
            with_clause.post_match_where = Some(w);
            input = i;
        }

        with_clauses.push(with_clause);
    }

    if starts_kw(input, "RETURN") {
        let (i, _) = kw("RETURN")(input)?;
        let (i, return_clause) = parse_return_clause(i)?;
        input = i;

        let group_by = if starts_kw(input, "GROUP") {
            let (i, _) = kw("GROUP")(input)?;
            let (i, _) = kw("BY")(i)?;
            let (i, group_by) = parse_expr_list(i)?;
            input = i;
            Some(group_by)
        } else {
            None
        };

        let having = if starts_kw(input, "HAVING") {
            let (i, _) = kw("HAVING")(input)?;
            let (i, h) = parse_expr(i)?;
            input = i;
            Some(h)
        } else {
            None
        };

        let order_by = if starts_kw(input, "ORDER") {
            let (i, _) = kw("ORDER")(input)?;
            let (i, _) = kw("BY")(i)?;
            let (i, order_by) = parse_order_by(i)?;
            input = i;
            Some(order_by)
        } else {
            None
        };

        let limit = if starts_kw(input, "LIMIT") {
            let (i, _) = kw("LIMIT")(input)?;
            let (i, limit) = parse_limit(i)?;
            input = i;
            Some(limit)
        } else {
            None
        };
        let offset = if starts_kw(input, "OFFSET") {
            let (i, _) = kw("OFFSET")(input)?;
            let (i, off) = parse_u32_limit_value(i)?;
            input = i;
            Some(off)
        } else {
            None
        };

        Ok((
            input,
            Statement::Query(QueryStmt {
                match_clauses: match_entries,
                where_clause,
                with_clauses,
                return_clause,
                group_by,
                having,
                order_by,
                limit,
                offset,
                match_mode,
            }),
        ))
    } else if starts_kw(input, "DELETE")
        || starts_kw(input, "DETACH")
        || starts_kw(input, "NODETACH")
    {
        let (i, detach, nodetach) = if starts_kw(input, "DETACH") {
            let (i, _) = kw("DETACH")(input)?;
            let (i, _) = kw("DELETE")(i)?;
            (i, true, false)
        } else if starts_kw(input, "NODETACH") {
            let (i, _) = kw("NODETACH")(input)?;
            let (i, _) = kw("DELETE")(i)?;
            (i, false, true)
        } else {
            let (i, _) = kw("DELETE")(input)?;
            (i, false, false)
        };
        let mut target_vars = Vec::new();
        let (mut i, first_var) = ident(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected identifier after DELETE".into(),
            )))
        })?;
        target_vars.push(first_var);
        while matches!(i.first(), Some(Token::Comma)) {
            let rest = &i[1..];
            let (rest, var) = ident(rest).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier after ',' in DELETE".into(),
                )))
            })?;
            target_vars.push(var);
            i = rest;
        }
        Ok((
            i,
            Statement::Delete(DeleteStmt {
                match_clause,
                where_clause,
                detach,
                nodetach,
                target_vars,
            }),
        ))
    } else if starts_kw(input, "SET") {
        set_statement(input, match_clause, where_clause)
    } else if starts_kw(input, "REMOVE") {
        remove_statement(input, match_clause, where_clause)
    } else if starts_kw(input, "FILTER") {
        // §14.6: FILTER statement — `MATCH ... [WHERE ...] FILTER [WHERE] condition`
        let (i, _) = kw("FILTER")(input)?;
        // Optional WHERE keyword before the filter expression
        let i = if starts_kw(i, "WHERE") {
            kw("WHERE")(i)?.0
        } else {
            i
        };
        let (i, filter_expr) = parse_expr(i)?;
        Ok((
            i,
            Statement::Filter(crate::ast::FilterStmt {
                match_clause,
                where_clause,
                filter_expr,
            }),
        ))
    } else if starts_kw(input, "LET") {
        // §14.7: LET statement — `MATCH ... [WHERE ...] LET var = expr [, ...] RETURN ...`
        let (i, _) = kw("LET")(input)?;
        let mut bindings = Vec::new();
        let (mut i, name) = ident(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected identifier after LET".into(),
            )))
        })?;
        let (j, _) = punct(Token::Eq)(i)?;
        let (j, expr) = parse_expr(j)?;
        bindings.push((name, expr));
        i = j;
        while matches!(i.first(), Some(Token::Comma)) {
            let (j, _) = punct(Token::Comma)(i)?;
            let (j, name) = ident(j).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier in LET binding list".into(),
                )))
            })?;
            let (j, _) = punct(Token::Eq)(j)?;
            let (j, expr) = parse_expr(j)?;
            bindings.push((name, expr));
            i = j;
        }
        let (i, _) = kw("RETURN")(i)?;
        let (i, return_clause) = parse_return_clause(i)?;
        Ok((
            i,
            Statement::Let(crate::ast::LetStmt {
                match_clause,
                where_clause,
                bindings,
                return_clause,
            }),
        ))
    } else {
        fail(GleaphError::ParseError(
            "MATCH statement must end with RETURN, DELETE, SET, REMOVE, FILTER or LET".into(),
        ))
    }
}

/// §15.2: Parse `CALL (<scope_vars>) { <body> }` — inline subquery with outer scope seeding.
/// Also handles `CALL proc(args) YIELD cols` — built-in procedure call.
///
/// Disambiguation: if the token after `CALL` is an identifier followed by `(`,
/// it's a procedure call. Otherwise it's a subquery.
fn parse_call_statement_with_optional<'a>(
    input: Tokens<'a>,
    optional: bool,
) -> TResult<'a, Statement> {
    let (input, _) = kw("CALL")(input)?;
    // Check for procedure call: CALL ident(...)
    if matches!(input.first(), Some(Token::Ident(_) | Token::QuotedIdent(_)))
        && matches!(input.get(1), Some(Token::LParen))
    {
        return parse_call_procedure(input);
    }
    // Parse optional scope variable list: `(var1, var2, ...)`
    let (input, scope_vars) = if matches!(input.first(), Some(Token::LParen)) {
        let (i, _) = punct(Token::LParen)(input)?;
        let mut vars = Vec::new();
        let mut j = i;
        while !matches!(j.first(), Some(Token::RParen)) && !j.is_empty() {
            let (k, v) = ident(j).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected variable name in CALL scope".into(),
                )))
            })?;
            vars.push(v);
            j = k;
            if matches!(j.first(), Some(Token::Comma)) {
                j = &j[1..];
            }
        }
        let (j, _) = punct(Token::RParen)(j)?;
        (j, vars)
    } else {
        (input, vec![])
    };
    // Parse `{ <body> }`
    let (input, _) = punct(Token::LBrace)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected { after CALL scope".into(),
        )))
    })?;
    let (input, body) = parse_statement_nom(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "failed to parse CALL body".into(),
        )))
    })?;
    let (input, _) = punct(Token::RBrace)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected } to close CALL body".into(),
        )))
    })?;
    Ok((
        input,
        Statement::Call(crate::ast::CallStmt {
            scope_vars,
            body: Box::new(body),
            optional,
        }),
    ))
}

/// Parse `CALL proc(args...) YIELD col [, col]...` — built-in procedure invocation.
fn parse_call_procedure<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, proc_name) = ident(input)?;
    let (input, _) = punct(Token::LParen)(input)?;
    // Parse arguments as comma-separated expressions
    let mut args = Vec::new();
    let mut i = input;
    if !matches!(i.first(), Some(Token::RParen)) {
        loop {
            let (j, expr) = parse_expr(i)?;
            args.push(expr);
            i = j;
            if matches!(i.first(), Some(Token::Comma)) {
                i = &i[1..];
            } else {
                break;
            }
        }
    }
    let (input, _) = punct(Token::RParen)(i).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ')' after procedure arguments".into(),
        )))
    })?;
    // Parse optional YIELD columns
    let (i, yield_cols) = if starts_kw(input, "YIELD") {
        let (i, _) = kw("YIELD")(input)?;
        let mut cols = Vec::new();
        let mut i = i;
        loop {
            let (j, col) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected column name in YIELD".into(),
                )))
            })?;
            cols.push(col);
            i = j;
            if matches!(i.first(), Some(Token::Comma)) {
                i = &i[1..];
            } else {
                break;
            }
        }
        (i, Some(cols))
    } else {
        (input, None)
    };
    Ok((
        i,
        Statement::CallProcedure(crate::ast::CallProcedureStmt {
            procedure: proc_name,
            args,
            yield_cols,
        }),
    ))
}

/// §14.12: Parse `SELECT <items> [FROM <graph>] MATCH <pattern> [WHERE ...] [GROUP BY ...] [HAVING ...] [ORDER BY ...] [LIMIT ...]`.
///
/// Desugars to `MATCH ... [WHERE ...] RETURN <items> [GROUP BY ...] [HAVING ...] [ORDER BY ...] [LIMIT ...]`.
/// The `FROM <graph>` clause is ignored (single-graph IC model).
fn parse_select_statement<'a>(mut input: Tokens<'a>) -> TResult<'a, Statement> {
    let (i, _) = kw("SELECT")(input)?;
    input = i;
    // Parse DISTINCT modifier
    let distinct = if starts_kw(input, "DISTINCT") {
        let (i, _) = kw("DISTINCT")(input)?;
        input = i;
        true
    } else {
        false
    };
    // Parse SELECT items: `expr [AS alias] , ...` or `*`
    let (return_items, no_bindings) = if matches!(input.first(), Some(Token::Star)) {
        input = &input[1..];
        (vec![], false) // SELECT * — return all bound variables
    } else {
        let mut items = Vec::new();
        loop {
            let (i, expr) = parse_expr(input)?;
            let (i, alias) = if starts_kw(i, "AS") {
                let (j, _) = kw("AS")(i)?;
                let (j, name) = ident(j).map_err(|_| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        "expected alias after AS".into(),
                    )))
                })?;
                (j, Some(name))
            } else {
                (i, None)
            };
            items.push(crate::ast::ReturnItem { expr, alias });
            input = i;
            if matches!(input.first(), Some(Token::Comma)) {
                input = &input[1..];
            } else {
                break;
            }
        }
        (items, false)
    };
    // Optional FROM <graph_name> — ignored.
    if starts_kw(input, "FROM") {
        let (i, _) = kw("FROM")(input)?;
        // graph name can be a keyword like GRAPH or an identifier
        let i = if starts_kw(i, "GRAPH") {
            let (j, _) = kw("GRAPH")(i)?;
            j
        } else {
            i
        };
        let (i, _) = ident(i).unwrap_or((i, String::new()));
        input = i;
    }
    // Require MATCH clause
    let (i, _) = kw("MATCH")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "SELECT requires MATCH clause".into(),
        )))
    })?;
    input = i;
    // Parse match mode (§16.4)
    let match_mode = if starts_kw(input, "REPEATABLE")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("ELEMENTS")))
    {
        let (i, _) = kw("REPEATABLE")(input)?;
        let (i, _) = kw("ELEMENTS")(i)?;
        input = i;
        Some(crate::ast::MatchMode::RepeatableElements)
    } else if starts_kw(input, "DIFFERENT")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("EDGES")))
    {
        let (i, _) = kw("DIFFERENT")(input)?;
        let (i, _) = kw("EDGES")(i)?;
        input = i;
        Some(crate::ast::MatchMode::DifferentEdges)
    } else {
        None
    };
    let (
        i,
        (shortest, shortest_mode, path_variable, path_mode, match_clause, any_paths, keep_clause),
    ) = parse_match_entry_clause(input)?;
    input = i;
    let mut match_entries = vec![crate::ast::MatchEntry {
        optional: false,
        shortest,
        shortest_mode,
        path_variable,
        path_mode,
        pattern: match_clause,
        any_paths,
        keep_clause,
    }];
    while starts_kw(input, "OPTIONAL") || starts_kw(input, "MATCH") {
        let (i, optional) = if starts_kw(input, "OPTIONAL") {
            let (i, _) = kw("OPTIONAL")(input)?;
            let (i, _) = kw("MATCH")(i)?;
            (i, true)
        } else {
            let (i, _) = kw("MATCH")(input)?;
            (i, false)
        };
        let (i, (s, sm, pv, pm, mc, ap, kc)) = parse_match_entry_clause(i)?;
        match_entries.push(crate::ast::MatchEntry {
            optional,
            shortest: s,
            shortest_mode: sm,
            path_variable: pv,
            path_mode: pm,
            pattern: mc,
            any_paths: ap,
            keep_clause: kc,
        });
        input = i;
    }
    let where_clause = if starts_kw(input, "WHERE") {
        let (i, _) = kw("WHERE")(input)?;
        let (i, w) = parse_where_clause(i)?;
        input = i;
        Some(w)
    } else {
        None
    };
    let group_by = if starts_kw(input, "GROUP") {
        let (i, _) = kw("GROUP")(input)?;
        let (i, _) = kw("BY")(i)?;
        let (i, gb) = parse_expr_list(i)?;
        input = i;
        Some(gb)
    } else {
        None
    };
    let having = if starts_kw(input, "HAVING") {
        let (i, _) = kw("HAVING")(input)?;
        let (i, h) = parse_expr(i)?;
        input = i;
        Some(h)
    } else {
        None
    };
    let order_by = if starts_kw(input, "ORDER") {
        let (i, _) = kw("ORDER")(input)?;
        let (i, _) = kw("BY")(i)?;
        let (i, ob) = parse_order_by(i)?;
        input = i;
        Some(ob)
    } else {
        None
    };
    let limit = if starts_kw(input, "LIMIT") {
        let (i, _) = kw("LIMIT")(input)?;
        let (i, l) = parse_limit(i)?;
        input = i;
        Some(l)
    } else {
        None
    };
    let offset = if starts_kw(input, "OFFSET") {
        let (i, _) = kw("OFFSET")(input)?;
        let (i, o) = parse_u32_limit_value(i)?;
        input = i;
        Some(o)
    } else {
        None
    };
    let return_clause = crate::ast::ReturnClause {
        distinct,
        items: return_items,
        star: false,
        no_bindings,
        finish: false,
    };
    Ok((
        input,
        Statement::Query(crate::ast::QueryStmt {
            match_clauses: match_entries,
            where_clause,
            with_clauses: vec![],
            return_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
            match_mode,
        }),
    ))
}

/// §16.2: Parse `USE [GRAPH] <name>` — select the active graph.
fn parse_use_graph_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("USE")(input)?;
    // Optional GRAPH keyword
    let input = if starts_kw(input, "GRAPH") {
        &input[1..]
    } else {
        input
    };
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected graph name after USE".into(),
        )))
    })?;
    Ok((input, Statement::UseGraph(name)))
}

/// Parse optional `IF NOT EXISTS` clause. Returns `true` if present.
fn parse_if_not_exists<'a>(input: Tokens<'a>) -> (Tokens<'a>, bool) {
    if starts_kw(input, "IF")
        && let Ok((rest, _)) = kw("IF")(input)
        && starts_kw(rest, "NOT")
        && let Ok((rest2, _)) = kw("NOT")(rest)
        && starts_kw(rest2, "EXISTS")
        && let Ok((rest3, _)) = kw("EXISTS")(rest2)
    {
        return (rest3, true);
    }
    (input, false)
}

/// Parse optional `IF EXISTS` clause. Returns `true` if present.
fn parse_if_exists<'a>(input: Tokens<'a>) -> (Tokens<'a>, bool) {
    if starts_kw(input, "IF")
        && let Ok((rest, _)) = kw("IF")(input)
        && starts_kw(rest, "EXISTS")
        && let Ok((rest2, _)) = kw("EXISTS")(rest)
    {
        return (rest2, true);
    }
    (input, false)
}

/// §12: Parse `CREATE [PROPERTY] GRAPH [IF NOT EXISTS] <name>` — create a named graph.
fn parse_create_graph_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("CREATE")(input)?;
    let input = skip_optional_property(input);
    let (input, _) = kw("GRAPH")(input)?;
    let (input, if_not_exists) = parse_if_not_exists(input);
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected graph name after CREATE GRAPH".into(),
        )))
    })?;
    Ok((
        input,
        Statement::CreateGraph {
            name,
            if_not_exists,
        },
    ))
}

/// §12: Parse `DROP [PROPERTY] GRAPH [IF EXISTS] <name>` — drop a named graph.
fn parse_drop_graph_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DROP")(input)?;
    let input = skip_optional_property(input);
    let (input, _) = kw("GRAPH")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected GRAPH after DROP".into(),
        )))
    })?;
    let (input, if_exists) = parse_if_exists(input);
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected graph name after DROP GRAPH".into(),
        )))
    })?;
    Ok((input, Statement::DropGraph { name, if_exists }))
}

/// Lookahead: does `CREATE ...` eventually reach `GRAPH TYPE`?
/// Matches: CREATE GRAPH TYPE, CREATE PROPERTY GRAPH TYPE, CREATE OR REPLACE GRAPH TYPE,
///          CREATE OR REPLACE PROPERTY GRAPH TYPE
fn lookahead_graph_type(input: Tokens<'_>) -> bool {
    fn is_kw(t: &Token, kw: &str) -> bool {
        matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    // Skip CREATE (pos 0), then scan for GRAPH TYPE allowing OR REPLACE and PROPERTY
    let mut i = 1;
    // Skip OR REPLACE
    if input.get(i).is_some_and(|t| is_kw(t, "OR")) {
        i += 1; // REPLACE
        if input.get(i).is_some_and(|t| is_kw(t, "REPLACE")) {
            i += 1;
        } else {
            return false;
        }
    }
    // Skip optional PROPERTY
    if input.get(i).is_some_and(|t| is_kw(t, "PROPERTY")) {
        i += 1;
    }
    input.get(i).is_some_and(|t| is_kw(t, "GRAPH"))
        && input.get(i + 1).is_some_and(|t| is_kw(t, "TYPE"))
}

/// Lookahead: does `CREATE ...` reach `GRAPH` (but not `GRAPH TYPE`)?
fn lookahead_graph(input: Tokens<'_>) -> bool {
    fn is_kw(t: &Token, kw: &str) -> bool {
        matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    let mut i = 1;
    // Skip optional PROPERTY
    if input.get(i).is_some_and(|t| is_kw(t, "PROPERTY")) {
        i += 1;
    }
    input.get(i).is_some_and(|t| is_kw(t, "GRAPH"))
        && !input.get(i + 1).is_some_and(|t| is_kw(t, "TYPE"))
}

/// Lookahead: `DROP [PROPERTY] GRAPH TYPE`
fn lookahead_drop_graph_type(input: Tokens<'_>) -> bool {
    fn is_kw(t: &Token, kw: &str) -> bool {
        matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    let mut i = 1; // skip DROP
    if input.get(i).is_some_and(|t| is_kw(t, "PROPERTY")) {
        i += 1;
    }
    input.get(i).is_some_and(|t| is_kw(t, "GRAPH"))
        && input.get(i + 1).is_some_and(|t| is_kw(t, "TYPE"))
}

/// Lookahead: `DROP [PROPERTY] GRAPH` (but not `DROP [PROPERTY] GRAPH TYPE`)
fn lookahead_drop_graph(input: Tokens<'_>) -> bool {
    fn is_kw(t: &Token, kw: &str) -> bool {
        matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    let mut i = 1; // skip DROP
    if input.get(i).is_some_and(|t| is_kw(t, "PROPERTY")) {
        i += 1;
    }
    input.get(i).is_some_and(|t| is_kw(t, "GRAPH"))
        && !input.get(i + 1).is_some_and(|t| is_kw(t, "TYPE"))
}

/// Consume optional `PROPERTY` keyword (returns input unchanged if not present).
fn skip_optional_property<'a>(input: Tokens<'a>) -> Tokens<'a> {
    if starts_kw(input, "PROPERTY") {
        kw("PROPERTY")(input).map(|(i, _)| i).unwrap_or(input)
    } else {
        input
    }
}

/// Parse optional `OR REPLACE` clause. Returns `true` if present.
fn parse_or_replace<'a>(input: Tokens<'a>) -> (Tokens<'a>, bool) {
    if starts_kw(input, "OR")
        && let Ok((rest, _)) = kw("OR")(input)
        && starts_kw(rest, "REPLACE")
        && let Ok((rest2, _)) = kw("REPLACE")(rest)
    {
        return (rest2, true);
    }
    (input, false)
}

/// §12: Parse `CREATE [OR REPLACE] [PROPERTY] GRAPH TYPE [IF NOT EXISTS] <name> { ... | LIKE name | COPY OF name }`.
fn parse_create_graph_type_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("CREATE")(input)?;
    let (input, or_replace) = parse_or_replace(input);
    let input = skip_optional_property(input);
    let (input, _) = kw("GRAPH")(input)?;
    let (input, _) = kw("TYPE")(input)?;
    let (input, if_not_exists) = parse_if_not_exists(input);
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected graph type name after CREATE GRAPH TYPE".into(),
        )))
    })?;
    // Check for LIKE name or COPY OF name
    let (input, source) = if starts_kw(input, "LIKE") {
        let (i, _) = kw("LIKE")(input)?;
        let (i, src) = ident(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected source graph type name after LIKE".into(),
            )))
        })?;
        (i, Some(src))
    } else if starts_kw(input, "COPY") {
        let (i, _) = kw("COPY")(input)?;
        let (i, _) = kw("OF")(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected OF after COPY".into(),
            )))
        })?;
        let (i, src) = ident(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected source graph type name after COPY OF".into(),
            )))
        })?;
        (i, Some(src))
    } else {
        (input, None)
    };
    // LIKE/COPY OF uses an empty definition (resolved at execution time from source)
    let (input, definition) = if source.is_some() {
        (
            input,
            GraphTypeDefinition {
                node_labels: vec![],
                edge_labels: vec![],
                node_types: vec![],
                edge_types: vec![],
            },
        )
    } else {
        parse_graph_type_body(input)?
    };
    Ok((
        input,
        Statement::CreateGraphType {
            name,
            definition,
            if_not_exists,
            or_replace,
            source,
        },
    ))
}

/// Parses the `{ ... }` body of a graph type definition.
///
/// Entries are comma-separated and may be:
/// - Node entry: `(:Label)` or `(Label)`
/// - Edge entry: `-[:Label]->` or `-[:Label]-`
/// - Inline edge type (§18.3): `(:From)-[:Label]->(:To)` or `(:From)-[:Label { props }]->(:To)`
/// - Inline node type (§18.2): `(TypeName :Label { props })`
fn parse_graph_type_body<'a>(input: Tokens<'a>) -> TResult<'a, GraphTypeDefinition> {
    let (input, _) = punct(Token::LBrace)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '{' after graph type name".into(),
        )))
    })?;

    let mut node_labels = Vec::new();
    let mut edge_labels = Vec::new();
    let mut node_types = Vec::new();
    let mut edge_types = Vec::new();
    let mut input = input;

    loop {
        if matches!(input.first(), Some(Token::RBrace)) {
            break;
        }
        if matches!(input.first(), Some(Token::Minus) | Some(Token::Tilde)) {
            // Edge entry: -[:Label]-> (Tilde is caught and rejected inside parse_graph_type_edge_entry)
            let (rest, label) = parse_graph_type_edge_entry(input)?;
            edge_labels.push(label);
            input = rest;
        } else if matches!(input.first(), Some(Token::LParen)) {
            // §18.3 inline edge type: (:From)-[:Label]->(:To)
            // §18.2 inline node type: (TypeName :Label { props })
            // or bare node entry: (:Label)
            if lookahead_inline_edge_type(input) {
                let (rest, et) = parse_graph_type_inline_edge(input)?;
                edge_types.push(et);
                input = rest;
            } else {
                let (rest, entry) = parse_graph_type_node_or_type_entry(input)?;
                match entry {
                    NodeEntry::Label(label) => node_labels.push(label),
                    NodeEntry::TypeDef(nt) => node_types.push(nt),
                }
                input = rest;
            }
        } else {
            return fail(GleaphError::ParseError(
                "expected node entry '(:Label)', node type '(Name :Label { ... })', edge type '(:From)-[:Label]->(:To)', or edge entry '-[:Label]->' in graph type body".into(),
            ));
        }
        // Optional comma separator
        if matches!(input.first(), Some(Token::Comma)) {
            input = &input[1..];
        }
    }

    let (input, _) = punct(Token::RBrace)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '}' to close graph type body".into(),
        )))
    })?;

    if node_labels.is_empty()
        && edge_labels.is_empty()
        && node_types.is_empty()
        && edge_types.is_empty()
    {
        return fail(GleaphError::ParseError(
            "graph type body must contain at least one node or edge label definition".into(),
        ));
    }

    // Sort and deduplicate
    node_labels.sort();
    node_labels.dedup();
    edge_labels.sort();
    edge_labels.dedup();

    Ok((
        input,
        GraphTypeDefinition {
            node_labels,
            edge_labels,
            node_types,
            edge_types,
        },
    ))
}

/// Internal result type for node entries — either a bare label or a typed node definition.
enum NodeEntry {
    Label(String),
    TypeDef(NodeTypeDef),
}

/// Parses a label list: `:Label1 | :Label2 | ...` (colon optional before each label).
/// Returns the parsed labels (sorted, deduped) and remaining input.
fn parse_label_list<'a>(input: Tokens<'a>) -> TResult<'a, Vec<String>> {
    let mut labels = Vec::new();
    let input = if matches!(input.first(), Some(Token::Colon)) {
        &input[1..]
    } else {
        input
    };
    let (mut input, first) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected label name".into(),
        )))
    })?;
    labels.push(first);
    while matches!(input.first(), Some(Token::Pipe)) {
        input = &input[1..]; // consume `|`
        if matches!(input.first(), Some(Token::Colon)) {
            input = &input[1..];
        }
        let (i, label) = ident(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected label name after '|'".into(),
            )))
        })?;
        labels.push(label);
        input = i;
    }
    labels.sort();
    labels.dedup();
    Ok((input, labels))
}

/// §18.2: Parses a node entry that may be a bare label or a typed node definition.
///
/// - Bare label: `(:Label)` or `(Label)` → `NodeEntry::Label`
/// - Typed node: `(TypeName :Label { props })` → `NodeEntry::TypeDef`
///
/// Disambiguation: `(Ident :` means typed node (type name then label).
/// `(:Ident)` or `(Ident)` means bare label.
fn parse_graph_type_node_or_type_entry<'a>(input: Tokens<'a>) -> TResult<'a, NodeEntry> {
    let (input, _) = punct(Token::LParen)(input)?;

    // Peek: if first token is ident followed by `:`, it's a typed node `(TypeName :Label ...)`
    if matches!(input.first(), Some(Token::Ident(_))) && matches!(input.get(1), Some(Token::Colon))
    {
        let (input, type_name) = ident(input).unwrap();
        let input = &input[1..]; // consume `:`
        let (input, labels) = parse_label_list(input)?;
        // Optional property definitions
        let (input, properties) = if matches!(input.first(), Some(Token::LBrace)) {
            parse_property_def_list(input)?
        } else {
            (input, Vec::new())
        };
        let (input, _) = punct(Token::RParen)(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected ')' to close node type definition".into(),
            )))
        })?;
        return Ok((
            input,
            NodeEntry::TypeDef(NodeTypeDef {
                name: type_name,
                labels,
                properties,
            }),
        ));
    }

    // Bare label: optional colon, then ident, then `)`
    let input = if matches!(input.first(), Some(Token::Colon)) {
        &input[1..]
    } else {
        input
    };
    let (input, label) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected label name in node entry".into(),
        )))
    })?;
    let (input, _) = punct(Token::RParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ')' after node label".into(),
        )))
    })?;
    Ok((input, NodeEntry::Label(label)))
}

/// Parses a property definition list: `{ name :: TYPE [NOT NULL] (, name :: TYPE [NOT NULL])* }`.
fn parse_property_def_list<'a>(input: Tokens<'a>) -> TResult<'a, Vec<PropertyDef>> {
    let (mut input, _) = punct(Token::LBrace)(input)?;
    let mut props = Vec::new();
    // Parse first property
    if !matches!(input.first(), Some(Token::RBrace)) {
        let (i, prop) = parse_property_def(input)?;
        props.push(prop);
        input = i;
        // Additional `, property` entries
        while matches!(input.first(), Some(Token::Comma)) {
            input = &input[1..]; // consume `,`
            let (i, prop) = parse_property_def(input)?;
            props.push(prop);
            input = i;
        }
    }
    let (input, _) = punct(Token::RBrace)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '}' to close property definitions".into(),
        )))
    })?;
    Ok((input, props))
}

/// Parses a single property definition: `name :: TYPE [NOT NULL]`.
fn parse_property_def<'a>(input: Tokens<'a>) -> TResult<'a, PropertyDef> {
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected property name in property definition".into(),
        )))
    })?;
    // Expect `::`
    let input = if matches!(input.first(), Some(Token::Colon))
        && matches!(input.get(1), Some(Token::Colon))
    {
        &input[2..]
    } else {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            format!("expected '::' after property name '{name}'"),
        ))));
    };
    // Parse type name
    let (input, value_type) = parse_value_type_name(input)?;
    // Optional NOT NULL
    let (input, required) = if starts_kw(input, "NOT") && starts_kw(&input[1..], "NULL") {
        (&input[2..], true)
    } else {
        (input, false)
    };
    Ok((
        input,
        PropertyDef {
            name,
            value_type,
            required,
        },
    ))
}

/// Parse a positive integer literal for character-string length constraints.
fn parse_char_length_int<'a>(input: Tokens<'a>) -> TResult<'a, u32> {
    match input.first() {
        Some(Token::Int(n)) => {
            if *n <= 0 {
                return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "character length must be at least 1".into(),
                ))));
            }
            let val = u32::try_from(*n).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "character length too large: {n}"
                ))))
            })?;
            Ok((&input[1..], val))
        }
        _ => Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected positive integer for character length".into(),
        )))),
    }
}

/// Parses a value type name keyword (INT, FLOAT, TEXT, etc.) → `ValueType`.
///
/// Also handles `LIST<SCALAR>` syntax for typed lists, e.g. `LIST<INT>`.
fn parse_value_type_name<'a>(input: Tokens<'a>) -> TResult<'a, ValueType> {
    // Handle LIST with optional angle-bracket element type.
    if starts_kw(input, "LIST") {
        let rest = &input[1..];
        // Check for LIST<SCALAR> syntax.
        if matches!(rest.first(), Some(Token::Lt)) {
            let after_lt = &rest[1..];
            let (after_type, scalar_name) = ident(after_lt).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected scalar type name after LIST<".into(),
                )))
            })?;
            let scalar = crate::ast::parse_scalar_type(&scalar_name).ok_or_else(|| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "unknown scalar type '{scalar_name}' in LIST<...>"
                ))))
            })?;
            let (after_gt, _) = punct(Token::Gt)(after_type)?;
            return Ok((after_gt, ValueType::TypedList(scalar)));
        }
        return Ok((rest, ValueType::List));
    }

    // ── Multi-token prefixes: SIGNED / UNSIGNED ──
    if starts_kw(input, "SIGNED") {
        let rest = &input[1..];
        // SIGNED SMALL INTEGER → Int16, SIGNED BIG INTEGER → Int64
        if starts_kw(rest, "SMALL") && starts_kw(&rest[1..], "INTEGER") {
            return Ok((&rest[2..], ValueType::Int16));
        }
        if starts_kw(rest, "BIG") && starts_kw(&rest[1..], "INTEGER") {
            return Ok((&rest[2..], ValueType::Int64));
        }
        // SIGNED INTEGER8..256 / SIGNED INT / SIGNED INTEGER
        let signed_types = [
            ("INTEGER8", ValueType::Int8),
            ("INTEGER16", ValueType::Int16),
            ("INTEGER32", ValueType::Int32),
            ("INTEGER64", ValueType::Int64),
            ("INTEGER128", ValueType::Int128),
            ("INTEGER256", ValueType::Int256),
            ("INTEGER", ValueType::Int32),
            ("INT", ValueType::Int32),
        ];
        for (kw, vt) in &signed_types {
            if starts_kw(rest, kw) {
                return Ok((&rest[1..], *vt));
            }
        }
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected integer type after SIGNED".into(),
        ))));
    }
    if starts_kw(input, "UNSIGNED") {
        let rest = &input[1..];
        // UNSIGNED SMALL INTEGER → Uint16, UNSIGNED BIG INTEGER → Uint64
        if starts_kw(rest, "SMALL") && starts_kw(&rest[1..], "INTEGER") {
            return Ok((&rest[2..], ValueType::Uint16));
        }
        if starts_kw(rest, "BIG") && starts_kw(&rest[1..], "INTEGER") {
            return Ok((&rest[2..], ValueType::Uint64));
        }
        // UNSIGNED INTEGER8..256 / UNSIGNED INT / UNSIGNED INTEGER
        let unsigned_types = [
            ("INTEGER8", ValueType::Uint8),
            ("INTEGER16", ValueType::Uint16),
            ("INTEGER32", ValueType::Uint32),
            ("INTEGER64", ValueType::Uint64),
            ("INTEGER128", ValueType::Uint128),
            ("INTEGER256", ValueType::Uint256),
            ("INTEGER", ValueType::Uint32),
            ("INT", ValueType::Uint32),
        ];
        for (kw, vt) in &unsigned_types {
            if starts_kw(rest, kw) {
                return Ok((&rest[1..], *vt));
            }
        }
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected integer type after UNSIGNED".into(),
        ))));
    }

    // ── Multi-token: SMALL INTEGER → Int16, BIG INTEGER → Int64 ──
    if starts_kw(input, "SMALL") && starts_kw(&input[1..], "INTEGER") {
        return Ok((&input[2..], ValueType::Int16));
    }
    if starts_kw(input, "BIG") && starts_kw(&input[1..], "INTEGER") {
        return Ok((&input[2..], ValueType::Int64));
    }

    // ── INTEGER(p) / INT(p) / UINT(p) — precision parameter ──
    if starts_kw(input, "INTEGER") || starts_kw(input, "INT") || starts_kw(input, "UINT") {
        let is_unsigned = starts_kw(input, "UINT");
        let rest = &input[1..];
        if matches!(rest.first(), Some(Token::LParen)) {
            let after_lp = &rest[1..];
            if let Some(Token::Int(p)) = after_lp.first() {
                let p = *p;
                if matches!(after_lp.get(1), Some(Token::RParen)) {
                    let after_rp = &after_lp[2..];
                    let vt = if p <= 0 || p > 256 {
                        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                            format!("integer precision must be 1..256, got {p}"),
                        ))));
                    } else if p <= 8 {
                        if is_unsigned {
                            ValueType::Uint8
                        } else {
                            ValueType::Int8
                        }
                    } else if p <= 16 {
                        if is_unsigned {
                            ValueType::Uint16
                        } else {
                            ValueType::Int16
                        }
                    } else if p <= 32 {
                        if is_unsigned {
                            ValueType::Uint32
                        } else {
                            ValueType::Int32
                        }
                    } else if p <= 64 {
                        if is_unsigned {
                            ValueType::Uint64
                        } else {
                            ValueType::Int64
                        }
                    } else if p <= 128 {
                        if is_unsigned {
                            ValueType::Uint128
                        } else {
                            ValueType::Int128
                        }
                    } else {
                        if is_unsigned {
                            ValueType::Uint256
                        } else {
                            ValueType::Int256
                        }
                    };
                    return Ok((after_rp, vt));
                }
            }
        }
    }

    // ── FLOAT with precision, DOUBLE PRECISION, FLOAT16/128/256, DECIMAL(p) ──
    if starts_kw(input, "FLOAT") {
        let rest = &input[1..];
        // FLOAT(p) or FLOAT(p, s)
        if matches!(rest.first(), Some(Token::LParen)) {
            let after_lp = &rest[1..];
            if let Some(Token::Int(p)) = after_lp.first() {
                let p = *p;
                // Check for FLOAT(p, s) — unsupported
                if matches!(after_lp.get(1), Some(Token::Comma)) {
                    return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        "FLOAT(p, s) is not supported; use FLOAT(p) or DECIMAL".into(),
                    ))));
                }
                if matches!(after_lp.get(1), Some(Token::RParen)) {
                    let after_rp = &after_lp[2..];
                    if p <= 0 {
                        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                            format!("FLOAT precision must be >= 1, got {p}"),
                        ))));
                    } else if p <= 7 {
                        return Ok((after_rp, ValueType::Float32));
                    } else if p <= 15 {
                        return Ok((after_rp, ValueType::Float64));
                    } else {
                        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                            format!(
                                "FLOAT({p}) exceeds maximum supported precision 15; FLOAT128/FLOAT256 are not yet supported"
                            ),
                        ))));
                    }
                }
            }
        }
        // Bare FLOAT → Float32
        return Ok((rest, ValueType::Float32));
    }

    // FLOAT16 / FLOAT128 / FLOAT256 — not yet supported
    if starts_kw(input, "FLOAT16") {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "FLOAT16 is not yet supported".into(),
        ))));
    }
    if starts_kw(input, "FLOAT128") {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "FLOAT128 is not yet supported".into(),
        ))));
    }
    if starts_kw(input, "FLOAT256") {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "FLOAT256 is not yet supported".into(),
        ))));
    }

    // DOUBLE [PRECISION] → Float64
    if starts_kw(input, "DOUBLE") {
        let rest = &input[1..];
        if starts_kw(rest, "PRECISION") {
            return Ok((&rest[1..], ValueType::Float64));
        }
        return Ok((rest, ValueType::Float64));
    }

    // REAL → Float32
    if starts_kw(input, "REAL") {
        return Ok((&input[1..], ValueType::Float32));
    }

    // DECIMAL(p) / DEC(p) / NUMERIC(p) — precision parameters not supported
    if starts_kw(input, "DECIMAL") || starts_kw(input, "DEC") || starts_kw(input, "NUMERIC") {
        let rest = &input[1..];
        if matches!(rest.first(), Some(Token::LParen)) {
            return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "DECIMAL precision parameters are not supported".into(),
            ))));
        }
    }

    // ── STRING / VARCHAR / CHAR with optional length constraint ──
    // Must be checked before the single-token table to handle `STRING(n)` etc.
    if starts_kw(input, "STRING") || starts_kw(input, "VARCHAR") || starts_kw(input, "CHAR") {
        let is_char = starts_kw(input, "CHAR");
        let rest = &input[1..];
        if matches!(rest.first(), Some(Token::LParen)) {
            let after_lp = &rest[1..];
            // Parse first integer literal
            let (after_first, first_val) = parse_char_length_int(after_lp)?;
            if matches!(after_first.first(), Some(Token::Comma)) && !is_char {
                // STRING(min, max)
                let (after_second, second_val) = parse_char_length_int(&after_first[1..])?;
                let (after_rp, _) = punct(Token::RParen)(after_second)?;
                if first_val > second_val {
                    return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        format!(
                            "STRING min length ({first_val}) exceeds max length ({second_val})"
                        ),
                    ))));
                }
                return Ok((
                    after_rp,
                    ValueType::TextConstrained {
                        min_length: first_val,
                        max_length: second_val,
                        fixed: false,
                    },
                ));
            }
            let (after_rp, _) = punct(Token::RParen)(after_first)?;
            if is_char {
                return Ok((
                    after_rp,
                    ValueType::TextConstrained {
                        min_length: first_val,
                        max_length: first_val,
                        fixed: true,
                    },
                ));
            }
            // STRING(max) or VARCHAR(max)
            return Ok((
                after_rp,
                ValueType::TextConstrained {
                    min_length: 0,
                    max_length: first_val,
                    fixed: false,
                },
            ));
        }
        // Bare STRING / VARCHAR / CHAR without parentheses → unconstrained Text
        return Ok((rest, ValueType::Text));
    }

    // ── BYTES / BINARY / VARBINARY with optional length constraint ──
    if starts_kw(input, "BYTES") || starts_kw(input, "VARBINARY") || starts_kw(input, "BINARY") {
        let is_binary = starts_kw(input, "BINARY");
        let rest = &input[1..];
        if matches!(rest.first(), Some(Token::LParen)) {
            let after_lp = &rest[1..];
            let (after_first, first_val) = parse_char_length_int(after_lp)?;
            if matches!(after_first.first(), Some(Token::Comma)) && !is_binary {
                // BYTES(min, max)
                let (after_second, second_val) = parse_char_length_int(&after_first[1..])?;
                let (after_rp, _) = punct(Token::RParen)(after_second)?;
                if first_val > second_val {
                    return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        format!("BYTES min length ({first_val}) exceeds max length ({second_val})"),
                    ))));
                }
                return Ok((
                    after_rp,
                    ValueType::BytesConstrained {
                        min_length: first_val,
                        max_length: second_val,
                        fixed: false,
                    },
                ));
            }
            let (after_rp, _) = punct(Token::RParen)(after_first)?;
            if is_binary {
                return Ok((
                    after_rp,
                    ValueType::BytesConstrained {
                        min_length: first_val,
                        max_length: first_val,
                        fixed: true,
                    },
                ));
            }
            // BYTES(max) or VARBINARY(max)
            return Ok((
                after_rp,
                ValueType::BytesConstrained {
                    min_length: 0,
                    max_length: first_val,
                    fixed: false,
                },
            ));
        }
        // Bare BYTES / BINARY / VARBINARY without parentheses → unconstrained Bytes
        return Ok((rest, ValueType::Bytes));
    }

    // ── Single-token type names ──
    let type_names = [
        ("INT8", ValueType::Int8),
        ("INT16", ValueType::Int16),
        ("INT32", ValueType::Int32),
        ("INT64", ValueType::Int64),
        ("INT128", ValueType::Int128),
        ("INT256", ValueType::Int256),
        ("INT", ValueType::Int32),
        ("INTEGER8", ValueType::Int8),
        ("INTEGER16", ValueType::Int16),
        ("INTEGER32", ValueType::Int32),
        ("INTEGER64", ValueType::Int64),
        ("INTEGER128", ValueType::Int128),
        ("INTEGER256", ValueType::Int256),
        ("INTEGER", ValueType::Int32),
        ("TINYINT", ValueType::Int8),
        ("SMALLINT", ValueType::Int16),
        ("BIGINT", ValueType::Int64),
        ("FLOAT32", ValueType::Float32),
        ("FLOAT64", ValueType::Float64),
        ("TEXT", ValueType::Text),
        ("BOOL", ValueType::Bool),
        ("BOOLEAN", ValueType::Bool),
        ("TIMESTAMP", ValueType::Timestamp),
        ("DATE", ValueType::Date),
        ("TIME", ValueType::Time),
        ("DATETIME", ValueType::DateTime),
        ("DURATION", ValueType::Duration),
        ("DECIMAL", ValueType::Decimal),
        ("DEC", ValueType::Decimal),
        ("NUMERIC", ValueType::Decimal),
        ("UINT8", ValueType::Uint8),
        ("UINT16", ValueType::Uint16),
        ("UINT32", ValueType::Uint32),
        ("UINT64", ValueType::Uint64),
        ("UINT128", ValueType::Uint128),
        ("UINT256", ValueType::Uint256),
        ("UINT", ValueType::Uint32),
        ("USMALLINT", ValueType::Uint16),
        ("UBIGINT", ValueType::Uint64),
    ];
    for (kw_str, vt) in &type_names {
        if starts_kw(input, kw_str) {
            return Ok((&input[1..], *vt));
        }
    }
    Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
        "expected type name (INT, INT8..INT256, UINT, UINT8..UINT256, FLOAT, FLOAT32, FLOAT64, REAL, DOUBLE, TEXT, BOOL, TIMESTAMP, LIST, LIST<T>, BYTES, DATE, TIME, DATETIME, DURATION, DECIMAL)".into(),
    ))))
}

/// Parse a parameter type from a type identifier, handling `LIST<SCALAR>` syntax.
///
/// After the caller consumed an `ident` token (e.g. "LIST"), this function
/// checks for a trailing `<SCALAR>` in the token stream. If the ident is "LIST"
/// and `<` follows, it parses the element type and closing `>`.
fn parse_param_type_from_ident<'a>(input: Tokens<'a>, type_ident: &str) -> TResult<'a, ValueType> {
    // LIST with optional <SCALAR>
    if type_ident.eq_ignore_ascii_case("LIST") {
        if matches!(input.first(), Some(Token::Lt)) {
            let after_lt = &input[1..];
            let (after_type, scalar_name) = ident(after_lt).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected scalar type name after LIST<".into(),
                )))
            })?;
            let scalar = crate::ast::parse_scalar_type(&scalar_name).ok_or_else(|| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "unknown scalar type '{scalar_name}' in LIST<...>"
                ))))
            })?;
            let (after_gt, _) = punct(Token::Gt)(after_type)?;
            return Ok((after_gt, ValueType::TypedList(scalar)));
        }
        return Ok((input, ValueType::List));
    }
    // STRING / VARCHAR / CHAR with optional (length)
    if type_ident.eq_ignore_ascii_case("STRING")
        || type_ident.eq_ignore_ascii_case("VARCHAR")
        || type_ident.eq_ignore_ascii_case("CHAR")
    {
        let is_char = type_ident.eq_ignore_ascii_case("CHAR");
        if matches!(input.first(), Some(Token::LParen)) {
            let after_lp = &input[1..];
            let (after_first, first_val) = parse_char_length_int(after_lp)?;
            if matches!(after_first.first(), Some(Token::Comma)) && !is_char {
                let (after_second, second_val) = parse_char_length_int(&after_first[1..])?;
                let (after_rp, _) = punct(Token::RParen)(after_second)?;
                if first_val > second_val {
                    return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        format!(
                            "STRING min length ({first_val}) exceeds max length ({second_val})"
                        ),
                    ))));
                }
                return Ok((
                    after_rp,
                    ValueType::TextConstrained {
                        min_length: first_val,
                        max_length: second_val,
                        fixed: false,
                    },
                ));
            }
            let (after_rp, _) = punct(Token::RParen)(after_first)?;
            if is_char {
                return Ok((
                    after_rp,
                    ValueType::TextConstrained {
                        min_length: first_val,
                        max_length: first_val,
                        fixed: true,
                    },
                ));
            }
            return Ok((
                after_rp,
                ValueType::TextConstrained {
                    min_length: 0,
                    max_length: first_val,
                    fixed: false,
                },
            ));
        }
        return Ok((input, ValueType::Text));
    }
    // BYTES / BINARY / VARBINARY with optional (length)
    if type_ident.eq_ignore_ascii_case("BYTES")
        || type_ident.eq_ignore_ascii_case("VARBINARY")
        || type_ident.eq_ignore_ascii_case("BINARY")
    {
        let is_binary = type_ident.eq_ignore_ascii_case("BINARY");
        if matches!(input.first(), Some(Token::LParen)) {
            let after_lp = &input[1..];
            let (after_first, first_val) = parse_char_length_int(after_lp)?;
            if matches!(after_first.first(), Some(Token::Comma)) && !is_binary {
                let (after_second, second_val) = parse_char_length_int(&after_first[1..])?;
                let (after_rp, _) = punct(Token::RParen)(after_second)?;
                if first_val > second_val {
                    return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        format!("BYTES min length ({first_val}) exceeds max length ({second_val})"),
                    ))));
                }
                return Ok((
                    after_rp,
                    ValueType::BytesConstrained {
                        min_length: first_val,
                        max_length: second_val,
                        fixed: false,
                    },
                ));
            }
            let (after_rp, _) = punct(Token::RParen)(after_first)?;
            if is_binary {
                return Ok((
                    after_rp,
                    ValueType::BytesConstrained {
                        min_length: first_val,
                        max_length: first_val,
                        fixed: true,
                    },
                ));
            }
            return Ok((
                after_rp,
                ValueType::BytesConstrained {
                    min_length: 0,
                    max_length: first_val,
                    fixed: false,
                },
            ));
        }
        return Ok((input, ValueType::Bytes));
    }
    let vt = parse_value_type(type_ident).ok_or_else(|| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
            "unknown parameter type '{type_ident}'"
        ))))
    })?;
    Ok((input, vt))
}

/// Lookahead: does `(:Label)` at the current position continue with `-[`,
/// indicating an inline edge type `(:From)-[:Label]->(:To)` rather than a
/// standalone node entry?
fn lookahead_inline_edge_type(input: Tokens<'_>) -> bool {
    // Expect `(`
    if !matches!(input.first(), Some(Token::LParen)) {
        return false;
    }
    // Scan forward past `(`, label list (`:Label | :Label ...`), `)` — then check for `-[`
    let mut pos = 1;
    // Optional colon
    if matches!(input.get(pos), Some(Token::Colon)) {
        pos += 1;
    }
    // First label ident
    if !matches!(input.get(pos), Some(Token::Ident(_))) {
        return false;
    }
    pos += 1;
    // Additional `| :Label` entries
    while matches!(input.get(pos), Some(Token::Pipe)) {
        pos += 1; // skip `|`
        if matches!(input.get(pos), Some(Token::Colon)) {
            pos += 1; // skip optional `:`
        }
        if !matches!(input.get(pos), Some(Token::Ident(_))) {
            return false;
        }
        pos += 1;
    }
    // `)`
    if !matches!(input.get(pos), Some(Token::RParen)) {
        return false;
    }
    pos += 1;
    // `-[`
    matches!(input.get(pos), Some(Token::Minus))
        && matches!(input.get(pos + 1), Some(Token::LBracket))
}

/// §18.3: Parses an inline edge type entry: `(:From)-[:Label { props }]->(:To)`.
///
/// Auto-generates a type name from the edge label (e.g. `Placed` → `_Placed`).
fn parse_graph_type_inline_edge<'a>(input: Tokens<'a>) -> TResult<'a, EdgeTypeDef> {
    // Parse `(:FromLabel)`
    let (input, _) = punct(Token::LParen)(input)?;
    let (input, from_labels) = parse_label_list(input)?;
    let (input, _) = punct(Token::RParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ')' after source label in inline edge type".into(),
        )))
    })?;
    // Parse `-[`
    let (input, _) = punct(Token::Minus)(input)?;
    let (input, _) = punct(Token::LBracket)(input)?;
    // Optional `:` before edge label
    let input = if matches!(input.first(), Some(Token::Colon)) {
        &input[1..]
    } else {
        input
    };
    let (input, edge_label) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected edge label in inline edge type".into(),
        )))
    })?;
    // Optional property definitions
    let (input, properties) = if matches!(input.first(), Some(Token::LBrace)) {
        parse_property_def_list(input)?
    } else {
        (input, Vec::new())
    };
    // `]->`
    let (input, _) = punct(Token::RBracket)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ']' after edge label in inline edge type".into(),
        )))
    })?;
    let input = if matches!(input.first(), Some(Token::ArrowRight)) {
        &input[1..]
    } else {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '->' after ']' in inline edge type".into(),
        ))));
    };
    // Parse `(:ToLabel)`
    let (input, _) = punct(Token::LParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '(' for destination labels in inline edge type".into(),
        )))
    })?;
    let (input, to_labels) = parse_label_list(input)?;
    let (input, _) = punct(Token::RParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ')' to close destination labels in inline edge type".into(),
        )))
    })?;
    // Auto-generate name from edge label
    let name = format!("_{edge_label}");
    Ok((
        input,
        EdgeTypeDef {
            name,
            label: edge_label,
            from_labels,
            to_labels,
            properties,
        },
    ))
}

/// Parses an edge entry in a graph type body: `-[:Label]->`.
fn parse_graph_type_edge_entry<'a>(input: Tokens<'a>) -> TResult<'a, String> {
    if matches!(input.first(), Some(Token::Tilde)) {
        return fail(GleaphError::ParseError(
            "undirected edge syntax ~[:Label]~ is not supported in graph type definitions. Gleaph is a directed-only graph database".into(),
        ));
    }
    let (input, _) = punct(Token::Minus)(input)?;
    let (input, _) = punct(Token::LBracket)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '[' after edge entry start".into(),
        )))
    })?;
    // Optional colon before label
    let input = if matches!(input.first(), Some(Token::Colon)) {
        &input[1..]
    } else {
        input
    };
    let (input, label) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected label name in edge entry".into(),
        )))
    })?;
    let (input, _) = punct(Token::RBracket)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ']' after edge label".into(),
        )))
    })?;
    let input = if matches!(input.first(), Some(Token::ArrowRight)) {
        &input[1..]
    } else {
        return fail(GleaphError::ParseError(
            "expected '->' after directed edge entry -[:Label]->".into(),
        ));
    };
    Ok((input, label))
}

/// §12: Parse `DROP [PROPERTY] GRAPH TYPE [IF EXISTS] <name>` — remove a graph type schema.
fn parse_drop_graph_type_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DROP")(input)?;
    let input = skip_optional_property(input);
    let (input, _) = kw("GRAPH")(input)?;
    let (input, _) = kw("TYPE")(input)?;
    let (input, if_exists) = parse_if_exists(input);
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected graph type name after DROP GRAPH TYPE".into(),
        )))
    })?;
    Ok((input, Statement::DropGraphType { name, if_exists }))
}

/// §12: Parse `DESCRIBE GRAPH TYPE <name>` — introspect a graph type schema.
fn parse_describe_graph_type<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DESCRIBE")(input)?;
    let (input, _) = kw("GRAPH")(input)?;
    let (input, _) = kw("TYPE")(input)?;
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected graph type name after DESCRIBE GRAPH TYPE".into(),
        )))
    })?;
    Ok((input, Statement::DescribeGraphType(name)))
}

/// §12: Parse `CREATE SCHEMA [IF NOT EXISTS] <name>` — create a schema namespace.
fn parse_create_schema_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("CREATE")(input)?;
    let (input, _) = kw("SCHEMA")(input)?;
    let (input, if_not_exists) = parse_if_not_exists(input);
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected schema name after CREATE SCHEMA".into(),
        )))
    })?;
    Ok((
        input,
        Statement::CreateSchema {
            name,
            if_not_exists,
        },
    ))
}

/// §12: Parse `DROP SCHEMA [IF EXISTS] <name>` — remove a schema namespace.
fn parse_drop_schema_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DROP")(input)?;
    let (input, _) = kw("SCHEMA")(input)?;
    let (input, if_exists) = parse_if_exists(input);
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected schema name after DROP SCHEMA".into(),
        )))
    })?;
    Ok((input, Statement::DropSchema { name, if_exists }))
}

/// Parse `SHOW STATS | PLANNER STATS | INDEXES | GRANTS | METRICS | SCHEMAS | GRAPH TYPES
/// | QUOTA | ALIASES | PREPARED`.
fn parse_show_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    use crate::ast::ShowTarget;
    let (input, _) = kw("SHOW")(input)?;
    // Two-word targets first
    if starts_kw(input, "PLANNER") {
        let (input, _) = kw("PLANNER")(input)?;
        let (input, _) = kw("STATS")(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected STATS after SHOW PLANNER".into(),
            )))
        })?;
        return Ok((input, Statement::Show(ShowTarget::PlannerStats)));
    }
    if starts_kw(input, "GRAPH") {
        let (input, _) = kw("GRAPH")(input)?;
        let (input, _) = kw("TYPES")(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected TYPES after SHOW GRAPH".into(),
            )))
        })?;
        return Ok((input, Statement::Show(ShowTarget::GraphTypes)));
    }
    // Single-word targets
    let (input, target_ident) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected target after SHOW (STATS, INDEXES, GRANTS, METRICS, SCHEMAS, GRAPH TYPES, QUOTA, ALIASES, PREPARED)".into(),
        )))
    })?;
    let target = match target_ident.to_ascii_uppercase().as_str() {
        "STATS" => ShowTarget::Stats,
        "INDEXES" | "INDICES" => ShowTarget::Indexes,
        "GRANTS" => ShowTarget::Grants,
        "METRICS" => ShowTarget::Metrics,
        "SCHEMAS" => ShowTarget::Schemas,
        "QUOTA" => ShowTarget::Quota,
        "ALIASES" => ShowTarget::Aliases,
        "PREPARED" => ShowTarget::Prepared,
        "SETTINGS" => ShowTarget::Settings,
        "CONSTRAINTS" => ShowTarget::Constraints,
        other => {
            return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                format!("unknown SHOW target: {other}"),
            ))));
        }
    };
    Ok((input, Statement::Show(target)))
}

/// Parse `GRANT READ|WRITE|ADMIN ON GRAPH TO '<principal>'`.
fn parse_grant_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("GRANT")(input)?;
    let (input, level_str) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected READ, WRITE, or ADMIN after GRANT".into(),
        )))
    })?;
    let level = match level_str.to_ascii_uppercase().as_str() {
        "READ" => gleaph_types::AccessLevel::Read,
        "WRITE" => gleaph_types::AccessLevel::Write,
        "ADMIN" => gleaph_types::AccessLevel::Admin,
        other => {
            return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                format!("unknown access level: {other}"),
            ))));
        }
    };
    // ON GRAPH
    let (input, _) = kw("ON")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ON GRAPH after access level".into(),
        )))
    })?;
    let (input, _) = kw("GRAPH")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected GRAPH after ON".into(),
        )))
    })?;
    // TO '<principal>'
    let (input, _) = kw("TO")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected TO after ON GRAPH".into(),
        )))
    })?;
    let (input, principal) = parse_string_literal(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected quoted principal after TO".into(),
        )))
    })?;
    Ok((input, Statement::Grant { level, principal }))
}

/// Parse `REVOKE ACCESS ON GRAPH FROM '<principal>'`.
fn parse_revoke_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("REVOKE")(input)?;
    let (input, _) = kw("ACCESS")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ACCESS after REVOKE".into(),
        )))
    })?;
    let (input, _) = kw("ON")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ON after REVOKE ACCESS".into(),
        )))
    })?;
    let (input, _) = kw("GRAPH")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected GRAPH after ON".into(),
        )))
    })?;
    let (input, _) = kw("FROM")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected FROM after ON GRAPH".into(),
        )))
    })?;
    let (input, principal) = parse_string_literal(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected quoted principal after FROM".into(),
        )))
    })?;
    Ok((input, Statement::Revoke { principal }))
}

/// Parse `SET TYPE CHECK STRICT|WARNING` — toggle error-mode type checking (§18.9 Phase 3).
fn parse_set_type_check_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("SET")(input)?;
    let (input, _) = kw("TYPE")(input)?;
    let (input, _) = kw("CHECK")(input)?;
    let (input, mode_str) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected STRICT or WARNING after SET TYPE CHECK".into(),
        )))
    })?;
    let mode = match mode_str.to_ascii_uppercase().as_str() {
        "STRICT" => crate::ast::TypeCheckMode::Strict,
        "WARNING" => crate::ast::TypeCheckMode::Warning,
        other => {
            return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                format!("expected STRICT or WARNING, got '{other}'"),
            ))));
        }
    };
    Ok((input, Statement::SetTypeCheck(mode)))
}

/// Parse `ANALYZE` — recompute planner statistics.
fn parse_analyze_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("ANALYZE")(input)?;
    Ok((input, Statement::Analyze))
}

/// Parse `CREATE INDEX ON :Label(property)` or `CREATE INDEX ON -[:Label](property)`.
/// For simplicity, we parse: `CREATE INDEX ON` then detect entity type from `:` (vertex) or `-` (edge),
/// skip the label part, and extract the property name from the parenthesized part.
fn parse_create_index_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("CREATE")(input)?;
    let (input, _) = kw("INDEX")(input)?;
    let (input, _) = kw("ON")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ON after CREATE INDEX".into(),
        )))
    })?;
    parse_index_target(input, true)
}

/// Parse `DROP INDEX ON :Label(property)` or `DROP INDEX ON -[:Label](property)`.
fn parse_drop_index_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DROP")(input)?;
    let (input, _) = kw("INDEX")(input)?;
    let (input, _) = kw("ON")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ON after DROP INDEX".into(),
        )))
    })?;
    parse_index_target(input, false)
}

/// Shared helper: parse `:Label(property)` or `-[:Label](property)` after `ON`.
fn parse_index_target<'a>(input: Tokens<'a>, is_create: bool) -> TResult<'a, Statement> {
    // Detect entity type: `:Label(prop)` for vertex, `-[:Label](prop)` for edge
    let (input, entity_type) = if matches!(input.first(), Some(Token::Colon)) {
        // Vertex: :Label(property)
        let (input, _) = punct(Token::Colon)(input)?;
        let (input, _label) = ident(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected label after ':'".into(),
            )))
        })?;
        (input, gleaph_types::EntityType::Vertex)
    } else if matches!(input.first(), Some(Token::Minus)) {
        // Edge: -[:Label](property)
        let (input, _) = punct(Token::Minus)(input)?;
        let (input, _) = punct(Token::LBracket)(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected '[' after '-' in edge index target".into(),
            )))
        })?;
        let (input, _) = punct(Token::Colon)(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected ':' inside edge brackets".into(),
            )))
        })?;
        let (input, _label) = ident(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected edge label".into(),
            )))
        })?;
        let (input, _) = punct(Token::RBracket)(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected ']' closing edge brackets".into(),
            )))
        })?;
        (input, gleaph_types::EntityType::Edge)
    } else {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ':Label(property)' or '-[:Label](property)' after ON".into(),
        ))));
    };
    // (property)
    let (input, _) = punct(Token::LParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '(' before property name".into(),
        )))
    })?;
    let (input, property_name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected property name inside parentheses".into(),
        )))
    })?;
    let (input, _) = punct(Token::RParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ')' after property name".into(),
        )))
    })?;
    let stmt = if is_create {
        Statement::CreateIndex {
            entity_type,
            property_name,
        }
    } else {
        Statement::DropIndex {
            entity_type,
            property_name,
        }
    };
    Ok((input, stmt))
}

/// Parse `CREATE CONSTRAINT name ON (:Label) ASSERT property IS UNIQUE|NOT NULL`.
fn parse_create_constraint_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("CREATE")(input)?;
    let (input, _) = kw("CONSTRAINT")(input)?;
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected constraint name after CREATE CONSTRAINT".into(),
        )))
    })?;
    let (input, _) = kw("ON")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ON after constraint name".into(),
        )))
    })?;
    // (:Label)
    let (input, _) = punct(Token::LParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '(' before label".into(),
        )))
    })?;
    let (input, _) = punct(Token::Colon)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ':' before label name".into(),
        )))
    })?;
    let (input, label) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected label name".into(),
        )))
    })?;
    let (input, _) = punct(Token::RParen)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ')' after label".into(),
        )))
    })?;
    // ASSERT property IS UNIQUE | IS NOT NULL
    let (input, _) = kw("ASSERT")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected ASSERT after (:Label)".into(),
        )))
    })?;
    let (input, property) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected property name after ASSERT".into(),
        )))
    })?;
    let (input, _) = kw("IS")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected IS after property name".into(),
        )))
    })?;
    // UNIQUE or NOT NULL
    let (input, kind) = if starts_kw(input, "UNIQUE") {
        let (input, _) = kw("UNIQUE")(input)?;
        (input, ConstraintKind::Unique)
    } else if starts_kw(input, "NOT") {
        let (input, _) = kw("NOT")(input)?;
        let (input, _) = kw("NULL")(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected NULL after NOT".into(),
            )))
        })?;
        (input, ConstraintKind::NotNull)
    } else {
        return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected UNIQUE or NOT NULL after IS".into(),
        ))));
    };
    Ok((
        input,
        Statement::CreateConstraint(ConstraintDef {
            name,
            label,
            property,
            kind,
        }),
    ))
}

/// Parse `DROP CONSTRAINT name`.
fn parse_drop_constraint_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("DROP")(input)?;
    let (input, _) = kw("CONSTRAINT")(input)?;
    let (input, name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected constraint name after DROP CONSTRAINT".into(),
        )))
    })?;
    Ok((input, Statement::DropConstraint(name)))
}

/// Extract a string literal token value.
fn parse_string_literal<'a>(input: Tokens<'a>) -> TResult<'a, String> {
    match input.split_first() {
        Some((Token::String(s), rest)) => Ok((rest, s.clone())),
        _ => Err(perr(input, ErrorKind::Tag)),
    }
}

/// §14.8: Parse `FOR <var> IN <expr> [WITH ORDINALITY <idx_var>] RETURN ...`
fn parse_for_statement<'a>(input: Tokens<'a>) -> TResult<'a, Statement> {
    let (input, _) = kw("FOR")(input)?;
    let (input, var) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected identifier after FOR".into(),
        )))
    })?;
    let (input, _) = kw("IN")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected IN after FOR variable".into(),
        )))
    })?;
    let (input, list_expr) = parse_expr(input)?;
    // Optional WITH ORDINALITY <idx_var>
    let (input, ordinality_var) = if starts_kw(input, "WITH")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("ORDINALITY")))
    {
        let (i, _) = kw("WITH")(input)?;
        let (i, _) = kw("ORDINALITY")(i)?;
        let (i, idx_var) = ident(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected identifier after WITH ORDINALITY".into(),
            )))
        })?;
        (i, Some(idx_var))
    } else {
        (input, None)
    };
    let (input, _) = kw("RETURN")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected RETURN after FOR expression".into(),
        )))
    })?;
    let (input, return_clause) = parse_return_clause(input)?;
    Ok((
        input,
        Statement::For(crate::ast::ForStmt {
            var,
            list_expr,
            ordinality_var,
            return_clause,
        }),
    ))
}

#[allow(clippy::type_complexity)]
fn parse_match_entry_clause<'a>(
    input: Tokens<'a>,
) -> TResult<
    'a,
    (
        bool,
        Option<crate::ast::ShortestMode>,
        Option<String>,
        Option<crate::ast::PathMode>,
        MatchClause,
        Option<u32>,
        Option<crate::ast::KeepClause>,
    ),
> {
    // §16.6: Parse optional path mode keyword (WALK / TRAIL / SIMPLE / ACYCLIC).
    let (input, path_mode) = if starts_kw(input, "WALK") {
        let (i, _) = kw("WALK")(input)?;
        (i, Some(crate::ast::PathMode::Walk))
    } else if starts_kw(input, "TRAIL") {
        let (i, _) = kw("TRAIL")(input)?;
        (i, Some(crate::ast::PathMode::Trail))
    } else if starts_kw(input, "SIMPLE") {
        let (i, _) = kw("SIMPLE")(input)?;
        (i, Some(crate::ast::PathMode::Simple))
    } else if starts_kw(input, "ACYCLIC") {
        let (i, _) = kw("ACYCLIC")(input)?;
        (i, Some(crate::ast::PathMode::Acyclic))
    } else {
        (input, None)
    };

    // §16.6: Parse ANY SHORTEST prefix (before ANY n PATHS, since both start with ANY).
    let (input, any_shortest) = if starts_kw(input, "ANY")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("SHORTEST")))
    {
        let (i, _) = kw("ANY")(input)?;
        let (i, _) = kw("SHORTEST")(i)?;
        (i, true)
    } else {
        (input, false)
    };

    // §16.6: Parse ANY n PATHS prefix.
    let (input, any_paths) = if !any_shortest
        && starts_kw(input, "ANY")
        && input.get(1).is_some_and(|t| matches!(t, Token::Int(_)))
        && input
            .get(2)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("PATHS")))
    {
        let (i, _) = kw("ANY")(input)?;
        let n = match i.first() {
            Some(Token::Int(n)) => *n as u32,
            _ => 1,
        };
        let i = &i[1..]; // consume the integer
        let (i, _) = kw("PATHS")(i)?;
        (i, Some(n))
    } else {
        (input, None)
    };

    // §16.6: Parse ALL SHORTEST / ALL PATHS / SHORTEST GROUP / SHORTEST k / SHORTEST / ANY SHORTEST
    let (input, shortest_mode) = if any_shortest {
        // ANY SHORTEST — equivalent to SHORTEST (return one shortest path).
        (input, Some(crate::ast::ShortestMode::One))
    } else if starts_kw(input, "ALL")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("SHORTEST")))
    {
        let (i, _) = kw("ALL")(input)?;
        let (i, _) = kw("SHORTEST")(i)?;
        (i, Some(crate::ast::ShortestMode::All))
    } else if starts_kw(input, "ALL")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("PATHS")))
    {
        // ALL PATHS — return all matching paths (no shortest filtering).
        let (i, _) = kw("ALL")(input)?;
        let (i, _) = kw("PATHS")(i)?;
        (i, None)
    } else if starts_kw(input, "SHORTEST")
        && input
            .get(1)
            .is_some_and(|t| matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case("GROUP")))
    {
        let (i, _) = kw("SHORTEST")(input)?;
        let (i, _) = kw("GROUP")(i)?;
        (i, Some(crate::ast::ShortestMode::Group))
    } else if starts_kw(input, "SHORTEST") {
        let (i, _) = kw("SHORTEST")(input)?;
        // SHORTEST k N — optional integer follows
        if let Some(Token::Int(n)) = i.first() {
            let n = *n as u32;
            let i = &i[1..];
            (i, Some(crate::ast::ShortestMode::K(n)))
        } else {
            (i, Some(crate::ast::ShortestMode::One))
        }
    } else {
        (input, None)
    };
    let shortest = shortest_mode.is_some();

    let (input, path_var, pattern) =
        if let (Some(Token::Ident(_)), Some(Token::Eq)) = (input.first(), input.get(1)) {
            let (input, path_var) = ident_non_kw(input)?;
            let (input, _) = punct(Token::Eq)(input)?;
            let (input, pattern) = parse_match_clause(input)?;
            (input, Some(path_var), pattern)
        } else {
            let (input, pattern) = parse_match_clause(input)?;
            (input, None, pattern)
        };

    // §16.4: Parse optional KEEP clause.
    let (input, keep_clause) = if starts_kw(input, "KEEP") {
        let (i, _) = kw("KEEP")(input)?;
        if matches!(i.first(), Some(Token::Star)) {
            let i = &i[1..];
            (i, Some(crate::ast::KeepClause::All))
        } else {
            let mut vars = Vec::new();
            let (mut i, v) = ident_non_kw(i)?;
            vars.push(v);
            while matches!(i.first(), Some(Token::Comma)) {
                i = &i[1..];
                let (j, v) = ident_non_kw(i)?;
                vars.push(v);
                i = j;
            }
            (i, Some(crate::ast::KeepClause::Vars(vars)))
        }
    } else {
        (input, None)
    };

    Ok((
        input,
        (
            shortest,
            shortest_mode,
            path_var,
            path_mode,
            pattern,
            any_paths,
            keep_clause,
        ),
    ))
}

fn set_statement<'a>(
    input: Tokens<'a>,
    match_clause: MatchClause,
    where_clause: Option<WhereClause>,
) -> TResult<'a, Statement> {
    let (i, _) = kw("SET")(input)?;
    let (i, set_clause) = parse_set_clause(i)?;
    Ok((
        i,
        Statement::Set(SetStmt {
            match_clause,
            where_clause,
            set_clause,
        }),
    ))
}

fn remove_statement<'a>(
    input: Tokens<'a>,
    match_clause: MatchClause,
    where_clause: Option<WhereClause>,
) -> TResult<'a, Statement> {
    let (i, _) = kw("REMOVE")(input)?;
    let (i, remove_clause) = parse_remove_clause(i)?;
    Ok((
        i,
        Statement::Remove(RemoveStmt {
            match_clause,
            where_clause,
            remove_clause,
        }),
    ))
}

fn parse_match_clause<'a>(mut input: Tokens<'a>) -> TResult<'a, MatchClause> {
    let (i, start) = parse_node_pattern(input)?;
    input = i;

    let mut chains = Vec::new();
    loop {
        if starts_edge(input) {
            let (i, edge) = parse_edge_pattern(input)?;
            let (i, node) = parse_node_pattern(i)?;
            chains.push(PatternElement::Hop(MatchChain { edge, node }));
            input = i;
        } else if matches!(input.first(), Some(Token::LParen))
            && matches!(input.get(1), Some(Token::LParen))
        {
            // Parenthesized subpath: ((x)-[:E]->(y)){quantifier}
            let i = &input[1..]; // skip outer '('
            let (i, inner_start) = parse_node_pattern(i)?;
            let mut inner_elements = Vec::new();
            let mut i = i;
            while starts_edge(i) {
                let (i2, edge) = parse_edge_pattern(i)?;
                let (i2, node) = parse_node_pattern(i2)?;
                inner_elements.push(PatternElement::Hop(MatchChain { edge, node }));
                i = i2;
            }
            // closing ')' for the subpath group
            let (i, _) = punct(Token::RParen)(i)?;
            // parse optional quantifier {n} or {n,m} or *n..m
            let mut i = i;
            let quantifier = parse_edge_path_length(&mut i)?;
            // Parse optional trailing node: `){quantifier}(b)`
            let (i, trailing_node) = if matches!(i.first(), Some(Token::LParen))
                && !matches!(i.get(1), Some(Token::LParen))
            {
                let (i, node) = parse_node_pattern(i)?;
                (i, Some(node))
            } else {
                (i, None)
            };
            chains.push(PatternElement::SubPath {
                inner_start,
                inner_elements,
                quantifier,
                var: None,
                trailing_node,
            });
            input = i;
        } else {
            break;
        }
    }

    Ok((
        input,
        MatchClause {
            start,
            elements: chains,
        },
    ))
}

fn parse_node_pattern<'a>(input: Tokens<'a>) -> TResult<'a, NodePattern> {
    let (input, _) = punct(Token::LParen)(input)?;
    let (input, var) = opt(ident_non_kw).parse(input)?;
    // Try `::` type annotation first (before labels, since `::` is exclusive with labels)
    let (input, labels, label_expr, type_annotation) =
        if matches!(input.first(), Some(Token::Colon))
            && matches!(input.get(1), Some(Token::Colon))
            && !matches!(input.get(2), Some(Token::Colon))
        // not `:::`
        {
            // `:: TypeExpr` — consume both colons
            let input = &input[2..];
            let (input, te) = parse_type_expr(input)?;
            (input, vec![], None, Some(te))
        } else {
            let (input, (labels, label_expr)) = parse_node_labels_and_expr(input)?;
            (input, labels, label_expr, None)
        };
    let (input, props_hint) = if matches!(input.first(), Some(Token::LBrace)) {
        let (input, props) = parse_property_map(input)?;
        (input, props)
    } else {
        (input, Vec::new())
    };
    // Optional inline WHERE: `(a:Person WHERE a.age > 25)`
    let (input, where_clause) = if starts_kw(input, "WHERE") {
        let (i, _) = kw("WHERE")(input)?;
        let (i, expr) = parse_expr(i)?;
        (i, Some(Box::new(expr)))
    } else {
        (input, None)
    };
    let (input, _) = punct(Token::RParen)(input)?;
    Ok((
        input,
        NodePattern {
            var,
            labels,
            props_hint,
            label_expr,
            where_clause,
            type_annotation,
        },
    ))
}

/// Parses a type expression: `TypeName ( '|' TypeName )*`.
fn parse_type_expr<'a>(input: Tokens<'a>) -> TResult<'a, TypeExpr> {
    let (mut input, first_name) = ident(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected type name after '::'".into(),
        )))
    })?;
    let mut te = TypeExpr::Name(first_name);
    while matches!(input.first(), Some(Token::Pipe)) {
        input = &input[1..]; // consume `|`
        let (i, name) = ident(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected type name after '|' in type expression".into(),
            )))
        })?;
        te = TypeExpr::Union(Box::new(te), Box::new(TypeExpr::Name(name)));
        input = i;
    }
    Ok((input, te))
}

fn parse_node_labels_and_expr<'a>(
    mut input: Tokens<'a>,
) -> TResult<'a, (Vec<String>, Option<LabelExpr>)> {
    // Check if next is colon
    if !matches!(input.first(), Some(Token::Colon)) {
        return Ok((input, (vec![], None)));
    }
    // Peek ahead: after `:`, is there `!` or `%`? If so, use label expr
    let peek = input.get(1);
    if matches!(peek, Some(Token::Bang) | Some(Token::Percent)) {
        // Label expression starting with ! or %
        let (i, _) = punct(Token::Colon)(input)?;
        let (i, le) = parse_label_expr(i)?;
        return Ok((i, (vec![], Some(le))));
    }
    // Try: parse first label
    let (i, _) = punct(Token::Colon)(input)?;
    let Ok((i, first_label)) = ident(i) else {
        return Ok((input, (vec![], None)));
    };
    if matches!(i.first(), Some(Token::Ampersand) | Some(Token::Pipe)) {
        // Label expression: combine first label with &/| continuation
        let first_le = LabelExpr::Name(first_label);
        let (i, rest_le) = match i.first() {
            Some(Token::Ampersand) => {
                let (i, _) = punct(Token::Ampersand)(i)?;
                let (i, rhs) = parse_label_expr(i)?;
                (i, LabelExpr::And(Box::new(first_le), Box::new(rhs)))
            }
            Some(Token::Pipe) => {
                let (i, _) = punct(Token::Pipe)(i)?;
                let (i, rhs) = parse_label_expr(i)?;
                (i, LabelExpr::Or(Box::new(first_le), Box::new(rhs)))
            }
            _ => unreachable!(),
        };
        return Ok((i, (vec![], Some(rest_le))));
    }
    // Old-style colon-separated labels: got first_label, try more
    let mut labels = vec![first_label];
    input = i;
    while matches!(input.first(), Some(Token::Colon)) {
        let (i, _) = punct(Token::Colon)(input)?;
        let Ok((i, label)) = ident(i) else {
            break;
        };
        labels.push(label);
        input = i;
    }
    Ok((input, (labels, None)))
}

fn parse_label_expr<'a>(input: Tokens<'a>) -> TResult<'a, LabelExpr> {
    parse_label_expr_prec(input, 0)
}

fn parse_label_expr_prec<'a>(input: Tokens<'a>, min_prec: u8) -> TResult<'a, LabelExpr> {
    let (mut input, mut lhs) = parse_label_expr_primary(input)?;
    loop {
        match input.first() {
            Some(Token::Ampersand) if min_prec <= 2 => {
                let (i, _) = punct(Token::Ampersand)(input)?;
                let (i, rhs) = parse_label_expr_prec(i, 3)?;
                lhs = LabelExpr::And(Box::new(lhs), Box::new(rhs));
                input = i;
            }
            Some(Token::Pipe) if min_prec <= 1 => {
                let (i, _) = punct(Token::Pipe)(input)?;
                let (i, rhs) = parse_label_expr_prec(i, 2)?;
                lhs = LabelExpr::Or(Box::new(lhs), Box::new(rhs));
                input = i;
            }
            _ => break,
        }
    }
    Ok((input, lhs))
}

fn parse_label_expr_primary<'a>(input: Tokens<'a>) -> TResult<'a, LabelExpr> {
    match input.first() {
        Some(Token::Bang) => {
            let (input, _) = punct(Token::Bang)(input)?;
            let (input, expr) = parse_label_expr_primary(input)?;
            Ok((input, LabelExpr::Not(Box::new(expr))))
        }
        Some(Token::Percent) => {
            let (input, _) = punct(Token::Percent)(input)?;
            Ok((input, LabelExpr::Wildcard))
        }
        Some(Token::Ident(_)) => {
            let (input, name) = ident(input)?;
            Ok((input, LabelExpr::Name(name)))
        }
        Some(Token::LParen) => {
            let (input, _) = punct(Token::LParen)(input)?;
            let (input, expr) = parse_label_expr(input)?;
            let (input, _) = punct(Token::RParen)(input)?;
            Ok((input, expr))
        }
        _ => fail(GleaphError::ParseError("expected label expression".into())),
    }
}

fn parse_edge_pattern<'a>(input: Tokens<'a>) -> TResult<'a, EdgePattern> {
    // Reject all tilde-based undirected syntax: ~[...]~, ~[...]~>, <~[...]~, ~/L/~, etc.
    if matches!(input.first(), Some(Token::Tilde))
        || matches!(
            (input.first(), input.get(1)),
            (Some(Token::Lt), Some(Token::Tilde))
        )
    {
        return fail(GleaphError::ParseError(
            "undirected edge syntax ~[...]~ is not supported. Gleaph is a directed-only graph database. Use -[...]- for bidirectional matching".into(),
        ));
    }

    if matches!(input.first(), Some(Token::ArrowLeft)) {
        // Could be <-[edge_inner]- or <-/Label/-
        let (input, _) = punct(Token::ArrowLeft)(input)?;
        if matches!(input.first(), Some(Token::Slash)) {
            // Simplified incoming or either: <-/Label/- or <-/Label/->
            return parse_simplified_edge_tail(input, true);
        }
        let (input, _) = punct(Token::LBracket)(input)?;
        let (input, (var, label, label_expr, length, properties, where_clause, type_annotation)) =
            parse_edge_inner(input)?;
        let (input, _) = punct(Token::RBracket)(input)?;
        let (input, _) = punct(Token::Minus)(input)?;
        Ok((
            input,
            EdgePattern {
                var,
                label,
                label_expr,
                direction: Direction::Incoming,
                length,
                properties,
                where_clause,
                type_annotation,
            },
        ))
    } else {
        // Starts with Minus: -[edge_inner]-> or -[edge_inner]- or -/Label/-> or -/Label/-
        let (input, _) = punct(Token::Minus)(input)?;
        if matches!(input.first(), Some(Token::Slash)) {
            // Simplified: -/Label/-> or -/Label/-
            return parse_simplified_edge_tail(input, false);
        }
        let (input, _) = punct(Token::LBracket)(input)?;
        let (input, (var, label, label_expr, length, properties, where_clause, type_annotation)) =
            parse_edge_inner(input)?;
        let (input, _) = punct(Token::RBracket)(input)?;
        // After `]`: `->` means Outgoing, `-` means Either
        if matches!(input.first(), Some(Token::ArrowRight)) {
            let input = &input[1..];
            Ok((
                input,
                EdgePattern {
                    var,
                    label,
                    label_expr,
                    direction: Direction::Outgoing,
                    length,
                    properties,
                    where_clause,
                    type_annotation,
                },
            ))
        } else if matches!(input.first(), Some(Token::Minus)) {
            let input = &input[1..];
            Ok((
                input,
                EdgePattern {
                    var,
                    label,
                    label_expr,
                    direction: Direction::Either,
                    length,
                    properties,
                    where_clause,
                    type_annotation,
                },
            ))
        } else {
            fail(GleaphError::ParseError(
                "expected '->' or '-' at end of edge pattern -[...]. Use -[...]-> for outgoing, -[...]- for bidirectional".into(),
            ))
        }
    }
}

/// Parse the tail of a simplified edge pattern after the opening `-/` or `<-/`.
/// `has_left_arrow`: true when the pattern started with `<-/`, false when `-/`.
/// Expects: Label `/->` or Label `/-`
fn parse_simplified_edge_tail<'a>(
    input: Tokens<'a>,
    has_left_arrow: bool,
) -> TResult<'a, EdgePattern> {
    let (mut input, _) = punct(Token::Slash)(input)?;

    // Parse label expression (single name, OR, AND, NOT, wildcard)
    let (i, le) = parse_label_expr(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected label expression in simplified edge pattern -/Label/->".into(),
        )))
    })?;
    input = i;
    let (label, label_expr) = match le {
        LabelExpr::Name(name) => (Some(name), None),
        complex => (None, Some(complex)),
    };

    // Parse optional quantifier (*, +, *n..m, {n}, {n,m})
    let length = parse_edge_path_length(&mut input)?;

    let (input, _) = punct(Token::Slash)(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected '/' after label in simplified edge pattern".into(),
        )))
    })?;
    // Determine direction from trailing tokens
    let (input, direction) = if matches!(input.first(), Some(Token::ArrowRight)) {
        // .../-> : if has_left_arrow => <-/L/-> = Either, else -/L/-> = Outgoing
        let input = &input[1..];
        let dir = if has_left_arrow {
            Direction::Either
        } else {
            Direction::Outgoing
        };
        (input, dir)
    } else if matches!(input.first(), Some(Token::Minus)) {
        // .../- : if has_left_arrow => <-/L/- = Incoming, else -/L/- = Either
        let input = &input[1..];
        let dir = if has_left_arrow {
            Direction::Incoming
        } else {
            Direction::Either
        };
        (input, dir)
    } else {
        return fail(GleaphError::ParseError(
            "expected '->' or '-' at end of simplified edge pattern /Label/".into(),
        ));
    };
    Ok((
        input,
        EdgePattern {
            var: None,
            label,
            label_expr,
            direction,
            length,
            properties: vec![],
            where_clause: None,
            type_annotation: None,
        },
    ))
}

#[allow(clippy::type_complexity)]
fn parse_edge_inner<'a>(
    mut input: Tokens<'a>,
) -> TResult<
    'a,
    (
        Option<String>,
        Option<String>,
        Option<LabelExpr>,
        PathLength,
        Vec<(String, Expr)>,
        Option<Box<Expr>>,
        Option<TypeExpr>,
    ),
> {
    let (i, var) = opt(ident_non_kw).parse(input)?;
    input = i;

    // Check for `:: TypeExpr` (exclusive with label/label_expr)
    if matches!(input.first(), Some(Token::Colon))
        && matches!(input.get(1), Some(Token::Colon))
        && !matches!(input.get(2), Some(Token::Colon))
    {
        // `:: TypeExpr`
        input = &input[2..];
        let (i, te) = parse_type_expr(input)?;
        input = i;
        // Still parse path length, props, WHERE after the type annotation
        let length = parse_edge_path_length(&mut input)?;
        let (i, properties) = if matches!(input.first(), Some(Token::LBrace)) {
            parse_property_map(input)?
        } else {
            (input, Vec::new())
        };
        input = i;
        let (i, where_clause) = if starts_kw(input, "WHERE") {
            let (i, _) = kw("WHERE")(input)?;
            let (i, expr) = parse_expr(i)?;
            (i, Some(Box::new(expr)))
        } else {
            (input, None)
        };
        return Ok((
            i,
            (var, None, None, length, properties, where_clause, Some(te)),
        ));
    }

    // Parse an optional label or full label expression after `:`.
    // Simple name  →  label = Some(name), label_expr = None
    // Expr (|/&/!) →  label = None,       label_expr = Some(expr)
    let (label, label_expr) = if matches!(input.first(), Some(Token::Colon)) {
        let (i, _) = punct(Token::Colon)(input)?;
        if matches!(i.first(), Some(Token::Bang) | Some(Token::Percent)) {
            // `:!X` or `:%` — start of a label expression
            let (i, le) = parse_label_expr(i)?;
            input = i;
            (None, Some(le))
        } else {
            let (i, name) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            if matches!(i.first(), Some(Token::Pipe) | Some(Token::Ampersand)) {
                // `:A|B` or `:A&B` — build OR/AND expression
                let first = LabelExpr::Name(name);
                let (i, le) = match i.first() {
                    Some(Token::Pipe) => {
                        let (i, _) = punct(Token::Pipe)(i)?;
                        let (i, rhs) = parse_label_expr(i)?;
                        (i, LabelExpr::Or(Box::new(first), Box::new(rhs)))
                    }
                    Some(Token::Ampersand) => {
                        let (i, _) = punct(Token::Ampersand)(i)?;
                        let (i, rhs) = parse_label_expr(i)?;
                        (i, LabelExpr::And(Box::new(first), Box::new(rhs)))
                    }
                    _ => unreachable!(),
                };
                input = i;
                (None, Some(le))
            } else {
                // Simple single label
                input = i;
                (Some(name), None)
            }
        }
    } else {
        (None, None)
    };

    let length = parse_edge_path_length(&mut input)?;

    let (input, properties) = if matches!(input.first(), Some(Token::LBrace)) {
        let (i, props) = parse_property_map(input)?;
        (i, props)
    } else {
        (input, Vec::new())
    };

    // Optional inline WHERE: `[e:KNOWS WHERE e.since > 2020]`
    let (input, where_clause) = if starts_kw(input, "WHERE") {
        let (i, _) = kw("WHERE")(input)?;
        let (i, expr) = parse_expr(i)?;
        (i, Some(Box::new(expr)))
    } else {
        (input, None)
    };

    Ok((
        input,
        (
            var,
            label,
            label_expr,
            length,
            properties,
            where_clause,
            None,
        ),
    ))
}

/// Extracts the path length (quantifier) portion of an edge inner, mutating `input` in place.
fn parse_edge_path_length<'a>(input: &mut Tokens<'a>) -> Result<PathLength, nom::Err<PError<'a>>> {
    const DEFAULT_MAX_HOPS: u32 = 10;
    let mut length = PathLength::Fixed(1);
    if matches!(input.first(), Some(Token::Plus)) {
        *input = &input[1..];
        length = PathLength::Range {
            min: 1,
            max: DEFAULT_MAX_HOPS,
        };
    } else if matches!(input.first(), Some(Token::Star)) {
        *input = &input[1..];
        let (min, has_explicit_min) = if matches!(input.first(), Some(Token::Int(_))) {
            let (i, n) = parse_hop_count(input, "min")?;
            *input = i;
            (n, true)
        } else {
            (1u32, false)
        };
        let max = if matches!(input.first(), Some(Token::RangeDots)) {
            *input = &input[1..];
            if matches!(input.first(), Some(Token::Int(_))) {
                let (i, n) = parse_hop_count(input, "max")?;
                *input = i;
                n
            } else {
                DEFAULT_MAX_HOPS
            }
        } else if has_explicit_min {
            min
        } else {
            DEFAULT_MAX_HOPS
        };
        if min > max {
            return Err(nom::Err::Failure(PError::Gleaph(
                GleaphError::ValidationError("variable-length path requires min <= max".into()),
            )));
        }
        if max > DEFAULT_MAX_HOPS {
            return Err(nom::Err::Failure(PError::Gleaph(
                GleaphError::ValidationError("variable-length path max must be <= 10".into()),
            )));
        }
        length = if min == max {
            PathLength::Fixed(min)
        } else {
            PathLength::Range { min, max }
        };
    } else if matches!(input.first(), Some(Token::LBrace))
        && matches!(input.get(1), Some(Token::Int(_)))
    {
        // Brace quantifiers: `{n}` (fixed) or `{n,m}` (range).
        *input = &input[1..]; // consume LBrace
        let (i, min) = parse_hop_count(input, "lower")?;
        *input = i;
        let max = if matches!(input.first(), Some(Token::Comma)) {
            *input = &input[1..]; // consume Comma
            if matches!(input.first(), Some(Token::Int(_))) {
                let (i, n) = parse_hop_count(input, "upper")?;
                *input = i;
                n
            } else {
                DEFAULT_MAX_HOPS
            }
        } else {
            min
        };
        if !matches!(input.first(), Some(Token::RBrace)) {
            return Err(nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected '}' after brace quantifier".into(),
            ))));
        }
        *input = &input[1..]; // consume RBrace
        if min > max {
            return Err(nom::Err::Failure(PError::Gleaph(
                GleaphError::ValidationError("variable-length path requires min <= max".into()),
            )));
        }
        if max > DEFAULT_MAX_HOPS {
            return Err(nom::Err::Failure(PError::Gleaph(
                GleaphError::ValidationError("variable-length path max must be <= 10".into()),
            )));
        }
        length = if min == max {
            PathLength::Fixed(min)
        } else {
            PathLength::Range { min, max }
        };
    }
    Ok(length)
}

fn parse_property_map<'a>(input: Tokens<'a>) -> TResult<'a, Vec<(String, Expr)>> {
    let (mut input, _) = punct(Token::LBrace)(input)?;
    let mut props = Vec::new();
    if matches!(input.first(), Some(Token::RBrace)) {
        let (input, _) = punct(Token::RBrace)(input)?;
        return Ok((input, props));
    }
    loop {
        let (i, prop) =
            separated_pair(ident, punct(Token::Colon), parse_value_expr).parse(input)?;
        props.push(prop);
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    let (input, _) = punct(Token::RBrace)(input)?;
    Ok((input, props))
}

fn parse_where_clause<'a>(input: Tokens<'a>) -> TResult<'a, WhereClause> {
    parse_expr(input)
}

fn parse_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    parse_expr_prec(input, 0)
}

fn parse_expr_prec<'a>(mut input: Tokens<'a>, min_prec: u8) -> TResult<'a, Expr> {
    let (i, mut lhs) = parse_prefix(input)?;
    input = i;

    loop {
        let Some((prec, op_kind)) = infix_prec(input) else {
            break;
        };
        if prec < min_prec {
            break;
        }

        // consume operator token/keyword
        let (i, op_kind) = consume_infix_op(input, op_kind)?;
        let next_min_prec = prec + 1;
        let (i, rhs) = parse_expr_prec(i, next_min_prec)?;
        lhs = match op_kind {
            InfixOp::Or => Expr::Or(Box::new(lhs), Box::new(rhs)),
            InfixOp::Xor => Expr::Xor(Box::new(lhs), Box::new(rhs)),
            InfixOp::And => Expr::And(Box::new(lhs), Box::new(rhs)),
            InfixOp::Compare(op) => Expr::Compare {
                left: Box::new(lhs),
                op,
                right: Box::new(rhs),
            },
            InfixOp::Add => Expr::BinaryOp {
                op: BinaryOp::Add,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            InfixOp::Sub => Expr::BinaryOp {
                op: BinaryOp::Sub,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            InfixOp::Mul => Expr::BinaryOp {
                op: BinaryOp::Mul,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            InfixOp::Div => Expr::BinaryOp {
                op: BinaryOp::Div,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            InfixOp::Mod => Expr::BinaryOp {
                op: BinaryOp::Mod,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            InfixOp::Concat => Expr::Concat(Box::new(lhs), Box::new(rhs)),
        };
        input = i;
    }

    Ok((input, lhs))
}

#[derive(Clone, Copy)]
enum InfixOp {
    Or,
    Xor,
    And,
    Compare(CmpOp),
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
}

fn infix_prec(input: Tokens<'_>) -> Option<(u8, InfixOp)> {
    match input.first()? {
        Token::Ident(s) if s.eq_ignore_ascii_case("OR") => Some((1, InfixOp::Or)),
        Token::Ident(s) if s.eq_ignore_ascii_case("XOR") => Some((1, InfixOp::Xor)),
        Token::Ident(s) if s.eq_ignore_ascii_case("AND") => Some((2, InfixOp::And)),
        Token::Eq => Some((4, InfixOp::Compare(CmpOp::Eq))),
        Token::Ne => Some((4, InfixOp::Compare(CmpOp::Ne))),
        Token::Lt => Some((4, InfixOp::Compare(CmpOp::Lt))),
        Token::Le => Some((4, InfixOp::Compare(CmpOp::Le))),
        Token::Gt => Some((4, InfixOp::Compare(CmpOp::Gt))),
        Token::Ge => Some((4, InfixOp::Compare(CmpOp::Ge))),
        Token::Plus => Some((5, InfixOp::Add)),
        Token::Minus => Some((5, InfixOp::Sub)),
        Token::Star => Some((6, InfixOp::Mul)),
        Token::Slash => Some((6, InfixOp::Div)),
        Token::Percent => Some((6, InfixOp::Mod)),
        Token::Pipe2 => Some((7, InfixOp::Concat)),
        _ => None,
    }
}

fn consume_infix_op<'a>(input: Tokens<'a>, op: InfixOp) -> TResult<'a, InfixOp> {
    let (input, _) = match op {
        InfixOp::Or => kw("OR")(input)?,
        InfixOp::Xor => kw("XOR")(input)?,
        InfixOp::And => kw("AND")(input)?,
        InfixOp::Compare(CmpOp::Eq) => punct(Token::Eq)(input)?,
        InfixOp::Compare(CmpOp::Ne) => punct(Token::Ne)(input)?,
        InfixOp::Compare(CmpOp::Lt) => punct(Token::Lt)(input)?,
        InfixOp::Compare(CmpOp::Le) => punct(Token::Le)(input)?,
        InfixOp::Compare(CmpOp::Gt) => punct(Token::Gt)(input)?,
        InfixOp::Compare(CmpOp::Ge) => punct(Token::Ge)(input)?,
        InfixOp::Add => punct(Token::Plus)(input)?,
        InfixOp::Sub => punct(Token::Minus)(input)?,
        InfixOp::Mul => punct(Token::Star)(input)?,
        InfixOp::Div => punct(Token::Slash)(input)?,
        InfixOp::Mod => punct(Token::Percent)(input)?,
        InfixOp::Concat => punct(Token::Pipe2)(input)?,
    };
    Ok((input, op))
}

fn parse_prefix<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    match input.first() {
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case("NOT") => {
            let (input, _) = kw("NOT")(input)?;
            let (input, expr) = parse_expr_prec(input, 3)?;
            Ok((input, Expr::Not(Box::new(expr))))
        }
        Some(Token::Minus) => {
            let (input, _) = punct(Token::Minus)(input)?;
            let (input, expr) = parse_expr_prec(input, 8)?;
            Ok((
                input,
                Expr::UnaryOp {
                    op: UnaryOp::Neg,
                    expr: Box::new(expr),
                },
            ))
        }
        Some(Token::Plus) => {
            let (input, _) = punct(Token::Plus)(input)?;
            let (input, expr) = parse_expr_prec(input, 8)?;
            Ok((
                input,
                Expr::UnaryOp {
                    op: UnaryOp::Pos,
                    expr: Box::new(expr),
                },
            ))
        }
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case("EXISTS") => parse_exists_expr(input),
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case("CASE") => parse_case_expr(input),
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case("CAST") => {
            let (input, _) = kw("CAST")(input)?;
            let (input, _) = punct(Token::LParen)(input)?;
            let (input, expr) = parse_expr(input)?;
            let (input, _) = kw("AS")(input).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected AS in CAST".into(),
                )))
            })?;
            let (input, target_type) = parse_value_type_name(input)?;
            let (input, _) = punct(Token::RParen)(input)?;
            apply_postfix_ops(
                input,
                Expr::Cast {
                    expr: Box::new(expr),
                    target_type,
                },
            )
        }
        // §20.6: VALUE { query } — scalar subquery
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case("VALUE") => {
            let (input, _) = kw("VALUE")(input)?;
            let (input, _) = punct(Token::LBrace)(input)?;
            let (input, stmt) = parse_statement_nom(input)?;
            let (input, _) = punct(Token::RBrace)(input)?;
            // Apply postfix ops so `VALUE { ... } IS NULL` etc. parse correctly.
            apply_postfix_ops(input, Expr::ValueSubquery(Box::new(stmt)))
        }
        // §20.14: PATH [n1, e1, n2, ...] — explicit path constructor
        Some(Token::Ident(s))
            if s.eq_ignore_ascii_case("PATH") && matches!(input.get(1), Some(Token::LBracket)) =>
        {
            let (input, _) = kw("PATH")(input)?;
            let (input, elems) = parse_list_literal_expr(input)?;
            let Expr::ListLiteral(elems) = elems else {
                unreachable!()
            };
            apply_postfix_ops(input, Expr::PathConstructor(elems))
        }
        // §20.5: LET x = e1, y = e2 IN body END — value-expression binding
        Some(Token::Ident(s)) if s.eq_ignore_ascii_case("LET") => parse_let_in_expr(input),
        _ => parse_atom(input),
    }
}

/// Parse CASE expression: `CASE [operand] WHEN expr THEN expr ... [ELSE expr] END`.
fn parse_case_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    let (input, _) = kw("CASE")(input)?;
    let (input, operand) = if !starts_kw(input, "WHEN") {
        let (i, e) = parse_expr_prec(input, 0)?;
        (i, Some(Box::new(e)))
    } else {
        (input, None)
    };
    let mut when_then = Vec::new();
    let mut input = input;
    while starts_kw(input, "WHEN") {
        let (i, _) = kw("WHEN")(input)?;
        let (i, when_expr) = parse_expr_prec(i, 0)?;
        let (i, _) = kw("THEN")(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected THEN after WHEN".into(),
            )))
        })?;
        let (i, then_expr) = parse_expr_prec(i, 0)?;
        when_then.push(CaseWhenThen {
            when: when_expr,
            then: then_expr,
        });
        input = i;
    }
    let (input, else_expr) = if starts_kw(input, "ELSE") {
        let (i, _) = kw("ELSE")(input)?;
        let (i, e) = parse_expr_prec(i, 0)?;
        (i, Some(Box::new(e)))
    } else {
        (input, None)
    };
    let (input, _) = kw("END")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected END after CASE".into(),
        )))
    })?;
    Ok((
        input,
        Expr::Case(CaseExpr {
            operand,
            when_then,
            else_expr,
        }),
    ))
}

/// Parse LET expression: `LET x = e1, y = e2 IN body END`.
fn parse_let_in_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    let (mut input, _) = kw("LET")(input)?;
    let mut bindings = Vec::new();
    loop {
        let (i, name) = ident_non_kw(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected identifier in LET binding".into(),
            )))
        })?;
        let (i, _) = punct(Token::Eq)(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected = in LET binding".into(),
            )))
        })?;
        let (i, expr) = parse_expr_prec(i, 0)?;
        bindings.push((name, expr));
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
        } else {
            break;
        }
    }
    let (input, _) = kw("IN")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected IN after LET bindings".into(),
        )))
    })?;
    let (input, body) = parse_expr_prec(input, 0)?;
    let (input, _) = kw("END")(input).map_err(|_| {
        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
            "expected END after LET ... IN body".into(),
        )))
    })?;
    apply_postfix_ops(
        input,
        Expr::LetIn {
            bindings,
            body: Box::new(body),
        },
    )
}

fn parse_exists_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    let (input, _) = kw("EXISTS")(input)?;
    let (input, _) = punct(Token::LBrace)(input)?;
    // §19.4: EXISTS shorthand — `EXISTS { (a)-[:X]->(b) }` desugars to
    // `EXISTS { MATCH (a)-[:X]->(b) RETURN * }` when the first token is `(`.
    let (input, stmt) = if matches!(input.first(), Some(Token::LParen)) {
        let (i, match_clause) = parse_match_clause(input)?;
        let stmt = crate::ast::Statement::Query(crate::ast::QueryStmt {
            match_clauses: vec![crate::ast::MatchEntry {
                optional: false,
                shortest: false,
                shortest_mode: None,
                path_variable: None,
                path_mode: None,
                any_paths: None,
                pattern: match_clause,
                keep_clause: None,
            }],
            where_clause: None,
            with_clauses: vec![],
            // Use NO BINDINGS: we only need to know if any rows exist (for EXISTS check)
            // without projecting specific columns.
            return_clause: crate::ast::ReturnClause {
                distinct: false,
                items: vec![],
                star: false,
                no_bindings: true,
                finish: false,
            },
            group_by: None,
            having: None,
            order_by: None,
            limit: None,
            offset: None,
            match_mode: None,
        });
        (i, stmt)
    } else {
        parse_statement_nom(input)?
    };
    let (input, _) = punct(Token::RBrace)(input)?;
    Ok((input, Expr::Exists(Box::new(stmt))))
}

fn parse_atom<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    let (input, expr) = parse_primary_expr(input)?;
    apply_postfix_ops(input, expr)
}

/// Apply postfix operators (`.prop`, `[idx]`, `(args)`, `IS ...`, `IN [...]`) to an expression.
/// Shared by `parse_atom` and keyword-expression arms in `parse_prefix` (VALUE, LET, etc.)
/// so that e.g. `VALUE { ... } IS NULL` parses correctly.
/// Parse IS [NOT] postfix predicate (NULL, TRUE, FALSE, UNKNOWN, LABELED, SOURCE, DESTINATION, ::type, DIRECTED).
fn parse_is_postfix<'a>(input: Tokens<'a>, expr: Expr) -> TResult<'a, Expr> {
    let (i, _) = kw("IS")(input)?;
    let negated = starts_kw(i, "NOT");
    let i = if negated { kw("NOT")(i)?.0 } else { i };

    if starts_kw(i, "NULL") {
        let (i, _) = kw("NULL")(i)?;
        let expr = if negated {
            Expr::IsNotNull(Box::new(expr))
        } else {
            Expr::IsNull(Box::new(expr))
        };
        Ok((i, expr))
    } else if starts_kw(i, "TRUE") {
        let (i, _) = kw("TRUE")(i)?;
        Ok((
            i,
            Expr::IsTruth {
                expr: Box::new(expr),
                negated,
                truth: TruthValue::True,
            },
        ))
    } else if starts_kw(i, "FALSE") {
        let (i, _) = kw("FALSE")(i)?;
        Ok((
            i,
            Expr::IsTruth {
                expr: Box::new(expr),
                negated,
                truth: TruthValue::False,
            },
        ))
    } else if starts_kw(i, "UNKNOWN") {
        let (i, _) = kw("UNKNOWN")(i)?;
        Ok((
            i,
            Expr::IsTruth {
                expr: Box::new(expr),
                negated,
                truth: TruthValue::Unknown,
            },
        ))
    } else if starts_kw(i, "LABELED") || starts_kw(i, "LABELLED") {
        let i = if starts_kw(i, "LABELED") {
            kw("LABELED")(i)?.0
        } else {
            kw("LABELLED")(i)?.0
        };
        let (i, _) = punct(Token::Colon)(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected ':' after IS LABELED".into(),
            )))
        })?;
        let (i, label_expr) = parse_label_expr(i)?;
        Ok((
            i,
            Expr::IsLabeled {
                expr: Box::new(expr),
                negated,
                label_expr,
            },
        ))
    } else if starts_kw(i, "SOURCE") {
        let (i, _) = kw("SOURCE")(i)?;
        let (i, _) = kw("OF")(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected OF after IS SOURCE".into(),
            )))
        })?;
        let (i, edge_expr) = parse_expr_prec(i, 0)?;
        Ok((
            i,
            Expr::IsSourceOf {
                node: Box::new(expr),
                negated,
                edge: Box::new(edge_expr),
            },
        ))
    } else if starts_kw(i, "DESTINATION") {
        let (i, _) = kw("DESTINATION")(i)?;
        let (i, _) = kw("OF")(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected OF after IS DESTINATION".into(),
            )))
        })?;
        let (i, edge_expr) = parse_expr_prec(i, 0)?;
        Ok((
            i,
            Expr::IsDestOf {
                node: Box::new(expr),
                negated,
                edge: Box::new(edge_expr),
            },
        ))
    } else if matches!(i.first(), Some(Token::Colon)) && matches!(i.get(1), Some(Token::Colon)) {
        // §19.6: IS [NOT] :: typename — runtime type predicate
        let i = &i[2..];
        let (i, type_name) = ident(i).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected type name after IS :: ".into(),
            )))
        })?;
        let value_type = parse_value_type(&type_name);
        Ok((
            i,
            Expr::IsType {
                expr: Box::new(expr),
                negated,
                value_type,
                type_name,
            },
        ))
    } else if starts_kw(i, "DIRECTED") {
        let (i, _) = kw("DIRECTED")(i)?;
        Ok((
            i,
            Expr::IsDirected {
                expr: Box::new(expr),
                negated,
            },
        ))
    } else {
        fail(GleaphError::ParseError("expected NULL, TRUE, FALSE, UNKNOWN, LABELED, DIRECTED, SOURCE, DESTINATION, or :: typename after IS [NOT]".into()))
    }
}

/// Parse a string predicate postfix: STARTS WITH, ENDS WITH, CONTAINS, LIKE, ILIKE.
/// Returns `None` if the current position is not a string predicate keyword.
fn try_parse_string_predicate<'a>(
    input: Tokens<'a>,
    expr: Expr,
    negated: bool,
) -> Option<TResult<'a, Expr>> {
    let kind = if starts_kw(input, "STARTS")
        && matches!(input.get(1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("WITH"))
    {
        Some(crate::ast::StringPredicateKind::StartsWith)
    } else if starts_kw(input, "ENDS")
        && matches!(input.get(1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("WITH"))
    {
        Some(crate::ast::StringPredicateKind::EndsWith)
    } else if starts_kw(input, "CONTAINS") {
        Some(crate::ast::StringPredicateKind::Contains)
    } else if starts_kw(input, "LIKE") {
        Some(crate::ast::StringPredicateKind::Like)
    } else if starts_kw(input, "ILIKE") {
        Some(crate::ast::StringPredicateKind::ILike)
    } else {
        None
    };
    let kind = kind?;
    Some((|| {
        let i = match kind {
            crate::ast::StringPredicateKind::StartsWith => {
                let (i, _) = kw("STARTS")(input)?;
                kw("WITH")(i)?.0
            }
            crate::ast::StringPredicateKind::EndsWith => {
                let (i, _) = kw("ENDS")(input)?;
                kw("WITH")(i)?.0
            }
            crate::ast::StringPredicateKind::Contains => kw("CONTAINS")(input)?.0,
            crate::ast::StringPredicateKind::Like => kw("LIKE")(input)?.0,
            crate::ast::StringPredicateKind::ILike => kw("ILIKE")(input)?.0,
        };
        let (i, pattern) = parse_atom(i)?;
        let pred = Expr::StringPredicate {
            expr: Box::new(expr),
            kind,
            pattern: Box::new(pattern),
        };
        let result = if negated {
            Expr::Not(Box::new(pred))
        } else {
            pred
        };
        Ok((i, result))
    })())
}

/// Parse IN [list] / IN $param or NOT IN [list] / NOT IN $param postfix predicate.
fn try_parse_in_list<'a>(
    input: Tokens<'a>,
    expr: Expr,
    negated: bool,
) -> Option<TResult<'a, Expr>> {
    if !starts_kw(input, "IN") {
        return None;
    }
    // Must be followed by `[` (literal list) or `$param` (parameter reference).
    // The `[` lookahead avoids consuming `IN` in `LET x IN ...` contexts.
    let next = input.get(1);
    let is_list = matches!(next, Some(Token::LBracket));
    let is_param = matches!(next, Some(Token::Param(_)));
    if !is_list && !is_param {
        return None;
    }
    Some((|| {
        let (i, _) = kw("IN")(input)?;
        if is_list {
            let (i, list_expr) = parse_list_literal_expr(i)?;
            let Expr::ListLiteral(list) = list_expr else {
                unreachable!()
            };
            Ok((
                i,
                Expr::InList {
                    expr: Box::new(expr),
                    list,
                    negated,
                },
            ))
        } else {
            // IN $param — parse the parameter expression (which may have :: annotation).
            let (i, param_expr) = parse_atom(i)?;
            Ok((
                i,
                Expr::InList {
                    expr: Box::new(expr),
                    list: vec![param_expr],
                    negated,
                },
            ))
        }
    })())
}

fn apply_postfix_ops<'a>(mut input: Tokens<'a>, mut expr: Expr) -> TResult<'a, Expr> {
    loop {
        if matches!(input.first(), Some(Token::LParen)) {
            let (i, call_expr) = parse_call_or_aggregate(expr, input)?;
            expr = call_expr;
            input = i;
            continue;
        }
        if matches!(input.first(), Some(Token::Dot)) {
            let (i, _) = punct(Token::Dot)(input)?;
            let (i, property) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            expr = Expr::PropertyAccess {
                target: Box::new(expr),
                property,
            };
            input = i;
            continue;
        }
        if matches!(input.first(), Some(Token::LBracket)) {
            let (i, _) = punct(Token::LBracket)(input)?;
            let (i, index) = parse_expr(i)?;
            let (i, _) = punct(Token::RBracket)(i)?;
            expr = Expr::ListIndex {
                list: Box::new(expr),
                index: Box::new(index),
            };
            input = i;
            continue;
        }
        if starts_kw(input, "IS") {
            let (i, e) = parse_is_postfix(input, expr)?;
            expr = e;
            input = i;
            continue;
        }
        // `IN [list]`
        if let Some(result) = try_parse_in_list(input, expr.clone(), false) {
            let (i, e) = result?;
            expr = e;
            input = i;
            continue;
        }
        // `NOT IN [list]` / `NOT STARTS WITH` / `NOT ENDS WITH` / `NOT CONTAINS` / `NOT LIKE` / `NOT ILIKE`
        if starts_kw(input, "NOT") {
            let next = input.get(1);
            let is_postfix_pred = matches!(next, Some(Token::Ident(s))
                if s.eq_ignore_ascii_case("IN") || s.eq_ignore_ascii_case("STARTS")
                || s.eq_ignore_ascii_case("ENDS") || s.eq_ignore_ascii_case("CONTAINS")
                || s.eq_ignore_ascii_case("LIKE") || s.eq_ignore_ascii_case("ILIKE"));
            if !is_postfix_pred {
                break;
            }
            let (i, _) = kw("NOT")(input)?;
            if let Some(result) = try_parse_in_list(i, expr.clone(), true) {
                let (i, e) = result?;
                expr = e;
                input = i;
                continue;
            }
            if let Some(result) = try_parse_string_predicate(i, expr.clone(), true) {
                let (i, e) = result?;
                expr = e;
                input = i;
                continue;
            }
            break;
        }
        // Positive string predicates: STARTS WITH, ENDS WITH, CONTAINS, LIKE, ILIKE
        if let Some(result) = try_parse_string_predicate(input, expr.clone(), false) {
            let (i, e) = result?;
            expr = e;
            input = i;
            continue;
        }
        break;
    }
    Ok((input, expr))
}

fn parse_call_or_aggregate<'a>(callee: Expr, input: Tokens<'a>) -> TResult<'a, Expr> {
    let Expr::Variable(name) = callee else {
        return fail(GleaphError::ParseError(
            "expression is not callable in this phase".into(),
        ));
    };

    let (input, parsed) = parse_call_args_maybe_star(input)?;
    if let Some((distinct, count_all, args)) = parsed {
        // STRING_AGG(expr, separator) — special 2-arg aggregate
        if name.eq_ignore_ascii_case("STRING_AGG") {
            let mut iter = args.into_iter();
            let expr = iter.next();
            let separator = iter.next();
            return Ok((
                input,
                Expr::Aggregate(AggregateExpr {
                    func: AggFunc::StringAgg,
                    expr: expr.map(Box::new),
                    distinct,
                    count_all: false,
                    separator: separator.map(Box::new),
                }),
            ));
        }
        // PERCENTILE_CONT(expr, p) / PERCENTILE_DISC(expr, p) — 2-arg percentile aggregates
        if name.eq_ignore_ascii_case("PERCENTILE_CONT")
            || name.eq_ignore_ascii_case("PERCENTILE_DISC")
        {
            let func = if name.eq_ignore_ascii_case("PERCENTILE_CONT") {
                AggFunc::PercentileCont
            } else {
                AggFunc::PercentileDisc
            };
            let mut iter = args.into_iter();
            let expr = iter.next();
            let percentile = iter.next();
            return Ok((
                input,
                Expr::Aggregate(AggregateExpr {
                    func,
                    expr: expr.map(Box::new),
                    distinct,
                    count_all: false,
                    separator: percentile.map(Box::new),
                }),
            ));
        }
        if let Some(func) = parse_agg_func_name(&name) {
            let expr = if count_all {
                Expr::Aggregate(AggregateExpr {
                    func,
                    expr: None,
                    distinct,
                    count_all: true,
                    separator: None,
                })
            } else {
                let expr = args.into_iter().next();
                Expr::Aggregate(AggregateExpr {
                    func,
                    expr: expr.map(Box::new),
                    distinct,
                    count_all: false,
                    separator: None,
                })
            };
            return Ok((input, expr));
        }
        if count_all {
            return fail(GleaphError::ParseError("only COUNT(*) is supported".into()));
        }
        return Ok((input, Expr::FunctionCall { name, args }));
    }
    unreachable!()
}

fn parse_agg_func_name(name: &str) -> Option<AggFunc> {
    if name.eq_ignore_ascii_case("COUNT") {
        Some(AggFunc::Count)
    } else if name.eq_ignore_ascii_case("SUM") {
        Some(AggFunc::Sum)
    } else if name.eq_ignore_ascii_case("AVG") {
        Some(AggFunc::Avg)
    } else if name.eq_ignore_ascii_case("MIN") {
        Some(AggFunc::Min)
    } else if name.eq_ignore_ascii_case("MAX") {
        Some(AggFunc::Max)
    } else if name.eq_ignore_ascii_case("COLLECT") || name.eq_ignore_ascii_case("COLLECT_LIST") {
        Some(AggFunc::Collect)
    } else {
        None
    }
}

fn parse_call_args_maybe_star<'a>(
    input: Tokens<'a>,
) -> TResult<'a, Option<(bool, bool, Vec<Expr>)>> {
    let (mut input, _) = punct(Token::LParen)(input)?;
    let mut distinct = false;
    if starts_kw(input, "DISTINCT") {
        let (i, _) = kw("DISTINCT")(input)?;
        input = i;
        distinct = true;
    }
    if matches!(input.first(), Some(Token::Star)) {
        let (input, _) = punct(Token::Star)(input)?;
        let (input, _) = punct(Token::RParen)(input)?;
        return Ok((input, Some((distinct, true, Vec::new()))));
    }
    let mut args = Vec::new();
    if matches!(input.first(), Some(Token::RParen)) {
        let (input, _) = punct(Token::RParen)(input)?;
        return Ok((input, Some((distinct, false, args))));
    }
    loop {
        let (i, arg) = parse_expr(input)?;
        args.push(arg);
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    let (input, _) = punct(Token::RParen)(input)?;
    Ok((input, Some((distinct, false, args))))
}

/// Promote an i64 integer literal to the smallest signed type that fits.
fn int_literal(v: i64) -> Value {
    if v >= i32::MIN as i64 && v <= i32::MAX as i64 {
        Value::Int32(v as i32)
    } else {
        Value::Int64(v)
    }
}

/// Parse a BigInt string (decimal, hex, octal, or binary) into the smallest
/// signed integer type that fits (Int128 or Int256).
fn big_int_literal(s: &str) -> Result<Value, GleaphError> {
    // Determine radix and digit portion
    let (radix, digits) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
    {
        (16u32, rest)
    } else if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        (8u32, rest)
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        (2u32, rest)
    } else {
        (10u32, s)
    };

    if radix == 10 {
        // Try i128 first, then i256
        if let Ok(v) = digits.parse::<i128>() {
            return Ok(Value::Int128(v));
        }
        if let Ok(v) = digits.parse::<ethnum::I256>() {
            return Ok(Value::Int256(gleaph_types::Int256(v)));
        }
    } else {
        if let Ok(v) = i128::from_str_radix(digits, radix) {
            return Ok(Value::Int128(v));
        }
        // For hex/oct/bin, parse as signed I256 directly
        if let Ok(v) = ethnum::I256::from_str_radix(digits, radix) {
            return Ok(Value::Int256(gleaph_types::Int256(v)));
        }
    }

    Err(GleaphError::ParseError(format!(
        "integer literal out of range: {s}"
    )))
}

fn parse_primary_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    match input.split_first() {
        Some((Token::Int(v), rest)) => Ok((rest, Expr::Literal(int_literal(*v)))),
        Some((Token::BigInt(s), rest)) => {
            let val = big_int_literal(s).map_err(|e| nom::Err::Failure(PError::Gleaph(e)))?;
            Ok((rest, Expr::Literal(val)))
        }
        Some((Token::Float(v), rest)) => Ok((rest, Expr::Literal(Value::Float64(*v)))),
        Some((Token::String(s), rest)) => Ok((rest, Expr::Literal(Value::Text(s.clone())))),
        Some((Token::Bytes(b), rest)) => Ok((rest, Expr::Literal(Value::Bytes(b.clone())))),
        Some((Token::Ident(id), rest)) if id.eq_ignore_ascii_case("TRUE") => {
            Ok((rest, Expr::Literal(Value::Bool(true))))
        }
        Some((Token::Ident(id), rest)) if id.eq_ignore_ascii_case("FALSE") => {
            Ok((rest, Expr::Literal(Value::Bool(false))))
        }
        Some((Token::Ident(id), rest)) if id.eq_ignore_ascii_case("NULL") => {
            Ok((rest, Expr::Literal(Value::Null)))
        }
        Some((Token::Ident(id), rest))
            if id.eq_ignore_ascii_case("DATE")
                && matches!(rest.first(), Some(Token::String(_))) =>
        {
            let Token::String(s) = &rest[0] else {
                unreachable!()
            };
            let days = crate::temporal::parse_date(s).ok_or_else(|| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "invalid DATE literal: '{s}'"
                ))))
            })?;
            Ok((&rest[1..], Expr::Literal(Value::Date(days))))
        }
        Some((Token::Ident(id), rest))
            if id.eq_ignore_ascii_case("TIME")
                && matches!(rest.first(), Some(Token::String(_))) =>
        {
            let Token::String(s) = &rest[0] else {
                unreachable!()
            };
            let nanos = crate::temporal::parse_time(s).ok_or_else(|| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "invalid TIME literal: '{s}'"
                ))))
            })?;
            Ok((&rest[1..], Expr::Literal(Value::Time(nanos))))
        }
        Some((Token::Ident(id), rest))
            if id.eq_ignore_ascii_case("DATETIME")
                && matches!(rest.first(), Some(Token::String(_))) =>
        {
            let Token::String(s) = &rest[0] else {
                unreachable!()
            };
            let (secs, sub) = crate::temporal::parse_datetime(s).ok_or_else(|| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "invalid DATETIME literal: '{s}'"
                ))))
            })?;
            Ok((&rest[1..], Expr::Literal(Value::DateTime(secs, sub))))
        }
        Some((Token::Ident(id), rest))
            if id.eq_ignore_ascii_case("DURATION")
                && matches!(rest.first(), Some(Token::String(_))) =>
        {
            let Token::String(s) = &rest[0] else {
                unreachable!()
            };
            let (months, nanos) = crate::temporal::parse_duration(s).ok_or_else(|| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                    "invalid DURATION literal: '{s}'"
                ))))
            })?;
            Ok((&rest[1..], Expr::Literal(Value::Duration(months, nanos))))
        }
        Some((Token::Ident(id), rest)) => Ok((rest, Expr::Variable(id.clone()))),
        // Backtick-quoted identifiers are always variable references, never keywords.
        Some((Token::QuotedIdent(id), rest)) => Ok((rest, Expr::Variable(id.clone()))),
        Some((Token::Param(name), rest)) => {
            // Check for `:: TYPE` annotation (GQL §21.3)
            if matches!(rest.first(), Some(Token::Colon))
                && matches!(rest.get(1), Some(Token::Colon))
                && !matches!(rest.get(2), Some(Token::Colon))
            {
                let rest = &rest[2..];
                let (rest, type_ident) = ident(rest).map_err(|_| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        "expected type name after $param ::".into(),
                    )))
                })?;
                let (rest, vt) = parse_param_type_from_ident(rest, &type_ident)?;
                // Parse optional `| TYPE` union extensions.
                let mut types = vec![vt];
                let mut rest = rest;
                while matches!(rest.first(), Some(Token::Pipe)) {
                    let after_pipe = &rest[1..];
                    let (r, ti) = ident(after_pipe).map_err(|_| {
                        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                            "expected type name after |".into(),
                        )))
                    })?;
                    let (r, vt2) = parse_param_type_from_ident(r, &ti)?;
                    types.push(vt2);
                    rest = r;
                }
                Ok((
                    rest,
                    Expr::Parameter {
                        name: name.clone(),
                        type_annotation: Some(types),
                    },
                ))
            } else {
                Ok((
                    rest,
                    Expr::Parameter {
                        name: name.clone(),
                        type_annotation: None,
                    },
                ))
            }
        }
        Some((Token::LParen, _)) => {
            let (input, _) = punct(Token::LParen)(input)?;
            let (input, expr) = parse_expr(input)?;
            let (input, _) = punct(Token::RParen)(input)?;
            Ok((input, expr))
        }
        Some((Token::LBracket, _)) => parse_list_literal_expr(input),
        Some((Token::LBrace, _)) => {
            // Record literal { key: value, ... }
            let (mut input, _) = punct(Token::LBrace)(input)?;
            let mut pairs = Vec::new();
            if !matches!(input.first(), Some(Token::RBrace)) {
                loop {
                    let (i, key) = ident(input)?;
                    let (i, _) = punct(Token::Colon)(i)?;
                    let (i, val) = parse_expr(i)?;
                    pairs.push((key, val));
                    input = i;
                    if matches!(input.first(), Some(Token::Comma)) {
                        let (i, _) = punct(Token::Comma)(input)?;
                        input = i;
                    } else {
                        break;
                    }
                }
            }
            let (input, _) = punct(Token::RBrace)(input)?;
            Ok((input, Expr::RecordLiteral(pairs)))
        }
        _ => fail(GleaphError::ParseError("expected expression".into())),
    }
}

fn parse_list_literal_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    let (input, _) = punct(Token::LBracket)(input)?;
    // Regular list literal
    let mut input = input;
    let mut items = Vec::new();
    if matches!(input.first(), Some(Token::RBracket)) {
        let (input, _) = punct(Token::RBracket)(input)?;
        return Ok((input, Expr::ListLiteral(items)));
    }
    loop {
        let (i, item) = parse_expr(input)?;
        items.push(item);
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    let (input, _) = punct(Token::RBracket)(input)?;
    Ok((input, Expr::ListLiteral(items)))
}

fn parse_return_clause<'a>(mut input: Tokens<'a>) -> TResult<'a, ReturnClause> {
    // Check for NO BINDINGS
    if starts_kw(input, "NO") {
        let (i, _) = kw("NO")(input)?;
        let (i, _) = kw("BINDINGS")(i)?;
        return Ok((
            i,
            ReturnClause {
                distinct: false,
                items: vec![],
                star: false,
                no_bindings: true,
                finish: false,
            },
        ));
    }
    // Check for FINISH
    if starts_kw(input, "FINISH") {
        let (i, _) = kw("FINISH")(input)?;
        return Ok((
            i,
            ReturnClause {
                distinct: false,
                items: vec![],
                star: false,
                no_bindings: false,
                finish: true,
            },
        ));
    }
    let mut distinct = false;
    if starts_kw(input, "DISTINCT") {
        let (i, _) = kw("DISTINCT")(input)?;
        input = i;
        distinct = true;
    }
    // `RETURN *` — return all bound variables.
    if matches!(input.first(), Some(Token::Star)) {
        let (i, _) = punct(Token::Star)(input)?;
        return Ok((
            i,
            ReturnClause {
                distinct,
                items: vec![],
                star: true,
                no_bindings: false,
                finish: false,
            },
        ));
    }
    let mut items = Vec::new();
    loop {
        let (i, expr) = parse_expr(input)?;
        let (i, alias) = if starts_kw(i, "AS") {
            let (i, _) = kw("AS")(i)?;
            let (i, alias) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            (i, Some(alias))
        } else {
            (i, None)
        };

        items.push(ReturnItem { expr, alias });
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    Ok((
        input,
        ReturnClause {
            distinct,
            items,
            star: false,
            no_bindings: false,
            finish: false,
        },
    ))
}

fn parse_set_clause<'a>(mut input: Tokens<'a>) -> TResult<'a, SetClause> {
    let mut items = Vec::new();
    loop {
        let (i, var) = ident(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected identifier".into(),
            )))
        })?;
        input = i;
        if matches!(input.first(), Some(Token::Colon)) {
            let (i, _) = punct(Token::Colon)(input)?;
            let (i, label) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            items.push(SetItem::Label { var, label });
            input = i;
        } else if matches!(
            (input.first(), input.get(1)),
            (Some(Token::Eq), Some(Token::LBrace))
        ) {
            // SET n = { key: expr, ... } — replace all properties
            let (i, _) = punct(Token::Eq)(input)?;
            let (i, _) = punct(Token::LBrace)(i)?;
            let mut properties = Vec::new();
            let mut i = i;
            if !matches!(i.first(), Some(Token::RBrace)) {
                loop {
                    let (i2, key) = ident(i).map_err(|_| {
                        nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                            "expected property name in SET all-properties".into(),
                        )))
                    })?;
                    let (i2, _) = punct(Token::Colon)(i2)?;
                    let (i2, value) = parse_expr(i2)?;
                    properties.push((key, value));
                    i = i2;
                    if matches!(i.first(), Some(Token::Comma)) {
                        let (i2, _) = punct(Token::Comma)(i)?;
                        i = i2;
                        continue;
                    }
                    break;
                }
            }
            let (i, _) = punct(Token::RBrace)(i)?;
            items.push(SetItem::AllProperties { var, properties });
            input = i;
        } else {
            let (i, _) = punct(Token::Dot)(input)?;
            let (i, property) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            let (i, _) = punct(Token::Eq)(i)?;
            let (i, value) = parse_expr(i)?;
            items.push(SetItem::Property {
                var,
                property,
                value,
            });
            input = i;
        }
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    Ok((input, SetClause { items }))
}

fn parse_remove_clause<'a>(mut input: Tokens<'a>) -> TResult<'a, RemoveClause> {
    let mut items = Vec::new();
    loop {
        let (i, var) = ident(input).map_err(|_| {
            nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                "expected identifier".into(),
            )))
        })?;
        input = i;
        if matches!(input.first(), Some(Token::Colon)) {
            let (i, _) = punct(Token::Colon)(input)?;
            let (i, label) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            items.push(RemoveItem::Label { var, label });
            input = i;
        } else {
            let (i, _) = punct(Token::Dot)(input)?;
            let (i, property) = ident(i).map_err(|_| {
                nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                    "expected identifier".into(),
                )))
            })?;
            items.push(RemoveItem::Property { var, property });
            input = i;
        }
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    Ok((input, RemoveClause { items }))
}

fn parse_order_by<'a>(mut input: Tokens<'a>) -> TResult<'a, OrderBy> {
    let mut items = Vec::new();
    loop {
        let (i, expr) = parse_expr(input)?;
        let (i, descending) = if starts_kw(i, "DESC") || starts_kw(i, "DESCENDING") {
            let (i, _) = if starts_kw(i, "DESCENDING") {
                kw("DESCENDING")(i)?
            } else {
                kw("DESC")(i)?
            };
            (i, true)
        } else if starts_kw(i, "ASC") || starts_kw(i, "ASCENDING") {
            let (i, _) = if starts_kw(i, "ASCENDING") {
                kw("ASCENDING")(i)?
            } else {
                kw("ASC")(i)?
            };
            (i, false)
        } else {
            (i, false)
        };
        let (i, nulls_first) = if starts_kw(i, "NULLS")
            && matches!(i.get(1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("FIRST"))
        {
            let (i, _) = kw("NULLS")(i)?;
            let (i, _) = kw("FIRST")(i)?;
            (i, Some(true))
        } else if starts_kw(i, "NULLS")
            && matches!(i.get(1), Some(Token::Ident(s)) if s.eq_ignore_ascii_case("LAST"))
        {
            let (i, _) = kw("NULLS")(i)?;
            let (i, _) = kw("LAST")(i)?;
            (i, Some(false))
        } else {
            (i, None)
        };
        items.push(OrderByItem {
            expr,
            descending,
            nulls_first,
        });
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    Ok((input, OrderBy { items }))
}

fn parse_expr_list<'a>(mut input: Tokens<'a>) -> TResult<'a, Vec<Expr>> {
    let mut items = Vec::new();
    loop {
        let (i, expr) = parse_expr(input)?;
        items.push(expr);
        input = i;
        if matches!(input.first(), Some(Token::Comma)) {
            let (i, _) = punct(Token::Comma)(input)?;
            input = i;
            continue;
        }
        break;
    }
    Ok((input, items))
}

fn parse_limit<'a>(input: Tokens<'a>) -> TResult<'a, Limit> {
    map(parse_u32_limit_value, Limit).parse(input)
}

fn parse_with_clause<'a>(mut input: Tokens<'a>) -> TResult<'a, WithClause> {
    let return_clause = {
        let (i, r) = parse_return_clause(input)?;
        input = i;
        r
    };
    let where_clause = if starts_kw(input, "WHERE") {
        let (i, _) = kw("WHERE")(input)?;
        let (i, w) = parse_expr(i)?;
        input = i;
        Some(w)
    } else {
        None
    };
    let order_by = if starts_kw(input, "ORDER") {
        let (i, _) = kw("ORDER")(input)?;
        let (i, _) = kw("BY")(i)?;
        let (i, o) = parse_order_by(i)?;
        input = i;
        Some(o)
    } else {
        None
    };
    let limit = if starts_kw(input, "LIMIT") {
        let (i, _) = kw("LIMIT")(input)?;
        let (i, l) = parse_limit(i)?;
        input = i;
        Some(l)
    } else {
        None
    };
    let offset = if starts_kw(input, "OFFSET") {
        let (i, _) = kw("OFFSET")(input)?;
        let (i, off) = parse_u32_limit_value(i)?;
        input = i;
        Some(off)
    } else {
        None
    };
    Ok((
        input,
        WithClause {
            items: return_clause.items,
            distinct: return_clause.distinct,
            star: return_clause.star,
            where_clause,
            order_by,
            limit,
            offset,
            match_clauses: Vec::new(),
            post_match_where: None,
        },
    ))
}

fn parse_value_expr<'a>(input: Tokens<'a>) -> TResult<'a, Expr> {
    match input.split_first() {
        Some((Token::Int(v), rest)) => Ok((rest, Expr::Literal(int_literal(*v)))),
        Some((Token::BigInt(s), rest)) => {
            let val = big_int_literal(s).map_err(|e| nom::Err::Failure(PError::Gleaph(e)))?;
            Ok((rest, Expr::Literal(val)))
        }
        Some((Token::Float(v), rest)) => Ok((rest, Expr::Literal(Value::Float64(*v)))),
        Some((Token::String(s), rest)) => Ok((rest, Expr::Literal(Value::Text(s.clone())))),
        Some((Token::Bytes(b), rest)) => Ok((rest, Expr::Literal(Value::Bytes(b.clone())))),
        Some((Token::Ident(id), rest)) => {
            if id.eq_ignore_ascii_case("TRUE") {
                return Ok((rest, Expr::Literal(Value::Bool(true))));
            }
            if id.eq_ignore_ascii_case("FALSE") {
                return Ok((rest, Expr::Literal(Value::Bool(false))));
            }
            if id.eq_ignore_ascii_case("NULL") {
                return Ok((rest, Expr::Literal(Value::Null)));
            }
            if id.eq_ignore_ascii_case("DATE") && matches!(rest.first(), Some(Token::String(_))) {
                let Token::String(s) = &rest[0] else {
                    unreachable!()
                };
                let days = crate::temporal::parse_date(s).ok_or_else(|| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                        "invalid DATE literal: '{s}'"
                    ))))
                })?;
                return Ok((&rest[1..], Expr::Literal(Value::Date(days))));
            }
            if id.eq_ignore_ascii_case("TIME") && matches!(rest.first(), Some(Token::String(_))) {
                let Token::String(s) = &rest[0] else {
                    unreachable!()
                };
                let nanos = crate::temporal::parse_time(s).ok_or_else(|| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                        "invalid TIME literal: '{s}'"
                    ))))
                })?;
                return Ok((&rest[1..], Expr::Literal(Value::Time(nanos))));
            }
            if id.eq_ignore_ascii_case("DATETIME") && matches!(rest.first(), Some(Token::String(_)))
            {
                let Token::String(s) = &rest[0] else {
                    unreachable!()
                };
                let (secs, sub) = crate::temporal::parse_datetime(s).ok_or_else(|| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                        "invalid DATETIME literal: '{s}'"
                    ))))
                })?;
                return Ok((&rest[1..], Expr::Literal(Value::DateTime(secs, sub))));
            }
            if id.eq_ignore_ascii_case("DURATION") && matches!(rest.first(), Some(Token::String(_)))
            {
                let Token::String(s) = &rest[0] else {
                    unreachable!()
                };
                let (months, nanos) = crate::temporal::parse_duration(s).ok_or_else(|| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(format!(
                        "invalid DURATION literal: '{s}'"
                    ))))
                })?;
                return Ok((&rest[1..], Expr::Literal(Value::Duration(months, nanos))));
            }
            if matches!(rest.first(), Some(Token::Dot)) {
                let (rest, _) = punct(Token::Dot)(rest)?;
                let (rest, property) = ident(rest).map_err(|_| {
                    nom::Err::Failure(PError::Gleaph(GleaphError::ParseError(
                        "expected identifier".into(),
                    )))
                })?;
                Ok((
                    rest,
                    Expr::PropertyAccess {
                        target: Box::new(Expr::Variable(id.clone())),
                        property,
                    },
                ))
            } else {
                Ok((rest, Expr::Variable(id.clone())))
            }
        }
        _ => fail(GleaphError::ParseError("expected expression".into())),
    }
}

fn nom_to_gleaph(err: nom::Err<PError<'_>>) -> GleaphError {
    match err {
        nom::Err::Incomplete(_) => GleaphError::ParseError("incomplete input".into()),
        nom::Err::Error(PError::Gleaph(e)) | nom::Err::Failure(PError::Gleaph(e)) => e,
        nom::Err::Error(PError::Nom { input, .. })
        | nom::Err::Failure(PError::Nom { input, .. }) => {
            if input.is_empty() {
                GleaphError::ParseError("unexpected end of input".into())
            } else {
                GleaphError::ParseError("unexpected token".into())
            }
        }
    }
}

/// Parses a GQL query string into a [`Statement`] AST node.
pub fn parse_statement(input: &str) -> Result<Statement, GleaphError> {
    let tokens = tokenize(input)?;
    parse_statement_from_tokens(&tokens)
}

/// Parses a pre-tokenized GQL input into a [`Statement`] AST node.
pub fn parse_statement_from_tokens(tokens: &[Token]) -> Result<Statement, GleaphError> {
    let (rest, stmt) = parse_statement_nom(tokens).map_err(nom_to_gleaph)?;
    if !rest.is_empty() {
        return Err(GleaphError::ParseError("unexpected trailing tokens".into()));
    }
    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AggFunc, Direction, Expr, PathLength, Statement};

    #[test]
    fn parses_match_where_return_order_limit() {
        let stmt = parse_statement(
            r#"MATCH (a:User)-[e:FOLLOWS]->(b:User) WHERE a.age >= 21 AND b.name <> 'x' RETURN a.id, b.name AS name ORDER BY b.name DESC LIMIT 10"#,
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert_eq!(q.match_clauses[0].pattern.elements.len(), 1);
        assert_eq!(q.match_clauses[0].pattern.start.labels, vec!["User"]);
        assert_eq!(
            q.match_clauses[0].pattern.chain(0).edge.direction,
            Direction::Outgoing
        );
        assert_eq!(q.limit, Some(Limit(10)));
        assert!(q.where_clause.is_some());
        assert_eq!(q.return_clause.items.len(), 2);
    }

    #[test]
    fn parses_multi_hop_match_delete() {
        let stmt =
            parse_statement("MATCH (a)-[:KNOWS]->(b)-[:WORKS_AT]->(c:Company) DELETE b").unwrap();
        let Statement::Delete(d) = stmt else {
            panic!("expected delete");
        };
        assert_eq!(d.match_clause.elements.len(), 2);
        assert!(!d.detach);
        assert_eq!(d.target_vars, vec!["b"]);
    }

    #[test]
    fn parses_detach_delete() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) DETACH DELETE a").unwrap();
        let Statement::Delete(d) = stmt else {
            panic!("expected delete");
        };
        assert!(d.detach);
    }

    #[test]
    fn parses_union_and_union_all() {
        let stmt =
            parse_statement("MATCH (a)-[:X]->(b) RETURN a UNION ALL MATCH (x)-[:X]->(y) RETURN x")
                .unwrap();
        match stmt {
            Statement::Compound {
                op: crate::ast::SetOp::UnionAll,
                ..
            } => {}
            _ => panic!("expected compound union all"),
        }
    }

    #[test]
    fn rejects_too_many_union_branches() {
        // Build a query with 17 UNION branches to exceed the new limit of 16
        let mut q = "MATCH (a)-[:X]->(b) RETURN a".to_string();
        for _ in 0..16 {
            q.push_str(" UNION MATCH (a)-[:X]->(b) RETURN a");
        }
        let err = parse_statement(&q).unwrap_err();
        assert!(err.to_string().contains("MAX_UNION_BRANCHES"));
    }

    #[test]
    fn parses_insert_node_and_edge() {
        let n = parse_statement(r#"INSERT (:User {id: 1, name: 'A'})"#).unwrap();
        let Statement::Create(ref cs) = n else {
            panic!("expected Create");
        };
        assert_eq!(cs.len(), 1);
        assert!(matches!(cs[0], CreateStmt::Node(_)));

        let e = parse_statement("INSERT (a:User)-[:KNOWS]->(b:User)").unwrap();
        let Statement::Create(ref cs) = e else {
            panic!("expected Create");
        };
        assert_eq!(cs.len(), 1);
        let CreateStmt::Edge(ref edge) = cs[0] else {
            panic!("expected edge create");
        };
        assert_eq!(edge.edge.label.as_deref(), Some("KNOWS"));
    }

    #[test]
    fn create_data_mutation_rejected() {
        let err = parse_statement(r#"CREATE (:User {id: 1})"#).unwrap_err();
        assert!(err.to_string().contains("Use INSERT"));
    }

    #[test]
    fn parses_aggregations_and_optional_match() {
        let stmt = parse_statement("OPTIONAL MATCH (a)-[:X]->(b) RETURN a").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(q.match_clauses.first().is_some_and(|m| m.optional));
        let stmt =
            parse_statement("MATCH (a)-[:X]->(b) RETURN COUNT(*), COUNT(DISTINCT b)").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(matches!(
            q.return_clause.items[0].expr,
            Expr::Aggregate(crate::ast::AggregateExpr {
                func: AggFunc::Count,
                count_all: true,
                ..
            })
        ));
    }

    #[test]
    fn parses_optional_match_after_match() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) OPTIONAL MATCH (b)-[:Y]->(c) RETURN a, c")
            .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert_eq!(q.match_clauses.len(), 2);
        assert!(!q.match_clauses[0].optional);
        assert!(q.match_clauses[1].optional);
    }

    #[test]
    fn parses_variable_length_edge_pattern() {
        let stmt = parse_statement("MATCH (a)-[:KNOWS*1..3]->(b) RETURN b").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query")
        };
        assert!(matches!(
            q.match_clauses[0].pattern.chain(0).edge.length,
            PathLength::Range { min: 1, max: 3 }
        ));
    }

    #[test]
    fn rejects_match_without_return_or_delete() {
        let err = parse_statement("MATCH (a)-[:X]->(b)").unwrap_err();
        assert!(matches!(err, GleaphError::ParseError(_)));
    }

    #[test]
    fn rejects_limit_over_u32_max() {
        let err = parse_statement("MATCH (a)-[:X]->(b) RETURN a LIMIT 5000000000").unwrap_err();
        assert!(matches!(err, GleaphError::ParseError(_)));
        assert!(err.to_string().contains("LIMIT exceeds"));
    }

    #[test]
    fn parses_literals_and_property_access() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) RETURN a, a.id, true, null").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(matches!(q.return_clause.items[0].expr, Expr::Variable(_)));
        assert!(matches!(
            q.return_clause.items[1].expr,
            Expr::PropertyAccess { .. }
        ));
    }

    #[test]
    fn parses_where_boolean_ops_and_precedence() {
        let stmt = parse_statement(
            r#"MATCH (a)-[:X]->(b) WHERE NOT a.age < 20 OR a.name IS NULL AND a.id IN [1, 2, 3] RETURN a"#,
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let where_expr = q.where_clause.expect("where");
        // Top-level OR should bind weaker than AND.
        assert!(matches!(where_expr, Expr::Or(_, _)));
    }

    #[test]
    fn parses_where_function_call_and_exists() {
        let stmt = parse_statement(
            r#"MATCH (a)-[:X]->(b) WHERE size(a.name) > 0 AND EXISTS { MATCH (x)-[:X]->(y) RETURN x } RETURN a"#,
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        let where_expr = q.where_clause.expect("where");
        assert!(matches!(where_expr, Expr::And(_, _)));
    }

    #[test]
    fn parses_group_by_and_having() {
        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) RETURN a.name, COUNT(*) GROUP BY a.name HAVING COUNT(*) > 0",
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(q.group_by.as_ref().is_some_and(|g| g.len() == 1));
        assert!(q.having.is_some());
    }

    #[test]
    fn parses_with_clauses() {
        let stmt = parse_statement(
            "MATCH (a)-[:KNOWS]->(b) WITH a, COUNT(*) AS c WHERE c > 0 RETURN a, c",
        )
        .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert_eq!(q.with_clauses.len(), 1);
        assert_eq!(q.with_clauses[0].items.len(), 2);
        assert!(q.with_clauses[0].where_clause.is_some());
    }

    #[test]
    fn parses_match_path_variable_binding() {
        let stmt = parse_statement("MATCH p = (a)-[:KNOWS]->(b) RETURN p, length(p)").unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert_eq!(q.match_clauses[0].path_variable.as_deref(), Some("p"));
    }

    #[test]
    fn parses_shortest_match_modifier() {
        let stmt = parse_statement("MATCH SHORTEST p = (a)-[:KNOWS*1..3]->(b) RETURN p, length(p)")
            .unwrap();
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(q.match_clauses[0].shortest);
    }

    #[test]
    fn parses_match_set_property_and_label() {
        let stmt =
            parse_statement("MATCH (a)-[:X]->(b) WHERE b.id = 1 SET b.age = 31, b:Member").unwrap();
        let Statement::Set(s) = stmt else {
            panic!("expected set");
        };
        assert_eq!(s.set_clause.items.len(), 2);
    }

    #[test]
    fn parses_match_remove_property_and_label() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) REMOVE b.age, b:Temporary").unwrap();
        let Statement::Remove(r) = stmt else {
            panic!("expected remove");
        };
        assert_eq!(r.remove_clause.items.len(), 2);
    }
}
