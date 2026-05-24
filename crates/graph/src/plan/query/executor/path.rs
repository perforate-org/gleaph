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
                store.for_each_directed_in_edges_for_label_unchecked(current, label, |edge| {
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
                })?;
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
mod path_test_helpers {
    use super::super::test_support::*;
    use gleaph_gql::types::PathElement;
    use gleaph_gql_planner::plan::PhysicalPlan;
    use gleaph_graph_kernel::entry::EdgeLabelId;
    use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};

    pub fn path_column<'a>(result: &'a PlanQueryResult, column: &str) -> &'a [PathElement] {
        match result.rows.first().and_then(|row| row.get(column)) {
            Some(Value::Path(elements)) => elements,
            other => panic!("expected path column {column}, got {other:?}"),
        }
    }

    pub fn vertex_path_id(element: &PathElement) -> GraphPathVertexId {
        match element {
            PathElement::Vertex(id) => {
                GraphPathVertexId::try_from_slice(id.as_ref()).expect("vertex path id")
            }
            other => panic!("expected vertex path element, got {other:?}"),
        }
    }

    pub fn assert_path_vertex_local(store: &GraphStore, element: &PathElement, local: VertexId) {
        assert_eq!(
            vertex_path_id(element).logical_vertex_id,
            store
                .logical_vertex_id(local)
                .expect("logical vertex id for local vertex")
        );
    }

    pub fn edge_path_id(element: &PathElement) -> GraphPathEdgeId {
        match element {
            PathElement::Edge(id) => GraphPathEdgeId::try_from_slice(id.as_ref()).expect("edge id"),
            other => panic!("expected edge path element, got {other:?}"),
        }
    }

    pub fn catalog_edge_label(store: &GraphStore, label_name: &str) -> EdgeLabelId {
        store.edge_label_id(label_name).expect("edge label")
    }

    pub fn gleaph_weight_call(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::var(edge_var)],
            distinct: false,
        })
    }

    pub fn scaled_gleaph_weight_cost(edge_var: &str, scale_param: &str) -> Expr {
        Expr::new(ExprKind::BinaryOp {
            left: Box::new(gleaph_weight_call(edge_var)),
            op: BinaryOp::Mul,
            right: Box::new(Expr::new(ExprKind::Parameter(scale_param.to_owned()))),
        })
    }

    pub fn setup_weighted_road_graph(store: &GraphStore) -> (VertexId, VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let a = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["WgtB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        let c = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert c");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(a, b, Some(road), 1)
            .expect("a->b");
        store
            .insert_directed_edge_with_inline_value(b, c, Some(road), 1)
            .expect("b->c");
        store
            .insert_directed_edge_with_inline_value(a, c, Some(road), 100)
            .expect("a->c");
        (a, b, c)
    }

    pub fn weighted_shortest_plan_with_cost(cost: Expr) -> PhysicalPlan {
        weighted_shortest_plan_with_cost_mode(cost, ShortestMode::AnyShortest)
    }

    pub fn weighted_shortest_plan_with_cost_mode(cost: Expr, mode: ShortestMode) -> PhysicalPlan {
        plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost,
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ])
    }

    pub fn weighted_2_24_precision_cost_expr() -> Expr {
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: Expr::new(ExprKind::Literal(Value::Float64(8_388_608.0))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: Expr::new(ExprKind::Literal(Value::Float64(16_777_217.0))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Float64(0.0))))),
        })
    }

    pub fn cast_expr_to_float32(expr: Expr) -> Expr {
        Expr::new(ExprKind::Cast {
            expr: Box::new(expr),
            target: gleaph_gql::ast::ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        })
    }

    pub fn weighted_2_24_precision_cost_expr_float32() -> Expr {
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: cast_expr_to_float32(Expr::new(ExprKind::Literal(Value::Float64(
                        8_388_608.0,
                    )))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: cast_expr_to_float32(Expr::new(ExprKind::Literal(Value::Float64(
                        16_777_217.0,
                    )))),
                },
            ],
            else_clause: Some(Box::new(cast_expr_to_float32(Expr::new(
                ExprKind::Literal(Value::Float64(0.0)),
            )))),
        })
    }

    pub fn weighted_decimal_precision_cost_expr() -> Expr {
        use gleaph_gql::types::Decimal;
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: Expr::new(ExprKind::Literal(Value::Decimal(
                        Decimal::parse("0.10").expect("decimal"),
                    ))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: Expr::new(ExprKind::Literal(Value::Decimal(
                        Decimal::parse("0.21").expect("decimal"),
                    ))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                Decimal::from_i64(0),
            ))))),
        })
    }

    pub fn weighted_wide_integer_precision_cost_expr() -> Expr {
        Expr::new(ExprKind::CaseSimple {
            operand: Box::new(gleaph_weight_call("e")),
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(1.0))),
                    result: Expr::new(ExprKind::Literal(Value::Int64(1_000_000))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Float32(100.0))),
                    result: Expr::new(ExprKind::Literal(Value::Int64(2_000_001))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int64(0))))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::path_test_helpers::*;
    use super::{
        ShortestFixedLabelExpand, WeightedCost, WeightedCostOrderKey,
        decode_direct_gleaph_weight_hop_cost, local_shard_id, materialize_path_from_search_states,
        weighted_shortest_can_use_hop_count, weighted_shortest_paths_between,
    };
    use ic_stable_lara::traits::CsrEdge;
    use pollster;
    #[test]
    fn shortest_path_optional_hit_with_dst_label_narrowing() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["OptSpHitA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["OptSpHitB", "OptSpHitC"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .insert_directed_edge_named(a, b, Some("OptSpHitRel"), Vec::<(&str, Value)>::new())
            .expect("a->b");
        let gql = "MATCH (a:OptSpHitA) OPTIONAL MATCH (a)-[e:OptSpHitRel]->(b:OptSpHitB) \
                   MATCH ANY SHORTEST (a)-[e2:OptSpHitRel]->(b:OptSpHitC) RETURN a, b";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("shortest path after optional hit with label narrowing");
        assert_eq!(
            result.rows.len(),
            1,
            "optional hit with stricter shortest-path dst label must return one row: {:?}",
            result.rows
        );
    }
    #[test]
    fn shortest_path_after_optional_miss_drops_null_destination_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptSpA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        store
            .insert_vertex_named(["OptSpB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .get_or_insert_edge_label_id("OptSpRel")
            .expect("edge label");
        let gql = "MATCH (a:OptSpA) OPTIONAL MATCH (a)-[e:OptSpRel]->(b:OptSpB) \
                   MATCH ANY SHORTEST (a)-[e2:OptSpRel]->(b) RETURN a, b";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("shortest path after optional miss should not error");
        assert!(
            result.rows.is_empty(),
            "optional miss leaves b null; shortest path should drop the row: {:?}",
            result.rows
        );
    }
    #[test]
    fn hop_cost_admits_large_finite_float64() {
        let cost = WeightedCost::from_value(Value::Float64(1e40)).expect("large finite hop cost");
        assert!(matches!(cost.value, Value::Float64(v) if v == 1e40));
    }
    #[test]
    fn hop_cost_rejects_null() {
        let err = WeightedCost::from_value(Value::Null).expect_err("null hop cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must not be NULL"
        ));
    }
    #[test]
    fn hop_cost_rejects_nan() {
        let err = WeightedCost::from_value(Value::Float64(f64::NAN)).expect_err("nan hop cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must be finite"
        ));
    }
    #[test]
    fn hop_cost_rejects_negative() {
        let err = WeightedCost::from_value(Value::Int32(-1)).expect_err("negative hop cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must be non-negative"
        ));
    }
    #[test]
    fn weighted_literal_cost_uses_hop_count_when_equivalent() {
        let positive = Expr::new(ExprKind::Literal(Value::Int32(1)));
        let zero = Expr::new(ExprKind::Literal(Value::Int32(0)));
        let negative = Expr::new(ExprKind::Literal(Value::Int32(-1)));

        assert!(weighted_shortest_can_use_hop_count(
            ShortestMode::AnyShortest,
            &zero
        ));
        assert!(weighted_shortest_can_use_hop_count(
            ShortestMode::AllShortest,
            &positive
        ));
        assert!(!weighted_shortest_can_use_hop_count(
            ShortestMode::AllShortest,
            &zero
        ));
        assert!(!weighted_shortest_can_use_hop_count(
            ShortestMode::AnyShortest,
            &negative
        ));
    }
    #[test]
    fn weighted_cost_add_overflow_errors() {
        let left = WeightedCost::from_value(Value::Float64(f64::MAX)).expect("left");
        let right = WeightedCost::from_value(Value::Float64(f64::MAX)).expect("right");
        let err = left.checked_add(&right).expect_err("overflow add");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost overflowed or became non-finite"
                || msg == "shortest-path edge cost must be finite"
        ));
    }
    #[test]
    fn element_id_returns_graph_kernel_bytes_for_vertices_and_edges() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let a = store
            .insert_vertex_named(["ElementIdSource"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b = store
            .insert_vertex_named(["ElementIdTarget"], [("name", Value::Text("b".into()))])
            .expect("insert b");
        let edge = store
            .insert_directed_edge_named(a, b, Some("ElementIdRel"), Vec::<(&str, Value)>::new())
            .expect("insert edge");
        let plan = plan_gql(
            "MATCH (a:ElementIdSource)-[e:ElementIdRel]->(b:ElementIdTarget) \
             RETURN ELEMENT_ID(a) AS aid, ELEMENT_ID(e) AS eid",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("element ids");

        assert_eq!(result.rows.len(), 1);
        let vertex_id = GraphPathVertexId::try_from_slice(bytes_column(&result, "aid"))
            .expect("vertex element id");
        assert_eq!(
            vertex_id.logical_vertex_id,
            store.logical_vertex_id(a).expect("logical id for a")
        );
        let edge_id =
            GraphPathEdgeId::try_from_slice(bytes_column(&result, "eid")).expect("edge element id");
        assert_eq!(edge_id.shard_id, 7);
        assert_eq!(edge_id.owner_vertex_id, edge.owner_vertex_id);
        assert_eq!(
            edge_id.edge_slot_index,
            EdgeSlotIndex::from_raw(edge.slot_index)
        );
    }
    #[test]
    fn element_id_of_null_optional_binding_returns_null() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["ElementIdOptional"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let plan = plan_gql(
            "MATCH (n:ElementIdOptional) \
             OPTIONAL MATCH (n)-[e:ElementIdMissing]->(m:ElementIdMissingTarget) \
             RETURN ELEMENT_ID(e) AS eid",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("optional element id");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("eid"), Some(&Value::Null));
    }
    #[test]
    fn shortest_path_binds_opaque_path_ids() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let a = store
            .insert_vertex_named(["ShortestPathSource"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b = store
            .insert_vertex_named(["ShortestPathMid"], [("name", Value::Text("b".into()))])
            .expect("insert b");
        let c = store
            .insert_vertex_named(["ShortestPathTarget"], [("name", Value::Text("c".into()))])
            .expect("insert c");
        let ab = store
            .insert_directed_edge_named(a, b, Some("ShortestPathRel"), Vec::<(&str, Value)>::new())
            .expect("insert ab");
        let bc = store
            .insert_directed_edge_named(b, c, Some("ShortestPathRel"), Vec::<(&str, Value)>::new())
            .expect("insert bc");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("ShortestPathSource".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("ShortestPathTarget".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("ShortestPathRel".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute shortest path");

        assert_eq!(result.rows.len(), 1);
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 5);
        assert_path_vertex_local(&store, &elements[0], a);
        assert_eq!(
            edge_path_id(&elements[1]).owner_vertex_id,
            ab.owner_vertex_id
        );
        assert_eq!(
            edge_path_id(&elements[1]).edge_slot_index,
            EdgeSlotIndex::from_raw(ab.slot_index)
        );
        assert_path_vertex_local(&store, &elements[2], b);
        assert_eq!(
            edge_path_id(&elements[3]).owner_vertex_id,
            bc.owner_vertex_id
        );
        assert_eq!(
            edge_path_id(&elements[3]).edge_slot_index,
            EdgeSlotIndex::from_raw(bc.slot_index)
        );
        assert_path_vertex_local(&store, &elements[4], c);
    }
    #[test]
    fn shortest_path_zero_hop_binds_null_edge_and_single_vertex_path() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["ShortestPathZero"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("ShortestPathZero".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "a".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 0,
                    max: Some(3),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p"), project(var("e"), "e")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute zero-hop shortest path");

        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 1);
        assert_path_vertex_local(&store, &elements[0], a);
        assert_eq!(result.rows[0].get("e"), Some(&Value::Null));
    }
    #[test]
    fn all_shortest_path_returns_all_equal_depth_paths() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["AllShortestSource"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b1 = store
            .insert_vertex_named(["AllShortestMid"], [("name", Value::Text("b1".into()))])
            .expect("insert b1");
        let b2 = store
            .insert_vertex_named(["AllShortestMid"], [("name", Value::Text("b2".into()))])
            .expect("insert b2");
        let c = store
            .insert_vertex_named(["AllShortestTarget"], [("name", Value::Text("c".into()))])
            .expect("insert c");
        store
            .insert_directed_edge_named(a, b1, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b1");
        store
            .insert_directed_edge_named(b1, c, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert b1-c");
        store
            .insert_directed_edge_named(a, b2, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b2");
        store
            .insert_directed_edge_named(b2, c, Some("AllShortestRel"), Vec::<(&str, Value)>::new())
            .expect("insert b2-c");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("AllShortestSource".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("AllShortestTarget".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AllShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("AllShortestRel".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute all shortest paths");

        assert_eq!(result.rows.len(), 2);
        let middle_vertices: BTreeSet<gleaph_graph_kernel::federation::LogicalVertexId> = result
            .rows
            .iter()
            .map(|row| match row.get("p") {
                Some(Value::Path(elements)) => vertex_path_id(&elements[2]).logical_vertex_id,
                other => panic!("expected path, got {other:?}"),
            })
            .collect();
        assert_eq!(
            middle_vertices,
            BTreeSet::from([
                store.logical_vertex_id(b1).expect("b1 logical id"),
                store.logical_vertex_id(b2).expect("b2 logical id"),
            ])
        );
    }
    #[test]
    fn shortest_path_rejects_unsupported_mode_and_label_expr() {
        let store = GraphStore::new();
        let k_err = store
            .execute_plan_query(
                &plan(vec![PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    emit_edge_binding: true,
                    emit_path_binding: true,
                    mode: ShortestMode::ShortestK(2),
                    direction: EdgeDirection::PointingRight,
                    label: None,
                    label_expr: None,
                    var_len: None,
                    cost: ShortestPathCost::HopCount,
                }]),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect_err("ShortestK should be unsupported");
        assert!(matches!(
            k_err,
            PlanQueryError::UnsupportedOp("ShortestPath.ShortestK")
        ));

        let label_expr_err = store
            .execute_plan_query(
                &plan(vec![PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: Some("p".into()),
                    emit_edge_binding: true,
                    emit_path_binding: true,
                    mode: ShortestMode::AnyShortest,
                    direction: EdgeDirection::PointingRight,
                    label: None,
                    label_expr: Some(LabelExpr::Name("Rel".into())),
                    var_len: None,
                    cost: ShortestPathCost::HopCount,
                }]),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect_err("label_expr should be unsupported");
        assert!(matches!(
            label_expr_err,
            PlanQueryError::UnsupportedOp("ShortestPath.label_expr")
        ));
    }
    #[test]
    fn weighted_shortest_path_cost_expr_uses_query_parameters() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: scaled_gleaph_weight_cost("e", "scale"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let mut parameters = params();
        parameters.insert("scale".into(), Value::Float32(1.0));
        let result = store
            .execute_plan_query(&plan, &parameters, GqlExecutionContext::default())
            .expect("parameterized weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(
            elements.len(),
            5,
            "GLEAPH.WEIGHT(e) * $scale with scale=1 should match unscaled weighted shortest path"
        );
        assert_path_vertex_local(&store, &elements[4], c);
    }
    #[test]
    fn weighted_shortest_any_prefers_exact_float64_cost_at_2_24() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(weighted_2_24_precision_cost_expr()),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float64 precision weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_all_shortest_does_not_epsilon_tie_distinct_costs() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost_mode(
                    weighted_2_24_precision_cost_expr(),
                    ShortestMode::AllShortest,
                ),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("all-shortest with distinct float64 costs");
        assert_eq!(
            result.rows.len(),
            1,
            "distinct float64 costs must not be epsilon-tied"
        );
        assert_eq!(path_column(&result, "p").len(), 5);
    }
    #[test]
    fn weighted_shortest_all_returns_all_equal_cost_paths() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["WgtAllSrc"], [("name", Value::Text("a".into()))])
            .expect("insert a");
        let b1 = store
            .insert_vertex_named(["WgtAllMid"], [("name", Value::Text("b1".into()))])
            .expect("insert b1");
        let b2 = store
            .insert_vertex_named(["WgtAllMid"], [("name", Value::Text("b2".into()))])
            .expect("insert b2");
        let c = store
            .insert_vertex_named(["WgtAllDst"], [("name", Value::Text("c".into()))])
            .expect("insert c");
        store
            .insert_directed_edge_named(a, b1, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b1");
        store
            .insert_directed_edge_named(b1, c, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert b1-c");
        store
            .insert_directed_edge_named(a, b2, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert a-b2");
        store
            .insert_directed_edge_named(b2, c, Some("WgtAllRel"), Vec::<(&str, Value)>::new())
            .expect("insert b2-c");
        let zero_cost = Expr::new(ExprKind::Literal(Value::Int32(0)));
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtAllSrc".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtAllDst".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AllShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtAllRel".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(3),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: zero_cost,
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("weighted all-shortest with equal zero costs");

        assert_eq!(result.rows.len(), 2);
        let middle_vertices: BTreeSet<gleaph_graph_kernel::federation::LogicalVertexId> = result
            .rows
            .iter()
            .map(|row| match row.get("p") {
                Some(Value::Path(elements)) => vertex_path_id(&elements[2]).logical_vertex_id,
                other => panic!("expected path, got {other:?}"),
            })
            .collect();
        assert_eq!(
            middle_vertices,
            BTreeSet::from([
                store.logical_vertex_id(b1).expect("b1 logical id"),
                store.logical_vertex_id(b2).expect("b2 logical id"),
            ])
        );
    }
    #[test]
    fn weighted_shortest_cast_float32_restores_f32_precision_limits() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost_mode(
                    weighted_2_24_precision_cost_expr_float32(),
                    ShortestMode::AllShortest,
                ),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float32-cast weighted shortest path");
        assert_eq!(
            result.rows.len(),
            2,
            "float32-cast costs should tie at 2^24 precision"
        );
    }
    #[test]
    fn weighted_shortest_decimal_cost_accumulates_exactly() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(weighted_decimal_precision_cost_expr()),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("decimal precision weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_wide_integer_cost_accumulates() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(weighted_wide_integer_precision_cost_expr()),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("wide-integer precision weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_floor_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Floor(Box::new(gleaph_weight_call("e"))));
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("floor-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("cast-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_float128_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Float128,
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float128-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_float256_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Float256,
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float256-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_int_precision_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 10,
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("int-precision-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_float_precision_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::FloatPrecision {
                precision: 24,
                scale: None,
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("float-precision-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_int8_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Int8 {
                keyword: gleaph_gql::ast::Keyword::new("INT8"),
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("int8-cast-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_decimal_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: None,
                scale: None,
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("decimal-cast-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_decimal_precision_cast_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Cast {
            expr: Box::new(gleaph_weight_call("e")),
            target: gleaph_gql::ast::ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(10),
                scale: Some(2),
            },
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("decimal-precision-cast weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_coalesce_wrapped_cost_runs() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Coalesce(vec![
            gleaph_weight_call("e"),
            Expr::new(ExprKind::Literal(Value::Float32(1.0))),
        ]));
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("coalesce-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_case_wrapped_cost_runs() {
        use gleaph_gql::ast::WhenClause;
        use gleaph_gql::token::Span;
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::var("e")),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Null)),
                result: gleaph_weight_call("e"),
            }],
            else_clause: Some(Box::new(gleaph_weight_call("e"))),
        });
        let result = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect("case-wrapped weighted shortest path");
        assert_eq!(path_column(&result, "p").len(), 5);
        assert_path_vertex_local(&store, &path_column(&result, "p")[4], c);
    }
    #[test]
    fn weighted_shortest_path_prefers_lower_total_cost_over_fewer_hops() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 5, "expected 2-hop weighted shortest path");
        assert_path_vertex_local(&store, &elements[4], c);
    }

    /// Graph where a cheaper arrival at `x` exhausts the hop bound while a higher-cost arrival
    /// can still reach `dst` (s->x cost 2 depth 1, s->a->x cost 1 depth 2, x->dst cost 1, max=2).
    fn setup_hop_bound_cheaper_vertex_unusable_graph(store: &GraphStore) -> (VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let s = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert s");
        let a = store
            .insert_vertex_named(["WgtB"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let x = store
            .insert_vertex_named(["WgtHub"], Vec::<(&str, Value)>::new())
            .expect("insert x");
        let dst = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert dst");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(s, x, Some(road), 2)
            .expect("s->x");
        store
            .insert_directed_edge_with_inline_value(s, a, Some(road), 0)
            .expect("s->a");
        store
            .insert_directed_edge_with_inline_value(a, x, Some(road), 1)
            .expect("a->x");
        store
            .insert_directed_edge_with_inline_value(x, dst, Some(road), 1)
            .expect("x->dst");
        (s, dst)
    }
    #[test]
    fn weighted_shortest_higher_cost_vertex_state_can_still_reach_dst_under_hop_bound() {
        let store = GraphStore::new();
        let (s, dst) = setup_hop_bound_cheaper_vertex_unusable_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(2),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("hop-bound weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 5, "expected s->x->dst (2 edges)");
        assert_path_vertex_local(&store, &elements[0], s);
        assert_path_vertex_local(&store, &elements[4], dst);
    }

    /// Graph where a longer prefix reaches `mid` with lower total cost after a stale higher-cost
    /// entry is already in the heap; min-queue ordering and `found_min_cost` skip the stale pop.
    fn setup_stale_mid_diamond_graph(store: &GraphStore) -> (VertexId, VertexId) {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let s = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert s");
        let a = store
            .insert_vertex_named(["WgtB"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let mid = store
            .insert_vertex_named(["WgtHub"], Vec::<(&str, Value)>::new())
            .expect("insert mid");
        let dst = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert dst");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(s, mid, Some(road), 10)
            .expect("s->mid");
        store
            .insert_directed_edge_with_inline_value(s, a, Some(road), 5)
            .expect("s->a");
        store
            .insert_directed_edge_with_inline_value(a, mid, Some(road), 1)
            .expect("a->mid");
        store
            .insert_directed_edge_with_inline_value(mid, dst, Some(road), 0)
            .expect("mid->dst");
        (s, dst)
    }
    #[test]
    fn stale_mid_diamond_edge_bindings_carry_expected_weights() {
        use gleaph_gql_planner::plan::{PlanOp, ShortestMode, VarLenSpec};
        let store = GraphStore::new();
        let (s, _dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let cost_expr = gleaph_weight_call("e");
        let decoders = crate::plan::query::gleaph_weight::prepare_gleaph_weight_decoders(
            &store,
            &[PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: None,
                emit_edge_binding: true,
                emit_path_binding: false,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost_expr.clone(),
                },
            }],
        )
        .expect("decoders")
        .expect("table");
        let decoder = decoders.get("e").expect("edge decoder");
        let mut weights = BTreeMap::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(s, road, |edge| {
                let binding =
                    edge_binding_for_expand(&store, s, EdgeDirection::PointingRight, edge)
                        .expect("binding");
                let w = crate::plan::query::gleaph_weight::decode_traversal_edge_weight(
                    &store,
                    binding.handle,
                    binding.value_len(),
                    binding.value_bytes_slice(),
                    binding.inline_value(),
                    Some(decoder),
                )
                .expect("decode");
                weights.insert(edge.neighbor_vid(), w);
            })
            .expect("for_each");
        let mut sorted: Vec<_> = weights.into_values().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(sorted, vec![5.0, 10.0]);
    }
    #[test]
    fn stale_mid_diamond_shortest_expand_hop_costs_are_5_10_and_1() {
        use gleaph_gql_planner::plan::{PlanOp, ShortestMode, VarLenSpec};
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let cost_expr = gleaph_weight_call("e");
        let decoders = crate::plan::query::gleaph_weight::prepare_gleaph_weight_decoders(
            &store,
            &[PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: None,
                emit_edge_binding: true,
                emit_path_binding: false,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost_expr.clone(),
                },
            }],
        )
        .expect("decoders")
        .expect("table");
        let decoder = decoders.get("e").expect("edge decoder");
        let prep = ShortestFixedLabelExpand::new(EdgeDirection::PointingRight, road).expect("prep");
        let mut from_s = Vec::new();
        prep.expand_into(&store, s, &mut from_s).expect("from s");
        let mut hop_costs = Vec::new();
        for (edge_dst, binding) in from_s {
            let hop = decode_direct_gleaph_weight_hop_cost(&store, decoder, binding).expect("hop");
            hop_costs.push((
                u32::from(match edge_dst {
                    ExpandDst::Local(v) => v,
                    ExpandDst::Remote(_) => panic!("remote"),
                }),
                hop.order_key,
            ));
        }
        hop_costs.sort_by_key(|(vid, _)| *vid);
        assert_eq!(hop_costs.len(), 2);
        assert!(
            matches!(hop_costs[0].1, WeightedCostOrderKey::Float64(v) if (v - 5.0).abs() < f64::EPSILON)
        );
        assert!(
            matches!(hop_costs[1].1, WeightedCostOrderKey::Float64(v) if (v - 10.0).abs() < f64::EPSILON)
        );

        let detour = hop_costs[0].0;
        store
            .for_each_directed_out_edges_for_label_unchecked(VertexId::from(detour), road, |edge| {
                assert_eq!(edge.inline_value_u16(), 1, "raw CSR edge value");
                assert_eq!(edge.value_bytes(), &[1, 0]);
                let handle = EdgeHandle {
                    owner_vertex_id: VertexId::from(detour),
                    label_id: ic_stable_lara::BucketLabelKey::from_raw(edge.label_id),
                    slot_index: edge.edge_slot_index.raw(),
                };
                let record = store
                    .find_outgoing_edge_record(handle)
                    .expect("lookup")
                    .expect("record");
                assert_eq!(
                    record.value_bytes(),
                    edge.value_bytes(),
                    "find_outgoing_edge_record must match iterated edge bytes"
                );
            })
            .expect("out from detour");
        let mut from_detour = Vec::new();
        prep.expand_into(&store, VertexId::from(detour), &mut from_detour)
            .expect("from detour");
        assert_eq!(from_detour.len(), 1);
        let binding = from_detour[0].1;
        assert_eq!(
            binding.inline_value(),
            1,
            "binding inline_value for detour->mid"
        );
        assert_eq!(
            binding.value_bytes_slice(),
            &[1, 0],
            "binding value_bytes for detour->mid"
        );
        let hop =
            decode_direct_gleaph_weight_hop_cost(&store, decoder, binding).expect("detour hop");
        assert!(
            matches!(hop.order_key, WeightedCostOrderKey::Float64(v) if (v - 1.0).abs() < f64::EPSILON),
            "detour->mid hop cost, got {:?}",
            hop.order_key
        );
        let _ = dst;
    }
    #[test]
    fn stale_mid_diamond_weighted_search_finds_cheaper_three_hop_path() {
        use gleaph_gql_planner::plan::{PlanOp, ShortestMode, VarLenSpec};
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let cost_expr = gleaph_weight_call("e");
        let decoders = crate::plan::query::gleaph_weight::prepare_gleaph_weight_decoders(
            &store,
            &[PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: gleaph_gql_planner::plan::ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: cost_expr.clone(),
                },
            }],
        )
        .expect("decoders")
        .expect("decoder table");
        let search = weighted_shortest_paths_between(
            &store,
            s,
            dst,
            EdgeDirection::PointingRight,
            Some(road),
            &Some(VarLenSpec {
                min: 1,
                max: Some(5),
            }),
            "e",
            &cost_expr,
            ShortestMode::AnyShortest,
            &BTreeMap::new(),
            Some(&decoders),
            true,
        )
        .expect("search");
        let path = materialize_path_from_search_states(
            &store,
            local_shard_id(&store),
            &search.states,
            *search.found.first().expect("path"),
        );
        let elements = match path {
            Value::Path(elements) => elements,
            other => panic!("unexpected path value: {other:?}"),
        };
        assert_eq!(
            elements.len(),
            7,
            "expected s->detour->mid->dst; got {elements:?}"
        );
    }
    #[test]
    fn weighted_shortest_skips_stale_higher_cost_vertex_entries() {
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
        let road = catalog_edge_label(&store, "WgtRoad");
        let mut weights = Vec::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(s, road, |edge| {
                weights.push(edge.inline_value_u16());
            })
            .expect("out edges from s");
        weights.sort_unstable();
        assert_eq!(
            weights,
            vec![5, 10],
            "edge weights from s must be persisted for weighted shortest path"
        );
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("stale-entry weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 7, "expected s->a->mid->dst (3 edges)");
        assert_path_vertex_local(&store, &elements[6], dst);
        assert_path_vertex_local(&store, &elements[0], s);
    }
    #[test]
    fn weighted_shortest_prefers_zero_weight_detour_over_direct_edge() {
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["WgtA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let c = store
            .insert_vertex_named(["WgtC"], Vec::<(&str, Value)>::new())
            .expect("insert c");
        let d1 = store
            .insert_vertex_named(["WgtD1"], Vec::<(&str, Value)>::new())
            .expect("insert d1");
        let d2 = store
            .insert_vertex_named(["WgtD2"], Vec::<(&str, Value)>::new())
            .expect("insert d2");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .install_edge_label_weight_profile_at_init(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let road = catalog_edge_label(&store, "WgtRoad");
        store
            .insert_directed_edge_with_inline_value(a, d1, Some(road), 0)
            .expect("a->d1");
        store
            .insert_directed_edge_with_inline_value(a, d2, Some(road), 0)
            .expect("a->d2");
        store
            .insert_directed_edge_with_inline_value(d1, d2, Some(road), 0)
            .expect("d1->d2");
        store
            .insert_directed_edge_with_inline_value(d1, c, Some(road), 0)
            .expect("d1->c");
        store
            .insert_directed_edge_with_inline_value(d2, c, Some(road), 0)
            .expect("d2->c");
        store
            .insert_directed_edge_with_inline_value(a, c, Some(road), 50)
            .expect("a->c direct");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("zero-weight detour weighted shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(
            elements.len(),
            5,
            "expected 2-hop zero-cost detour a->d1->c, not 1-hop direct edge"
        );
        assert_path_vertex_local(&store, &elements[elements.len() - 1], c);
    }
    #[test]
    fn hop_count_shortest_path_ignores_edge_weights() {
        let store = GraphStore::new();
        let (_a, _b, c) = setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![project(var("p"), "p")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("hop-count shortest path");
        let elements = path_column(&result, "p");
        assert_eq!(elements.len(), 3, "expected 1-hop unweighted shortest path");
        assert_path_vertex_local(&store, &elements[2], c);
    }
    #[test]
    fn gleaph_weight_in_return_does_not_change_shortest_path_search() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: Some("p".into()),
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtRoad".into()),
                label_expr: None,
                var_len: Some(VarLenSpec {
                    min: 1,
                    max: Some(5),
                }),
                cost: ShortestPathCost::HopCount,
            },
            PlanOp::Project {
                columns: vec![
                    project(var("p"), "p"),
                    project(gleaph_weight_call("e"), "w"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("shortest path with gleaph_weight in return");
        let elements = path_column(&result, "p");
        assert_eq!(
            elements.len(),
            3,
            "RETURN GLEAPH.WEIGHT must not affect hop-count search"
        );
        assert!(matches!(result.rows[0].get("w"), Some(Value::Float32(_))));
    }
    #[test]
    fn weighted_shortest_path_literal_overflow_cost_errors() {
        let store = GraphStore::new();
        setup_weighted_road_graph(&store);
        let cost = Expr::new(ExprKind::Literal(Value::Float64(f64::NAN)));
        let err = store
            .execute_plan_query(
                &weighted_shortest_plan_with_cost(cost),
                &params(),
                GqlExecutionContext::default(),
            )
            .expect_err("non-finite literal cost");
        assert!(matches!(
            err,
            PlanQueryError::GleaphCost {
                message: msg
            } if msg == "shortest-path edge cost must be finite"
        ));
    }
    #[test]
    fn weighted_shortest_path_rejects_missing_weight_profile() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["WgtNoProfileA"], Vec::<(&str, Value)>::new())
            .expect("a");
        let c = store
            .insert_vertex_named(["WgtNoProfileC"], Vec::<(&str, Value)>::new())
            .expect("c");
        store
            .get_or_insert_edge_label_id("WgtNoProfileRoad")
            .expect("road label");
        let road = catalog_edge_label(&store, "WgtNoProfileRoad");
        store.insert_directed_edge(a, c, Some(road)).expect("edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("WgtNoProfileA".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: "c".into(),
                label: Some("WgtNoProfileC".into()),
                property_projection: None,
            },
            PlanOp::ShortestPath {
                src: "a".into(),
                dst: "c".into(),
                edge: "e".into(),
                path_var: None,
                emit_edge_binding: true,
                emit_path_binding: true,
                mode: ShortestMode::AnyShortest,
                direction: EdgeDirection::PointingRight,
                label: Some("WgtNoProfileRoad".into()),
                label_expr: None,
                var_len: None,
                cost: ShortestPathCost::EdgeCostExpr {
                    edge_var: "e".into(),
                    expr: gleaph_weight_call("e"),
                },
            },
        ]);
        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("missing profile");
        assert!(matches!(err, PlanQueryError::GleaphWeight { .. }));
    }
}
