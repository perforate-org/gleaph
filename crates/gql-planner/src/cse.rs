//! Common Subexpression Elimination (CSE) detection.
//!
//! Walks all expressions in the plan and identifies duplicates.
//! This is annotation-only — the executor can use the information
//! to cache intermediate results.

use rapidhash::RapidHashMap;

use gleaph_gql::ast::{Expr, ExprKind};

use crate::plan::{PlanAnnotations, PlanOp};

/// Detect common subexpressions across all plan operators.
pub fn detect_common_subexpressions(
    ops: &[PlanOp],
    annotations: &mut PlanAnnotations,
) {
    let exprs = collect_all_expressions(ops);
    let mut counts: RapidHashMap<String, usize> = RapidHashMap::default();

    for expr in &exprs {
        let key = canonical_expr(expr);
        if !key.is_empty() {
            *counts.entry(key).or_insert(0) += 1;
        }
    }

    let common: Vec<String> = counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(key, _)| key)
        .collect();

    if !common.is_empty() {
        let mut sorted = common;
        sorted.sort();
        annotations.optimizer.common_subexpressions = Some(sorted.into_iter().map(crate::plan::Str::from).collect());
    }
}

/// Collect all expressions from all plan operators.
fn collect_all_expressions(ops: &[PlanOp]) -> Vec<&Expr> {
    let mut exprs = Vec::new();
    for op in ops {
        match op {
            PlanOp::PropertyFilter { predicates, .. } => {
                for p in predicates {
                    collect_subexprs(p, &mut exprs);
                }
            }
            PlanOp::Filter { condition } => {
                collect_subexprs(condition, &mut exprs);
            }
            PlanOp::ExpandFilter { dst_filter, .. } => {
                for f in dst_filter {
                    collect_subexprs(f, &mut exprs);
                }
            }
            PlanOp::Project { columns, .. } => {
                for col in columns {
                    collect_subexprs(&col.expr, &mut exprs);
                }
            }
            PlanOp::Let { bindings } => {
                for b in bindings {
                    collect_subexprs(&b.value, &mut exprs);
                }
            }
            PlanOp::Aggregate { group_by, .. } => {
                for g in group_by {
                    collect_subexprs(g, &mut exprs);
                }
            }
            _ => {}
        }
    }
    exprs
}

/// Recursively collect an expression and all interesting sub-expressions.
fn collect_subexprs<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    // Only collect property accesses and function calls (common candidates).
    match &expr.kind {
        ExprKind::PropertyAccess { expr: inner, .. } => {
            out.push(expr);
            collect_subexprs(inner, out);
        }
        ExprKind::FunctionCall { args, .. } => {
            out.push(expr);
            for arg in args {
                collect_subexprs(arg, out);
            }
        }
        ExprKind::Compare { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::BinaryOp { left, right, .. } => {
            collect_subexprs(left, out);
            collect_subexprs(right, out);
        }
        ExprKind::Not(inner) | ExprKind::Paren(inner) | ExprKind::UnaryOp { expr: inner, .. } => {
            collect_subexprs(inner, out);
        }
        _ => {}
    }
}

/// Create a canonical string representation of an expression for dedup.
fn canonical_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::PropertyAccess { expr: inner, property } => {
            format!("{}.{}", canonical_expr(inner), property)
        }
        ExprKind::Variable(v) => v.clone(),
        ExprKind::FunctionCall { name, args, .. } => {
            let arg_strs: Vec<String> = args.iter().map(canonical_expr).collect();
            format!("{:?}({})", name, arg_strs.join(","))
        }
        _ => String::new(), // Don't canonicalize complex expressions.
    }
}
