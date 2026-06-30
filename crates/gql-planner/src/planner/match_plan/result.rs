use gleaph_gql::ast::*;
use std::collections::BTreeMap;

use crate::expr_alias::substitute_return_aliases_in_expr;
use crate::expr_children::for_each_immediate_child_expr;
use crate::plan::*;

pub(crate) fn plan_result_statement(result: &ResultStatement, ops: &mut Vec<PlanOp>) {
    match result {
        ResultStatement::Return(ret) => plan_return(ret, ops),
        ResultStatement::Select(sel) => plan_select(sel, ops),
        ResultStatement::Finish => {}
    }
}

fn plan_return(ret: &ReturnStatement, ops: &mut Vec<PlanOp>) {
    let distinct = ret.set_quantifier == SetQuantifier::Distinct;
    match &ret.body {
        ReturnBody::Star => {
            ops.push(PlanOp::Project {
                columns: vec![],
                distinct,
            });
        }
        #[cfg(feature = "cypher")]
        ReturnBody::NoBindings => {
            // Cypher extension: explicit empty return bindings.
            // Produces an output row set with zero projected columns.
            ops.push(PlanOp::Project {
                columns: vec![],
                distinct,
            });
        }
        ReturnBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
            // Aggregation.
            if let Some(gb) = group_by {
                let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
                ops.push(PlanOp::Aggregate {
                    group_by: gb.items.clone(),
                    aggregates: agg_specs,
                });
                if let Some(h) = having_rw {
                    ops.push(PlanOp::Filter { condition: h });
                }
                ops.push(PlanOp::Project {
                    columns: proj_cols,
                    distinct,
                });
            } else if items.iter().any(|item| expr_contains_aggregate(&item.expr))
                || having_rw.as_ref().is_some_and(expr_contains_aggregate)
            {
                // Implicit whole-result aggregation (no GROUP BY): executor needs `Aggregate`
                // before `Project`; bare `Aggregate` exprs in `Project` are not evaluable.
                let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
                ops.push(PlanOp::Aggregate {
                    group_by: Vec::new(),
                    aggregates: agg_specs,
                });
                if let Some(h) = having_rw {
                    ops.push(PlanOp::Filter { condition: h });
                }
                ops.push(PlanOp::Project {
                    columns: proj_cols,
                    distinct,
                });
            } else {
                let columns: Vec<ProjectColumn> = items
                    .iter()
                    .map(|item| ProjectColumn {
                        expr: item.expr.clone(),
                        alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
                    })
                    .collect();
                ops.push(PlanOp::Project { columns, distinct });
            }

            if let Some(ob) = order_by {
                ops.push(PlanOp::Sort {
                    order_by: ob.clone(),
                });
            }

            if limit.is_some() || offset.is_some() {
                ops.push(PlanOp::Limit {
                    count: limit.as_ref().map(|l| l.count.clone()),
                    offset: offset.as_ref().map(|o| o.count.clone()),
                });
            }
        }
    }
}

fn plan_select(sel: &SelectStatement, ops: &mut Vec<PlanOp>) {
    let distinct = sel.set_quantifier == SetQuantifier::Distinct;

    let (items, group_by, having, order_by, limit, offset) = match &sel.body {
        SelectBody::Star {
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => (None, group_by, having, order_by, limit, offset),
        SelectBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => (Some(items), group_by, having, order_by, limit, offset),
    };

    if let Some(gb) = group_by {
        if let Some(items) = items {
            let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
            let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
            ops.push(PlanOp::Aggregate {
                group_by: gb.items.clone(),
                aggregates: agg_specs,
            });
            if let Some(h) = having_rw {
                ops.push(PlanOp::Filter { condition: h });
            }
            ops.push(PlanOp::Project {
                columns: proj_cols,
                distinct,
            });
        }
    } else if let Some(items) = items {
        let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
        if items.iter().any(|item| expr_contains_aggregate(&item.expr))
            || having_rw.as_ref().is_some_and(expr_contains_aggregate)
        {
            let (agg_specs, proj_cols) = extract_aggregates(items, having_rw.as_ref());
            ops.push(PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: agg_specs,
            });
            if let Some(h) = having_rw {
                ops.push(PlanOp::Filter { condition: h });
            }
            ops.push(PlanOp::Project {
                columns: proj_cols,
                distinct,
            });
        } else {
            let columns: Vec<ProjectColumn> = items
                .iter()
                .map(|item| ProjectColumn {
                    expr: item.expr.clone(),
                    alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
                })
                .collect();
            ops.push(PlanOp::Project { columns, distinct });
        }
    } else {
        ops.push(PlanOp::Project {
            columns: vec![],
            distinct,
        });
    }

    if let Some(ob) = order_by {
        ops.push(PlanOp::Sort {
            order_by: ob.clone(),
        });
    }

    if limit.is_some() || offset.is_some() {
        ops.push(PlanOp::Limit {
            count: limit.as_ref().map(|l| l.count.clone()),
            offset: offset.as_ref().map(|o| o.count.clone()),
        });
    }
}

/// Expand `RETURN`/`SELECT` column aliases inside `HAVING` so post-aggregate filtering runs on
/// expressions that are actually bound on aggregate rows (aggregates and grouping keys).
fn rewrite_having_with_return_aliases(items: &[ReturnItem], having: Option<&Expr>) -> Option<Expr> {
    let aliases: BTreeMap<String, Expr> = items
        .iter()
        .filter_map(|item| {
            let alias = item.alias.as_ref()?;
            Some((alias.clone(), item.expr.clone()))
        })
        .collect();
    having.map(|h| substitute_return_aliases_in_expr(h, &aliases))
}

/// True when `expr` contains any [`ExprKind::Aggregate`] (including nested).
fn expr_contains_aggregate(expr: &Expr) -> bool {
    if matches!(&expr.kind, ExprKind::Aggregate { .. }) {
        return true;
    }
    let mut found = false;
    for_each_immediate_child_expr(expr, |child| {
        found |= expr_contains_aggregate(child);
    });
    found
}

/// Compare aggregate specs ignoring output alias (used for deduplication).
fn aggregate_spec_body_eq(a: &AggregateSpec, b: &AggregateSpec) -> bool {
    a.func == b.func
        && a.distinct == b.distinct
        && a.expr == b.expr
        && a.expr2 == b.expr2
        && a.filter == b.filter
        && a.order_by == b.order_by
}

/// DFS collect unique [`AggregateSpec`] bodies from `expr` in stable pre-order.
fn collect_unique_aggregate_specs_from_expr(expr: &Expr, out: &mut Vec<AggregateSpec>) {
    if let ExprKind::Aggregate { .. } = &expr.kind
        && let Some(spec) = try_extract_aggregate(expr)
        && !out.iter().any(|s| aggregate_spec_body_eq(s, &spec))
    {
        out.push(spec);
    }
    for_each_immediate_child_expr(expr, |child| {
        collect_unique_aggregate_specs_from_expr(child, out);
    });
}

/// Extract aggregate functions from return items and optional `HAVING`.
fn extract_aggregates(
    items: &[ReturnItem],
    having: Option<&Expr>,
) -> (Vec<AggregateSpec>, Vec<ProjectColumn>) {
    let mut agg_specs = Vec::new();
    for item in items {
        collect_unique_aggregate_specs_from_expr(&item.expr, &mut agg_specs);
    }
    if let Some(h) = having {
        collect_unique_aggregate_specs_from_expr(h, &mut agg_specs);
    }
    let proj_cols = items
        .iter()
        .map(|item| ProjectColumn {
            expr: item.expr.clone(),
            alias: item.alias.as_ref().map(|a| Str::from(a.as_str())),
        })
        .collect();

    (agg_specs, proj_cols)
}

/// Try to extract an aggregate function from an expression.
fn try_extract_aggregate(expr: &Expr) -> Option<AggregateSpec> {
    let ExprKind::Aggregate {
        func,
        expr: agg_expr,
        expr2,
        distinct,
        order_by,
        filter,
    } = &expr.kind
    else {
        return None;
    };
    Some(AggregateSpec {
        func: *func,
        expr: agg_expr.as_deref().cloned(),
        expr2: expr2.as_deref().cloned(),
        distinct: *distinct,
        filter: filter.as_deref().cloned(),
        order_by: order_by.clone(),
        alias: None,
    })
}

/// Flatten AND chains into individual predicates.
pub(super) fn flatten_conjunction(expr: &Expr) -> Vec<Expr> {
    match &expr.kind {
        ExprKind::And(left, right) => {
            let mut result = flatten_conjunction(left);
            result.extend(flatten_conjunction(right));
            result
        }
        _ => vec![expr.clone()],
    }
}

/// Flatten OR chains into individual predicates.
pub(super) fn flatten_disjunction(expr: &Expr) -> Vec<Expr> {
    match &expr.kind {
        ExprKind::Or(left, right) => {
            let mut result = flatten_disjunction(left);
            result.extend(flatten_disjunction(right));
            result
        }
        _ => vec![expr.clone()],
    }
}
