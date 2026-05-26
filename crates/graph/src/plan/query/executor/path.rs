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

fn shortest_paths_between(
    store: &GraphStore,
    src: VertexId,
    dst: VertexId,
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    var_len: &Option<VarLenSpec>,
    mode: ShortestMode,
    store_hop_edges: bool,
) -> Result<ShortestPathSearchResult, PlanQueryError> {
    let bounds = var_len.unwrap_or(VarLenSpec {
        min: 1,
        max: Some(1),
    });
    let vertex_count = u64::from(u32::from(store.vertex_count()));
    let max_hops = bounds.max.unwrap_or_else(|| vertex_count.saturating_sub(1));

    let mut found_depth = None;
    let mut found = Vec::new();
    let mut any_visited = if matches!(mode, ShortestMode::AnyShortest) && bounds.min <= 1 {
        let mut visited = IntSet::default();
        visited.insert(u32::from(src));
        Some(visited)
    } else {
        None
    };
    let mut states = vec![PathSearchNode {
        current: src,
        previous: None,
        edge: None,
        depth: 0,
    }];
    let mut queue = vec![0usize];
    let mut queue_head = 0usize;
    let mut candidates = Vec::new();
    let fixed_label_expand = match label_id {
        Some(lid) => Some(ShortestFixedLabelExpand::new(direction, lid)?),
        None => None,
    };

    while queue_head < queue.len() {
        let state_idx = queue[queue_head];
        queue_head += 1;
        let current = states[state_idx].current;
        let depth = states[state_idx].depth;
        if found_depth.is_some_and(|d| depth > d) {
            break;
        }
        if depth >= bounds.min && current == dst {
            found_depth = Some(depth);
            found.push(state_idx);
            if matches!(mode, ShortestMode::AnyShortest) {
                break;
            }
            continue;
        }
        if found_depth.is_some_and(|d| depth >= d) {
            continue;
        }
        if depth >= max_hops {
            continue;
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _expand_scope = bench_scope("shortest_bfs_expand");
        candidates.clear();
        match fixed_label_expand {
            Some(prep) => prep.expand_into(store, current, &mut candidates)?,
            None => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _generic_scope = bench_scope("shortest_bfs_expand_generic");
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
            }
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _relax_scope = bench_scope("shortest_bfs_relax_neighbors");
        for (edge_dst, edge_binding) in candidates.iter().copied() {
            let ExpandDst::Local(next) = edge_dst else {
                continue;
            };
            let next_depth = depth + 1;
            if let Some(visited) = any_visited.as_mut() {
                if !visited.insert(u32::from(next)) {
                    continue;
                }
            } else if path_search_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let next_state_idx = states.len();
            states.push(PathSearchNode {
                current: next,
                previous: Some(state_idx),
                edge: store_hop_edges.then_some(edge_binding),
                depth: next_depth,
            });
            if next == dst && next_depth >= bounds.min {
                if matches!(mode, ShortestMode::AnyShortest) {
                    return Ok(ShortestPathSearchResult {
                        states,
                        found: vec![next_state_idx],
                    });
                }
                found_depth = Some(next_depth);
                found.push(next_state_idx);
                continue;
            }
            queue.push(next_state_idx);
        }
    }

    Ok(ShortestPathSearchResult { states, found })
}

pub(crate) fn weighted_shortest_can_use_hop_count(mode: ShortestMode, cost_expr: &Expr) -> bool {
    let ExprKind::Literal(value) = &cost_expr.kind else {
        return false;
    };
    let Ok(cost) = WeightedCost::from_value(value.clone()) else {
        return false;
    };
    match mode {
        ShortestMode::AnyShortest => true,
        ShortestMode::AllShortest => matches!(cost.cmp(&WeightedCost::zero()), Ordering::Greater),
        ShortestMode::ShortestK(_) => false,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WeightedCost {
    pub(crate) value: Value,
    pub(crate) order_key: WeightedCostOrderKey,
}

#[derive(Clone, Debug)]
pub(crate) enum WeightedCostOrderKey {
    Zero,
    Uint128(u128),
    Float64(f64),
    Normalized(Box<Option<NormalizedNumeric>>),
}

impl WeightedCost {
    fn zero() -> Self {
        Self {
            value: Value::Int32(0),
            order_key: WeightedCostOrderKey::Zero,
        }
    }

    fn from_validated_non_negative_float32(value: f32) -> Self {
        Self {
            value: Value::Float32(value),
            order_key: if value == 0.0 {
                WeightedCostOrderKey::Zero
            } else {
                WeightedCostOrderKey::Float64(f64::from(value))
            },
        }
    }

    pub(crate) fn from_value(value: Value) -> Result<Self, PlanQueryError> {
        if matches!(value, Value::Null) {
            return Err(PlanQueryError::GleaphCost {
                message: "shortest-path edge cost must not be NULL".into(),
            });
        }
        if !value.is_numeric() {
            return Err(PlanQueryError::GleaphCost {
                message: format!("shortest-path edge cost must be numeric, got {value:?}"),
            });
        }
        if let Some(order_key) = compact_weighted_cost_order_key(&value)? {
            return Ok(Self { value, order_key });
        }
        let numeric = match normalized_numeric_parts(&value) {
            Err(NumericOrderError::NonFiniteFloat) => {
                return Err(PlanQueryError::GleaphCost {
                    message: "shortest-path edge cost must be finite".into(),
                });
            }
            Err(NumericOrderError::UnsupportedValue) => {
                return Err(PlanQueryError::GleaphCost {
                    message: "shortest-path edge cost uses unsupported numeric value".into(),
                });
            }
            Ok(numeric) => numeric,
        };
        if numeric.as_ref().is_some_and(|numeric| numeric.negative) {
            return Err(PlanQueryError::GleaphCost {
                message: "shortest-path edge cost must be non-negative".into(),
            });
        }
        Ok(Self {
            value,
            order_key: WeightedCostOrderKey::Normalized(Box::new(numeric)),
        })
    }

    pub(crate) fn checked_add(&self, hop: &Self) -> Result<Self, PlanQueryError> {
        if matches!(self.order_key, WeightedCostOrderKey::Zero) {
            return Ok(hop.clone());
        }
        if matches!(hop.order_key, WeightedCostOrderKey::Zero) {
            return Ok(self.clone());
        }
        if let (Value::Float32(left), Value::Float32(right)) = (&self.value, &hop.value) {
            let sum = left + right;
            if !sum.is_finite() {
                return Err(PlanQueryError::GleaphCost {
                    message: "shortest-path edge cost must be finite".into(),
                });
            }
            return Ok(Self::from_validated_non_negative_float32(sum));
        }
        let sum = eval_binary_numeric(self.value.clone(), BinaryOp::Add, hop.value.clone())
            .map_err(map_weighted_cost_add_err)?;
        Self::from_value(sum)
    }

    fn cmp(&self, other: &Self) -> Ordering {
        compare_weighted_cost_order_key(self, other)
    }

    fn cmp_infallible(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

fn compact_weighted_cost_order_key(
    value: &Value,
) -> Result<Option<WeightedCostOrderKey>, PlanQueryError> {
    if value.is_signed_int() {
        let Some(value) = value.as_i128() else {
            return Ok(None);
        };
        if value < 0 {
            return Err(PlanQueryError::GleaphCost {
                message: "shortest-path edge cost must be non-negative".into(),
            });
        }
        return Ok(if value == 0 {
            Some(WeightedCostOrderKey::Zero)
        } else {
            Some(WeightedCostOrderKey::Uint128(value as u128))
        });
    }
    if value.is_unsigned_int() {
        let Some(value) = value.as_u128() else {
            return Ok(None);
        };
        return Ok(if value == 0 {
            Some(WeightedCostOrderKey::Zero)
        } else {
            Some(WeightedCostOrderKey::Uint128(value))
        });
    }
    let float = match value {
        Value::Float16(value) => Some(value.to_f64()),
        Value::Float32(value) => Some(f64::from(*value)),
        Value::Float64(value) => Some(*value),
        _ => None,
    };
    let Some(float) = float else {
        return Ok(None);
    };
    if !float.is_finite() {
        return Err(PlanQueryError::GleaphCost {
            message: "shortest-path edge cost must be finite".into(),
        });
    }
    if float < 0.0 {
        return Err(PlanQueryError::GleaphCost {
            message: "shortest-path edge cost must be non-negative".into(),
        });
    }
    Ok(if float == 0.0 {
        Some(WeightedCostOrderKey::Zero)
    } else {
        Some(WeightedCostOrderKey::Float64(float))
    })
}

fn compare_weighted_cost_order_key(left: &WeightedCost, right: &WeightedCost) -> Ordering {
    match (&left.order_key, &right.order_key) {
        (WeightedCostOrderKey::Zero, WeightedCostOrderKey::Zero) => Ordering::Equal,
        (WeightedCostOrderKey::Zero, WeightedCostOrderKey::Uint128(_))
        | (WeightedCostOrderKey::Zero, WeightedCostOrderKey::Float64(_)) => Ordering::Less,
        (WeightedCostOrderKey::Uint128(_), WeightedCostOrderKey::Zero)
        | (WeightedCostOrderKey::Float64(_), WeightedCostOrderKey::Zero) => Ordering::Greater,
        (WeightedCostOrderKey::Uint128(left), WeightedCostOrderKey::Uint128(right)) => {
            left.cmp(right)
        }
        (WeightedCostOrderKey::Float64(left), WeightedCostOrderKey::Float64(right)) => left
            .partial_cmp(right)
            .expect("validated weighted shortest-path float costs must be finite"),
        (WeightedCostOrderKey::Normalized(left), WeightedCostOrderKey::Normalized(right)) => {
            compare_weighted_numeric(left.as_ref(), right.as_ref())
        }
        _ => compare_values(&left.value, &right.value)
            .expect("validated weighted shortest-path costs must be mutually comparable"),
    }
}

fn compare_weighted_numeric(
    left: &Option<NormalizedNumeric>,
    right: &Option<NormalizedNumeric>,
) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(right)) => {
            if right.negative {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(left), None) => {
            if left.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(left), Some(right)) => left.cmp_numeric(right),
    }
}

type WeightedHopCostCache = IntMap<u64, IntMap<u64, WeightedCost>>;

#[inline]
fn weighted_hop_cache_outer_key(edge: EdgeBinding) -> u64 {
    u64::from(u32::from(edge.handle.owner_vertex_id)) << 32 | u64::from(edge.handle.slot_index)
}

fn weighted_hop_cache_value_key(edge: EdgeBinding) -> u64 {
    let mut hasher = RapidHasher::default();
    hasher.write_u8(edge.value_len());
    hasher.write(edge.value_bytes_slice());
    hasher.finish()
}

fn map_weighted_cost_add_err(err: NumericOpError) -> PlanQueryError {
    match err {
        NumericOpError::Overflow => PlanQueryError::GleaphCost {
            message: "shortest-path edge cost overflowed or became non-finite".into(),
        },
        NumericOpError::NonFinite => PlanQueryError::GleaphCost {
            message: "shortest-path edge cost must be finite".into(),
        },
        NumericOpError::UnsupportedConversion => PlanQueryError::GleaphCost {
            message: "shortest-path edge cost uses unsupported numeric conversion".into(),
        },
        NumericOpError::DivisionByZero => PlanQueryError::GleaphCost {
            message: "shortest-path edge cost evaluation failed: DivisionByZero".into(),
        },
        _ => PlanQueryError::GleaphCost {
            message: format!("shortest-path edge cost evaluation failed: {err:?}"),
        },
    }
}

struct WeightedQueueEntry {
    cost: WeightedCost,
    tie: u64,
    state_idx: usize,
}

impl Eq for WeightedQueueEntry {}

impl PartialEq for WeightedQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cost.cmp_infallible(&other.cost) == Ordering::Equal && self.tie == other.tie
    }
}

impl Ord for WeightedQueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.cost.cmp_infallible(&other.cost).reverse() {
            Ordering::Equal => other.tie.cmp(&self.tie),
            non_eq => non_eq,
        }
    }
}

impl PartialOrd for WeightedQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(crate) fn weighted_shortest_paths_between(
    store: &GraphStore,
    src: VertexId,
    dst: VertexId,
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    var_len: &Option<VarLenSpec>,
    edge_var: &str,
    cost_expr: &Expr,
    mode: ShortestMode,
    parameters: &BTreeMap<String, Value>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    store_hop_edges: bool,
) -> Result<ShortestPathSearchResult, PlanQueryError> {
    let bounds = var_len.unwrap_or(VarLenSpec {
        min: 1,
        max: Some(1),
    });
    let vertex_count = u64::from(u32::from(store.vertex_count()));
    let max_hops = bounds.max.unwrap_or_else(|| vertex_count.saturating_sub(1));

    let mut heap = BinaryHeap::new();
    let mut tie = 0u64;
    let mut states = vec![PathSearchNode {
        current: src,
        previous: None,
        edge: None,
        depth: 0,
    }];
    heap.push(WeightedQueueEntry {
        cost: WeightedCost::zero(),
        tie,
        state_idx: 0,
    });

    let mut found_min_cost: Option<WeightedCost> = None;
    let mut found = Vec::new();
    let mut hop_cost_cache: WeightedHopCostCache = IntMap::default();
    let direct_gleaph_weight_decoder =
        direct_gleaph_weight_hop_cost_decoder(cost_expr, edge_var, gleaph_weight_decoders)?;
    let use_hop_cost_cache = direct_gleaph_weight_decoder.is_none();
    let mut any_best_cost = if matches!(mode, ShortestMode::AnyShortest)
        && bounds.min <= 1
        && !matches!(cost_expr.kind, ExprKind::Literal(_))
    {
        let mut best = IntMap::default();
        best.insert(u32::from(src), WeightedCost::zero());
        Some(best)
    } else {
        None
    };
    let mut candidates = Vec::new();
    let fixed_label_expand = match label_id {
        Some(lid) => Some(ShortestFixedLabelExpand::new(direction, lid)?),
        None => None,
    };

    while let Some(entry) = heap.pop() {
        if let Some(ref min) = found_min_cost
            && matches!(entry.cost.cmp(min), Ordering::Greater)
        {
            break;
        }
        let state_idx = entry.state_idx;
        let current = states[state_idx].current;
        let depth = states[state_idx].depth;
        if depth >= bounds.min && current == dst {
            match &found_min_cost {
                None => {
                    found_min_cost = Some(entry.cost.clone());
                    found.push(state_idx);
                }
                Some(min) => match entry.cost.cmp(min) {
                    Ordering::Equal => found.push(state_idx),
                    Ordering::Less => {
                        found_min_cost = Some(entry.cost.clone());
                        found.clear();
                        found.push(state_idx);
                    }
                    Ordering::Greater => {}
                },
            }
            if matches!(mode, ShortestMode::AnyShortest) {
                break;
            }
            continue;
        }
        if depth >= max_hops {
            continue;
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _expand_scope = bench_scope("weighted_shortest_expand");
        candidates.clear();
        match fixed_label_expand {
            Some(prep) => prep.expand_into(store, current, &mut candidates)?,
            None => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _generic_scope = bench_scope("weighted_shortest_expand_generic");
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
            }
        }
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _relax_scope = bench_scope("weighted_shortest_relax");
        for (edge_dst, edge_binding) in candidates.iter().copied() {
            let ExpandDst::Local(next) = edge_dst else {
                continue;
            };
            if any_best_cost.is_none() && path_search_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let hop_cost = if use_hop_cost_cache {
                let outer = weighted_hop_cache_outer_key(edge_binding);
                let value_key = weighted_hop_cache_value_key(edge_binding);
                if let Some(cost) = hop_cost_cache.get(&outer).and_then(|m| m.get(&value_key)) {
                    cost.clone()
                } else {
                    let cost = eval_shortest_hop_cost(
                        store,
                        cost_expr,
                        edge_var,
                        edge_binding,
                        parameters,
                        gleaph_weight_decoders,
                    )?;
                    hop_cost_cache
                        .entry(outer)
                        .or_default()
                        .insert(value_key, cost.clone());
                    cost
                }
            } else {
                decode_direct_gleaph_weight_hop_cost(
                    store,
                    direct_gleaph_weight_decoder
                        .expect("direct GLEAPH.WEIGHT decoder must be present"),
                    edge_binding,
                )?
            };
            let next_cost = entry.cost.checked_add(&hop_cost)?;
            if let Some(ref min) = found_min_cost
                && matches!(next_cost.cmp(min), Ordering::Greater)
            {
                continue;
            }
            if let Some(best_cost) = any_best_cost.as_mut() {
                let next_vertex = u32::from(next);
                if best_cost
                    .get(&next_vertex)
                    .is_some_and(|best| !matches!(next_cost.cmp(best), Ordering::Less))
                {
                    continue;
                }
                best_cost.insert(next_vertex, next_cost.clone());
            }
            tie += 1;
            let next_state_idx = states.len();
            states.push(PathSearchNode {
                current: next,
                previous: Some(state_idx),
                edge: store_hop_edges.then_some(edge_binding),
                depth: depth + 1,
            });
            heap.push(WeightedQueueEntry {
                cost: next_cost,
                tie,
                state_idx: next_state_idx,
            });
        }
    }

    Ok(ShortestPathSearchResult { states, found })
}

fn path_search_contains_vertex(
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

fn eval_shortest_hop_cost(
    store: &GraphStore,
    expr: &Expr,
    edge_var: &str,
    edge_binding: EdgeBinding,
    parameters: &BTreeMap<String, Value>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<WeightedCost, PlanQueryError> {
    if let Some(cost) = eval_direct_gleaph_weight_hop_cost(
        store,
        expr,
        edge_var,
        edge_binding,
        gleaph_weight_decoders,
    )? {
        return Ok(cost);
    }
    let mut row = PlanRow::new();
    row.insert(edge_var.to_string(), PlanBinding::Edge(edge_binding));
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs: None,
        caller: None,
        gleaph_weight_decoders,
    };
    let value = evaluator.eval_expr(&row, expr)?;
    WeightedCost::from_value(value)
}

fn eval_direct_gleaph_weight_hop_cost(
    store: &GraphStore,
    expr: &Expr,
    edge_var: &str,
    edge_binding: EdgeBinding,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Option<WeightedCost>, PlanQueryError> {
    let Some(decoder) =
        direct_gleaph_weight_hop_cost_decoder(expr, edge_var, gleaph_weight_decoders)?
    else {
        return Ok(None);
    };
    decode_direct_gleaph_weight_hop_cost(store, decoder, edge_binding).map(Some)
}

fn direct_gleaph_weight_hop_cost_decoder<'a>(
    expr: &Expr,
    edge_var: &str,
    gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Option<&'a PreparedWeightDecoder>, PlanQueryError> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
    else {
        return Ok(None);
    };
    if !super::super::gleaph_weight::is_gleaph_weight_call(name, *distinct) {
        return Ok(None);
    }
    let Some(arg) = super::super::gleaph_weight::gleaph_weight_single_arg(args) else {
        return Ok(None);
    };
    let Some(arg_edge_var) = super::super::gleaph_weight::gleaph_weight_arg_edge_var(arg) else {
        return Ok(None);
    };
    if arg_edge_var != edge_var {
        return Ok(None);
    }
    gleaph_weight_decoders
        .and_then(|decoders| decoders.get(edge_var))
        .ok_or_else(|| PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH.WEIGHT({edge_var}): no prepared decoder for this edge variable"
            ),
        })
        .map(Some)
}

pub(crate) fn decode_direct_gleaph_weight_hop_cost(
    store: &GraphStore,
    decoder: &PreparedWeightDecoder,
    edge_binding: EdgeBinding,
) -> Result<WeightedCost, PlanQueryError> {
    let weight = super::super::gleaph_weight::decode_traversal_edge_weight(
        store,
        edge_binding.handle,
        edge_binding.value_len(),
        edge_binding.value_bytes_slice(),
        edge_binding.inline_value(),
        Some(decoder),
    )?;
    Ok(WeightedCost::from_validated_non_negative_float32(weight))
}

thread_local! {
    /// Reuses capacity when materializing many shortest-path rows on one thread (e.g. `AllShortest`).
    static PATH_MATERIALIZE_SCRATCH: RefCell<Vec<PathElement>> = const { RefCell::new(Vec::new()) };
}

/// Below this estimated element count (`depth * 2 + 1`), allocate a fresh `Vec` only: `thread_local`
/// lookup + `RefCell` borrow win nothing on tiny paths and showed up as instruction regressions in
/// `plan_query_materialize_value_rows` benches.
const PATH_MATERIALIZE_SCRATCH_MIN_ELEMENTS: usize = 16;

pub(crate) fn path_binding_to_value(store: &GraphStore, pb: &PathBinding) -> Value {
    materialize_path_from_search_states(store, pb.shard_id, pb.states.as_ref(), pb.leaf_state_idx)
}

pub(crate) fn materialize_path_from_search_states(
    store: &GraphStore,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    states: &[PathSearchNode],
    state_idx: usize,
) -> Value {
    let depth = states[state_idx].depth as usize;
    let min_cap = depth.saturating_mul(2).saturating_add(1);
    if min_cap < PATH_MATERIALIZE_SCRATCH_MIN_ELEMENTS {
        let mut elements = Vec::with_capacity(min_cap);
        fill_path_elements_leaf_to_root(store, shard_id, states, state_idx, &mut elements);
        return Value::Path(elements);
    }
    PATH_MATERIALIZE_SCRATCH.with(|scratch| {
        if let Ok(mut elements) = scratch.try_borrow_mut() {
            fill_path_elements_leaf_to_root(store, shard_id, states, state_idx, &mut elements);
            return Value::Path(std::mem::take(&mut *elements));
        }
        let mut elements = Vec::with_capacity(min_cap);
        fill_path_elements_leaf_to_root(store, shard_id, states, state_idx, &mut elements);
        Value::Path(elements)
    })
}

fn fill_path_elements_leaf_to_root(
    store: &GraphStore,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    states: &[PathSearchNode],
    state_idx: usize,
    elements: &mut Vec<PathElement>,
) {
    elements.clear();
    let mut chain: Vec<usize> = Vec::new();
    let mut idx = state_idx;
    loop {
        chain.push(idx);
        let Some(previous) = states[idx].previous else {
            break;
        };
        idx = previous;
    }
    let cap = chain.len() * 2;
    if elements.capacity() < cap {
        elements.reserve(cap.saturating_sub(elements.capacity()));
    }
    for (hop, &si) in chain.iter().rev().enumerate() {
        let state = &states[si];
        if hop > 0
            && let Some(edge_binding) = state.edge
        {
            elements.push(edge_path_element(shard_id, edge_binding.handle));
        }
        elements.push(vertex_path_element(store, state.current));
    }
}

pub(crate) fn local_shard_id(store: &GraphStore) -> gleaph_graph_kernel::federation::ShardId {
    store.federation_routing().map(|r| r.shard_id).unwrap_or(0)
}

pub(crate) fn vertex_element_id_bytes(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<Vec<u8>, PlanQueryError> {
    let path_id =
        store
            .path_vertex_element_id(vertex_id)
            .ok_or_else(|| PlanQueryError::MissingBinding {
                variable: format!("logical vertex id for {vertex_id:?}"),
            })?;
    Ok(path_id.to_bytes().to_vec())
}

pub(crate) fn edge_element_id_bytes(
    shard_id: gleaph_graph_kernel::federation::ShardId,
    owner_vertex_id: VertexId,
    edge_slot_index: gleaph_graph_kernel::entry::EdgeSlotIndex,
) -> Vec<u8> {
    GraphPathEdgeId::new(shard_id, owner_vertex_id, edge_slot_index)
        .to_bytes()
        .to_vec()
}

fn vertex_path_element(store: &GraphStore, vertex_id: VertexId) -> PathElement {
    let path_id = if store.federation_configured() {
        store.path_vertex_element_id(vertex_id).unwrap_or_else(|| {
            GraphPathVertexId::new(
                gleaph_graph_kernel::federation::standalone_logical_vertex_id(vertex_id),
            )
        })
    } else {
        GraphPathVertexId::new(
            gleaph_graph_kernel::federation::standalone_logical_vertex_id(vertex_id),
        )
    };
    PathElement::Vertex(path_id.to_bytes().into())
}

fn edge_path_element(
    shard_id: gleaph_graph_kernel::federation::ShardId,
    handle: EdgeHandle,
) -> PathElement {
    PathElement::Edge(
        GraphPathEdgeId::new(
            shard_id,
            handle.owner_vertex_id,
            EdgeSlotIndex::from_raw(handle.slot_index),
        )
        .to_bytes()
        .into(),
    )
}


#[cfg(test)]
mod tests;
