//! Preparation for `GLEAPH_WEIGHT(edgeVar)` traversal intrinsic.

use std::collections::{BTreeMap, BTreeSet};

use gleaph_gql::ast::{Expr, ExprKind, LetBinding};
use gleaph_gql::types::LabelExpr;
use gleaph_gql_planner::plan::{PlanOp, ProjectColumn, ScanValue, Str, VarLenSpec};
use gleaph_graph_kernel::entry::{PreparedWeightDecoder, WeightProfilePrepareError};

use crate::facade::GraphStore;

use super::error::PlanQueryError;

const GLEAPH_WEIGHT: &str = "gleaph_weight";

/// Per-edge-variable prepared decoders for `GLEAPH_WEIGHT`.
pub(crate) fn prepare_gleaph_weight_decoders(
    store: &GraphStore,
    ops: &[PlanOp],
) -> Result<Option<BTreeMap<String, PreparedWeightDecoder>>, PlanQueryError> {
    let mut edge_vars = BTreeSet::new();
    for_each_expr_in_ops(ops, &mut |expr| {
        if let Some(ev) = gleaph_weight_edge_var(expr) {
            edge_vars.insert(ev);
        }
    });

    if edge_vars.is_empty() {
        return Ok(None);
    }

    let mut out = BTreeMap::new();
    for edge_var in edge_vars {
        let decoder = decoder_for_gleaph_weight_edge(store, ops, &edge_var)?;
        out.insert(edge_var, decoder);
    }
    Ok(Some(out))
}

fn gleaph_weight_edge_var(expr: &Expr) -> Option<String> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
    else {
        return None;
    };
    if *distinct {
        return None;
    }
    let Some(last) = name.parts.last().map(|s| s.as_str()) else {
        return None;
    };
    if !last.eq_ignore_ascii_case(GLEAPH_WEIGHT) || name.parts.len() != 1 {
        return None;
    }
    if args.len() != 1 {
        return None;
    }
    let ExprKind::Variable(v) = &args[0].kind else {
        return None;
    };
    Some(v.clone())
}

fn decoder_for_gleaph_weight_edge(
    store: &GraphStore,
    ops: &[PlanOp],
    edge_var: &str,
) -> Result<PreparedWeightDecoder, PlanQueryError> {
    let producer = first_edge_producer_for_var(ops, edge_var).ok_or_else(|| PlanQueryError::GleaphWeight {
        message: format!(
            "GLEAPH_WEIGHT({edge_var}): no Expand/ExpandFilter/ShortestPath binds variable '{edge_var}'"
        ),
    })?;

    match producer {
        EdgeProducer::Expand {
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        }
        | EdgeProducer::ExpandFilter {
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        } => {
            if label_expr.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH_WEIGHT({edge_var}): edge pattern must use a single fixed label, not a label expression"
                    ),
                });
            }
            if var_len.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH_WEIGHT({edge_var}): variable-length edge patterns are not supported"
                    ),
                });
            }
            if indexed_edge_equality.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH_WEIGHT({edge_var}): indexed edge equality expansion is not supported"
                    ),
                });
            }
            if hop_aux_binding.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH_WEIGHT({edge_var}): hop auxiliary bindings are not supported"
                    ),
                });
            }
            let label_name = label.ok_or_else(|| PlanQueryError::GleaphWeight {
                message: format!(
                    "GLEAPH_WEIGHT({edge_var}): edge pattern must have exactly one fixed edge label"
                ),
            })?;
            finish_decoder_from_label_name(store, edge_var, label_name.as_ref())
        }
        EdgeProducer::ShortestPath {
            label,
            label_expr,
            var_len,
        } => {
            if label_expr.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH_WEIGHT({edge_var}): shortest-path edge pattern must use a single fixed label"
                    ),
                });
            }
            if var_len.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH_WEIGHT({edge_var}): variable-length bounds on shortest-path are not supported for GLEAPH_WEIGHT"
                    ),
                });
            }
            let label_name = label.ok_or_else(|| PlanQueryError::GleaphWeight {
                message: format!(
                    "GLEAPH_WEIGHT({edge_var}): shortest-path must have exactly one fixed edge label"
                ),
            })?;
            finish_decoder_from_label_name(store, edge_var, label_name.as_ref())
        }
    }
}

fn finish_decoder_from_label_name(
    store: &GraphStore,
    edge_var: &str,
    label_name: &str,
) -> Result<PreparedWeightDecoder, PlanQueryError> {
    let label_id = store
        .label_id(label_name)
        .ok_or_else(|| PlanQueryError::GleaphWeight {
            message: format!("GLEAPH_WEIGHT({edge_var}): unknown edge label '{label_name}'"),
        })?;
    if !label_id.is_edge_inline_capable() {
        return Err(PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH_WEIGHT({edge_var}): label '{label_name}' is not an edge-inline label id"
            ),
        });
    }
    let profile = store.edge_label_weight_profile(label_id).ok_or_else(|| {
        PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH_WEIGHT({edge_var}): edge label '{label_name}' has no weight profile configured"
            ),
        }
    })?;
    profile.prepare().map_err(
        |e: WeightProfilePrepareError| PlanQueryError::GleaphWeight {
            message: format!("GLEAPH_WEIGHT({edge_var}): invalid weight profile: {e}"),
        },
    )
}

enum EdgeProducer<'a> {
    Expand {
        label: Option<&'a Str>,
        label_expr: &'a Option<LabelExpr>,
        var_len: &'a Option<VarLenSpec>,
        indexed_edge_equality: &'a Option<(Str, ScanValue)>,
        hop_aux_binding: &'a Option<Str>,
    },
    ExpandFilter {
        label: Option<&'a Str>,
        label_expr: &'a Option<LabelExpr>,
        var_len: &'a Option<VarLenSpec>,
        indexed_edge_equality: &'a Option<(Str, ScanValue)>,
        hop_aux_binding: &'a Option<Str>,
    },
    ShortestPath {
        label: Option<&'a Str>,
        label_expr: &'a Option<LabelExpr>,
        var_len: &'a Option<VarLenSpec>,
    },
}

fn first_edge_producer_for_var<'a>(ops: &'a [PlanOp], edge_var: &str) -> Option<EdgeProducer<'a>> {
    for op in ops {
        if let Some(p) = edge_producer_from_op(op, edge_var) {
            return Some(p);
        }
    }
    None
}

fn edge_producer_from_op<'a>(op: &'a PlanOp, edge_var: &str) -> Option<EdgeProducer<'a>> {
    match op {
        PlanOp::Expand {
            edge,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
            ..
        } if edge.as_ref() == edge_var => Some(EdgeProducer::Expand {
            label: label.as_ref(),
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        }),
        PlanOp::ExpandFilter {
            edge,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
            ..
        } if edge.as_ref() == edge_var => Some(EdgeProducer::ExpandFilter {
            label: label.as_ref(),
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        }),
        PlanOp::ShortestPath {
            edge,
            label,
            label_expr,
            var_len,
            ..
        } if edge.as_ref() == edge_var => Some(EdgeProducer::ShortestPath {
            label: label.as_ref(),
            label_expr,
            var_len,
        }),
        PlanOp::HashJoin { left, right, .. } => first_edge_producer_for_var(left, edge_var)
            .or_else(|| first_edge_producer_for_var(right, edge_var)),
        PlanOp::CartesianProduct { left, right } => first_edge_producer_for_var(left, edge_var)
            .or_else(|| first_edge_producer_for_var(right, edge_var)),
        PlanOp::OptionalMatch { sub_plan } => first_edge_producer_for_var(sub_plan, edge_var),
        PlanOp::UseGraph {
            sub_plan: Some(sub),
            ..
        } => first_edge_producer_for_var(sub, edge_var),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            first_edge_producer_for_var(&sub_plan.ops, edge_var)
        }
        PlanOp::SetOperation { right, .. } => first_edge_producer_for_var(&right.ops, edge_var),
        _ => None,
    }
}

fn for_each_expr_in_ops(ops: &[PlanOp], f: &mut impl FnMut(&Expr)) {
    for op in ops {
        for_each_expr_in_op(op, f);
    }
}

fn for_each_expr_in_op(op: &PlanOp, f: &mut impl FnMut(&Expr)) {
    match op {
        PlanOp::PropertyFilter { predicates, .. } => {
            for p in predicates {
                visit_expr(p, f);
            }
        }
        PlanOp::Filter { condition } => visit_expr(condition, f),
        PlanOp::ExpandFilter { dst_filter, .. } => {
            for p in dst_filter {
                visit_expr(p, f);
            }
        }
        PlanOp::Let { bindings } => {
            for LetBinding { value, .. } in bindings {
                visit_expr(value, f);
            }
        }
        PlanOp::For { list, .. } => visit_expr(list, f),
        PlanOp::Project { columns, .. } => {
            for ProjectColumn { expr, .. } in columns {
                visit_expr(expr, f);
            }
        }
        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                visit_expr(&item.expr, f);
            }
        }
        PlanOp::Limit { count, offset } => {
            if let Some(e) = count {
                visit_expr(e, f);
            }
            if let Some(e) = offset {
                visit_expr(e, f);
            }
        }
        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            for item in &order_by.items {
                visit_expr(&item.expr, f);
            }
            visit_expr(k, f);
            if let Some(e) = offset {
                visit_expr(e, f);
            }
        }
        PlanOp::Materialize { columns, .. } => {
            for ProjectColumn { expr, .. } in columns {
                visit_expr(expr, f);
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for e in group_by {
                visit_expr(e, f);
            }
            for spec in aggregates {
                if let Some(e) = &spec.expr {
                    visit_expr(e, f);
                }
                if let Some(e2) = &spec.expr2 {
                    visit_expr(e2, f);
                }
                if let Some(fe) = &spec.filter {
                    visit_expr(fe, f);
                }
                if let Some(ob) = &spec.order_by {
                    for item in &ob.items {
                        visit_expr(&item.expr, f);
                    }
                }
            }
        }
        PlanOp::CallProcedure { args, .. } => {
            for a in args {
                visit_expr(a, f);
            }
        }
        PlanOp::HashJoin { left, right, .. } => {
            for_each_expr_in_ops(left, f);
            for_each_expr_in_ops(right, f);
        }
        PlanOp::CartesianProduct { left, right } => {
            for_each_expr_in_ops(left, f);
            for_each_expr_in_ops(right, f);
        }
        PlanOp::OptionalMatch { sub_plan } => for_each_expr_in_ops(sub_plan, f),
        PlanOp::UseGraph {
            sub_plan: Some(sub),
            ..
        } => for_each_expr_in_ops(sub, f),
        PlanOp::InlineProcedureCall { sub_plan, .. } => for_each_expr_in_ops(&sub_plan.ops, f),
        PlanOp::SetOperation { right, .. } => for_each_expr_in_ops(&right.ops, f),
        _ => {}
    }
}

fn visit_expr(expr: &Expr, f: &mut impl FnMut(&Expr)) {
    f(expr);
    match &expr.kind {
        ExprKind::Literal(_)
        | ExprKind::Variable(_)
        | ExprKind::Parameter(_)
        | ExprKind::SessionUser
        | ExprKind::CurrentDate
        | ExprKind::CurrentTime
        | ExprKind::CurrentTimestamp
        | ExprKind::CurrentLocalTime
        | ExprKind::CurrentLocalTimestamp => {}
        ExprKind::Paren(inner)
        | ExprKind::Not(inner)
        | ExprKind::IsNull(inner)
        | ExprKind::IsNotNull(inner) => {
            visit_expr(inner, f);
        }
        ExprKind::UnaryOp { expr: inner, .. } => visit_expr(inner, f),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right) => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        ExprKind::Compare { left, right, .. } => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        ExprKind::PropertyAccess { expr, .. } | ExprKind::ElementId(expr) => visit_expr(expr, f),
        ExprKind::FunctionCall { args, .. } => {
            for a in args {
                visit_expr(a, f);
            }
        }
        ExprKind::Coalesce(exprs)
        | ExprKind::ListLiteral(exprs)
        | ExprKind::ListConstructor { items: exprs, .. } => {
            for e in exprs {
                visit_expr(e, f);
            }
        }
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (_, e) in fields {
                visit_expr(e, f);
            }
        }
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            visit_expr(operand, f);
            for wc in when_clauses {
                visit_expr(&wc.condition, f);
                visit_expr(&wc.result, f);
            }
            if let Some(e) = else_clause {
                visit_expr(e, f);
            }
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            for wc in when_clauses {
                visit_expr(&wc.condition, f);
                visit_expr(&wc.result, f);
            }
            if let Some(e) = else_clause {
                visit_expr(e, f);
            }
        }
        ExprKind::IsTruth { expr, .. } => visit_expr(expr, f),
        ExprKind::Cast { expr, .. } => visit_expr(expr, f),
        ExprKind::Aggregate { expr, expr2, .. } => {
            if let Some(e) = expr {
                visit_expr(e, f);
            }
            if let Some(e2) = expr2 {
                visit_expr(e2, f);
            }
        }
        _ => {}
    }
}
