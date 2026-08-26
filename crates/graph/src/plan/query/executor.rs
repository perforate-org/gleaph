mod bindings;
mod context;
mod eval;
mod expand;
mod for_loop;
mod join;
mod ops;
mod path;
mod scan;
mod set_operation;
mod wcoj;

pub(crate) use super::sort_keys::compare_sort_keys;
pub use bindings::EdgeBinding;
pub(crate) use eval::{
    binding_to_value, eval_sort_expr, project_row, try_read_inline_edge_property,
    validate_and_decode_inline_struct, value_row,
};
pub(crate) use ops::execute_ops_from;
pub use path::PathBinding;
pub(crate) use path::path_binding_to_value;
pub(crate) use scan::resolve_edge_equality_probe_payloads;

/// The parser stores parameter names with the `$` sigil (`Parameter("$e")`), while the wire
/// params map is keyed by the bare name — the Router convention shared by `seed.rs` /
/// `aggregate_index_fast_path.rs` / `uniqueness.rs` (which strip the sigil before lookup). Strip
/// it here so graph-side parameter resolution agrees with the Router.
pub(crate) fn param_map_key(name: &str) -> &str {
    name.strip_prefix('$').unwrap_or(name)
}

#[cfg(test)]
pub(crate) use expand::edge_binding_for_expand;

use super::error::PlanQueryError;
use super::sort_keys::compare_sort_values;
use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::expr_evaluator::truthy;
use candid::Principal;
use context::{ExecuteCtx, QueryExprEvaluator};
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, ObjectName, OrderByClause, SortDirection};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::OutputSchema;
use gleaph_gql_planner::collect_expr_variables;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, Str};
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId};
use gleaph_graph_kernel::gql_dialect::INSERTION;
use gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::OutEdgeOrder;
use std::collections::BTreeMap;

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
        element_id_key: &ElementIdEncodingKey,
        rows: &[PlanQueryRow],
    ) -> Result<Self, PlanQueryError> {
        Ok(Self {
            rows: materialize_plan_rows(store, element_id_key, rows)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanBinding {
    Vertex(VertexId),
    /// Neighbor bound via a shard-local remote ref (logical id only on this shard).
    RemoteVertex(GlobalVertexId),
    Edge(EdgeBinding),
    /// Edges along a variable-length expand (`{min,max}` quantifier), in hop order.
    EdgeGroup(std::sync::Arc<[EdgeBinding]>),
    /// Vertices along a variable-length expand hop sequence (near or far group), in hop order.
    VertexGroup(std::sync::Arc<[VertexId]>),
    Value(Value),
    /// Shortest-path walk materialized to [`Value::Path`] only in [`binding_to_value`] / expression eval.
    Path(PathBinding),
    /// Up to k shortest paths on one row (`SHORTEST k GROUP`).
    PathGroup(std::sync::Arc<[PathBinding]>),
}

pub use super::row::{PlanQueryRow, PlanRow};

/// Execute a read plan through [`execute_ops`] and return binding rows (paths stay
/// [`PlanBinding::Path`] until [`materialize_plan_rows`] or [`PlanQueryResult::try_from_plan_rows`]).
pub async fn execute_plan_query_bindings(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let initial_rows = super::arena::QueryArena::with(|arena| {
        arena.reset();
        vec![super::row::empty_row_for_plan_with_arena(plan, arena)]
    });
    execute_plan_query_bindings_with_initial_rows(
        store,
        plan,
        parameters,
        index,
        execution,
        initial_rows,
        false,
    )
    .await
}

/// Like [`execute_plan_query_bindings`] but starts from `initial_rows` and may skip the
/// leading index anchor op ([`PlanOp::IndexScan`], [`PlanOp::IndexIntersection`], or labeled
/// [`PlanOp::NodeScan`]) when the router supplied seed bindings.
pub async fn execute_plan_query_bindings_with_initial_rows(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    initial_rows: Vec<PlanRow>,
    skip_leading_index_scan: bool,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    // The router-issued element-id key is carried as owned data in the per-op evaluator
    // (`QueryExprEvaluator::element_id_key`), never parked in ambient thread-local state across the
    // `await`s in `execute_ops_from`. Materialization resolves the same key explicitly from
    // `execution` (see `execute_plan_query`).
    execute_plan_query_bindings_with_initial_rows_inner(
        store,
        plan,
        parameters,
        index,
        execution,
        initial_rows,
        skip_leading_index_scan,
    )
    .await
}

async fn execute_plan_query_bindings_with_initial_rows_inner(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    initial_rows: Vec<PlanRow>,
    skip_leading_index_scan: bool,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let ops = if skip_leading_index_scan {
        skip_leading_index_anchor_ops(&plan.ops)
    } else {
        plan.ops.as_slice()
    };
    let ctx = ExecuteCtx::new(store, parameters, index, execution);
    super::arena::QueryArena::with(|arena| arena.reset());
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("plan_query_execute_ops");
    execute_ops_from(&ctx, ops, initial_rows).await
}

fn is_router_seed_skippable_op(op: &PlanOp) -> bool {
    // Only the leading *scan/index anchor* is represented by the router-supplied seed
    // bindings, so only those ops may be skipped. A residual `PropertyFilter` is NOT an
    // anchor: the router resolves seeds from the index/label anchor alone and cannot apply
    // non-indexed (or otherwise residual) equality predicates when building seeds. The shard
    // must always re-apply such filters against the seed rows; skipping them would return the
    // unfiltered anchor set (e.g. all label vertices), so `PropertyFilter` is excluded here.
    matches!(
        op,
        PlanOp::NodeScan { label: Some(_), .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::EdgeIndexScan { .. }
    )
}

fn skip_leading_index_anchor_ops(ops: &[PlanOp]) -> &[PlanOp] {
    let skip = ops
        .iter()
        .take_while(|op| is_router_seed_skippable_op(op))
        .count();
    &ops[skip..]
}

pub fn materialize_plan_rows(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    rows: &[PlanRow],
) -> Result<Vec<BTreeMap<String, Value>>, PlanQueryError> {
    super::materialize::materialize_plan_rows(store, element_id_key, rows, &OutputSchema::default())
}

pub fn materialize_plan_rows_for_schema(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    rows: &[PlanRow],
    schema: &OutputSchema,
) -> Result<Vec<BTreeMap<String, Value>>, PlanQueryError> {
    super::materialize::materialize_plan_rows(store, element_id_key, rows, schema)
}

pub async fn execute_plan_query(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
) -> Result<PlanQueryResult, PlanQueryError> {
    let element_id_key =
        crate::element_id_encoding::resolve_or_host_fixture(execution.element_id_encoding_key());
    let bindings = execute_plan_query_bindings(store, plan, parameters, index, execution).await?;
    super::materialize::hydrate_plan_rows(
        store,
        &element_id_key,
        &super::materialize::PlanQueryBindings { rows: bindings },
        &plan.output,
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EdgeSequenceOrder {
    #[default]
    Ascending,
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

pub(crate) fn vertex_binding_for_projection(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    vertex_id: VertexId,
    property_projection: Option<&[Str]>,
) -> Result<PlanBinding, PlanQueryError> {
    match property_projection {
        None | Some([]) => Ok(PlanBinding::Vertex(vertex_id)),
        Some(props) => Ok(PlanBinding::Value(vertex_to_projected_record(
            store, execution, vertex_id, props,
        )?)),
    }
}

fn vertex_to_projected_record(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    vertex_id: VertexId,
    properties: &[Str],
) -> Result<Value, PlanQueryError> {
    let mut fields = Vec::with_capacity(properties.len());
    for property in properties {
        let value = execution
            .resolved_property_id(property.as_ref())
            .and_then(|property_id| store.vertex_property(vertex_id, property_id))
            .unwrap_or(Value::Null);
        fields.push((property.to_string(), value));
    }
    Ok(Value::Record(fields))
}

pub(crate) fn edge_to_projected_record(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    binding: EdgeBinding,
    properties: &[Str],
) -> Result<Value, PlanQueryError> {
    let mut fields = Vec::with_capacity(properties.len());
    for property in properties {
        let value = if let Some(property_id) = execution.resolved_property_id(property.as_ref()) {
            if let Some(value) = try_read_inline_edge_property(
                &binding,
                property_id,
                execution.resolved_labels.as_ref(),
            )? {
                value
            } else {
                store
                    .edge_property_at_canonical_handle(
                        binding.resolved_canonical_handle(store)?,
                        property_id,
                    )
                    .unwrap_or(Value::Null)
            }
        } else {
            Value::Null
        };
        fields.push((property.to_string(), value));
    }
    Ok(Value::Record(fields))
}

pub(crate) fn dst_filter_is_dst_vertex_only(dst_filter: &[Expr], dst: &str) -> bool {
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn vertex_row_matches_dst_filters(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    parameters: &BTreeMap<String, Value>,
    dst: &Str,
    dst_id: VertexId,
    dst_filter: &[Expr],
    caller: Option<Principal>,
    execution: &GqlExecutionContext,
) -> Result<bool, PlanQueryError> {
    let mut stub = PlanRow::new();
    stub.insert(dst.to_string(), PlanBinding::Vertex(dst_id));
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs: None,
        caller,
        resolved_labels: execution.resolved_labels.as_ref(),
        resolved_properties: execution.resolved_properties.as_ref(),
        element_id_key: *element_id_key,
    };
    row_matches_all(&evaluator, &stub, dst_filter)
}

pub(crate) fn ensure_simple_expand(
    var_len: &Option<gleaph_gql_planner::plan::VarLenSpec>,
) -> Result<(), PlanQueryError> {
    if var_len.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.var_len"));
    }
    Ok(())
}

pub(crate) fn ensure_var_len_expand() -> Result<(), PlanQueryError> {
    Ok(())
}

pub(crate) fn row_matches_all(
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

pub(crate) fn sort_rows(
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

/// TopK over input verified ascending by its single sort key (ADR 0081 §3).
///
/// Survivors are the first `offset + k` rows of the ascending stream, and consumption
/// stops at the first tuple whose key is **strictly greater** than the boundary
/// survivor's key: equal keys keep being examined until their tie group closes, because
/// GQL leaves which tied rows survive unspecified while output must stay deterministic.
/// Residual filters between the ordered scan and this site only thin an ascending
/// stream, so the boundary rule remains valid with filters present. Keys are evaluated
/// lazily so the strict-greater stop skips evaluating the whole tail.
pub(crate) fn topk_ordered_input(
    evaluator: &QueryExprEvaluator<'_>,
    rows: Vec<PlanRow>,
    order_by: &OrderByClause,
    k: usize,
    offset: usize,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let cap = offset.saturating_add(k);
    if cap == 0 || k == 0 {
        return Ok(Vec::new());
    }
    let [item] = order_by.items.as_slice() else {
        return Err(PlanQueryError::UnsupportedOp(
            "TopK(ordered input requires exactly one sort item)",
        ));
    };

    let mut survivors: Vec<(Value, PlanRow)> = Vec::with_capacity(cap.min(rows.len()));
    let mut boundary: Option<Value> = None;
    // Monotonicity guard: the delivered-order contract says every accepted key sorts
    // at or after its predecessor. The first violation fails safe into a full sort
    // over everything seen instead of trusting a possibly unsorted prefix.
    let mut prev_accepted: Option<Value> = None;
    let mut unordered_tail: Vec<PlanRow> = Vec::new();

    for row in rows {
        if !unordered_tail.is_empty() {
            unordered_tail.push(row);
            continue;
        }
        let key = eval_sort_expr(evaluator, &row, &item.expr)?;
        if let Some(prev) = &prev_accepted
            && compare_sort_values(&key, prev, item.direction, item.null_order)?
                == std::cmp::Ordering::Less
        {
            unordered_tail.push(row);
            continue;
        }
        prev_accepted = Some(key.clone());
        let Some(boundary_key) = &boundary else {
            survivors.push((key, row));
            if survivors.len() == cap {
                // Monotone input: the last accepted row carries the largest key.
                boundary = survivors.last().map(|(key, _)| key.clone());
            }
            continue;
        };
        match compare_sort_values(&key, boundary_key, item.direction, item.null_order)? {
            // Strictly greater than the boundary survivor: the tie group has closed
            // and every later row can only be larger, so stop consuming.
            std::cmp::Ordering::Greater => break,
            // Still inside the closing tie group; the earliest tied survivors stand
            // (deterministic under GQL's unspecified tie resolution).
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Less => {
                // Unreachable under the monotonicity guard above; kept fail-safe.
                unordered_tail.push(row);
            }
        }
    }

    if unordered_tail.is_empty() {
        Ok(survivors
            .into_iter()
            .map(|(_, row)| row)
            .skip(offset)
            .collect())
    } else {
        let mut all: Vec<PlanRow> = survivors.into_iter().map(|(_, row)| row).collect();
        all.extend(unordered_tail);
        Ok(sort_rows(evaluator, all, order_by)?
            .into_iter()
            .skip(offset)
            .take(k)
            .collect())
    }
}

pub(crate) fn insertion_order_after_expand(
    ops: &[PlanOp],
    expand_idx: usize,
    edge_var: &str,
    execution: &GqlExecutionContext,
    fixed_label: Option<&str>,
) -> Result<EdgeSequenceOrder, PlanQueryError> {
    for op in &ops[expand_idx + 1..] {
        match op {
            PlanOp::Sort { order_by } | PlanOp::TopK { order_by, .. } => {
                let Some((sort_edge_var, order)) = insertion_order_sort(order_by) else {
                    continue;
                };
                if sort_edge_var != edge_var {
                    continue;
                }
                let Some(label) = fixed_label else {
                    return Err(PlanQueryError::InsertionOrder {
                        message: format!(
                            "ORDER BY INSERTION({sort_edge_var}) requires a single fixed edge label"
                        ),
                    });
                };
                // ADR 0052 §3: the capability must be declared in the Graph Type. A label whose
                // resolved policy is not `Insertion` (including a label absent from the wire
                // table) fails closed instead of silently using physical slot order.
                if !matches!(
                    execution.resolved_edge_label_ordering(label),
                    Some(EdgeOrderingPolicy::Insertion)
                ) {
                    return Err(PlanQueryError::InsertionOrder {
                        message: format!(
                            "ORDER BY INSERTION({sort_edge_var}) requires edge label '{label}' to be declared ORDER BY INSERTION in its Graph Type"
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
            PlanOp::OptionalMatch { sub_plan } if ops_bind_edge_variable(sub_plan, edge_var) => {
                break;
            }
            PlanOp::Aggregate { .. }
            | PlanOp::ShortestPath { .. }
            | PlanOp::HashJoin { .. }
            | PlanOp::CartesianProduct { .. }
            | PlanOp::Materialize { .. } => break,
            _ => {}
        }
    }
    Ok(EdgeSequenceOrder::Ascending)
}

fn previous_op_binds_edge(ops: &[PlanOp], op_idx: usize, edge_var: &str) -> bool {
    for op in ops[..op_idx].iter().rev() {
        match op {
            PlanOp::Expand { edge, .. } | PlanOp::ExpandFilter { edge, .. } => {
                if edge.as_ref() == edge_var {
                    return true;
                }
                // A different edge variable (or an unnamed edge) does not invalidate
                // earlier bindings; keep scanning backwards for the target variable.
            }
            PlanOp::OptionalMatch { sub_plan } if ops_bind_edge_variable(sub_plan, edge_var) => {
                return false;
            }
            PlanOp::Aggregate { .. }
            | PlanOp::ShortestPath { .. }
            | PlanOp::HashJoin { .. }
            | PlanOp::CartesianProduct { .. }
            | PlanOp::Materialize { .. } => return false,
            _ => {}
        }
    }
    false
}

/// Whether an operation sequence can overwrite a named edge binding from an outer row.
///
/// `OptionalMatch` executes its sub-plan against each input row and preserves that input row on a
/// miss. It therefore does not invalidate an outer edge binding unless its sub-plan itself binds
/// the same name. Insertion-order lowering uses this distinction so `ORDER BY INSERTION(e)` remains
/// Graph-owned ordering rather than falling through to generic expression evaluation.
fn ops_bind_edge_variable(ops: &[PlanOp], edge_var: &str) -> bool {
    ops.iter().any(|op| op_binds_edge_variable(op, edge_var))
}

fn op_binds_edge_variable(op: &PlanOp, edge_var: &str) -> bool {
    match op {
        PlanOp::Expand {
            edge,
            emit_edge_binding,
            ..
        }
        | PlanOp::ExpandFilter {
            edge,
            emit_edge_binding,
            ..
        }
        | PlanOp::ShortestPath {
            edge,
            emit_edge_binding,
            ..
        } => *emit_edge_binding && edge.as_ref() == edge_var,
        PlanOp::EdgeIndexScan { variable, .. } => variable.as_ref() == edge_var,
        PlanOp::WorstCaseOptimalJoin { edges, .. } => {
            edges.iter().any(|edge| edge.variable.as_ref() == edge_var)
        }
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => ops_bind_edge_variable(sub_plan, edge_var),
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            ops_bind_edge_variable(left, edge_var) || ops_bind_edge_variable(right, edge_var)
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            ops_bind_edge_variable(&sub_plan.ops, edge_var)
        }
        PlanOp::SetOperation { right, .. } => ops_bind_edge_variable(&right.ops, edge_var),
        _ => false,
    }
}

pub(crate) fn insertion_order_sort(
    order_by: &OrderByClause,
) -> Option<(String, EdgeSequenceOrder)> {
    let [item] = order_by.items.as_slice() else {
        return None;
    };
    if item.null_order.is_some() {
        return None;
    }
    let order = match item.direction {
        Some(SortDirection::Asc | SortDirection::Ascending) => EdgeSequenceOrder::Ascending,
        Some(SortDirection::Desc | SortDirection::Descending) => EdgeSequenceOrder::Descending,
        None => EdgeSequenceOrder::Ascending,
    };
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &item.expr.kind
    else {
        return None;
    };
    if !is_insertion_call(name, *distinct) || args.len() != 1 {
        return None;
    }
    sequence_arg_edge_var(&args[0]).map(|edge_var| (edge_var, order))
}

fn is_insertion_call(name: &ObjectName, distinct: bool) -> bool {
    !distinct && INSERTION.matches_ascii_case_insensitive(&name.parts)
}

fn sequence_arg_edge_var(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Paren(inner) => sequence_arg_edge_var(inner),
        ExprKind::Variable(v) => Some(v.clone()),
        _ => None,
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
pub(crate) async fn vertex_binding_for_traversal(
    store: &GraphStore,
    row: &PlanRow,
    variable: &str,
    expand_direction: Option<EdgeDirection>,
) -> Result<Option<VertexId>, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Value(Value::Null)) => Ok(None),
        Some(PlanBinding::RemoteVertex(logical)) => {
            resolve_federated_traversal_vertex(store, *logical, expand_direction).await
        }
        _ => vertex_binding(row, variable).map(Some),
    }
}

/// Maps a logical vertex to a local [`VertexId`] when this shard is authoritative.
pub(crate) async fn resolve_federated_traversal_vertex(
    store: &GraphStore,
    vertex_id: GlobalVertexId,
    expand_direction: Option<EdgeDirection>,
) -> Result<Option<VertexId>, PlanQueryError> {
    let Some(routing) = store.federation_routing() else {
        return Err(PlanQueryError::UnsupportedOp(
            "Expand(remote vertex requires federation routing)",
        ));
    };
    if vertex_id.shard_id != routing.shard_id {
        let op = match expand_direction {
            Some(EdgeDirection::PointingLeft) => {
                "Expand.reverse(federated placement on another shard)"
            }
            Some(EdgeDirection::Undirected) => {
                "Expand.undirected(federated placement on another shard)"
            }
            _ => "Expand.forward(federated placement on another shard)",
        };
        return Err(PlanQueryError::UnsupportedOp(op));
    }
    Ok(store.resolve_local_vertex(vertex_id))
}

pub(crate) fn limit_value(value: &Value) -> Result<usize, PlanQueryError> {
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

pub(crate) fn dedup_rows(rows: &mut Vec<PlanRow>) {
    let mut unique = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !unique.contains(&row) {
            unique.push(row);
        }
    }
    *rows = unique;
}

pub(crate) fn plan_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::NodeScan { .. } => "NodeScan",
        PlanOp::IndexScan { .. } => "IndexScan",
        // Router-owned seed: plans dispatched to a graph shard never contain a
        // TextScan (the Router lowers it to canister search + bound rows before
        // wire transport), so if one ever arrives here the shared catch-all
        // rejects it as `UnsupportedOp("TextScan")`.
        PlanOp::TextScan { .. } => "TextScan",
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
        PlanOp::Search { .. } => "Search",
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
        PlanOp::SemiApply { .. } => "SemiApply",
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
mod test_support;

#[cfg(test)]
mod tests_integration;
