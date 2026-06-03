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
            let property = self.expect_ident()?;
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
            let property = self.expect_ident()?;
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
