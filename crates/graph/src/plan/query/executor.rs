use super::aggregate;
use super::error::PlanQueryError;
use super::sort_keys::compare_sort_keys;
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use crate::gql_execution_context::{GqlExecutionContext, try_eval_runtime_function_call};
use crate::index::edge_equal;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::expr_evaluator::{
    SearchedCaseWhenOutcome, eval_abs_expr, eval_acos_expr, eval_and_expr, eval_asin_expr,
    eval_atan_expr, eval_binary_expr, eval_cast_expr, eval_ceil_expr, eval_compare_expr,
    eval_concat_expr, eval_cos_expr, eval_cosh_expr, eval_cot_expr, eval_degrees_expr,
    eval_exp_expr, eval_floor_expr, eval_ln_expr, eval_log_expr, eval_log10_expr, eval_mod_expr,
    eval_not_expr, eval_or_expr, eval_power_expr, eval_radians_expr, eval_sin_expr, eval_sinh_expr,
    eval_sqrt_expr, eval_tan_expr, eval_tanh_expr, eval_unary_expr, eval_xor_expr,
    searched_case_when_outcome, truthy,
};
use candid::Principal;
use gleaph_gql::ast::{
    BinaryOp, CmpOp, Expr, ExprKind, ObjectName, OrderByClause, SortDirection, TruthValue,
};
use gleaph_gql::numeric_ops::{NumericOpError, eval_binary_numeric};
use gleaph_gql::numeric_order::{NormalizedNumeric, NumericOrderError, normalized_numeric_parts};
use gleaph_gql::types::{EdgeDirection, LabelExpr, PathElement};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql::{Value, hash_value_for_join, value_to_index_key_bytes};
use gleaph_gql_planner::collect_expr_variables;
use gleaph_gql_planner::plan::{
    AggregateSpec, ConditionalScanCandidate, IndexScanSpec, PhysicalPlan, PlanOp, ProjectColumn,
    ScanValue, ShortestMode, ShortestPathCost, Str, VarLenSpec,
};
use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, PreparedWeightDecoder, Vertex,
    decode_inline_weight,
};
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};
use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::{BucketDirectedness, OutEdgeOrder};
use ic_stable_lara::traits::{CsrEdge, CsrVertexTombstone};
use nohash_hasher::{IntMap, IntSet};
use rapidhash::fast::RapidHasher;
#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::hash::Hasher;
use std::pin::Pin;
use std::sync::Arc;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

pub trait PlanQueryExecutor {
    fn execute_plan_query(
        &self,
        plan: &PhysicalPlan,
        parameters: &BTreeMap<String, Value>,
        execution: GqlExecutionContext,
    ) -> Result<PlanQueryResult, PlanQueryError>;
}

impl PlanQueryExecutor for GraphStore {
    fn execute_plan_query(
        &self,
        plan: &PhysicalPlan,
        parameters: &BTreeMap<String, Value>,
        execution: GqlExecutionContext,
    ) -> Result<PlanQueryResult, PlanQueryError> {
        pollster::block_on(execute_plan_query(self, plan, parameters, None, execution))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanQueryResult {
    pub rows: Vec<BTreeMap<String, Value>>,
}

impl PlanQueryResult {
    /// Hydrate binding rows into GQL [`Value`] rows (paths become [`Value::Path`], vertices full
    /// records, etc.). Inverse of [`execute_plan_query_bindings`] + this constructor is
    /// [`execute_plan_query`].
    pub fn try_from_plan_rows(
        store: &GraphStore,
        rows: &[PlanQueryRow],
    ) -> Result<Self, PlanQueryError> {
        Ok(Self {
            rows: materialize_plan_rows(store, rows)?,
        })
    }
}

/// Edge variable binding for one traversal hop: stable handle plus CSR `inline_value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeBinding {
    pub handle: EdgeHandle,
    pub inline_value: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanBinding {
    Vertex(VertexId),
    Edge(EdgeBinding),
    Value(Value),
    /// Shortest-path walk materialized to [`Value::Path`] only in [`binding_to_value`] / expression eval.
    Path(PathBinding),
}

/// One query result row before GQL [`Value`] materialization: column name → [`PlanBinding`].
///
/// Shortest-path columns stay as [`PlanBinding::Path`] until [`materialize_plan_rows`] or
/// [`PlanQueryResult::try_from_plan_rows`].
pub type PlanQueryRow = BTreeMap<String, PlanBinding>;

/// Alias used throughout the executor implementation.
pub(crate) type PlanRow = PlanQueryRow;

/// Bindings for each hash-join key column (planner order), used for equality and hashing.
type HashJoinKey = Vec<PlanBinding>;

/// Left subplan rows that share the same exact [`HashJoinKey`] within one hash bucket.
type HashJoinBucketEntry = (HashJoinKey, Vec<PlanRow>);

type HashJoinBuckets = IntMap<u64, Vec<HashJoinBucketEntry>>;

#[cfg(test)]
thread_local! {
    static NODE_SCAN_VISITS: Cell<usize> = const { Cell::new(0) };
}

/// Execute a read plan through [`execute_ops`] and return binding rows (paths stay
/// [`PlanBinding::Path`] until [`materialize_plan_rows`] or [`PlanQueryResult::try_from_plan_rows`]).
pub async fn execute_plan_query_bindings(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let gleaph_weight_decoders = {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("plan_query_prepare_gleaph_weight");
        super::gleaph_weight::prepare_gleaph_weight_decoders(store, &plan.ops)?
    };
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("plan_query_execute_ops");
    execute_ops(
        store,
        &plan.ops,
        parameters,
        index,
        execution,
        gleaph_weight_decoders.as_ref(),
    )
    .await
}

pub fn materialize_plan_rows(
    store: &GraphStore,
    rows: &[PlanRow],
) -> Result<Vec<BTreeMap<String, Value>>, PlanQueryError> {
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("plan_query_materialize_value_rows");
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(value_row(store, row)?);
    }
    Ok(out)
}

pub async fn execute_plan_query(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
) -> Result<PlanQueryResult, PlanQueryError> {
    let rows = execute_plan_query_bindings(store, plan, parameters, index, execution).await?;
    Ok(PlanQueryResult {
        rows: materialize_plan_rows(store, &rows)?,
    })
}

async fn execute_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    execute_ops_from(
        store,
        ops,
        parameters,
        vec![PlanRow::new()],
        index,
        execution,
        gleaph_weight_decoders,
    )
    .await
}

/// Variables that operators in `ops` may bind (used to NULL-pad `OptionalMatch` miss rows).
///
/// Downstream mandatory [`Expand`] / [`ShortestPath`] ops skip rows whose traversal
/// endpoints are null-padded optional bindings instead of failing in [`vertex_binding`].
fn subplan_written_vars(ops: &[PlanOp]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for op in ops {
        extend_subplan_written_vars_from_op(op, &mut out);
    }
    out
}

fn extend_subplan_written_vars_from_op(op: &PlanOp, out: &mut BTreeSet<String>) {
    match op {
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. }
        | PlanOp::IndexIntersection { variable, .. } => {
            out.insert(variable.to_string());
        }
        PlanOp::ConditionalIndexScan {
            candidates,
            fallback_variable,
            ..
        } => {
            out.insert(fallback_variable.to_string());
            for c in candidates {
                out.insert(c.variable.to_string());
            }
        }
        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            hop_aux_binding,
            ..
        } => {
            out.insert(edge.to_string());
            out.insert(near.to_string());
            out.insert(far.to_string());
            if let Some(h) = hop_aux_binding {
                out.insert(h.to_string());
            }
            // When EdgeBindEndpoints execution is implemented, `far` must honor
            // `expand_dst_matches_prebound_vertex` if `far` is already vertex-bound.
        }
        PlanOp::Expand {
            edge,
            dst,
            hop_aux_binding,
            ..
        }
        | PlanOp::ExpandFilter {
            edge,
            dst,
            hop_aux_binding,
            ..
        } => {
            out.insert(edge.to_string());
            out.insert(dst.to_string());
            if let Some(h) = hop_aux_binding {
                out.insert(h.to_string());
            }
        }
        PlanOp::ShortestPath { edge, path_var, .. } => {
            out.insert(edge.to_string());
            if let Some(p) = path_var {
                out.insert(p.to_string());
            }
        }
        PlanOp::Let { bindings } => {
            for b in bindings {
                out.insert(b.variable.clone());
            }
        }
        PlanOp::For {
            variable,
            ordinality,
            ..
        } => {
            out.insert(variable.to_string());
            if let Some(o) = ordinality {
                out.insert(o.to_string());
            }
        }
        PlanOp::WorstCaseOptimalJoin { variables, .. } => {
            for v in variables {
                out.insert(v.to_string());
            }
        }
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => {
            for child in sub_plan {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::UseGraph { sub_plan: None, .. } => {}
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            for child in left {
                extend_subplan_written_vars_from_op(child, out);
            }
            for child in right {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            for child in &sub_plan.ops {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::SetOperation { right, .. } => {
            for child in &right.ops {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::InsertVertex { variable, .. } => {
            if let Some(v) = variable {
                out.insert(v.to_string());
            }
        }
        PlanOp::InsertEdge { variable, .. } => {
            if let Some(v) = variable {
                out.insert(v.to_string());
            }
        }
        PlanOp::PropertyFilter { .. }
        | PlanOp::Filter { .. }
        | PlanOp::CallProcedure { .. }
        | PlanOp::Aggregate { .. }
        | PlanOp::Project { .. }
        | PlanOp::Sort { .. }
        | PlanOp::Limit { .. }
        | PlanOp::TopK { .. }
        | PlanOp::Materialize { .. }
        | PlanOp::SetProperties { .. }
        | PlanOp::RemoveProperties { .. }
        | PlanOp::DeleteVertex { .. }
        | PlanOp::DetachDeleteVertex { .. }
        | PlanOp::DeleteEdge { .. } => {}
    }
}

async fn execute_optional_match(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    rows: Vec<PlanRow>,
    sub_plan: &[PlanOp],
    written: &BTreeSet<String>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        let extended = execute_ops_from(
            store,
            sub_plan,
            parameters,
            vec![row.clone()],
            index,
            execution,
            gleaph_weight_decoders,
        )
        .await?;
        if extended.is_empty() {
            let mut padded = row;
            for v in written {
                if !padded.contains_key(v) {
                    padded.insert(v.clone(), PlanBinding::Value(Value::Null));
                }
            }
            out.push(padded);
        } else {
            out.extend(extended);
        }
    }
    Ok(out)
}

fn execute_ops_from<'a>(
    store: &'a GraphStore,
    ops: &'a [PlanOp],
    parameters: &'a BTreeMap<String, Value>,
    initial_rows: Vec<PlanRow>,
    index: Option<&'a dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<PlanRow>, PlanQueryError>> + 'a>> {
    Box::pin(async move {
        let mut rows = initial_rows;
        let caller = execution.caller;
        let gwd = gleaph_weight_decoders;
        // Index of the nearest preceding `PlanOp::Aggregate` for resolving
        // `ExprKind::Aggregate` in post-aggregate ops (e.g. `HAVING`).
        let mut active_aggregate_op_idx: Option<usize> = None;

        let mut op_idx = 0;
        while op_idx < ops.len() {
            let op = &ops[op_idx];
            let aggregate_specs = active_aggregate_op_idx.and_then(|idx| match &ops[idx] {
                PlanOp::Aggregate { aggregates, .. } => Some(aggregates.as_slice()),
                _ => None,
            });
            let evaluator = QueryExprEvaluator {
                store,
                parameters,
                aggregate_specs,
                caller,
                gleaph_weight_decoders: gwd,
            };
            if let Some(limit_idx) = limited_streaming_prefix_limit_idx(ops, op_idx) {
                let result = execute_limited_streaming_prefix(
                    store,
                    &ops[op_idx..=limit_idx],
                    rows,
                    parameters,
                    caller,
                    gwd,
                    aggregate_specs,
                )?;
                rows = result.rows;
                if result.clears_active_aggregate {
                    active_aggregate_op_idx = None;
                }
                op_idx = limit_idx + 1;
                continue;
            }
            rows = match op {
                PlanOp::NodeScan {
                    variable,
                    label,
                    property_projection: _,
                } => execute_node_scan(store, rows, variable, label.as_ref())?,
                PlanOp::IndexScan {
                    variable,
                    property,
                    value,
                    cmp,
                    property_projection: _,
                } => {
                    execute_index_scan(
                        store,
                        rows,
                        parameters,
                        index,
                        variable.as_ref(),
                        property.as_ref(),
                        value,
                        *cmp,
                    )
                    .await?
                }
                PlanOp::ConditionalIndexScan {
                    candidates,
                    fallback_label,
                    fallback_variable,
                    property_projection: _,
                } => {
                    execute_conditional_index_scan(
                        store,
                        rows,
                        parameters,
                        index,
                        candidates,
                        fallback_label.as_ref(),
                        fallback_variable,
                    )
                    .await?
                }
                PlanOp::IndexIntersection {
                    variable,
                    scans,
                    property_projection: _,
                } => {
                    execute_index_intersection(
                        store,
                        rows,
                        parameters,
                        index,
                        variable.as_ref(),
                        scans,
                    )
                    .await?
                }
                PlanOp::PropertyFilter { predicates, .. } => rows
                    .into_iter()
                    .filter_map(|row| match row_matches_all(&evaluator, &row, predicates) {
                        Ok(true) => Some(Ok(row)),
                        Ok(false) => None,
                        Err(err) => Some(Err(err)),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Let { bindings } => rows
                    .into_iter()
                    .map(|mut row| -> Result<PlanRow, PlanQueryError> {
                        for binding in bindings {
                            let value = evaluator.eval_expr(&row, &binding.value)?;
                            row.insert(binding.variable.clone(), PlanBinding::Value(value));
                        }
                        Ok(row)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Filter { condition } => rows
                    .into_iter()
                    .filter_map(|row| {
                        match row_matches_all(&evaluator, &row, std::slice::from_ref(condition)) {
                            Ok(true) => Some(Ok(row)),
                            Ok(false) => None,
                            Err(err) => Some(Err(err)),
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Expand {
                    src,
                    edge,
                    dst,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    edge_property_projection,
                    dst_property_projection,
                    hop_aux_binding,
                    emit_edge_binding,
                } => {
                    ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
                    let sequence_order = gleaph_sequence_order_after_expand(
                        ops,
                        op_idx,
                        edge.as_ref(),
                        label.is_some() && label_expr.is_none(),
                    )?;
                    execute_expand(
                        store,
                        rows,
                        parameters,
                        src,
                        edge,
                        dst,
                        *direction,
                        label.as_ref(),
                        sequence_order,
                        &[],
                        *emit_edge_binding,
                        indexed_edge_equality.as_ref(),
                        edge_property_projection.as_deref(),
                        dst_property_projection.as_deref(),
                        caller,
                        gwd,
                    )?
                }
                PlanOp::ExpandFilter {
                    src,
                    edge,
                    dst,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    dst_filter,
                    edge_property_projection,
                    dst_property_projection,
                    hop_aux_binding,
                    emit_edge_binding,
                } => {
                    ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
                    let sequence_order = gleaph_sequence_order_after_expand(
                        ops,
                        op_idx,
                        edge.as_ref(),
                        label.is_some() && label_expr.is_none(),
                    )?;
                    execute_expand(
                        store,
                        rows,
                        parameters,
                        src,
                        edge,
                        dst,
                        *direction,
                        label.as_ref(),
                        sequence_order,
                        dst_filter,
                        *emit_edge_binding,
                        indexed_edge_equality.as_ref(),
                        edge_property_projection.as_deref(),
                        dst_property_projection.as_deref(),
                        caller,
                        gwd,
                    )?
                }
                PlanOp::ShortestPath {
                    src,
                    dst,
                    edge,
                    path_var,
                    emit_edge_binding,
                    emit_path_binding,
                    mode,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    cost,
                } => execute_shortest_path(
                    store,
                    rows,
                    src,
                    dst,
                    edge,
                    path_var.as_ref(),
                    *emit_edge_binding,
                    *emit_path_binding,
                    *mode,
                    *direction,
                    label.as_ref(),
                    label_expr,
                    var_len,
                    cost,
                    parameters,
                    gwd,
                )?,
                PlanOp::Aggregate {
                    group_by,
                    aggregates,
                } => {
                    let agg_evaluator = QueryExprEvaluator {
                        store,
                        parameters,
                        aggregate_specs: None,
                        caller,
                        gleaph_weight_decoders: gwd,
                    };
                    let out =
                        aggregate::execute_aggregate(rows, group_by, aggregates, &agg_evaluator)?;
                    active_aggregate_op_idx = Some(op_idx);
                    out
                }
                PlanOp::Project { columns, distinct } => {
                    #[cfg(all(feature = "canbench", target_family = "wasm"))]
                    let _scope = bench_scope("plan_op_project");
                    let proj_evaluator = QueryExprEvaluator {
                        store,
                        parameters,
                        aggregate_specs,
                        caller,
                        gleaph_weight_decoders: gwd,
                    };
                    let mut projected = rows
                        .iter()
                        .map(|row| project_row(&proj_evaluator, row, columns))
                        .collect::<Result<Vec<_>, _>>()?;
                    if *distinct {
                        dedup_rows(&mut projected);
                    }
                    active_aggregate_op_idx = None;
                    projected
                }
                PlanOp::Limit { count, offset } => {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let count = match count {
                        Some(expr) => {
                            Some(limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?)
                        }
                        None => None,
                    };
                    rows.into_iter()
                        .skip(offset)
                        .take(count.unwrap_or(usize::MAX))
                        .collect()
                }
                PlanOp::Sort { order_by }
                    if gleaph_sequence_sort(order_by).is_some_and(|(edge_var, _)| {
                        previous_op_binds_edge(ops, op_idx, edge_var.as_str())
                    }) =>
                {
                    rows
                }
                PlanOp::Sort { order_by } => sort_rows(&evaluator, rows, order_by)?,
                PlanOp::TopK {
                    order_by,
                    k,
                    offset,
                } if gleaph_sequence_sort(order_by).is_some_and(|(edge_var, _)| {
                    previous_op_binds_edge(ops, op_idx, edge_var.as_str())
                }) =>
                {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let k = limit_value(&evaluator.eval_expr(&PlanRow::new(), k)?)?;
                    rows.into_iter().skip(offset).take(k).collect()
                }
                PlanOp::TopK {
                    order_by,
                    k,
                    offset,
                } => {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let k = limit_value(&evaluator.eval_expr(&PlanRow::new(), k)?)?;
                    sort_rows(&evaluator, rows, order_by)?
                        .into_iter()
                        .skip(offset)
                        .take(k)
                        .collect()
                }
                PlanOp::Materialize { columns, distinct } => {
                    let mut materialized = rows
                        .iter()
                        .map(|row| project_row(&evaluator, row, columns))
                        .collect::<Result<Vec<_>, _>>()?;
                    if *distinct {
                        dedup_rows(&mut materialized);
                    }
                    materialized
                }
                PlanOp::UseGraph {
                    graph_name: _,
                    sub_plan: Some(sub_plan),
                } => {
                    // v1 has a single physical GraphStore; USE scopes its sub-plan
                    // but does not route to a separate graph store yet.
                    execute_ops_from(store, sub_plan, parameters, rows, index, execution, gwd)
                        .await?
                }
                PlanOp::UseGraph {
                    graph_name: _,
                    sub_plan: None,
                } => {
                    // Same single-store v1 behavior: a bare USE marker is metadata.
                    rows
                }
                PlanOp::CartesianProduct { left, right } => {
                    execute_cartesian_product(
                        store, parameters, rows, left, right, index, execution, gwd,
                    )
                    .await?
                }
                PlanOp::HashJoin {
                    left,
                    right,
                    join_keys,
                } => {
                    execute_hash_join(
                        store, parameters, rows, left, right, join_keys, index, execution, gwd,
                    )
                    .await?
                }
                PlanOp::OptionalMatch { sub_plan } => {
                    let written = subplan_written_vars(sub_plan);
                    execute_optional_match(
                        store, parameters, rows, sub_plan, &written, index, execution, gwd,
                    )
                    .await?
                }
                other if other.is_dml() => {
                    return Err(PlanQueryError::UnsupportedOp(plan_op_name(other)));
                }
                other => return Err(PlanQueryError::UnsupportedOp(plan_op_name(other))),
            };
            op_idx += 1;
        }

        Ok(rows)
    })
}

struct LimitedStreamingPrefixResult {
    rows: Vec<PlanRow>,
    clears_active_aggregate: bool,
}

struct LimitedRows {
    offset_remaining: usize,
    take_remaining: usize,
    rows: Vec<PlanRow>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EdgeSequenceOrder {
    Ascending,
    #[default]
    Descending,
}

impl From<EdgeSequenceOrder> for OutEdgeOrder {
    fn from(value: EdgeSequenceOrder) -> Self {
        match value {
            EdgeSequenceOrder::Ascending => Self::Ascending,
            EdgeSequenceOrder::Descending => Self::Descending,
        }
    }
}

impl LimitedRows {
    fn new(offset: usize, count: usize) -> Self {
        Self {
            offset_remaining: offset,
            take_remaining: count,
            rows: Vec::new(),
        }
    }

    fn is_done(&self) -> bool {
        self.take_remaining == 0
    }

    fn push(&mut self, row: PlanRow) -> bool {
        if self.offset_remaining > 0 {
            self.offset_remaining -= 1;
            return false;
        }
        if self.take_remaining == 0 {
            return true;
        }
        self.rows.push(row);
        self.take_remaining -= 1;
        self.take_remaining == 0
    }
}

#[derive(Clone, Copy)]
enum CsrOffsetFastPath {
    ForwardLabel(LaraLabelId),
    ForwardDirectedness(BucketDirectedness),
    ReverseLabel(LaraLabelId),
    ReverseDirectedness(BucketDirectedness),
}

fn csr_offset_fast_path_for_expand(
    direction: EdgeDirection,
    label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
) -> Option<CsrOffsetFastPath> {
    if sequence_order != EdgeSequenceOrder::Descending {
        return None;
    }
    match direction {
        EdgeDirection::PointingRight => Some(match label_id {
            Some(lid) => {
                let storage = lid.pack(EdgeDirectedness::Directed);
                CsrOffsetFastPath::ForwardLabel(LaraLabelId::from_raw(storage.raw()))
            }
            None => CsrOffsetFastPath::ForwardDirectedness(BucketDirectedness::Directed),
        }),
        EdgeDirection::PointingLeft => Some(match label_id {
            Some(lid) => {
                let storage = lid.pack(EdgeDirectedness::Directed);
                CsrOffsetFastPath::ReverseLabel(LaraLabelId::from_raw(storage.raw()))
            }
            None => CsrOffsetFastPath::ReverseDirectedness(BucketDirectedness::Directed),
        }),
        EdgeDirection::Undirected => Some(match label_id {
            Some(lid) => {
                let storage = lid.pack(EdgeDirectedness::Undirected);
                CsrOffsetFastPath::ForwardLabel(LaraLabelId::from_raw(storage.raw()))
            }
            None => CsrOffsetFastPath::ForwardDirectedness(BucketDirectedness::Undirected),
        }),
        _ => None,
    }
}

fn stream_expand_owner_vertex_id(
    src_id: VertexId,
    _dst_id: VertexId,
    direction: EdgeDirection,
    _edge: Edge,
) -> VertexId {
    match direction {
        EdgeDirection::PointingRight => src_id,
        EdgeDirection::PointingLeft => src_id,
        EdgeDirection::Undirected => src_id,
        _ => unreachable!("unsupported CSR expand streaming direction"),
    }
}

fn visit_csr_expand_fast_path<Visit>(
    store: &GraphStore,
    src_id: VertexId,
    fast_path: CsrOffsetFastPath,
    offset_remaining: &mut usize,
    visit: Visit,
) -> Result<Result<bool, PlanQueryError>, GraphStoreError>
where
    Visit: FnMut(Edge) -> Result<bool, PlanQueryError>,
{
    match fast_path {
        CsrOffsetFastPath::ForwardLabel(label) => {
            store.skip_then_visit_each_out_edge_for_label(src_id, label, offset_remaining, visit)
        }
        CsrOffsetFastPath::ForwardDirectedness(directedness) => store
            .skip_then_visit_each_out_edge_by_directedness(
                src_id,
                directedness,
                offset_remaining,
                visit,
            ),
        CsrOffsetFastPath::ReverseLabel(label) => {
            store.skip_then_visit_each_in_edge_for_label(src_id, label, offset_remaining, visit)
        }
        CsrOffsetFastPath::ReverseDirectedness(directedness) => store
            .skip_then_visit_each_in_edge_by_directedness(
                src_id,
                directedness,
                offset_remaining,
                visit,
            ),
    }
}

fn build_expanded_row(
    store: &GraphStore,
    row: &PlanRow,
    edge_key: Option<&str>,
    dst_key: &str,
    dst_id: VertexId,
    edge_binding: EdgeBinding,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
) -> Result<PlanRow, PlanQueryError> {
    let mut expanded = row.clone();
    if let Some(edge_key) = edge_key {
        let edge_binding = if edge_property_projection.is_some_and(|props| !props.is_empty()) {
            PlanBinding::Value(edge_to_projected_record(
                store,
                edge_binding,
                edge_property_projection.unwrap(),
            )?)
        } else {
            PlanBinding::Edge(edge_binding)
        };
        expanded.insert(edge_key.to_owned(), edge_binding);
    }
    expanded.insert(
        dst_key.to_owned(),
        vertex_binding_for_projection(store, dst_id, dst_property_projection)?,
    );
    Ok(expanded)
}

/// Operators after an expand must not drop rows, so a CSR `advance_by` skip matches global `OFFSET`
/// semantics for that expand hop.
fn streaming_ops_preserve_row_cardinality_after(ops: &[PlanOp], start: usize) -> bool {
    let mut i = start;
    while i < ops.len() {
        match &ops[i] {
            PlanOp::Project { distinct, .. } if !distinct => i += 1,
            PlanOp::Let { .. } => i += 1,
            _ => return false,
        }
    }
    true
}

fn limited_streaming_prefix_limit_idx(ops: &[PlanOp], start_idx: usize) -> Option<usize> {
    for (idx, op) in ops.iter().enumerate().skip(start_idx) {
        match op {
            PlanOp::Limit { count: Some(_), .. } => return Some(idx),
            PlanOp::Limit { count: None, .. } => return None,
            op if streaming_prefix_op_supported(op) => {}
            _ => return None,
        }
    }
    None
}

fn streaming_prefix_op_supported(op: &PlanOp) -> bool {
    match op {
        PlanOp::NodeScan { .. }
        | PlanOp::PropertyFilter { .. }
        | PlanOp::Let { .. }
        | PlanOp::Filter { .. }
        | PlanOp::Expand { .. }
        | PlanOp::ExpandFilter { .. } => true,
        PlanOp::Project { distinct, .. } => !distinct,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_limited_streaming_prefix(
    store: &GraphStore,
    ops: &[PlanOp],
    initial_rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    aggregate_specs: Option<&[AggregateSpec]>,
) -> Result<LimitedStreamingPrefixResult, PlanQueryError> {
    let Some((PlanOp::Limit { count, offset }, streaming_ops)) = ops.split_last() else {
        return Ok(LimitedStreamingPrefixResult {
            rows: initial_rows,
            clears_active_aggregate: false,
        });
    };
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs,
        caller,
        gleaph_weight_decoders,
    };
    let offset = match offset {
        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
        None => 0,
    };
    let count = match count {
        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
        None => {
            return Ok(LimitedStreamingPrefixResult {
                rows: initial_rows,
                clears_active_aggregate: false,
            });
        }
    };
    let mut sink = LimitedRows::new(offset, count);
    let mut clears_active_aggregate = false;
    if sink.is_done() {
        return Ok(LimitedStreamingPrefixResult {
            rows: sink.rows,
            clears_active_aggregate,
        });
    }

    for row in initial_rows {
        if stream_row_through_ops(
            store,
            streaming_ops,
            0,
            row,
            parameters,
            caller,
            gleaph_weight_decoders,
            &evaluator,
            &mut sink,
            &mut clears_active_aggregate,
        )? {
            break;
        }
    }

    Ok(LimitedStreamingPrefixResult {
        rows: sink.rows,
        clears_active_aggregate,
    })
}

#[allow(clippy::too_many_arguments)]
fn stream_row_through_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let Some(op) = ops.get(op_idx) else {
        return Ok(sink.push(row));
    };
    match op {
        PlanOp::NodeScan {
            variable,
            label,
            property_projection: _,
        } => stream_node_scan(
            store,
            ops,
            op_idx,
            row,
            variable,
            label.as_ref(),
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        ),
        PlanOp::PropertyFilter { predicates, .. } => {
            if row_matches_all(evaluator, &row, predicates)? {
                stream_row_through_ops(
                    store,
                    ops,
                    op_idx + 1,
                    row,
                    parameters,
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                Ok(false)
            }
        }
        PlanOp::Let { bindings } => {
            let mut row = row;
            for binding in bindings {
                let value = evaluator.eval_expr(&row, &binding.value)?;
                row.insert(binding.variable.clone(), PlanBinding::Value(value));
            }
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                row,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::Filter { condition } => {
            if row_matches_all(evaluator, &row, std::slice::from_ref(condition))? {
                stream_row_through_ops(
                    store,
                    ops,
                    op_idx + 1,
                    row,
                    parameters,
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                Ok(false)
            }
        }
        PlanOp::Project { columns, distinct } => {
            debug_assert!(!distinct);
            let projected = project_row(evaluator, &row, columns)?;
            *clears_active_aggregate = true;
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                projected,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::Expand {
            src,
            edge,
            dst,
            direction,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            emit_edge_binding,
        } => {
            ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
            stream_expand(
                store,
                ops,
                op_idx,
                row,
                parameters,
                src,
                edge,
                dst,
                *direction,
                label.as_ref(),
                EdgeSequenceOrder::Descending,
                &[],
                *emit_edge_binding,
                indexed_edge_equality.as_ref(),
                edge_property_projection.as_deref(),
                dst_property_projection.as_deref(),
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::ExpandFilter {
            src,
            edge,
            dst,
            direction,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            dst_filter,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            emit_edge_binding,
        } => {
            ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
            stream_expand(
                store,
                ops,
                op_idx,
                row,
                parameters,
                src,
                edge,
                dst,
                *direction,
                label.as_ref(),
                EdgeSequenceOrder::Descending,
                dst_filter,
                *emit_edge_binding,
                indexed_edge_equality.as_ref(),
                edge_property_projection.as_deref(),
                dst_property_projection.as_deref(),
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        _ => unreachable!("limited streaming prefix only contains supported operators"),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_node_scan(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    variable: &Str,
    label: Option<&Str>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let label_id = label.and_then(|label| store.vertex_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(false);
    }

    for raw in 0..u32::from(store.vertex_count()) {
        #[cfg(test)]
        NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        }
        if let Some(filter) = label_id
            && !store.vertex_has_label(vertex_id, vertex, filter)
        {
            continue;
        }
        let mut scanned = row.clone();
        scanned.insert(variable.to_string(), PlanBinding::Vertex(vertex_id));
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            scanned,
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn stream_expand(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&Str>,
    sequence_order: EdgeSequenceOrder,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let label_id = label.and_then(|label| store.edge_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(false);
    }

    let Some(src_id) = vertex_binding_for_traversal(&row, src)? else {
        return Ok(false);
    };
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = csr_offset_fast_path_for_expand(direction, label_id, sequence_order);

    let csr_offset_fast_path = (indexed_edge_equality.is_none()
        && dst_filter.is_empty()
        && !matches!(row.get(dst.as_ref()), Some(PlanBinding::Vertex(_)))
        && streaming_ops_preserve_row_cardinality_after(ops, op_idx + 1))
    .then(|| csr_expand_fast_path)
    .flatten();

    if let Some(fast_path) = csr_offset_fast_path {
        let mut offset_slot = sink.offset_remaining;
        let mut visit = |edge: Edge| {
            // `skip_then_visit_each_out_edge_for_label` applies the global OFFSET inside the CSR
            // iterator; clear the sink-side skip before downstream `LimitedRows::push`.
            sink.offset_remaining = 0;
            let dst_id = edge.neighbor_vid();
            let owner_vertex_id = stream_expand_owner_vertex_id(src_id, dst_id, direction, edge);
            let edge_binding = EdgeBinding {
                handle: EdgeHandle {
                    owner_vertex_id,
                    label_id: LaraLabelId::from_raw(edge.label_id),
                    slot_index: edge.edge_slot_index.raw(),
                },
                inline_value: edge.inline_value,
            };
            let expanded = build_expanded_row(
                store,
                &row,
                edge_key.as_deref(),
                dst_key.as_str(),
                dst_id,
                edge_binding,
                edge_property_projection,
                dst_property_projection,
            );
            let expanded = expanded?;
            Ok(stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                expanded,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )?)
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        sink.offset_remaining = offset_slot;
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    if let Some(fast_path) = csr_expand_fast_path {
        let edge_equality_filter =
            edge_equality_stream_filter(store, indexed_edge_equality, parameters)?;
        if matches!(edge_equality_filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(false);
        }
        let mut offset_slot = 0;
        let mut visit = |edge: Edge| {
            let dst_id = edge.neighbor_vid();
            let owner_vertex_id = stream_expand_owner_vertex_id(src_id, dst_id, direction, edge);
            if !edge_matches_stream_filter(
                store,
                &edge_equality_filter,
                direction,
                owner_vertex_id,
                LaraLabelId::from_raw(edge.label_id),
                edge.edge_slot_index,
            )? {
                return Ok(false);
            }
            if !expand_dst_matches_prebound_vertex(&row, dst, dst_id) {
                return Ok(false);
            }
            if dst_only_prefilter
                && !vertex_row_matches_dst_filters(
                    store,
                    parameters,
                    dst,
                    dst_id,
                    dst_filter,
                    caller,
                    gleaph_weight_decoders,
                )?
            {
                return Ok(false);
            }
            let edge_binding = EdgeBinding {
                handle: EdgeHandle {
                    owner_vertex_id,
                    label_id: LaraLabelId::from_raw(edge.label_id),
                    slot_index: edge.edge_slot_index.raw(),
                },
                inline_value: edge.inline_value,
            };
            let expanded = build_expanded_row(
                store,
                &row,
                edge_key.as_deref(),
                dst_key.as_str(),
                dst_id,
                edge_binding,
                edge_property_projection,
                dst_property_projection,
            );
            let expanded = expanded?;
            if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
                return Ok(false);
            }
            Ok(stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                expanded,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )?)
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    let mut candidates = Vec::new();
    expand_candidates_into(
        store,
        src_id,
        direction,
        label_id,
        EdgeSequenceOrder::Descending,
        indexed_edge_equality,
        parameters,
        &mut candidates,
    )?;
    for (dst_id, edge_binding) in candidates.iter().copied() {
        if !expand_dst_matches_prebound_vertex(&row, dst, dst_id) {
            continue;
        }
        if dst_only_prefilter
            && !vertex_row_matches_dst_filters(
                store,
                parameters,
                dst,
                dst_id,
                dst_filter,
                caller,
                gleaph_weight_decoders,
            )?
        {
            continue;
        }
        let expanded = build_expanded_row(
            store,
            &row,
            edge_key.as_deref(),
            dst_key.as_str(),
            dst_id,
            edge_binding,
            edge_property_projection,
            dst_property_projection,
        );
        let expanded = expanded?;
        if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
            continue;
        }
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            expanded,
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn execute_cartesian_product(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    rows: Vec<PlanRow>,
    left: &[PlanOp],
    right: &[PlanOp],
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        let left_rows = execute_ops_from(
            store,
            left,
            parameters,
            vec![row.clone()],
            index,
            execution,
            gleaph_weight_decoders,
        )
        .await?;
        let right_rows = execute_ops_from(
            store,
            right,
            parameters,
            vec![row],
            index,
            execution,
            gleaph_weight_decoders,
        )
        .await?;
        for left_row in &left_rows {
            for right_row in &right_rows {
                if let Some(merged) = merge_rows(left_row, right_row) {
                    out.push(merged);
                }
            }
        }
    }
    Ok(out)
}

fn merge_rows(left: &PlanRow, right: &PlanRow) -> Option<PlanRow> {
    let mut merged = left.clone();
    for (name, right_binding) in right {
        match merged.get(name) {
            Some(left_binding) if left_binding != right_binding => return None,
            Some(_) => {}
            None => {
                merged.insert(name.clone(), right_binding.clone());
            }
        }
    }
    Some(merged)
}

/// Like [`merge_rows`], but caller guarantees join-key columns already match between `left` and `right`.
/// Skips re-checking join-key bindings (hot path for [`execute_hash_join`] after key equality).
fn merge_rows_with_known_join_keys(
    left: &PlanRow,
    right: &PlanRow,
    join_keys: &[Str],
) -> Option<PlanRow> {
    let mut merged = left.clone();
    for (name, right_binding) in right {
        let name_str = name.as_str();
        let skip_join_col = match join_keys {
            [only] => name_str == only.as_ref(),
            keys => keys.iter().any(|k| k.as_ref() == name_str),
        };
        if skip_join_col {
            continue;
        }
        match merged.get(name_str) {
            Some(existing) if existing != right_binding => return None,
            Some(_) => {}
            None => {
                merged.insert(name.clone(), right_binding.clone());
            }
        }
    }
    Some(merged)
}

async fn execute_hash_join(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    rows: Vec<PlanRow>,
    left: &[PlanOp],
    right: &[PlanOp],
    join_keys: &[Str],
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    if join_keys.is_empty() {
        return Err(PlanQueryError::UnsupportedOp("HashJoin(empty join_keys)"));
    }

    let mut out = Vec::new();
    for row in rows {
        let left_rows = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_left_subplan");
            execute_ops_from(
                store,
                left,
                parameters,
                vec![row.clone()],
                index,
                execution,
                gleaph_weight_decoders,
            )
            .await?
        };
        let right_rows = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_right_subplan");
            execute_ops_from(
                store,
                right,
                parameters,
                vec![row],
                index,
                execution,
                gleaph_weight_decoders,
            )
            .await?
        };

        let join_key_fast_vertex = join_keys.len() == 1 && {
            let jk = join_keys[0].as_ref();
            left_rows
                .iter()
                .all(|r| matches!(r.get(jk), Some(PlanBinding::Vertex(_))))
                && right_rows
                    .iter()
                    .all(|r| matches!(r.get(jk), Some(PlanBinding::Vertex(_))))
        };

        if join_key_fast_vertex {
            let jk = join_keys[0].as_ref();
            let left_by_vertex = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("hash_join_vertex_partition");
                let mut left_by_vertex: IntMap<u32, Vec<PlanRow>> = IntMap::default();
                for lr in left_rows {
                    let PlanBinding::Vertex(vid) = lr.get(jk).expect("join key binding") else {
                        unreachable!("join_key_fast_vertex pre-scan should guarantee Vertex");
                    };
                    left_by_vertex.entry(u32::from(*vid)).or_default().push(lr);
                }
                left_by_vertex
            };
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_vertex_probe_merge");
            for rr in right_rows {
                let Some(PlanBinding::Vertex(vid)) = rr.get(jk) else {
                    continue;
                };
                let Some(left_matches) = left_by_vertex.get(&u32::from(*vid)) else {
                    continue;
                };
                for lr in left_matches {
                    if let Some(merged) = merge_rows_with_known_join_keys(lr, &rr, join_keys) {
                        out.push(merged);
                    }
                }
            }
        } else {
            let buckets = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("hash_join_bucket_partition");
                let mut buckets: HashJoinBuckets = IntMap::default();
                for lr in left_rows {
                    let key = extract_join_key(&lr, join_keys)?;
                    insert_join_bucket(&mut buckets, key, lr);
                }
                buckets
            };

            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("hash_join_bucket_probe_merge");
            for rr in right_rows {
                let key = extract_join_key(&rr, join_keys)?;
                let h = hash_join_mix(&key);
                let Some(bucket) = buckets.get(&h) else {
                    continue;
                };
                for (left_key, left_matches) in bucket {
                    if left_key == &key {
                        for lr in left_matches {
                            if let Some(merged) =
                                merge_rows_with_known_join_keys(lr, &rr, join_keys)
                            {
                                out.push(merged);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn extract_join_key(row: &PlanRow, join_keys: &[Str]) -> Result<HashJoinKey, PlanQueryError> {
    join_keys
        .iter()
        .map(|k| {
            row.get(k.as_ref())
                .cloned()
                .ok_or_else(|| PlanQueryError::MissingBinding {
                    variable: k.as_ref().to_owned(),
                })
        })
        .collect()
}

fn insert_join_bucket(buckets: &mut HashJoinBuckets, key: HashJoinKey, row: PlanRow) {
    let h = hash_join_mix(&key);
    let bucket = buckets.entry(h).or_default();
    if let Some((_, rows)) = bucket.iter_mut().find(|(k, _)| k == &key) {
        rows.push(row);
    } else {
        bucket.push((key, vec![row]));
    }
}

/// Mix join-key bindings for hash buckets; must satisfy `a == b ⇒ mix(a) == mix(b)` for [`PlanBinding`].
fn hash_join_mix(bindings: &[PlanBinding]) -> u64 {
    let mut hasher = RapidHasher::default();
    for b in bindings {
        hash_plan_binding_for_join(b, &mut hasher);
    }
    hasher.finish()
}

fn hash_plan_binding_for_join(binding: &PlanBinding, hasher: &mut RapidHasher<'_>) {
    match binding {
        PlanBinding::Vertex(v) => {
            hasher.write_u8(1);
            hasher.write_u32(u32::from(*v));
        }
        PlanBinding::Edge(e) => {
            hasher.write_u8(2);
            hasher.write_u32(u32::from(e.handle.owner_vertex_id));
            hasher.write_u32(e.handle.slot_index);
            hasher.write_u16(e.inline_value);
        }
        PlanBinding::Value(v) => {
            hasher.write_u8(3);
            hash_value_for_join(v, hasher);
        }
        PlanBinding::Path(pb) => {
            hasher.write_u8(4);
            hasher.write_u64(pb.shard_id);
            hasher.write_usize(pb.leaf_state_idx);
            hasher.write_usize(Arc::as_ptr(&pb.states) as usize);
            hasher.write_usize(pb.states.len());
        }
    }
}

fn property_id_for_scan(store: &GraphStore, property_name: &str) -> Result<u32, PlanQueryError> {
    store
        .property_id(property_name)
        .map(|p| p.raw())
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan.unknown_property"))
}

fn resolve_scan_value_bytes(
    sv: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let v = match sv {
        ScanValue::Literal(val) => val.clone(),
        ScanValue::Parameter(name) => parameters.get(name.as_ref()).cloned().ok_or_else(|| {
            PlanQueryError::MissingParameter {
                name: name.to_string(),
            }
        })?,
    };
    value_to_index_key_bytes(&v).map_err(|_| PlanQueryError::InvalidExpressionValue {
        expression: "index scan value encoding".to_owned(),
    })
}

fn cmp_to_posting_range_request(
    cmp: CmpOp,
    bound_bytes: Vec<u8>,
) -> Result<PostingRangeRequest, PlanQueryError> {
    Ok(match cmp {
        CmpOp::Lt => PostingRangeRequest::Lt(bound_bytes),
        CmpOp::Le => PostingRangeRequest::Le(bound_bytes),
        CmpOp::Gt => PostingRangeRequest::Gt(bound_bytes),
        CmpOp::Ge => PostingRangeRequest::Ge(bound_bytes),
        CmpOp::Eq | CmpOp::Ne => {
            return Err(PlanQueryError::UnsupportedOp(
                "IndexScan.range(internal CmpOp)",
            ));
        }
    })
}

fn local_shard_filter_id() -> Result<u64, PlanQueryError> {
    GraphStore::new()
        .index_routing()
        .map(|r| r.shard_id)
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan(no shard routing)"))
}

fn filter_hits_for_local_shard(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &str,
    hits: &[PostingHit],
    shard: u64,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        for h in hits {
            if h.shard_id != shard {
                continue;
            }
            let vid = VertexId::from_le_bytes(h.vertex_id.to_le_bytes());
            let Some(vertex) = store.vertex(vid) else {
                continue;
            };
            if vertex.is_tombstone() {
                continue;
            }
            let mut r = row.clone();
            r.insert(variable.to_string(), PlanBinding::Vertex(vid));
            out.push(r);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn execute_index_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    variable: &str,
    property_name: &str,
    scan_value: &ScanValue,
    cmp: CmpOp,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let Some(ix) = index else {
        return Err(PlanQueryError::UnsupportedOp("IndexScan(no index client)"));
    };
    let shard = local_shard_filter_id()?;
    let pid = property_id_for_scan(store, property_name)?;
    let Some(bytes) = resolve_scan_value_bytes(scan_value, parameters)? else {
        return Ok(Vec::new());
    };
    let hits = if cmp == CmpOp::Eq {
        ix.lookup_equal(pid, bytes).await?
    } else {
        let req = cmp_to_posting_range_request(cmp, bytes)?;
        ix.lookup_range(pid, &req).await?
    };
    filter_hits_for_local_shard(store, rows, variable, &hits, shard)
}

async fn execute_conditional_index_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    candidates: &[ConditionalScanCandidate],
    fallback_label: Option<&Str>,
    fallback_variable: &Str,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    for c in candidates {
        let pv = parameters
            .get(c.param_name.as_ref())
            .cloned()
            .unwrap_or(Value::Null);
        if pv != Value::Null {
            let Some(bytes) = value_to_index_key_bytes(&pv).ok().flatten() else {
                break;
            };
            let Some(ix) = index else {
                return Err(PlanQueryError::UnsupportedOp(
                    "ConditionalIndexScan(no index client)",
                ));
            };
            let shard = local_shard_filter_id()?;
            let pid = property_id_for_scan(store, c.property.as_ref())?;
            let hits = if c.cmp == CmpOp::Eq {
                ix.lookup_equal(pid, bytes).await?
            } else {
                let req = cmp_to_posting_range_request(c.cmp, bytes)?;
                ix.lookup_range(pid, &req).await?
            };
            return filter_hits_for_local_shard(store, rows, c.variable.as_ref(), &hits, shard);
        }
    }
    execute_node_scan(store, rows, fallback_variable, fallback_label)
}

async fn execute_index_intersection(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    variable: &str,
    scans: &[IndexScanSpec],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let Some(ix) = index else {
        return Err(PlanQueryError::UnsupportedOp(
            "IndexIntersection(no index client)",
        ));
    };
    let shard = local_shard_filter_id()?;
    let mut sets: Vec<IntSet<u32>> = Vec::with_capacity(scans.len());
    for spec in scans {
        if spec.cmp != CmpOp::Eq {
            return Err(PlanQueryError::UnsupportedOp("IndexIntersection.cmp"));
        }
        let pid = property_id_for_scan(store, spec.property.as_ref())?;
        let Some(bytes) = resolve_scan_value_bytes(&spec.value, parameters)? else {
            return Ok(Vec::new());
        };
        let hits = ix.lookup_equal(pid, bytes).await?;
        let mut hs = IntSet::default();
        for h in hits {
            if h.shard_id != shard {
                continue;
            }
            let vid = VertexId::from_le_bytes(h.vertex_id.to_le_bytes());
            if let Some(vertex) = store.vertex(vid)
                && !vertex.is_tombstone()
            {
                hs.insert(u32::from(vid));
            }
        }
        sets.push(hs);
    }
    let mut intersection: Option<IntSet<u32>> = None;
    for s in sets {
        intersection = Some(match intersection {
            None => s,
            Some(prev) => prev.intersection(&s).copied().collect::<IntSet<_>>(),
        });
    }
    let Some(ids) = intersection else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in rows {
        for vid in &ids {
            let mut r = row.clone();
            r.insert(
                variable.to_string(),
                PlanBinding::Vertex(VertexId::from(*vid)),
            );
            out.push(r);
        }
    }
    Ok(out)
}

fn execute_node_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &Str,
    label: Option<&Str>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.vertex_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for row in rows {
        for raw in 0..u32::from(store.vertex_count()) {
            #[cfg(test)]
            NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
            let vertex_id = VertexId::from(raw);
            let Some(vertex) = store.vertex(vertex_id) else {
                continue;
            };
            if vertex.is_tombstone() {
                continue;
            }
            if let Some(filter) = label_id
                && !store.vertex_has_label(vertex_id, vertex, filter)
            {
                continue;
            }
            let mut row = row.clone();
            row.insert(variable.to_string(), PlanBinding::Vertex(vertex_id));
            out.push(row);
        }
    }
    Ok(out)
}

fn vertex_binding_for_projection(
    store: &GraphStore,
    vertex_id: VertexId,
    property_projection: Option<&[Str]>,
) -> Result<PlanBinding, PlanQueryError> {
    match property_projection {
        None | Some([]) => Ok(PlanBinding::Vertex(vertex_id)),
        Some(props) => Ok(PlanBinding::Value(vertex_to_projected_record(
            store, vertex_id, props,
        )?)),
    }
}

fn vertex_to_projected_record(
    store: &GraphStore,
    vertex_id: VertexId,
    properties: &[Str],
) -> Result<Value, PlanQueryError> {
    let mut fields = Vec::with_capacity(properties.len());
    for property in properties {
        let value = store
            .property_id(property.as_ref())
            .and_then(|property_id| store.vertex_property(vertex_id, property_id))
            .unwrap_or(Value::Null);
        fields.push((property.to_string(), value));
    }
    Ok(Value::Record(fields))
}

fn edge_to_projected_record(
    store: &GraphStore,
    binding: EdgeBinding,
    properties: &[Str],
) -> Result<Value, PlanQueryError> {
    let mut fields = Vec::with_capacity(properties.len());
    for property in properties {
        let value = store
            .property_id(property.as_ref())
            .and_then(|property_id| store.edge_property(binding.handle, property_id))
            .unwrap_or(Value::Null);
        fields.push((property.to_string(), value));
    }
    Ok(Value::Record(fields))
}

fn dst_filter_is_dst_vertex_only(dst_filter: &[Expr], dst: &str) -> bool {
    !dst_filter.is_empty()
        && dst_filter
            .iter()
            .all(|predicate| expr_variables_subset_of_dst(predicate, dst))
}

fn expr_variables_subset_of_dst(expr: &Expr, dst: &str) -> bool {
    collect_expr_variables(expr)
        .iter()
        .all(|variable| variable == dst)
}

fn vertex_row_matches_dst_filters(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    dst: &Str,
    dst_id: VertexId,
    dst_filter: &[Expr],
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<bool, PlanQueryError> {
    let mut stub = PlanRow::new();
    stub.insert(dst.to_string(), PlanBinding::Vertex(dst_id));
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs: None,
        caller,
        gleaph_weight_decoders,
    };
    row_matches_all(&evaluator, &stub, dst_filter)
}

fn edge_matches_indexed_equality(
    store: &GraphStore,
    owner_vertex_id: VertexId,
    label_id: LaraLabelId,
    edge_slot_index: EdgeSlotIndex,
    property: &str,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<bool, PlanQueryError> {
    let Some(property_id) = store.property_id(property) else {
        return Ok(false);
    };
    let Some(expected) = resolve_scan_value_bytes(scan_value, parameters)? else {
        return Ok(false);
    };
    let handle = EdgeHandle {
        owner_vertex_id,
        label_id,
        slot_index: edge_slot_index.raw(),
    };
    let Some(actual) = store.edge_property(handle, property_id) else {
        return Ok(false);
    };
    let actual_bytes =
        value_to_index_key_bytes(&actual).map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "indexed edge equality value encoding".to_owned(),
        })?;
    Ok(actual_bytes == Some(expected))
}

enum EdgeEqualityStreamFilter {
    None,
    NoMatches,
    /// Fast path when all postings share one label: `(owner, slot)` keys in an [`IntSet`].
    IndexedSingleLabel {
        label_id: u16,
        slots: IntSet<u64>,
    },
    /// Fallback when postings span multiple labels.
    IndexedMultiLabel(BTreeSet<(u32, u16, u32)>),
    StoreLookup {
        property_id: gleaph_graph_kernel::entry::PropertyId,
        expected: Vec<u8>,
    },
}

#[inline]
fn equality_index_slot_key(owner: VertexId, slot_index: u32) -> u64 {
    u64::from(u32::from(owner)) << 32 | u64::from(slot_index)
}

fn edge_equality_stream_filter(
    store: &GraphStore,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    parameters: &BTreeMap<String, Value>,
) -> Result<EdgeEqualityStreamFilter, PlanQueryError> {
    let Some((property, scan_value)) = indexed_edge_equality else {
        return Ok(EdgeEqualityStreamFilter::None);
    };
    let Some(property_id) = store.property_id(property.as_ref()) else {
        return Ok(EdgeEqualityStreamFilter::NoMatches);
    };
    let Some(expected) = resolve_scan_value_bytes(scan_value, parameters)? else {
        return Ok(EdgeEqualityStreamFilter::NoMatches);
    };
    let Some(postings) = edge_equal::lookup_equal(property_id, &expected) else {
        return Ok(EdgeEqualityStreamFilter::StoreLookup {
            property_id,
            expected,
        });
    };
    let mut labels = IntSet::default();
    let mut slots = IntSet::default();
    for posting in &postings {
        labels.insert(posting.label_id);
        slots.insert(equality_index_slot_key(
            posting.owner_vertex_id,
            posting.slot_index,
        ));
    }
    if labels.len() == 1 {
        let label_id = *labels.iter().next().expect("non-empty labels");
        Ok(EdgeEqualityStreamFilter::IndexedSingleLabel {
            label_id,
            slots,
        })
    } else {
        let mut heterogeneous = BTreeSet::new();
        for posting in &postings {
            heterogeneous.insert((
                u32::from(posting.owner_vertex_id),
                posting.label_id,
                posting.slot_index,
            ));
        }
        Ok(EdgeEqualityStreamFilter::IndexedMultiLabel(heterogeneous))
    }
}

fn edge_matches_stream_filter(
    store: &GraphStore,
    filter: &EdgeEqualityStreamFilter,
    direction: EdgeDirection,
    owner_vertex_id: VertexId,
    label_id: LaraLabelId,
    edge_slot_index: EdgeSlotIndex,
) -> Result<bool, PlanQueryError> {
    match filter {
        EdgeEqualityStreamFilter::None => Ok(true),
        EdgeEqualityStreamFilter::NoMatches => Ok(false),
        EdgeEqualityStreamFilter::IndexedSingleLabel { label_id: expected, slots } => {
            if label_id.raw() != *expected {
                return Ok(false);
            }
            let key = equality_index_slot_key(owner_vertex_id, edge_slot_index.raw());
            if slots.contains(&key) {
                return Ok(true);
            }
            if matches!(
                direction,
                EdgeDirection::PointingLeft | EdgeDirection::Undirected
            ) {
                let canonical = store.canonical_edge_handle(EdgeHandle {
                    owner_vertex_id,
                    label_id,
                    slot_index: edge_slot_index.raw(),
                });
                return Ok(slots.contains(&equality_index_slot_key(
                    canonical.owner_vertex_id,
                    canonical.slot_index,
                )));
            }
            Ok(false)
        }
        EdgeEqualityStreamFilter::IndexedMultiLabel(slots) => {
            if slots.contains(&(
                u32::from(owner_vertex_id),
                label_id.raw(),
                edge_slot_index.raw(),
            )) {
                return Ok(true);
            }
            if matches!(
                direction,
                EdgeDirection::PointingLeft | EdgeDirection::Undirected
            ) {
                let canonical = store.canonical_edge_handle(EdgeHandle {
                    owner_vertex_id,
                    label_id,
                    slot_index: edge_slot_index.raw(),
                });
                return Ok(slots.contains(&(
                    u32::from(canonical.owner_vertex_id),
                    canonical.label_id.raw(),
                    canonical.slot_index,
                )));
            }
            Ok(false)
        }
        EdgeEqualityStreamFilter::StoreLookup {
            property_id,
            expected,
        } => {
            let handle = EdgeHandle {
                owner_vertex_id,
                label_id,
                slot_index: edge_slot_index.raw(),
            };
            let Some(actual) = store.edge_property(handle, *property_id) else {
                return Ok(false);
            };
            let actual_bytes = value_to_index_key_bytes(&actual).map_err(|_| {
                PlanQueryError::InvalidExpressionValue {
                    expression: "indexed edge equality value encoding".to_owned(),
                }
            })?;
            Ok(actual_bytes.as_deref() == Some(expected.as_slice()))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_expand(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&Str>,
    sequence_order: EdgeSequenceOrder,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.edge_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs: None,
        caller,
        gleaph_weight_decoders,
    };
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let mut out = Vec::new();
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = csr_offset_fast_path_for_expand(direction, label_id, sequence_order);
    let edge_equality_filter = if csr_expand_fast_path.is_some() {
        let filter = edge_equality_stream_filter(store, indexed_edge_equality, parameters)?;
        if matches!(filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(Vec::new());
        }
        Some(filter)
    } else {
        None
    };
    let mut candidates = Vec::new();
    for row in rows {
        let Some(src_id) = vertex_binding_for_traversal(&row, src)? else {
            continue;
        };
        if let Some(fast_path) = csr_expand_fast_path {
            let mut offset_slot = 0;
            let mut visit = |edge: Edge| {
                let dst_id = edge.neighbor_vid();
                let owner_vertex_id =
                    stream_expand_owner_vertex_id(src_id, dst_id, direction, edge);
                if !edge_matches_stream_filter(
                    store,
                    edge_equality_filter
                        .as_ref()
                        .expect("filter exists with fast path"),
                    direction,
                    owner_vertex_id,
                    LaraLabelId::from_raw(edge.label_id),
                    edge.edge_slot_index,
                )? {
                    return Ok(false);
                }
                if !expand_dst_matches_prebound_vertex(&row, dst, dst_id) {
                    return Ok(false);
                }
                if dst_only_prefilter {
                    if !vertex_row_matches_dst_filters(
                        store,
                        parameters,
                        dst,
                        dst_id,
                        dst_filter,
                        caller,
                        gleaph_weight_decoders,
                    )? {
                        return Ok(false);
                    }
                }
                let edge_binding = EdgeBinding {
                    handle: EdgeHandle {
                        owner_vertex_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw(),
                    },
                    inline_value: edge.inline_value,
                };
                let expanded = build_expanded_row(
                    store,
                    &row,
                    edge_key.as_deref(),
                    dst_key.as_str(),
                    dst_id,
                    edge_binding,
                    edge_property_projection,
                    dst_property_projection,
                );
                let expanded = expanded?;
                if !dst_only_prefilter && !row_matches_all(&evaluator, &expanded, dst_filter)? {
                    return Ok(false);
                }
                out.push(expanded);
                Ok(false)
            };
            let res =
                visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
            match res {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(e.into()),
            }
            continue;
        }
        candidates.clear();
        expand_candidates_into(
            store,
            src_id,
            direction,
            label_id,
            sequence_order,
            indexed_edge_equality,
            parameters,
            &mut candidates,
        )?;
        for (dst_id, edge_binding) in candidates.iter().copied() {
            if !expand_dst_matches_prebound_vertex(&row, dst, dst_id) {
                continue;
            }
            if dst_only_prefilter {
                if !vertex_row_matches_dst_filters(
                    store,
                    parameters,
                    dst,
                    dst_id,
                    dst_filter,
                    caller,
                    gleaph_weight_decoders,
                )? {
                    continue;
                }
            }
            let expanded = build_expanded_row(
                store,
                &row,
                edge_key.as_deref(),
                dst_key.as_str(),
                dst_id,
                edge_binding,
                edge_property_projection,
                dst_property_projection,
            );
            let expanded = expanded?;
            if !dst_only_prefilter && !row_matches_all(&evaluator, &expanded, dst_filter)? {
                continue;
            }
            out.push(expanded);
        }
    }
    Ok(out)
}

fn expand_dst_matches_prebound_vertex(row: &PlanRow, dst: &Str, dst_id: VertexId) -> bool {
    match row.get(dst.as_ref()) {
        Some(PlanBinding::Vertex(id)) => dst_id == *id,
        _ => true,
    }
}

#[derive(Clone, Debug)]
struct PathSearchNode {
    current: VertexId,
    previous: Option<usize>,
    edge: Option<EdgeBinding>,
    depth: u64,
}

/// Lazy path result from [`execute_shortest_path`]: shares [`Arc`] search state across many rows.
#[derive(Clone, Debug)]
pub struct PathBinding {
    shard_id: u64,
    states: Arc<Vec<PathSearchNode>>,
    leaf_state_idx: usize,
}

impl PartialEq for PathBinding {
    fn eq(&self, other: &Self) -> bool {
        self.shard_id == other.shard_id
            && self.leaf_state_idx == other.leaf_state_idx
            && Arc::ptr_eq(&self.states, &other.states)
    }
}

impl Eq for PathBinding {}

struct ShortestPathSearchResult {
    states: Vec<PathSearchNode>,
    found: Vec<usize>,
}

#[allow(clippy::too_many_arguments)]
fn execute_shortest_path(
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
        let Some(src_id) = vertex_binding_for_traversal(&row, src)? else {
            continue;
        };
        let Some(dst_id) = vertex_binding_for_traversal(&row, dst)? else {
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
        for state_idx in found {
            let mut row = row.clone();
            if let Some(edge_key) = &edge_key {
                match path_states[state_idx].edge {
                    Some(edge_binding) => {
                        row.insert(edge_key.clone(), PlanBinding::Edge(edge_binding));
                    }
                    None => {
                        row.insert(edge_key.clone(), PlanBinding::Value(Value::Null));
                    }
                }
            }
            if let Some(path_key) = &path_key {
                row.insert(
                    path_key.clone(),
                    PlanBinding::Path(PathBinding {
                        shard_id,
                        states: Arc::clone(&path_states),
                        leaf_state_idx: state_idx,
                    }),
                );
            }
            out.push(row);
            if matches!(mode, ShortestMode::AnyShortest) {
                break;
            }
        }
    }
    Ok(out)
}

/// Pre-resolves catalog edge label → Lara storage key once per shortest-path search, then expands
/// with [`GraphStore::for_each_out_edges_for_label_unchecked`] to avoid repeated `ensure_vertex` and
/// `expand_candidates_into` plumbing per hop.
#[derive(Clone, Copy)]
enum ShortestFixedLabelExpand {
    Forward { storage: LaraLabelId },
    Reverse { storage: LaraLabelId },
    Undirected { storage: LaraLabelId },
}

impl ShortestFixedLabelExpand {
    fn new(direction: EdgeDirection, catalog: EdgeLabelId) -> Result<Self, PlanQueryError> {
        match direction {
            EdgeDirection::PointingRight => Ok(Self::Forward {
                storage: catalog.pack(EdgeDirectedness::Directed),
            }),
            EdgeDirection::PointingLeft => Ok(Self::Reverse {
                storage: catalog.pack(EdgeDirectedness::Directed),
            }),
            EdgeDirection::Undirected => Ok(Self::Undirected {
                storage: catalog.pack(EdgeDirectedness::Undirected),
            }),
            other => Err(PlanQueryError::UnsupportedDirection(other)),
        }
    }

    fn expand_into(
        self,
        store: &GraphStore,
        current: VertexId,
        out: &mut Vec<ExpandCandidate>,
    ) -> Result<(), PlanQueryError> {
        match self {
            Self::Forward { storage } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_forward");
                store
                    .for_each_out_edges_for_label_unchecked(current, storage, |edge| {
                        out.push((
                            edge.neighbor_vid(),
                            EdgeBinding {
                                handle: EdgeHandle {
                                    owner_vertex_id: current,
                                    label_id: LaraLabelId::from_raw(edge.label_id),
                                    slot_index: edge.edge_slot_index.raw(),
                                },
                                inline_value: edge.inline_value,
                            },
                        ));
                    })
                    .map_err(GraphStoreError::from)?;
            }
            Self::Reverse { storage } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_reverse");
                store
                    .for_each_in_edges_for_label_unchecked(current, storage, |edge| {
                        out.push((
                            edge.neighbor_vid(),
                            EdgeBinding {
                                handle: EdgeHandle {
                                    owner_vertex_id: current,
                                    label_id: LaraLabelId::from_raw(edge.label_id),
                                    slot_index: edge.edge_slot_index.raw(),
                                },
                                inline_value: edge.inline_value,
                            },
                        ));
                    })
                    .map_err(GraphStoreError::from)?;
            }
            Self::Undirected { storage } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("shortest_fixed_expand_undirected");
                store
                    .for_each_out_edges_for_label_unchecked(current, storage, |edge| {
                        out.push((
                            edge.neighbor_vid(),
                            EdgeBinding {
                                handle: EdgeHandle {
                                    owner_vertex_id: current,
                                    label_id: LaraLabelId::from_raw(edge.label_id),
                                    slot_index: edge.edge_slot_index.raw(),
                                },
                                inline_value: edge.inline_value,
                            },
                        ));
                    })
                    .map_err(GraphStoreError::from)?;
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
        for (next, edge_binding) in candidates.iter().copied() {
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

fn weighted_shortest_can_use_hop_count(mode: ShortestMode, cost_expr: &Expr) -> bool {
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
struct WeightedCost {
    value: Value,
    order_key: WeightedCostOrderKey,
}

#[derive(Clone, Debug)]
enum WeightedCostOrderKey {
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

    fn from_value(value: Value) -> Result<Self, PlanQueryError> {
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

    fn checked_add(&self, hop: &Self) -> Result<Self, PlanQueryError> {
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

type WeightedHopCostCache = IntMap<u64, IntMap<u16, WeightedCost>>;

#[inline]
fn weighted_hop_cache_outer_key(edge: EdgeBinding) -> u64 {
    u64::from(u32::from(edge.handle.owner_vertex_id)) << 32 | u64::from(edge.handle.slot_index)
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

fn weighted_shortest_paths_between(
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
        if let Some(ref min) = found_min_cost {
            if matches!(entry.cost.cmp(min), Ordering::Greater) {
                break;
            }
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
        for (next, edge_binding) in candidates.iter().copied() {
            if any_best_cost.is_none() && path_search_contains_vertex(&states, state_idx, next) {
                continue;
            }
            let hop_cost = if use_hop_cost_cache {
                let outer = weighted_hop_cache_outer_key(edge_binding);
                let inline = edge_binding.inline_value;
                if let Some(cost) = hop_cost_cache.get(&outer).and_then(|m| m.get(&inline)) {
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
                        .insert(inline, cost.clone());
                    cost
                }
            } else {
                decode_direct_gleaph_weight_hop_cost(
                    direct_gleaph_weight_decoder
                        .expect("direct GLEAPH.WEIGHT decoder must be present"),
                    edge_binding,
                )?
            };
            let next_cost = entry.cost.checked_add(&hop_cost)?;
            if let Some(ref min) = found_min_cost {
                if matches!(next_cost.cmp(min), Ordering::Greater) {
                    continue;
                }
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
    if let Some(cost) =
        eval_direct_gleaph_weight_hop_cost(expr, edge_var, edge_binding, gleaph_weight_decoders)?
    {
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
    decode_direct_gleaph_weight_hop_cost(decoder, edge_binding).map(Some)
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
    if !super::gleaph_weight::is_gleaph_weight_call(name, *distinct) {
        return Ok(None);
    }
    let Some(arg) = super::gleaph_weight::gleaph_weight_single_arg(args) else {
        return Ok(None);
    };
    let Some(arg_edge_var) = super::gleaph_weight::gleaph_weight_arg_edge_var(arg) else {
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

fn decode_direct_gleaph_weight_hop_cost(
    decoder: &PreparedWeightDecoder,
    edge_binding: EdgeBinding,
) -> Result<WeightedCost, PlanQueryError> {
    let weight = decode_inline_weight(decoder, edge_binding.inline_value).map_err(|e| {
        PlanQueryError::GleaphWeight {
            message: format!("GLEAPH.WEIGHT decode failed: {e}"),
        }
    })?;
    Ok(WeightedCost::from_validated_non_negative_float32(weight))
}

thread_local! {
    /// Reuses capacity when materializing many shortest-path rows on one thread (e.g. `AllShortest`).
    static PATH_MATERIALIZE_SCRATCH: RefCell<Vec<PathElement>> = RefCell::new(Vec::new());
}

/// Below this estimated element count (`depth * 2 + 1`), allocate a fresh `Vec` only: `thread_local`
/// lookup + `RefCell` borrow win nothing on tiny paths and showed up as instruction regressions in
/// `plan_query_materialize_value_rows` benches.
const PATH_MATERIALIZE_SCRATCH_MIN_ELEMENTS: usize = 16;

fn path_binding_to_value(pb: &PathBinding) -> Value {
    materialize_path_from_search_states(pb.shard_id, pb.states.as_ref(), pb.leaf_state_idx)
}

fn materialize_path_from_search_states(
    shard_id: u64,
    states: &[PathSearchNode],
    state_idx: usize,
) -> Value {
    let depth = states[state_idx].depth as usize;
    let min_cap = depth.saturating_mul(2).saturating_add(1);
    if min_cap < PATH_MATERIALIZE_SCRATCH_MIN_ELEMENTS {
        let mut elements = Vec::with_capacity(min_cap);
        fill_path_elements_leaf_to_root(shard_id, states, state_idx, &mut elements);
        return Value::Path(elements);
    }
    PATH_MATERIALIZE_SCRATCH.with(|scratch| {
        if let Ok(mut elements) = scratch.try_borrow_mut() {
            fill_path_elements_leaf_to_root(shard_id, states, state_idx, &mut elements);
            return Value::Path(std::mem::take(&mut *elements));
        }
        let mut elements = Vec::with_capacity(min_cap);
        fill_path_elements_leaf_to_root(shard_id, states, state_idx, &mut elements);
        Value::Path(elements)
    })
}

fn fill_path_elements_leaf_to_root(
    shard_id: u64,
    states: &[PathSearchNode],
    mut state_idx: usize,
    elements: &mut Vec<PathElement>,
) {
    elements.clear();
    let cap = states[state_idx].depth as usize * 2 + 1;
    if elements.capacity() < cap {
        elements.reserve(cap.saturating_sub(elements.capacity()));
    }
    loop {
        let state = &states[state_idx];
        elements.push(vertex_path_element(shard_id, state.current));
        if let Some(edge_binding) = state.edge {
            elements.push(edge_path_element(shard_id, edge_binding.handle));
        }
        let Some(previous) = state.previous else {
            break;
        };
        state_idx = previous;
    }
    elements.reverse();
}

fn local_shard_id(store: &GraphStore) -> u64 {
    store.index_routing().map(|r| r.shard_id).unwrap_or(0)
}

fn vertex_element_id_bytes(shard_id: u64, vertex_id: VertexId) -> Vec<u8> {
    GraphPathVertexId::new(shard_id, vertex_id)
        .to_bytes()
        .to_vec()
}

fn edge_element_id_bytes(
    shard_id: u64,
    owner_vertex_id: VertexId,
    edge_slot_index: gleaph_graph_kernel::entry::EdgeSlotIndex,
) -> Vec<u8> {
    GraphPathEdgeId::new(shard_id, owner_vertex_id, edge_slot_index)
        .to_bytes()
        .to_vec()
}

fn vertex_path_element(shard_id: u64, vertex_id: VertexId) -> PathElement {
    PathElement::Vertex(
        GraphPathVertexId::new(shard_id, vertex_id)
            .to_bytes()
            .into(),
    )
}

fn edge_path_element(shard_id: u64, handle: EdgeHandle) -> PathElement {
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

type ExpandCandidate = (VertexId, EdgeBinding);

fn expand_candidates_into(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<(), PlanQueryError> {
    let indexed = indexed_edge_equality.map(|(property, value)| (property.as_ref(), value));
    if let Some((property, scan_value)) = indexed
        && expand_candidates_via_equality_index(
            store,
            src_id,
            direction,
            edge_label_id,
            property,
            scan_value,
            parameters,
            out,
        )?
    {
        return Ok(());
    }

    match direction {
        EdgeDirection::PointingRight => {
            let mut error = None;
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if let Some((property, scan_value)) = indexed {
                        match edge_matches_indexed_equality(
                            store,
                            src_id,
                            LaraLabelId::from_raw(edge.label_id),
                            edge.edge_slot_index,
                            property,
                            scan_value,
                            parameters,
                        ) {
                            Ok(false) => return,
                            Ok(true) => {}
                            Err(err) => {
                                error = Some(err);
                                return;
                            }
                        }
                    }
                    out.push((
                        edge.neighbor_vid(),
                        EdgeBinding {
                            handle: EdgeHandle {
                                owner_vertex_id: src_id,
                                label_id: LaraLabelId::from_raw(edge.label_id),
                                slot_index: edge.edge_slot_index.raw(),
                            },
                            inline_value: edge.inline_value,
                        },
                    ));
                },
            )?;
            if let Some(err) = error {
                return Err(err);
            }
        }
        EdgeDirection::PointingLeft => {
            let mut error = None;
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if let Some((property, scan_value)) = indexed {
                        match edge_matches_indexed_equality(
                            store,
                            src_id,
                            LaraLabelId::from_raw(edge.label_id),
                            edge.edge_slot_index,
                            property,
                            scan_value,
                            parameters,
                        ) {
                            Ok(false) => return,
                            Ok(true) => {}
                            Err(err) => {
                                error = Some(err);
                                return;
                            }
                        }
                    }
                    out.push((
                        edge.neighbor_vid(),
                        EdgeBinding {
                            handle: EdgeHandle {
                                owner_vertex_id: src_id,
                                label_id: LaraLabelId::from_raw(edge.label_id),
                                slot_index: edge.edge_slot_index.raw(),
                            },
                            inline_value: edge.inline_value,
                        },
                    ));
                },
            )?;
            if let Some(err) = error {
                return Err(err);
            }
        }
        EdgeDirection::Undirected => {
            let mut error = None;
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                sequence_order,
                |edge| {
                    if error.is_some() {
                        return;
                    }
                    if let Some((property, scan_value)) = indexed {
                        match edge_matches_indexed_equality(
                            store,
                            src_id,
                            LaraLabelId::from_raw(edge.label_id),
                            edge.edge_slot_index,
                            property,
                            scan_value,
                            parameters,
                        ) {
                            Ok(false) => return,
                            Ok(true) => {}
                            Err(err) => {
                                error = Some(err);
                                return;
                            }
                        }
                    }
                    out.push((
                        edge.neighbor_vid(),
                        EdgeBinding {
                            handle: EdgeHandle {
                                owner_vertex_id: src_id,
                                label_id: LaraLabelId::from_raw(edge.label_id),
                                slot_index: edge.edge_slot_index.raw(),
                            },
                            inline_value: edge.inline_value,
                        },
                    ));
                },
            )?;
            if let Some(err) = error {
                return Err(err);
            }
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    Ok(())
}

fn for_each_csr_expand_edge<F>(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    sequence_order: EdgeSequenceOrder,
    visit: F,
) -> Result<(), PlanQueryError>
where
    F: FnMut(Edge),
{
    match direction {
        EdgeDirection::PointingRight | EdgeDirection::Undirected => {
            if let Some(lid) = edge_label_id {
                let storage = lid.pack(if matches!(direction, EdgeDirection::Undirected) {
                    EdgeDirectedness::Undirected
                } else {
                    EdgeDirectedness::Directed
                });
                store
                    .for_each_out_edges_for_label_ordered(
                        src_id,
                        LaraLabelId::from_raw(storage.raw()),
                        sequence_order.into(),
                        visit,
                    )
                    .map_err(GraphStoreError::from)?;
            } else {
                let directedness = match direction {
                    EdgeDirection::PointingRight => BucketDirectedness::Directed,
                    EdgeDirection::Undirected => BucketDirectedness::Undirected,
                    _ => unreachable!(),
                };
                store
                    .for_each_out_edges_by_directedness_unchecked(
                        src_id,
                        directedness,
                        sequence_order.into(),
                        visit,
                    )
                    .map_err(GraphStoreError::from)?;
            }
            Ok(())
        }
        EdgeDirection::PointingLeft => {
            if let Some(lid) = edge_label_id {
                let storage = lid.pack(EdgeDirectedness::Directed);
                store
                    .for_each_in_edges_for_label_ordered(
                        src_id,
                        LaraLabelId::from_raw(storage.raw()),
                        sequence_order.into(),
                        visit,
                    )
                    .map_err(GraphStoreError::from)?;
            } else {
                store
                    .for_each_in_edges_by_directedness_unchecked(
                        src_id,
                        BucketDirectedness::Directed,
                        sequence_order.into(),
                        visit,
                    )
                    .map_err(GraphStoreError::from)?;
            }
            Ok(())
        }
        other => Err(PlanQueryError::UnsupportedDirection(other)),
    }
}

/// Probes the in-process edge equality index and, on hit, enumerates only matching slots.
/// Returns `Ok(true)` when the index owned the lookup (including zero matches).
fn expand_candidates_via_equality_index(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<EdgeLabelId>,
    property: &str,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
    out: &mut Vec<ExpandCandidate>,
) -> Result<bool, PlanQueryError> {
    if !matches!(direction, EdgeDirection::PointingRight) {
        return Ok(false);
    }
    let Some(property_id) = store.property_id(property) else {
        return Ok(false);
    };
    let Some(expected) = resolve_scan_value_bytes(scan_value, parameters)? else {
        out.clear();
        return Ok(true);
    };
    let Some(postings) = edge_equal::lookup_equal(property_id, &expected) else {
        return Ok(false);
    };

    let mut out_slots: BTreeSet<(u16, u32)> = BTreeSet::new();
    let mut in_slots: BTreeSet<(u32, u16, u32)> = BTreeSet::new();
    for posting in &postings {
        if posting.owner_vertex_id == src_id {
            out_slots.insert((posting.label_id, posting.slot_index));
        }
        in_slots.insert((
            u32::from(posting.owner_vertex_id),
            posting.label_id,
            posting.slot_index,
        ));
    }

    match direction {
        EdgeDirection::PointingRight => {
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                EdgeSequenceOrder::Descending,
                |edge| {
                    if !out_slots.contains(&(edge.label_id, edge.edge_slot_index.raw())) {
                        return;
                    }
                    out.push((
                        edge.neighbor_vid(),
                        EdgeBinding {
                            handle: EdgeHandle {
                                owner_vertex_id: src_id,
                                label_id: LaraLabelId::from_raw(edge.label_id),
                                slot_index: edge.edge_slot_index.raw(),
                            },
                            inline_value: edge.inline_value,
                        },
                    ));
                },
            )?;
        }
        EdgeDirection::PointingLeft => {
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                EdgeSequenceOrder::Descending,
                |edge| {
                    let canonical = store.canonical_edge_handle(EdgeHandle {
                        owner_vertex_id: src_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw(),
                    });
                    if !in_slots.contains(&(
                        u32::from(canonical.owner_vertex_id),
                        canonical.label_id.raw(),
                        canonical.slot_index,
                    )) {
                        return;
                    }
                    out.push((
                        edge.neighbor_vid(),
                        EdgeBinding {
                            handle: EdgeHandle {
                                owner_vertex_id: src_id,
                                label_id: LaraLabelId::from_raw(edge.label_id),
                                slot_index: edge.edge_slot_index.raw(),
                            },
                            inline_value: edge.inline_value,
                        },
                    ));
                },
            )?;
        }
        EdgeDirection::Undirected => {
            for_each_csr_expand_edge(
                store,
                src_id,
                direction,
                edge_label_id,
                EdgeSequenceOrder::Descending,
                |edge| {
                    let canonical = store.canonical_edge_handle(EdgeHandle {
                        owner_vertex_id: src_id,
                        label_id: LaraLabelId::from_raw(edge.label_id),
                        slot_index: edge.edge_slot_index.raw(),
                    });
                    if !in_slots.contains(&(
                        u32::from(canonical.owner_vertex_id),
                        canonical.label_id.raw(),
                        canonical.slot_index,
                    )) {
                        return;
                    }
                    out.push((
                        edge.neighbor_vid(),
                        EdgeBinding {
                            handle: EdgeHandle {
                                owner_vertex_id: src_id,
                                label_id: LaraLabelId::from_raw(edge.label_id),
                                slot_index: edge.edge_slot_index.raw(),
                            },
                            inline_value: edge.inline_value,
                        },
                    ));
                },
            )?;
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    Ok(true)
}

fn ensure_simple_expand(
    label_expr: &Option<LabelExpr>,
    var_len: &Option<gleaph_gql_planner::plan::VarLenSpec>,
    hop_aux_binding: &Option<Str>,
) -> Result<(), PlanQueryError> {
    if label_expr.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.label_expr"));
    }
    if var_len.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.var_len"));
    }
    if hop_aux_binding.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.hop_aux_binding"));
    }
    Ok(())
}

fn row_matches_all(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    predicates: &[Expr],
) -> Result<bool, PlanQueryError> {
    for predicate in predicates {
        let value = evaluator.eval_expr(row, predicate)?;
        if truthy(value).map_err(PlanQueryError::from)? != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn sort_rows(
    evaluator: &QueryExprEvaluator<'_>,
    rows: Vec<PlanRow>,
    order_by: &OrderByClause,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut keyed_rows = rows
        .into_iter()
        .map(|row| {
            let keys = order_by
                .items
                .iter()
                .map(|item| eval_sort_expr(evaluator, &row, &item.expr))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((keys, row))
        })
        .collect::<Result<Vec<_>, PlanQueryError>>()?;

    for left_idx in 0..keyed_rows.len() {
        for right_idx in (left_idx + 1)..keyed_rows.len() {
            compare_sort_keys(&keyed_rows[left_idx].0, &keyed_rows[right_idx].0, order_by)?;
        }
    }

    keyed_rows.sort_by(|(left_keys, _), (right_keys, _)| {
        compare_sort_keys(left_keys, right_keys, order_by)
            .expect("sort keys are pre-validated before sorting")
    });

    Ok(keyed_rows.into_iter().map(|(_, row)| row).collect())
}

fn gleaph_sequence_order_after_expand(
    ops: &[PlanOp],
    expand_idx: usize,
    edge_var: &str,
    has_single_fixed_label: bool,
) -> Result<EdgeSequenceOrder, PlanQueryError> {
    for op in &ops[expand_idx + 1..] {
        match op {
            PlanOp::Sort { order_by } | PlanOp::TopK { order_by, .. } => {
                let Some((sort_edge_var, order)) = gleaph_sequence_sort(order_by) else {
                    continue;
                };
                if sort_edge_var != edge_var {
                    continue;
                }
                if !has_single_fixed_label {
                    return Err(PlanQueryError::GleaphSequence {
                        message: format!(
                            "ORDER BY GLEAPH.SEQUENCE({sort_edge_var}) requires a single fixed edge label"
                        ),
                    });
                }
                return Ok(order);
            }
            PlanOp::Expand { edge, .. } | PlanOp::ExpandFilter { edge, .. }
                if edge.as_ref() == edge_var =>
            {
                break;
            }
            PlanOp::Aggregate { .. }
            | PlanOp::ShortestPath { .. }
            | PlanOp::HashJoin { .. }
            | PlanOp::CartesianProduct { .. }
            | PlanOp::OptionalMatch { .. }
            | PlanOp::Materialize { .. } => break,
            _ => {}
        }
    }
    Ok(EdgeSequenceOrder::Descending)
}

fn previous_op_binds_edge(ops: &[PlanOp], op_idx: usize, edge_var: &str) -> bool {
    for op in ops[..op_idx].iter().rev() {
        match op {
            PlanOp::Expand { edge, .. } | PlanOp::ExpandFilter { edge, .. } => {
                return edge.as_ref() == edge_var;
            }
            PlanOp::Aggregate { .. }
            | PlanOp::ShortestPath { .. }
            | PlanOp::HashJoin { .. }
            | PlanOp::CartesianProduct { .. }
            | PlanOp::OptionalMatch { .. }
            | PlanOp::Materialize { .. } => return false,
            _ => {}
        }
    }
    false
}

fn gleaph_sequence_sort(order_by: &OrderByClause) -> Option<(String, EdgeSequenceOrder)> {
    let [item] = order_by.items.as_slice() else {
        return None;
    };
    if item.null_order.is_some() {
        return None;
    }
    let order = match item.direction {
        Some(SortDirection::Asc | SortDirection::Ascending) => EdgeSequenceOrder::Ascending,
        Some(SortDirection::Desc | SortDirection::Descending) | None => {
            EdgeSequenceOrder::Descending
        }
    };
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &item.expr.kind
    else {
        return None;
    };
    if !is_gleaph_sequence_call(name, *distinct) || args.len() != 1 {
        return None;
    }
    super::gleaph_weight::gleaph_weight_arg_edge_var(&args[0]).map(|edge_var| (edge_var, order))
}

fn is_gleaph_sequence_call(name: &ObjectName, distinct: bool) -> bool {
    !distinct
        && name.parts.len() == 2
        && name.parts[0].eq_ignore_ascii_case("gleaph")
        && name.parts[1].eq_ignore_ascii_case("sequence")
}

fn eval_sort_expr(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    expr: &Expr,
) -> Result<Value, PlanQueryError> {
    match evaluator.eval_expr(row, expr) {
        Ok(value) => Ok(value),
        Err(PlanQueryError::MissingBinding { .. }) => {
            let projected_name = expression_name(expr);
            match row.get(&projected_name) {
                Some(PlanBinding::Value(value)) => Ok(value.clone()),
                Some(binding) => binding_to_value(evaluator.store, binding),
                None => Err(PlanQueryError::MissingBinding {
                    variable: projected_name,
                }),
            }
        }
        Err(err) => Err(err),
    }
}

struct QueryExprEvaluator<'a> {
    store: &'a GraphStore,
    parameters: &'a BTreeMap<String, Value>,
    /// When set, `ExprKind::Aggregate` reads precomputed results from the row
    /// (see [`aggregate_slot_key`]). Sourced from the active preceding
    /// [`PlanOp::Aggregate`] (not necessarily `ops[op_idx - 1]`, e.g. when `HAVING`
    /// inserts a [`PlanOp::Filter`] between aggregate and project).
    aggregate_specs: Option<&'a [AggregateSpec]>,
    /// IC caller for runtime functions such as `MSG_CALLER()`.
    caller: Option<Principal>,
    /// Prepared decoders for `GLEAPH.WEIGHT(edgeVar)` (when the query uses it).
    gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
}

fn try_eval_gleaph_weight(
    decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    name: &ObjectName,
    args: &[Expr],
    distinct: bool,
    row: &PlanRow,
) -> Result<Option<Value>, PlanQueryError> {
    if !super::gleaph_weight::is_gleaph_weight_call(name, distinct) {
        return Ok(None);
    }
    // Inline edge weights decode to FLOAT32; cost expressions may widen via casts or arithmetic.
    if distinct {
        return Err(PlanQueryError::GleaphWeight {
            message: "GLEAPH.WEIGHT does not support DISTINCT".into(),
        });
    }
    let map = decoders.ok_or_else(|| PlanQueryError::GleaphWeight {
        message: "GLEAPH.WEIGHT requires query preparation (no decoder table)".into(),
    })?;
    if args.len() != 1 {
        return Err(PlanQueryError::GleaphWeight {
            message: format!("GLEAPH.WEIGHT expects 1 argument, got {}", args.len()),
        });
    }
    let Some(edge_var) = super::gleaph_weight::gleaph_weight_arg_edge_var(&args[0]) else {
        return Err(PlanQueryError::GleaphWeight {
            message: "GLEAPH.WEIGHT argument must be an edge variable".into(),
        });
    };
    let decoder = map
        .get(&edge_var)
        .ok_or_else(|| PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH.WEIGHT({edge_var}): no prepared decoder for this edge variable"
            ),
        })?;
    let binding = row
        .get(edge_var.as_str())
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: edge_var.clone(),
        })?;
    match binding {
        PlanBinding::Value(Value::Null) => return Ok(Some(Value::Null)),
        PlanBinding::Edge(edge) => {
            let w = decode_inline_weight(decoder, edge.inline_value).map_err(|e| {
                PlanQueryError::GleaphWeight {
                    message: format!("GLEAPH.WEIGHT decode failed: {e}"),
                }
            })?;
            Ok(Some(Value::Float32(w)))
        }
        _ => Err(PlanQueryError::GleaphWeight {
            message: format!("GLEAPH.WEIGHT({edge_var}): binding is not an edge"),
        }),
    }
}

impl QueryExprEvaluator<'_> {
    fn eval_expr(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Paren(inner) => self.eval_expr(row, inner),
            ExprKind::Variable(name) => binding_to_value(
                self.store,
                row.get(name)
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: name.clone(),
                    })?,
            ),
            ExprKind::ElementId(expr) => self.eval_element_id(row, expr),
            ExprKind::Parameter(name) => self
                .parameters
                .get(name)
                .cloned()
                .ok_or_else(|| PlanQueryError::MissingParameter { name: name.clone() }),
            ExprKind::PropertyAccess { expr, property } => self.eval_property(row, expr, property),
            ExprKind::UnaryOp { op, expr } => {
                let value = self.eval_expr(row, expr)?;
                eval_unary_expr(*op, value).map_err(PlanQueryError::from)
            }
            ExprKind::BinaryOp { left, op, right } => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_binary_expr(left, *op, right).map_err(PlanQueryError::from)
            }
            ExprKind::Not(expr) => {
                let value = self.eval_expr(row, expr)?;
                eval_not_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::And(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_and_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Or(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_or_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Xor(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_xor_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Compare { left, op, right } => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_compare_expr(left, *op, right).map_err(PlanQueryError::from)
            }
            ExprKind::IsNull(expr) => Ok(Value::Bool(self.eval_expr(row, expr)? == Value::Null)),
            ExprKind::IsNotNull(expr) => Ok(Value::Bool(self.eval_expr(row, expr)? != Value::Null)),
            ExprKind::IsLabeled {
                expr,
                label,
                negated,
            } => {
                let matched = self.eval_is_labeled(row, expr, label)?;
                Ok(Value::Bool(if *negated { !matched } else { matched }))
            }
            ExprKind::IsTruth {
                expr,
                value,
                negated,
            } => {
                let evaluated = self.eval_expr(row, expr)?;
                let matched = matches!(
                    (evaluated, *value),
                    (Value::Bool(true), TruthValue::True)
                        | (Value::Bool(false), TruthValue::False)
                        | (Value::Null, TruthValue::Unknown),
                );
                Ok(Value::Bool(if *negated { !matched } else { matched }))
            }
            ExprKind::Concat(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_concat_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Coalesce(exprs) => {
                for expr in exprs {
                    let value = self.eval_expr(row, expr)?;
                    if value != Value::Null {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ExprKind::Abs(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_abs_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Floor(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_floor_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Ceil(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_ceil_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Sqrt(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_sqrt_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Exp(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_exp_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Ln(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_ln_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Log10(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_log10_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Sin(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_sin_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cos(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_cos_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Tan(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_tan_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Asin(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_asin_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Acos(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_acos_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Atan(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_atan_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Degrees(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_degrees_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Radians(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_radians_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cot(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_cot_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Sinh(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_sinh_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cosh(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_cosh_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Tanh(inner) => {
                let value = self.eval_expr(row, inner)?;
                eval_tanh_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::Cast { expr, target } => {
                let value = self.eval_expr(row, expr)?;
                eval_cast_expr(value, target).map_err(PlanQueryError::from)
            }
            ExprKind::Mod(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_mod_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Log(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_log_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Power(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_power_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::CaseSimple {
                operand,
                when_clauses,
                else_clause,
            } => {
                let operand = self.eval_expr(row, operand)?;
                for clause in when_clauses {
                    let condition = self.eval_expr(row, &clause.condition)?;
                    if operand == Value::Null || condition == Value::Null {
                        continue;
                    }
                    if eval_compare_expr(operand.clone(), CmpOp::Eq, condition).ok()
                        == Some(Value::Bool(true))
                    {
                        return self.eval_expr(row, &clause.result);
                    }
                }
                match else_clause {
                    Some(expr) => self.eval_expr(row, expr),
                    None => Ok(Value::Null),
                }
            }
            ExprKind::CaseSearched {
                when_clauses,
                else_clause,
            } => {
                for clause in when_clauses {
                    let condition = self.eval_expr(row, &clause.condition)?;
                    if searched_case_when_outcome(condition).map_err(PlanQueryError::from)?
                        == SearchedCaseWhenOutcome::Match
                    {
                        return self.eval_expr(row, &clause.result);
                    }
                }
                match else_clause {
                    Some(expr) => self.eval_expr(row, expr),
                    None => Ok(Value::Null),
                }
            }
            ExprKind::NullIf(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                if left == Value::Null || right == Value::Null {
                    return Ok(left);
                }
                let equal = eval_compare_expr(left.clone(), gleaph_gql::ast::CmpOp::Eq, right)
                    .map_err(PlanQueryError::from)?;
                if equal == Value::Bool(true) {
                    Ok(Value::Null)
                } else {
                    Ok(left)
                }
            }
            ExprKind::ListLiteral(items) | ExprKind::ListConstructor { items, .. } => items
                .iter()
                .map(|expr| self.eval_expr(row, expr))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::List),
            ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => fields
                .iter()
                .map(|(name, expr)| self.eval_expr(row, expr).map(|value| (name.clone(), value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Record),
            ExprKind::Aggregate { .. } => {
                let Some(specs) = self.aggregate_specs else {
                    return Err(PlanQueryError::UnsupportedExpression {
                        expression: "aggregate".to_owned(),
                    });
                };
                aggregate::resolve_aggregate_from_row(row, expr, specs)
            }
            ExprKind::FunctionCall {
                name,
                args,
                distinct,
            } => {
                if let Some(v) =
                    try_eval_gleaph_weight(self.gleaph_weight_decoders, name, args, *distinct, row)?
                {
                    return Ok(v);
                }
                match try_eval_runtime_function_call(self.caller, name, args, *distinct) {
                    Ok(Some(value)) => Ok(value),
                    Ok(None) => Err(PlanQueryError::UnsupportedExpression {
                        expression: format!("{:?}", expr.kind),
                    }),
                    Err(e) => Err(e.into()),
                }
            }
            _ => Err(PlanQueryError::UnsupportedExpression {
                expression: format!("{:?}", expr.kind),
            }),
        }
    }

    fn eval_is_labeled(
        &self,
        row: &PlanRow,
        expr: &Expr,
        label: &LabelExpr,
    ) -> Result<bool, PlanQueryError> {
        let ExprKind::Variable(name) = &expr.kind else {
            return Err(PlanQueryError::UnsupportedExpression {
                expression: format!(
                    "IS LABELED requires a variable expression, got {:?}",
                    expr.kind
                ),
            });
        };
        match row.get(name.as_str()) {
            Some(PlanBinding::Vertex(vertex_id)) => {
                let Some(vertex) = self.store.vertex(*vertex_id) else {
                    return Ok(false);
                };
                Ok(vertex_matches_label_expr(
                    self.store, *vertex_id, vertex, label,
                ))
            }
            Some(PlanBinding::Value(Value::Null)) => Ok(false),
            Some(PlanBinding::Value(_) | PlanBinding::Edge(_) | PlanBinding::Path(_)) => Ok(false),
            None => Err(PlanQueryError::MissingBinding {
                variable: name.clone(),
            }),
        }
    }

    fn eval_property(
        &self,
        row: &PlanRow,
        expr: &Expr,
        property: &str,
    ) -> Result<Value, PlanQueryError> {
        if let ExprKind::Variable(name) = &expr.kind {
            return match row.get(name) {
                Some(PlanBinding::Vertex(vertex_id)) => self
                    .store
                    .property_id(property)
                    .and_then(|property_id| self.store.vertex_property(*vertex_id, property_id))
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Edge(edge)) => self
                    .store
                    .property_id(property)
                    .and_then(|property_id| self.store.edge_property(edge.handle, property_id))
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Value(value)) => Ok(record_property(value, property)),
                Some(PlanBinding::Path(pb)) => {
                    Ok(record_property(&path_binding_to_value(pb), property))
                }
                None => Err(PlanQueryError::MissingBinding {
                    variable: name.clone(),
                }),
            };
        }

        let value = self.eval_expr(row, expr)?;
        Ok(record_property(&value, property))
    }

    fn eval_element_id(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        if let ExprKind::Variable(name) = &expr.kind {
            let shard_id = local_shard_id(self.store);
            return match row.get(name) {
                Some(PlanBinding::Vertex(vertex_id)) => {
                    Ok(Value::Bytes(vertex_element_id_bytes(shard_id, *vertex_id)))
                }
                Some(PlanBinding::Edge(edge)) => Ok(Value::Bytes(edge_element_id_bytes(
                    shard_id,
                    edge.handle.owner_vertex_id,
                    EdgeSlotIndex::from_raw(edge.handle.slot_index),
                ))),
                Some(PlanBinding::Value(Value::Null)) => Ok(Value::Null),
                Some(binding) => Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!("ELEMENT_ID({name}) for {binding:?}"),
                }),
                None => Err(PlanQueryError::MissingBinding {
                    variable: name.clone(),
                }),
            };
        }

        let value = self.eval_expr(row, expr)?;
        if value == Value::Null {
            Ok(Value::Null)
        } else {
            Err(PlanQueryError::InvalidExpressionValue {
                expression: format!("ELEMENT_ID({:?})", expr.kind),
            })
        }
    }
}

impl aggregate::PlanRowExprEval for QueryExprEvaluator<'_> {
    fn eval_expr_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        QueryExprEvaluator::eval_expr(self, row, expr)
    }

    fn eval_sort_key_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        eval_sort_expr(self, row, expr)
    }
}

fn project_row(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    columns: &[ProjectColumn],
) -> Result<PlanRow, PlanQueryError> {
    if columns.is_empty() {
        return row
            .iter()
            .map(|(name, binding)| {
                binding_to_value(evaluator.store, binding)
                    .map(|value| (name.clone(), PlanBinding::Value(value)))
            })
            .collect();
    }

    // Fast path: `RETURN v` / `RETURN v AS alias` — keep graph bindings so later
    // `value_row` does a single `binding_to_value` (avoids materializing a large
    // `Value::Record` in Project then cloning it again in `execute_plan_query`).
    if columns.len() == 1 {
        let column = &columns[0];
        if let ExprKind::Variable(var_name) = &column.expr.kind {
            let binding =
                row.get(var_name.as_str())
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: var_name.clone(),
                    })?;
            let name = column
                .alias
                .as_ref()
                .map(Str::to_string)
                .unwrap_or_else(|| var_name.clone());
            let mut out = PlanRow::new();
            out.insert(name, binding.clone());
            return Ok(out);
        }
    }

    columns
        .iter()
        .map(|column| {
            let name = column
                .alias
                .as_ref()
                .map(Str::to_string)
                .unwrap_or_else(|| expression_name(&column.expr));
            evaluator
                .eval_expr(row, &column.expr)
                .map(|value| (name, PlanBinding::Value(value)))
        })
        .collect()
}

fn expression_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", expression_name(expr), property)
        }
        _ => "expr".to_owned(),
    }
}

fn value_row(store: &GraphStore, row: &PlanRow) -> Result<BTreeMap<String, Value>, PlanQueryError> {
    if row.len() == 1 {
        let (name, binding) = row.iter().next().expect("len==1 guarantees one entry");
        let value = binding_to_value(store, binding)?;
        let mut out = BTreeMap::new();
        out.insert(name.clone(), value);
        return Ok(out);
    }
    row.iter()
        .map(|(name, binding)| binding_to_value(store, binding).map(|value| (name.clone(), value)))
        .collect()
}

fn binding_to_value(store: &GraphStore, binding: &PlanBinding) -> Result<Value, PlanQueryError> {
    match binding {
        PlanBinding::Vertex(vertex_id) => vertex_to_value(store, *vertex_id),
        PlanBinding::Edge(edge) => edge_to_value(store, *edge),
        PlanBinding::Value(value) => Ok(value.clone()),
        PlanBinding::Path(pb) => Ok(path_binding_to_value(pb)),
    }
}

fn vertex_to_value(store: &GraphStore, vertex_id: VertexId) -> Result<Value, PlanQueryError> {
    let vertex = store
        .vertex(vertex_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("vertex {vertex_id:?}"),
        })?;

    let labels = store.vertex_label_gql_list(vertex_id, vertex);

    let properties_value = store.vertex_properties_gql_record(vertex_id);

    Ok(Value::Record(vec![
        ("id".to_owned(), Value::Uint64(u64::from(vertex_id))),
        ("labels".to_owned(), Value::List(labels)),
        ("properties".to_owned(), properties_value),
    ]))
}

fn edge_to_value(store: &GraphStore, binding: EdgeBinding) -> Result<Value, PlanQueryError> {
    let handle = binding.handle;
    let (_edge, bucket_label) = store
        .find_outgoing_edge_with_bucket_label(handle)?
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("edge {:?}", handle),
        })?;
    let storage = LaraLabelId::from_raw(bucket_label.raw());
    let catalog_id = EdgeLabelId::from_raw(storage.label_index());
    Ok(Value::Record(vec![
        (
            "owner_vertex_id".to_owned(),
            Value::Uint64(u64::from(handle.owner_vertex_id)),
        ),
        (
            "edge_slot_index".to_owned(),
            Value::Uint64(u64::from(handle.slot_index)),
        ),
        (
            "inline_value".to_owned(),
            Value::Uint64(u64::from(binding.inline_value)),
        ),
        (
            "label".to_owned(),
            if catalog_id.raw() == 0 {
                Value::Null
            } else {
                store
                    .edge_label_name(catalog_id)
                    .map(Value::Text)
                    .unwrap_or(Value::Null)
            },
        ),
        (
            "undirected".to_owned(),
            Value::Bool(storage.is_undirected()),
        ),
        ("properties".to_owned(), {
            store.edge_properties_gql_record(handle)
        }),
    ]))
}

fn record_property(value: &Value, property: &str) -> Value {
    match value {
        Value::Record(fields) => fields
            .iter()
            .find(|(name, _)| name == property)
            .map(|(_, value)| value.clone())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn vertex_matches_label_expr(
    store: &GraphStore,
    vertex_id: VertexId,
    vertex: Vertex,
    expr: &LabelExpr,
) -> bool {
    match expr {
        LabelExpr::Name(name) => store
            .vertex_label_id(name)
            .is_some_and(|label_id| store.vertex_has_label(vertex_id, vertex, label_id)),
        LabelExpr::Wildcard => store.vertex_has_any_label(vertex_id, vertex),
        LabelExpr::And(left, right) => {
            vertex_matches_label_expr(store, vertex_id, vertex, left)
                && vertex_matches_label_expr(store, vertex_id, vertex, right)
        }
        LabelExpr::Or(left, right) => {
            vertex_matches_label_expr(store, vertex_id, vertex, left)
                || vertex_matches_label_expr(store, vertex_id, vertex, right)
        }
        LabelExpr::Not(inner) => !vertex_matches_label_expr(store, vertex_id, vertex, inner),
    }
}

fn vertex_binding(row: &PlanRow, variable: &str) -> Result<VertexId, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Vertex(vertex_id)) => Ok(*vertex_id),
        Some(_) | None => Err(PlanQueryError::MissingBinding {
            variable: variable.to_owned(),
        }),
    }
}

/// Resolve a graph traversal source when the variable may be null-padded after an optional miss.
fn vertex_binding_for_traversal(
    row: &PlanRow,
    variable: &str,
) -> Result<Option<VertexId>, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Value(Value::Null)) => Ok(None),
        _ => vertex_binding(row, variable).map(Some),
    }
}

fn limit_value(value: &Value) -> Result<usize, PlanQueryError> {
    match value {
        Value::Int8(v) if *v >= 0 => Ok(*v as usize),
        Value::Int16(v) if *v >= 0 => Ok(*v as usize),
        Value::Int32(v) if *v >= 0 => Ok(*v as usize),
        Value::Int64(v) if *v >= 0 => {
            usize::try_from(*v).map_err(|_| PlanQueryError::InvalidLimit {
                value: value.clone(),
            })
        }
        Value::Uint8(v) => Ok(*v as usize),
        Value::Uint16(v) => Ok(*v as usize),
        Value::Uint32(v) => Ok(*v as usize),
        Value::Uint64(v) => usize::try_from(*v).map_err(|_| PlanQueryError::InvalidLimit {
            value: value.clone(),
        }),
        _ => Err(PlanQueryError::InvalidLimit {
            value: value.clone(),
        }),
    }
}

fn dedup_rows(rows: &mut Vec<PlanRow>) {
    let mut unique = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !unique.contains(&row) {
            unique.push(row);
        }
    }
    *rows = unique;
}

fn plan_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::NodeScan { .. } => "NodeScan",
        PlanOp::IndexScan { .. } => "IndexScan",
        PlanOp::EdgeIndexScan { .. } => "EdgeIndexScan",
        PlanOp::EdgeBindEndpoints { .. } => "EdgeBindEndpoints",
        PlanOp::ConditionalIndexScan { .. } => "ConditionalIndexScan",
        PlanOp::PropertyFilter { .. } => "PropertyFilter",
        PlanOp::Expand { .. } => "Expand",
        PlanOp::ExpandFilter { .. } => "ExpandFilter",
        PlanOp::ShortestPath { .. } => "ShortestPath",
        PlanOp::Let { .. } => "Let",
        PlanOp::For { .. } => "For",
        PlanOp::Filter { .. } => "Filter",
        PlanOp::CallProcedure { .. } => "CallProcedure",
        PlanOp::InlineProcedureCall { .. } => "InlineProcedureCall",
        PlanOp::UseGraph { .. } => "UseGraph",
        PlanOp::HashJoin { .. } => "HashJoin",
        PlanOp::CartesianProduct { .. } => "CartesianProduct",
        PlanOp::Aggregate { .. } => "Aggregate",
        PlanOp::Project { .. } => "Project",
        PlanOp::Sort { .. } => "Sort",
        PlanOp::Limit { .. } => "Limit",
        PlanOp::SetOperation { .. } => "SetOperation",
        PlanOp::OptionalMatch { .. } => "OptionalMatch",
        PlanOp::IndexIntersection { .. } => "IndexIntersection",
        PlanOp::WorstCaseOptimalJoin { .. } => "WorstCaseOptimalJoin",
        PlanOp::TopK { .. } => "TopK",
        PlanOp::Materialize { .. } => "Materialize",
        PlanOp::InsertVertex { .. } => "InsertVertex",
        PlanOp::InsertEdge { .. } => "InsertEdge",
        PlanOp::SetProperties { .. } => "SetProperties",
        PlanOp::RemoveProperties { .. } => "RemoveProperties",
        PlanOp::DeleteVertex { .. } => "DeleteVertex",
        PlanOp::DetachDeleteVertex { .. } => "DetachDeleteVertex",
        PlanOp::DeleteEdge { .. } => "DeleteEdge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::IndexRouting;
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use crate::gql_execution_context::GqlExecutionContext;
    use crate::index::lookup::PropertyIndexLookup;
    use crate::plan::query::GLEAPH_PATH_EXTENSION_HANDLER;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_gql::ast::{
        AggregateFunc, BinaryOp, CmpOp, Expr, ExprKind, NullOrder, ObjectName, OrderByClause,
        SetOp, SortDirection, SortItem, Statement,
    };
    use gleaph_gql::parser;
    use gleaph_gql::token::Span;
    use gleaph_gql::type_check::NoSchema;
    use gleaph_gql::types::PathElement;
    use gleaph_gql::value::{ExtensionSortableKey, ExtensionValue};
    use gleaph_gql_planner::plan::{
        AggregateSpec, ConditionalScanCandidate, PlanAnnotations, PlanDiagnostics, ScanValue,
        ShortestMode, ShortestPathCost, Str, WcojEdge,
    };
    use gleaph_gql_planner::{PlanBuildOptions, build_plan_with_schema_and_options};
    use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
    use std::any::Any;
    use std::borrow::Cow;
    use std::cell::RefCell;
    use std::cmp::Ordering;
    use std::fmt;
    use std::rc::Rc;

    #[derive(Default)]
    struct MockPropertyIndex {
        equal_hits: RefCell<Vec<PostingHit>>,
        range_hits: RefCell<Vec<PostingHit>>,
        equal_calls: RefCell<Vec<(u32, Vec<u8>)>>,
        range_calls: RefCell<Vec<(u32, PostingRangeRequest)>>,
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for MockPropertyIndex {
        async fn lookup_equal(
            &self,
            property_id: u32,
            value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            self.equal_calls.borrow_mut().push((property_id, value));
            Ok(self.equal_hits.borrow().clone())
        }

        async fn lookup_range(
            &self,
            property_id: u32,
            req: &PostingRangeRequest,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            self.range_calls
                .borrow_mut()
                .push((property_id, req.clone()));
            Ok(self.range_hits.borrow().clone())
        }

        async fn posting_insert(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }

        async fn posting_remove(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }
    }

    fn plan(ops: Vec<PlanOp>) -> PhysicalPlan {
        PhysicalPlan {
            ops,
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        }
    }

    fn plan_gql(input: &str) -> PhysicalPlan {
        let program = parser::parse(input).unwrap_or_else(|err| panic!("parse error: {err}"));
        let tx = program
            .transaction_activity
            .expect("expected transaction activity");
        let block = tx.body.expect("expected statement block");
        let Statement::Query(composite) = &block.first else {
            panic!("expected query statement");
        };
        build_plan_with_schema_and_options(
            &composite.left,
            PlanBuildOptions {
                stats: None,
                path_extensions: &GLEAPH_PATH_EXTENSION_HANDLER,
            },
            &NoSchema,
        )
        .expect("plan should build")
    }

    fn prop(variable: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable(variable.to_owned()))),
            property: property.to_owned(),
        })
    }

    fn var(variable: &str) -> Expr {
        Expr::new(ExprKind::Variable(variable.to_owned()))
    }

    fn order_by(items: Vec<SortItem>) -> OrderByClause {
        OrderByClause {
            span: Span::DUMMY,
            items,
        }
    }

    fn sort_item(
        expr: Expr,
        direction: Option<SortDirection>,
        null_order: Option<NullOrder>,
    ) -> SortItem {
        SortItem {
            span: Span::DUMMY,
            expr,
            direction,
            null_order,
        }
    }

    fn project(expr: Expr, alias: &str) -> ProjectColumn {
        ProjectColumn {
            expr,
            alias: Some(alias.into()),
        }
    }

    fn params() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    fn reset_node_scan_visits() {
        NODE_SCAN_VISITS.with(|visits| visits.set(0));
    }

    fn node_scan_visits() -> usize {
        NODE_SCAN_VISITS.with(|visits| visits.get())
    }

    #[derive(Clone, Debug)]
    struct TestOrderableExt(u8);

    impl fmt::Display for TestOrderableExt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "TestOrderableExt({})", self.0)
        }
    }

    impl ExtensionValue for TestOrderableExt {
        fn type_name(&self) -> &str {
            "TestOrderableExt"
        }

        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }

        fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .is_some_and(|o| self.0 == o.0)
        }

        fn cmp_ext(&self, other: &dyn ExtensionValue) -> Option<Ordering> {
            other
                .as_any()
                .downcast_ref::<Self>()
                .map(|o| self.0.cmp(&o.0))
        }

        fn sortable_index_key(&self) -> Option<ExtensionSortableKey<'_>> {
            Some(ExtensionSortableKey {
                domain: Cow::Borrowed("test.orderable/v1"),
                bytes: Cow::Owned(vec![self.0]),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
            Some(Cow::Owned(vec![self.0]))
        }
    }

    #[derive(Clone, Debug)]
    struct TestNonOrderableExt;

    impl fmt::Display for TestNonOrderableExt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "TestNonOrderableExt")
        }
    }

    impl ExtensionValue for TestNonOrderableExt {
        fn type_name(&self) -> &str {
            "TestNonOrderableExt"
        }

        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }

        fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
            other.as_any().downcast_ref::<Self>().is_some()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
            Some(Cow::Borrowed(&[0]))
        }
    }

    fn orderable_ext(value: u8) -> Value {
        Value::Extension(Box::new(TestOrderableExt(value)))
    }

    fn non_orderable_ext() -> Value {
        Value::Extension(Box::new(TestNonOrderableExt))
    }

    /// Minimal [`AggregateSpec`] for tests (no `expr2` / `filter` / `order_by`).
    fn agg_spec(
        func: AggregateFunc,
        expr: Option<Expr>,
        distinct: bool,
        alias: Option<&str>,
    ) -> AggregateSpec {
        AggregateSpec {
            func,
            expr,
            expr2: None,
            distinct,
            filter: None,
            order_by: None,
            alias: alias.map(|a| a.into()),
        }
    }

    fn text_column(result: &PlanQueryResult, column: &str) -> Vec<String> {
        result
            .rows
            .iter()
            .map(|row| match row.get(column) {
                Some(Value::Text(value)) => value.clone(),
                other => panic!("expected text column {column}, got {other:?}"),
            })
            .collect()
    }

    fn bytes_column<'a>(result: &'a PlanQueryResult, column: &str) -> &'a [u8] {
        match result.rows.first().and_then(|row| row.get(column)) {
            Some(Value::Bytes(value)) => value,
            other => panic!("expected bytes column {column}, got {other:?}"),
        }
    }

    fn configure_test_index(store: &GraphStore) {
        store
            .set_index_routing(Some(IndexRouting {
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("set index routing");
    }

    fn path_column<'a>(result: &'a PlanQueryResult, column: &str) -> &'a [PathElement] {
        match result.rows.first().and_then(|row| row.get(column)) {
            Some(Value::Path(elements)) => elements,
            other => panic!("expected path column {column}, got {other:?}"),
        }
    }

    fn vertex_path_id(element: &PathElement) -> GraphPathVertexId {
        match element {
            PathElement::Vertex(id) => {
                GraphPathVertexId::try_from_slice(id.as_ref()).expect("vertex path id")
            }
            other => panic!("expected vertex path element, got {other:?}"),
        }
    }

    fn edge_path_id(element: &PathElement) -> GraphPathEdgeId {
        match element {
            PathElement::Edge(id) => GraphPathEdgeId::try_from_slice(id.as_ref()).expect("edge id"),
            other => panic!("expected edge path element, got {other:?}"),
        }
    }

    #[test]
    fn executes_equality_index_scan_with_sortable_key() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let vid = store
            .insert_vertex_named(["IndexScanEq"], [("age", Value::Uint8(5))])
            .expect("insert vertex");
        let pid = store.property_id("age").expect("age property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "age".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&Value::Uint8(5)).unwrap().unwrap()
        );
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn equality_index_scan_unifies_decimal_and_integer_key_with_final_filter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let price = gleaph_gql::types::Decimal::parse("5.00").expect("decimal");
        let vid = store
            .insert_vertex_named(["IndexScanDecimalEq"], [("price", Value::Decimal(price))])
            .expect("insert vertex");
        let pid = store.property_id("price").expect("price property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "price".into(),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "price")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(5)))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute decimal equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&Value::Decimal(price))
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn equality_index_scan_unifies_float_and_decimal_key_with_final_filter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let bound = gleaph_gql::types::Decimal::parse("1.5").expect("decimal");
        let vid = store
            .insert_vertex_named(["IndexScanFloatEq"], [("score", Value::Float64(1.5))])
            .expect("insert vertex");
        let pid = store.property_id("score").expect("score property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "score".into(),
                value: ScanValue::Literal(Value::Decimal(bound)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "score")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(bound)))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute float equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&Value::Float64(1.5))
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn equality_index_scan_final_filter_drops_inexact_float_decimal_candidate() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let bound = gleaph_gql::types::Decimal::parse("0.1").expect("decimal");
        let vid = store
            .insert_vertex_named(["IndexScanFloatInexact"], [("score", Value::Float64(0.1))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "score".into(),
                value: ScanValue::Literal(Value::Decimal(bound)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "score")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(bound)))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute inexact float equality index scan");

        assert!(result.rows.is_empty());
    }

    #[test]
    fn equality_index_scan_matches_list_valued_posting() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let stored = Value::List(vec![Value::Uint8(1), Value::Text("a".into())]);
        let bound = Value::List(vec![Value::Int64(1), Value::Text("a".into())]);
        let vid = store
            .insert_vertex_named(["IndexScanListEq"], [("tags", stored.clone())])
            .expect("insert vertex");
        let pid = store.property_id("tags").expect("tags property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "tags".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "tags")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(bound))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute list equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&stored).unwrap().unwrap()
        );
    }

    #[test]
    fn equality_index_scan_matches_record_valued_posting_independent_of_field_order() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let stored = Value::Record(vec![
            ("b".into(), Value::Int64(2)),
            ("a".into(), Value::Int64(1)),
        ]);
        let bound = Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Int64(2)),
        ]);
        let vid = store
            .insert_vertex_named(["IndexScanRecordEq"], [("profile", stored.clone())])
            .expect("insert vertex");
        let pid = store
            .property_id("profile")
            .expect("profile property")
            .raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "profile".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "profile")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(bound))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute record equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&stored).unwrap().unwrap()
        );
    }

    #[test]
    fn equality_index_scan_final_filter_drops_inexact_nested_numeric_candidate() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let stored = Value::Record(vec![("score".into(), Value::Float64(0.1))]);
        let bound = Value::Record(vec![(
            "score".into(),
            Value::Decimal(gleaph_gql::types::Decimal::parse("0.1").expect("decimal")),
        )]);
        let vid = store
            .insert_vertex_named(["IndexScanRecordInexact"], [("profile", stored)])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "profile".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "profile")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(bound))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute record inexact equality index scan");

        assert!(result.rows.is_empty());
    }

    #[test]
    fn executes_range_index_scan_with_lookup_range() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let low = store
            .insert_vertex_named(["IndexScanRange"], [("age", Value::Int64(1))])
            .expect("insert low");
        let high = store
            .insert_vertex_named(["IndexScanRange"], [("age", Value::Int64(9))])
            .expect("insert high");
        let pid = store.property_id("age").expect("age property").raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().extend([
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(low)).unwrap(),
            },
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(high)).unwrap(),
            },
        ]);
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "age".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute range index scan");

        assert_eq!(result.rows.len(), 2);
        assert!(index.equal_calls.borrow().is_empty());
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Ge(bytes)
                if bytes == &value_to_index_key_bytes(&Value::Int64(5)).unwrap().unwrap()
        ));
    }

    #[test]
    fn executes_list_range_index_scan_with_lookup_range() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let hit = store
            .insert_vertex_named(
                ["IndexScanListRange"],
                [("tags", Value::List(vec![Value::Int64(2)]))],
            )
            .expect("insert hit");
        let miss = store
            .insert_vertex_named(
                ["IndexScanListRange"],
                [("tags", Value::List(vec![Value::Int64(0)]))],
            )
            .expect("insert miss");
        let pid = store.property_id("tags").expect("tags property").raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().extend([
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(hit)).unwrap(),
            },
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(miss)).unwrap(),
            },
        ]);
        let bound = Value::List(vec![Value::Int64(1)]);
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "tags".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Ge,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "tags")),
                    op: CmpOp::Ge,
                    right: Box::new(Expr::new(ExprKind::Literal(bound.clone()))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute list range index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Ge(bytes)
                if bytes == &value_to_index_key_bytes(&bound).unwrap().unwrap()
        ));
        assert!(index.equal_calls.borrow().is_empty());
    }

    #[test]
    fn executes_record_range_index_scan_with_lookup_range() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let hit = store
            .insert_vertex_named(
                ["IndexScanRecordRange"],
                [(
                    "profile",
                    Value::Record(vec![
                        ("a".into(), Value::Int64(1)),
                        ("b".into(), Value::Int64(1)),
                    ]),
                )],
            )
            .expect("insert hit");
        let pid = store
            .property_id("profile")
            .expect("profile property")
            .raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(hit)).unwrap(),
        });
        let bound = Value::Record(vec![
            ("b".into(), Value::Int64(2)),
            ("a".into(), Value::Int64(1)),
        ]);
        let canonical_bound = Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Int64(2)),
        ]);
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "profile".into(),
            value: ScanValue::Literal(bound),
            cmp: CmpOp::Lt,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute record range index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Lt(bytes)
                if bytes == &value_to_index_key_bytes(&canonical_bound).unwrap().unwrap()
        ));
        assert!(index.equal_calls.borrow().is_empty());
    }

    #[test]
    fn executes_orderable_extension_equality_index_scan() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let value = orderable_ext(7);
        store
            .insert_vertex_named(
                ["IndexScanExtensionEqCatalog"],
                [("principal", Value::Text("catalog".into()))],
            )
            .expect("insert catalog vertex");
        let vid = store
            .insert_vertex_named(["IndexScanExtensionEq"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let pid = store.property_id("principal").expect("property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "principal".into(),
            value: ScanValue::Literal(value.clone()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute extension equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&value).unwrap().unwrap()
        );
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn executes_orderable_extension_range_index_scan() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let bound = orderable_ext(7);
        store
            .insert_vertex_named(
                ["IndexScanExtensionRangeCatalog"],
                [("principal", Value::Text("catalog".into()))],
            )
            .expect("insert catalog vertex");
        let vid = store
            .insert_vertex_named(["IndexScanExtensionRange"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let pid = store.property_id("principal").expect("property").raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "principal".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute extension range index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Ge(bytes)
                if bytes == &value_to_index_key_bytes(&bound).unwrap().unwrap()
        ));
        assert!(index.equal_calls.borrow().is_empty());
    }

    #[test]
    fn index_scan_rejects_unsupported_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(["IndexScanBadParam"], [("tags", Value::List(vec![]))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Parameter("tags".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("unsupported parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn index_scan_rejects_non_orderable_extension_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanBadExtensionParam"],
                [("principal", Value::Text("catalog".into()))],
            )
            .expect("insert catalog vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("principal".into(), non_orderable_ext());
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "principal".into(),
            value: ScanValue::Parameter("principal".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("non-orderable extension parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn range_index_scan_rejects_unsupported_nested_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(["IndexScanBadRangeParam"], [("tags", Value::List(vec![]))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Parameter("tags".into()),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("unsupported range parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn index_scan_rejects_non_finite_float_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(["IndexScanBadFloatParam"], [("score", Value::Float64(1.0))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("score".into(), Value::Float64(f64::INFINITY));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "score".into(),
            value: ScanValue::Parameter("score".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("non-finite parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn conditional_index_scan_falls_back_for_null_or_unsupported_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalFallback"],
                [("tags", Value::List(vec![]))],
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "tags".into(),
                property: "tags".into(),
                variable: "n".into(),
                cmp: CmpOp::Eq,
            }],
            fallback_label: Some("IndexScanConditionalFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn conditional_index_scan_falls_back_for_non_orderable_extension_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalExtensionFallback"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("principal".into(), non_orderable_ext());
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "principal".into(),
                property: "principal".into(),
                variable: "n".into(),
                cmp: CmpOp::Eq,
            }],
            fallback_label: Some("IndexScanConditionalExtensionFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn conditional_range_index_scan_falls_back_for_unsupported_nested_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalRangeFallback"],
                [("tags", Value::List(vec![]))],
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "tags".into(),
                property: "tags".into(),
                variable: "n".into(),
                cmp: CmpOp::Ge,
            }],
            fallback_label: Some("IndexScanConditionalRangeFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn conditional_index_scan_falls_back_for_non_finite_float_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalFloatFallback"],
                [("score", Value::Float64(1.0))],
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("score".into(), Value::Float64(f64::NAN));
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "score".into(),
                property: "score".into(),
                variable: "n".into(),
                cmp: CmpOp::Eq,
            }],
            fallback_label: Some("IndexScanConditionalFloatFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn executes_planner_match_return_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryPersonReturn"],
                [("name", Value::Text("Planner Alice".into()))],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryOtherReturn"],
                [("name", Value::Text("Planner Bob".into()))],
            )
            .expect("insert non-matching vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryPersonReturn) RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Alice".into()))
        );
    }

    #[test]
    fn executes_planner_property_filter() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryPersonFilter"],
                [
                    ("name", Value::Text("Planner Filter Ada".into())),
                    ("age", Value::Int64(37)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryPersonFilter"],
                [
                    ("name", Value::Text("Planner Filter Bob".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan =
            plan_gql("MATCH (n:PlannerQueryPersonFilter) WHERE n.age > 18 RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Filter Ada".into()))
        );
    }

    #[test]
    fn executes_planner_let_binding() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerQueryLetAge"], [("age", Value::Int64(36))])
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryLetAge) LET x = n.age + 1 RETURN x");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("x"), Some(&Value::Int64(37)));
    }

    #[test]
    fn executes_planner_let_binding_dependency_order() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerQueryLetChain"], [("k", Value::Int64(10))])
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryLetChain) LET x = n.k + 1, y = x * 2 RETURN y");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("y"), Some(&Value::Int64(22)));
    }

    #[test]
    fn executes_planner_standalone_filter() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryStandaloneFilter"],
                [
                    ("name", Value::Text("Active Ada".into())),
                    ("active", Value::Bool(true)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryStandaloneFilter"],
                [
                    ("name", Value::Text("Inactive Bob".into())),
                    ("active", Value::Bool(false)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan = plan_gql(
            "MATCH (n:PlannerQueryStandaloneFilter) FILTER n.active RETURN n.name AS name",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Active Ada".into()))
        );
    }

    #[test]
    fn executes_planner_one_hop_expand() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["PlannerQueryExpandSource"],
                [("name", Value::Text("Planner Expand Alice".into()))],
            )
            .expect("insert source");
        let b = store
            .insert_vertex_named(
                ["PlannerQueryExpandTarget"],
                [("name", Value::Text("Planner Expand Bob".into()))],
            )
            .expect("insert target");
        let unrelated = store
            .insert_vertex_named(
                ["PlannerQueryExpandTarget"],
                [("name", Value::Text("Planner Expand Carol".into()))],
            )
            .expect("insert unrelated target");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("PlannerQueryKnows"),
                [("since", Value::Int64(2026))],
            )
            .expect("insert matching edge");
        store
            .insert_directed_edge_named(
                a,
                unrelated,
                Some("PlannerQueryIgnores"),
                [("since", Value::Int64(2025))],
            )
            .expect("insert non-matching edge");
        let plan = plan_gql(
            "MATCH (a:PlannerQueryExpandSource)-[e:PlannerQueryKnows]->(b:PlannerQueryExpandTarget) \
             RETURN a.name AS a_name, b.name AS b_name, e.since AS since",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("a_name"),
            Some(&Value::Text("Planner Expand Alice".into()))
        );
        assert_eq!(
            result.rows[0].get("b_name"),
            Some(&Value::Text("Planner Expand Bob".into()))
        );
        assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2026)));
    }

    #[test]
    fn executes_planner_order_by() {
        let store = GraphStore::new();
        for name in ["Planner Sort C", "Planner Sort A", "Planner Sort B"] {
            store
                .insert_vertex_named(["PlannerQuerySort"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQuerySort) RETURN n.name ORDER BY n.name");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "n.name"),
            vec!["Planner Sort A", "Planner Sort B", "Planner Sort C"]
        );
    }

    #[test]
    fn executes_planner_order_by_limit_topk() {
        let store = GraphStore::new();
        for name in [
            "Planner TopK D",
            "Planner TopK A",
            "Planner TopK C",
            "Planner TopK B",
        ] {
            store
                .insert_vertex_named(["PlannerQueryTopK"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryTopK) RETURN n.name ORDER BY n.name LIMIT 2");
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::TopK { .. })));

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "n.name"),
            vec!["Planner TopK A", "Planner TopK B"]
        );
    }

    #[test]
    fn executes_planner_order_by_record_value() {
        let store = GraphStore::new();
        for (name, rank) in [("Planner Record B", 2), ("Planner Record A", 1)] {
            store
                .insert_vertex_named(
                    ["PlannerQueryRecordSort"],
                    [
                        ("name", Value::Text(name.into())),
                        ("rank", Value::Int64(rank)),
                    ],
                )
                .expect("insert vertex");
        }
        let plan = plan_gql(
            "MATCH (n:PlannerQueryRecordSort) RETURN n.name AS name, {rank: n.rank} AS sort_key ORDER BY sort_key",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "name"),
            vec!["Planner Record A", "Planner Record B"]
        );
    }

    #[test]
    fn executes_planner_record_equality_independent_of_field_order() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryRecordEq"],
                [("a", Value::Int64(1)), ("b", Value::Int64(2))],
            )
            .expect("insert vertex");
        let plan = plan_gql(
            "MATCH (n:PlannerQueryRecordEq) \
             RETURN {b: n.b, a: n.a} = {a: n.a, b: n.b} AS same",
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("same"), Some(&Value::Bool(true)));
    }

    #[test]
    fn executes_planner_return_star() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryReturnStar"],
                [("name", Value::Text("Planner Star".into()))],
            )
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryReturnStar) RETURN *");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0].get("n"), Some(Value::Record(_))));
    }

    #[test]
    fn executes_planner_limit() {
        let store = GraphStore::new();
        for name in ["Planner Limit A", "Planner Limit B"] {
            store
                .insert_vertex_named(["PlannerQueryLimit"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryLimit) RETURN n.name LIMIT 1");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn planner_limit_stops_node_scan_after_enough_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryLazyLimit"],
                [("name", Value::Text("first".into()))],
            )
            .expect("insert first");
        for i in 0..64 {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyLimit"],
                    [("name", Value::Text(format!("tail {i}")))],
                )
                .expect("insert tail");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryLazyLimit) RETURN n.name LIMIT 1");

        reset_node_scan_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(text_column(&result, "n.name"), vec!["first"]);
        assert_eq!(node_scan_visits(), 1);
    }

    #[test]
    fn planner_limit_stops_after_filter_accepts_enough_rows() {
        let store = GraphStore::new();
        for i in 0..10 {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyFilterLimit"],
                    [
                        ("name", Value::Text(format!("drop {i}"))),
                        ("keep", Value::Bool(false)),
                    ],
                )
                .expect("insert dropped");
        }
        for name in ["keep a", "keep b"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyFilterLimit"],
                    [
                        ("name", Value::Text(name.into())),
                        ("keep", Value::Bool(true)),
                    ],
                )
                .expect("insert kept");
        }
        for i in 0..32 {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyFilterLimit"],
                    [
                        ("name", Value::Text(format!("unvisited {i}"))),
                        ("keep", Value::Bool(true)),
                    ],
                )
                .expect("insert tail");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("PlannerQueryLazyFilterLimit".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "keep")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
                })],
                stage: 0,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                offset: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "n.name")],
                distinct: false,
            },
        ]);

        reset_node_scan_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(text_column(&result, "n.name"), vec!["keep a", "keep b"]);
        assert_eq!(node_scan_visits(), 12);
    }

    #[test]
    fn order_by_limit_remains_a_materializing_barrier() {
        let store = GraphStore::new();
        for name in ["c", "a", "b"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyLimitSort"],
                    [("name", Value::Text(name.into()))],
                )
                .expect("insert vertex");
        }
        let plan =
            plan_gql("MATCH (n:PlannerQueryLazyLimitSort) RETURN n.name ORDER BY n.name LIMIT 1");

        reset_node_scan_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(text_column(&result, "n.name"), vec!["a"]);
        assert_eq!(node_scan_visits(), 3);
    }

    #[test]
    fn labeled_expand_limit_offset_pages_latest_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyEdgePageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["LazyEdgePageTarget"],
                    [("name", Value::Text(format!("edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("LazyEdgePageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let first_page = plan_gql(
            "MATCH (a:LazyEdgePageSource)-[:LazyEdgePageRel]->(b) RETURN b.name LIMIT 2 OFFSET 0",
        );
        let second_page = plan_gql(
            "MATCH (a:LazyEdgePageSource)-[:LazyEdgePageRel]->(b) RETURN b.name LIMIT 2 OFFSET 2",
        );

        let first = store
            .execute_plan_query(&first_page, &params(), GqlExecutionContext::default())
            .expect("execute first page");
        let second = store
            .execute_plan_query(&second_page, &params(), GqlExecutionContext::default())
            .expect("execute second page");

        assert_eq!(text_column(&first, "b.name"), vec!["edge 4", "edge 3"]);
        assert_eq!(text_column(&second, "b.name"), vec!["edge 2", "edge 1"]);
    }

    #[test]
    fn gleaph_sequence_asc_pages_labeled_edges_in_insertion_order() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["SeqAscPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["SeqAscPageTarget"],
                    [("name", Value::Text(format!("seq edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("SeqAscPageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (a:SeqAscPageSource)-[e:SeqAscPageRel]->(b) \
             ORDER BY GLEAPH.SEQUENCE(e) ASC LIMIT 2 OFFSET 1 RETURN b.name",
        );

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute asc page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["seq edge 1", "seq edge 2"]
        );
    }

    #[test]
    fn gleaph_sequence_desc_matches_default_labeled_edge_order() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["SeqDescPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..4 {
            let dst = store
                .insert_vertex_named(
                    ["SeqDescPageTarget"],
                    [("name", Value::Text(format!("seq desc edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("SeqDescPageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (a:SeqDescPageSource)-[e:SeqDescPageRel]->(b) \
             ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 2 RETURN b.name",
        );

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute desc page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["seq desc edge 3", "seq desc edge 2"]
        );
    }

    #[test]
    fn gleaph_sequence_rejects_unlabeled_edge_pattern() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["SeqNoLabelSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        let dst = store
            .insert_vertex_named(["SeqNoLabelTarget"], Vec::<(&str, Value)>::new())
            .expect("insert target");
        store
            .insert_directed_edge_named(src, dst, Option::<&str>::None, Vec::<(&str, Value)>::new())
            .expect("insert edge");

        let page = plan_gql(
            "MATCH (a:SeqNoLabelSource)-[e]->(b) \
             ORDER BY GLEAPH.SEQUENCE(e) ASC RETURN b",
        );

        let err = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect_err("unlabeled sequence order should fail");

        assert!(err.to_string().contains("single fixed edge label"), "{err}");
    }

    #[test]
    fn unlabeled_directed_expand_limit_offset_uses_latest_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyUnlabeledPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["LazyUnlabeledPageTarget"],
                    [("name", Value::Text(format!("unlabeled edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Option::<&str>::None,
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page =
            plan_gql("MATCH (a:LazyUnlabeledPageSource)-[]->(b) RETURN b.name LIMIT 2 OFFSET 2");

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["unlabeled edge 2", "unlabeled edge 1"]
        );
    }

    #[test]
    fn reverse_expand_limit_offset_uses_latest_in_edges() {
        let store = GraphStore::new();
        let dst = store
            .insert_vertex_named(["LazyReversePageTarget"], Vec::<(&str, Value)>::new())
            .expect("insert target");
        for i in 0..5 {
            let src = store
                .insert_vertex_named(
                    ["LazyReversePageSource"],
                    [("name", Value::Text(format!("reverse edge {i}")))],
                )
                .expect("insert source");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("LazyReversePageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (b:LazyReversePageTarget)<-[:LazyReversePageRel]-(a) RETURN a.name LIMIT 2 OFFSET 2",
        );

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "a.name"),
            vec!["reverse edge 2", "reverse edge 1"]
        );
    }

    #[test]
    fn undirected_expand_limit_offset_uses_latest_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyUndirectedPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["LazyUndirectedPageTarget"],
                    [("name", Value::Text(format!("undirected edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_undirected_edge_named(
                    src,
                    dst,
                    Option::<&str>::None,
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page =
            plan_gql("MATCH (a:LazyUndirectedPageSource)~[]~(b) RETURN b.name LIMIT 2 OFFSET 2");

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["undirected edge 2", "undirected edge 1"]
        );
    }

    #[test]
    fn filtered_expand_limit_offset_skips_only_matching_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyFilteredPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for (i, keep) in [
            (0, true),
            (1, false),
            (2, true),
            (3, false),
            (4, true),
            (5, true),
        ] {
            let dst = store
                .insert_vertex_named(
                    ["LazyFilteredPageTarget"],
                    [
                        ("name", Value::Text(format!("filtered edge {i}"))),
                        ("keep", Value::Bool(keep)),
                    ],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("LazyFilteredPageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (a:LazyFilteredPageSource)-[:LazyFilteredPageRel]->(b) \
             WHERE b.keep = true RETURN b.name LIMIT 2 OFFSET 1",
        );

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["filtered edge 4", "filtered edge 2"]
        );
    }

    #[test]
    fn executes_planner_expand_filter() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["PlannerQueryExpandFilterSource"],
                [("name", Value::Text("Planner EF A".into()))],
            )
            .expect("insert source");
        let keep = store
            .insert_vertex_named(
                ["PlannerQueryExpandFilterTarget"],
                [
                    ("name", Value::Text("Planner EF Keep".into())),
                    ("age", Value::Int64(30)),
                ],
            )
            .expect("insert keep target");
        let drop = store
            .insert_vertex_named(
                ["PlannerQueryExpandFilterTarget"],
                [
                    ("name", Value::Text("Planner EF Drop".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert drop target");
        store
            .insert_directed_edge_named(
                a,
                keep,
                Some("PlannerQueryExpandFilterRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert keep edge");
        store
            .insert_directed_edge_named(
                a,
                drop,
                Some("PlannerQueryExpandFilterRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert drop edge");
        let plan = plan_gql(
            "MATCH (a:PlannerQueryExpandFilterSource)-[e:PlannerQueryExpandFilterRel]->\
             (b:PlannerQueryExpandFilterTarget) WHERE b.age > 18 \
             RETURN a.name AS a_name, b.name AS b_name",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::ExpandFilter { .. }))
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("b_name"),
            Some(&Value::Text("Planner EF Keep".into()))
        );
    }

    #[test]
    fn executes_planner_use_graph_as_single_store_pass_through() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryUseGraph"],
                [("name", Value::Text("Planner UseGraph".into()))],
            )
            .expect("insert vertex");
        let plan = plan_gql("USE myGraph MATCH (n:PlannerQueryUseGraph) RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner UseGraph".into()))
        );
    }

    #[test]
    fn executes_planner_cartesian_product_for_independent_matches() {
        let store = GraphStore::new();
        for name in ["Planner CP Alice", "Planner CP Bob"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryCartesianPerson"],
                    [("name", Value::Text(name.into()))],
                )
                .expect("insert person");
        }
        for city in ["Planner CP Tokyo", "Planner CP Paris"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryCartesianCity"],
                    [("name", Value::Text(city.into()))],
                )
                .expect("insert city");
        }
        let plan = plan_gql(
            "MATCH (a:PlannerQueryCartesianPerson) MATCH (b:PlannerQueryCartesianCity) \
             RETURN a.name AS person, b.name AS city",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::CartesianProduct { .. }))
        );

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 4);
        assert!(result.rows.iter().any(|row| {
            row.get("person") == Some(&Value::Text("Planner CP Alice".into()))
                && row.get("city") == Some(&Value::Text("Planner CP Tokyo".into()))
        }));
        assert!(result.rows.iter().any(|row| {
            row.get("person") == Some(&Value::Text("Planner CP Bob".into()))
                && row.get("city") == Some(&Value::Text("Planner CP Paris".into()))
        }));
    }

    #[test]
    fn node_scan_projects_vertex_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryPersonNodeScan"],
                [("name", Value::Text("Node Alice".into()))],
            )
            .expect("insert vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryPersonNodeScan".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Node Alice".into()))
        );
    }

    #[test]
    fn property_filter_keeps_matching_vertices() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryPersonFilter"],
                [
                    ("name", Value::Text("Filter Ada".into())),
                    ("age", Value::Int64(37)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["QueryPersonFilter"],
                [
                    ("name", Value::Text("Filter Bob".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryPersonFilter".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "age")),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Filter Ada".into()))
        );
    }

    #[test]
    fn sort_orders_projected_scalars_ascending_and_descending() {
        let store = GraphStore::new();
        for name in ["Sort Scalar C", "Sort Scalar A", "Sort Scalar B"] {
            store
                .insert_vertex_named(["QuerySortScalar"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let scan_project = || {
            vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("QuerySortScalar".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(prop("n", "name"), "name")],
                    distinct: false,
                },
            ]
        };
        let asc = plan(
            scan_project()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, None)]),
                }])
                .collect(),
        );
        let desc = plan(
            scan_project()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(
                        var("name"),
                        Some(SortDirection::Desc),
                        None,
                    )]),
                }])
                .collect(),
        );

        let asc_result = store
            .execute_plan_query(&asc, &params(), GqlExecutionContext::default())
            .expect("execute ascending sort");
        let desc_result = store
            .execute_plan_query(&desc, &params(), GqlExecutionContext::default())
            .expect("execute descending sort");

        assert_eq!(
            text_column(&asc_result, "name"),
            vec!["Sort Scalar A", "Sort Scalar B", "Sort Scalar C"]
        );
        assert_eq!(
            text_column(&desc_result, "name"),
            vec!["Sort Scalar C", "Sort Scalar B", "Sort Scalar A"]
        );
    }

    #[test]
    fn sort_orders_multiple_keys() {
        let store = GraphStore::new();
        for (group, name) in [
            (Value::Int64(2), "Multi B"),
            (Value::Int64(1), "Multi B"),
            (Value::Int64(1), "Multi A"),
            (Value::Int64(2), "Multi A"),
        ] {
            store
                .insert_vertex_named(
                    ["QuerySortMulti"],
                    [("group", group), ("name", Value::Text(name.into()))],
                )
                .expect("insert vertex");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QuerySortMulti".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "group"), "group"),
                    project(prop("n", "name"), "name"),
                ],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: order_by(vec![
                    sort_item(var("group"), None, None),
                    sort_item(var("name"), Some(SortDirection::Desc), None),
                ]),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute multi-key sort");

        assert_eq!(
            text_column(&result, "name"),
            vec!["Multi B", "Multi A", "Multi B", "Multi A"]
        );
    }

    #[test]
    fn sort_honors_explicit_null_ordering() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["QuerySortNulls"], Vec::<(&str, Value)>::new())
            .expect("insert null vertex");
        for name in ["Null Ada", "Null Bob"] {
            store
                .insert_vertex_named(["QuerySortNulls"], [("name", Value::Text(name.into()))])
                .expect("insert named vertex");
        }
        let base_ops = || {
            vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("QuerySortNulls".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(prop("n", "name"), "name")],
                    distinct: false,
                },
            ]
        };
        let nulls_first = plan(
            base_ops()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, Some(NullOrder::First))]),
                }])
                .collect(),
        );
        let nulls_last = plan(
            base_ops()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, Some(NullOrder::Last))]),
                }])
                .collect(),
        );

        let first = store
            .execute_plan_query(&nulls_first, &params(), GqlExecutionContext::default())
            .expect("execute nulls first sort");
        let last = store
            .execute_plan_query(&nulls_last, &params(), GqlExecutionContext::default())
            .expect("execute nulls last sort");

        assert_eq!(first.rows[0].get("name"), Some(&Value::Null));
        assert_eq!(last.rows[2].get("name"), Some(&Value::Null));
    }

    #[test]
    fn sort_rejects_incomparable_keys() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QuerySortIncomparable"],
                [("key", Value::Text("x".into()))],
            )
            .expect("insert text vertex");
        store
            .insert_vertex_named(["QuerySortIncomparable"], [("key", Value::Int64(1))])
            .expect("insert int vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QuerySortIncomparable".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "key"), "key")],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: order_by(vec![sort_item(var("key"), None, None)]),
            },
        ]);

        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("incomparable keys should fail");

        assert!(matches!(err, PlanQueryError::IncomparableSortValues { .. }));
    }

    #[test]
    fn topk_sorts_then_applies_offset_and_k() {
        let store = GraphStore::new();
        for name in ["TopK D", "TopK A", "TopK C", "TopK B"] {
            store
                .insert_vertex_named(["QueryTopK"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryTopK".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
            PlanOp::TopK {
                order_by: order_by(vec![sort_item(var("name"), None, None)]),
                k: Expr::new(ExprKind::Literal(Value::Int64(2))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute topk");

        assert_eq!(text_column(&result, "name"), vec!["TopK B", "TopK C"]);
    }

    #[test]
    fn cartesian_product_combines_independent_subplans() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryCartesianLeft"],
                [("name", Value::Text("Left A".into()))],
            )
            .expect("insert left a");
        store
            .insert_vertex_named(
                ["QueryCartesianLeft"],
                [("name", Value::Text("Left B".into()))],
            )
            .expect("insert left b");
        store
            .insert_vertex_named(
                ["QueryCartesianRight"],
                [("name", Value::Text("Right A".into()))],
            )
            .expect("insert right a");
        store
            .insert_vertex_named(
                ["QueryCartesianRight"],
                [("name", Value::Text("Right B".into()))],
            )
            .expect("insert right b");
        let plan = plan(vec![
            PlanOp::CartesianProduct {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryCartesianLeft".into()),
                    property_projection: None,
                }],
                right: vec![PlanOp::NodeScan {
                    variable: "b".into(),
                    label: Some("QueryCartesianRight".into()),
                    property_projection: None,
                }],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "left"),
                    project(prop("b", "name"), "right"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute cartesian product");

        assert_eq!(result.rows.len(), 4);
    }

    #[test]
    fn cartesian_product_drops_conflicting_bindings() {
        let left = PlanRow::from([("x".to_owned(), PlanBinding::Value(Value::Int64(1)))]);
        let same = PlanRow::from([("x".to_owned(), PlanBinding::Value(Value::Int64(1)))]);
        let different = PlanRow::from([("x".to_owned(), PlanBinding::Value(Value::Int64(2)))]);

        assert_eq!(merge_rows(&left, &same), Some(left.clone()));
        assert_eq!(merge_rows(&left, &different), None);
    }

    #[test]
    fn hash_join_matches_planned_two_match() {
        let store = GraphStore::new();
        let alice = store
            .insert_vertex_named(
                ["QueryHashJoinUser"],
                [("name", Value::Text("HJ Alice".into()))],
            )
            .expect("insert alice");
        let bob = store
            .insert_vertex_named(
                ["QueryHashJoinTarget"],
                [("name", Value::Text("HJ Bob".into()))],
            )
            .expect("insert bob");
        store
            .insert_directed_edge_named(
                alice,
                bob,
                Some("QueryHashJoinKnows"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");

        let gql = "MATCH (a:QueryHashJoinUser) MATCH (a)-[r:QueryHashJoinKnows]->(b:QueryHashJoinTarget) \
                   RETURN a.name AS an, b.name AS bn";
        let sequential = plan_gql(gql);
        let seq_result = store
            .execute_plan_query(&sequential, &params(), GqlExecutionContext::default())
            .expect("sequential two-match");

        let hash_plan = plan(vec![
            PlanOp::HashJoin {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryHashJoinUser".into()),
                    property_projection: None,
                }],
                right: vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("QueryHashJoinUser".into()),
                        property_projection: None,
                    },
                    PlanOp::Expand {
                        src: "a".into(),
                        edge: "r".into(),
                        dst: "b".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("QueryHashJoinKnows".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                        hop_aux_binding: None,
                        emit_edge_binding: true,
                    },
                ],
                join_keys: vec!["a".into()],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "an"),
                    project(prop("b", "name"), "bn"),
                ],
                distinct: false,
            },
        ]);

        let hj_result = store
            .execute_plan_query(&hash_plan, &params(), GqlExecutionContext::default())
            .expect("hash join");

        assert_eq!(hj_result.rows.len(), seq_result.rows.len());
        assert_eq!(hj_result.rows, seq_result.rows);
    }

    #[test]
    fn hash_join_joins_equivalent_decimal_scales() {
        use gleaph_gql::types::Decimal;

        let lit_decimal = |s: &str| {
            Expr::new(ExprKind::Literal(Value::Decimal(
                Decimal::parse(s).expect("decimal literal"),
            )))
        };
        let lit_text = |t: &str| Expr::new(ExprKind::Literal(Value::Text(t.into())));

        let plan = plan(vec![PlanOp::HashJoin {
            left: vec![PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: lit_decimal("1.0"),
                        alias: Some("k".into()),
                    },
                    ProjectColumn {
                        expr: lit_text("L"),
                        alias: Some("left_tag".into()),
                    },
                ],
                distinct: false,
            }],
            right: vec![PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: lit_decimal("1.00"),
                        alias: Some("k".into()),
                    },
                    ProjectColumn {
                        expr: lit_text("R"),
                        alias: Some("right_tag".into()),
                    },
                ],
                distinct: false,
            }],
            join_keys: vec!["k".into()],
        }]);

        let store = GraphStore::new();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("decimal hash join");

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.get("left_tag"), Some(&Value::Text("L".into())));
        assert_eq!(row.get("right_tag"), Some(&Value::Text("R".into())));
        assert_eq!(
            row.get("k"),
            Some(&Value::Decimal(Decimal::parse("1.0").expect("k")))
        );
    }

    /// Two left + three right rows share the same join key → six merged rows (L×R multiplicity).
    /// Also checks `right_id` survives `merge_rows` (right-only binding) alongside `left_id`.
    #[test]
    fn hash_join_same_key_row_multiplicity_2x3() {
        let store = GraphStore::new();
        for (left_id, tag) in [(0i64, "L0"), (1, "L1")] {
            store
                .insert_vertex_named(
                    ["QueryHashJoinDupL"],
                    [
                        ("jk", Value::Int64(7)),
                        ("left_id", Value::Int64(left_id)),
                        ("left_tag", Value::Text(tag.into())),
                    ],
                )
                .expect("insert left");
        }
        for (right_id, tag) in [(0i64, "R0"), (1, "R1"), (2, "R2")] {
            store
                .insert_vertex_named(
                    ["QueryHashJoinDupR"],
                    [
                        ("jk", Value::Int64(7)),
                        ("right_id", Value::Int64(right_id)),
                        ("right_tag", Value::Text(tag.into())),
                    ],
                )
                .expect("insert right");
        }

        let scan_project_l = || {
            vec![
                PlanOp::NodeScan {
                    variable: "nl".into(),
                    label: Some("QueryHashJoinDupL".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("nl", "jk"), "jk"),
                        project(prop("nl", "left_id"), "left_id"),
                        project(prop("nl", "left_tag"), "left_tag"),
                    ],
                    distinct: false,
                },
            ]
        };
        let scan_project_r = || {
            vec![
                PlanOp::NodeScan {
                    variable: "nr".into(),
                    label: Some("QueryHashJoinDupR".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("nr", "jk"), "jk"),
                        project(prop("nr", "right_id"), "right_id"),
                        project(prop("nr", "right_tag"), "right_tag"),
                    ],
                    distinct: false,
                },
            ]
        };

        let plan = plan(vec![PlanOp::HashJoin {
            left: scan_project_l(),
            right: scan_project_r(),
            join_keys: vec!["jk".into()],
        }]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("hash join multiplicity");

        assert_eq!(result.rows.len(), 6);
        let mut pairs: Vec<(i64, i64)> = result
            .rows
            .iter()
            .map(|row| {
                let li = match row.get("left_id") {
                    Some(Value::Int64(x)) => *x,
                    other => panic!("expected int left_id, got {other:?}"),
                };
                let ri = match row.get("right_id") {
                    Some(Value::Int64(x)) => *x,
                    other => panic!("expected int right_id, got {other:?}"),
                };
                (li, ri)
            })
            .collect();
        pairs.sort();
        assert_eq!(pairs, vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2),]);
        for row in &result.rows {
            assert_eq!(row.get("jk"), Some(&Value::Int64(7)));
            assert!(row.get("left_tag").is_some());
            assert!(row.get("right_tag").is_some());
        }
    }

    #[test]
    fn hash_join_two_join_keys_excludes_partial_match() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyL"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(2)),
                    ("lt", Value::Text("L12".into())),
                ],
            )
            .expect("insert L12");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyL"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(3)),
                    ("lt", Value::Text("L13".into())),
                ],
            )
            .expect("insert L13");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyR"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(2)),
                    ("rt", Value::Text("R12".into())),
                ],
            )
            .expect("insert R12");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyR"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(99)),
                    ("rt", Value::Text("R199".into())),
                ],
            )
            .expect("insert R199");

        let plan = plan(vec![PlanOp::HashJoin {
            left: vec![
                PlanOp::NodeScan {
                    variable: "l".into(),
                    label: Some("QueryHashJoin2KeyL".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("l", "ka"), "ka"),
                        project(prop("l", "kb"), "kb"),
                        project(prop("l", "lt"), "lt"),
                    ],
                    distinct: false,
                },
            ],
            right: vec![
                PlanOp::NodeScan {
                    variable: "r".into(),
                    label: Some("QueryHashJoin2KeyR".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("r", "ka"), "ka"),
                        project(prop("r", "kb"), "kb"),
                        project(prop("r", "rt"), "rt"),
                    ],
                    distinct: false,
                },
            ],
            join_keys: vec!["ka".into(), "kb".into()],
        }]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("two-key hash join");

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.get("ka"), Some(&Value::Int64(1)));
        assert_eq!(row.get("kb"), Some(&Value::Int64(2)));
        assert_eq!(row.get("lt"), Some(&Value::Text("L12".into())));
        assert_eq!(row.get("rt"), Some(&Value::Text("R12".into())));
    }

    #[test]
    fn hash_join_matches_sequential_on_branching_graph() {
        let store = GraphStore::new();
        let alice = store
            .insert_vertex_named(
                ["QueryHashJoinBranchUser"],
                [("name", Value::Text("Branch Alice".into()))],
            )
            .expect("insert user");
        let bob = store
            .insert_vertex_named(
                ["QueryHashJoinBranchTarget"],
                [("name", Value::Text("Branch Bob".into()))],
            )
            .expect("insert bob");
        let carol = store
            .insert_vertex_named(
                ["QueryHashJoinBranchTarget"],
                [("name", Value::Text("Branch Carol".into()))],
            )
            .expect("insert carol");
        store
            .insert_directed_edge_named(
                alice,
                bob,
                Some("QueryHashJoinBranchRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge to bob");
        store
            .insert_directed_edge_named(
                alice,
                carol,
                Some("QueryHashJoinBranchRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge to carol");

        let gql = "MATCH (a:QueryHashJoinBranchUser) MATCH (a)-[r:QueryHashJoinBranchRel]->(b:QueryHashJoinBranchTarget) \
                   RETURN a.name AS an, b.name AS bn";
        let sequential = plan_gql(gql);
        let seq_result = store
            .execute_plan_query(&sequential, &params(), GqlExecutionContext::default())
            .expect("sequential two-match branching");

        let hash_plan = plan(vec![
            PlanOp::HashJoin {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryHashJoinBranchUser".into()),
                    property_projection: None,
                }],
                right: vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("QueryHashJoinBranchUser".into()),
                        property_projection: None,
                    },
                    PlanOp::Expand {
                        src: "a".into(),
                        edge: "r".into(),
                        dst: "b".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("QueryHashJoinBranchRel".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                        hop_aux_binding: None,
                        emit_edge_binding: true,
                    },
                ],
                join_keys: vec!["a".into()],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "an"),
                    project(prop("b", "name"), "bn"),
                ],
                distinct: false,
            },
        ]);

        let hj_result = store
            .execute_plan_query(&hash_plan, &params(), GqlExecutionContext::default())
            .expect("hash join branching");

        assert_eq!(hj_result.rows.len(), seq_result.rows.len());
        fn pair_key(row: &std::collections::BTreeMap<String, Value>) -> (String, String) {
            let an = match row.get("an") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("expected text an, got {other:?}"),
            };
            let bn = match row.get("bn") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("expected text bn, got {other:?}"),
            };
            (an, bn)
        }
        let mut hj_keys: Vec<_> = hj_result.rows.iter().map(pair_key).collect();
        hj_keys.sort();
        let mut seq_keys: Vec<_> = seq_result.rows.iter().map(pair_key).collect();
        seq_keys.sort();
        assert_eq!(hj_keys, seq_keys);
    }

    #[test]
    fn directed_expand_projects_endpoint_and_edge_properties() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["QueryExpandSource"],
                [("name", Value::Text("Expand Alice".into()))],
            )
            .expect("insert source");
        let b = store
            .insert_vertex_named(
                ["QueryExpandTarget"],
                [("name", Value::Text("Expand Bob".into()))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(a, b, Some("QueryKnows"), [("since", Value::Int64(2026))])
            .expect("insert edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryExpandSource".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("QueryKnows".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "a_name"),
                    project(prop("b", "name"), "b_name"),
                    project(prop("e", "since"), "since"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("a_name"),
            Some(&Value::Text("Expand Alice".into()))
        );
        assert_eq!(
            result.rows[0].get("b_name"),
            Some(&Value::Text("Expand Bob".into()))
        );
        assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2026)));
    }

    #[test]
    fn reverse_expand_resolves_edge_properties_through_alias() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["QueryReverseSource"], [("name", Value::Text("A".into()))])
            .expect("insert source");
        let b = store
            .insert_vertex_named(["QueryReverseTarget"], [("name", Value::Text("B".into()))])
            .expect("insert target");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("QueryReverseKnows"),
                [("since", Value::Int64(2027))],
            )
            .expect("insert edge");

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "b".into(),
                label: Some("QueryReverseTarget".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "b".into(),
                edge: "e".into(),
                dst: "a".into(),
                direction: EdgeDirection::PointingLeft,
                label: Some("QueryReverseKnows".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![project(prop("e", "since"), "since")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute reverse query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2027)));
    }

    #[test]
    fn undirected_expand_from_noncanonical_endpoint_resolves_edge_properties_through_alias() {
        let store = GraphStore::new();
        let low = store
            .insert_vertex_named(["QueryUndirLow"], [("name", Value::Text("low".into()))])
            .expect("insert low");
        let high = store
            .insert_vertex_named(["QueryUndirHigh"], [("name", Value::Text("high".into()))])
            .expect("insert high");
        store
            .insert_undirected_edge_named(
                low,
                high,
                Some("QueryUndirKnows"),
                [("since", Value::Int64(2028))],
            )
            .expect("insert edge");

        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryUndirLow".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::Undirected,
                label: Some("QueryUndirKnows".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![project(prop("e", "since"), "since")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute undirected query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2028)));
    }

    fn setup_reused_dst_expand_graph(store: &GraphStore) -> VertexId {
        let a = store
            .insert_vertex_named(["ReuseExpandA"], [("name", Value::Text("anchor".into()))])
            .expect("insert anchor");
        let b = store
            .insert_vertex_named(["ReuseExpandB"], [("name", Value::Text("other".into()))])
            .expect("insert neighbor");
        store
            .insert_directed_edge_named(a, a, Some("ReuseExpandRel"), Vec::<(&str, Value)>::new())
            .expect("self-loop");
        store
            .insert_directed_edge_named(a, b, Some("ReuseExpandRel"), Vec::<(&str, Value)>::new())
            .expect("out-edge");
        a
    }

    #[test]
    fn expand_reused_dst_only_keeps_self_loop_edges() {
        let store = GraphStore::new();
        let anchor = setup_reused_dst_expand_graph(&store);
        let plan =
            plan_gql("MATCH (a:ReuseExpandA)-[:ReuseExpandRel]->(a) RETURN ELEMENT_ID(a) AS a_id");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("reused dst expand");
        assert_eq!(
            result.rows.len(),
            1,
            "only self-loop may satisfy reused dst: {:?}",
            result.rows
        );
        let Value::Bytes(id_bytes) = result.rows[0].get("a_id").expect("a_id column") else {
            panic!(
                "expected ELEMENT_ID bytes, got {:?}",
                result.rows[0].get("a_id")
            );
        };
        assert_eq!(
            GraphPathVertexId::try_from_slice(id_bytes.as_ref())
                .expect("decode vertex id")
                .vertex_id,
            anchor,
        );
    }

    #[test]
    fn expand_reused_dst_rejects_neighbor_mismatch() {
        let store = GraphStore::new();
        setup_reused_dst_expand_graph(&store);
        let plan = plan_gql("MATCH (a:ReuseExpandA)-[:ReuseExpandRel]->(a) RETURN a.name AS name");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("reused dst expand");
        assert!(
            !result
                .rows
                .iter()
                .any(|row| row.get("name") == Some(&Value::Text("other".into()))),
            "reused dst must not adopt neighbor vertex binding: {:?}",
            result.rows
        );
    }

    #[test]
    fn limited_expand_reused_dst_skips_neighbor_mismatch() {
        let store = GraphStore::new();
        setup_reused_dst_expand_graph(&store);
        let plan =
            plan_gql("MATCH (a:ReuseExpandA)-[:ReuseExpandRel]->(a) RETURN a.name AS name LIMIT 1");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("reused dst expand");
        assert_eq!(text_column(&result, "name"), vec!["anchor"]);
    }

    fn setup_reused_dst_relabeled_graph(store: &GraphStore) -> VertexId {
        let a = store
            .insert_vertex_named(
                ["ReuseRelabelPerson", "ReuseRelabelUser"],
                [("name", Value::Text("anchor".into()))],
            )
            .expect("insert anchor");
        let b = store
            .insert_vertex_named(
                ["ReuseRelabelPerson"],
                [("name", Value::Text("other".into()))],
            )
            .expect("insert neighbor");
        store
            .insert_directed_edge_named(a, a, Some("ReuseRelabelRel"), Vec::<(&str, Value)>::new())
            .expect("self-loop");
        store
            .insert_directed_edge_named(a, b, Some("ReuseRelabelRel"), Vec::<(&str, Value)>::new())
            .expect("out-edge");
        a
    }

    #[test]
    fn expand_reused_dst_relabeled_endpoints_keep_self_loop() {
        let store = GraphStore::new();
        let anchor = setup_reused_dst_relabeled_graph(&store);
        let plan = plan_gql(
            "MATCH (a:ReuseRelabelPerson)-[:ReuseRelabelRel]->(a:ReuseRelabelUser) RETURN ELEMENT_ID(a) AS a_id",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("reused relabeled dst expand");
        assert_eq!(
            result.rows.len(),
            1,
            "self-loop with relabeled reuse must keep anchor: {:?}",
            result.rows
        );
        let Value::Bytes(id_bytes) = result.rows[0].get("a_id").expect("a_id column") else {
            panic!(
                "expected ELEMENT_ID bytes, got {:?}",
                result.rows[0].get("a_id")
            );
        };
        assert_eq!(
            GraphPathVertexId::try_from_slice(id_bytes.as_ref())
                .expect("decode vertex id")
                .vertex_id,
            anchor,
        );
    }

    #[test]
    fn expand_filter_applies_destination_predicate() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["QueryExpandFilterSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        let keep = store
            .insert_vertex_named(["QueryExpandFilterTarget"], [("age", Value::Int64(44))])
            .expect("insert keep target");
        let drop = store
            .insert_vertex_named(["QueryExpandFilterTarget"], [("age", Value::Int64(10))])
            .expect("insert drop target");
        store
            .insert_directed_edge_named(
                a,
                keep,
                Some("QueryExpandFilterEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert keep edge");
        store
            .insert_directed_edge_named(
                a,
                drop,
                Some("QueryExpandFilterEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert drop edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryExpandFilterSource".into()),
                property_projection: None,
            },
            PlanOp::ExpandFilter {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("QueryExpandFilterEdge".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                dst_filter: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("b", "age")),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![project(prop("b", "age"), "age")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("age"), Some(&Value::Int64(44)));
    }

    #[test]
    fn expand_indexed_edge_equality_filters_candidates() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["IdxEqA"], Vec::<(&str, Value)>::new())
            .expect("a");
        let b_match = store
            .insert_vertex_named(["IdxEqB"], Vec::<(&str, Value)>::new())
            .expect("b match");
        let b_miss = store
            .insert_vertex_named(["IdxEqB"], Vec::<(&str, Value)>::new())
            .expect("b miss");
        store
            .insert_directed_edge_named(a, b_match, Some("IdxEqRel"), [("weight", Value::Int64(5))])
            .expect("match edge");
        store
            .insert_directed_edge_named(a, b_miss, Some("IdxEqRel"), [("weight", Value::Int64(9))])
            .expect("miss edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("IdxEqA".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("IdxEqRel".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: Some(("weight".into(), ScanValue::Literal(Value::Int64(5)))),
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![project(var("b"), "b")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("indexed expand");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn indexed_expand_limit_offset_skips_only_matching_edges() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["IdxEqPageA"], Vec::<(&str, Value)>::new())
            .expect("a");
        for (i, weight) in [(0, 5), (1, 9), (2, 5), (3, 9), (4, 5), (5, 5)] {
            let b = store
                .insert_vertex_named(
                    ["IdxEqPageB"],
                    [("name", Value::Text(format!("indexed edge {i}")))],
                )
                .expect("b");
            store
                .insert_directed_edge_named(
                    a,
                    b,
                    Some("IdxEqPageRel"),
                    [("weight", Value::Int64(weight))],
                )
                .expect("edge");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("IdxEqPageA".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("IdxEqPageRel".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: Some(("weight".into(), ScanValue::Literal(Value::Int64(5)))),
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![project(prop("b", "name"), "name")],
                distinct: false,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("indexed expand");

        assert_eq!(
            text_column(&result, "name"),
            vec!["indexed edge 4", "indexed edge 2"]
        );
    }

    #[test]
    fn expand_applies_dst_property_projection_for_property_return() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["ProjA"], [("uid", Value::Text("a1".into()))])
            .expect("a");
        let b = store
            .insert_vertex_named(["ProjB"], [("uid", Value::Text("b1".into()))])
            .expect("b");
        store
            .insert_directed_edge_named(a, b, Some("ProjRel"), Vec::<(&str, Value)>::new())
            .expect("edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("ProjA".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("ProjRel".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: Some(Rc::from([])),
                dst_property_projection: Some(Rc::from([Str::from("uid")])),
                hop_aux_binding: None,
                emit_edge_binding: false,
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "uid"), "a_uid"),
                    project(prop("b", "uid"), "b_uid"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("projection expand");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("a_uid"), Some(&Value::Text("a1".into())));
        assert_eq!(result.rows[0].get("b_uid"), Some(&Value::Text("b1".into())));
    }

    #[test]
    fn return_star_projects_vertex_and_edge_records() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["QueryReturnStarSource"],
                [("name", Value::Text("Star A".into()))],
            )
            .expect("insert source");
        let b = store
            .insert_vertex_named(
                ["QueryReturnStarTarget"],
                [("name", Value::Text("Star B".into()))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("QueryReturnStarEdge"),
                [("since", Value::Int64(1))],
            )
            .expect("insert edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryReturnStarSource".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("QueryReturnStarEdge".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0].get("a"), Some(Value::Record(_))));
        assert!(matches!(result.rows[0].get("b"), Some(Value::Record(_))));
        assert!(matches!(result.rows[0].get("e"), Some(Value::Record(_))));
    }

    #[test]
    fn materialize_and_limit_shape_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryLimitPerson"],
                [("name", Value::Text("Limit A".into()))],
            )
            .expect("insert first");
        store
            .insert_vertex_named(
                ["QueryLimitPerson"],
                [("name", Value::Text("Limit B".into()))],
            )
            .expect("insert second");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryLimitPerson".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
            PlanOp::Materialize {
                columns: vec![],
                distinct: false,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                offset: None,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Limit A".into()))
        );
    }

    #[test]
    fn optional_match_planner_null_padding_when_no_edge() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptMatchA"], [("name", Value::Text("solo".into()))])
            .expect("insert vertex");
        let gql = "MATCH (n:OptMatchA) OPTIONAL MATCH (n)-[e:OptMatchRel]->(m:OptMatchB) \
                   RETURN n.name AS nn, m.name AS mn";
        let plan = plan_gql(gql);
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::OptionalMatch { .. })),
            "expected OptionalMatch in plan: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute optional match");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("nn"), Some(&Value::Text("solo".into())));
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Null));
    }

    #[test]
    fn optional_match_planner_returns_m_when_edge_exists() {
        let store = GraphStore::new();
        let n = store
            .insert_vertex_named(["OptMatchA2"], [("name", Value::Text("a".into()))])
            .expect("insert n");
        let m = store
            .insert_vertex_named(["OptMatchB2"], [("name", Value::Text("buddy".into()))])
            .expect("insert m");
        store
            .insert_directed_edge_named(n, m, Some("OptMatchRel2"), Vec::<(&str, Value)>::new())
            .expect("insert edge");
        let gql = "MATCH (n:OptMatchA2) OPTIONAL MATCH (n)-[e:OptMatchRel2]->(m:OptMatchB2) \
                   RETURN m.name AS mn";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute optional match");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Text("buddy".into())));
    }

    #[test]
    fn optional_match_leading_empty_graph_null_binds_pattern_var() {
        let store = GraphStore::new();
        let gql = "OPTIONAL MATCH (n:OptMatchLeading) RETURN n IS NULL AS is_n_null";
        let plan = plan_gql(gql);
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::OptionalMatch { .. })),
            "expected OptionalMatch: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute leading optional");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("is_n_null"), Some(&Value::Bool(true)));
    }

    #[test]
    fn mandatory_match_after_optional_miss_drops_null_bound_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptChainA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        store
            .insert_vertex_named(["OptChainB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .get_or_insert_edge_label_id("OptChainRel")
            .expect("edge label");
        let gql = "MATCH (a:OptChainA) OPTIONAL MATCH (a)-[e:OptChainRel]->(b:OptChainB) \
                   MATCH (b)-[e2:OptChainRel]->(c:OptChainB) RETURN a, b, c";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("mandatory match after optional miss should not error");
        assert!(
            result.rows.is_empty(),
            "optional miss leaves b null; mandatory follow-on match should drop the row: {:?}",
            result.rows
        );
    }

    #[test]
    fn mandatory_match_after_optional_hit_continues_chain() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["OptChainA2"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["OptChainB2"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        let c = store
            .insert_vertex_named(["OptChainC2"], Vec::<(&str, Value)>::new())
            .expect("insert c");
        store
            .insert_directed_edge_named(a, b, Some("OptChainRel2"), Vec::<(&str, Value)>::new())
            .expect("a->b");
        store
            .insert_directed_edge_named(b, c, Some("OptChainRel2"), Vec::<(&str, Value)>::new())
            .expect("b->c");
        let gql = "MATCH (a:OptChainA2) OPTIONAL MATCH (a)-[e:OptChainRel2]->(b:OptChainB2) \
                   MATCH (b)-[e2:OptChainRel2]->(c:OptChainC2) RETURN a, b, c";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("mandatory match after optional hit");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn mandatory_node_only_match_after_optional_miss_drops_null_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptLabelA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        store
            .insert_vertex_named(["OptLabelB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        let gql = "MATCH (a:OptLabelA) OPTIONAL MATCH (a)-[e:OptLabelRel]->(b:OptLabelB) \
                   MATCH (b:OptLabelB) RETURN b";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("mandatory node-only match after optional miss");
        assert!(
            result.rows.is_empty(),
            "null optional binding must fail mandatory labeled node match: {:?}",
            result.rows
        );
    }

    #[test]
    fn rebound_node_label_is_enforced_without_rescan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["RebindA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let gql = "MATCH (a:RebindA) MATCH (a:RebindB) RETURN a";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("rebound label check");
        assert!(
            result.rows.is_empty(),
            "vertex labeled RebindA must not satisfy rebound RebindB match: {:?}",
            result.rows
        );
    }

    #[test]
    fn rebound_label_succeeds_when_vertex_has_both_labels() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["DualA", "DualB"], Vec::<(&str, Value)>::new())
            .expect("insert dual-label vertex");
        let gql = "MATCH (a:DualA) MATCH (a:DualB) RETURN a";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("dual-label rebound");
        assert_eq!(
            result.rows.len(),
            1,
            "vertex with both labels must satisfy sequential label matches: {:?}",
            result.rows
        );
    }

    // Manual NodeScan + PropertyFilter plans: `plan_gql` may emit IndexScan for inline
    // label properties, which fails in tests without an index client.
    #[test]
    fn rebound_inline_property_fails_when_value_mismatches() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PropRebindA"], [("nick", Value::Text("x".into()))])
            .expect("insert a");
        let nick_eq = |value: &str| {
            Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::var("a")),
                    property: "nick".into(),
                })),
                op: gleaph_gql::ast::CmpOp::Eq,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Text(value.into())))),
            })
        };
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("PropRebindA".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq("x")],
                stage: 0,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq("y")],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(var("a"), "a")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("rebound inline property mismatch");
        assert!(
            result.rows.is_empty(),
            "stricter rebound property must filter mismatched rows: {:?}",
            result.rows
        );
    }

    #[test]
    fn rebound_inline_property_succeeds_when_value_matches() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PropRebindB"], [("nick", Value::Text("same".into()))])
            .expect("insert a");
        let nick_eq = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::var("a")),
                property: "nick".into(),
            })),
            op: gleaph_gql::ast::CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Text("same".into())))),
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("PropRebindB".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq.clone()],
                stage: 0,
            },
            PlanOp::PropertyFilter {
                predicates: vec![nick_eq],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(var("a"), "a")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("rebound inline property match");
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn optional_miss_fails_labeled_node_only_match() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptMissA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        store
            .get_or_insert_edge_label_id("OptMissRel")
            .expect("edge label");
        let gql = "MATCH (a:OptMissA) OPTIONAL MATCH (a)-[e:OptMissRel]->(b:OptMissB) \
                   MATCH (b:OptMissB) RETURN b";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("optional miss labeled match");
        assert!(
            result.rows.is_empty(),
            "optional miss must drop rows on mandatory labeled node-only match: {:?}",
            result.rows
        );
    }

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
    fn return_abs_gleaph_weight_does_not_break_decoder_prep() {
        let store = GraphStore::new();
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let a = store
            .insert_vertex_named(["AbsWgtA"], Vec::<(&str, Value)>::new())
            .expect("a");
        let b = store
            .insert_vertex_named(["AbsWgtB"], Vec::<(&str, Value)>::new())
            .expect("b");
        let label_id = store
            .get_or_insert_edge_label_id("AbsWgtRoad")
            .expect("label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_directed_edge_with_inline_value(a, b, Some(label_id), 3)
            .expect("edge");
        let gql = "MATCH (a:AbsWgtA)-[e:AbsWgtRoad]->(b:AbsWgtB) RETURN ABS(GLEAPH.WEIGHT(e)) AS w";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("abs gleaph weight return");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("w"), Some(&Value::Float32(3.0)));
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
    fn optional_match_manual_null_padding_edge_and_dst() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptManualN"], Vec::<(&str, Value)>::new())
            .expect("insert n");
        let expand = PlanOp::Expand {
            src: "n".into(),
            edge: "e".into(),
            dst: "m".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("OptManualRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
        };
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("OptManualN".into()),
                property_projection: None,
            },
            PlanOp::OptionalMatch {
                sub_plan: vec![expand],
            },
            PlanOp::Project {
                columns: vec![
                    project(Expr::new(ExprKind::IsNull(Box::new(var("e")))), "e_null"),
                    project(Expr::new(ExprKind::IsNull(Box::new(var("m")))), "m_null"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute manual optional");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("e_null"), Some(&Value::Bool(true)));
        assert_eq!(result.rows[0].get("m_null"), Some(&Value::Bool(true)));
    }

    #[test]
    fn optional_match_gleaph_weight_on_null_edge_returns_null() {
        let store = GraphStore::new();
        store
            .get_or_insert_edge_label_id("NullWgtRel")
            .expect("edge label");
        store
            .set_edge_label_weight_profile(
                store
                    .get_or_insert_edge_label_id("NullWgtRel")
                    .expect("label"),
                gleaph_graph_kernel::entry::EdgeWeightProfile {
                    encoding: gleaph_graph_kernel::entry::WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_vertex_named(["NullWgtN"], Vec::<(&str, Value)>::new())
            .expect("insert n");
        let gql = "MATCH (n:NullWgtN) OPTIONAL MATCH (n)-[e:NullWgtRel]->(m) \
                   RETURN GLEAPH.WEIGHT(e) AS w";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("gleaph weight on optional miss should return null");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("w"), Some(&Value::Null));
    }

    fn eval_test_expr(expr: Expr) -> Value {
        let store = GraphStore::new();
        let params = params();
        let evaluator = QueryExprEvaluator {
            store: &store,
            parameters: &params,
            aggregate_specs: None,
            caller: None,
            gleaph_weight_decoders: None,
        };
        evaluator
            .eval_expr(&PlanRow::new(), &expr)
            .expect("eval test expr")
    }

    #[test]
    fn case_searched_skips_untaken_invalid_result() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Bool(false))),
                result: Expr::new(ExprKind::Sqrt(Box::new(Expr::new(ExprKind::Literal(
                    Value::Float32(-1.0),
                ))))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Float32(1.0))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Float32(1.0));
    }

    #[test]
    fn case_searched_unknown_skips_invalid_then() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Null)),
                    result: Expr::new(ExprKind::Sqrt(Box::new(Expr::new(ExprKind::Literal(
                        Value::Float32(-1.0),
                    ))))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Bool(true))),
                    result: Expr::new(ExprKind::Literal(Value::Int32(2))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(2));
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
    fn case_simple_skips_untaken_invalid_result() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Literal(Value::Int32(0)))),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Int32(1))),
                result: Expr::new(ExprKind::Sqrt(Box::new(Expr::new(ExprKind::Literal(
                    Value::Float32(-1.0),
                ))))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(2))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(2));
    }

    #[test]
    fn case_searched_unknown_condition_falls_through() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Null)),
                    result: Expr::new(ExprKind::Literal(Value::Int32(1))),
                },
                WhenClause {
                    span: Span::DUMMY,
                    condition: Expr::new(ExprKind::Literal(Value::Bool(true))),
                    result: Expr::new(ExprKind::Literal(Value::Int32(2))),
                },
            ],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(2));
    }

    #[test]
    fn case_searched_all_unknown_uses_else() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Null)),
                result: Expr::new(ExprKind::Literal(Value::Int32(1))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(3));
    }

    #[test]
    fn case_simple_skips_incomparable_when_and_uses_else() {
        use gleaph_gql::ast::WhenClause;
        let expr = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Literal(Value::Int32(1)))),
            when_clauses: vec![WhenClause {
                span: Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Text("a".into()))),
                result: Expr::new(ExprKind::Literal(Value::Int32(99))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Int32(3))))),
        });
        assert_eq!(eval_test_expr(expr), Value::Int32(3));
    }

    fn agg_count_star() -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_sum_expr(inner: Expr, distinct: bool) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Sum,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct,
            order_by: None,
            filter: None,
        })
    }

    fn agg_min_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Min,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_max_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Max,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_avg_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Avg,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    #[test]
    fn aggregate_count_star_empty_graph_after_scan() {
        let store = GraphStore::new();
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("NoVerticesForAgg".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("cnt"))],
            },
            PlanOp::Project {
                columns: vec![project(agg_count_star(), "cnt")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("global aggregate on empty match");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(0)));
    }

    #[test]
    fn aggregate_count_star_after_node_scan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggScanLbl"], [("x", Value::Int64(1))])
            .expect("v1");
        store
            .insert_vertex_named(["AggScanLbl"], [("x", Value::Int64(2))])
            .expect("v2");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggScanLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("cnt"))],
            },
            PlanOp::Project {
                columns: vec![project(agg_count_star(), "cnt")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("count");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }

    #[test]
    fn aggregate_groups_by_property_and_counts_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggGrpLbl"], [("dept", Value::Text("S".into()))])
            .expect("a");
        store
            .insert_vertex_named(["AggGrpLbl"], [("dept", Value::Text("S".into()))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggGrpLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![prop("n", "dept")],
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("c"))],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "dept"), "d"),
                    project(agg_count_star(), "c"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("grouped");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("d"), Some(&Value::Text("S".into())));
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(2)));
    }

    #[test]
    fn aggregate_sum_min_max_avg_numeric_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggNumLbl"], [("v", Value::Int64(10))])
            .expect("a");
        store
            .insert_vertex_named(["AggNumLbl"], [("v", Value::Int64(20))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggNumLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![
                    agg_spec(AggregateFunc::Sum, Some(prop("n", "v")), false, Some("s")),
                    agg_spec(AggregateFunc::Min, Some(prop("n", "v")), false, Some("mn")),
                    agg_spec(AggregateFunc::Max, Some(prop("n", "v")), false, Some("mx")),
                    agg_spec(AggregateFunc::Avg, Some(prop("n", "v")), false, Some("a")),
                ],
            },
            PlanOp::Project {
                columns: vec![
                    project(agg_sum_expr(prop("n", "v"), false), "s"),
                    project(agg_min_expr(prop("n", "v")), "mn"),
                    project(agg_max_expr(prop("n", "v")), "mx"),
                    project(agg_avg_expr(prop("n", "v")), "a"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("agg");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("s"), Some(&Value::Int64(30)));
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Int64(10)));
        assert_eq!(result.rows[0].get("mx"), Some(&Value::Int64(20)));
        assert_eq!(result.rows[0].get("a"), Some(&Value::Int64(15)));
    }

    #[test]
    fn aggregate_count_distinct_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggDistLbl"], [("k", Value::Int64(1))])
            .expect("a");
        store
            .insert_vertex_named(["AggDistLbl"], [("k", Value::Int64(1))])
            .expect("b");
        let count_distinct = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Count,
            expr: Some(Box::new(prop("n", "k"))),
            expr2: None,
            distinct: true,
            order_by: None,
            filter: None,
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggDistLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(
                    AggregateFunc::Count,
                    Some(prop("n", "k")),
                    true,
                    Some("c"),
                )],
            },
            PlanOp::Project {
                columns: vec![project(count_distinct, "c")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("distinct");
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }

    #[test]
    fn aggregate_grouped_empty_input_yields_no_rows() {
        let store = GraphStore::new();
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("NoSuchAggLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![prop("n", "dept")],
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("c"))],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "dept"), "d"),
                    project(agg_count_star(), "c"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("empty groups");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn aggregate_count_star_with_filter_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["FiltAggLbl"], [("ok", Value::Bool(false))])
            .expect("v0");
        store
            .insert_vertex_named(["FiltAggLbl"], [("ok", Value::Bool(true))])
            .expect("v1");
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(prop("n", "ok")),
            op: CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
        });
        let count_star_filtered = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: Some(Box::new(filter.clone())),
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("FiltAggLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::CountStar,
                    expr: None,
                    expr2: None,
                    distinct: false,
                    filter: Some(filter),
                    order_by: None,
                    alias: Some("c".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(count_star_filtered, "c")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("filtered");
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }

    #[test]
    fn aggregate_collect_list_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["CollLbl"], [("v", Value::Int64(3))])
            .expect("a");
        store
            .insert_vertex_named(["CollLbl"], [("v", Value::Int64(1))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("CollLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::Collect,
                    expr: Some(prop("n", "v")),
                    expr2: None,
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: Some("xs".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(
                    Expr::new(ExprKind::Aggregate {
                        func: AggregateFunc::Collect,
                        expr: Some(Box::new(prop("n", "v"))),
                        expr2: None,
                        distinct: false,
                        order_by: None,
                        filter: None,
                    }),
                    "xs",
                )],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("collect");
        match result.rows[0].get("xs") {
            Some(Value::List(xs)) => {
                assert_eq!(xs.len(), 2);
            }
            other => panic!("expected list: {other:?}"),
        }
    }

    #[test]
    fn aggregate_percentile_cont_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PctLbl"], [("v", Value::Int64(10))])
            .expect("a");
        store
            .insert_vertex_named(["PctLbl"], [("v", Value::Int64(30))])
            .expect("b");
        let p = Expr::new(ExprKind::Literal(Value::Float64(0.5)));
        let agg = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::PercentileCont,
            expr: Some(Box::new(prop("n", "v"))),
            expr2: Some(Box::new(p.clone())),
            distinct: false,
            order_by: None,
            filter: None,
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("PctLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::PercentileCont,
                    expr: Some(prop("n", "v")),
                    expr2: Some(p),
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: Some("m".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(agg, "m")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("pct");
        match result.rows[0].get("m") {
            Some(Value::Float64(f)) => assert!((f - 20.0).abs() < 1e-9),
            other => panic!("expected float median: {other:?}"),
        }
    }

    #[test]
    fn aggregate_sum_with_expr2_is_rejected() {
        let store = GraphStore::new();
        let plan = plan(vec![PlanOp::Aggregate {
            group_by: Vec::new(),
            aggregates: vec![AggregateSpec {
                func: AggregateFunc::Sum,
                expr: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                expr2: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                distinct: false,
                filter: None,
                order_by: None,
                alias: None,
            }],
        }]);
        let err = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect_err("sum with expr2");
        assert!(
            matches!(err, PlanQueryError::UnsupportedOp(name) if name == "Aggregate.expr2"),
            "{err:?}"
        );
    }

    #[test]
    fn executes_planner_match_return_count_star() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerAggCntLbl"], Vec::<(&str, Value)>::new())
            .expect("vertex");
        let plan = plan_gql("MATCH (n:PlannerAggCntLbl) RETURN count(*) AS c");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("planner aggregate");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_planner_match_return_count_star_plus_literal() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerAggPlus"], Vec::<(&str, Value)>::new())
            .expect("v1");
        store
            .insert_vertex_named(["PlannerAggPlus"], Vec::<(&str, Value)>::new())
            .expect("v2");
        let plan = plan_gql("MATCH (n:PlannerAggPlus) RETURN count(*) + 1 AS c");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("nested aggregate expr");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(3)));
    }

    #[test]
    fn executes_planner_avg_nested_in_arithmetic() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggAvgArith"], [("x", Value::Int64(10))]);
        let _ = store.insert_vertex_named(["PlannerAggAvgArith"], [("x", Value::Int64(30))]);
        let plan = plan_gql("MATCH (n:PlannerAggAvgArith) RETURN avg(n.x) * 2 AS doubled");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("avg * 2");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("doubled") {
            Some(Value::Float64(f)) => assert!((f - 40.0).abs() < 1e-6),
            Some(Value::Int64(i)) => assert_eq!(*i, 40),
            other => panic!("expected numeric doubled: {other:?}"),
        }
    }

    #[test]
    fn executes_planner_group_by_having_count_filter() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(2))]);
        let plan = plan_gql(
            "MATCH (n:PlannerHavingK) RETURN n.k, count(*) AS cnt GROUP BY n.k HAVING count(*) > 1",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("having");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n.k"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }

    #[test]
    fn executes_planner_group_by_having_count_return_alias() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(2))]);
        let plan = plan_gql(
            "MATCH (n:PlannerHavingK) RETURN n.k, count(*) AS cnt GROUP BY n.k HAVING cnt > 1",
        );
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("having with return alias");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n.k"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }

    #[test]
    fn executes_planner_collect_list_names() {
        let store = GraphStore::new();
        let _ =
            store.insert_vertex_named(["PlannerAggCollect"], [("name", Value::Text("a".into()))]);
        let _ =
            store.insert_vertex_named(["PlannerAggCollect"], [("name", Value::Text("b".into()))]);
        let plan = plan_gql("MATCH (n:PlannerAggCollect) RETURN COLLECT_LIST(n.name) AS names");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("collect_list");
        assert_eq!(result.rows.len(), 1);
        let list = result.rows[0].get("names").expect("names column");
        let Value::List(items) = list else {
            panic!("expected list, got {list:?}");
        };
        assert_eq!(items.len(), 2);
        let mut texts: Vec<String> = items
            .iter()
            .map(|v| match v {
                Value::Text(t) => t.clone(),
                _ => panic!("expected text in list: {v:?}"),
            })
            .collect();
        texts.sort();
        assert_eq!(texts, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn executes_planner_stddev_pop_two_values() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggStd"], [("v", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerAggStd"], [("v", Value::Int64(3))]);
        let plan = plan_gql("MATCH (n:PlannerAggStd) RETURN STDDEV_POP(n.v) AS s");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("stddev_pop");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("s") {
            Some(Value::Float64(f)) => assert!((f - 1.0).abs() < 1e-6),
            other => panic!("expected float stddev: {other:?}"),
        }
    }

    #[test]
    fn executes_planner_percentile_cont_planned() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(10))]);
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(20))]);
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(30))]);
        let plan = plan_gql("MATCH (n:PlannerAggPct) RETURN PERCENTILE_CONT(n.v, 0.5) AS m");
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("percentile");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("m") {
            Some(Value::Float64(f)) => assert!((f - 20.0).abs() < 1e-6),
            other => panic!("expected float median: {other:?}"),
        }
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
        assert_eq!(vertex_id.shard_id, 7);
        assert_eq!(vertex_id.vertex_id, a);
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
        assert_eq!(vertex_path_id(&elements[0]).shard_id, 7);
        assert_eq!(vertex_path_id(&elements[0]).vertex_id, a);
        assert_eq!(
            edge_path_id(&elements[1]).owner_vertex_id,
            ab.owner_vertex_id
        );
        assert_eq!(
            edge_path_id(&elements[1]).edge_slot_index,
            EdgeSlotIndex::from_raw(ab.slot_index)
        );
        assert_eq!(vertex_path_id(&elements[2]).vertex_id, b);
        assert_eq!(
            edge_path_id(&elements[3]).owner_vertex_id,
            bc.owner_vertex_id
        );
        assert_eq!(
            edge_path_id(&elements[3]).edge_slot_index,
            EdgeSlotIndex::from_raw(bc.slot_index)
        );
        assert_eq!(vertex_path_id(&elements[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&elements[0]).shard_id, 0);
        assert_eq!(vertex_path_id(&elements[0]).vertex_id, a);
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
        let middle_vertices: BTreeSet<VertexId> = result
            .rows
            .iter()
            .map(|row| match row.get("p") {
                Some(Value::Path(elements)) => vertex_path_id(&elements[2]).vertex_id,
                other => panic!("expected path, got {other:?}"),
            })
            .collect();
        assert_eq!(middle_vertices, BTreeSet::from([b1, b2]));
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
    fn unsupported_operator_returns_stable_error() {
        let store = GraphStore::new();
        let cases = vec![
            (
                PlanOp::EdgeIndexScan {
                    variable: "e".into(),
                    property: "w".into(),
                    value: ScanValue::Literal(Value::Int64(1)),
                    property_projection: None,
                },
                "EdgeIndexScan",
            ),
            (
                PlanOp::SetOperation {
                    op: SetOp::Union,
                    right: Box::new(plan(Vec::new())),
                },
                "SetOperation",
            ),
            (
                PlanOp::CallProcedure {
                    name: vec!["db".into(), "labels".into()],
                    args: Vec::new(),
                    yield_columns: None,
                    optional: false,
                },
                "CallProcedure",
            ),
            (
                PlanOp::WorstCaseOptimalJoin {
                    variables: Vec::new(),
                    edges: Vec::<WcojEdge>::new(),
                },
                "WorstCaseOptimalJoin",
            ),
        ];

        for (op, expected_name) in cases {
            let plan = plan(vec![op]);
            let err = store
                .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
                .expect_err("operator should be unsupported in v1");

            assert!(
                matches!(err, PlanQueryError::UnsupportedOp(name) if name == expected_name),
                "expected UnsupportedOp({expected_name}), got {err:?}"
            );
        }
    }

    fn catalog_edge_label(store: &GraphStore, label_name: &str) -> EdgeLabelId {
        store.edge_label_id(label_name).expect("edge label")
    }

    fn setup_weighted_road_graph(store: &GraphStore) -> (VertexId, VertexId, VertexId) {
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
            .set_edge_label_weight_profile(
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

    fn gleaph_weight_call(edge_var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]),
            args: vec![Expr::var(edge_var)],
            distinct: false,
        })
    }

    fn scaled_gleaph_weight_cost(edge_var: &str, scale_param: &str) -> Expr {
        Expr::new(ExprKind::BinaryOp {
            left: Box::new(gleaph_weight_call(edge_var)),
            op: BinaryOp::Mul,
            right: Box::new(Expr::new(ExprKind::Parameter(scale_param.to_owned()))),
        })
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
        assert_eq!(vertex_path_id(&elements[4]).vertex_id, c);
    }

    fn weighted_shortest_plan_with_cost(cost: Expr) -> PhysicalPlan {
        weighted_shortest_plan_with_cost_mode(cost, ShortestMode::AnyShortest)
    }

    fn weighted_shortest_plan_with_cost_mode(cost: Expr, mode: ShortestMode) -> PhysicalPlan {
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

    fn weighted_2_24_precision_cost_expr() -> Expr {
        use gleaph_gql::ast::WhenClause;
        use gleaph_gql::token::Span;
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

    fn cast_expr_to_float32(expr: Expr) -> Expr {
        Expr::new(ExprKind::Cast {
            expr: Box::new(expr),
            target: gleaph_gql::ast::ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        })
    }

    fn weighted_2_24_precision_cost_expr_float32() -> Expr {
        use gleaph_gql::ast::WhenClause;
        use gleaph_gql::token::Span;
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

    fn weighted_decimal_precision_cost_expr() -> Expr {
        use gleaph_gql::ast::WhenClause;
        use gleaph_gql::token::Span;
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

    fn weighted_wide_integer_precision_cost_expr() -> Expr {
        use gleaph_gql::ast::WhenClause;
        use gleaph_gql::token::Span;
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        let middle_vertices: BTreeSet<VertexId> = result
            .rows
            .iter()
            .map(|row| match row.get("p") {
                Some(Value::Path(elements)) => vertex_path_id(&elements[2]).vertex_id,
                other => panic!("expected path, got {other:?}"),
            })
            .collect();
        assert_eq!(middle_vertices, BTreeSet::from([b1, b2]));
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&path_column(&result, "p")[4]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&elements[4]).vertex_id, c);
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
            .set_edge_label_weight_profile(
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
        assert_eq!(vertex_path_id(&elements[0]).vertex_id, s);
        assert_eq!(vertex_path_id(&elements[4]).vertex_id, dst);
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
            .set_edge_label_weight_profile(
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
    fn weighted_shortest_skips_stale_higher_cost_vertex_entries() {
        let store = GraphStore::new();
        let (s, dst) = setup_stale_mid_diamond_graph(&store);
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
        assert_eq!(vertex_path_id(&elements[6]).vertex_id, dst);
        assert_eq!(vertex_path_id(&elements[0]).vertex_id, s);
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
            .set_edge_label_weight_profile(
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
        assert_eq!(vertex_path_id(&elements[elements.len() - 1]).vertex_id, c);
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
        assert_eq!(vertex_path_id(&elements[2]).vertex_id, c);
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
        store
            .insert_directed_edge_with_inline_value(a, c, Some(road), 1)
            .expect("edge");
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
