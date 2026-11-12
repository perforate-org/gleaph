//! Variable-length `Expand` / `ExpandFilter` (`{min,max}` quantifiers).

use std::collections::BTreeMap;
use std::sync::Arc;

use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql_planner::plan::PlanOp;
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
use crate::plan::query::executor::expand::{ExpandCommon, expand_common};
use crate::plan::query::executor::{EdgeSequenceOrder, PlanBinding, edge_to_projected_record};
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

pub(crate) async fn execute_var_len_expand(
    op: &PlanOp,
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    dst_filter: &[Expr],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let ExpandCommon {
        src,
        direction,
        label,
        ..
    } = expand_common(op);
    let execution = &ctx.execution;

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

    let caller = ctx.caller();
    let mut out = Vec::new();
    for row in rows {
        match resolve_traversal_expand_source(ctx.store, row.get(src.as_ref()), direction).await? {
            None => continue,
            Some(TraversalExpandSource::LocalCsr(src_id)) => {
                collect_var_len_expand_rows(
                    op,
                    &VarLenEnv {
                        store: ctx.store,
                        execution,
                        parameters: ctx.parameters,
                        caller,
                    },
                    &row,
                    src_id,
                    label_id,
                    dst_filter,
                    &mut out,
                )?;
            }
        }
    }
    Ok(out)
}

/// What a var-length collection runs against: the execution, its graph, and the evaluator over both.
///
/// Its two callers - the entry that owns the op and the streaming walk that has already built an evaluator - each hold these; the
/// eighteen other arguments this function used to take were the op's fields, which it now reads through `expand_common`.
pub(crate) struct VarLenEnv<'a> {
    pub(crate) store: &'a crate::facade::GraphStore,
    pub(crate) execution: &'a crate::gql_execution_context::GqlExecutionContext,
    pub(crate) parameters: &'a BTreeMap<String, Value>,
    pub(crate) caller: Option<candid::Principal>,
}

pub(crate) fn collect_var_len_expand_rows(
    op: &PlanOp,
    env: &VarLenEnv<'_>,
    row: &PlanRow,
    src_id: VertexId,
    label_id: Option<EdgeLabelId>,
    dst_filter: &[Expr],
    out: &mut Vec<PlanRow>,
) -> Result<(), PlanQueryError> {
    let ExpandCommon {
        edge,
        dst,
        direction,
        label_expr,
        var_len,
        emit_edge_binding,
        hop_aux_binding,
        near_group_var,
        far_group_var,
        path_var,
        emit_path_binding,
        indexed_edge_equality,
        edge_inline_property_predicate,
        edge_inline_vector_predicate,
        edge_property_projection,
        dst_property_projection,
        ..
    } = expand_common(op);
    let var_len = var_len.expect("a var-length collection is called for a var-length expansion");
    let VarLenEnv {
        store,
        execution,
        parameters,
        caller,
    } = env;
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
            // Destination filters are planned against the *vertex* binding. Building the row
            // first projects the vertex into a record value when the plan reads properties from
            // it, and a label predicate (`IS LABELED`) on a record silently evaluates false -
            // which is how `(b:Film)` turned a working variable-length path into an empty result.
            // Evaluate the filters on the vertex, exactly like the fixed-hop path's
            // `vertex_row_matches_dst_filters`, and only then project.
            let dst_passes = expand_dst_matches_prebound_vertex(row, dst, edge_dst)
                && crate::plan::query::executor::vertex_row_matches_dst_filters(
                    store, parameters, dst, current, dst_filter, *caller, execution,
                )?;
            if dst_passes {
                let expanded = build_var_len_row(
                    super::RowBuildContext {
                        store,
                        execution,
                        row,
                        edge_key: edge_key.as_deref(),
                        hop_aux_key: hop_aux_key.as_deref(),
                        dst_key: dst_key.as_str(),
                        edge_property_projection,
                        dst_property_projection,
                    },
                    VarLenPathView {
                        near_key: near_key.as_deref(),
                        far_key: far_key.as_deref(),
                        path_key: path_key.as_deref(),
                        shard_id,
                        states: &states,
                        state_idx,
                    },
                    edge_dst,
                )?;
                out.push(expanded);
            }
        }
        if depth >= max_hops {
            continue;
        }

        candidates.clear();
        expand_candidates_for_expand_op_into(
            super::candidates::CandidateScan {
                store,
                execution,
                src_id: current,
                direction,
                sequence_order: EdgeSequenceOrder::Ascending,
            },
            super::candidates::CandidateFilters {
                indexed_edge_equality,
                edge_inline_property_predicate,
                edge_inline_vector_predicate,
                parameters,
            },
            label_id,
            label_expr,
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

/// The path state a var-length row is built from: the run so far, where in it the row sits, and the keys the hop binds.
///
/// Six values - three `Option<&str>` keys, a shard id, the state slice and an index into it - so a call site could name the far key where
/// the path key belongs.
#[derive(Clone, Copy)]
struct VarLenPathView<'a> {
    near_key: Option<&'a str>,
    far_key: Option<&'a str>,
    path_key: Option<&'a str>,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    states: &'a [PathSearchNode],
    state_idx: usize,
}

fn build_var_len_row(
    context: super::RowBuildContext<'_>,
    path: VarLenPathView<'_>,
    dst: ExpandDst,
) -> Result<PlanRow, PlanQueryError> {
    let super::RowBuildContext {
        store,
        execution,
        row,
        edge_key,
        hop_aux_key,
        dst_key,
        edge_property_projection,
        dst_property_projection,
    } = context;
    let VarLenPathView {
        near_key,
        far_key,
        path_key,
        shard_id,
        states,
        state_idx,
    } = path;
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
