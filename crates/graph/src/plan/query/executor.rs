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

pub use bindings::EdgeBinding;
pub(crate) use eval::{binding_to_value, eval_sort_expr, project_row, value_row};
pub(crate) use ops::execute_ops_from;
pub use path::PathBinding;
pub(crate) use path::path_binding_to_value;
pub(crate) use scan::{federation_routing, resolve_scan_payload_bytes};

#[cfg(test)]
pub(crate) use expand::edge_binding_for_expand;

use super::error::PlanQueryError;
use super::sort_keys::compare_sort_keys;
use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;
use crate::plan::expr_evaluator::truthy;
use candid::Principal;
use context::{ExecuteCtx, QueryExprEvaluator};
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, ObjectName, OrderByClause, SortDirection};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::OutputSchema;
use gleaph_gql_planner::collect_expr_variables;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp, Str};
use gleaph_graph_kernel::entry::PreparedWeightDecoder;
use gleaph_graph_kernel::federation::GlobalVertexId;
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
        rows: &[PlanQueryRow],
    ) -> Result<Self, PlanQueryError> {
        Ok(Self {
            rows: materialize_plan_rows(store, rows)?,
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
    let ops = if skip_leading_index_scan {
        skip_leading_index_anchor_ops(&plan.ops)
    } else {
        plan.ops.as_slice()
    };
    let gleaph_weight_decoders = {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = bench_scope("plan_query_prepare_gleaph_weight");
        super::gleaph_weight::prepare_gleaph_weight_decoders(store, &execution, ops)?
    };
    let ctx = ExecuteCtx::new(
        store,
        parameters,
        index,
        execution,
        gleaph_weight_decoders.as_ref(),
    );
    super::arena::QueryArena::with(|arena| arena.reset());
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let _scope = bench_scope("plan_query_execute_ops");
    execute_ops_from(&ctx, ops, initial_rows).await
}

fn is_router_seed_skippable_op(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::NodeScan { label: Some(_), .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::PropertyFilter { .. }
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
    rows: &[PlanRow],
) -> Result<Vec<BTreeMap<String, Value>>, PlanQueryError> {
    super::materialize::materialize_plan_rows(store, rows, &OutputSchema::default())
}

pub fn materialize_plan_rows_for_schema(
    store: &GraphStore,
    rows: &[PlanRow],
    schema: &OutputSchema,
) -> Result<Vec<BTreeMap<String, Value>>, PlanQueryError> {
    super::materialize::materialize_plan_rows(store, rows, schema)
}

pub async fn execute_plan_query(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
) -> Result<PlanQueryResult, PlanQueryError> {
    let bindings = execute_plan_query_bindings(store, plan, parameters, index, execution).await?;
    super::materialize::hydrate_plan_rows(
        store,
        &super::materialize::PlanQueryBindings { rows: bindings },
        &plan.output,
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EdgeSequenceOrder {
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
        let value = execution
            .resolved_property_id(property.as_ref())
            .and_then(|property_id| store.edge_property(binding.handle, property_id))
            .unwrap_or(Value::Null);
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

pub(crate) fn vertex_row_matches_dst_filters(
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
        resolved_labels: None,
        resolved_properties: None,
        gleaph_weight_decoders,
    };
    row_matches_all(&evaluator, &stub, dst_filter)
}

pub(crate) fn ensure_simple_expand(
    _label_expr: &Option<LabelExpr>,
    var_len: &Option<gleaph_gql_planner::plan::VarLenSpec>,
    _hop_aux_binding: &Option<Str>,
) -> Result<(), PlanQueryError> {
    if var_len.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.var_len"));
    }
    Ok(())
}

pub(crate) fn ensure_var_len_expand(
    _label_expr: &Option<LabelExpr>,
    _hop_aux_binding: &Option<Str>,
    _indexed_edge_equality: &Option<(Str, gleaph_gql_planner::plan::ScanValue)>,
    _edge_payload_predicate: &Option<gleaph_gql_planner::plan::EdgePayloadPredicate>,
    _edge_vector_predicate: &Option<gleaph_gql_planner::plan::EdgeVectorPredicate>,
    _edge_property_projection: &Option<std::rc::Rc<[Str]>>,
) -> Result<(), PlanQueryError> {
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

pub(crate) fn gleaph_sequence_order_after_expand(
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

pub(crate) fn gleaph_sequence_sort(
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
    let placement = placement::resolve_placement(routing.router_canister, vertex_id)
        .await
        .map_err(|_| PlanQueryError::UnsupportedOp("Expand(remote placement lookup)"))?;
    match placement {
        gleaph_graph_kernel::federation::VertexPlacement::Active(loc)
            if loc.shard_id == routing.shard_id =>
        {
            Ok(Some(VertexId::from(loc.local_vertex_id)))
        }
        gleaph_graph_kernel::federation::VertexPlacement::Active(_) => {
            let op = match expand_direction {
                Some(EdgeDirection::PointingLeft) => {
                    "Expand.reverse(federated placement on another shard)"
                }
                Some(EdgeDirection::Undirected) => {
                    "Expand.undirected(federated placement on another shard)"
                }
                _ => "Expand.forward(federated placement on another shard)",
            };
            Err(PlanQueryError::UnsupportedOp(op))
        }
    }
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
mod test_support;

#[cfg(test)]
mod tests_integration;
