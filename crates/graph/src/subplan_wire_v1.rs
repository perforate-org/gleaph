//! Candid wire format v1 for remote `USE GRAPH` sub-plans (structured ops, not only a query string).
//!
//! Reconstructs [`gleaph_gql_planner::PlanOp`] via [`gleaph_gql::parser::parse_expr`] and runs the same
//! pipeline as [`crate::graph_registry::subplan_to_routed_query`].

use candid::{CandidType, Deserialize};
use gleaph_gql::ast::{Expr, NullOrder, OrderByClause, SortDirection, SortItem};
use gleaph_gql::parser;
use gleaph_gql::token::Span;
use gleaph_gql_planner::PlanOp;
use gleaph_gql_planner::plan::{AggregateSpec, ProjectColumn, YieldColumn};

use crate::GleaphError;

/// Version tag for forward-compatible decoding.
#[derive(Clone, Debug, CandidType, Deserialize)]
pub struct SubplanWireV1 {
    pub version: u32,
    pub ops: Vec<PlanOpWire>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
pub enum PlanOpWire {
    CallProcedure {
        name: Vec<String>,
        args: Vec<String>,
        optional: bool,
        yield_cols: Option<Vec<WireYieldCol>>,
    },
    Filter {
        expr: String,
    },
    PropertyFilter {
        predicates: Vec<String>,
        stage: u32,
    },
    Project {
        distinct: bool,
        columns: Vec<WireProjectCol>,
    },
    Limit {
        count: Option<String>,
        offset: Option<String>,
    },
    Sort {
        items: Vec<WireSortItem>,
    },
    TopK {
        items: Vec<WireSortItem>,
        k: String,
        offset: Option<String>,
    },
    Aggregate {
        group_by: Vec<String>,
        aggregates: Vec<WireAggSpec>,
    },
}

#[derive(Clone, Debug, CandidType, Deserialize)]
pub struct WireYieldCol {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
pub struct WireProjectCol {
    pub expr: String,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
pub struct WireSortItem {
    pub expr: String,
    pub descending: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
pub struct WireAggSpec {
    pub func: String,
    pub expr: Option<String>,
    pub distinct: bool,
    pub alias: String,
}

fn parse_opt_expr(s: &Option<String>) -> Result<Option<Expr>, GleaphError> {
    match s {
        None => Ok(None),
        Some(text) if text.is_empty() => Ok(None),
        Some(text) => Ok(Some(parser::parse_expr(text)?)),
    }
}

fn parse_req_expr(text: &str) -> Result<Expr, GleaphError> {
    parser::parse_expr(text).map_err(GleaphError::from)
}

fn wire_order_clause(items: &[WireSortItem]) -> Result<OrderByClause, GleaphError> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        out.push(SortItem {
            span: Span::DUMMY,
            expr: parse_req_expr(&it.expr)?,
            direction: Some(if it.descending {
                SortDirection::Descending
            } else {
                SortDirection::Ascending
            }),
            null_order: it.nulls_first.map(|nf| {
                if nf {
                    NullOrder::First
                } else {
                    NullOrder::Last
                }
            }),
        });
    }
    Ok(OrderByClause {
        span: Span::DUMMY,
        items: out,
    })
}

/// Decodes v1 wire ops into physical plan operators (same shapes as routed `USE GRAPH`).
pub fn wire_v1_to_plan_ops(wire: &SubplanWireV1) -> Result<Vec<PlanOp>, GleaphError> {
    if wire.version != 1 {
        return Err(GleaphError::Execution(
            gleaph_gql_executor::ExecutionError::InvalidPlan(format!(
                "unsupported subplan wire version {}",
                wire.version
            )),
        ));
    }
    let mut out = Vec::with_capacity(wire.ops.len());
    for op in &wire.ops {
        match op {
            PlanOpWire::CallProcedure {
                name,
                args,
                optional,
                yield_cols,
            } => {
                let mut arg_exprs = Vec::with_capacity(args.len());
                for a in args {
                    arg_exprs.push(parse_req_expr(a)?);
                }
                let yc = yield_cols.as_ref().map(|cols| {
                    cols.iter()
                        .map(|c| YieldColumn {
                            name: c.name.clone().into(),
                            alias: c.alias.as_ref().map(|a| a.as_str().into()),
                        })
                        .collect::<Vec<_>>()
                });
                out.push(PlanOp::CallProcedure {
                    name: name.iter().map(|s| s.as_str().into()).collect(),
                    args: arg_exprs,
                    yield_columns: yc,
                    optional: *optional,
                });
            }
            PlanOpWire::Filter { expr } => {
                out.push(PlanOp::Filter {
                    condition: parse_req_expr(expr)?,
                });
            }
            PlanOpWire::PropertyFilter { predicates, stage } => {
                let mut preds = Vec::with_capacity(predicates.len());
                for p in predicates {
                    preds.push(parse_req_expr(p)?);
                }
                out.push(PlanOp::PropertyFilter {
                    predicates: preds,
                    stage: *stage as usize,
                });
            }
            PlanOpWire::Project { distinct, columns } => {
                let mut cols = Vec::with_capacity(columns.len());
                for c in columns {
                    cols.push(ProjectColumn {
                        expr: parse_req_expr(&c.expr)?,
                        alias: c.alias.as_ref().map(|a| a.as_str().into()),
                    });
                }
                out.push(PlanOp::Project {
                    columns: cols,
                    distinct: *distinct,
                });
            }
            PlanOpWire::Limit { count, offset } => {
                out.push(PlanOp::Limit {
                    count: parse_opt_expr(count)?,
                    offset: parse_opt_expr(offset)?,
                });
            }
            PlanOpWire::Sort { items } => {
                out.push(PlanOp::Sort {
                    order_by: wire_order_clause(items)?,
                });
            }
            PlanOpWire::TopK { items, k, offset } => {
                out.push(PlanOp::TopK {
                    order_by: wire_order_clause(items)?,
                    k: parse_req_expr(k)?,
                    offset: parse_opt_expr(offset)?,
                });
            }
            PlanOpWire::Aggregate {
                group_by,
                aggregates,
            } => {
                let mut gb = Vec::with_capacity(group_by.len());
                for g in group_by {
                    gb.push(parse_req_expr(g)?);
                }
                let mut aggs = Vec::with_capacity(aggregates.len());
                for a in aggregates {
                    aggs.push(AggregateSpec {
                        func: a.func.as_str().into(),
                        expr: parse_opt_expr(&a.expr)?,
                        distinct: a.distinct,
                        alias: Some(a.alias.as_str().into()),
                    });
                }
                out.push(PlanOp::Aggregate {
                    group_by: gb,
                    aggregates: aggs,
                });
            }
        }
    }
    Ok(out)
}

/// Serializes supported routed sub-plans to wire v1 (inverse of [`wire_v1_to_plan_ops`] for those shapes).
#[allow(dead_code)] // Encoding entry point for hosts that prefer structured payloads over rescanning plans.
pub fn plan_ops_to_wire_v1(ops: &[PlanOp]) -> Result<SubplanWireV1, GleaphError> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let w = match op {
            PlanOp::CallProcedure {
                name,
                args,
                yield_columns,
                optional,
            } => PlanOpWire::CallProcedure {
                name: name.iter().map(|s| s.to_string()).collect(),
                args: args
                    .iter()
                    .map(|e| crate::graph_registry::render_expr(e).map_err(GleaphError::from))
                    .collect::<Result<Vec<_>, GleaphError>>()?,
                optional: *optional,
                yield_cols: yield_columns.as_ref().map(|yc| {
                    yc.iter()
                        .map(|c| WireYieldCol {
                            name: c.name.to_string(),
                            alias: c.alias.as_ref().map(|a| a.to_string()),
                        })
                        .collect()
                }),
            },
            PlanOp::Filter { condition } => PlanOpWire::Filter {
                expr: crate::graph_registry::render_expr(condition)?,
            },
            PlanOp::PropertyFilter { predicates, stage } => PlanOpWire::PropertyFilter {
                predicates: predicates
                    .iter()
                    .map(|p| crate::graph_registry::render_expr(p).map_err(GleaphError::from))
                    .collect::<Result<Vec<_>, GleaphError>>()?,
                stage: *stage as u32,
            },
            PlanOp::Project { columns, distinct } => PlanOpWire::Project {
                distinct: *distinct,
                columns: columns
                    .iter()
                    .map(|c| {
                        Ok::<_, GleaphError>(WireProjectCol {
                            expr: crate::graph_registry::render_expr(&c.expr)?,
                            alias: c.alias.as_ref().map(|a| a.to_string()),
                        })
                    })
                    .collect::<Result<Vec<_>, GleaphError>>()?,
            },
            PlanOp::Limit { count, offset } => PlanOpWire::Limit {
                count: count
                    .as_ref()
                    .map(|e| crate::graph_registry::render_expr(e).map_err(GleaphError::from))
                    .transpose()?,
                offset: offset
                    .as_ref()
                    .map(|e| crate::graph_registry::render_expr(e).map_err(GleaphError::from))
                    .transpose()?,
            },
            PlanOp::Sort { order_by } => PlanOpWire::Sort {
                items: order_by
                    .items
                    .iter()
                    .map(|it| {
                        Ok::<_, GleaphError>(WireSortItem {
                            expr: crate::graph_registry::render_expr(&it.expr)?,
                            descending: matches!(
                                it.direction,
                                Some(SortDirection::Desc | SortDirection::Descending)
                            ),
                            nulls_first: it.null_order.map(|o| matches!(o, NullOrder::First)),
                        })
                    })
                    .collect::<Result<Vec<_>, GleaphError>>()?,
            },
            PlanOp::TopK {
                order_by,
                k,
                offset,
            } => PlanOpWire::TopK {
                items: order_by
                    .items
                    .iter()
                    .map(|it| {
                        Ok::<_, GleaphError>(WireSortItem {
                            expr: crate::graph_registry::render_expr(&it.expr)?,
                            descending: matches!(
                                it.direction,
                                Some(SortDirection::Desc | SortDirection::Descending)
                            ),
                            nulls_first: it.null_order.map(|o| matches!(o, NullOrder::First)),
                        })
                    })
                    .collect::<Result<Vec<_>, GleaphError>>()?,
                k: crate::graph_registry::render_expr(k)?,
                offset: offset
                    .as_ref()
                    .map(|e| crate::graph_registry::render_expr(e).map_err(GleaphError::from))
                    .transpose()?,
            },
            PlanOp::Aggregate {
                group_by,
                aggregates,
            } => PlanOpWire::Aggregate {
                group_by: group_by
                    .iter()
                    .map(|e| crate::graph_registry::render_expr(e).map_err(GleaphError::from))
                    .collect::<Result<Vec<_>, GleaphError>>()?,
                aggregates: aggregates
                    .iter()
                    .map(|a| {
                        let alias = a.alias.as_ref().ok_or_else(|| {
                            GleaphError::Execution(
                                gleaph_gql_executor::ExecutionError::InvalidPlan(
                                    "wire v1 aggregate requires alias".to_owned(),
                                ),
                            )
                        })?;
                        Ok::<_, GleaphError>(WireAggSpec {
                            func: a.func.to_string(),
                            expr: a
                                .expr
                                .as_ref()
                                .map(|e| {
                                    crate::graph_registry::render_expr(e).map_err(GleaphError::from)
                                })
                                .transpose()?,
                            distinct: a.distinct,
                            alias: alias.to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, GleaphError>>()?,
            },
            _ => {
                return Err(GleaphError::Execution(
                    gleaph_gql_executor::ExecutionError::InvalidPlan(
                        "plan op not representable in subplan wire v1".to_owned(),
                    ),
                ));
            }
        };
        out.push(w);
    }
    Ok(SubplanWireV1 {
        version: 1,
        ops: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{Expr, ExprKind};
    use gleaph_gql_planner::plan::{ProjectColumn, YieldColumn};

    #[test]
    fn wire_v1_round_trips_simple_routed_subplan() {
        let ops = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: Some("lbl".into()),
                }],
                distinct: false,
            },
        ];
        let wire = plan_ops_to_wire_v1(&ops).expect("to wire");
        let back = wire_v1_to_plan_ops(&wire).expect("from wire");
        assert_eq!(back.len(), ops.len());
        let q1 = crate::graph_registry::subplan_to_routed_query(&ops).expect("q1");
        let q2 = crate::graph_registry::subplan_to_routed_query(&back).expect("q2");
        assert_eq!(q1, q2);
    }
}
