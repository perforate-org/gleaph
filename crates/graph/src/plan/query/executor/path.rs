//! Shortest-path search (`ShortestPath` plan operator).

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gleaph_gql::Value;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::BindingLayout;
use gleaph_gql_planner::plan::{PlanOp, ShortestMode, ShortestPathCost, Str, VarLenSpec};
use gleaph_graph_kernel::entry::{Edge, EdgeDirectedness, EdgeLabelId, PreparedWeightDecoder};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::LabeledEdgePayloadBatch;
use ic_stable_lara::labeled::{LabeledEdgePayloadBatchScratch, OutEdgeOrder};

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::bindings::EdgeBinding;
use super::expand::{ExpandCandidate, ExpandDst, edge_binding_for_scanned_expand};
use crate::federation::resolve_traversal_expand_local_csr;

use super::PlanBinding;
use crate::facade::{GraphStore, GraphStoreError};

mod materialize;
mod search;
pub mod weighted;

pub(crate) use materialize::{
    edge_element_id_bytes, local_shard_id, path_binding_to_value, vertex_element_id_bytes,
};
pub(crate) use search::shortest_paths_between;
pub(crate) use weighted::{weighted_shortest_can_use_hop_count, weighted_shortest_paths_between};

#[cfg(test)]
mod tests;

#[derive(Clone, Debug)]
pub(crate) struct PathSearchNode {
    pub(crate) current: VertexId,
    pub(crate) previous: Option<usize>,
    pub(crate) edge: Option<EdgeBinding>,
    pub(crate) depth: u64,
}

/// Lazy path result from [`execute_shortest_path`]: shares [`Arc`] search state across many rows.
#[derive(Clone, Debug)]
pub struct PathBinding {
    pub(crate) shard_id: gleaph_graph_kernel::federation::ShardId,
    pub(crate) states: Arc<Vec<PathSearchNode>>,
    pub(crate) leaf_state_idx: usize,
}

impl PartialEq for PathBinding {
    fn eq(&self, other: &Self) -> bool {
        self.shard_id == other.shard_id
            && self.leaf_state_idx == other.leaf_state_idx
            && Arc::ptr_eq(&self.states, &other.states)
    }
}

impl Eq for PathBinding {}

impl PathBinding {
    pub(crate) fn materialize_cache_key(&self) -> (usize, usize) {
        (Arc::as_ptr(&self.states) as usize, self.leaf_state_idx)
    }
}

pub(crate) struct ShortestPathSearchResult {
    pub(crate) states: Vec<PathSearchNode>,
    pub(crate) found: Vec<usize>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_shortest_path(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    src: &Str,
    dst: &Str,
    edge: &Str,
    path_var: Option<&Str>,
    emit_edge_binding: bool,
    emit_path_binding: bool,
    mode: ShortestMode,
    direction: EdgeDirection,
    label: Option<&str>,
    execution: &crate::gql_execution_context::GqlExecutionContext,
    label_expr: &Option<LabelExpr>,
    var_len: &Option<VarLenSpec>,
    cost: &ShortestPathCost,
    parameters: &BTreeMap<String, Value>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    remaining_ops: &[PlanOp],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = if label_expr.is_some() {
        None
    } else {
        match label {
            Some(label) => execution
                .resolved_edge_label_id(label)
                .map(Some)
                .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                    namespace: "edge",
                    name: label.to_owned(),
                })?,
            None => None,
        }
    };

    let shard_id = local_shard_id(store);
    let store_hop_edges = emit_edge_binding || emit_path_binding;
    let mut out = Vec::new();
    for row in rows {
        let Some(src_id) = resolve_traversal_expand_local_csr(
            store,
            row.get(src.as_ref()),
            direction,
            "ShortestPath.src.peer",
        )
        .await?
        else {
            continue;
        };
        let Some(dst_id) = resolve_traversal_expand_local_csr(
            store,
            row.get(dst.as_ref()),
            direction,
            "ShortestPath.dst.peer",
        )
        .await?
        else {
            continue;
        };
        let paths = match cost {
            ShortestPathCost::HopCount => shortest_paths_between(
                store,
                src_id,
                dst_id,
                direction,
                label_id,
                label_expr.as_ref(),
                execution,
                var_len,
                mode,
                store_hop_edges,
                emit_edge_binding,
            )?,
            ShortestPathCost::EdgeCostExpr { edge_var, expr }
                if weighted_shortest_can_use_hop_count(mode, expr) =>
            {
                shortest_paths_between(
                    store,
                    src_id,
                    dst_id,
                    direction,
                    label_id,
                    label_expr.as_ref(),
                    execution,
                    var_len,
                    mode,
                    store_hop_edges,
                    emit_edge_binding,
                )?
            }
            ShortestPathCost::EdgeCostExpr { edge_var, expr } => weighted_shortest_paths_between(
                store,
                src_id,
                dst_id,
                direction,
                label_id,
                label_expr.as_ref(),
                execution,
                var_len,
                edge_var.as_ref(),
                expr,
                mode,
                parameters,
                gleaph_weight_decoders,
                store_hop_edges,
                emit_edge_binding,
            )?,
        };
        let ShortestPathSearchResult { states, found } = paths;
        let edge_key = emit_edge_binding.then(|| edge.to_string());
        let path_key = emit_path_binding
            .then(|| path_var.map(|path_var| path_var.to_string()))
            .flatten();
        let path_states = Arc::new(states);
        let path_only_rows = path_key.as_ref().is_some_and(|path_key| {
            super::super::live_vars::shortest_path_may_emit_path_only_rows(
                remaining_ops,
                path_key,
                src.as_ref(),
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
            )
        });
        let path_only_layout = path_only_rows.then(|| {
            let path_key = path_key.as_ref().expect("path_only_rows implies path_key");
            Rc::new(BindingLayout::single(Str::from(path_key.as_str())))
        });
        if mode.emits_path_group() {
            if found.is_empty() {
                continue;
            }
            let path_group: Arc<[PathBinding]> = found
                .iter()
                .map(|&state_idx| PathBinding {
                    shard_id,
                    states: Arc::clone(&path_states),
                    leaf_state_idx: state_idx,
                })
                .collect();
            let edge_group: Arc<[EdgeBinding]> = found
                .iter()
                .filter_map(|&state_idx| path_states[state_idx].edge.clone())
                .collect();
            let path_group_binding = PlanBinding::PathGroup(path_group);
            let row = if path_only_rows {
                let path_key = path_key.as_ref().expect("path_only_rows implies path_key");
                PlanRow::with_layout_and_binding(
                    Rc::clone(path_only_layout.as_ref().expect("path_only_layout")),
                    path_key,
                    path_group_binding,
                )
            } else {
                let mut updates = Vec::with_capacity(2);
                if let Some(edge_key) = edge_key.as_deref() {
                    let edge_binding = if edge_group.is_empty() {
                        PlanBinding::Value(Value::Null)
                    } else {
                        PlanBinding::EdgeGroup(edge_group)
                    };
                    updates.push((edge_key, edge_binding));
                }
                if let Some(path_key) = path_key.as_deref() {
                    updates.push((path_key, path_group_binding));
                }
                row.fork(updates)
            };
            out.push(row);
            continue;
        }
        out.reserve(found.len());
        for state_idx in found {
            let path_binding = PlanBinding::Path(PathBinding {
                shard_id,
                states: Arc::clone(&path_states),
                leaf_state_idx: state_idx,
            });
            let row = if path_only_rows {
                let path_key = path_key.as_ref().expect("path_only_rows implies path_key");
                PlanRow::with_layout_and_binding(
                    Rc::clone(path_only_layout.as_ref().expect("path_only_layout")),
                    path_key,
                    path_binding,
                )
            } else {
                let path_updates: Vec<(&str, PlanBinding)> = {
                    let mut updates = Vec::with_capacity(2);
                    if let Some(edge_key) = edge_key.as_deref() {
                        let edge_binding = match &path_states[state_idx].edge {
                            Some(edge_binding) => PlanBinding::Edge(edge_binding.clone()),
                            None => PlanBinding::Value(Value::Null),
                        };
                        updates.push((edge_key, edge_binding));
                    }
                    if let Some(path_key) = path_key.as_deref() {
                        updates.push((path_key, path_binding));
                    }
                    updates
                };
                row.fork(path_updates)
            };
            out.push(row);
            if matches!(mode, ShortestMode::AnyShortest) {
                break;
            }
        }
    }
    Ok(out)
}

/// Controls whether shortest-path expansion hydrates edge payloads and reuses batch scratch.
pub(crate) struct ShortestExpandOptions<'a> {
    pub load_payloads: bool,
    pub payload_scratch: Option<&'a mut LabeledEdgePayloadBatchScratch<Edge>>,
}

/// Holds a catalog [`EdgeLabelId`] per shortest-path search; expansion uses directed/undirected
/// GraphStore APIs so wire MSB packing stays internal.
#[derive(Clone, Copy)]
pub(crate) enum ShortestFixedLabelExpand {
    Forward { label: EdgeLabelId },
    Reverse { label: EdgeLabelId },
    Undirected { label: EdgeLabelId },
}

impl ShortestFixedLabelExpand {
    pub(crate) fn new(
        direction: EdgeDirection,
        catalog: EdgeLabelId,
    ) -> Result<Self, PlanQueryError> {
        match direction {
            EdgeDirection::PointingRight => Ok(Self::Forward { label: catalog }),
            EdgeDirection::PointingLeft => Ok(Self::Reverse { label: catalog }),
            EdgeDirection::Undirected => Ok(Self::Undirected { label: catalog }),
            other => Err(PlanQueryError::UnsupportedDirection(other)),
        }
    }

    pub(crate) fn expand_into(
        self,
        store: &GraphStore,
        current: VertexId,
        out: &mut Vec<ExpandCandidate>,
        options: ShortestExpandOptions<'_>,
    ) -> Result<(), PlanQueryError> {
        match self {
            Self::Forward { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_forward");
                let mut expand_err = None;
                let mut visit_edge = |edge: Edge| {
                    if expand_err.is_some() {
                        return;
                    }
                    if let Ok(Some(edge_dst @ ExpandDst::Local(_))) =
                        ExpandDst::from_edge(store, &edge)
                    {
                        match edge_binding_for_scanned_expand(
                            store,
                            current,
                            EdgeDirection::PointingRight,
                            edge,
                        ) {
                            Ok(binding) => out.push((edge_dst, binding)),
                            Err(err) => expand_err = Some(err),
                        }
                    }
                };
                if options.load_payloads {
                    if let Some(scratch) = options.payload_scratch {
                        store.for_each_directed_out_edges_for_label_with_payloads_reusing(
                            current,
                            label,
                            OutEdgeOrder::Descending,
                            scratch,
                            &mut visit_edge,
                        )?;
                    } else {
                        store.for_each_directed_out_edges_for_label_with_payloads(
                            current,
                            label,
                            OutEdgeOrder::Descending,
                            &mut visit_edge,
                        )?;
                    }
                } else {
                    store.for_each_directed_out_edges_for_label_topology_unchecked(
                        current,
                        label,
                        OutEdgeOrder::Descending,
                        &mut visit_edge,
                    )?;
                }
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Reverse { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_reverse");
                let mut expand_err = None;
                let mut visit_edge = |edge: Edge| {
                    if expand_err.is_some() {
                        return;
                    }
                    if let Ok(Some(edge_dst @ ExpandDst::Local(_))) =
                        ExpandDst::from_edge(store, &edge)
                    {
                        match edge_binding_for_scanned_expand(
                            store,
                            current,
                            EdgeDirection::PointingLeft,
                            edge,
                        ) {
                            Ok(binding) => out.push((edge_dst, binding)),
                            Err(err) => expand_err = Some(err),
                        }
                    }
                };
                if options.load_payloads {
                    if let Some(scratch) = options.payload_scratch {
                        store.for_each_directed_in_edges_for_label_with_payloads_reusing(
                            current,
                            label,
                            OutEdgeOrder::Descending,
                            scratch,
                            &mut visit_edge,
                        )?;
                    } else {
                        store.for_each_directed_in_edges_for_label_with_payloads(
                            current,
                            label,
                            OutEdgeOrder::Descending,
                            &mut visit_edge,
                        )?;
                    }
                } else {
                    store.for_each_directed_in_edges_for_label_topology_unchecked(
                        current,
                        label,
                        OutEdgeOrder::Descending,
                        &mut visit_edge,
                    )?;
                }
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Undirected { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_undirected");
                let mut expand_err = None;
                store.for_each_undirected_edges_for_label_unchecked(current, label, |edge| {
                    if expand_err.is_some() {
                        return;
                    }
                    if let Ok(Some(edge_dst @ ExpandDst::Local(_))) =
                        ExpandDst::from_edge(store, &edge)
                    {
                        match edge_binding_for_scanned_expand(
                            store,
                            current,
                            EdgeDirection::Undirected,
                            edge,
                        ) {
                            Ok(binding) => out.push((edge_dst, binding)),
                            Err(err) => expand_err = Some(err),
                        }
                    }
                })?;
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// Expands fixed-label edges while passing payload bytes by reference (no `Edge` clone).
    pub(crate) fn expand_payload_slices<Visit>(
        self,
        store: &GraphStore,
        current: VertexId,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        mut visit: Visit,
    ) -> Result<(), PlanQueryError>
    where
        Visit: FnMut(&Edge, &[u8]) -> Result<(), PlanQueryError>,
    {
        match self {
            Self::Forward { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_forward_slices");
                let mut expand_err = None;
                let mut visit_edge = |edge: &Edge, payload: &[u8]| {
                    if expand_err.is_some() {
                        return;
                    }
                    match visit(edge, payload) {
                        Ok(()) => {}
                        Err(err) => expand_err = Some(err),
                    }
                };
                store
                    .for_each_directed_out_edges_for_label_with_payload_slices_reusing(
                        current,
                        label,
                        OutEdgeOrder::Descending,
                        scratch,
                        &mut visit_edge,
                    )
                    .map_err(PlanQueryError::from)?;
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Reverse { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_reverse_slices");
                let mut expand_err = None;
                let mut visit_edge = |edge: &Edge, payload: &[u8]| {
                    if expand_err.is_some() {
                        return;
                    }
                    match visit(edge, payload) {
                        Ok(()) => {}
                        Err(err) => expand_err = Some(err),
                    }
                };
                store
                    .for_each_directed_in_edges_for_label_with_payload_slices_reusing(
                        current,
                        label,
                        OutEdgeOrder::Descending,
                        scratch,
                        &mut visit_edge,
                    )
                    .map_err(PlanQueryError::from)?;
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Undirected { .. } => {
                return Err(PlanQueryError::UnsupportedOp(
                    "weighted shortest-path slice expand does not support undirected labels yet"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// Expands fixed-label edges in LARA payload batches (dense slab bulk-read groups).
    pub(crate) fn expand_payload_batches<Visit>(
        self,
        store: &GraphStore,
        current: VertexId,
        scratch: &mut LabeledEdgePayloadBatchScratch<Edge>,
        mut visit: Visit,
    ) -> Result<(), PlanQueryError>
    where
        Visit: for<'b> FnMut(&'b LabeledEdgePayloadBatch<'b, Edge>) -> Result<(), PlanQueryError>,
    {
        match self {
            Self::Forward { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_forward_slices");
                let storage_label =
                    LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw());
                let mut expand_err = None;
                store
                    .visit_out_edge_payload_batches_for_label(
                        current,
                        storage_label,
                        OutEdgeOrder::Descending,
                        scratch,
                        |batch| {
                            if expand_err.is_some() {
                                return;
                            }
                            match visit(&batch) {
                                Ok(()) => {}
                                Err(err) => expand_err = Some(err),
                            }
                        },
                    )
                    .map_err(GraphStoreError::from)
                    .map_err(PlanQueryError::from)?;
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Reverse { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_reverse_slices");
                let storage_label =
                    LaraLabelId::from_raw(label.pack(EdgeDirectedness::Directed).raw());
                let mut expand_err = None;
                store
                    .visit_in_edge_payload_batches_for_label(
                        current,
                        storage_label,
                        OutEdgeOrder::Descending,
                        scratch,
                        |batch| {
                            if expand_err.is_some() {
                                return;
                            }
                            match visit(&batch) {
                                Ok(()) => {}
                                Err(err) => expand_err = Some(err),
                            }
                        },
                    )
                    .map_err(GraphStoreError::from)
                    .map_err(PlanQueryError::from)?;
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Undirected { .. } => {
                return Err(PlanQueryError::UnsupportedOp(
                    "weighted shortest-path batch expand does not support undirected labels yet"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}
