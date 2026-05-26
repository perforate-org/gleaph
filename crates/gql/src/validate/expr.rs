use crate::ast::*;
use rapidhash::RapidHashSet;

use super::query_validation::{collect_pattern_bindings, validate_composite_query};
use super::{VResult, verr};

pub(super) fn validate_let(
    bindings: &[LetBinding],
    scope: &mut RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    if bindings.is_empty() {
        return Err(verr("LET must have at least one binding"));
    }
    for binding in bindings {
        validate_expr(&binding.value, scope, graph_scope)?;
        scope.insert(binding.variable.clone());
    }
    Ok(())
}

pub(super) fn validate_expr(
    expr: &Expr,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::SessionUser
        | ExprKind::CurrentDate
        | ExprKind::CurrentTime
        | ExprKind::CurrentTimestamp
        | ExprKind::CurrentLocalTime
        | ExprKind::CurrentLocalTimestamp => Ok(()),

        ExprKind::Variable(name) => {
            if !scope.contains(name) {
                return Err(verr(&format!("variable '{name}' is not in scope")));
            }
            Ok(())
        }

        ExprKind::PropertyAccess { expr, .. } => validate_expr(expr, scope, graph_scope),

        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Compare { left, right, .. }
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right)
        | ExprKind::Mod(left, right)
        | ExprKind::Log(left, right)
        | ExprKind::Power(left, right)
        | ExprKind::DurationBetween { left, right, .. }
        | ExprKind::Left(left, right)
        | ExprKind::Right(left, right)
        | ExprKind::TrimList {
            list: left,
            count: right,
        }
        | ExprKind::IsSourceOf {
            node: left,
            edge: right,
            ..
        }
        | ExprKind::IsDestOf {
            node: left,
            edge: right,
            ..
        } => {
            validate_expr(left, scope, graph_scope)?;
            validate_expr(right, scope, graph_scope)
        }

        ExprKind::Paren(e)
        | ExprKind::Not(e)
        | ExprKind::UnaryOp { expr: e, .. }
        | ExprKind::IsNull(e)
        | ExprKind::IsNotNull(e)
        | ExprKind::IsNormalized { expr: e, .. }
        | ExprKind::IsTruth { expr: e, .. }
        | ExprKind::IsLabeled { expr: e, .. }
        | ExprKind::IsDirected { expr: e, .. }
        | ExprKind::IsTyped { expr: e, .. }
        | ExprKind::Cast { expr: e, .. }
        | ExprKind::Normalize { expr: e, .. }
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
        | ExprKind::ElementId(e)
        | ExprKind::PathLength(e)
        | ExprKind::Elements(e) => validate_expr(e, scope, graph_scope),

        ExprKind::Trim {
            trim_char, expr, ..
        } => {
            if let Some(tc) = trim_char {
                validate_expr(tc, scope, graph_scope)?;
            }
            validate_expr(expr, scope, graph_scope)
        }

        ExprKind::Degrees(e)
        | ExprKind::Radians(e)
        | ExprKind::Cot(e)
        | ExprKind::Sinh(e)
        | ExprKind::Cosh(e)
        | ExprKind::Tanh(e) => validate_expr(e, scope, graph_scope),

        #[cfg(feature = "sql-compat")]
        ExprKind::Sign(e) => validate_expr(e, scope, graph_scope),

        #[cfg(feature = "sql-compat")]
        ExprKind::Atan2(left, right) => {
            validate_expr(left, scope, graph_scope)?;
            validate_expr(right, scope, graph_scope)
        }

        #[cfg(feature = "sql-compat")]
        ExprKind::Truncate { expr, places } | ExprKind::Round { expr, places } => {
            validate_expr(expr, scope, graph_scope)?;
            if let Some(p) = places {
                validate_expr(p, scope, graph_scope)?;
            }
            Ok(())
        }

        #[cfg(feature = "cypher")]
        ExprKind::Nodes(e)
        | ExprKind::Edges(e)
        | ExprKind::Labels(e)
        | ExprKind::Label(e)
        | ExprKind::Source(e)
        | ExprKind::Destination(e) => validate_expr(e, scope, graph_scope),

        ExprKind::FoldString { expr, chars, .. } => {
            validate_expr(expr, scope, graph_scope)?;
            if let Some(c) = chars {
                validate_expr(c, scope, graph_scope)?;
            }
            Ok(())
        }

        #[cfg(feature = "sql-compat")]
        ExprKind::InList { expr, list, .. } => {
            validate_expr(expr, scope, graph_scope)?;
            for e in list {
                validate_expr(e, scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::StringPredicate { expr, pattern, .. } => {
            validate_expr(expr, scope, graph_scope)?;
            validate_expr(pattern, scope, graph_scope)
        }

        ExprKind::ListLiteral(items)
        | ExprKind::ListConstructor { items, .. }
        | ExprKind::Coalesce(items)
        | ExprKind::AllDifferent(items)
        | ExprKind::Same(items)
        | ExprKind::PathConstructor { elements: items }
        | ExprKind::DateLiteral(items)
        | ExprKind::DateFunction(items)
        | ExprKind::TimeLiteral(items)
        | ExprKind::DatetimeLiteral(items)
        | ExprKind::TimestampLiteral(items)
        | ExprKind::DurationLiteral(items)
        | ExprKind::ZonedTimeFunction(items)
        | ExprKind::ZonedDatetimeFunction(items)
        | ExprKind::LocalTimeFunction(items)
        | ExprKind::LocalDatetimeFunction(items)
        | ExprKind::DurationFunction(items) => {
            for e in items {
                validate_expr(e, scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (_, v) in fields {
                validate_expr(v, scope, graph_scope)?;
            }
            Ok(())
        }

        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, index } => {
            validate_expr(list, scope, graph_scope)?;
            validate_expr(index, scope, graph_scope)
        }

        #[cfg(feature = "cypher")]
        ExprKind::ListSlice { list, from, to } => {
            validate_expr(list, scope, graph_scope)?;
            if let Some(f) = from {
                validate_expr(f, scope, graph_scope)?;
            }
            if let Some(t) = to {
                validate_expr(t, scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            validate_expr(operand, scope, graph_scope)?;
            for wc in when_clauses {
                validate_expr(&wc.condition, scope, graph_scope)?;
                validate_expr(&wc.result, scope, graph_scope)?;
            }
            if let Some(e) = else_clause {
                validate_expr(e, scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            for wc in when_clauses {
                validate_expr(&wc.condition, scope, graph_scope)?;
                validate_expr(&wc.result, scope, graph_scope)?;
            }
            if let Some(e) = else_clause {
                validate_expr(e, scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::Aggregate {
            expr: e,
            expr2,
            filter,
            order_by,
            ..
        } => {
            if let Some(inner) = e {
                validate_expr(inner, scope, graph_scope)?;
            }
            if let Some(inner2) = expr2 {
                validate_expr(inner2, scope, graph_scope)?;
            }
            if let Some(f) = filter {
                validate_expr(f, scope, graph_scope)?;
            }
            if let Some(ob) = order_by {
                for item in &ob.items {
                    validate_expr(&item.expr, scope, graph_scope)?;
                }
            }
            Ok(())
        }

        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                validate_expr(arg, scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::ExistsSubquery(cq) | ExprKind::ValueSubquery(cq) => {
            validate_composite_query(cq, scope, graph_scope)
        }

        ExprKind::ExistsPattern(gp) => {
            let mut inner_scope = scope.clone();
            collect_pattern_bindings(gp, &mut inner_scope)?;
            if let Some(ref w) = gp.where_clause {
                validate_expr(w, &inner_scope, graph_scope)?;
            }
            Ok(())
        }

        ExprKind::LetIn { bindings, expr } => {
            let mut inner_scope = scope.clone();
            validate_let(bindings, &mut inner_scope, graph_scope)?;
            validate_expr(expr, &inner_scope, graph_scope)
        }

        ExprKind::PropertyExists { expr, .. } => validate_expr(expr, scope, graph_scope),
    }
}
