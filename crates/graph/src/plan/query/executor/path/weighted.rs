use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::hash::Hasher;

use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, Expr, ExprKind};
use gleaph_gql::numeric_ops::{NumericOpError, eval_binary_numeric};
use gleaph_gql::numeric_order::{NormalizedNumeric, NumericOrderError, normalized_numeric_parts};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql::value::cmp::compare_values;
use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId};
use gleaph_graph_kernel::federation::ElementIdEncodingKey;
use gleaph_graph_kernel::plan_exec::ResolvedInlineSchema;
use ic_stable_lara::VertexId;
use nohash_hasher::IntMap;
use rapidhash::fast::RapidHasher;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

use super::search::path_search_contains_vertex;
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::labeled::LabeledEdgeInlinePropertyBatchScratch;

use super::{
    PathSearchNode, ShortestExpandOptions, ShortestFixedLabelExpand, ShortestPathSearchResult,
};
use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::EdgeBinding;
use crate::plan::query::executor::context::QueryExprEvaluator;
use crate::plan::query::executor::eval::try_read_inline_edge_property;
use crate::plan::query::executor::expand::{
    ExpandDst, edge_binding_matches_label_expr, expand_candidates_into,
};
use crate::plan::query::executor::{EdgeSequenceOrder, PlanBinding};
use crate::plan::query::row::PlanRow;

/// Pre-validates `COST BY e.property` for a concrete edge label.
///
/// Resolves the property name once against the Router-projected tables, proves it equals the
/// scalar inline property id in the concrete label's `inline_schema`, and returns the resolved
/// `PropertyId` for per-hop reads. Struct inline schemas are excluded from `COST BY` in Slice 25.
/// Returns `Ok(None)` when the expression is not a direct inline-property access (e.g.
/// a non-inline-property expression), letting the caller fall back to the generic evaluator.
#[derive(Clone, Debug)]
struct PreparedInlineCost {
    property_id: PropertyId,
    field_path: Vec<String>,
}

fn property_access_path(expr: &Expr, edge_var: &str) -> Option<Vec<String>> {
    match &expr.kind {
        ExprKind::Variable(name) if name == edge_var => Some(Vec::new()),
        ExprKind::PropertyAccess { expr, property } => {
            let mut path = property_access_path(expr, edge_var)?;
            path.push(property.clone());
            Some(path)
        }
        _ => None,
    }
}

fn prepare_inline_property_cost(
    expr: &Expr,
    edge_var: &str,
    execution: &GqlExecutionContext,
    label_id: Option<EdgeLabelId>,
) -> Result<Option<PreparedInlineCost>, PlanQueryError> {
    let label_id = match label_id {
        Some(id) => id,
        None => return Ok(None),
    };
    let Some(path) = property_access_path(expr, edge_var) else {
        return Ok(None);
    };
    if path.is_empty() {
        return Ok(None);
    }
    let property_name = path[0].as_str();
    let Some(property_id) = execution.resolved_property_id(property_name) else {
        return Err(PlanQueryError::GleaphCost {
            message: format!(
                "COST BY e.property: property '{property_name}' is not projected for this query"
            ),
        });
    };
    let Some((inline_property_id, _profile)) =
        execution.resolved_edge_label_inline_property(label_id)
    else {
        return Err(PlanQueryError::GleaphCost {
            message: "COST BY e.property: edge label has no Router-resolved inline property".into(),
        });
    };
    if inline_property_id != property_id {
        return Err(PlanQueryError::GleaphCost {
            message: format!(
                "COST BY e.property: property '{property_name}' is not the inline slot for this edge label"
            ),
        });
    }
    let Some(schema) = execution.resolved_edge_label_inline_schema(label_id) else {
        return Ok(None);
    };
    if path.len() == 1 {
        if !schema.is_scalar() {
            return Err(PlanQueryError::GleaphCost {
                message: "COST BY e.property requires a scalar inline property or a struct field"
                    .into(),
            });
        }
    } else {
        let ResolvedInlineSchema::Struct { fields, .. } = schema else {
            return Err(PlanQueryError::GleaphCost {
                message: format!("COST BY e.{property_name} has no nested field"),
            });
        };
        let field_path = path[1..].join(".");
        if !fields.iter().any(|field| field.name == field_path) {
            return Err(PlanQueryError::GleaphCost {
                message: format!("COST BY e.{field_path} is not declared in the inline struct"),
            });
        }
    }
    Ok(Some(PreparedInlineCost {
        property_id,
        field_path: path[1..].to_vec(),
    }))
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
        ShortestMode::ShortestK(_) | ShortestMode::ShortestKGroup(_) => false,
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
        if let (WeightedCostOrderKey::Uint128(left), WeightedCostOrderKey::Uint128(right)) =
            (&self.order_key, &hop.order_key)
        {
            let sum = left
                .checked_add(*right)
                .ok_or_else(|| PlanQueryError::GleaphCost {
                    message: "shortest-path edge cost overflow".into(),
                })?;
            return Ok(Self {
                value: Value::Uint128(sum),
                order_key: if sum == 0 {
                    WeightedCostOrderKey::Zero
                } else {
                    WeightedCostOrderKey::Uint128(sum)
                },
            });
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
fn weighted_hop_cache_outer_key(edge: &EdgeBinding) -> u64 {
    u64::from(u32::from(edge.handle.owner_vertex_id)) << 32
        | u64::from(edge.handle.slot_index.raw())
}

fn weighted_hop_cache_value_key(edge: &EdgeBinding) -> u64 {
    let mut hasher = RapidHasher::default();
    hasher.write_u64(u64::try_from(edge.inline_property_len()).unwrap_or(u64::MAX));
    hasher.write(edge.inline_property_bytes_slice());
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
    label_expr: Option<&LabelExpr>,
    execution: &GqlExecutionContext,
    var_len: &Option<VarLenSpec>,
    edge_var: &str,
    cost_expr: &Expr,
    mode: ShortestMode,
    parameters: &BTreeMap<String, Value>,
    store_hop_edges: bool,
    _emit_edge_binding: bool,
) -> Result<ShortestPathSearchResult, PlanQueryError> {
    if let Some(k) = mode.shortest_k_limit() {
        return weighted_shortest_k_paths_between(
            store,
            src,
            dst,
            direction,
            label_id,
            label_expr,
            execution,
            var_len,
            k,
            edge_var,
            cost_expr,
            parameters,
            store_hop_edges,
            _emit_edge_binding,
        );
    }

    let element_id_key =
        crate::element_id_encoding::resolve_or_host_fixture(execution.element_id_encoding_key());
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
    let prepared_inline_cost =
        prepare_inline_property_cost(cost_expr, edge_var, execution, label_id)?;
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
    let mut inline_property_scratch = LabeledEdgeInlinePropertyBatchScratch::<Edge>::default();
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
            Some(prep) => prep.expand_into(
                store,
                current,
                &mut candidates,
                ShortestExpandOptions {
                    load_inline_property_bytes: true,
                    inline_property_scratch: Some(&mut inline_property_scratch),
                },
            )?,
            None => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _generic_scope = bench_scope("weighted_shortest_expand_generic");
                expand_candidates_into(
                    store,
                    &crate::gql_execution_context::GqlExecutionContext::default(),
                    current,
                    direction,
                    label_id,
                    EdgeSequenceOrder::Ascending,
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
        let base_cost = entry.cost.clone();
        for (edge_dst, edge_binding) in &candidates {
            if let Some(expr) = label_expr
                && !edge_binding_matches_label_expr(execution, expr, edge_binding)
            {
                continue;
            }
            let ExpandDst::Local(next) = *edge_dst else {
                continue;
            };
            relax_weighted_shortest_neighbor(
                next,
                &base_cost,
                depth,
                state_idx,
                &mut states,
                &mut heap,
                &mut tie,
                &mut found_min_cost,
                any_best_cost.as_mut(),
                store_hop_edges.then(|| edge_binding.clone()),
                || {
                    let outer = weighted_hop_cache_outer_key(edge_binding);
                    let value_key = weighted_hop_cache_value_key(edge_binding);
                    if let Some(cost) = hop_cost_cache.get(&outer).and_then(|m| m.get(&value_key)) {
                        #[cfg(all(feature = "canbench", target_family = "wasm"))]
                        let _scope = bench_scope("weighted_shortest_hop_cost_cache_hit");
                        return Ok(cost.clone());
                    }
                    #[cfg(all(feature = "canbench", target_family = "wasm"))]
                    let _scope = bench_scope("weighted_shortest_hop_cost_cache_miss");
                    let cost = eval_shortest_hop_cost(
                        store,
                        execution,
                        &element_id_key,
                        cost_expr,
                        edge_var,
                        edge_binding.clone(),
                        parameters,
                        prepared_inline_cost.clone(),
                    )?;
                    hop_cost_cache
                        .entry(outer)
                        .or_default()
                        .insert(value_key, cost.clone());
                    Ok(cost)
                },
            )?;
        }
    }

    Ok(ShortestPathSearchResult { states, found })
}

#[allow(clippy::too_many_arguments)]
fn weighted_shortest_k_paths_between(
    store: &GraphStore,
    src: VertexId,
    dst: VertexId,
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    label_expr: Option<&LabelExpr>,
    execution: &GqlExecutionContext,
    var_len: &Option<VarLenSpec>,
    k: u64,
    edge_var: &str,
    cost_expr: &Expr,
    parameters: &BTreeMap<String, Value>,
    store_hop_edges: bool,
    _emit_edge_binding: bool,
) -> Result<ShortestPathSearchResult, PlanQueryError> {
    let k = usize::try_from(k).map_err(|_| PlanQueryError::InvalidLimit {
        value: Value::Uint64(k),
    })?;
    let element_id_key =
        crate::element_id_encoding::resolve_or_host_fixture(execution.element_id_encoding_key());
    let bounds = var_len.unwrap_or(VarLenSpec {
        min: 1,
        max: Some(1),
    });
    let vertex_count = u64::from(u32::from(store.vertex_count()));
    let max_hops = bounds.max.unwrap_or_else(|| vertex_count.saturating_sub(1));

    let mut states = vec![PathSearchNode {
        current: src,
        previous: None,
        edge: None,
        depth: 0,
    }];
    if k == 0 {
        return Ok(ShortestPathSearchResult {
            states,
            found: Vec::new(),
        });
    }

    let mut heap = BinaryHeap::new();
    let mut tie = 0u64;
    heap.push(WeightedQueueEntry {
        cost: WeightedCost::zero(),
        tie,
        state_idx: 0,
    });

    let mut found = Vec::with_capacity(k.min(8));
    let mut found_min_cost = None;
    let mut hop_cost_cache: WeightedHopCostCache = IntMap::default();
    let prepared_inline_cost =
        prepare_inline_property_cost(cost_expr, edge_var, execution, label_id)?;
    let mut candidates = Vec::new();
    let mut inline_property_scratch = LabeledEdgeInlinePropertyBatchScratch::<Edge>::default();
    let fixed_label_expand = match label_id {
        Some(lid) => Some(ShortestFixedLabelExpand::new(direction, lid)?),
        None => None,
    };

    while let Some(entry) = heap.pop() {
        if found.len() >= k {
            break;
        }
        let state_idx = entry.state_idx;
        let current = states[state_idx].current;
        let depth = states[state_idx].depth;
        if depth >= bounds.min && current == dst {
            found.push(state_idx);
            continue;
        }
        if depth >= max_hops {
            continue;
        }

        candidates.clear();
        match fixed_label_expand {
            Some(prep) => prep.expand_into(
                store,
                current,
                &mut candidates,
                ShortestExpandOptions {
                    load_inline_property_bytes: true,
                    inline_property_scratch: Some(&mut inline_property_scratch),
                },
            )?,
            None => {
                expand_candidates_into(
                    store,
                    &crate::gql_execution_context::GqlExecutionContext::default(),
                    current,
                    direction,
                    label_id,
                    EdgeSequenceOrder::Ascending,
                    None,
                    None,
                    None,
                    &BTreeMap::new(),
                    &mut candidates,
                )?;
            }
        }
        let base_cost = entry.cost.clone();
        for (edge_dst, edge_binding) in &candidates {
            if let Some(expr) = label_expr
                && !edge_binding_matches_label_expr(execution, expr, edge_binding)
            {
                continue;
            }
            let ExpandDst::Local(next) = *edge_dst else {
                continue;
            };
            relax_weighted_shortest_neighbor(
                next,
                &base_cost,
                depth,
                state_idx,
                &mut states,
                &mut heap,
                &mut tie,
                &mut found_min_cost,
                None,
                store_hop_edges.then(|| edge_binding.clone()),
                || {
                    let outer = weighted_hop_cache_outer_key(edge_binding);
                    let value_key = weighted_hop_cache_value_key(edge_binding);
                    if let Some(cost) = hop_cost_cache.get(&outer).and_then(|m| m.get(&value_key)) {
                        return Ok(cost.clone());
                    }
                    let cost = eval_shortest_hop_cost(
                        store,
                        execution,
                        &element_id_key,
                        cost_expr,
                        edge_var,
                        edge_binding.clone(),
                        parameters,
                        prepared_inline_cost.clone(),
                    )?;
                    hop_cost_cache
                        .entry(outer)
                        .or_default()
                        .insert(value_key, cost.clone());
                    Ok(cost)
                },
            )?;
        }
    }

    Ok(ShortestPathSearchResult { states, found })
}

fn relax_weighted_shortest_neighbor(
    next: VertexId,
    base_cost: &WeightedCost,
    depth: u64,
    state_idx: usize,
    states: &mut Vec<PathSearchNode>,
    heap: &mut BinaryHeap<WeightedQueueEntry>,
    tie: &mut u64,
    found_min_cost: &mut Option<WeightedCost>,
    any_best_cost: Option<&mut IntMap<u32, WeightedCost>>,
    hop_edge: Option<EdgeBinding>,
    hop_cost: impl FnOnce() -> Result<WeightedCost, PlanQueryError>,
) -> Result<(), PlanQueryError> {
    if any_best_cost.is_none() && path_search_contains_vertex(states, state_idx, next) {
        return Ok(());
    }
    let hop_cost = hop_cost()?;
    let next_cost = base_cost.checked_add(&hop_cost)?;
    if let Some(min) = &*found_min_cost
        && matches!(next_cost.cmp(min), Ordering::Greater)
    {
        return Ok(());
    }
    if let Some(best_cost) = any_best_cost {
        let next_vertex = u32::from(next);
        if best_cost
            .get(&next_vertex)
            .is_some_and(|best| !matches!(next_cost.cmp(best), Ordering::Less))
        {
            return Ok(());
        }
        best_cost.insert(next_vertex, next_cost.clone());
    }
    *tie += 1;
    let next_state_idx = states.len();
    states.push(PathSearchNode {
        current: next,
        previous: Some(state_idx),
        edge: hop_edge,
        depth: depth + 1,
    });
    heap.push(WeightedQueueEntry {
        cost: next_cost,
        tie: *tie,
        state_idx: next_state_idx,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn eval_shortest_hop_cost(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    element_id_key: &ElementIdEncodingKey,
    expr: &Expr,
    edge_var: &str,
    edge_binding: EdgeBinding,
    parameters: &BTreeMap<String, Value>,
    prepared_inline_cost: Option<PreparedInlineCost>,
) -> Result<WeightedCost, PlanQueryError> {
    if let Some(prepared) = prepared_inline_cost {
        let value = try_read_inline_edge_property(
            &edge_binding,
            prepared.property_id,
            execution.resolved_labels.as_ref(),
        )?
        .ok_or_else(|| PlanQueryError::GleaphCost {
            message: "COST BY e.property: inline property read returned no value".into(),
        })?;
        let value = if prepared.field_path.is_empty() {
            value
        } else {
            let mut current = value;
            for field in prepared.field_path {
                let Value::Record(fields) = current else {
                    return Err(PlanQueryError::GleaphCost {
                        message: "COST BY inline struct field encountered a non-record value"
                            .into(),
                    });
                };
                current = fields
                    .into_iter()
                    .find(|(name, _)| name == &field)
                    .map(|(_, value)| value)
                    .ok_or_else(|| PlanQueryError::GleaphCost {
                        message: format!("COST BY inline struct field `{field}` is missing"),
                    })?;
            }
            current
        };
        return WeightedCost::from_value(value);
    }
    let mut row = PlanRow::new();
    row.insert(edge_var.to_string(), PlanBinding::Edge(edge_binding));
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs: None,
        caller: execution.caller,
        resolved_labels: execution.resolved_labels.as_ref(),
        resolved_properties: execution.resolved_properties.as_ref(),
        element_id_key: *element_id_key,
    };
    let value = evaluator.eval_expr(&row, expr)?;
    WeightedCost::from_value(value)
}
