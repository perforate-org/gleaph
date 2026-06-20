//! Expression parser — Pratt (precedence-climbing) parser for GQL expressions.

use crate::Value;
use crate::ast::{
    AggregateFunc, BinaryOp, CmpOp, DurationQualifier, Expr, ExprKind, Keyword, LetBinding,
    NormalForm, ObjectName, StringFoldKind, TrimSpec, TruthValue, UnaryOp, WhenClause,
};
use crate::error::GqlError;
use crate::token::Token;
use crate::types::Decimal;

use super::helpers::Parser;
#[cfg(feature = "cypher")]
use crate::ast::StringPredicateKind;

// ════════════════════════════════════════════════════════════════════════════════
// Precedence levels (lowest → highest)
// ════════════════════════════════════════════════════════════════════════════════

/// Binding powers for Pratt parsing. Higher means tighter binding.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prec {
    None = 0,
    Or = 1,      // OR, XOR
    And = 2,     // AND
    Not = 3,     // NOT (prefix)
    Is = 4,      // IS NULL, IS TYPED, comparisons, IN, LIKE, etc.
    Concat = 5,  // ||
    Add = 6,     // +, -
    Mul = 7,     // *, /, %
    Unary = 8,   // unary +, -
    Postfix = 9, // .prop, IS LABELED, [index]
}

// ════════════════════════════════════════════════════════════════════════════════
// Expression parsing
// ════════════════════════════════════════════════════════════════════════════════

impl Parser<'_> {
    /// Parses a GQL expression.
    pub fn parse_expr(&mut self) -> Result<Expr, GqlError> {
        self.parse_expr_prec(Prec::None)
    }

    /// Pratt parser core: parse an expression with the given minimum precedence.
    ///
    /// Guards recursion depth so deeply nested expressions (e.g. thousands of
    /// parentheses or `NOT`/unary chains) fail with a bounded parse error
    /// instead of overflowing the stack.
    fn parse_expr_prec(&mut self, min_prec: Prec) -> Result<Expr, GqlError> {
        self.recurse(|p| p.parse_expr_prec_inner(min_prec))
    }

    fn parse_expr_prec_inner(&mut self, min_prec: Prec) -> Result<Expr, GqlError> {
        let start = self.save();

        // ── Prefix operators ────────────────────────────────────────────
        let mut lhs = if self.at_keyword("NOT") && min_prec <= Prec::Not {
            self.advance();
            let inner = self.parse_expr_prec(Prec::Not)?;
            Expr {
                span: self.span_since(start),
                kind: ExprKind::Not(Box::new(inner)),
            }
        } else if self.at_token(&Token::Minus) && min_prec <= Prec::Unary {
            self.advance();
            let inner = self.parse_expr_prec(Prec::Unary)?;
            Expr {
                span: self.span_since(start),
                kind: ExprKind::UnaryOp {
                    op: UnaryOp::Neg,
                    expr: Box::new(inner),
                },
            }
        } else if self.at_token(&Token::Plus) && min_prec <= Prec::Unary {
            self.advance();
            let inner = self.parse_expr_prec(Prec::Unary)?;
            Expr {
                span: self.span_since(start),
                kind: ExprKind::UnaryOp {
                    op: UnaryOp::Pos,
                    expr: Box::new(inner),
                },
            }
        } else {
            self.parse_primary()?
        };

        // ── Infix / postfix loop ────────────────────────────────────────
        loop {
            // Property access: expr.prop
            if self.at_token(&Token::Dot) && min_prec <= Prec::Postfix {
                self.advance();
                let property = self.expect_ident()?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::PropertyAccess {
                        expr: Box::new(lhs),
                        property,
                    },
                };
                continue;
            }

            // List index / slice: expr[index] or expr[from..to] (Cypher extension)
            #[cfg(feature = "cypher")]
            if self.at_token(&Token::LBracket) && min_prec <= Prec::Postfix {
                self.advance();
                // Check for slice with optional from
                if self.at_token(&Token::RangeDots) {
                    // expr[..to]
                    self.advance();
                    let to = self.parse_expr()?;
                    self.expect_token(&Token::RBracket)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::ListSlice {
                            list: Box::new(lhs),
                            from: None,
                            to: Some(Box::new(to)),
                        },
                    };
                    continue;
                }
                let idx = self.parse_expr()?;
                if self.eat_token(&Token::RangeDots) {
                    // expr[from..to] or expr[from..]
                    let to = if self.at_token(&Token::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.parse_expr()?))
                    };
                    self.expect_token(&Token::RBracket)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::ListSlice {
                            list: Box::new(lhs),
                            from: Some(Box::new(idx)),
                            to,
                        },
                    };
                } else {
                    self.expect_token(&Token::RBracket)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::ListIndex {
                            list: Box::new(lhs),
                            index: Box::new(idx),
                        },
                    };
                }
                continue;
            }

            // IS postfix predicates
            if self.at_keyword("IS") && min_prec <= Prec::Is {
                let save = self.save();
                self.advance(); // eat IS
                let negated = self.eat_keyword("NOT");

                if self.eat_keyword("NULL") {
                    lhs = if negated {
                        Expr {
                            span: self.span_since(start),
                            kind: ExprKind::IsNotNull(Box::new(lhs)),
                        }
                    } else {
                        Expr {
                            span: self.span_since(start),
                            kind: ExprKind::IsNull(Box::new(lhs)),
                        }
                    };
                    continue;
                }
                if self.eat_keyword("TYPED") {
                    let target = self.parse_value_type()?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsTyped {
                            expr: Box::new(lhs),
                            target,
                            negated,
                        },
                    };
                    continue;
                }
                if self.at_keyword("NFC")
                    || self.at_keyword("NFD")
                    || self.at_keyword("NFKC")
                    || self.at_keyword("NFKD")
                    || self.at_keyword("NORMALIZED")
                {
                    let form = self.parse_normal_form_opt();
                    self.expect_keyword("NORMALIZED")?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsNormalized {
                            expr: Box::new(lhs),
                            form,
                            negated,
                        },
                    };
                    continue;
                }
                if self.eat_keyword("DIRECTED") {
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsDirected {
                            expr: Box::new(lhs),
                            negated,
                        },
                    };
                    continue;
                }
                if self.eat_keyword("LABELED") {
                    let label = self.parse_label_expr()?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsLabeled {
                            expr: Box::new(lhs),
                            label,
                            negated,
                        },
                    };
                    continue;
                }
                if self.eat_keyword("SOURCE") {
                    self.expect_keyword("OF")?;
                    let edge = self.parse_expr_prec(Prec::Is)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsSourceOf {
                            node: Box::new(lhs),
                            edge: Box::new(edge),
                            negated,
                        },
                    };
                    continue;
                }
                if self.eat_keyword("DESTINATION") {
                    self.expect_keyword("OF")?;
                    let edge = self.parse_expr_prec(Prec::Is)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsDestOf {
                            node: Box::new(lhs),
                            edge: Box::new(edge),
                            negated,
                        },
                    };
                    continue;
                }
                // IS [NOT] TRUE / FALSE / UNKNOWN
                if self.eat_keyword("TRUE") {
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsTruth {
                            expr: Box::new(lhs),
                            value: TruthValue::True,
                            negated,
                        },
                    };
                    continue;
                }
                if self.eat_keyword("FALSE") {
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsTruth {
                            expr: Box::new(lhs),
                            value: TruthValue::False,
                            negated,
                        },
                    };
                    continue;
                }
                if self.eat_keyword("UNKNOWN") {
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::IsTruth {
                            expr: Box::new(lhs),
                            value: TruthValue::Unknown,
                            negated,
                        },
                    };
                    continue;
                }

                // Didn't match any IS predicate — restore
                self.restore(save);
            }

            // Colon label predicate: expr :labelExpr (GQL isLabeledExpressionPart2)
            if self.at_token(&Token::Colon) && min_prec <= Prec::Is {
                self.advance(); // eat :
                let label = self.parse_label_expr()?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::IsLabeled {
                        expr: Box::new(lhs),
                        label,
                        negated: false,
                    },
                };
                continue;
            }

            // [NOT] IN (list) — sql-compat extension (not in GQL)
            #[cfg(feature = "sql-compat")]
            if (self.at_keyword("IN") || (self.at_keyword("NOT") && self.at_keyword_ahead(1, "IN")))
                && min_prec <= Prec::Is
            {
                let negated = self.eat_keyword("NOT");
                self.expect_keyword("IN")?;
                self.expect_token(&Token::LParen)?;
                let list = self.comma_list(|p| p.parse_expr())?;
                self.expect_token(&Token::RParen)?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::InList {
                        expr: Box::new(lhs),
                        list,
                        negated,
                    },
                };
                continue;
            }

            // String predicates: [NOT] LIKE / STARTS WITH / ENDS WITH / CONTAINS
            if min_prec <= Prec::Is
                && let Some(pred) = self.try_parse_string_predicate(start, &lhs)?
            {
                lhs = pred;
                continue;
            }

            // OR
            if self.at_keyword("OR") && min_prec <= Prec::Or {
                self.advance();
                let rhs = self.parse_expr_prec(Prec::And)?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Or(Box::new(lhs), Box::new(rhs)),
                };
                continue;
            }

            // XOR
            if self.at_keyword("XOR") && min_prec <= Prec::Or {
                self.advance();
                let rhs = self.parse_expr_prec(Prec::And)?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Xor(Box::new(lhs), Box::new(rhs)),
                };
                continue;
            }

            // AND
            if self.at_keyword("AND") && min_prec <= Prec::And {
                self.advance();
                let rhs = self.parse_expr_prec(Prec::Not)?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::And(Box::new(lhs), Box::new(rhs)),
                };
                continue;
            }

            // Comparison operators: =, <>, <, >, <=, >=
            if min_prec <= Prec::Is
                && let Some(op) = self.peek_cmp_op()
            {
                self.advance();
                let rhs = self.parse_expr_prec(Prec::Concat)?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Compare {
                        left: Box::new(lhs),
                        op,
                        right: Box::new(rhs),
                    },
                };
                continue;
            }

            // Concatenation: ||
            if self.at_token(&Token::Concat) && min_prec <= Prec::Concat {
                self.advance();
                let rhs = self.parse_expr_prec(Prec::Add)?;
                lhs = Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Concat(Box::new(lhs), Box::new(rhs)),
                };
                continue;
            }

            // Addition: +, -
            if min_prec <= Prec::Add {
                if self.at_token(&Token::Plus) {
                    self.advance();
                    let rhs = self.parse_expr_prec(Prec::Mul)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::BinaryOp {
                            left: Box::new(lhs),
                            op: BinaryOp::Add,
                            right: Box::new(rhs),
                        },
                    };
                    continue;
                }
                if self.at_token(&Token::Minus) {
                    self.advance();
                    let rhs = self.parse_expr_prec(Prec::Mul)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::BinaryOp {
                            left: Box::new(lhs),
                            op: BinaryOp::Sub,
                            right: Box::new(rhs),
                        },
                    };
                    continue;
                }
            }

            // Multiplication: *, /, %
            if min_prec <= Prec::Mul {
                if self.at_token(&Token::Star) {
                    self.advance();
                    let rhs = self.parse_expr_prec(Prec::Unary)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::BinaryOp {
                            left: Box::new(lhs),
                            op: BinaryOp::Mul,
                            right: Box::new(rhs),
                        },
                    };
                    continue;
                }
                if self.at_token(&Token::Slash) {
                    self.advance();
                    let rhs = self.parse_expr_prec(Prec::Unary)?;
                    lhs = Expr {
                        span: self.span_since(start),
                        kind: ExprKind::BinaryOp {
                            left: Box::new(lhs),
                            op: BinaryOp::Div,
                            right: Box::new(rhs),
                        },
                    };
                    continue;
                }
                // `%` operator removed: conflicts with GQL label wildcard.
                // Use MOD(a, b) function instead.
            }

            // No more infix/postfix matched — break.
            break;
        }

        Ok(lhs)
    }

    // ════════════════════════════════════════════════════════════════════════
    // Primary expression
    // ════════════════════════════════════════════════════════════════════════

    fn parse_primary(&mut self) -> Result<Expr, GqlError> {
        let start = self.save();
        match self.peek() {
            // ── Integer literal ──────────────────────────────────────────
            Some(Token::Int(v)) => {
                let v = *v;
                self.advance();
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Literal(Value::Int64(v)) })
            }

            // ── BigInt literal ───────────────────────────────────────────
            Some(Token::BigInt(s)) => {
                let s = s.clone();
                self.advance();
                self.parse_bigint_literal(start, &s)
            }

            // ── Float literal ────────────────────────────────────────────
            Some(Token::Float(v)) => {
                let v = *v;
                self.advance();
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Literal(Value::Float64(v)) })
            }

            // ── Exact numeric (M suffix) ─────────────────────────────────
            Some(Token::ExactNumeric(s)) => {
                let s = s.clone();
                self.advance();
                let d = Decimal::parse(&s)
                    .ok_or_else(|| self.error(format!("invalid exact numeric: {s}M")))?;
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Literal(Value::Decimal(d)) })
            }

            // ── String literal ───────────────────────────────────────────
            Some(Token::StringLit(s)) => {
                let s = s.clone();
                self.advance();
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Literal(Value::Text(s)) })
            }

            // ── Bytes literal ────────────────────────────────────────────
            Some(Token::BytesLit(b)) => {
                let b = b.clone();
                self.advance();
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Literal(Value::Bytes(b)) })
            }

            // ── Parameter ($name) — GQL standard generalParameterReference ──
            Some(Token::Param(s)) => {
                let s = s.clone();
                self.advance();
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Parameter(format!("${s}")) })
            }

            // ── Substituted parameter ($$name) — GQL standard, but only for
            // object references (graph, schema, type, procedure), not value
            // expressions. Reject in expression context.
            Some(Token::SubstitutedParam(_)) => {
                Err(self.expected("expression ($$param is only valid for graph/schema references, use $param for values)"))
            }

            // ── Parenthesized expression ─────────────────────────────────
            Some(Token::LParen) => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                Ok(Expr { span: self.span_since(start), kind: ExprKind::Paren(Box::new(expr)) })
            }

            // ── List literal: [ expr, ... ] ──────────────────────────────
            Some(Token::LBracket) => {
                self.advance();
                let items = if self.at_token(&Token::RBracket) {
                    vec![]
                } else {
                    self.comma_list(|p| p.parse_expr())?
                };
                self.expect_token(&Token::RBracket)?;
                Ok(Expr { span: self.span_since(start), kind: ExprKind::ListLiteral(items) })
            }

            // ── Record literal: { key: val, ... } ───────────────────────
            Some(Token::LBrace) => self.parse_record_literal(start, false),

            // ── Keyword-initiated expressions ────────────────────────────
            Some(Token::Ident(_)) | Some(Token::QuotedIdent(_)) => {
                self.parse_keyword_or_var_primary(start)
            }

            _ => Err(self.expected("expression")),
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Keyword-initiated or variable-reference primaries
    // ════════════════════════════════════════════════════════════════════════

    fn parse_keyword_or_var_primary(&mut self, start: usize) -> Result<Expr, GqlError> {
        // TRUE / FALSE / UNKNOWN
        if self.at_keyword("TRUE") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Bool(true)),
            });
        }
        if self.at_keyword("FALSE") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Bool(false)),
            });
        }
        if self.at_keyword("UNKNOWN") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Null),
            }); // UNKNOWN ≈ NULL in GQL three-valued logic
        }
        if self.at_keyword("NULL") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Null),
            });
        }

        // LIST[...] or ARRAY[...]
        if (self.at_keyword("LIST") || self.at_keyword("ARRAY"))
            && matches!(self.peek_ahead(1), Some(Token::LBracket))
        {
            let keyword = Keyword::new(self.current_ident_upper());
            self.advance(); // eat LIST/ARRAY
            self.advance(); // eat [
            let items = if self.at_token(&Token::RBracket) {
                vec![]
            } else {
                self.comma_list(|p| p.parse_expr())?
            };
            self.expect_token(&Token::RBracket)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::ListConstructor { keyword, items },
            });
        }

        // RECORD { ... }
        if self.at_keyword("RECORD") && matches!(self.peek_ahead(1), Some(Token::LBrace)) {
            self.advance(); // eat RECORD
            return self.parse_record_literal(start, true);
        }

        // PATH[ ... ]
        if self.at_keyword("PATH") && matches!(self.peek_ahead(1), Some(Token::LBracket)) {
            self.advance(); // eat PATH
            self.advance(); // eat [
            let items = if self.at_token(&Token::RBracket) {
                vec![]
            } else {
                self.comma_list(|p| p.parse_expr())?
            };
            self.expect_token(&Token::RBracket)?;
            if items.is_empty() || items.len() % 2 == 0 {
                return Err(self.expected(
                    "PATH constructor with an odd number of elements starting with a node",
                ));
            }
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::PathConstructor { elements: items },
            });
        }

        // CASE expression
        if self.at_keyword("CASE") {
            return self.parse_case_expr(start);
        }

        // CAST(expr AS type)
        if self.at_keyword("CAST") {
            return self.parse_cast_expr(start);
        }

        // COALESCE(expr, ...)
        if self.at_keyword("COALESCE") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let args = self.comma_list(|p| p.parse_expr())?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Coalesce(args),
            });
        }

        // NULLIF(expr, expr)
        if self.at_keyword("NULLIF") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::NullIf(Box::new(a), Box::new(b)),
            });
        }

        // EXISTS { pattern | subquery }
        if self.at_keyword("EXISTS") {
            return self.parse_exists_expr(start);
        }

        // Aggregate functions
        if let Some(agg) = self.try_parse_aggregate(start)? {
            return Ok(agg);
        }

        // Numeric functions
        if let Some(func) = self.try_parse_numeric_function(start)? {
            return Ok(func);
        }

        // String functions
        if let Some(func) = self.try_parse_string_function(start)? {
            return Ok(func);
        }

        // Size / cardinality
        if self.at_keyword("SIZE") || self.at_keyword("CARDINALITY") {
            let keyword = Keyword::new(self.current_ident_upper());
            self.advance();
            self.expect_token(&Token::LParen)?;
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Cardinality {
                    keyword,
                    expr: Box::new(arg),
                },
            });
        }

        // PATH_LENGTH(expr)
        if self.at_keyword("PATH_LENGTH") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::PathLength(Box::new(arg)),
            });
        }

        // ELEMENTS(expr)
        if self.at_keyword("ELEMENTS") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Elements(Box::new(arg)),
            });
        }

        // Cypher-compat: NODES(expr), EDGES(expr), LABELS(expr), LABEL(expr),
        // SOURCE(expr), DESTINATION(expr)
        #[cfg(feature = "cypher")]
        {
            if self.at_keyword("NODES") {
                self.advance();
                self.expect_token(&Token::LParen)?;
                let arg = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Nodes(Box::new(arg)),
                });
            }
            if self.at_keyword("EDGES") {
                self.advance();
                self.expect_token(&Token::LParen)?;
                let arg = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Edges(Box::new(arg)),
                });
            }
            if self.at_keyword("LABELS") {
                self.advance();
                self.expect_token(&Token::LParen)?;
                let arg = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Labels(Box::new(arg)),
                });
            }
            if self.at_keyword("LABEL") {
                self.advance();
                self.expect_token(&Token::LParen)?;
                let arg = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Label(Box::new(arg)),
                });
            }
            if self.at_keyword("SOURCE") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                self.advance();
                self.expect_token(&Token::LParen)?;
                let arg = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Source(Box::new(arg)),
                });
            }
            if self.at_keyword("DESTINATION") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                self.advance();
                self.expect_token(&Token::LParen)?;
                let arg = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Destination(Box::new(arg)),
                });
            }
        }

        // ── Datetime constants (GQL §20.25) ──
        // These are bare keywords with NO parentheses.
        // CURRENT_DATE, CURRENT_TIME, CURRENT_TIMESTAMP: always bare.
        // LOCAL_TIME: bare or with parens (handled in try_parse_datetime_function).
        // LOCAL_TIMESTAMP: always bare.
        // LOCAL_DATETIME: always requires parens (handled in try_parse_datetime_function).
        if self.at_keyword("CURRENT_DATE") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CurrentDate,
            });
        }
        if self.at_keyword("CURRENT_TIME") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CurrentTime,
            });
        }
        if self.at_keyword("CURRENT_TIMESTAMP") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CurrentTimestamp,
            });
        }
        if self.at_keyword("LOCAL_TIME") && !matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CurrentLocalTime,
            });
        }
        if self.at_keyword("LOCAL_TIMESTAMP") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CurrentLocalTimestamp,
            });
        }
        // ZONED_TIME / ZONED_DATETIME require parens in GQL.
        if self.at_keyword("ZONED_TIME") && !matches!(self.peek_ahead(1), Some(Token::LParen)) {
            return Err(self.expected("'(' after ZONED_TIME"));
        }
        if self.at_keyword("ZONED_DATETIME") && !matches!(self.peek_ahead(1), Some(Token::LParen)) {
            return Err(self.expected("'(' after ZONED_DATETIME"));
        }
        // LOCAL_DATETIME requires parens: LOCAL_DATETIME(args...).
        // Bare LOCAL_DATETIME is not valid GQL. Consume to prevent
        // fallback to variable/function-call.
        if self.at_keyword("LOCAL_DATETIME") && !matches!(self.peek_ahead(1), Some(Token::LParen)) {
            return Err(self.expected("'(' after LOCAL_DATETIME"));
        }

        // Datetime constructors with parens
        if let Some(dt) = self.try_parse_datetime_function(start)? {
            return Ok(dt);
        }

        // SESSION_USER
        if self.at_keyword("SESSION_USER") {
            self.advance();
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::SessionUser,
            });
        }

        // ELEMENT_ID(expr)
        if self.at_keyword("ELEMENT_ID") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::ElementId(Box::new(arg)),
            });
        }

        // ALL_DIFFERENT(expr, ...)
        if self.at_keyword("ALL_DIFFERENT") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let args = self.comma_list(|p| p.parse_expr())?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::AllDifferent(args),
            });
        }

        // SAME(expr, ...)
        if self.at_keyword("SAME") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let args = self.comma_list(|p| p.parse_expr())?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Same(args),
            });
        }

        // PROPERTY_EXISTS(expr, property)
        if self.at_keyword("PROPERTY_EXISTS") {
            self.advance();
            self.expect_token(&Token::LParen)?;
            let expr = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let property = self.expect_ident()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::PropertyExists {
                    expr: Box::new(expr),
                    property,
                },
            });
        }

        // LET binding IN expr END
        if self.at_keyword("LET") {
            return self.parse_let_expr(start);
        }

        // VALUE { subquery }
        if self.at_keyword("VALUE") && matches!(self.peek_ahead(1), Some(Token::LBrace)) {
            self.advance(); // eat VALUE
            self.advance(); // eat {
            let query = self.parse_composite_query_expr()?;
            self.expect_token(&Token::RBrace)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::ValueSubquery(Box::new(query)),
            });
        }

        // ── Variable reference (identifier) or dotted generic call `a.b(...)` ──
        // This is the catch-all — any unquoted identifier that doesn't match
        // a keyword above, or any quoted identifier.
        if self.at_ident() {
            let save = self.save();
            let fname = self.parse_object_name()?;
            if self.at_token(&Token::LParen) {
                return self.parse_generic_function_call_named(start, fname);
            }
            self.restore(save);
            let name = self.expect_ident()?;
            if self.at_token(&Token::LParen) {
                return self.parse_generic_function_call(start, name);
            }
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Variable(name),
            });
        }

        Err(self.expected("expression"))
    }

    // ════════════════════════════════════════════════════════════════════════
    // Helpers
    // ════════════════════════════════════════════════════════════════════════

    /// Parse a BigInt literal string into the smallest integer type that fits.
    fn parse_bigint_literal(&self, start: usize, s: &str) -> Result<Expr, GqlError> {
        // Try i128 first
        if let Ok(v) = s.parse::<i128>() {
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Int128(v)),
            });
        }
        // Try u128
        if let Ok(v) = s.parse::<u128>() {
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Uint128(v)),
            });
        }
        // Try Int256
        if let Some(v) = crate::types::Int256::parse(s) {
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Int256(v)),
            });
        }
        // Try Uint256
        if let Some(v) = crate::types::Uint256::parse(s) {
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Literal(Value::Uint256(v)),
            });
        }
        Err(self.error(format!("integer literal too large: {s}")))
    }

    /// Parse a record literal: { key: val, key: val, ... }
    fn parse_record_literal(&mut self, start: usize, keyworded: bool) -> Result<Expr, GqlError> {
        self.expect_token(&Token::LBrace)?;
        let mut fields = Vec::new();
        if !self.at_token(&Token::RBrace) {
            loop {
                let key = self.expect_ident()?;
                self.expect_token(&Token::Colon)?;
                let val = self.parse_expr()?;
                fields.push((key, val));
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
        }
        self.expect_token(&Token::RBrace)?;
        if keyworded {
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::RecordConstructor(fields),
            })
        } else {
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::RecordLiteral(fields),
            })
        }
    }

    /// Parse a CASE expression (simple or searched).
    fn parse_case_expr(&mut self, start: usize) -> Result<Expr, GqlError> {
        self.expect_keyword("CASE")?;

        // Determine if this is a simple or searched CASE.
        // Simple: CASE expr WHEN val THEN result ...
        // Searched: CASE WHEN cond THEN result ...
        if self.at_keyword("WHEN") {
            // Searched CASE
            let mut when_clauses = Vec::new();
            while self.at_keyword("WHEN") {
                let when_start = self.save();
                self.advance(); // WHEN
                let condition = self.parse_expr()?;
                self.expect_keyword("THEN")?;
                let result = self.parse_expr()?;
                when_clauses.push(WhenClause {
                    span: self.span_since(when_start),
                    condition,
                    result,
                });
            }
            let else_clause = if self.eat_keyword("ELSE") {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_keyword("END")?;
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CaseSearched {
                    when_clauses,
                    else_clause,
                },
            })
        } else {
            // Simple CASE
            let operand = Box::new(self.parse_expr()?);
            let mut when_clauses = Vec::new();
            while self.at_keyword("WHEN") {
                let when_start = self.save();
                self.advance(); // WHEN
                let condition = self.parse_expr()?;
                self.expect_keyword("THEN")?;
                let result = self.parse_expr()?;
                when_clauses.push(WhenClause {
                    span: self.span_since(when_start),
                    condition,
                    result,
                });
            }
            let else_clause = if self.eat_keyword("ELSE") {
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_keyword("END")?;
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::CaseSimple {
                    operand,
                    when_clauses,
                    else_clause,
                },
            })
        }
    }

    /// Parse CAST(expr AS value_type).
    fn parse_cast_expr(&mut self, start: usize) -> Result<Expr, GqlError> {
        self.expect_keyword("CAST")?;
        self.expect_token(&Token::LParen)?;
        let expr = self.parse_expr()?;
        self.expect_keyword("AS")?;
        let target = self.parse_value_type()?;
        self.expect_token(&Token::RParen)?;
        Ok(Expr {
            span: self.span_since(start),
            kind: ExprKind::Cast {
                expr: Box::new(expr),
                target,
            },
        })
    }

    /// Parse EXISTS expression (GQL §20.13 `existsPredicate`).
    ///
    /// Grammar alternatives:
    /// - `EXISTS { graphPattern }` or `EXISTS ( graphPattern )`
    /// - `EXISTS { matchStatementBlock }` or `EXISTS ( matchStatementBlock )`
    /// - `EXISTS { procedureBody }` — full subquery with RETURN (braces only)
    ///
    /// Parenthesized forms allow graph patterns and match-only blocks.
    /// Only braces allow a full subquery (with RETURN).
    fn parse_exists_expr(&mut self, start: usize) -> Result<Expr, GqlError> {
        self.expect_keyword("EXISTS")?;

        let (close_token, is_braces) = if self.eat_token(&Token::LBrace) {
            (Token::RBrace, true)
        } else if self.eat_token(&Token::LParen) {
            (Token::RParen, false)
        } else {
            return Err(self.expected("'{' or '(' after EXISTS"));
        };

        if self.at_keyword("MATCH") || self.at_keyword("OPTIONAL") {
            if is_braces {
                // Braces: allow full subquery (procedureBody) — MATCH...RETURN is OK.
                let query = self.parse_composite_query_expr()?;
                self.expect_token(&close_token)?;
                Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::ExistsSubquery(Box::new(query)),
                })
            } else {
                // Parentheses: match statement block only (no RETURN).
                // Parse MATCH statements but stop at RETURN.
                let mut matches = Vec::new();
                while self.at_keyword("MATCH") || self.at_keyword("OPTIONAL") {
                    matches.push(self.parse_match_statement()?);
                }
                self.expect_token(&close_token)?;
                // Wrap match-only block as a subquery with no RETURN.
                let linear = crate::ast::LinearQueryStatement {
                    span: self.span_since(start),
                    at_schema: None,
                    prefix_bindings: vec![],
                    parts: matches
                        .into_iter()
                        .map(crate::ast::SimpleQueryStatement::Match)
                        .collect(),
                    result: None,
                };
                let query = crate::ast::CompositeQueryExpr {
                    span: self.span_since(start),
                    left: linear,
                    rest: vec![],
                };
                Ok(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::ExistsSubquery(Box::new(query)),
                })
            }
        } else {
            let pattern = self.parse_graph_pattern()?;
            self.expect_token(&close_token)?;
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::ExistsPattern(Box::new(pattern)),
            })
        }
    }

    /// Parse LET var = expr [, var = expr]... IN expr END.
    fn parse_let_expr(&mut self, start: usize) -> Result<Expr, GqlError> {
        self.expect_keyword("LET")?;
        let mut bindings = Vec::new();
        loop {
            let bind_start = self.save();
            let variable = self.expect_ident()?;
            self.expect_token(&Token::Eq)?;
            // Parse binding value, but stop before bare `IN` (which is the
            // LET..IN delimiter). We use parse_let_binding_value which avoids
            // treating `IN` as an infix operator when not followed by `(`.
            let value = self.parse_let_binding_value()?;
            bindings.push(LetBinding {
                span: self.span_since(bind_start),
                variable,
                value,
            });
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        self.expect_keyword("IN")?;
        let expr = self.parse_expr()?;
        self.expect_keyword("END")?;
        Ok(Expr {
            span: self.span_since(start),
            kind: ExprKind::LetIn {
                bindings,
                expr: Box::new(expr),
            },
        })
    }

    /// Parse a LET binding value expression. This is like `parse_expr` but
    /// treats a bare `IN` keyword (not followed by `(`) as a terminator
    /// rather than an infix operator.
    fn parse_let_binding_value(&mut self) -> Result<Expr, GqlError> {
        // Parse at the same precedence level as normal, but we need to
        // intercept the IN keyword. The simplest approach: parse at a
        // precedence level above Is (which handles IN) so that IN is not
        // treated as infix.
        self.parse_expr_prec(Prec::Concat)
    }

    /// Parse a generic function call: `name(args...)` or `schema.fn(args...)`.
    fn parse_generic_function_call(
        &mut self,
        start: usize,
        name: String,
    ) -> Result<Expr, GqlError> {
        self.parse_generic_function_call_named(start, ObjectName::simple(name))
    }

    fn parse_generic_function_call_named(
        &mut self,
        start: usize,
        name: ObjectName,
    ) -> Result<Expr, GqlError> {
        self.expect_token(&Token::LParen)?;
        let distinct = self.eat_keyword("DISTINCT");
        let args = if self.at_token(&Token::RParen) {
            vec![]
        } else {
            self.comma_list(|p| p.parse_expr())?
        };
        self.expect_token(&Token::RParen)?;
        Ok(Expr {
            span: self.span_since(start),
            kind: ExprKind::FunctionCall {
                name,
                args,
                distinct,
            },
        })
    }

    /// Try to parse a comparison operator, returning it without consuming.
    fn peek_cmp_op(&self) -> Option<CmpOp> {
        match self.peek() {
            Some(Token::Eq) => Some(CmpOp::Eq),
            Some(Token::Ne) => Some(CmpOp::Ne),
            Some(Token::Lt) => Some(CmpOp::Lt),
            Some(Token::Gt) => Some(CmpOp::Gt),
            Some(Token::Le) => Some(CmpOp::Le),
            Some(Token::Ge) => Some(CmpOp::Ge),
            _ => None,
        }
    }

    /// Parse an optional normalization form keyword (NFC, NFD, NFKC, NFKD).
    /// Defaults to NFC if not specified.
    fn parse_normal_form_opt(&mut self) -> NormalForm {
        if self.eat_keyword("NFC") {
            NormalForm::NFC
        } else if self.eat_keyword("NFD") {
            NormalForm::NFD
        } else if self.eat_keyword("NFKC") {
            NormalForm::NFKC
        } else if self.eat_keyword("NFKD") {
            NormalForm::NFKD
        } else {
            NormalForm::NFC
        }
    }

    /// Try to parse a string predicate ([NOT] CONTAINS, [NOT] ILIKE).
    fn try_parse_string_predicate(
        &mut self,
        start: usize,
        lhs: &Expr,
    ) -> Result<Option<Expr>, GqlError> {
        let save = self.save();
        let negated = self.eat_keyword("NOT");

        #[allow(unused_mut)]
        let mut kind = None;

        #[cfg(feature = "cypher")]
        if kind.is_none() {
            if self.eat_keyword("ILIKE") {
                kind = Some(StringPredicateKind::ILike);
            } else if self.eat_keyword("CONTAINS") {
                kind = Some(StringPredicateKind::Contains);
            } else if self.at_keyword("STARTS") && self.at_keyword_ahead(1, "WITH") {
                self.advance(); // STARTS
                self.advance(); // WITH
                kind = Some(StringPredicateKind::StartsWith);
            } else if self.at_keyword("ENDS") && self.at_keyword_ahead(1, "WITH") {
                self.advance(); // ENDS
                self.advance(); // WITH
                kind = Some(StringPredicateKind::EndsWith);
            }
        }

        if let Some(kind) = kind {
            let pattern = self.parse_expr_prec(Prec::Concat)?;
            Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::StringPredicate {
                    expr: Box::new(lhs.clone()),
                    kind,
                    pattern: Box::new(pattern),
                    negated,
                },
            }))
        } else {
            self.restore(save);
            Ok(None)
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Aggregate functions
    // ════════════════════════════════════════════════════════════════════════

    /// Try to parse an aggregate function call. Returns `None` if the current
    /// token is not an aggregate keyword.
    fn try_parse_aggregate(&mut self, start: usize) -> Result<Option<Expr>, GqlError> {
        // COUNT is special: COUNT(*) vs COUNT([DISTINCT] expr)
        if self.at_keyword("COUNT") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance(); // eat (
            if self.eat_token(&Token::Star) {
                self.expect_token(&Token::RParen)?;
                return Ok(Some(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Aggregate {
                        func: AggregateFunc::CountStar,
                        expr: None,
                        expr2: None,
                        distinct: false,
                        order_by: None,
                        filter: None,
                    },
                }));
            }
            let distinct = self.eat_keyword("DISTINCT");
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Aggregate {
                    func: AggregateFunc::Count,
                    expr: Some(Box::new(arg)),
                    expr2: None,
                    distinct,
                    order_by: None,
                    filter: None,
                },
            }));
        }

        // Other aggregate functions
        let func = if self.at_keyword("SUM") {
            Some(AggregateFunc::Sum)
        } else if self.at_keyword("AVG") {
            Some(AggregateFunc::Avg)
        } else if self.at_keyword("MIN") {
            Some(AggregateFunc::Min)
        } else if self.at_keyword("MAX") {
            Some(AggregateFunc::Max)
        } else if self.at_keyword("COLLECT_LIST") {
            Some(AggregateFunc::Collect)
        } else if self.at_keyword("STDDEV_SAMP") {
            Some(AggregateFunc::StddevSamp)
        } else if self.at_keyword("STDDEV_POP") {
            Some(AggregateFunc::StddevPop)
        } else if self.at_keyword("PERCENTILE_CONT") {
            Some(AggregateFunc::PercentileCont)
        } else if self.at_keyword("PERCENTILE_DISC") {
            Some(AggregateFunc::PercentileDisc)
        } else {
            None
        };

        if let Some(func) = func {
            if !matches!(self.peek_ahead(1), Some(Token::LParen)) {
                return Ok(None);
            }
            let is_binary = matches!(
                func,
                AggregateFunc::PercentileCont | AggregateFunc::PercentileDisc
            );
            self.advance(); // eat function name
            self.advance(); // eat (
            let distinct = self.eat_keyword("DISTINCT");
            let arg = self.parse_expr()?;
            // Binary set functions (PERCENTILE_CONT/DISC) have a second argument.
            let arg2 = if is_binary {
                self.expect_token(&Token::Comma)?;
                Some(Box::new(self.parse_expr()?))
            } else {
                None
            };
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Aggregate {
                    func,
                    expr: Some(Box::new(arg)),
                    expr2: arg2,
                    distinct,
                    order_by: None,
                    filter: None,
                },
            }));
        }

        Ok(None)
    }

    // ════════════════════════════════════════════════════════════════════════
    // Numeric functions
    // ════════════════════════════════════════════════════════════════════════

    /// Try to parse a numeric built-in function.
    fn try_parse_numeric_function(&mut self, start: usize) -> Result<Option<Expr>, GqlError> {
        // Single-argument numeric functions
        macro_rules! unary_fn {
            ($kw:expr, $variant:ident) => {
                if self.at_keyword($kw) && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                    self.advance();
                    self.advance();
                    let arg = self.parse_expr()?;
                    self.expect_token(&Token::RParen)?;
                    return Ok(Some(Expr {
                        span: self.span_since(start),
                        kind: ExprKind::$variant(Box::new(arg)),
                    }));
                }
            };
        }

        unary_fn!("ABS", Abs);
        unary_fn!("FLOOR", Floor);
        unary_fn!("CEIL", Ceil);
        if self.at_keyword("CEILING") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Ceil(Box::new(arg)),
            }));
        }
        unary_fn!("SQRT", Sqrt);
        unary_fn!("EXP", Exp);
        unary_fn!("LN", Ln);
        unary_fn!("LOG10", Log10);
        unary_fn!("SIN", Sin);
        unary_fn!("COS", Cos);
        unary_fn!("TAN", Tan);
        unary_fn!("ASIN", Asin);
        unary_fn!("ACOS", Acos);
        unary_fn!("ATAN", Atan);
        // GQL standard trigonometric: DEGREES, RADIANS, COT, SINH, COSH, TANH
        unary_fn!("DEGREES", Degrees);
        unary_fn!("RADIANS", Radians);
        unary_fn!("COT", Cot);
        unary_fn!("SINH", Sinh);
        unary_fn!("COSH", Cosh);
        unary_fn!("TANH", Tanh);
        // SQL-compat: SIGN
        #[cfg(feature = "sql-compat")]
        {
            unary_fn!("SIGN", Sign);
        }

        // Two-argument numeric functions
        if self.at_keyword("LOG") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Log(Box::new(a), Box::new(b)),
            }));
        }
        if self.at_keyword("POWER") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Power(Box::new(a), Box::new(b)),
            }));
        }
        if self.at_keyword("MOD") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Mod(Box::new(a), Box::new(b)),
            }));
        }
        // SQL-compat: ATAN2, TRUNCATE/TRUNC, ROUND
        #[cfg(feature = "sql-compat")]
        {
            if self.at_keyword("ATAN2") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                self.advance();
                self.advance();
                let a = self.parse_expr()?;
                self.expect_token(&Token::Comma)?;
                let b = self.parse_expr()?;
                self.expect_token(&Token::RParen)?;
                return Ok(Some(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Atan2(Box::new(a), Box::new(b)),
                }));
            }

            if (self.at_keyword("TRUNCATE") || self.at_keyword("TRUNC"))
                && matches!(self.peek_ahead(1), Some(Token::LParen))
            {
                self.advance();
                self.advance();
                let expr = self.parse_expr()?;
                let places = if self.eat_token(&Token::Comma) {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                self.expect_token(&Token::RParen)?;
                return Ok(Some(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Truncate {
                        expr: Box::new(expr),
                        places,
                    },
                }));
            }

            if self.at_keyword("ROUND") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                self.advance();
                self.advance();
                let expr = self.parse_expr()?;
                let places = if self.eat_token(&Token::Comma) {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                self.expect_token(&Token::RParen)?;
                return Ok(Some(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::Round {
                        expr: Box::new(expr),
                        places,
                    },
                }));
            }
        }

        Ok(None)
    }

    // ════════════════════════════════════════════════════════════════════════
    // String functions
    // ════════════════════════════════════════════════════════════════════════

    /// Try to parse a string built-in function.
    fn try_parse_string_function(&mut self, start: usize) -> Result<Option<Expr>, GqlError> {
        // UPPER(expr)
        if self.at_keyword("UPPER") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Upper(Box::new(arg)),
            }));
        }

        // LOWER(expr)
        if self.at_keyword("LOWER") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Lower(Box::new(arg)),
            }));
        }

        // LEFT(expr, n)
        if self.at_keyword("LEFT") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Left(Box::new(a), Box::new(b)),
            }));
        }

        // RIGHT(expr, n)
        if self.at_keyword("RIGHT") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Right(Box::new(a), Box::new(b)),
            }));
        }

        // TRIM([LEADING|TRAILING|BOTH] [char FROM] expr)
        if self.at_keyword("TRIM") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            return Ok(Some(self.parse_trim_expr(start)?));
        }

        // BTRIM / LTRIM / RTRIM(expr [, chars])
        for (kw, kind) in &[
            ("BTRIM", StringFoldKind::BTrim),
            ("LTRIM", StringFoldKind::LTrim),
            ("RTRIM", StringFoldKind::RTrim),
        ] {
            if self.at_keyword(kw) && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                let kind = *kind;
                self.advance();
                self.advance();
                let expr = self.parse_expr()?;
                let chars = if self.eat_token(&Token::Comma) {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                self.expect_token(&Token::RParen)?;
                return Ok(Some(Expr {
                    span: self.span_since(start),
                    kind: ExprKind::FoldString {
                        kind,
                        expr: Box::new(expr),
                        chars,
                    },
                }));
            }
        }

        // NORMALIZE(expr [, form])
        if self.at_keyword("NORMALIZE") && matches!(self.peek_ahead(1), Some(Token::LParen)) {
            self.advance();
            self.advance();
            let expr = self.parse_expr()?;
            let form = if self.eat_token(&Token::Comma) {
                self.parse_normal_form_opt()
            } else {
                NormalForm::NFC
            };
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::Normalize {
                    expr: Box::new(expr),
                    form,
                },
            }));
        }

        // CHAR_LENGTH / CHARACTER_LENGTH(expr)
        if (self.at_keyword("CHAR_LENGTH") || self.at_keyword("CHARACTER_LENGTH"))
            && matches!(self.peek_ahead(1), Some(Token::LParen))
        {
            let keyword = Keyword::new(self.current_ident_upper());
            self.advance();
            self.advance();
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::CharLength {
                    keyword,
                    expr: Box::new(arg),
                },
            }));
        }

        // BYTE_LENGTH / OCTET_LENGTH(expr)
        if (self.at_keyword("BYTE_LENGTH") || self.at_keyword("OCTET_LENGTH"))
            && matches!(self.peek_ahead(1), Some(Token::LParen))
        {
            let keyword = Keyword::new(self.current_ident_upper());
            self.advance();
            self.advance();
            let arg = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::ByteLength {
                    keyword,
                    expr: Box::new(arg),
                },
            }));
        }

        Ok(None)
    }

    /// Parse TRIM([spec] [char FROM] expr) or TRIM(listExpr, numericExpr).
    fn parse_trim_expr(&mut self, start: usize) -> Result<Expr, GqlError> {
        self.expect_keyword("TRIM")?;
        self.expect_token(&Token::LParen)?;

        // Optional trim spec: LEADING | TRAILING | BOTH
        let spec = if self.eat_keyword("LEADING") {
            Some(TrimSpec::Leading)
        } else if self.eat_keyword("TRAILING") {
            Some(TrimSpec::Trailing)
        } else if self.eat_keyword("BOTH") {
            Some(TrimSpec::Both)
        } else {
            None
        };

        // GQL: TRIM( [spec] [char FROM] expr ) or TRIM( listExpr, numericExpr )
        //
        // When a spec is present, the next token may be `FROM` directly
        // (no trim character), or a char literal followed by `FROM`, or
        // just the expression to trim.
        if spec.is_some() && self.eat_keyword("FROM") {
            // TRIM(LEADING FROM expr) — no trim character
            let expr = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            return Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Trim {
                    spec,
                    trim_char: None,
                    expr: Box::new(expr),
                },
            });
        }

        let first = self.parse_expr()?;

        if self.eat_keyword("FROM") {
            // `first` is the trim character, now parse the main expr
            let expr = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Trim {
                    spec,
                    trim_char: Some(Box::new(first)),
                    expr: Box::new(expr),
                },
            })
        } else if spec.is_none() && self.eat_token(&Token::Comma) {
            // trimListFunction: TRIM(listExpr, numericExpr)
            let count = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::TrimList {
                    list: Box::new(first),
                    count: Box::new(count),
                },
            })
        } else {
            self.expect_token(&Token::RParen)?;
            Ok(Expr {
                span: self.span_since(start),
                kind: ExprKind::Trim {
                    spec,
                    trim_char: None,
                    expr: Box::new(first),
                },
            })
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // Datetime constructor functions
    // ════════════════════════════════════════════════════════════════════════

    /// Try to parse a datetime constructor function or typed literal.
    ///
    /// Typed literals (GQL §20.25): `DATE 'string'`, `TIME 'string'`,
    /// `DATETIME 'string'`, `TIMESTAMP 'string'`.
    /// Function form: `DATE(...)`, `TIME(...)`, etc.
    fn try_parse_datetime_function(&mut self, start: usize) -> Result<Option<Expr>, GqlError> {
        // First, try typed literal form: KEYWORD 'string' (no parentheses).
        macro_rules! dt_lit {
            ($kw:expr, $variant:ident) => {
                if self.at_keyword($kw) && matches!(self.peek_ahead(1), Some(Token::StringLit(_))) {
                    self.advance(); // consume keyword
                    if let Some(Token::StringLit(s)) = self.peek().cloned() {
                        self.advance(); // consume string
                        return Ok(Some(Expr {
                            span: self.span_since(start),
                            kind: ExprKind::$variant(vec![Expr::new(ExprKind::Literal(
                                Value::Text(s),
                            ))]),
                        }));
                    }
                }
            };
        }

        // ── Typed temporal literals (GQL §20.25) ──
        // dateLiteral:     DATE dateString
        // timeLiteral:     TIME timeString       — no function form
        // datetimeLiteral: (DATETIME | TIMESTAMP) datetimeString — no function form
        // durationLiteral: DURATION durationString
        dt_lit!("TIME", TimeLiteral);
        dt_lit!("DATETIME", DatetimeLiteral);
        dt_lit!("TIMESTAMP", TimestampLiteral);
        dt_lit!("DURATION", DurationLiteral);

        // ── Temporal function calls (GQL §20.25) ──
        // dateFunction:           DATE( dateFunctionParameters? )
        // timeFunction:           ZONED_TIME( timeFunctionParameters? )
        // datetimeFunction:       ZONED_DATETIME( datetimeFunctionParameters? )
        // localtimeFunction:      LOCAL_TIME( timeFunctionParameters? )?
        // localdatetimeFunction:  LOCAL_DATETIME( datetimeFunctionParameters? )
        // durationFunction:       DURATION( durationFunctionParameters )
        //
        macro_rules! dt_fn {
            ($kw:expr, $variant:ident) => {
                if self.at_keyword($kw) && matches!(self.peek_ahead(1), Some(Token::LParen)) {
                    self.advance();
                    self.advance();
                    let args = if self.at_token(&Token::RParen) {
                        vec![]
                    } else {
                        self.comma_list(|p| p.parse_expr())?
                    };
                    self.expect_token(&Token::RParen)?;
                    return Ok(Some(Expr {
                        span: self.span_since(start),
                        kind: ExprKind::$variant(args),
                    }));
                }
            };
        }

        dt_lit!("DATE", DateLiteral);

        dt_fn!("DATE", DateFunction);
        dt_fn!("ZONED_TIME", ZonedTimeFunction);
        dt_fn!("ZONED_DATETIME", ZonedDatetimeFunction);
        dt_fn!("LOCAL_TIME", LocalTimeFunction);
        dt_fn!("LOCAL_DATETIME", LocalDatetimeFunction);
        dt_fn!("DURATION", DurationFunction);

        // DURATION_BETWEEN(expr, expr)
        if self.at_keyword("DURATION_BETWEEN") && matches!(self.peek_ahead(1), Some(Token::LParen))
        {
            self.advance();
            self.advance();
            let a = self.parse_expr()?;
            self.expect_token(&Token::Comma)?;
            let b = self.parse_expr()?;
            self.expect_token(&Token::RParen)?;
            let qualifier = if self.eat_keyword("YEAR") {
                self.expect_keyword("TO")?;
                self.expect_keyword("MONTH")?;
                Some(DurationQualifier::YearToMonth)
            } else if self.eat_keyword("DAY") {
                self.expect_keyword("TO")?;
                self.expect_keyword("SECOND")?;
                Some(DurationQualifier::DayToSecond)
            } else {
                None
            };
            return Ok(Some(Expr {
                span: self.span_since(start),
                kind: ExprKind::DurationBetween {
                    left: Box::new(a),
                    right: Box::new(b),
                    qualifier,
                },
            }));
        }

        Ok(None)
    }
}
