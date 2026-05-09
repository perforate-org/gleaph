//! Substitute RETURN / SELECT column aliases inside `HAVING` for physical planning.
//!
//! Post-aggregate [`crate::plan::PlanOp::Filter`] runs before [`crate::plan::PlanOp::Project`],
//! so result aliases are not row bindings yet; expanding aliases preserves executor semantics.

use std::collections::BTreeMap;

use gleaph_gql::ast::{Expr, ExprKind, LetBinding, OrderByClause, SortItem, WhenClause};

/// Maps each immediate child [`Expr`] and rebuilds `expr`. Leaf nodes (no child expressions)
/// are returned unchanged.
pub(crate) fn map_immediate_children_expr(
    expr: &Expr,
    mut map_child: impl FnMut(&Expr) -> Expr,
) -> Expr {
    let span = expr.span;
    let kind = match &expr.kind {
        ExprKind::Paren(e) => ExprKind::Paren(Box::new(map_child(e))),
        ExprKind::UnaryOp { op, expr: e } => ExprKind::UnaryOp {
            op: *op,
            expr: Box::new(map_child(e)),
        },
        ExprKind::Not(e) => ExprKind::Not(Box::new(map_child(e))),
        ExprKind::IsNull(e) => ExprKind::IsNull(Box::new(map_child(e))),
        ExprKind::IsNotNull(e) => ExprKind::IsNotNull(Box::new(map_child(e))),
        ExprKind::BinaryOp { left, op, right } => ExprKind::BinaryOp {
            left: Box::new(map_child(left)),
            op: *op,
            right: Box::new(map_child(right)),
        },
        ExprKind::And(left, right) => {
            ExprKind::And(Box::new(map_child(left)), Box::new(map_child(right)))
        }
        ExprKind::Or(left, right) => {
            ExprKind::Or(Box::new(map_child(left)), Box::new(map_child(right)))
        }
        ExprKind::Xor(left, right) => {
            ExprKind::Xor(Box::new(map_child(left)), Box::new(map_child(right)))
        }
        ExprKind::Compare { left, op, right } => ExprKind::Compare {
            left: Box::new(map_child(left)),
            op: *op,
            right: Box::new(map_child(right)),
        },
        ExprKind::Concat(left, right) => {
            ExprKind::Concat(Box::new(map_child(left)), Box::new(map_child(right)))
        }
        ExprKind::NullIf(left, right) => {
            ExprKind::NullIf(Box::new(map_child(left)), Box::new(map_child(right)))
        }
        ExprKind::PropertyAccess { expr: e, property } => ExprKind::PropertyAccess {
            expr: Box::new(map_child(e)),
            property: property.clone(),
        },
        ExprKind::IsLabeled {
            expr: e,
            label,
            negated,
        } => ExprKind::IsLabeled {
            expr: Box::new(map_child(e)),
            label: label.clone(),
            negated: *negated,
        },
        ExprKind::IsTyped {
            expr: e,
            target,
            negated,
        } => ExprKind::IsTyped {
            expr: Box::new(map_child(e)),
            target: target.clone(),
            negated: *negated,
        },
        ExprKind::IsDirected { expr: e, negated } => ExprKind::IsDirected {
            expr: Box::new(map_child(e)),
            negated: *negated,
        },
        ExprKind::IsNormalized {
            expr: e,
            form,
            negated,
        } => ExprKind::IsNormalized {
            expr: Box::new(map_child(e)),
            form: *form,
            negated: *negated,
        },
        ExprKind::IsTruth {
            expr: e,
            value,
            negated,
        } => ExprKind::IsTruth {
            expr: Box::new(map_child(e)),
            value: *value,
            negated: *negated,
        },
        ExprKind::IsSourceOf {
            node,
            edge,
            negated,
        } => ExprKind::IsSourceOf {
            node: Box::new(map_child(node)),
            edge: Box::new(map_child(edge)),
            negated: *negated,
        },
        ExprKind::IsDestOf {
            node,
            edge,
            negated,
        } => ExprKind::IsDestOf {
            node: Box::new(map_child(node)),
            edge: Box::new(map_child(edge)),
            negated: *negated,
        },
        ExprKind::StringPredicate {
            expr: target,
            kind,
            pattern,
            negated,
        } => ExprKind::StringPredicate {
            expr: Box::new(map_child(target)),
            kind: *kind,
            pattern: Box::new(map_child(pattern)),
            negated: *negated,
        },
        ExprKind::ListLiteral(elems) => {
            ExprKind::ListLiteral(elems.iter().map(&mut map_child).collect())
        }
        ExprKind::ListConstructor { keyword, items } => ExprKind::ListConstructor {
            keyword: keyword.clone(),
            items: items.iter().map(&mut map_child).collect(),
        },
        ExprKind::AllDifferent(elems) => {
            ExprKind::AllDifferent(elems.iter().map(&mut map_child).collect())
        }
        ExprKind::Same(elems) => ExprKind::Same(elems.iter().map(&mut map_child).collect()),
        ExprKind::Coalesce(elems) => ExprKind::Coalesce(elems.iter().map(&mut map_child).collect()),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
        } => ExprKind::FunctionCall {
            name: name.clone(),
            args: args.iter().map(&mut map_child).collect(),
            distinct: *distinct,
        },
        ExprKind::Aggregate {
            func,
            expr,
            expr2,
            distinct,
            order_by,
            filter,
        } => ExprKind::Aggregate {
            func: *func,
            expr: expr.as_ref().map(|e| Box::new(map_child(e.as_ref()))),
            expr2: expr2.as_ref().map(|e| Box::new(map_child(e.as_ref()))),
            distinct: *distinct,
            order_by: order_by.as_ref().map(|ob| OrderByClause {
                span: ob.span,
                items: ob
                    .items
                    .iter()
                    .map(|it| SortItem {
                        span: it.span,
                        expr: map_child(&it.expr),
                        direction: it.direction,
                        null_order: it.null_order,
                    })
                    .collect(),
            }),
            filter: filter.as_ref().map(|e| Box::new(map_child(e.as_ref()))),
        },
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => ExprKind::CaseSimple {
            operand: Box::new(map_child(operand)),
            when_clauses: when_clauses
                .iter()
                .map(|w| WhenClause {
                    span: w.span,
                    condition: map_child(&w.condition),
                    result: map_child(&w.result),
                })
                .collect(),
            else_clause: else_clause
                .as_ref()
                .map(|e| Box::new(map_child(e.as_ref()))),
        },
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => ExprKind::CaseSearched {
            when_clauses: when_clauses
                .iter()
                .map(|w| WhenClause {
                    span: w.span,
                    condition: map_child(&w.condition),
                    result: map_child(&w.result),
                })
                .collect(),
            else_clause: else_clause
                .as_ref()
                .map(|e| Box::new(map_child(e.as_ref()))),
        },
        ExprKind::LetIn {
            bindings,
            expr: body,
        } => ExprKind::LetIn {
            bindings: bindings
                .iter()
                .map(|b| LetBinding {
                    span: b.span,
                    variable: b.variable.clone(),
                    value: map_child(&b.value),
                })
                .collect(),
            expr: Box::new(map_child(body)),
        },
        ExprKind::RecordLiteral(fields) => ExprKind::RecordLiteral(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), map_child(v)))
                .collect(),
        ),
        ExprKind::RecordConstructor(fields) => ExprKind::RecordConstructor(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), map_child(v)))
                .collect(),
        ),
        ExprKind::PathConstructor { elements } => ExprKind::PathConstructor {
            elements: elements.iter().map(&mut map_child).collect(),
        },
        ExprKind::PathLength(e) => ExprKind::PathLength(Box::new(map_child(e))),
        ExprKind::ElementId(e) => ExprKind::ElementId(Box::new(map_child(e))),
        ExprKind::Cast { expr: e, target } => ExprKind::Cast {
            expr: Box::new(map_child(e)),
            target: target.clone(),
        },
        ExprKind::DateLiteral(args) => {
            ExprKind::DateLiteral(args.iter().map(&mut map_child).collect())
        }
        ExprKind::DateFunction(args) => {
            ExprKind::DateFunction(args.iter().map(&mut map_child).collect())
        }
        ExprKind::TimeLiteral(args) => {
            ExprKind::TimeLiteral(args.iter().map(&mut map_child).collect())
        }
        ExprKind::DatetimeLiteral(args) => {
            ExprKind::DatetimeLiteral(args.iter().map(&mut map_child).collect())
        }
        ExprKind::TimestampLiteral(args) => {
            ExprKind::TimestampLiteral(args.iter().map(&mut map_child).collect())
        }
        ExprKind::ZonedTimeFunction(args) => {
            ExprKind::ZonedTimeFunction(args.iter().map(&mut map_child).collect())
        }
        ExprKind::ZonedDatetimeFunction(args) => {
            ExprKind::ZonedDatetimeFunction(args.iter().map(&mut map_child).collect())
        }
        ExprKind::LocalTimeFunction(args) => {
            ExprKind::LocalTimeFunction(args.iter().map(&mut map_child).collect())
        }
        ExprKind::LocalDatetimeFunction(args) => {
            ExprKind::LocalDatetimeFunction(args.iter().map(&mut map_child).collect())
        }
        ExprKind::DurationLiteral(args) => {
            ExprKind::DurationLiteral(args.iter().map(&mut map_child).collect())
        }
        ExprKind::DurationFunction(args) => {
            ExprKind::DurationFunction(args.iter().map(&mut map_child).collect())
        }
        ExprKind::PropertyExists { expr: e, property } => ExprKind::PropertyExists {
            expr: Box::new(map_child(e)),
            property: property.clone(),
        },
        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, index } => ExprKind::ListIndex {
            list: Box::new(map_child(list)),
            index: Box::new(map_child(index)),
        },
        #[cfg(feature = "cypher")]
        ExprKind::ListSlice { list, from, to } => ExprKind::ListSlice {
            list: Box::new(map_child(list)),
            from: from.as_ref().map(|e| Box::new(map_child(e.as_ref()))),
            to: to.as_ref().map(|e| Box::new(map_child(e.as_ref()))),
        },
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
        | ExprKind::ValueSubquery(_) => expr.kind.clone(),
        #[allow(unreachable_patterns)]
        _ => expr.kind.clone(),
    };
    Expr { span, kind }
}

/// Replace [`ExprKind::Variable`] references that name a `RETURN` / `SELECT` column alias with
/// that column's expression (after recursively rewriting children).
pub(crate) fn substitute_return_aliases_in_expr(
    expr: &Expr,
    aliases: &BTreeMap<String, Expr>,
) -> Expr {
    if let ExprKind::Variable(name) = &expr.kind {
        if let Some(subst) = aliases.get(name) {
            return substitute_return_aliases_in_expr(subst, aliases);
        }
        return expr.clone();
    }
    map_immediate_children_expr(expr, |c| substitute_return_aliases_in_expr(c, aliases))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{Expr, ExprKind};

    #[test]
    fn rewrites_having_alias_variable_to_column_expression() {
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "cnt".to_string(),
            Expr::new(ExprKind::Literal(Value::Int64(42))),
        );
        let having = Expr::new(ExprKind::Variable("cnt".into()));
        let out = substitute_return_aliases_in_expr(&having, &aliases);
        assert!(matches!(out.kind, ExprKind::Literal(Value::Int64(42))));
    }
}
