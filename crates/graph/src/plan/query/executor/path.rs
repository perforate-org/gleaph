//! Shortest-path search (`ShortestPath` plan operator).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::hash::Hasher;
use std::rc::Rc;
use std::sync::Arc;

use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, Expr, ExprKind};
use gleaph_gql::numeric_ops::{NumericOpError, eval_binary_numeric};
use gleaph_gql::numeric_order::{NormalizedNumeric, NumericOrderError, normalized_numeric_parts};
use gleaph_gql::types::{EdgeDirection, LabelExpr, PathElement};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::BindingLayout;
use gleaph_gql_planner::plan::{PlanOp, ShortestMode, ShortestPathCost, Str, VarLenSpec};
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeSlotIndex, PreparedWeightDecoder};
use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::OutEdgeOrder;
use nohash_hasher::{IntMap, IntSet};
use rapidhash::fast::RapidHasher;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::bindings::EdgeBinding;
use super::context::QueryExprEvaluator;
use super::expand::{ExpandCandidate, ExpandDst, edge_binding_for_expand, expand_candidates_into};
use super::{EdgeSequenceOrder, PlanBinding, vertex_binding_for_traversal};
use crate::facade::{EdgeHandle, GraphStore};

#[derive(Clone, Debug)]
pub(crate) struct PathSearchNode {
    current: VertexId,
    previous: Option<usize>,
    edge: Option<EdgeBinding>,
    depth: u64,
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
    label: Option<&Str>,
    label_expr: &Option<LabelExpr>,
    var_len: &Option<VarLenSpec>,
    cost: &ShortestPathCost,
    parameters: &BTreeMap<String, Value>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    remaining_ops: &[PlanOp],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    if matches!(mode, ShortestMode::ShortestK(_)) {
        return Err(PlanQueryError::UnsupportedOp("ShortestPath.ShortestK"));
    }
    if label_expr.is_some() {
        return Err(PlanQueryError::UnsupportedOp("ShortestPath.label_expr"));
    }

    let label_id = label.and_then(|label| store.edge_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let shard_id = local_shard_id(store);
    let store_hop_edges = emit_edge_binding || emit_path_binding;
    let mut out = Vec::new();
    for row in rows {
        let Some(src_id) = vertex_binding_for_traversal(store, &row, src, Some(direction)).await?
        else {
            continue;
        };
        let Some(dst_id) = vertex_binding_for_traversal(store, &row, dst, None).await? else {
            continue;
        };
        let paths = match cost {
            ShortestPathCost::HopCount => shortest_paths_between(
                store,
                src_id,
                dst_id,
                direction,
                label_id,
                var_len,
                mode,
                store_hop_edges,
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
                    var_len,
                    mode,
                    store_hop_edges,
                )?
            }
            ShortestPathCost::EdgeCostExpr { edge_var, expr } => weighted_shortest_paths_between(
                store,
                src_id,
                dst_id,
                direction,
                label_id,
                var_len,
                edge_var.as_ref(),
                expr,
                mode,
                parameters,
                gleaph_weight_decoders,
                store_hop_edges,
            )?,
        };
        let ShortestPathSearchResult { states, found } = paths;
        out.reserve(found.len());
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
                        let edge_binding = match path_states[state_idx].edge {
                            Some(edge_binding) => PlanBinding::Edge(edge_binding),
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
    ) -> Result<(), PlanQueryError> {
        match self {
            Self::Forward { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_forward");
                let mut expand_err = None;
                store.for_each_directed_out_edges_for_label_with_values(
                    current,
                    label,
                    OutEdgeOrder::Descending,
                    |edge| {
                        if expand_err.is_some() {
                            return;
                        }
                        if let Ok(Some(edge_dst @ ExpandDst::Local(_))) =
                            ExpandDst::from_edge(store, &edge)
                        {
                            match edge_binding_for_expand(
                                store,
                                current,
                                EdgeDirection::PointingRight,
                                edge,
                            ) {
                                Ok(binding) => out.push((edge_dst, binding)),
                                Err(err) => expand_err = Some(err),
                            }
                        }
                    },
                )?;
                if let Some(err) = expand_err {
                    return Err(err);
                }
            }
            Self::Reverse { label } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_reverse");
                let mut expand_err = None;
                store.for_each_directed_in_edges_for_label_with_values(
                    current,
                    label,
                    OutEdgeOrder::Descending,
                    |edge| {
                        if expand_err.is_some() {
                            return;
                        }
                        if let Ok(Some(edge_dst @ ExpandDst::Local(_))) =
                            ExpandDst::from_edge(store, &edge)
                        {
                            match edge_binding_for_expand(
                                store,
                                current,
                                EdgeDirection::PointingLeft,
                                edge,
                            ) {
                                Ok(binding) => out.push((edge_dst, binding)),
                                Err(err) => expand_err = Some(err),
                            }
                        }
                    },
                )?;
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
                        match edge_binding_for_expand(
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
}

mod materialize;
mod search;
mod weighted;

pub(crate) use materialize::{
    edge_element_id_bytes, local_shard_id, materialize_path_from_search_states,
    path_binding_to_value, vertex_element_id_bytes,
};
pub(crate) use search::shortest_paths_between;
pub(crate) use weighted::{
    WeightedCost, WeightedCostOrderKey, decode_direct_gleaph_weight_hop_cost,
    weighted_shortest_can_use_hop_count, weighted_shortest_paths_between,
};

#[cfg(test)]
mod tests;
