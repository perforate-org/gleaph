//! Variable-length `Expand` / `ExpandFilter` (`{min,max}` quantifiers).

use std::collections::BTreeMap;
use std::sync::Arc;

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::plan::{
    EdgePayloadPredicate, EdgeVectorPredicate, ScanValue, Str, VarLenSpec,
};
use gleaph_graph_kernel::entry::EdgeLabelId;
use ic_stable_lara::VertexId;

use super::super::bindings::{
    edge_bindings_along_var_len_path, hop_aux_group, vertices_along_var_len_path,
};
use super::super::context::ExecuteCtx;
use super::super::path::{PathBinding, PathSearchNode, local_shard_id};
use super::{
    ExpandDst, edge_binding_matches_label_expr, expand_candidates_for_expand_op_into,
    expand_dst_binding, expand_dst_matches_prebound_vertex,
};
use crate::federation::{TraversalExpandSource, resolve_traversal_expand_source};
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::{
    EdgeSequenceOrder, PlanBinding, edge_to_projected_record, row_matches_all,
};
use crate::plan::query::row::PlanRow;

fn var_len_path_contains_vertex(
    states: &[PathSearchNode],
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
    hop_aux_binding: Option<&Str>,
    near_group_var: Option<&Str>,
    far_group_var: Option<&Str>,
    path_var: Option<&Str>,
    emit_path_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
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
    let caller = ctx.caller();
    let gleaph_weight_decoders = ctx.gleaph_weight_decoders;
    let hop_aux_key = hop_aux_binding.map(|name| name.as_ref());
    let mut out = Vec::new();
    for row in rows {
        match resolve_traversal_expand_source(ctx.store, row.get(src.as_ref()), direction).await? {
            None => continue,
            Some(TraversalExpandSource::LocalCsr(src_id)) => {
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
                    hop_aux_binding,
                    near_group_var,
                    far_group_var,
                    path_var,
                    emit_path_binding,
                    ctx.parameters,
                    indexed_edge_equality,
                    edge_payload_predicate,
                    edge_vector_predicate,
                    edge_property_projection,
                    dst_property_projection,
                    &evaluator,
                    &mut out,
                )?;
            }
        }
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
    hop_aux_binding: Option<&Str>,
    near_group_var: Option<&Str>,
    far_group_var: Option<&Str>,
    path_var: Option<&Str>,
    emit_path_binding: bool,
    parameters: &BTreeMap<String, Value>,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_payload_predicate: Option<&EdgePayloadPredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
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

    let mut states = vec![PathSearchNode {
        current: src_id,
        previous: None,
        edge: None,
        depth: 0,
    }];
    let mut queue = vec![0usize];
    let mut head = 0usize;
    let mut candidates = Vec::new();
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let hop_aux_key = hop_aux_binding.map(|name| name.to_string());
    let near_key = near_group_var.map(|v| v.to_string());
    let far_key = far_group_var.map(|v| v.to_string());
    let path_key = emit_path_binding
        .then(|| path_var.map(|v| v.to_string()))
        .flatten();
    let dst_key = dst.to_string();
    let shard_id = local_shard_id(store);

    while head < queue.len() {
        let state_idx = queue[head];
        head += 1;
        let depth = states[state_idx].depth;
        let current = states[state_idx].current;
        if depth >= min_hops && depth <= max_hops {
            let edge_dst = ExpandDst::Local(current);
            if expand_dst_matches_prebound_vertex(row, dst, edge_dst) {
                let expanded = build_var_len_row(
                    store,
                    execution,
                    row,
                    edge_key.as_deref(),
                    hop_aux_key.as_deref(),
                    near_key.as_deref(),
                    far_key.as_deref(),
                    path_key.as_deref(),
                    shard_id,
                    dst_key.as_str(),
                    edge_dst,
                    &states,
                    state_idx,
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
        expand_candidates_for_expand_op_into(
            store,
            execution,
            current,
            direction,
            label_id,
            label_expr,
            EdgeSequenceOrder::Descending,
            indexed_edge_equality,
            edge_payload_predicate,
            edge_vector_predicate,
            parameters,
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
            states.push(PathSearchNode {
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
    execution: &crate::gql_execution_context::GqlExecutionContext,
    row: &PlanRow,
    edge_key: Option<&str>,
    hop_aux_key: Option<&str>,
    near_key: Option<&str>,
    far_key: Option<&str>,
    path_key: Option<&str>,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    dst_key: &str,
    dst: ExpandDst,
    states: &[PathSearchNode],
    state_idx: usize,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<PlanRow, PlanQueryError> {
    let dst_binding = expand_dst_binding(store, execution, dst, dst_property_projection)?;
    let mut updates = vec![(dst_key, dst_binding)];
    let path_edges = (edge_key.is_some() || hop_aux_key.is_some()).then(|| {
        edge_bindings_along_var_len_path(
            states,
            state_idx,
            |state| state.edge.as_ref(),
            |state| state.previous,
        )
    });
    if let Some(edge_key) = edge_key {
        let edges = path_edges
            .as_ref()
            .expect("edge or hop_aux requested path edges");
        let edge_binding = if edge_property_projection.is_some_and(|props| !props.is_empty()) {
            let list = edges
                .iter()
                .cloned()
                .map(|edge| {
                    edge_to_projected_record(
                        store,
                        execution,
                        edge,
                        edge_property_projection.expect("checked non-empty"),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            PlanBinding::Value(Value::List(list))
        } else {
            PlanBinding::EdgeGroup(edges.clone())
        };
        updates.push((edge_key, edge_binding));
    }
    if let Some(hop_aux_key) = hop_aux_key {
        let edges = path_edges
            .as_ref()
            .expect("edge or hop_aux requested path edges");
        updates.push((hop_aux_key, PlanBinding::Value(hop_aux_group(edges))));
    }
    if let Some(near_key) = near_key {
        let vertices = vertices_along_var_len_path(
            states,
            state_idx,
            |state| state.current,
            |state| state.edge.is_some(),
            |state| state.previous,
            true,
        );
        updates.push((near_key, PlanBinding::VertexGroup(vertices)));
    }
    if let Some(far_key) = far_key {
        let vertices = vertices_along_var_len_path(
            states,
            state_idx,
            |state| state.current,
            |state| state.edge.is_some(),
            |state| state.previous,
            false,
        );
        updates.push((far_key, PlanBinding::VertexGroup(vertices)));
    }
    if let Some(path_key) = path_key {
        updates.push((
            path_key,
            PlanBinding::Path(PathBinding {
                shard_id,
                states: Arc::new(states.to_vec()),
                leaf_state_idx: state_idx,
            }),
        ));
    }
    Ok(row.fork(updates))
}
