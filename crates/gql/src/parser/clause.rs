//! Common clause parsers: WHERE, ORDER BY, LIMIT, OFFSET, GROUP BY, HAVING,
//! RETURN, YIELD.

use crate::Value;
use crate::ast::{
    Expr, ExprKind, GroupByClause, LimitClause, NullOrder, OffsetClause, OrderByClause, ReturnBody,
    ReturnItem, ReturnStatement, SetQuantifier, SortDirection, SortItem, YieldItem,
};
use crate::error::GqlError;
use crate::parser::helpers::Parser;
use crate::token::Token;

impl Parser<'_> {
    // ── WHERE ────────────────────────────────────────────────────────────

    /// Parses the search condition after the `WHERE` keyword has been consumed.
    pub fn parse_where_clause(&mut self) -> Result<Expr, GqlError> {
        self.parse_expr()
    }

    // ── ORDER BY ─────────────────────────────────────────────────────────

    /// Parses the sort specification list after `ORDER BY` has been consumed.
    pub fn parse_order_by_clause(&mut self) -> Result<OrderByClause, GqlError> {
        let start = self.save();
        let items = self.comma_list(Self::parse_sort_item)?;
        Ok(OrderByClause {
            span: self.span_since(start),
            items,
        })
    }

    /// Parses a single sort specification: expr [ASC|DESC] [NULLS FIRST|LAST].
    fn parse_sort_item(&mut self) -> Result<SortItem, GqlError> {
        let start = self.save();
        let expr = self.parse_expr()?;

        let direction = if self.eat_keyword("ASC") {
            Some(SortDirection::Asc)
        } else if self.eat_keyword("ASCENDING") {
            Some(SortDirection::Ascending)
        } else if self.eat_keyword("DESC") {
            Some(SortDirection::Desc)
        } else if self.eat_keyword("DESCENDING") {
            Some(SortDirection::Descending)
        } else {
            None
        };

        let null_order = if self.eat_keyword("NULLS") {
            if self.eat_keyword("FIRST") {
                Some(NullOrder::First)
            } else if self.eat_keyword("LAST") {
                Some(NullOrder::Last)
            } else {
                return Err(self.expected("'FIRST' or 'LAST' after NULLS"));
            }
        } else {
            None
        };

        Ok(SortItem {
            span: self.span_since(start),
            expr,
            direction,
            null_order,
        })
    }

    // ── LIMIT ────────────────────────────────────────────────────────────

    /// Parses the count expression after `LIMIT` has been consumed.
    /// Accepts an unsigned integer literal or a parameter reference.
    pub fn parse_limit_clause(&mut self) -> Result<LimitClause, GqlError> {
        let start = self.save();
        let count = self.parse_limit_offset_value()?;
        Ok(LimitClause {
            span: self.span_since(start),
            count,
        })
    }

    // ── OFFSET ───────────────────────────────────────────────────────────

    /// Parses the count expression after `OFFSET` or `SKIP` has been consumed.
    /// Accepts an unsigned integer literal or a parameter reference.
    pub fn parse_offset_clause(&mut self, skip_keyword: bool) -> Result<OffsetClause, GqlError> {
        let start = self.save();
        let count = self.parse_limit_offset_value()?;
        Ok(OffsetClause {
            span: self.span_since(start),
            count,
            skip_keyword,
        })
    }

    /// Shared helper for LIMIT/OFFSET value: unsigned integer or parameter.
    fn parse_limit_offset_value(&mut self) -> Result<Expr, GqlError> {
        let start = self.save();
        match self.peek() {
            Some(Token::Int(v)) if *v >= 0 => {
                let v = *v;
                self.advance();
                Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Literal(Value::Int64(v)),
                })
            }
            Some(Token::Param(_)) => {
                if let Token::Param(name) = self.advance().clone() {
                    Ok(Expr {
                        span: self.span_since(start),
                        kind: ExprKind::Parameter(format!("${name}")),
                    })
                } else {
                    unreachable!()
                }
            }
            // $$param is for object references only, not value contexts.
            _ => Err(self.expected("unsigned integer or parameter")),
        }
    }

    // ── GROUP BY ─────────────────────────────────────────────────────────

    /// Parses the grouping list after `GROUP BY` has been consumed.
    /// Handles comma-separated expressions, or an empty grouping set `()`.
    pub fn parse_group_by_clause(&mut self) -> Result<GroupByClause, GqlError> {
        let start = self.save();
        // Empty grouping set: ()
        if self.at_token(&Token::LParen) && matches!(self.peek_ahead(1), Some(Token::RParen)) {
            self.advance(); // (
            self.advance(); // )
            return Ok(GroupByClause {
                span: self.span_since(start),
                items: vec![],
            });
        }

        let items = self.comma_list(Self::parse_expr)?;
        Ok(GroupByClause {
            span: self.span_since(start),
            items,
        })
    }

    // ── HAVING ───────────────────────────────────────────────────────────

    /// Parses the search condition after `HAVING` has been consumed.
    pub fn parse_having_clause(&mut self) -> Result<Expr, GqlError> {
        self.parse_expr()
    }

    // ── RETURN ───────────────────────────────────────────────────────────

    /// Parses the body of a RETURN statement after `RETURN` has been consumed.
    pub fn parse_return_statement(&mut self) -> Result<ReturnStatement, GqlError> {
        let start = self.save();
        // Optional set quantifier: DISTINCT or ALL.
        let set_quantifier = if self.eat_keyword("DISTINCT") {
            SetQuantifier::Distinct
        } else if self.eat_keyword("ALL") {
            SetQuantifier::All
        } else {
            SetQuantifier::None
        };

        // RETURN * — return all bindings.
        if self.eat_token(&Token::Star) {
            return Ok(ReturnStatement {
                span: self.span_since(start),
                set_quantifier,
                body: ReturnBody::Star,
            });
        }

        // RETURN NO BINDINGS — cypher extension (not in GQL)
        #[cfg(feature = "cypher")]
        if self.at_keyword("NO") && self.at_keyword_ahead(1, "BINDINGS") {
            self.advance(); // NO
            self.advance(); // BINDINGS
            return Ok(ReturnStatement {
                span: self.span_since(start),
                set_quantifier,
                body: ReturnBody::NoBindings,
            });
        }

        // Comma-separated return items.
        let items = self.comma_list(Self::parse_return_item)?;

        // Optional GROUP BY clause (§16.15).
        let group_by = if self.at_keyword("GROUP") && self.at_keyword_ahead(1, "BY") {
            self.advance(); // GROUP
            self.advance(); // BY
            Some(self.parse_group_by_clause()?)
        } else {
            None
        };

        // Optional HAVING clause.
        let having = if self.eat_keyword("HAVING") {
            Some(self.parse_having_clause()?)
        } else {
            None
        };

        // Optional ORDER BY, OFFSET, LIMIT.
        let (order_by, offset, limit) = self.parse_order_by_and_page()?;

        Ok(ReturnStatement {
            span: self.span_since(start),
            set_quantifier,
            body: ReturnBody::Items {
                items,
                group_by,
                having,
                order_by,
                limit,
                offset,
            },
        })
    }

    /// Parses a single return item: expression [AS alias].
    pub fn parse_return_item(&mut self) -> Result<ReturnItem, GqlError> {
        let start = self.save();
        let expr = self.parse_expr()?;
        let alias = if self.eat_keyword("AS") {
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(ReturnItem {
            span: self.span_since(start),
            expr,
            alias,
        })
    }

    // ── YIELD ────────────────────────────────────────────────────────────

    /// Parses a comma-separated list of yield items after `YIELD` has been
    /// consumed.
    pub fn parse_yield_clause(&mut self) -> Result<Vec<YieldItem>, GqlError> {
        self.comma_list(Self::parse_yield_item)
    }

    /// Parses a single yield item: name [AS alias].
    fn parse_yield_item(&mut self) -> Result<YieldItem, GqlError> {
        let start = self.save();
        let name = self.expect_ident()?;
        let alias = if self.eat_keyword("AS") {
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(YieldItem {
            span: self.span_since(start),
            name,
            alias,
        })
    }

    // ── ORDER BY + page (combined) ───────────────────────────────────────

    /// Tries to parse an optional ORDER BY, then optional OFFSET/SKIP, then
    /// optional LIMIT. Returns a tuple of `(order_by, offset, limit)`.
    #[allow(clippy::type_complexity)]
    pub fn parse_order_by_and_page(
        &mut self,
    ) -> Result<
        (
            Option<OrderByClause>,
            Option<OffsetClause>,
            Option<LimitClause>,
        ),
        GqlError,
    > {
        let order_by = if self.at_keyword("ORDER") && self.at_keyword_ahead(1, "BY") {
            self.advance(); // ORDER
            self.advance(); // BY
            Some(self.parse_order_by_clause()?)
        } else {
            None
        };

        // OFFSET/SKIP and LIMIT can appear in either order.
        let mut offset = None;
        let mut limit = None;

        for _ in 0..2 {
            if offset.is_none() && (self.at_keyword("OFFSET") || self.at_keyword("SKIP")) {
                let skip_keyword = self.at_keyword("SKIP");
                self.advance();
                offset = Some(self.parse_offset_clause(skip_keyword)?);
            } else if limit.is_none() && self.eat_keyword("LIMIT") {
                limit = Some(self.parse_limit_clause()?);
            } else {
                break;
            }
        }

        Ok((order_by, offset, limit))
    }
}
