//! Canonical immediate-child traversal for GQL [`Expr`] trees.
//!
//! Keep this aligned with child-walk cases in `gleaph_gql::validate`.
//! When adding `ExprKind` child traversal to `validate.rs`, update the table test below in the same PR.

use gleaph_gql::ast::{Expr, ExprKind};

/// Invoke `visit` on each direct child expression of `expr`.
pub fn for_each_immediate_child_expr(expr: &Expr, mut visit: impl FnMut(&Expr)) {
    match &expr.kind {
        ExprKind::Paren(e)
        | ExprKind::UnaryOp { expr: e, .. }
        | ExprKind::Not(e)
        | ExprKind::IsNull(e)
        | ExprKind::IsNotNull(e) => visit(e),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Compare { left, right, .. }
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right) => {
            visit(left);
            visit(right);
        }
        ExprKind::PropertyAccess { expr: e, .. }
        | ExprKind::IsLabeled { expr: e, .. }
        | ExprKind::IsTyped { expr: e, .. }
        | ExprKind::IsDirected { expr: e, .. }
        | ExprKind::IsNormalized { expr: e, .. }
        | ExprKind::IsTruth { expr: e, .. }
        | ExprKind::PropertyExists { expr: e, .. }
        | ExprKind::Cast { expr: e, .. }
        | ExprKind::PathLength(e)
        | ExprKind::ElementId(e) => visit(e),
        ExprKind::IsSourceOf { node, edge, .. } | ExprKind::IsDestOf { node, edge, .. } => {
            visit(node);
            visit(edge);
        }
        ExprKind::StringPredicate {
            expr: target,
            pattern,
            ..
        } => {
            visit(target);
            visit(pattern);
        }
        ExprKind::ListLiteral(elems)
        | ExprKind::ListConstructor { items: elems, .. }
        | ExprKind::AllDifferent(elems)
        | ExprKind::Same(elems)
        | ExprKind::Coalesce(elems) => {
            for e in elems {
                visit(e);
            }
        }
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                visit(a);
            }
        }
        ExprKind::Aggregate {
            expr,
            expr2,
            filter,
            order_by,
            ..
        } => {
            if let Some(e) = expr {
                visit(e);
            }
            if let Some(e) = expr2 {
                visit(e);
            }
            if let Some(e) = filter {
                visit(e);
            }
            if let Some(ob) = order_by {
                for item in &ob.items {
                    visit(&item.expr);
                }
            }
        }
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            visit(operand);
            for w in when_clauses {
                visit(&w.condition);
                visit(&w.result);
            }
            if let Some(e) = else_clause {
                visit(e);
            }
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            for w in when_clauses {
                visit(&w.condition);
                visit(&w.result);
            }
            if let Some(e) = else_clause {
                visit(e);
            }
        }
        ExprKind::LetIn {
            bindings,
            expr: body,
        } => {
            for b in bindings {
                visit(&b.value);
            }
            visit(body);
        }
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (_, v) in fields {
                visit(v);
            }
        }
        ExprKind::PathConstructor { elements } => {
            for e in elements {
                visit(e);
            }
        }
        ExprKind::DateLiteral(args)
        | ExprKind::DateFunction(args)
        | ExprKind::TimeLiteral(args)
        | ExprKind::DatetimeLiteral(args)
        | ExprKind::TimestampLiteral(args)
        | ExprKind::ZonedTimeFunction(args)
        | ExprKind::ZonedDatetimeFunction(args)
        | ExprKind::LocalTimeFunction(args)
        | ExprKind::LocalDatetimeFunction(args)
        | ExprKind::DurationLiteral(args)
        | ExprKind::DurationFunction(args) => {
            for a in args {
                visit(a);
            }
        }
        ExprKind::DurationBetween { left, right, .. }
        | ExprKind::Mod(left, right)
        | ExprKind::Log(left, right)
        | ExprKind::Power(left, right)
        | ExprKind::Left(left, right)
        | ExprKind::Right(left, right)
        | ExprKind::TrimList {
            list: left,
            count: right,
        } => {
            visit(left);
            visit(right);
        }
        ExprKind::Normalize { expr: e, .. }
        | ExprKind::Upper(e)
        | ExprKind::Lower(e)
        | ExprKind::CharLength { expr: e, .. }
        | ExprKind::ByteLength { expr: e, .. }
        | ExprKind::Cardinality { expr: e, .. }
        | ExprKind::Abs(e)
        | ExprKind::Floor(e)
        | ExprKind::Ceil(e)
        | ExprKind::Sqrt(e)
        | ExprKind::Exp(e)
        | ExprKind::Ln(e)
        | ExprKind::Log10(e)
        | ExprKind::Sin(e)
        | ExprKind::Cos(e)
        | ExprKind::Tan(e)
        | ExprKind::Asin(e)
        | ExprKind::Acos(e)
        | ExprKind::Atan(e)
        | ExprKind::Degrees(e)
        | ExprKind::Radians(e)
        | ExprKind::Cot(e)
        | ExprKind::Sinh(e)
        | ExprKind::Cosh(e)
        | ExprKind::Tanh(e)
        | ExprKind::Elements(e) => visit(e),
        ExprKind::Trim {
            trim_char, expr: e, ..
        } => {
            if let Some(tc) = trim_char {
                visit(tc);
            }
            visit(e);
        }
        ExprKind::FoldString { expr: e, chars, .. } => {
            visit(e);
            if let Some(c) = chars {
                visit(c);
            }
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::Sign(e) => visit(e),
        #[cfg(feature = "sql-compat")]
        ExprKind::Atan2(left, right) => {
            visit(left);
            visit(right);
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::Truncate { expr: e, places } | ExprKind::Round { expr: e, places } => {
            visit(e);
            if let Some(p) = places {
                visit(p);
            }
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::InList { expr: e, list, .. } => {
            visit(e);
            for item in list {
                visit(item);
            }
        }
        #[cfg(feature = "cypher")]
        ExprKind::Nodes(e)
        | ExprKind::Edges(e)
        | ExprKind::Labels(e)
        | ExprKind::Label(e)
        | ExprKind::Source(e)
        | ExprKind::Destination(e) => visit(e),
        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, index } => {
            visit(list);
            visit(index);
        }
        #[cfg(feature = "cypher")]
        ExprKind::ListSlice { list, from, to } => {
            visit(list);
            if let Some(e) = from {
                visit(e);
            }
            if let Some(e) = to {
                visit(e);
            }
        }
        ExprKind::Literal(_)
        | ExprKind::Variable(_)
        | ExprKind::Parameter(_)
        | ExprKind::SessionUser
        | ExprKind::CurrentDate
        | ExprKind::CurrentTime
        | ExprKind::CurrentTimestamp
        | ExprKind::CurrentLocalTime
        | ExprKind::CurrentLocalTimestamp
        | ExprKind::ExistsSubquery(_)
        | ExprKind::ExistsPattern(_)
        | ExprKind::ValueSubquery(_) => {}
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pushdown::collect_variables;
    use gleaph_gql::ast::{ObjectName, ValueType, WhenClause};
    use gleaph_gql::token::Span;
    use gleaph_gql::value::Value;

    fn fake_weight(var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::simple("FAKE_WEIGHT"),
            args: vec![Expr::var(var)],
            distinct: false,
        })
    }

    #[test]
    fn collect_variables_finds_nested_edge_var_in_builtin_wrappers() {
        let e = "e";
        let samples = [
            Expr::new(ExprKind::Abs(Box::new(fake_weight(e)))),
            Expr::new(ExprKind::Floor(Box::new(fake_weight(e)))),
            Expr::new(ExprKind::Ceil(Box::new(fake_weight(e)))),
            Expr::new(ExprKind::Sqrt(Box::new(fake_weight(e)))),
            Expr::new(ExprKind::Cast {
                expr: Box::new(fake_weight(e)),
                target: ValueType::Float32 {
                    keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
                },
            }),
            Expr::new(ExprKind::Coalesce(vec![
                fake_weight(e),
                Expr::new(ExprKind::Literal(Value::Float32(1.0))),
            ])),
            Expr::new(ExprKind::NullIf(
                Box::new(fake_weight(e)),
                Box::new(Expr::new(ExprKind::Literal(Value::Float32(0.0)))),
            )),
            Expr::new(ExprKind::CaseSimple {
                operand: Box::new(Expr::var(e)),
                when_clauses: vec![WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Null)),
                    result: fake_weight(e),
                }],
                else_clause: None,
            }),
            Expr::new(ExprKind::CaseSearched {
                when_clauses: vec![WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Bool(true))),
                    result: fake_weight(e),
                }],
                else_clause: None,
            }),
            Expr::new(ExprKind::IsLabeled {
                expr: Box::new(fake_weight(e)),
                label: gleaph_gql::types::LabelExpr::Name("L".into()),
                negated: false,
            }),
            Expr::new(ExprKind::LetIn {
                bindings: vec![gleaph_gql::ast::LetBinding {
                    span: Span::DUMMY,
                    variable: "x".into(),
                    value: fake_weight(e),
                }],
                expr: Box::new(Expr::var(e)),
            }),
            Expr::new(ExprKind::Mod(
                Box::new(fake_weight(e)),
                Box::new(Expr::new(ExprKind::Literal(Value::Int32(2)))),
            )),
            Expr::new(ExprKind::Power(
                Box::new(fake_weight(e)),
                Box::new(Expr::new(ExprKind::Literal(Value::Int32(2)))),
            )),
            Expr::new(ExprKind::FunctionCall {
                name: ObjectName::simple("FAKE_FN"),
                args: vec![fake_weight(e)],
                distinct: false,
            }),
            Expr::new(ExprKind::Aggregate {
                func: gleaph_gql::ast::AggregateFunc::Count,
                expr: Some(Box::new(fake_weight(e))),
                expr2: None,
                filter: Some(Box::new(fake_weight(e))),
                order_by: None,
                distinct: false,
            }),
            Expr::new(ExprKind::ListLiteral(vec![fake_weight(e)])),
        ];
        for expr in &samples {
            let vars = collect_variables(expr);
            assert_eq!(vars, vec![e], "expr {:?}", expr.kind);
        }
    }
}
