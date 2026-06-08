use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::hash::Hasher;

use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, Expr, ExprKind};
use gleaph_gql::numeric_ops::{NumericOpError, eval_binary_numeric};
use gleaph_gql::numeric_order::{NormalizedNumeric, NumericOrderError, normalized_numeric_parts};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::{ShortestMode, VarLenSpec};
use gleaph_graph_kernel::entry::{EdgeLabelId, PreparedWeightDecoder};
use ic_stable_lara::VertexId;
use nohash_hasher::IntMap;
use rapidhash::fast::RapidHasher;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

use super::search::path_search_contains_vertex;
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::labeled::LabeledEdgePayloadBatchScratch;

use super::{
    PathSearchNode, ShortestExpandOptions, ShortestFixedLabelExpand, ShortestPathSearchResult,
};
use crate::facade::GraphStore;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::EdgeBinding;
use crate::plan::query::executor::context::QueryExprEvaluator;
use crate::plan::query::executor::expand::{ExpandDst, expand_candidates_into};
use crate::plan::query::executor::{EdgeSequenceOrder, PlanBinding};
use crate::plan::query::gleaph_weight;
use crate::plan::query::row::PlanRow;

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
fn weighted_hop_cache_outer_key(edge: &EdgeBinding) -> u64 {
    u64::from(u32::from(edge.handle.owner_vertex_id)) << 32 | u64::from(edge.handle.slot_index)
}

fn weighted_hop_cache_value_key(edge: &EdgeBinding) -> u64 {
    let mut hasher = RapidHasher::default();
    hasher.write_u64(u64::try_from(edge.payload_len()).unwrap_or(u64::MAX));
    hasher.write(edge.payload_bytes_slice());
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
    let mut payload_scratch = LabeledEdgePayloadBatchScratch::<Edge>::default();
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
                    load_payloads: true,
                    payload_scratch: Some(&mut payload_scratch),
                },
            )?,
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
        for (edge_dst, edge_binding) in candidates.iter().cloned() {
            let ExpandDst::Local(next) = edge_dst else {
                continue;
            };
            if any_best_cost.is_none() && path_search_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let hop_cost = if use_hop_cost_cache {
                let outer = weighted_hop_cache_outer_key(&edge_binding);
                let value_key = weighted_hop_cache_value_key(&edge_binding);
                if let Some(cost) = hop_cost_cache.get(&outer).and_then(|m| m.get(&value_key)) {
                    #[cfg(all(feature = "canbench", target_family = "wasm"))]
                    let _scope = bench_scope("weighted_shortest_hop_cost_cache_hit");
                    cost.clone()
                } else {
                    #[cfg(all(feature = "canbench", target_family = "wasm"))]
                    let _scope = bench_scope("weighted_shortest_hop_cost_cache_miss");
                    let cost = eval_shortest_hop_cost(
                        store,
                        cost_expr,
                        edge_var,
                        edge_binding.clone(),
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
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("weighted_shortest_hop_cost_decode_direct");
                decode_direct_gleaph_weight_hop_cost(store, edge_binding.clone())?
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
                edge: store_hop_edges.then_some(edge_binding.clone()),
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
        edge_binding.clone(),
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
        resolved_labels: None,
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
    if direct_gleaph_weight_hop_cost_decoder(expr, edge_var, gleaph_weight_decoders)?.is_none() {
        return Ok(None);
    }
    decode_direct_gleaph_weight_hop_cost(store, edge_binding).map(Some)
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
    if !gleaph_weight::is_gleaph_weight_call(name, *distinct) {
        return Ok(None);
    }
    let Some(arg) = gleaph_weight::gleaph_weight_single_arg(args) else {
        return Ok(None);
    };
    let Some(arg_edge_var) = gleaph_weight::gleaph_weight_arg_edge_var(arg) else {
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
    edge_binding: EdgeBinding,
) -> Result<WeightedCost, PlanQueryError> {
    let weight = gleaph_weight::decode_traversal_edge_weight(
        store,
        edge_binding.handle,
        edge_binding.payload_len(),
        edge_binding.payload_bytes_slice(),
    )?;
    Ok(WeightedCost::from_validated_non_negative_float32(weight))
}
