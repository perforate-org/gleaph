//! Variable-length `Expand` / `ExpandFilter` (`{min,max}` quantifiers).

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{Str, VarLenSpec};
use gleaph_graph_kernel::entry::EdgeLabelId;
use ic_stable_lara::VertexId;

use super::super::context::ExecuteCtx;
use super::{
    ExpandDst, build_expanded_row, edge_binding_matches_label_expr, expand_candidates_into,
    expand_dst_binding, expand_dst_matches_prebound_vertex,
};
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::EdgeBinding;
use crate::plan::query::executor::{
    EdgeSequenceOrder, PlanBinding, row_matches_all, vertex_binding_for_traversal,
};
use crate::plan::query::row::PlanRow;

struct VarLenSearchNode {
    current: VertexId,
    previous: Option<usize>,
    edge: Option<EdgeBinding>,
    depth: u64,
}

fn var_len_path_contains_vertex(
    states: &[VarLenSearchNode],
    mut state_idx: usize,
    vertex: VertexId,
) -> bool {
    loop {
        let state = &states[state_idx];
        if state.current == vertex {
            return true;
        }
        let Some(previous) = state.previous else {
            return false;
        };
        state_idx = previous;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_var_len_expand(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&str>,
    label_expr: Option<&LabelExpr>,
    execution: &crate::gql_execution_context::GqlExecutionContext,
    var_len: &VarLenSpec,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = match label {
        Some(label) => execution
            .resolved_edge_label_id(label)
            .map(Some)
            .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                namespace: "edge",
                name: label.to_owned(),
            })?,
        None => None,
    };

    let evaluator = ctx.expr_evaluator(None);
    let mut out = Vec::new();
    for row in rows {
        if row
            .get(src.as_ref())
            .is_some_and(|binding| matches!(binding, PlanBinding::RemoteVertex(_)))
        {
            return Err(PlanQueryError::UnsupportedOp("Expand.var_len.remote"));
        }
        let Some(src_id) =
            vertex_binding_for_traversal(ctx.store, &row, src, Some(direction)).await?
        else {
            continue;
        };
        collect_var_len_expand_rows(
            ctx.store,
            &row,
            src_id,
            edge,
            dst,
            direction,
            label_id,
            label_expr,
            execution,
            var_len,
            dst_filter,
            emit_edge_binding,
            edge_property_projection,
            dst_property_projection,
            &evaluator,
            &mut out,
        )?;
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_var_len_expand_rows(
    store: &crate::facade::GraphStore,
    row: &PlanRow,
    src_id: VertexId,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    label_expr: Option<&LabelExpr>,
    execution: &crate::gql_execution_context::GqlExecutionContext,
    var_len: &VarLenSpec,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
    evaluator: &super::super::context::QueryExprEvaluator<'_>,
    out: &mut Vec<PlanRow>,
) -> Result<(), PlanQueryError> {
    let vertex_count = u64::from(u32::from(store.vertex_count()));
    let max_hops = var_len
        .max
        .unwrap_or_else(|| vertex_count.saturating_sub(1));
    let min_hops = var_len.min;

    let mut states = vec![VarLenSearchNode {
        current: src_id,
        previous: None,
        edge: None,
        depth: 0,
    }];
    let mut queue = vec![0usize];
    let mut head = 0usize;
    let mut candidates = Vec::new();
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let dst_key = dst.to_string();

    while head < queue.len() {
        let state_idx = queue[head];
        head += 1;
        let depth = states[state_idx].depth;
        let current = states[state_idx].current;
        let last_edge = states[state_idx].edge.clone();
        if depth >= min_hops && depth <= max_hops {
            let edge_dst = ExpandDst::Local(current);
            if expand_dst_matches_prebound_vertex(row, dst, edge_dst) {
                let expanded = build_var_len_row(
                    store,
                    row,
                    edge_key.as_deref(),
                    dst_key.as_str(),
                    edge_dst,
                    depth,
                    last_edge.as_ref(),
                    edge_property_projection,
                    dst_property_projection,
                )?;
                if row_matches_all(evaluator, &expanded, dst_filter)? {
                    out.push(expanded);
                }
            }
        }
        if depth >= max_hops {
            continue;
        }

        candidates.clear();
        expand_candidates_into(
            store,
            current,
            direction,
            label_id,
            EdgeSequenceOrder::Descending,
            None,
            None,
            None,
            &BTreeMap::new(),
            &mut candidates,
        )?;
        for (edge_dst, edge_binding) in candidates.iter().cloned() {
            if let Some(expr) = label_expr
                && !edge_binding_matches_label_expr(execution, expr, &edge_binding)
            {
                continue;
            }
            let ExpandDst::Local(next) = edge_dst else {
                continue;
            };
            if var_len_path_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let next_depth = depth + 1;
            let next_state_idx = states.len();
            states.push(VarLenSearchNode {
                current: next,
                previous: Some(state_idx),
                edge: Some(edge_binding),
                depth: next_depth,
            });
            queue.push(next_state_idx);
        }
    }
    Ok(())
}

fn build_var_len_row(
    store: &crate::facade::GraphStore,
    row: &PlanRow,
    edge_key: Option<&str>,
    dst_key: &str,
    dst: ExpandDst,
    depth: u64,
    last_edge: Option<&EdgeBinding>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<PlanRow, PlanQueryError> {
    if depth == 0 {
        let dst_binding = expand_dst_binding(store, dst, dst_property_projection)?;
        let mut updates = vec![(dst_key, dst_binding)];
        if let Some(edge_key) = edge_key {
            updates.push((edge_key, PlanBinding::Value(Value::Null)));
        }
        return Ok(row.fork(updates));
    }
    let edge_binding = last_edge
        .cloned()
        .expect("non-zero depth implies a traversed edge");
    build_expanded_row(
        None,
        store,
        row,
        edge_key,
        dst_key,
        dst,
        edge_binding,
        edge_property_projection,
        dst_property_projection,
    )
}
