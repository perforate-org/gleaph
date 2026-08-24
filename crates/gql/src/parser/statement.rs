//! Statement-level parsers: program, composite query, linear query, and
//! individual statement parsers (MATCH, INSERT, SET, REMOVE, DELETE, etc.).

use crate::ast::{
    BindingTypeAnnotation, CallProcedureStatement, CompositeQueryExpr, DeleteDetach,
    DeleteStatement, FilterStatement, ForOrdinality, ForStatement, GqlProgram, GraphPattern,
    InlineProcedureCall, InlineProcedureScope, InsertStatement, IsOrColon, LetBinding,
    LetStatement, LinearQueryStatement, MatchStatement, NextStatement, ObjectName,
    ProcedureBindingDefinition, ProcedureBindingInitializer, ProcedureBindingKind, RemoveItem,
    RemoveStatement, ResultStatement, SchemaReference, SetItem, SetOp, SetQuantifier, SetStatement,
    SimpleQueryStatement, Statement, StatementBlock, TransactionActivity, TransactionEnd,
    TypedPrefix,
};
#[cfg(feature = "gleaph")]
use crate::ast::{
    Expr, ExprKind, GrantComparison, GrantCondition, GrantConditionSelector, GrantDirection,
    GrantPredicate, GrantPrivilege, GrantResourceSelector, GrantStatement, GrantSubjectLiteral,
    GrantTarget, GrantValueExpr, RevokeStatement, SearchOutputBinding, SearchOutputKind,
    SearchProvider, SearchStatement, VectorSearchSpec,
};
use crate::error::GqlError;
use crate::parser::helpers::Parser;
use crate::token::Token;

impl Parser<'_> {
    // ════════════════════════════════════════════════════════════════════════
    // §6 — Top-level program
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a complete GQL program (§6).
    ///
    /// ```text
    /// program := sessionCommand* transactionActivity? sessionCloseCommand?
    /// ```
    pub fn parse_program(&mut self) -> Result<GqlProgram, GqlError> {
        let start = self.save();
        let mut session_activity = Vec::new();

        // Collect leading SESSION SET / SESSION RESET commands.
        while self.at_keyword("SESSION") {
            // Distinguish SET/RESET from CLOSE by looking ahead.
            if self.at_keyword_ahead(1, "SET") || self.at_keyword_ahead(1, "RESET") {
                session_activity.push(self.parse_session_command()?);
            } else {
                break;
            }
        }

        // Optional transaction activity.
        let transaction_activity = if !self.at_end() {
            // Check for SESSION CLOSE (which is not part of transaction activity).
            if self.at_keyword("SESSION") && self.at_keyword_ahead(1, "CLOSE") {
                None
            } else {
                Some(self.parse_transaction_activity()?)
            }
        } else {
            None
        };

        // Optional trailing SESSION CLOSE.
        if self.at_keyword("SESSION") && self.at_keyword_ahead(1, "CLOSE") {
            self.expect_keyword("SESSION")?;
            self.expect_keyword("CLOSE")?;
            session_activity.push(crate::ast::SessionCommand::Close);
        }

        Ok(GqlProgram {
            span: self.span_since(start),
            session_activity,
            transaction_activity,
            #[cfg(feature = "gleaph")]
            doc_comments: Vec::new(),
        })
    }

    /// Parses a transaction activity: optional START TRANSACTION, a statement
    /// block (with NEXT chaining), and optional COMMIT/ROLLBACK.
    fn parse_transaction_activity(&mut self) -> Result<TransactionActivity, GqlError> {
        let save = self.save();
        // Optional START TRANSACTION.
        let start = if self.at_keyword("START") && self.at_keyword_ahead(1, "TRANSACTION") {
            Some(self.parse_start_transaction()?)
        } else {
            None
        };

        // Parse the optional statement block (first statement + NEXT chained statements).
        // The body is absent for bare transaction commands like `START TRANSACTION READ WRITE`,
        // `COMMIT`, or `ROLLBACK`.
        let body = if !self.at_end() && !self.at_keyword("COMMIT") && !self.at_keyword("ROLLBACK") {
            Some(self.parse_statement_block()?)
        } else {
            None
        };

        // Optional end-transaction command.
        let end = if self.eat_keyword("COMMIT") {
            Some(TransactionEnd::Commit)
        } else if self.eat_keyword("ROLLBACK") {
            Some(TransactionEnd::Rollback)
        } else {
            None
        };

        Ok(TransactionActivity {
            span: self.span_since(save),
            start,
            body,
            end,
        })
    }

    /// Parses a statement block: `statement (NEXT [YIELD items] statement)*`
    /// (GQL `statementBlock`).
    fn parse_statement_block(&mut self) -> Result<StatementBlock, GqlError> {
        let start = self.save();
        let first = self.parse_statement()?;
        let mut next = Vec::new();

        while self.eat_keyword("NEXT") {
            let next_start = self.save();
            let yield_items = if self.eat_keyword("YIELD") {
                Some(self.parse_yield_clause()?)
            } else {
                None
            };
            let statement = self.parse_statement()?;
            next.push(NextStatement {
                span: self.span_since(next_start),
                yield_items,
                statement,
            });
        }

        Ok(StatementBlock {
            span: self.span_since(start),
            first,
            next,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.1 — Statement dispatch
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a single statement, dispatching based on the leading keyword.
    pub fn parse_statement(&mut self) -> Result<Statement, GqlError> {
        // Catalog / DDL.
        if self.at_keyword("CREATE") || self.at_keyword("DROP") {
            return self.parse_catalog_statement();
        }

        // Session commands appearing as statements.
        if self.at_keyword("SESSION") {
            let cmd = self.parse_session_command()?;
            return Ok(Statement::Session(cmd));
        }

        // Data modification statements.
        if self.at_keyword("INSERT") {
            return Ok(Statement::Insert(self.parse_insert_statement()?));
        }
        if self.at_keyword("SET") {
            return Ok(Statement::Set(self.parse_set_statement()?));
        }
        if self.at_keyword("REMOVE") {
            return Ok(Statement::Remove(self.parse_remove_statement()?));
        }
        if self.at_keyword("DELETE") || self.at_keyword("DETACH") || self.at_keyword("NODETACH") {
            return Ok(Statement::Delete(self.parse_delete_statement()?));
        }

        // Otherwise it is a query statement (composite query expression).
        // This includes CALL / OPTIONAL CALL, MATCH / OPTIONAL MATCH, etc.
        // which are parsed as simple query statement parts of a linear query.
        let cqe = self.parse_composite_query_expr()?;
        Ok(Statement::Query(Box::new(cqe)))
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.2 — Composite query expression
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a composite query expression: linear queries joined by
    /// UNION / EXCEPT / INTERSECT / OTHERWISE.
    pub fn parse_composite_query_expr(&mut self) -> Result<CompositeQueryExpr, GqlError> {
        self.recurse(Self::parse_composite_query_expr_inner)
    }

    fn parse_composite_query_expr_inner(&mut self) -> Result<CompositeQueryExpr, GqlError> {
        let start = self.save();
        let (at_schema, prefix_bindings) = self.parse_procedure_prefix()?;
        let mut left = self.parse_linear_query()?;
        left.at_schema = at_schema;
        left.prefix_bindings = prefix_bindings;
        let mut rest = Vec::new();

        loop {
            let op = if self.eat_keyword("UNION") {
                if self.eat_keyword("ALL") {
                    SetOp::UnionAll
                } else if self.eat_keyword("DISTINCT") {
                    SetOp::UnionDistinct
                } else {
                    SetOp::Union
                }
            } else if self.eat_keyword("EXCEPT") {
                if self.eat_keyword("ALL") {
                    SetOp::ExceptAll
                } else if self.eat_keyword("DISTINCT") {
                    SetOp::ExceptDistinct
                } else {
                    SetOp::Except
                }
            } else if self.eat_keyword("INTERSECT") {
                if self.eat_keyword("ALL") {
                    SetOp::IntersectAll
                } else if self.eat_keyword("DISTINCT") {
                    SetOp::IntersectDistinct
                } else {
                    SetOp::Intersect
                }
            } else if self.eat_keyword("OTHERWISE") {
                SetOp::Otherwise
            } else {
                break;
            };

            let right = self.parse_linear_query()?;
            rest.push((op, right));
        }

        Ok(CompositeQueryExpr {
            span: self.span_since(start),
            left,
            rest,
        })
    }

    /// Consumes the procedure-body prefix that can appear before the first
    /// statement: `AT <schema>` and a block of binding-variable definitions.
    fn parse_procedure_prefix(
        &mut self,
    ) -> Result<(Option<SchemaReference>, Vec<ProcedureBindingDefinition>), GqlError> {
        let at_schema = if self.eat_keyword("AT") {
            Some(self.parse_schema_reference()?)
        } else {
            None
        };

        let mut bindings = Vec::new();
        while self.at_keyword("VALUE")
            || self.at_keyword("GRAPH")
            || self.at_keyword("PROPERTY")
            || self.at_keyword("TABLE")
            || (self.at_keyword("BINDING") && self.at_keyword_ahead(1, "TABLE"))
        {
            bindings.push(self.parse_binding_variable_definition()?);
        }

        Ok((at_schema, bindings))
    }

    fn parse_schema_reference(&mut self) -> Result<SchemaReference, GqlError> {
        if let Some(Token::SubstitutedParam(name)) = self.peek().cloned() {
            self.advance();
            return Ok(SchemaReference::Parameter(name));
        }

        if self.at_keyword("HOME_SCHEMA") {
            self.advance();
            return Ok(SchemaReference::Current("HOME_SCHEMA".to_string()));
        }
        if self.at_keyword("CURRENT_SCHEMA") {
            self.advance();
            return Ok(SchemaReference::Current("CURRENT_SCHEMA".to_string()));
        }
        if self.eat_token(&Token::Dot) {
            return Ok(SchemaReference::Current(".".to_string()));
        }

        if self.eat_token(&Token::Slash) {
            let segments = self.consume_schema_path_segments()?;
            return Ok(SchemaReference::Absolute(segments));
        }

        if self.eat_token(&Token::RangeDots) {
            let mut segments = vec!["..".to_string()];
            while self.eat_token(&Token::Slash) {
                if self.eat_token(&Token::RangeDots) {
                    segments.push("..".to_string());
                    continue;
                }
                segments.push(self.expect_ident()?);
            }
            return Ok(SchemaReference::Relative(segments));
        }

        Err(self.expected("schema reference"))
    }

    fn consume_schema_path_segments(&mut self) -> Result<Vec<String>, GqlError> {
        let mut segments = Vec::new();
        if !self.at_ident() {
            return Ok(segments);
        }

        segments.push(self.expect_ident()?);
        while self.eat_token(&Token::Slash) {
            segments.push(self.expect_ident()?);
        }
        Ok(segments)
    }

    fn parse_binding_variable_definition(
        &mut self,
    ) -> Result<ProcedureBindingDefinition, GqlError> {
        let start = self.save();
        let kind = if self.eat_keyword("PROPERTY") {
            self.expect_keyword("GRAPH")?;
            ProcedureBindingKind::Graph
        } else if self.eat_keyword("GRAPH") {
            ProcedureBindingKind::Graph
        } else if self.eat_keyword("BINDING") {
            self.expect_keyword("TABLE")?;
            ProcedureBindingKind::Table
        } else if self.eat_keyword("TABLE") {
            ProcedureBindingKind::Table
        } else if self.eat_keyword("VALUE") {
            ProcedureBindingKind::Value
        } else {
            return Err(self.expected("binding variable definition"));
        };

        let variable = self.expect_ident()?;

        // Parse optional type annotation: `[TYPED | ::] <type>`.
        let (typed_prefix, type_annotation) = self.parse_binding_type_annotation(&kind)?;

        self.expect_token(&Token::Eq)?;
        let initializer = if matches!(
            kind,
            ProcedureBindingKind::Graph | ProcedureBindingKind::Table
        ) {
            match self.peek() {
                Some(Token::LBrace) if matches!(kind, ProcedureBindingKind::Table) => {
                    ProcedureBindingInitializer::Query(Box::new(self.parse_nested_query_block()?))
                }
                Some(Token::SubstitutedParam(_)) | Some(Token::Slash) | Some(Token::Ident(_)) => {
                    ProcedureBindingInitializer::Object(self.parse_object_name()?)
                }
                _ => ProcedureBindingInitializer::Expr(self.parse_expr()?),
            }
        } else {
            ProcedureBindingInitializer::Expr(self.parse_expr()?)
        };
        Ok(ProcedureBindingDefinition {
            span: self.span_since(start),
            kind,
            variable,
            typed_prefix,
            type_annotation,
            initializer,
        })
    }

    /// Parses the optional type annotation between a binding variable name and
    /// the `=` initializer.  Returns `None` when the next token is already `=`.
    ///
    /// Grammar: `(typed? <referenceValueType>)?` where `typed` is `::` or
    /// `TYPED`.
    pub(crate) fn parse_binding_type_annotation(
        &mut self,
        kind: &ProcedureBindingKind,
    ) -> Result<(TypedPrefix, Option<BindingTypeAnnotation>), GqlError> {
        // If the next token is `=`, there's no type annotation.
        if self.at_token(&Token::Eq) {
            return Ok((TypedPrefix::None, None));
        }

        // Consume optional `typed` prefix (`::` or `TYPED`).
        let typed_prefix = if self.eat_token(&Token::DoubleColon) {
            TypedPrefix::DoubleColon
        } else if self.eat_keyword("TYPED") {
            TypedPrefix::Typed
        } else {
            TypedPrefix::None
        };

        match kind {
            ProcedureBindingKind::Graph => {
                // ANY [PROPERTY] GRAPH [NOT NULL]  or
                // [PROPERTY] GRAPH <nestedGraphTypeSpec> [NOT NULL]
                if self.eat_keyword("ANY") {
                    let property_keyword = self.eat_keyword("PROPERTY");
                    let graph_keyword = self.eat_keyword("GRAPH");
                    let not_null = self.eat_not_null();
                    Ok((
                        typed_prefix,
                        Some(BindingTypeAnnotation::AnyGraph {
                            property_keyword,
                            graph_keyword,
                            not_null,
                        }),
                    ))
                } else {
                    // [PROPERTY] GRAPH <graphTypeRef> [NOT NULL]
                    let property_keyword = self.eat_keyword("PROPERTY");
                    let graph_keyword = self.eat_keyword("GRAPH");
                    if self.at_token(&Token::Eq) {
                        // No actual type, just the `typed` prefix alone.
                        return Ok((typed_prefix, None));
                    }
                    let graph_type = self.parse_object_name()?;
                    let not_null = self.eat_not_null();
                    Ok((
                        typed_prefix,
                        Some(BindingTypeAnnotation::ClosedGraph {
                            property_keyword,
                            graph_keyword,
                            graph_type,
                            not_null,
                        }),
                    ))
                }
            }
            ProcedureBindingKind::Table => {
                // [BINDING] TABLE <fieldTypesSpec> [NOT NULL]
                let binding_keyword = self.eat_keyword("BINDING");
                let table_keyword = self.eat_keyword("TABLE");
                let not_null = self.eat_not_null();
                Ok((
                    typed_prefix,
                    Some(BindingTypeAnnotation::BindingTable {
                        binding_keyword,
                        table_keyword,
                        not_null,
                    }),
                ))
            }
            ProcedureBindingKind::Value => {
                // A general value type (INT32, STRING, etc.)
                let vt = self.parse_value_type()?;
                Ok((typed_prefix, Some(BindingTypeAnnotation::Value(vt))))
            }
        }
    }

    fn parse_nested_query_block(&mut self) -> Result<CompositeQueryExpr, GqlError> {
        self.expect_token(&Token::LBrace)?;
        let query = self.parse_composite_query_expr()?;
        self.expect_token(&Token::RBrace)?;
        Ok(query)
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.3 — Linear query statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a linear query: a sequence of simple query statements ending
    /// with a primitive result statement (RETURN / SELECT / FINISH).
    pub fn parse_linear_query(&mut self) -> Result<LinearQueryStatement, GqlError> {
        let start = self.save();
        let mut parts = Vec::new();

        loop {
            // USE <graph> — focused statement (GQL §14).
            // Must be checked before RETURN/SELECT so we can wrap the result
            // in a Focused part or InlineProcedureCall with use_graph.
            if self.at_keyword("USE") {
                let focused = self.parse_use_graph_focused()?;
                parts.push(focused);
                // After a focused nested block (USE g { ... }) we are done.
                if matches!(
                    parts.last(),
                    Some(SimpleQueryStatement::InlineProcedureCall(_))
                ) {
                    return Ok(LinearQueryStatement {
                        span: self.span_since(start),
                        at_schema: None,
                        prefix_bindings: vec![],
                        parts,
                        result: None,
                    });
                }
                continue;
            }

            if self.at_keyword("SELECT") {
                return self.parse_select_statement_as_linear_query(parts);
            }

            // Primitive result: RETURN or FINISH.
            if self.at_keyword("RETURN") || self.at_keyword("FINISH") {
                let result = self.parse_return_or_finish()?;
                return Ok(LinearQueryStatement {
                    span: self.span_since(start),
                    at_schema: None,
                    prefix_bindings: vec![],
                    parts,
                    result: Some(result),
                });
            }

            // Simple query statements.
            if let Some(sq) = self.try_parse_simple_query_statement()? {
                parts.push(sq);
                if self.at_token(&Token::LBrace) {
                    self.parse_nested_query_block()?;
                    return Ok(LinearQueryStatement {
                        span: self.span_since(start),
                        at_schema: None,
                        prefix_bindings: vec![],
                        parts,
                        result: None,
                    });
                }
            } else {
                // Nothing matched — end of linear query.
                break;
            }
        }

        // If we parsed nothing at all, report an error rather than
        // returning an empty linear query (which would loop the caller).
        if parts.is_empty() {
            return Err(self.expected("statement"));
        }

        Ok(LinearQueryStatement {
            span: self.span_since(start),
            at_schema: None,
            prefix_bindings: vec![],
            parts,
            result: None,
        })
    }

    /// Parses `USE <graph>` followed by a simple query statement, a braced
    /// nested procedure body, or nothing (when USE precedes RETURN/SELECT).
    ///
    /// Returns a `SimpleQueryStatement::Focused` or
    /// `SimpleQueryStatement::InlineProcedureCall` (with `use_graph` set).
    fn parse_use_graph_focused(&mut self) -> Result<SimpleQueryStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("USE")?;
        let graph = self.parse_object_name()?;

        // focusedNestedDataModifyingProcedureSpecification:
        // USE <graph> { <body> }
        if self.at_token(&Token::LBrace) {
            self.expect_token(&Token::LBrace)?;
            let body = self.parse_composite_query_expr()?;
            self.expect_token(&Token::RBrace)?;
            return Ok(SimpleQueryStatement::InlineProcedureCall(
                InlineProcedureCall {
                    span: self.span_since(start),
                    optional: false,
                    use_graph: Some(graph),
                    scope: InlineProcedureScope::ImplicitAll,
                    body: Box::new(body),
                },
            ));
        }

        // focusedLinearQueryStatementPart /
        // focusedLinearDataModifyingStatementBody:
        // USE <graph> <simpleStatement>
        if let Some(inner) = self.try_parse_simple_query_statement()? {
            return Ok(SimpleQueryStatement::Focused {
                graph,
                body: Some(Box::new(inner)),
            });
        }

        // focusedPrimitiveResultStatement:
        // USE <graph> before RETURN/SELECT/FINISH — the graph scope applies
        // to the result statement.  Body is None; the caller handles the result.
        Ok(SimpleQueryStatement::Focused { graph, body: None })
    }

    fn parse_select_statement_as_linear_query(
        &mut self,
        parts: Vec<SimpleQueryStatement>,
    ) -> Result<LinearQueryStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("SELECT")?;

        let set_quantifier = if self.eat_keyword("DISTINCT") {
            SetQuantifier::Distinct
        } else if self.eat_keyword("ALL") {
            SetQuantifier::All
        } else {
            SetQuantifier::None
        };

        let is_star = self.eat_token(&Token::Star);
        let items = if is_star {
            vec![]
        } else {
            self.comma_list(Self::parse_return_item)?
        };

        let source = if self.eat_keyword("FROM") {
            Some(self.parse_select_source()?)
        } else {
            None
        };

        let group_by = if self.at_keyword("GROUP") && self.at_keyword_ahead(1, "BY") {
            self.advance();
            self.advance();
            Some(self.parse_group_by_clause()?)
        } else {
            None
        };

        let having = if self.eat_keyword("HAVING") {
            Some(self.parse_having_clause()?)
        } else {
            None
        };

        let (order_by, offset, limit) = self.parse_order_by_and_page()?;

        let body = if is_star {
            crate::ast::SelectBody::Star {
                group_by,
                having,
                order_by,
                limit,
                offset,
            }
        } else {
            crate::ast::SelectBody::Items {
                items,
                group_by,
                having,
                order_by,
                limit,
                offset,
            }
        };

        Ok(LinearQueryStatement {
            span: self.span_since(start),
            at_schema: None,
            prefix_bindings: vec![],
            parts,
            result: Some(ResultStatement::Select(Box::new(
                crate::ast::SelectStatement {
                    span: self.span_since(start),
                    set_quantifier,
                    source,
                    body,
                },
            ))),
        })
    }

    fn parse_select_source(&mut self) -> Result<crate::ast::SelectSource, GqlError> {
        if self.at_token(&Token::LBrace) {
            let query = self.parse_nested_query_block()?;
            return Ok(crate::ast::SelectSource::QuerySpecification(
                crate::ast::SelectQuerySpecification::Nested(Box::new(query)),
            ));
        }

        let first_graph = self.parse_object_name()?;
        if self.at_token(&Token::LBrace) {
            let query = self.parse_nested_query_block()?;
            return Ok(crate::ast::SelectSource::QuerySpecification(
                crate::ast::SelectQuerySpecification::GraphNested {
                    graph: first_graph,
                    query: Box::new(query),
                },
            ));
        }

        let first_match = self.parse_select_graph_match_statement()?;

        let mut matches = vec![crate::ast::SelectGraphMatch {
            graph: first_graph,
            match_statement: first_match,
        }];

        while self.eat_token(&Token::Comma) {
            let graph = self.parse_object_name()?;
            let match_statement = self.parse_select_graph_match_statement()?;
            matches.push(crate::ast::SelectGraphMatch {
                graph,
                match_statement,
            });
        }

        Ok(crate::ast::SelectSource::GraphMatchList(matches))
    }

    fn parse_select_graph_match_statement(&mut self) -> Result<MatchStatement, GqlError> {
        let start = self.save();
        let optional = self.eat_keyword("OPTIONAL");
        self.expect_keyword("MATCH")?;
        let path = self.parse_path_pattern()?;
        let where_clause = if self.eat_keyword("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(MatchStatement {
            span: self.span_since(start),
            optional,
            graph_name: None,
            pattern: GraphPattern {
                span: self.span_since(start),
                match_mode: None,
                paths: vec![path],
                keep: None,
                where_clause,
            },
            yield_items: None,
        })
    }

    /// Tries to parse a simple query statement. Returns `None` if the current
    /// token does not begin a recognised simple query statement.
    fn try_parse_simple_query_statement(
        &mut self,
    ) -> Result<Option<SimpleQueryStatement>, GqlError> {
        // USE GRAPH — handled by parse_linear_query which wraps the
        // subsequent statement in SimpleQueryStatement::Focused or sets
        // use_graph on InlineProcedureCall.  We should not see USE here
        // because parse_linear_query intercepts it first.
        // If we do arrive here, fall through to the next check.
        if self.at_keyword("USE") {
            return Ok(None);
        }

        // MATCH / OPTIONAL MATCH.
        if self.at_keyword("MATCH")
            || (self.at_keyword("OPTIONAL") && self.at_keyword_ahead(1, "MATCH"))
        {
            return Ok(Some(SimpleQueryStatement::Match(
                self.parse_match_statement()?,
            )));
        }

        // FILTER.
        if self.at_keyword("FILTER") {
            return Ok(Some(SimpleQueryStatement::Filter(
                self.parse_filter_statement()?,
            )));
        }

        // LET.
        if self.at_keyword("LET") {
            return Ok(Some(SimpleQueryStatement::Let(self.parse_let_statement()?)));
        }

        // FOR.
        if self.at_keyword("FOR") {
            return Ok(Some(SimpleQueryStatement::For(self.parse_for_statement()?)));
        }

        // ORDER BY (as a standalone statement).
        if self.at_keyword("ORDER") && self.at_keyword_ahead(1, "BY") {
            self.advance(); // ORDER
            self.advance(); // BY
            let order_by = self.parse_order_by_clause()?;
            return Ok(Some(SimpleQueryStatement::OrderBy(order_by)));
        }

        // OFFSET / SKIP (standalone).
        if self.at_keyword("OFFSET") || self.at_keyword("SKIP") {
            let skip_keyword = self.at_keyword("SKIP");
            self.advance();
            let offset = self.parse_offset_clause(skip_keyword)?;
            return Ok(Some(SimpleQueryStatement::Offset(offset)));
        }

        // LIMIT (standalone).
        if self.at_keyword("LIMIT") {
            self.advance();
            let limit = self.parse_limit_clause()?;
            return Ok(Some(SimpleQueryStatement::Limit(limit)));
        }

        // CALL / OPTIONAL CALL (as query statement).
        if self.at_keyword("CALL")
            || (self.at_keyword("OPTIONAL") && self.at_keyword_ahead(1, "CALL"))
        {
            // Check for inline procedure call: CALL { ... } or CALL (vars) { ... }.
            let is_inline = if self.at_keyword("OPTIONAL") {
                matches!(self.peek_ahead(2), Some(Token::LBrace))
                    || matches!(self.peek_ahead(2), Some(Token::LParen))
            } else {
                matches!(self.peek_ahead(1), Some(Token::LBrace))
                    || matches!(self.peek_ahead(1), Some(Token::LParen))
            };
            if is_inline {
                return Ok(Some(SimpleQueryStatement::InlineProcedureCall(
                    self.parse_inline_procedure_call()?,
                )));
            }
            return Ok(Some(SimpleQueryStatement::CallProcedure(
                self.parse_call_procedure_statement()?,
            )));
        }

        // SEARCH.
        #[cfg(feature = "gleaph")]
        if self.at_keyword("SEARCH") {
            return Ok(Some(SimpleQueryStatement::Search(
                self.parse_search_statement()?,
            )));
        }

        // GRANT / REVOKE (Gleaph extension, ADR 0074 §5).
        #[cfg(feature = "gleaph")]
        if self.at_keyword("GRANT") {
            return Ok(Some(SimpleQueryStatement::Grant(
                self.parse_grant_statement()?,
            )));
        }
        #[cfg(feature = "gleaph")]
        if self.at_keyword("REVOKE") {
            return Ok(Some(SimpleQueryStatement::Revoke(
                self.parse_revoke_statement()?,
            )));
        }

        // Inline data modification inside a linear query.
        if self.at_keyword("INSERT") {
            return Ok(Some(SimpleQueryStatement::Insert(
                self.parse_insert_statement()?,
            )));
        }
        if self.at_keyword("SET") {
            return Ok(Some(SimpleQueryStatement::Set(self.parse_set_statement()?)));
        }
        if self.at_keyword("REMOVE") {
            return Ok(Some(SimpleQueryStatement::Remove(
                self.parse_remove_statement()?,
            )));
        }
        if self.at_keyword("DELETE") || self.at_keyword("DETACH") || self.at_keyword("NODETACH") {
            return Ok(Some(SimpleQueryStatement::Delete(
                self.parse_delete_statement()?,
            )));
        }

        Ok(None)
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.4 — MATCH statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a MATCH statement: `[OPTIONAL] MATCH graphPattern`.
    pub fn parse_match_statement(&mut self) -> Result<MatchStatement, GqlError> {
        let start = self.save();
        let optional = self.eat_keyword("OPTIONAL");

        self.expect_keyword("MATCH")?;

        // Optional ON <graphName> — cypher extension (GQL standard uses USE GRAPH).
        #[cfg(feature = "cypher")]
        let graph_name = if self.eat_keyword("ON") {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        #[cfg(not(feature = "cypher"))]
        let graph_name = None;

        let pattern = self.parse_graph_pattern()?;
        let yield_items = if self.eat_keyword("YIELD") {
            Some(self.parse_yield_clause()?)
        } else {
            None
        };

        Ok(MatchStatement {
            span: self.span_since(start),
            optional,
            graph_name,
            pattern,
            yield_items,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // SEARCH statement (Gleaph extension)
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a `SEARCH` clause.
    ///
    /// Grammar:
    /// ```text
    /// SEARCH bindingVariable IN (
    ///   VECTOR INDEX objectName
    ///   FOR expr
    ///   [WHERE expr]
    ///   LIMIT expr
    /// ) (SCORE | DISTANCE) AS bindingVariable
    /// ```
    #[cfg(feature = "gleaph")]
    pub fn parse_search_statement(&mut self) -> Result<SearchStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("SEARCH")?;
        let binding = self.expect_ident()?.to_owned();
        self.expect_keyword("IN")?;
        self.expect_token(&Token::LParen)?;
        let provider = self.parse_search_provider()?;
        self.expect_token(&Token::RParen)?;
        let output = self.parse_search_output_binding()?;
        Ok(SearchStatement {
            span: self.span_since(start),
            binding,
            provider,
            output,
        })
    }

    #[cfg(feature = "gleaph")]
    fn parse_search_provider(&mut self) -> Result<SearchProvider, GqlError> {
        self.expect_keyword("VECTOR")?;
        self.expect_keyword("INDEX")?;
        let index_name = self.parse_object_name()?;
        self.expect_keyword("FOR")?;
        let query = self.parse_expr()?;
        let filter = if self.eat_keyword("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_keyword("LIMIT")?;
        let limit = self.parse_expr()?;
        Ok(SearchProvider::VectorIndex(VectorSearchSpec {
            index_name,
            query,
            limit,
            filter,
        }))
    }

    #[cfg(feature = "gleaph")]
    fn parse_search_output_binding(&mut self) -> Result<SearchOutputBinding, GqlError> {
        let kind = if self.eat_keyword("SCORE") {
            SearchOutputKind::Score
        } else if self.eat_keyword("DISTANCE") {
            SearchOutputKind::Distance
        } else {
            return Err(self.expected("SCORE AS or DISTANCE AS"));
        };
        self.expect_keyword("AS")?;
        let alias = self.expect_ident()?.to_owned();
        Ok(SearchOutputBinding { kind, alias })
    }

    // ════════════════════════════════════════════════════════════════════════
    // GRANT / REVOKE statements (Gleaph extension, ADR 0074 §5)
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a `GRANT` statement.
    ///
    /// Grammar:
    /// ```text
    /// GRANT privilege ON GRAPH objectName resourceSelector [condition] TO subject
    /// GRANT EXECUTE ON PREPARED QUERY ident TO subject
    /// privilege      := MATCH | TRAVERSE [OUTGOING | INCOMING] | READ | CREATE | UPDATE | DELETE
    /// resourceSelector := (NODES | VERTICES) ident [ "{" ident ("," ident)* "}" ]
    ///                   | EDGES ident
    /// condition      := FOR patternSelector WHERE predicate   (ADR 0075 §3)
    /// subject        := PRINCIPAL stringLiteral | PUBLIC
    /// ```
    ///
    /// The property list is only valid together with the `READ` privilege and only on
    /// a vertex selector; it lowers to per-property `READ_PROPERTY` rows. Both `NODES`
    /// and `VERTICES` are accepted; the canonical form is vertex. The two targets are one
    /// discriminated union ([`GrantTarget`]), so a graph privilege can never pair with a
    /// prepared-query resource or vice versa.
    #[cfg(feature = "gleaph")]
    pub fn parse_grant_statement(&mut self) -> Result<GrantStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("GRANT")?;
        if self.eat_keyword("EXECUTE") {
            self.expect_keyword("ON")?;
            self.expect_keyword("PREPARED")?;
            self.expect_keyword("QUERY")?;
            let name = self.expect_ident()?.to_owned();
            self.expect_keyword("TO")?;
            let subject = self.parse_grant_subject_literal()?;
            return Ok(GrantStatement {
                span: self.span_since(start),
                target: GrantTarget::PreparedQuery { name },
                subject,
            });
        }
        let mut privilege = self.parse_grant_privilege()?;
        self.expect_keyword("ON")?;
        self.expect_keyword("GRAPH")?;
        let graph = self.parse_object_name()?;
        let resource = self.parse_grant_resource_selector(&mut privilege)?;
        let condition = self.parse_grant_condition_opt()?;
        self.expect_keyword("TO")?;
        let subject = self.parse_grant_subject_literal()?;
        Ok(GrantStatement {
            span: self.span_since(start),
            target: GrantTarget::Graph {
                privilege,
                graph,
                resource,
                condition,
            },
            subject,
        })
    }

    /// Parses a `REVOKE` statement — the exact-key inverse of [`Self::parse_grant_statement`].
    #[cfg(feature = "gleaph")]
    pub fn parse_revoke_statement(&mut self) -> Result<RevokeStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("REVOKE")?;
        if self.eat_keyword("EXECUTE") {
            self.expect_keyword("ON")?;
            self.expect_keyword("PREPARED")?;
            self.expect_keyword("QUERY")?;
            let name = self.expect_ident()?.to_owned();
            self.expect_keyword("FROM")?;
            let subject = self.parse_grant_subject_literal()?;
            return Ok(RevokeStatement {
                span: self.span_since(start),
                target: GrantTarget::PreparedQuery { name },
                subject,
            });
        }
        let mut privilege = self.parse_grant_privilege()?;
        self.expect_keyword("ON")?;
        self.expect_keyword("GRAPH")?;
        let graph = self.parse_object_name()?;
        let resource = self.parse_grant_resource_selector(&mut privilege)?;
        let condition = self.parse_grant_condition_opt()?;
        self.expect_keyword("FROM")?;
        let subject = self.parse_grant_subject_literal()?;
        Ok(RevokeStatement {
            span: self.span_since(start),
            target: GrantTarget::Graph {
                privilege,
                graph,
                resource,
                condition,
            },
            subject,
        })
    }

    /// Parses an optional conditional policy selector ([ADR 0075] §3):
    /// `FOR (v:Label) WHERE …` or `FOR ()-[e:Label]->() WHERE …`.
    ///
    /// The `WHERE` body is parsed with the ordinary expression grammar and then
    /// restricted to the ADR 0075 §2 DSL: comparisons of one selector-variable property
    /// against a literal or `MSG_CALLER()`, joined by `AND`. Every unsupported shape is
    /// rejected here with its own distinct error.
    #[cfg(feature = "gleaph")]
    fn parse_grant_condition_opt(&mut self) -> Result<Option<GrantCondition>, GqlError> {
        if !self.eat_keyword("FOR") {
            return Ok(None);
        }
        let start = self.save();
        let selector = self.parse_condition_selector()?;
        self.expect_keyword("WHERE")?;
        let expr = self.parse_expr()?;
        let conjuncts = normalize_policy_predicate(&expr, selector.variable(), selector.label())?;
        Ok(Some(GrantCondition {
            span: self.span_since(start),
            selector,
            predicate: GrantPredicate { conjuncts },
        }))
    }

    /// Parses `(v : Label)` or the edge forms `()-[e : Label]->()`, `()-[e : Label]-()`,
    /// and `<-[e : Label]-()` (direction spelling is accepted but not preserved; the
    /// canonical rendering is undirected).
    #[cfg(feature = "gleaph")]
    fn parse_condition_selector(&mut self) -> Result<GrantConditionSelector, GqlError> {
        // Incoming spelling: `<-[e:L]-()`.
        if self.eat_token(&Token::LeftArrowBracket) {
            let (variable, label) = self.parse_edge_pattern_body()?;
            self.expect_edge_bracket_tail()?;
            return Ok(GrantConditionSelector::Edge { variable, label });
        }
        self.expect_token(&Token::LParen)?;
        // Outgoing/undirected spelling: `()-[e:L]->()` / `()-[e:L]-()`.
        if self.eat_token(&Token::RParen) {
            self.expect_token(&Token::MinusLeftBracket)?;
            let (variable, label) = self.parse_edge_pattern_body()?;
            self.expect_edge_bracket_tail()?;
            return Ok(GrantConditionSelector::Edge { variable, label });
        }
        let variable = self.expect_ident()?.to_owned();
        self.expect_token(&Token::Colon)?;
        let label = self.expect_ident()?.to_owned();
        self.expect_token(&Token::RParen)?;
        Ok(GrantConditionSelector::Vertex { variable, label })
    }

    /// Consumes the closing bracket of an edge pattern body plus its optional direction
    /// tail, then the empty closing endpoint `()`.
    #[cfg(feature = "gleaph")]
    fn expect_edge_bracket_tail(&mut self) -> Result<(), GqlError> {
        if self.eat_token(&Token::BracketRightArrow) || self.eat_token(&Token::RightBracketMinus) {
        } else if self.eat_token(&Token::RBracket) {
            let _ = self.eat_token(&Token::RightArrow);
            let _ = self.eat_token(&Token::Minus);
        } else {
            return Err(self.expected("']' after edge pattern"));
        }
        self.expect_token(&Token::LParen)?;
        self.expect_token(&Token::RParen)
    }

    /// Parses the `e : L` interior of an edge pattern bracket.
    #[cfg(feature = "gleaph")]
    fn parse_edge_pattern_body(&mut self) -> Result<(String, String), GqlError> {
        let variable = self.expect_ident()?.to_owned();
        self.expect_token(&Token::Colon)?;
        let label = self.expect_ident()?.to_owned();
        Ok((variable, label))
    }
    #[cfg(feature = "gleaph")]
    fn parse_grant_privilege(&mut self) -> Result<GrantPrivilege, GqlError> {
        if self.eat_keyword("MATCH") {
            return Ok(GrantPrivilege::Match);
        }
        if self.eat_keyword("TRAVERSE") {
            let direction = if self.eat_keyword("OUTGOING") {
                Some(GrantDirection::Outgoing)
            } else if self.eat_keyword("INCOMING") {
                Some(GrantDirection::Incoming)
            } else {
                None
            };
            return Ok(GrantPrivilege::Traverse { direction });
        }
        if self.eat_keyword("READ") {
            return Ok(GrantPrivilege::Read {
                properties: Vec::new(),
            });
        }
        if self.eat_keyword("CREATE") {
            return Ok(GrantPrivilege::Create);
        }
        if self.eat_keyword("UPDATE") {
            return Ok(GrantPrivilege::Update);
        }
        if self.eat_keyword("DELETE") {
            return Ok(GrantPrivilege::Delete);
        }
        Err(self.expected("MATCH, TRAVERSE, READ, CREATE, UPDATE, or DELETE"))
    }

    #[cfg(feature = "gleaph")]
    fn parse_grant_resource_selector(
        &mut self,
        privilege: &mut GrantPrivilege,
    ) -> Result<GrantResourceSelector, GqlError> {
        if self.eat_keyword("EDGES") {
            return Ok(GrantResourceSelector::Edge {
                label: self.expect_ident()?.to_owned(),
            });
        }
        if !self.at_keyword("NODES") && !self.at_keyword("VERTICES") {
            return Err(self.expected("NODES, VERTICES, or EDGES"));
        }
        self.advance();
        let label = self.expect_ident()?.to_owned();
        if matches!(self.peek(), Some(Token::LBrace)) {
            if !matches!(privilege, GrantPrivilege::Read { .. }) {
                return Err(self.expected("`{...}` property lists require the READ privilege"));
            }
            self.advance();
            let mut properties = Vec::new();
            loop {
                properties.push(self.expect_ident()?.to_owned());
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
            self.expect_token(&Token::RBrace)?;
            if let GrantPrivilege::Read { properties: slot } = privilege {
                *slot = properties;
            }
        }
        Ok(GrantResourceSelector::Vertex { label })
    }

    #[cfg(feature = "gleaph")]
    fn parse_grant_subject_literal(&mut self) -> Result<GrantSubjectLiteral, GqlError> {
        if self.eat_keyword("PUBLIC") {
            return Ok(GrantSubjectLiteral::Public);
        }
        self.expect_keyword("PRINCIPAL")?;
        match self.peek() {
            Some(Token::StringLit(text)) => {
                let text = text.clone();
                self.advance();
                Ok(GrantSubjectLiteral::Principal(text))
            }
            _ => Err(self.expected("a principal string literal")),
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Conditional policy predicate restriction (ADR 0075 §2)
    // ════════════════════════════════════════════════════════════════════════

    // ════════════════════════════════════════════════════════════════════════
    // §14.6 — FILTER statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `FILTER [WHERE] <expr>`.
    pub fn parse_filter_statement(&mut self) -> Result<FilterStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("FILTER")?;
        // The WHERE keyword is optional after FILTER.
        let where_keyword = self.eat_keyword("WHERE");
        let condition = self.parse_expr()?;
        Ok(FilterStatement {
            span: self.span_since(start),
            where_keyword,
            condition,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.7 — LET statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `LET var = expr [, var = expr]*`.
    pub fn parse_let_statement(&mut self) -> Result<LetStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("LET")?;
        let bindings = self.comma_list(Self::parse_let_binding)?;
        Ok(LetStatement {
            span: self.span_since(start),
            bindings,
        })
    }

    /// Parses a single let binding: `variable = expression`.
    fn parse_let_binding(&mut self) -> Result<LetBinding, GqlError> {
        let start = self.save();
        let variable = self.expect_ident()?;
        self.expect_token(&Token::Eq)?;
        let value = self.parse_expr()?;
        Ok(LetBinding {
            span: self.span_since(start),
            variable,
            value,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.8 — FOR statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `FOR var IN expr [WITH ORDINALITY|OFFSET var]`.
    pub fn parse_for_statement(&mut self) -> Result<ForStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("FOR")?;
        let variable = self.expect_ident()?;
        self.expect_keyword("IN")?;
        let list = self.parse_expr()?;

        let ordinality = if self.at_keyword("WITH") {
            self.advance(); // WITH
            if self.at_keyword("ORDINALITY") || self.at_keyword("OFFSET") {
                let ord_start = self.save();
                let offset_keyword = self.at_keyword("OFFSET");
                self.advance(); // ORDINALITY or OFFSET
                let var = self.expect_ident()?;
                Some(ForOrdinality {
                    span: self.span_since(ord_start),
                    offset_keyword,
                    variable: var,
                })
            } else {
                return Err(self.expected("'ORDINALITY' or 'OFFSET' after WITH"));
            }
        } else {
            None
        };

        Ok(ForStatement {
            span: self.span_since(start),
            variable,
            list,
            ordinality,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §13.2 — INSERT statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `INSERT <insert-graph-pattern>`.
    pub fn parse_insert_statement(&mut self) -> Result<InsertStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("INSERT")?;

        // Optional INTO <graph-name> — cypher extension (GQL standard has no INTO clause).
        #[cfg(feature = "cypher")]
        let graph_name = if self.eat_keyword("INTO") {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        #[cfg(not(feature = "cypher"))]
        let graph_name = None;

        let patterns = self.parse_insert_graph_pattern()?;
        Ok(InsertStatement {
            span: self.span_since(start),
            graph_name,
            patterns,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §13.3 — SET statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `SET setItem [, setItem]*`.
    pub fn parse_set_statement(&mut self) -> Result<SetStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("SET")?;
        let items = self.comma_list(Self::parse_set_item)?;
        Ok(SetStatement {
            span: self.span_since(start),
            items,
        })
    }

    /// Parses a single SET item.
    ///
    /// There are three forms:
    /// - `v.prop = expr` — set a property
    /// - `v = { key: val, ... }` — replace all properties
    /// - `v :Label` or `v IS Label` — set a label
    fn parse_set_item(&mut self) -> Result<SetItem, GqlError> {
        let start = self.save();
        let variable = self.expect_ident()?;

        if self.eat_token(&Token::Dot) {
            // v.prop = expr
            let mut property = self.expect_ident()?;
            while self.eat_token(&Token::Dot) {
                property.push('.');
                property.push_str(&self.expect_ident()?);
            }
            self.expect_token(&Token::Eq)?;
            let value = self.parse_expr()?;
            Ok(SetItem::Property {
                span: self.span_since(start),
                variable,
                property,
                value,
            })
        } else if self.at_token(&Token::Eq) {
            // v = expr  (all-properties replacement)
            self.advance();
            let value = self.parse_expr()?;
            Ok(SetItem::AllProperties {
                span: self.span_since(start),
                variable,
                value,
            })
        } else if self.at_token(&Token::Colon) || self.at_keyword("IS") {
            // v :Label or v IS Label
            let is_or_colon = if self.eat_token(&Token::Colon) {
                IsOrColon::Colon
            } else {
                self.advance(); // IS
                IsOrColon::Is
            };
            let label = self.expect_ident()?;
            Ok(SetItem::Label {
                span: self.span_since(start),
                variable,
                label,
                is_or_colon,
            })
        } else {
            Err(self.expected("'.', '=', ':', or 'IS' after variable in SET item"))
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // §13.4 — REMOVE statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `REMOVE removeItem [, removeItem]*`.
    pub fn parse_remove_statement(&mut self) -> Result<RemoveStatement, GqlError> {
        let start = self.save();
        self.expect_keyword("REMOVE")?;
        let items = self.comma_list(Self::parse_remove_item)?;
        Ok(RemoveStatement {
            span: self.span_since(start),
            items,
        })
    }

    /// Parses a single REMOVE item.
    ///
    /// Two forms:
    /// - `v.prop` — remove a property
    /// - `v :Label` / `v IS Label` — remove a label
    fn parse_remove_item(&mut self) -> Result<RemoveItem, GqlError> {
        let start = self.save();
        let variable = self.expect_ident()?;

        if self.eat_token(&Token::Dot) {
            let mut property = self.expect_ident()?;
            while self.eat_token(&Token::Dot) {
                property.push('.');
                property.push_str(&self.expect_ident()?);
            }
            Ok(RemoveItem::Property {
                span: self.span_since(start),
                variable,
                property,
            })
        } else if self.at_token(&Token::Colon) || self.at_keyword("IS") {
            let is_or_colon = if self.eat_token(&Token::Colon) {
                IsOrColon::Colon
            } else {
                self.advance(); // IS
                IsOrColon::Is
            };
            let label = self.expect_ident()?;
            Ok(RemoveItem::Label {
                span: self.span_since(start),
                variable,
                label,
                is_or_colon,
            })
        } else {
            Err(self.expected("'.' or ':' after variable in REMOVE item"))
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // §13.5 — DELETE statement
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `[DETACH | NODETACH] DELETE expr [, expr]*`.
    pub fn parse_delete_statement(&mut self) -> Result<DeleteStatement, GqlError> {
        let start = self.save();
        let detach = if self.eat_keyword("DETACH") {
            DeleteDetach::Detach
        } else if self.eat_keyword("NODETACH") {
            DeleteDetach::NoDetach
        } else {
            DeleteDetach::Unspecified
        };

        self.expect_keyword("DELETE")?;

        // Comma-separated list of delete items (GQL §13.5: valueExpression).
        let items = self.comma_list(|p| p.parse_expr())?;

        Ok(DeleteStatement {
            span: self.span_since(start),
            detach,
            items,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §15 — CALL procedure
    // ════════════════════════════════════════════════════════════════════════

    /// Parses `[OPTIONAL] CALL [(var, ...)] { <composite-query> }`.
    pub fn parse_inline_procedure_call(&mut self) -> Result<InlineProcedureCall, GqlError> {
        let start = self.save();
        let optional = self.eat_keyword("OPTIONAL");
        self.expect_keyword("CALL")?;
        let scope = if self.eat_token(&Token::LParen) {
            let vars = if self.at_token(&Token::RParen) {
                vec![]
            } else {
                self.comma_list(|p| p.expect_ident())?
            };
            self.expect_token(&Token::RParen)?;
            InlineProcedureScope::Explicit(vars)
        } else {
            InlineProcedureScope::ImplicitAll
        };
        self.expect_token(&Token::LBrace)?;
        let body = self.parse_composite_query_expr()?;
        self.expect_token(&Token::RBrace)?;
        Ok(InlineProcedureCall {
            span: self.span_since(start),
            optional,
            use_graph: None,
            scope,
            body: Box::new(body),
        })
    }

    /// Parses `[OPTIONAL] CALL procedureName( args ) [YIELD items]`.
    pub fn parse_call_procedure_statement(&mut self) -> Result<CallProcedureStatement, GqlError> {
        let start = self.save();
        let optional = self.eat_keyword("OPTIONAL");
        self.expect_keyword("CALL")?;

        let name = self.parse_object_name()?;

        // Argument list in parentheses.
        let args = if self.eat_token(&Token::LParen) {
            if self.at_token(&Token::RParen) {
                self.advance();
                vec![]
            } else {
                let args = self.comma_list(|p| p.parse_expr())?;
                self.expect_token(&Token::RParen)?;
                args
            }
        } else {
            vec![]
        };

        // Optional YIELD clause.
        let yield_items = if self.eat_keyword("YIELD") {
            Some(self.parse_yield_clause()?)
        } else {
            None
        };

        Ok(CallProcedureStatement {
            span: self.span_since(start),
            optional,
            name,
            args,
            yield_items,
        })
    }

    // ════════════════════════════════════════════════════════════════════════
    // §14.10 — Primitive result statement (RETURN / FINISH)
    // ════════════════════════════════════════════════════════════════════════

    /// Parses RETURN or FINISH as a primitive result statement.
    fn parse_return_or_finish(&mut self) -> Result<ResultStatement, GqlError> {
        if self.eat_keyword("FINISH") {
            Ok(ResultStatement::Finish)
        } else {
            self.expect_keyword("RETURN")?;
            let ret = self.parse_return_statement()?;

            // Optional trailing ORDER BY / OFFSET / LIMIT after the return body
            // is already handled inside parse_return_statement (clause.rs).
            Ok(ResultStatement::Return(Box::new(ret)))
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Helpers
    // ════════════════════════════════════════════════════════════════════════

    /// Parses a possibly-qualified object name.
    ///
    /// Supports:
    /// - Simple: `a`
    /// - Dot-qualified: `a.b.c`
    /// - Absolute catalog path: `/a`, `/a/b` (GQL §13 absoluteDirectoryPath)
    /// - Mixed: `/a/b.c`
    /// - Substituted parameter reference: `$$name`
    pub fn parse_object_name(&mut self) -> Result<ObjectName, GqlError> {
        if let Some(Token::SubstitutedParam(name)) = self.peek().cloned() {
            self.advance();
            return Ok(ObjectName::simple(format!("$${name}")));
        }

        // GQL §11.1 currentGraph / §17.2 homeGraph — reserved keyword
        // graph references allowed wherever a graph expression is expected.
        for kw in &[
            "CURRENT_GRAPH",
            "CURRENT_PROPERTY_GRAPH",
            "HOME_GRAPH",
            "HOME_PROPERTY_GRAPH",
        ] {
            if self.eat_keyword(kw) {
                return Ok(ObjectName::simple(kw.to_string()));
            }
        }

        // Handle absolute catalog paths starting with `/`.
        if self.eat_token(&Token::Slash) {
            let first = self.expect_ident()?;
            let mut parts = vec![format!("/{first}")];
            // Continue with `/ident` segments.
            while self.eat_token(&Token::Slash) {
                parts.push(self.expect_ident()?);
            }
            // Also allow `.ident` segments after the slash path.
            while self.eat_token(&Token::Dot) {
                parts.push(self.expect_ident()?);
            }
            return Ok(ObjectName::qualified(parts));
        }

        let mut parts = vec![self.expect_ident()?];
        while self.eat_token(&Token::Dot) {
            parts.push(self.expect_ident()?);
        }
        Ok(ObjectName::qualified(parts))
    }

    /// Parses a schema name which MUST start with `/` per GQL §13
    /// (catalogSchemaParentName / absoluteDirectoryPath).
    pub fn parse_schema_name(&mut self) -> Result<ObjectName, GqlError> {
        if !self.at_token(&Token::Slash) {
            return Err(self.expected("'/' before schema name (GQL requires absolute path)"));
        }
        self.parse_object_name()
    }
}

/// Maximum comparisons in one conditional policy conjunction ([ADR 0075] §2
/// determinism bound). The grammar-level authoring bound; the storage layer
/// independently enforces its encoding cap as defense in depth.
#[cfg(feature = "gleaph")]
pub const MAX_GRANT_CONDITION_CONJUNCTS: usize = 8;

/// Restricts a parsed `WHERE` expression to the ADR 0075 §2 conditional-policy DSL and
/// normalizes it into an AND-ordered comparison list.
///
/// Accepted shape: `Comparison (AND Comparison)*` where each comparison is
/// `<selector-variable>.<property> <op> <literal | MSG_CALLER()>`. Every unsupported
/// shape is rejected with its own distinct error message so grant authors can see which
/// construct was refused.
#[cfg(feature = "gleaph")]
fn normalize_policy_predicate(
    expr: &Expr,
    selector_variable: &str,
    selector_label: &str,
) -> Result<Vec<GrantComparison>, GqlError> {
    let mut conjuncts = Vec::new();
    collect_policy_conjuncts(expr, selector_variable, selector_label, &mut conjuncts)?;
    Ok(conjuncts)
}

/// Recursive AND-flattening walker. `depth` is carried through recursion via the
/// accumulated list length so the conjunction cap bounds the whole predicate.
#[cfg(feature = "gleaph")]
fn collect_policy_conjuncts(
    expr: &Expr,
    selector_variable: &str,
    selector_label: &str,
    out: &mut Vec<GrantComparison>,
) -> Result<(), GqlError> {
    match &expr.kind {
        ExprKind::And(left, right) => {
            collect_policy_conjuncts(left, selector_variable, selector_label, out)?;
            collect_policy_conjuncts(right, selector_variable, selector_label, out)?;
            Ok(())
        }
        ExprKind::Compare { left, op, right } => {
            let property = policy_property_ref(left, selector_variable, selector_label)?;
            let value = policy_value(right)?;
            if out.len() >= MAX_GRANT_CONDITION_CONJUNCTS {
                return Err(GqlError::Parse(format!(
                    "conditional policy for ({selector_variable}:{selector_label}) exceeds the \
                     maximum of {MAX_GRANT_CONDITION_CONJUNCTS} AND comparisons"
                )));
            }
            out.push(GrantComparison {
                property,
                op: *op,
                value,
            });
            Ok(())
        }
        ExprKind::Or(_, _) => Err(GqlError::Parse(
            "OR is not supported in conditional policies; conditions are AND-only (ADR 0075 §2)"
                .into(),
        )),
        ExprKind::Not(_) => Err(GqlError::Parse(
            "NOT is not supported in conditional policies; conditions are AND-only (ADR 0075 §2)"
                .into(),
        )),
        ExprKind::Xor(_, _) => Err(GqlError::Parse(
            "XOR is not supported in conditional policies; conditions are AND-only (ADR 0075 §2)"
                .into(),
        )),
        ExprKind::BinaryOp { .. } => Err(GqlError::Parse(
            "arithmetic expressions are not supported in conditional policies (ADR 0075 §2)".into(),
        )),
        ExprKind::PropertyExists { .. }
        | ExprKind::ExistsSubquery(_)
        | ExprKind::ExistsPattern(_) => Err(GqlError::Parse(
            "EXISTS is not supported in conditional policies (ReBAC conditions are a later phase)"
                .into(),
        )),
        _ => Err(GqlError::Parse(format!(
            "unsupported conditional-policy predicate shape; expected \
             <{selector_variable}.property> <op> <literal or MSG_CALLER()> joined by AND"
        ))),
    }
}

/// Validates the left side of one policy comparison: exactly one property access on the
/// selector variable.
#[cfg(feature = "gleaph")]
fn policy_property_ref(
    expr: &Expr,
    selector_variable: &str,
    _selector_label: &str,
) -> Result<String, GqlError> {
    match &expr.kind {
        ExprKind::PropertyAccess {
            expr: base,
            property,
        } => match &base.kind {
            ExprKind::Variable(name) if name == selector_variable => Ok(property.clone()),
            ExprKind::Variable(name) => Err(GqlError::Parse(format!(
                "conditional policy comparisons must reference the selector variable \
                 '{name}.{property}' as '{selector_variable}.{property}'"
            ))),
            _ => Err(GqlError::Parse(
                "the left side of a conditional policy comparison must be \
                 <selector-variable>.<property>"
                    .into(),
            )),
        },
        _ => Err(GqlError::Parse(
            "the left side of a conditional policy comparison must be \
             <selector-variable>.<property>"
                .into(),
        )),
    }
}

/// Validates the right side of one policy comparison: a scalar literal or zero-argument
/// `MSG_CALLER()` (any case).
#[cfg(feature = "gleaph")]
fn policy_value(expr: &Expr) -> Result<GrantValueExpr, GqlError> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(GrantValueExpr::Literal(value.clone())),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
        } => {
            let is_msg_caller =
                name.parts.len() == 1 && name.parts[0].eq_ignore_ascii_case("MSG_CALLER");
            if !is_msg_caller {
                return Err(GqlError::Parse(format!(
                    "'{}' is not supported in conditional policies; only MSG_CALLER() may appear \
                     on the right side",
                    name.parts.join(".")
                )));
            }
            if !args.is_empty() || *distinct {
                return Err(GqlError::Parse(
                    "MSG_CALLER() takes no arguments and no DISTINCT in conditional policies"
                        .into(),
                ));
            }
            Ok(GrantValueExpr::MsgCaller)
        }
        ExprKind::BinaryOp { .. } => Err(GqlError::Parse(
            "arithmetic expressions are not supported in conditional policies (ADR 0075 §2)".into(),
        )),
        ExprKind::PropertyAccess { .. } | ExprKind::Variable(_) => Err(GqlError::Parse(
            "property-to-property comparisons are not supported in conditional policies; the \
             right side must be a literal or MSG_CALLER()"
                .into(),
        )),
        _ => Err(GqlError::Parse(
            "the right side of a conditional policy comparison must be a literal or MSG_CALLER()"
                .into(),
        )),
    }
}

#[cfg(test)]
#[cfg(all(test, not(feature = "gleaph")))]
mod search_feature_boundary_tests {
    use crate::parser;

    #[test]
    fn search_vector_index_rejected_without_gleaph_feature() {
        let err = parser::parse(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX document_embedding \
               FOR $query \
               LIMIT 100 \
             ) SCORE AS similarity \
             RETURN d, similarity",
        )
        .expect_err("SEARCH should be rejected when the gleaph feature is disabled");
        let msg = err.to_string();
        assert!(
            msg.contains("SEARCH") || msg.contains("RETURN") || msg.contains("expected"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn standard_gql_parses_without_gleaph_feature() {
        let program = parser::parse("MATCH (n:Person) WHERE n.age > 18 RETURN n.name")
            .expect("standard GQL should parse without the gleaph feature");
        assert!(program.transaction_activity.is_some());
    }
}

#[cfg(all(test, feature = "gleaph"))]
mod search_parser_tests {
    use super::*;
    use crate::ast::{
        ExprKind, SearchOutputKind, SearchProvider, SearchStatement, SimpleQueryStatement,
        VectorSearchSpec,
    };
    use crate::parser;
    use crate::token::Span;

    fn first_simple_query_part(input: &str) -> SimpleQueryStatement {
        let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
        let tx = program.transaction_activity.expect("expected tx");
        let block = tx.body.expect("expected body");
        let cq = match block.first {
            Statement::Query(cq) => cq,
            other => panic!("expected query, got {other:?}"),
        };
        cq.left
            .parts
            .into_iter()
            .nth(1)
            .expect("expected a second part")
    }

    fn assert_search_vector(
        stmt: &SearchStatement,
        binding: &str,
        expected_index_name: &str,
        expected_limit: i64,
        output_kind: SearchOutputKind,
        alias: &str,
    ) {
        assert_eq!(stmt.binding, binding);
        let SearchProvider::VectorIndex(VectorSearchSpec {
            index_name,
            query,
            limit,
            filter,
        }) = &stmt.provider;
        assert_eq!(index_name.parts, vec![expected_index_name.to_string()]);
        assert!(matches!(
            &query.kind,
            ExprKind::Parameter(v) if v == "$query"
        ));
        assert!(matches!(
            &limit.kind,
            ExprKind::Literal(crate::Value::Int64(v)) if *v == expected_limit
        ));
        assert!(filter.is_none());
        assert_eq!(stmt.output.kind, output_kind);
        assert_eq!(stmt.output.alias, alias);
    }

    #[test]
    fn parse_search_score_as() {
        let input = "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX document_embedding \
               FOR $query \
               LIMIT 100 \
             ) SCORE AS similarity \
             RETURN d, similarity";
        let part = first_simple_query_part(input);
        let SimpleQueryStatement::Search(stmt) = part else {
            panic!("expected Search, got {part:?}");
        };
        assert_search_vector(
            &stmt,
            "d",
            "document_embedding",
            100,
            SearchOutputKind::Score,
            "similarity",
        );
    }

    #[test]
    fn parse_search_distance_as() {
        let part = first_simple_query_part(
            "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX document_embedding \
               FOR $query \
               LIMIT 10 \
             ) DISTANCE AS distance \
             RETURN d, distance",
        );
        let SimpleQueryStatement::Search(stmt) = part else {
            panic!("expected Search, got {part:?}");
        };
        assert_search_vector(
            &stmt,
            "d",
            "document_embedding",
            10,
            SearchOutputKind::Distance,
            "distance",
        );
    }

    #[test]
    fn parse_search_with_where_is_rejected_by_planner_not_parser() {
        let input = "MATCH (d:Document) \
             SEARCH d IN ( \
               VECTOR INDEX document_embedding \
               FOR $query \
               WHERE d.published_at \u{003e}= $cutoff \
               LIMIT 100 \
             ) SCORE AS similarity \
             RETURN d, similarity";
        // Parser accepts the reserved WHERE shape.
        let program = parser::parse(input).expect("parser should accept SEARCH ... WHERE");
        let tx = program.transaction_activity.expect("expected tx");
        let block = tx.body.expect("expected body");
        let cq = match block.first {
            Statement::Query(cq) => cq,
            other => panic!("expected query, got {other:?}"),
        };
        let part = cq
            .left
            .parts
            .into_iter()
            .nth(1)
            .expect("expected a second part");
        let SimpleQueryStatement::Search(stmt) = part else {
            panic!("expected Search");
        };
        assert!(stmt.provider.filter().is_some());
    }

    #[test]
    fn parse_search_missing_in_fails() {
        let err = parser::parse(
            "MATCH (d:Document) SEARCH d VECTOR INDEX document_embedding FOR $query LIMIT 100 SCORE AS similarity RETURN d",
        )
        .expect_err("expected parse error");
        assert!(err.to_string().contains("IN"));
    }

    #[test]
    fn parse_search_missing_vector_index_fails() {
        let err = parser::parse(
            "MATCH (d:Document) SEARCH d IN (INDEX document_embedding FOR $query LIMIT 100) SCORE AS similarity RETURN d",
        )
        .expect_err("expected parse error");
        assert!(err.to_string().contains("VECTOR"));
    }

    #[test]
    fn parse_search_missing_for_fails() {
        let err = parser::parse(
            "MATCH (d:Document) SEARCH d IN (VECTOR INDEX document_embedding $query LIMIT 100) SCORE AS similarity RETURN d",
        )
        .expect_err("expected parse error");
        assert!(err.to_string().contains("FOR"));
    }

    #[test]
    fn parse_search_missing_limit_fails() {
        let err = parser::parse(
            "MATCH (d:Document) SEARCH d IN (VECTOR INDEX document_embedding FOR $query) SCORE AS similarity RETURN d",
        )
        .expect_err("expected parse error");
        assert!(err.to_string().contains("LIMIT"));
    }

    #[test]
    fn parse_search_missing_score_distance_as_fails() {
        let err = parser::parse(
            "MATCH (d:Document) SEARCH d IN (VECTOR INDEX document_embedding FOR $query LIMIT 100) RETURN d",
        )
        .expect_err("expected parse error");
        assert!(err.to_string().contains("SCORE") || err.to_string().contains("DISTANCE"));
    }

    #[test]
    fn parse_search_preserves_span() {
        let part = first_simple_query_part(
            "MATCH (d:Document) SEARCH d IN (VECTOR INDEX document_embedding FOR $query LIMIT 100) SCORE AS similarity RETURN d",
        );
        let SimpleQueryStatement::Search(stmt) = part else {
            panic!("expected Search");
        };
        assert_ne!(stmt.span, Span::DUMMY);
    }
}

#[cfg(all(test, feature = "gleaph"))]
mod grant_parser_tests {
    use super::*;
    use crate::ast::{
        GrantDirection, GrantPrivilege, GrantResourceSelector, GrantSubjectLiteral, GrantTarget,
    };
    use crate::parser;
    use crate::token::Span;

    /// First simple query part of the single-statement program `input`.
    fn first_part(input: &str) -> SimpleQueryStatement {
        let program = parser::parse(input).unwrap_or_else(|e| panic!("parse error: {e}"));
        let tx = program.transaction_activity.expect("expected tx");
        let block = tx.body.expect("expected body");
        let cq = match block.first {
            Statement::Query(cq) => cq,
            other => panic!("expected query, got {other:?}"),
        };
        cq.left.parts.into_iter().next().expect("expected a part")
    }

    fn parse_valid(input: &str) -> SimpleQueryStatement {
        let part = first_part(input);
        assert!(
            crate::parser::parse(input).is_ok(),
            "round-trip source must stay valid"
        );
        part
    }

    #[test]
    fn parse_grant_match_nodes_principal_subject() {
        let SimpleQueryStatement::Grant(stmt) =
            parse_valid("GRANT MATCH ON GRAPH social NODES Person TO PRINCIPAL 'w7x7r-cok77-xa'")
        else {
            panic!("expected Grant");
        };
        assert_eq!(
            stmt.target,
            GrantTarget::Graph {
                privilege: GrantPrivilege::Match,
                graph: crate::ast::ObjectName::simple("social"),
                resource: GrantResourceSelector::Vertex {
                    label: "Person".to_string()
                },
                condition: None,
            }
        );
        assert_eq!(
            stmt.subject,
            GrantSubjectLiteral::Principal("w7x7r-cok77-xa".to_string())
        );
    }

    #[test]
    fn parse_grant_traverse_directional_modifiers() {
        for (text, direction) in [
            ("OUTGOING", Some(GrantDirection::Outgoing)),
            ("INCOMING", Some(GrantDirection::Incoming)),
            ("", None),
        ] {
            let input = format!(
                "GRANT TRAVERSE {direction_text}ON GRAPH g EDGES KNOWS TO PUBLIC",
                direction_text = if text.is_empty() {
                    String::new()
                } else {
                    format!("{text} ")
                }
            );
            let SimpleQueryStatement::Grant(stmt) = parse_valid(&input) else {
                panic!("expected Grant for {input}");
            };
            let GrantTarget::Graph {
                privilege,
                resource,
                ..
            } = &stmt.target
            else {
                panic!("expected graph target for {input}");
            };
            assert_eq!(
                privilege,
                &GrantPrivilege::Traverse { direction },
                "input: {input}"
            );
            assert_eq!(
                resource,
                &GrantResourceSelector::Edge {
                    label: "KNOWS".to_string()
                }
            );
            assert_eq!(stmt.subject, GrantSubjectLiteral::Public);
        }
    }

    #[test]
    fn parse_grant_read_property_list_lowers_into_privilege() {
        let SimpleQueryStatement::Grant(stmt) =
            parse_valid("GRANT READ ON GRAPH g NODES Person { name, age } TO PUBLIC")
        else {
            panic!("expected Grant");
        };
        let GrantTarget::Graph { privilege, .. } = stmt.target else {
            panic!("expected graph target");
        };
        assert_eq!(
            privilege,
            GrantPrivilege::Read {
                properties: vec!["name".to_string(), "age".to_string()]
            }
        );
    }

    #[test]
    fn parse_grant_vertices_keyword_canonicalizes_to_vertex_selector() {
        let SimpleQueryStatement::Grant(stmt) =
            parse_valid("GRANT UPDATE ON GRAPH g VERTICES Person TO PUBLIC")
        else {
            panic!("expected Grant");
        };
        let GrantTarget::Graph {
            privilege,
            resource,
            ..
        } = stmt.target
        else {
            panic!("expected graph target");
        };
        assert_eq!(privilege, GrantPrivilege::Update);
        assert_eq!(
            resource,
            GrantResourceSelector::Vertex {
                label: "Person".to_string()
            }
        );
    }

    // ── Conditional policy selectors (ADR 0075 §3) ──

    use crate::ast::CmpOp;
    use crate::ast::{
        GrantComparison, GrantCondition, GrantConditionSelector, GrantPredicate, GrantValueExpr,
    };

    fn grant_condition(input: &str) -> GrantCondition {
        let SimpleQueryStatement::Grant(stmt) = parse_valid(input) else {
            panic!("expected Grant for {input}");
        };
        let GrantTarget::Graph { condition, .. } = stmt.target else {
            panic!("expected graph target for {input}");
        };
        condition.expect("expected a conditional selector")
    }

    fn assert_condition_error(input: &str, fragment: &str) {
        let err = parser::parse(input).expect_err("expected parse error");
        let msg = err.to_string();
        assert!(
            msg.contains(fragment),
            "error message must name the refused shape ({fragment}), got: {msg}"
        );
    }

    #[test]
    fn conditional_vertex_selector_parses_and_normalizes_and_chain() {
        let condition = grant_condition(
            "GRANT READ ON GRAPH social NODES Post \
             FOR (p:Post) WHERE p.visibility = 'public' AND p.owner = MSG_CALLER() \
             TO PUBLIC",
        );
        assert_eq!(
            condition.selector,
            GrantConditionSelector::Vertex {
                variable: "p".to_string(),
                label: "Post".to_string()
            }
        );
        assert_eq!(
            condition.predicate,
            GrantPredicate {
                conjuncts: vec![
                    GrantComparison {
                        property: "visibility".to_string(),
                        op: CmpOp::Eq,
                        value: GrantValueExpr::Literal(crate::Value::Text("public".to_string())),
                    },
                    GrantComparison {
                        property: "owner".to_string(),
                        op: CmpOp::Eq,
                        value: GrantValueExpr::MsgCaller,
                    },
                ],
            }
        );
    }

    #[test]
    fn conditional_selectors_parse_all_comparison_operators_and_msg_caller_case() {
        let condition = grant_condition(
            "GRANT MATCH ON GRAPH g NODES T FOR (v:T) WHERE v.a <> 1 AND v.b <= 2 AND \
             v.c < 3 AND v.d >= 4 AND v.e > 5 AND v.f = msg_caller() TO PUBLIC",
        );
        let ops: Vec<CmpOp> = condition.predicate.conjuncts.iter().map(|c| c.op).collect();
        assert_eq!(
            ops,
            vec![
                CmpOp::Ne,
                CmpOp::Le,
                CmpOp::Lt,
                CmpOp::Ge,
                CmpOp::Gt,
                CmpOp::Eq
            ]
        );
        assert!(matches!(
            condition.predicate.conjuncts.last().unwrap().value,
            GrantValueExpr::MsgCaller
        ));
    }

    #[test]
    fn conditional_edge_selector_parses_in_all_direction_spellings() {
        for input in [
            "GRANT MATCH ON GRAPH g NODES T FOR ()-[e:KNOWS]->() WHERE e.weight > 5 TO PUBLIC",
            "GRANT MATCH ON GRAPH g NODES T FOR ()-[e:KNOWS]-() WHERE e.weight > 5 TO PUBLIC",
            "GRANT MATCH ON GRAPH g NODES T FOR <-[e:KNOWS]-() WHERE e.weight > 5 TO PUBLIC",
        ] {
            let condition = grant_condition(input);
            assert_eq!(
                condition.selector,
                GrantConditionSelector::Edge {
                    variable: "e".to_string(),
                    label: "KNOWS".to_string()
                },
                "input: {input}"
            );
        }
    }

    #[test]
    fn conditional_revoke_accepts_the_selector_form() {
        let program = parser::parse(
            "REVOKE READ ON GRAPH g NODES Post FOR (p:Post) WHERE p.owner = MSG_CALLER() \
             FROM PUBLIC",
        )
        .expect("conditional revoke parses");
        let tx = program.transaction_activity.expect("tx");
        let block = tx.body.expect("body");
        let mut found_revoke = false;
        for stmt in block.iter_statements() {
            let Statement::Query(query) = stmt else {
                continue;
            };
            let queries = std::iter::once(&query.left).chain(query.rest.iter().map(|(_, lq)| lq));
            for lq in queries {
                for part in &lq.parts {
                    if let SimpleQueryStatement::Revoke(revoke) = part {
                        let GrantTarget::Graph { condition, .. } = &revoke.target else {
                            panic!("expected graph target");
                        };
                        assert_eq!(
                            condition.as_ref().expect("conditional selector").selector,
                            GrantConditionSelector::Vertex {
                                variable: "p".to_string(),
                                label: "Post".to_string()
                            }
                        );
                        found_revoke = true;
                    }
                }
            }
        }
        assert!(found_revoke, "the REVOKE statement was not reached");
    }

    #[test]
    fn conditional_policy_rejects_or_not_arithmetic_exists_with_distinct_errors() {
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE v.a = 1 OR v.b = 2 TO PUBLIC",
            "OR",
        );
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE NOT v.a = 1 TO PUBLIC",
            "NOT",
        );
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE v.a = 1 + 2 TO PUBLIC",
            "arithmetic",
        );
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE EXISTS { (x) } TO PUBLIC",
            "EXISTS",
        );
    }

    #[test]
    fn conditional_policy_rejects_wrong_variable_property_to_property_and_unknown_functions() {
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE w.a = 1 TO PUBLIC",
            "selector variable",
        );
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE v.a = v.b TO PUBLIC",
            "literal or MSG_CALLER",
        );
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE v.a = upper('x') TO PUBLIC",
            "literal or MSG_CALLER",
        );
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE a = 1 TO PUBLIC",
            "<selector-variable>.<property>",
        );
    }

    #[test]
    fn conditional_policy_enforces_the_conjunction_depth_cap() {
        let mut where_clause = String::from("v.p0 = 0");
        for index in 1..12 {
            where_clause.push_str(&format!(" AND v.p{index} = {index}"));
        }
        let input =
            format!("GRANT READ ON GRAPH g NODES T FOR (v:T) WHERE {where_clause} TO PUBLIC");
        assert_condition_error(&input, "maximum of 8");
    }

    #[test]
    fn conditional_selector_requires_where_body() {
        assert_condition_error(
            "GRANT READ ON GRAPH g NODES T FOR (v:T) TO PUBLIC",
            "expected",
        );
    }

    #[test]
    fn parse_grant_execute_on_prepared_query_to_public_and_principal() {
        let SimpleQueryStatement::Grant(stmt) =
            parse_valid("GRANT EXECUTE ON PREPARED QUERY find-users TO PUBLIC")
        else {
            panic!("expected Grant");
        };
        assert_eq!(
            stmt.target,
            GrantTarget::PreparedQuery {
                name: "find-users".to_string()
            }
        );
        assert_eq!(stmt.subject, GrantSubjectLiteral::Public);

        let SimpleQueryStatement::Grant(stmt) =
            parse_valid("GRANT EXECUTE ON PREPARED QUERY find-users TO PRINCIPAL 'w7x7r-cok77-xa'")
        else {
            panic!("expected Grant");
        };
        assert_eq!(
            stmt.target,
            GrantTarget::PreparedQuery {
                name: "find-users".to_string()
            }
        );
        assert_eq!(
            stmt.subject,
            GrantSubjectLiteral::Principal("w7x7r-cok77-xa".to_string())
        );
    }

    #[test]
    fn parse_revoke_execute_on_prepared_query_mirrors_grant() {
        let SimpleQueryStatement::Revoke(stmt) =
            parse_valid("REVOKE EXECUTE ON PREPARED QUERY find-users FROM PUBLIC")
        else {
            panic!("expected Revoke");
        };
        assert_eq!(
            stmt.target,
            GrantTarget::PreparedQuery {
                name: "find-users".to_string()
            }
        );
        assert_eq!(stmt.subject, GrantSubjectLiteral::Public);
    }

    #[test]
    fn parse_graph_privilege_with_prepared_query_resource_fails() {
        // The discriminated union makes mixed forms a parse error, not a lowering case.
        assert!(
            parser::parse("GRANT MATCH ON PREPARED QUERY find-users TO PUBLIC").is_err(),
            "graph privileges require the ON GRAPH form"
        );
        assert!(
            parser::parse("REVOKE DELETE ON PREPARED QUERY find-users FROM PUBLIC").is_err(),
            "graph privileges require the ON GRAPH form"
        );
    }

    #[test]
    fn parse_execute_requires_prepared_query_keyword_sequence() {
        assert!(parser::parse("GRANT EXECUTE ON GRAPH g TO PUBLIC").is_err());
        assert!(parser::parse("GRANT EXECUTE TO PUBLIC").is_err());
        assert!(parser::parse("GRANT EXECUTE ON PREPARED QUERY TO PUBLIC").is_err());
    }

    #[test]
    fn parse_revoke_mirrors_grant_shape_with_from_preposition() {
        let SimpleQueryStatement::Revoke(stmt) =
            parse_valid("REVOKE DELETE ON GRAPH g EDGES KNOWS FROM PRINCIPAL 'a'")
        else {
            panic!("expected Revoke");
        };
        let GrantTarget::Graph {
            privilege,
            resource,
            ..
        } = stmt.target
        else {
            panic!("expected graph target");
        };
        assert_eq!(privilege, GrantPrivilege::Delete);
        assert_eq!(
            resource,
            GrantResourceSelector::Edge {
                label: "KNOWS".to_string()
            }
        );
        assert_eq!(
            stmt.subject,
            GrantSubjectLiteral::Principal("a".to_string())
        );
    }

    #[test]
    fn parse_grant_missing_graph_name_fails() {
        assert!(parser::parse("GRANT MATCH ON NODES Person TO PUBLIC").is_err());
    }

    #[test]
    fn parse_grant_missing_subject_fails() {
        assert!(parser::parse("GRANT MATCH ON GRAPH g NODES Person").is_err());
    }

    #[test]
    fn parse_grant_property_list_requires_read_privilege() {
        let err = parser::parse("GRANT MATCH ON GRAPH g NODES Person { name } TO PUBLIC")
            .expect_err("property lists are READ-only syntax");
        assert!(
            err.to_string().contains("READ"),
            "expected the precise READ-only message, got {err}"
        );
    }

    #[test]
    fn parse_grant_unknown_privilege_fails() {
        assert!(parser::parse("GRANT OWN ON GRAPH g NODES Person TO PUBLIC").is_err());
    }

    #[test]
    fn parse_grant_preserves_span() {
        let SimpleQueryStatement::Grant(stmt) =
            parse_valid("GRANT MATCH ON GRAPH g NODES Person TO PUBLIC")
        else {
            panic!("expected Grant");
        };
        assert_ne!(stmt.span, Span::DUMMY);
    }

    #[test]
    fn validate_accepts_grant_program() {
        let program =
            parser::parse("GRANT MATCH ON GRAPH g NODES Person TO PUBLIC").expect("parse");
        crate::validate::validate(&program).expect("generic validation must accept GRANT");
    }

    #[test]
    fn validate_accepts_prepared_query_publication_program() {
        let program = parser::parse("GRANT EXECUTE ON PREPARED QUERY q TO PUBLIC").expect("parse");
        crate::validate::validate(&program)
            .expect("generic validation must accept EXECUTE publication");
    }
}

#[cfg(all(test, not(feature = "gleaph")))]
mod grant_feature_boundary_tests {
    use crate::parser;

    #[test]
    fn grant_rejected_without_gleaph_feature() {
        assert!(parser::parse("GRANT MATCH ON GRAPH g NODES Person TO PUBLIC").is_err());
    }

    #[test]
    fn revoke_rejected_without_gleaph_feature() {
        assert!(parser::parse("REVOKE MATCH ON GRAPH g NODES Person FROM PUBLIC").is_err());
    }

    #[test]
    fn standard_gql_still_parses_without_gleaph_feature() {
        parser::parse("MATCH (n:Person) RETURN n").expect("standard GQL parses");
    }
}
