use gleaph_gql::ast::*;
use gleaph_gql::token::Span;
use std::collections::{BTreeMap, BTreeSet};

use crate::expr_alias::substitute_return_aliases_in_expr;
use crate::expr_children::for_each_immediate_child_expr;
use crate::plan::*;
use crate::planner::{PlanAnnotations, PlanInputs, PlanOutput, PlannerError};

pub(crate) fn plan_result_statement(
    result: &ResultStatement,
    stage: usize,
    inputs: PlanInputs<'_>,
    output: PlanOutput<'_>,
) -> Result<(), PlannerError> {
    match result {
        ResultStatement::Return(ret) => {
            let PlanOutput { ops, .. } = output;
            plan_return(ret, ops);
        }
        ResultStatement::Select(sel) => plan_select(sel, stage, inputs, output)?,
        ResultStatement::Finish => {}
    }
    Ok(())
}

/// Lowers a `SELECT ... FROM` source into the same plan shape `USE <graph> MATCH ...` produces.
///
/// The source cannot be planned after rows already exist: a `SELECT` source defines the table its
/// projection reads, so a preceding part would leave the projection running against other
/// bindings. Both unsupported shapes therefore fail closed instead of emitting a plan that
/// silently ignores the source.
fn plan_select_source(
    source: &SelectSource,
    ops: &mut Vec<PlanOp>,
    _stage: usize,
    inputs: PlanInputs<'_>,
    _annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    // ISO/IEC 39075:2024 14.12 defines a `SELECT ... FROM` source by rewriting the statement to
    // `CALL VSC { <source statements> RETURN <new names> }` (Syntax Rules 25-27): `VSC` is the
    // incoming binding table, so a source appends to the preceding rows (a left lateral join), and
    // the returned names are the source's own names minus the incoming ones, which keeps the two
    // tables name-disjoint as §15.1 rule 9 requires. Building that rewrite as a query and planning
    // it means the source body gets the same pipeline and annotations a written-out rewrite would.
    let incoming: BTreeSet<String> = crate::binding_layout::derive_binding_layout(ops)
        .names()
        .iter()
        .map(|name| name.to_string())
        .collect();

    // The rewrite starts from the enclosing rows, so the body is planned with the incoming names
    // already bound (otherwise the source would scan for a name that is already on the row).
    let mut seeded = crate::planner::SeededBindings::default();
    let mut seed_kinds: BTreeMap<String, gleaph_gql::type_check::BindingKind> = BTreeMap::new();
    for name in &incoming {
        let Some(kind) = inputs.binding_kinds.get(name).copied() else {
            continue;
        };
        seed_kinds.insert(name.clone(), kind);
        if matches!(
            kind,
            gleaph_gql::type_check::BindingKind::Node
                | gleaph_gql::type_check::BindingKind::Edge
                | gleaph_gql::type_check::BindingKind::Path
        ) {
            seeded.bound.insert(name.clone());
        }
    }

    let sub_plan = match source {
        SelectSource::GraphMatchList(graph_matches) => {
            if graph_matches.is_empty() {
                return Err(PlannerError::InvalidStatement(
                    "SELECT ... FROM with an empty graph match list".into(),
                ));
            }
            let mut parts = Vec::with_capacity(graph_matches.len());
            let mut bound: BTreeSet<String> = BTreeSet::new();
            for graph_match in graph_matches {
                // A name declared by two items is one variable: the items are consecutive parts of
                // one body, so the later one constrains the earlier binding (14.12 rule 26 a).
                bound.extend(pattern_bound_names(&graph_match.match_statement.pattern));
                parts.push(SimpleQueryStatement::Focused {
                    graph: graph_match.graph.clone(),
                    body: Some(Box::new(SimpleQueryStatement::Match(
                        graph_match.match_statement.clone(),
                    ))),
                });
            }
            let returned = bound
                .into_iter()
                .filter(|name| !incoming.contains(name))
                .collect::<Vec<_>>();
            plan_source_rewrite(parts, returned, inputs, &seeded, &seed_kinds)?
        }
        SelectSource::QuerySpecification(specification) => {
            let (graph, query) = match specification {
                SelectQuerySpecification::Nested(query) => (None, query.as_ref()),
                SelectQuerySpecification::GraphNested { graph, query } => {
                    (Some(graph), query.as_ref())
                }
            };
            // The rewrite returns the nested query's outgoing columns (rule 25 g). Planning the
            // nested query once first is what makes those names available to the rewrite; the
            // rewrite then plans it again as the `CALL NQS` the standard names, so the final plan is
            // the planned rewrite rather than the probe.
            let nested = super::build_scoped_subquery_plan(None, query, inputs)?;
            let returned: Vec<String> = nested
                .output
                .columns
                .iter()
                .map(|column| column.name.to_string())
                .collect();
            let call = SimpleQueryStatement::InlineProcedureCall(InlineProcedureCall {
                span: Span::DUMMY,
                optional: false,
                use_graph: None,
                scope: gleaph_gql::ast::InlineProcedureScope::ImplicitAll,
                body: Box::new(query.clone()),
            });
            let parts = match graph {
                Some(graph) => vec![SimpleQueryStatement::Focused {
                    graph: graph.clone(),
                    body: Some(Box::new(call)),
                }],
                None => vec![call],
            };
            plan_source_rewrite(parts, returned, inputs, &seeded, &seed_kinds)?
        }
    };

    // `VSC` names the incoming bindings (rule 25 a), so the scope is explicit: empty when the
    // source starts the statement, the preceding parts' names otherwise.
    let scope = crate::plan::InlineProcedureScope::Explicit(
        incoming
            .iter()
            .map(|name| Str::from(name.as_str()))
            .collect(),
    );
    ops.push(super::scoped_subquery_op(sub_plan, scope, false));
    Ok(())
}

/// Plans the `CALL` body the standard's rewrite names, with `RETURN <returned>` as its result.
fn plan_source_rewrite(
    parts: Vec<SimpleQueryStatement>,
    returned: Vec<String>,
    inputs: PlanInputs<'_>,
    seeded: &crate::planner::SeededBindings,
    seed_kinds: &BTreeMap<String, gleaph_gql::type_check::BindingKind>,
) -> Result<PhysicalPlan, PlannerError> {
    // `SSBRIL` is empty when the source adds no name (every name it declares is already on the row):
    // the rewrite then has no RETURN and the body's rows pass through unchanged (rule 25 g).
    let result = if returned.is_empty() {
        None
    } else {
        let items: Vec<ReturnItem> = returned
            .into_iter()
            .map(|name| ReturnItem {
                span: Span::DUMMY,
                expr: Expr::new(ExprKind::Variable(name)),
                alias: None,
            })
            .collect();
        Some(ResultStatement::Return(Box::new(ReturnStatement {
            span: Span::DUMMY,
            set_quantifier: SetQuantifier::None,
            body: ReturnBody::Items {
                items,
                group_by: None,
                having: None,
                order_by: None,
                limit: None,
                offset: None,
            },
        })))
    };
    let query = LinearQueryStatement {
        span: Span::DUMMY,
        at_schema: None,
        prefix_bindings: Vec::new(),
        parts,
        result,
    };
    super::super::build_plan_with_binding_kinds_and_options_seeded(
        &query,
        inputs.options,
        Some(seed_kinds),
        inputs.schema,
        seeded,
    )
}

/// Collects the graph pattern variables a `MATCH` declares, in declaration order.
fn pattern_bound_names(pattern: &GraphPattern) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for path in &pattern.paths {
        if let Some(variable) = &path.variable {
            names.insert(variable.clone());
        }
        path_expr_bound_names(&path.expr, &mut names);
    }
    names
}

fn path_expr_bound_names(expr: &PathPatternExpr, names: &mut BTreeSet<String>) {
    match expr {
        PathPatternExpr::Term(term) => path_term_bound_names(term, names),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                path_term_bound_names(term, names);
            }
        }
    }
}

fn path_term_bound_names(term: &PathTerm, names: &mut BTreeSet<String>) {
    for factor in &term.factors {
        match &factor.primary {
            PathPrimary::Node(node) => {
                if let Some(variable) = &node.variable {
                    names.insert(variable.clone());
                }
            }
            PathPrimary::Edge(edge) => {
                if let Some(variable) = &edge.variable {
                    names.insert(variable.clone());
                }
            }
            PathPrimary::Parenthesized { variable, expr, .. } => {
                if let Some(variable) = variable {
                    names.insert(variable.clone());
                }
                path_expr_bound_names(expr, names);
            }
            PathPrimary::Simplified(_) => {}
        }
    }
}

/// Build a map from `RETURN`/`SELECT` alias name to the aliased expression.
fn return_aliases(items: &[ReturnItem]) -> BTreeMap<String, Expr> {
    items
        .iter()
        .filter_map(|item| {
            let alias = item.alias.as_ref()?;
            Some((alias.clone(), item.expr.clone()))
        })
        .collect()
}

/// Expand `ORDER BY` keys that reference `RETURN`/`SELECT` aliases into the underlying
/// expressions. This keeps sort keys resolvable on graph bindings so that the late-
/// projection pass can still push `Project` after `Sort`/`TopK`.
fn rewrite_order_by_with_return_aliases(
    order_by: &OrderByClause,
    aliases: &BTreeMap<String, Expr>,
) -> OrderByClause {
    OrderByClause {
        span: order_by.span,
        items: order_by
            .items
            .iter()
            .map(|item| SortItem {
                span: item.span,
                expr: substitute_return_aliases_in_expr(&item.expr, aliases),
                direction: item.direction,
                null_order: item.null_order,
            })
            .collect(),
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
            let aliases = return_aliases(items);
            let having_rw = rewrite_having_with_return_aliases(items, having.as_ref());
            let having_has_aggregate = having_rw.as_ref().is_some_and(expr_contains_aggregate);
            let is_aggregate = group_by.is_some()
                || items.iter().any(|item| expr_contains_aggregate(&item.expr))
                || having_has_aggregate;

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
            } else if is_aggregate {
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

            // Expand return aliases in ORDER BY so that sorting can run on graph bindings
            // before the late-projection optimization moves `Project` after `Sort`/`TopK`.
            // Aggregation paths keep aliases because the sort runs on aggregate/result rows.
            let order_by = if is_aggregate {
                order_by.clone()
            } else {
                order_by
                    .as_ref()
                    .map(|ob| rewrite_order_by_with_return_aliases(ob, &aliases))
            };

            if let Some(ob) = order_by {
                // Aggregate paths sort aggregate/result rows rather than graph bindings,
                // so the ADR 0081 delivery proof does not apply and the Sort is kept.
                if is_aggregate {
                    ops.push(PlanOp::Sort { order_by: ob });
                } else {
                    crate::ordered_delivery::emit_sort_or_mark_ordered_delivery(
                        ops,
                        ob,
                        limit.is_some() || offset.is_some(),
                    );
                }
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

fn plan_select(
    sel: &SelectStatement,
    stage: usize,
    inputs: PlanInputs<'_>,
    output: PlanOutput<'_>,
) -> Result<(), PlannerError> {
    let PlanOutput { ops, annotations } = output;
    if let Some(source) = &sel.source {
        plan_select_source(source, ops, stage, inputs, annotations)?;
    }

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

    // Expand SELECT aliases into ORDER BY sort keys on graph bindings, except for
    // aggregation paths where sorting runs on aggregate/result rows.
    let mut sort_on_graph_bindings = items.is_some() && group_by.is_none();
    let order_by = if let Some(items) = items {
        let aliases = return_aliases(items);
        let is_aggregate =
            group_by.is_some() || items.iter().any(|item| expr_contains_aggregate(&item.expr));
        if is_aggregate {
            sort_on_graph_bindings = false;
            order_by.clone()
        } else {
            order_by
                .as_ref()
                .map(|ob| rewrite_order_by_with_return_aliases(ob, &aliases))
        }
    } else {
        order_by.clone()
    };

    if let Some(ob) = order_by {
        if sort_on_graph_bindings {
            crate::ordered_delivery::emit_sort_or_mark_ordered_delivery(
                ops,
                ob,
                limit.is_some() || offset.is_some(),
            );
        } else {
            ops.push(PlanOp::Sort { order_by: ob });
        }
    }

    if limit.is_some() || offset.is_some() {
        ops.push(PlanOp::Limit {
            count: limit.as_ref().map(|l| l.count.clone()),
            offset: offset.as_ref().map(|o| o.count.clone()),
        });
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{Expr, ExprKind, OrderByClause, SortItem};
    use gleaph_gql::token::Span;

    fn prop(var: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::var(var)),
            property: property.into(),
        })
    }

    #[test]
    fn order_by_alias_expands_to_source_expression() {
        let aliases = BTreeMap::from([("b_name".to_string(), prop("b", "name"))]);
        let order_by = OrderByClause {
            span: Span::DUMMY,
            items: vec![SortItem {
                span: Span::DUMMY,
                expr: Expr::var("b_name"),
                direction: None,
                null_order: None,
            }],
        };

        let rewritten = rewrite_order_by_with_return_aliases(&order_by, &aliases);

        assert_eq!(rewritten.items.len(), 1);
        assert_eq!(rewritten.items[0].expr, prop("b", "name"));
    }

    #[test]
    fn order_by_alias_follows_chain_of_aliases() {
        let aliases = BTreeMap::from([
            ("x".to_string(), prop("n", "name")),
            ("y".to_string(), Expr::var("x")),
        ]);
        let order_by = OrderByClause {
            span: Span::DUMMY,
            items: vec![SortItem {
                span: Span::DUMMY,
                expr: Expr::var("y"),
                direction: Some(gleaph_gql::ast::SortDirection::Desc),
                null_order: None,
            }],
        };

        let rewritten = rewrite_order_by_with_return_aliases(&order_by, &aliases);

        assert_eq!(rewritten.items[0].expr, prop("n", "name"));
        assert_eq!(
            rewritten.items[0].direction,
            Some(gleaph_gql::ast::SortDirection::Desc)
        );
    }

    #[test]
    fn order_by_unmatched_alias_left_as_variable() {
        let aliases = BTreeMap::from([("b_name".to_string(), prop("b", "name"))]);
        let order_by = OrderByClause {
            span: Span::DUMMY,
            items: vec![SortItem {
                span: Span::DUMMY,
                expr: Expr::var("other"),
                direction: None,
                null_order: None,
            }],
        };

        let rewritten = rewrite_order_by_with_return_aliases(&order_by, &aliases);

        assert_eq!(rewritten.items[0].expr, Expr::var("other"));
    }
}
