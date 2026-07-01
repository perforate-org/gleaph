use super::error::PlanMutationError;
use super::expr_evaluator::{MutationPropertyExprEvaluation, MutationPropertyExprEvaluator};
use super::gleaph_finalize;
use crate::edge_payload_scalar_codec::encode_edge_payload_scalar;
use crate::facade::mutation_executor::{GraphMutationExecutor, insert_vertex_with_async};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use crate::gql_execution_context::GqlExecutionContext;
use crate::property::{ensure_persistable, ensure_property_id};
use gleaph_gql::Value;
use gleaph_gql::ast::ExprKind;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_ic::{UniqueKeyOutcome, encode_unique_value};
use gleaph_gql_planner::plan::{
    PhysicalPlan, PlanOp, ProjectColumn, RemovePlanItem, SetPlanItem, Str,
};
use gleaph_graph_kernel::entry::EdgePayloadProfile;
use gleaph_graph_kernel::entry::{ConstraintNameId, EdgeLabelId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::ElementIdEncodingKey;
use gleaph_graph_kernel::plan_exec::{ConstrainedPropertyDispatch, LabelStatsDelta};
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::OutEdgeOrder;
use ic_stable_lara::traits::CsrEdge;
use std::collections::{BTreeMap, BTreeSet};

pub trait PlanMutationExecutor {
    fn execute_plan_mutations(
        &self,
        plan: &PhysicalPlan,
        execution: GqlExecutionContext,
    ) -> Result<PlanMutationBindings, PlanMutationError>;
}

/// One constrained value a delete/remove frees, captured before the canonical write so the value
/// and the owning element id are still readable (ADR 0030 slice 5b). `encoded_value` is the
/// canonical key the Router reserved for the matching `Acquire`, so the Router's `Release`
/// reconciliation keys the same reservation and matches it by `owner_element_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUniqueRelease {
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
    pub owner_element_id: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanMutationBindings {
    pub vertices: BTreeMap<String, VertexId>,
    pub edges: BTreeMap<String, EdgeHandle>,
    pub label_stats_delta: LabelStatsDelta,
    /// `CALL ... YIELD` columns from Gleaph finalize procedures in this plan.
    pub procedure_yields: BTreeMap<String, Value>,
    /// Last scalar rows produced by projecting procedure yields.
    pub procedure_rows: Vec<BTreeMap<String, Value>>,
    /// Vertices created by `InsertVertex` ops in this segment, in execution order. Used by the
    /// cross-shard uniqueness `Acquire` emit (ADR 0030 slice 5) so an anonymous `CREATE (:L {..})`
    /// (no variable binding) still exposes its canonical element id.
    pub created_vertices: Vec<VertexId>,
    /// Constrained values freed by deletes/removes in this segment, in execution order, captured
    /// **before** the canonical delete so the property value and owner element id are still readable
    /// (ADR 0030 slice 5b). The `Release` emit (`emit_unique_releases`) pins one receipt per entry.
    pub released_unique_values: Vec<PendingUniqueRelease>,
    /// Like `released_unique_values` but for `ShardLocalGlobal` constraints (ADR 0030 slice 10):
    /// these are freed directly in the owning shard's local unique table (owner-matched) rather than
    /// pinned as outbox `Release` receipts.
    pub released_local_unique_values: Vec<PendingUniqueRelease>,
    forward_edge_insert_counts: BTreeMap<VertexId, u32>,
    /// Forward hubs from this DML batch (sources with enough edge inserts).
    pub hot_forward_vertices: Vec<VertexId>,
}

impl PlanMutationBindings {
    /// Test-only constructor: a bindings value carrying just the created-vertex list, used to
    /// exercise the ADR 0030 `Acquire` emit without running a full mutation segment (the
    /// `forward_edge_insert_counts` field is module-private, so external test modules cannot use a
    /// struct literal).
    #[cfg(test)]
    pub(crate) fn with_created_vertices_for_test(created_vertices: Vec<VertexId>) -> Self {
        Self {
            created_vertices,
            ..Default::default()
        }
    }

    /// Test-only constructor carrying just the captured release list, to exercise the ADR 0030
    /// `Release` emit without running a full delete segment.
    #[cfg(test)]
    pub(crate) fn with_released_unique_values_for_test(
        released_unique_values: Vec<PendingUniqueRelease>,
    ) -> Self {
        Self {
            released_unique_values,
            ..Default::default()
        }
    }

    fn add_vertex_label_delta(&mut self, label: VertexLabelId, delta: i64) {
        add_label_delta(&mut self.label_stats_delta.vertex, label, delta);
    }

    fn add_edge_label_delta(&mut self, label: EdgeLabelId, delta: i64) {
        add_label_delta(&mut self.label_stats_delta.edge, label, delta);
    }
}

impl PlanMutationExecutor for GraphStore {
    fn execute_plan_mutations(
        &self,
        plan: &PhysicalPlan,
        execution: GqlExecutionContext,
    ) -> Result<PlanMutationBindings, PlanMutationError> {
        execute_ops(self, &plan.ops, &BTreeMap::new(), execution)
    }
}

pub fn execute_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
) -> Result<PlanMutationBindings, PlanMutationError> {
    let mut bindings = PlanMutationBindings::default();
    execute_ops_with_bindings(store, ops, parameters, execution, &mut bindings)?;
    Ok(bindings)
}

fn execute_ops_with_bindings(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    let evaluator = MutationPropertyExprEvaluator::new(parameters, execution.caller);
    for op in ops {
        match op {
            PlanOp::InsertVertex {
                variable,
                labels,
                properties,
            } => {
                let properties = evaluator.resolve_assignments(properties)?;
                let label_ids = resolve_vertex_labels(&execution, labels)?;
                let property_ids = resolve_mutation_properties(&execution, properties)?;
                let unique_labels = label_ids.iter().copied().collect::<BTreeSet<_>>();
                let vertex_id = store.insert_vertex_with(label_ids, property_ids)?;
                for label_id in unique_labels {
                    bindings.add_vertex_label_delta(label_id, 1);
                }
                bindings.created_vertices.push(vertex_id);
                if let Some(variable) = variable {
                    bindings.vertices.insert(variable.to_string(), vertex_id);
                }
            }
            PlanOp::InsertEdge {
                variable,
                src,
                dst,
                direction,
                labels,
                properties,
            } => {
                let src_id = *bindings.vertices.get(src.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: src.to_string(),
                    }
                })?;
                let dst_id = *bindings.vertices.get(dst.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: dst.to_string(),
                    }
                })?;
                let properties = evaluator.resolve_assignments(properties)?;
                let resolved_label = resolve_edge_label(&execution, labels.first())?;
                let property_ids = resolve_mutation_properties(&execution, properties)?;
                let (inline_payload, sidecar_properties) =
                    classify_edge_assignments(&execution, resolved_label, property_ids)?;
                let handle = match direction {
                    EdgeDirection::PointingRight => insert_directed_edge_with_inline(
                        store,
                        src_id,
                        dst_id,
                        resolved_label,
                        inline_payload.as_ref(),
                        sidecar_properties,
                    )?,
                    EdgeDirection::PointingLeft => insert_directed_edge_with_inline(
                        store,
                        dst_id,
                        src_id,
                        resolved_label,
                        inline_payload.as_ref(),
                        sidecar_properties,
                    )?,
                    EdgeDirection::Undirected => insert_undirected_edge_with_inline(
                        store,
                        src_id,
                        dst_id,
                        resolved_label,
                        inline_payload.as_ref(),
                        sidecar_properties,
                    )?,
                    other => return Err(PlanMutationError::UnsupportedDirection(*other)),
                };
                if let Some(label_id) = resolved_label {
                    bindings.add_edge_label_delta(label_id, 1);
                }
                if let Some(variable) = variable {
                    bindings.edges.insert(variable.to_string(), handle);
                }
                record_forward_edge_insert(bindings, src_id);
            }
            PlanOp::UseGraph {
                sub_plan: Some(sub_plan),
                ..
            } => {
                execute_ops_with_bindings(store, sub_plan, parameters, execution.clone(), bindings)?
            }
            PlanOp::SetProperties { items } => {
                for item in items {
                    execute_set_item(store, item, &execution, &evaluator, bindings)?;
                }
            }
            PlanOp::RemoveProperties { items } => {
                for item in items {
                    execute_remove_item(store, item, &execution, bindings)?;
                }
            }
            PlanOp::DeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                collect_vertex_delete_label_deltas(
                    store,
                    &crate::element_id_encoding::resolve_or_host_fixture(
                        execution.element_id_encoding_key(),
                    ),
                    vertex_id,
                    false,
                    &execution.constrained_properties,
                    &execution.local_constrained_properties,
                    bindings,
                )?;
                store.delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DetachDeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                collect_vertex_delete_label_deltas(
                    store,
                    &crate::element_id_encoding::resolve_or_host_fixture(
                        execution.element_id_encoding_key(),
                    ),
                    vertex_id,
                    true,
                    &execution.constrained_properties,
                    &execution.local_constrained_properties,
                    bindings,
                )?;
                store.detach_delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DeleteEdge { variable } => {
                let handle = *bindings.edges.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingElementBinding {
                        variable: variable.to_string(),
                    }
                })?;
                if let Some(label_id) = edge_label_delta_for_handle(store, handle) {
                    bindings.add_edge_label_delta(label_id, -1);
                }
                store.delete_edge_by_handle(handle)?;
                bindings.edges.remove(variable.as_ref());
            }
            PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
                project_procedure_yields(columns, bindings)?
            }
            PlanOp::CallProcedure {
                name,
                args,
                yield_columns,
                optional,
            } => gleaph_finalize::execute_call_procedure(
                store,
                name,
                args,
                yield_columns.as_deref(),
                *optional,
                bindings,
            )?,
            other if !is_mutation_op(other) => {}
            other => return Err(PlanMutationError::UnsupportedOp(plan_op_name(other))),
        }
    }
    finish_hot_forward_vertices(bindings);
    Ok(())
}

/// Apply the mutation ops in order against pre-seeded `bindings`, **without** finalizing
/// hot-forward accounting. Read-prefix ops are skipped here: matched variables are bound in
/// the read phase before this canonical segment and supplied via `bindings` (see
/// [`execute_mutation_tail_async`]).
pub(crate) async fn apply_mutation_ops_async(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    let evaluator = MutationPropertyExprEvaluator::new(parameters, execution.caller);
    for op in ops {
        match op {
            PlanOp::InsertVertex {
                variable,
                labels,
                properties,
            } => {
                let properties = evaluator.resolve_assignments(properties)?;
                let label_ids = resolve_vertex_labels(&execution, labels)?;
                let property_ids = resolve_mutation_properties(&execution, properties)?;
                let unique_labels = label_ids.iter().copied().collect::<BTreeSet<_>>();
                let vertex_id = insert_vertex_with_async(store, label_ids, property_ids).await?;
                for label_id in unique_labels {
                    bindings.add_vertex_label_delta(label_id, 1);
                }
                bindings.created_vertices.push(vertex_id);
                if let Some(variable) = variable {
                    bindings.vertices.insert(variable.to_string(), vertex_id);
                }
            }
            PlanOp::InsertEdge {
                variable,
                src,
                dst,
                direction,
                labels,
                properties,
            } => {
                let src_id = *bindings.vertices.get(src.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: src.to_string(),
                    }
                })?;
                let dst_id = *bindings.vertices.get(dst.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: dst.to_string(),
                    }
                })?;
                let properties = evaluator.resolve_assignments(properties)?;
                let resolved_label = resolve_edge_label(&execution, labels.first())?;
                let property_ids = resolve_mutation_properties(&execution, properties)?;
                let (inline_payload, sidecar_properties) =
                    classify_edge_assignments(&execution, resolved_label, property_ids)?;
                let handle = match direction {
                    EdgeDirection::PointingRight => insert_directed_edge_with_inline(
                        store,
                        src_id,
                        dst_id,
                        resolved_label,
                        inline_payload.as_ref(),
                        sidecar_properties,
                    )?,
                    EdgeDirection::PointingLeft => insert_directed_edge_with_inline(
                        store,
                        dst_id,
                        src_id,
                        resolved_label,
                        inline_payload.as_ref(),
                        sidecar_properties,
                    )?,
                    EdgeDirection::Undirected => insert_undirected_edge_with_inline(
                        store,
                        src_id,
                        dst_id,
                        resolved_label,
                        inline_payload.as_ref(),
                        sidecar_properties,
                    )?,
                    other => return Err(PlanMutationError::UnsupportedDirection(*other)),
                };
                if let Some(label_id) = resolved_label {
                    bindings.add_edge_label_delta(label_id, 1);
                }
                if let Some(variable) = variable {
                    bindings.edges.insert(variable.to_string(), handle);
                }
                record_forward_edge_insert(bindings, src_id);
            }
            PlanOp::UseGraph {
                sub_plan: Some(sub_plan),
                ..
            } => {
                Box::pin(apply_mutation_ops_async(
                    store,
                    sub_plan,
                    parameters,
                    execution.clone(),
                    bindings,
                ))
                .await?
            }
            PlanOp::SetProperties { items } => {
                for item in items {
                    execute_set_item(store, item, &execution, &evaluator, bindings)?;
                }
            }
            PlanOp::RemoveProperties { items } => {
                for item in items {
                    execute_remove_item(store, item, &execution, bindings)?;
                }
            }
            PlanOp::DeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                collect_vertex_delete_label_deltas(
                    store,
                    &crate::element_id_encoding::resolve_or_host_fixture(
                        execution.element_id_encoding_key(),
                    ),
                    vertex_id,
                    false,
                    &execution.constrained_properties,
                    &execution.local_constrained_properties,
                    bindings,
                )?;
                store.delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DetachDeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                collect_vertex_delete_label_deltas(
                    store,
                    &crate::element_id_encoding::resolve_or_host_fixture(
                        execution.element_id_encoding_key(),
                    ),
                    vertex_id,
                    true,
                    &execution.constrained_properties,
                    &execution.local_constrained_properties,
                    bindings,
                )?;
                store.detach_delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DeleteEdge { variable } => {
                let handle = *bindings.edges.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingElementBinding {
                        variable: variable.to_string(),
                    }
                })?;
                if let Some(label_id) = edge_label_delta_for_handle(store, handle) {
                    bindings.add_edge_label_delta(label_id, -1);
                }
                store.delete_edge_by_handle(handle)?;
                bindings.edges.remove(variable.as_ref());
            }
            PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
                project_procedure_yields(columns, bindings)?
            }
            PlanOp::CallProcedure {
                name,
                args,
                yield_columns,
                optional,
            } => gleaph_finalize::execute_call_procedure(
                store,
                name,
                args,
                yield_columns.as_deref(),
                *optional,
                bindings,
            )?,
            other if !is_mutation_op(other) => {}
            other => return Err(PlanMutationError::UnsupportedOp(plan_op_name(other))),
        }
    }
    Ok(())
}

async fn execute_ops_with_bindings_async(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    Box::pin(apply_mutation_ops_async(
        store, ops, parameters, execution, bindings,
    ))
    .await?;
    finish_hot_forward_vertices(bindings);
    Ok(())
}

/// One read-phase binding row projected to the vertex/edge handles a mutation tail can seed.
#[derive(Clone, Debug, Default)]
pub struct SeededMutationRow {
    pub vertices: BTreeMap<String, VertexId>,
    pub edges: BTreeMap<String, EdgeHandle>,
}

/// Number of leading read-prefix ops in a DML plan: everything before the first op the
/// mutation executor owns (a mutation op, a Gleaph-finalize `CALL`, or a `USE GRAPH`
/// sub-plan). Those tail ops must run in the mutation executor, not the read phase.
pub fn read_prefix_len(ops: &[PlanOp]) -> usize {
    ops.iter()
        .position(|op| {
            is_mutation_op(op)
                || matches!(
                    op,
                    PlanOp::CallProcedure { .. }
                        | PlanOp::UseGraph {
                            sub_plan: Some(_),
                            ..
                        }
                )
        })
        .unwrap_or(ops.len())
}

/// Run a plan's mutation tail once per read-phase seed row (ADR 0029 §1 canonical segment).
///
/// `seed_rows` are the binding rows produced by the plan's read prefix, already executed
/// (with index access) in the read phase. An empty slice means the statement has no read
/// prefix (e.g. a bare `INSERT`): the ops run once with no pre-bound variables. Variable
/// bindings reset per row; label-stats and hot-forward accounting accumulate across the
/// whole batch, which executes as a single shard-local segment with no inter-canister
/// `await` between writes.
pub async fn execute_mutation_tail_async(
    store: &GraphStore,
    mutation_ops: &[PlanOp],
    seed_rows: &[SeededMutationRow],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
) -> Result<PlanMutationBindings, PlanMutationError> {
    let mut bindings = PlanMutationBindings::default();
    if seed_rows.is_empty() {
        Box::pin(apply_mutation_ops_async(
            store,
            mutation_ops,
            parameters,
            execution,
            &mut bindings,
        ))
        .await?;
    } else {
        for row in seed_rows {
            bindings.vertices = row.vertices.clone();
            bindings.edges = row.edges.clone();
            Box::pin(apply_mutation_ops_async(
                store,
                mutation_ops,
                parameters,
                execution.clone(),
                &mut bindings,
            ))
            .await?;
        }
    }
    finish_hot_forward_vertices(&mut bindings);
    Ok(bindings)
}

fn execute_set_item(
    store: &GraphStore,
    item: &SetPlanItem,
    execution: &GqlExecutionContext,
    evaluator: &impl MutationPropertyExprEvaluation,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    match item {
        SetPlanItem::Property {
            variable,
            property,
            value,
        } => {
            let value = evaluator.eval(property.as_ref(), value)?;
            let property_id = resolve_property_id(execution, property.as_ref())?;

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                store
                    .set_vertex_property(*vertex_id, property_id, value)
                    .map_err(GraphStoreError::from)?;
                return Ok(());
            }

            if let Some(edge) = bindings.edges.get(variable.as_ref()) {
                if is_inline_edge_property(execution, *edge, property_id) {
                    let payload_bytes =
                        encode_inline_edge_property(execution, *edge, property_id, &value)?;
                    store.update_edge_payload_at_handle(*edge, &payload_bytes)?;
                } else {
                    store
                        .set_edge_property(*edge, property_id, value)
                        .map_err(GraphStoreError::from)?;
                }
                return Ok(());
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
        SetPlanItem::AllProperties { variable, value } => {
            execute_set_all_properties(store, variable, value, execution, evaluator, bindings)
        }
        SetPlanItem::Label { variable, label } => {
            let label_id = execution
                .resolved_vertex_label_id(label.as_ref())
                .ok_or_else(|| PlanMutationError::MissingResolvedLabel {
                    namespace: "node",
                    name: label.to_string(),
                })?;

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                let vertex = store.vertex(*vertex_id).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                let had_label = store.vertex_has_label(*vertex_id, vertex, label_id);
                let vertex = store
                    .add_vertex_label(*vertex_id, vertex, label_id)
                    .map_err(GraphStoreError::from)?;
                store
                    .set_vertex(*vertex_id, vertex)
                    .map_err(GraphStoreError::from)?;
                if !had_label {
                    bindings.add_vertex_label_delta(label_id, 1);
                }
                return Ok(());
            }

            if bindings.edges.contains_key(variable.as_ref()) {
                return Err(PlanMutationError::UnsupportedSetItem("EdgeLabel"));
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
    }
}

fn execute_set_all_properties(
    store: &GraphStore,
    variable: &Str,
    value: &gleaph_gql::ast::Expr,
    execution: &GqlExecutionContext,
    evaluator: &impl MutationPropertyExprEvaluation,
    bindings: &PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    let fields = match evaluator.eval(variable.as_ref(), value)? {
        Value::Record(fields) => fields,
        _ => {
            return Err(PlanMutationError::InvalidPropertyReplacement {
                variable: variable.to_string(),
            });
        }
    };

    if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
        for (property_id, _) in store.vertex_properties(*vertex_id) {
            store.remove_vertex_property(*vertex_id, property_id);
        }
        for (name, value) in fields {
            let property_id = resolve_property_id(execution, &name)?;
            store
                .set_vertex_property(*vertex_id, property_id, value)
                .map_err(GraphStoreError::from)?;
        }
        return Ok(());
    }

    if let Some(edge) = bindings.edges.get(variable.as_ref()) {
        let (payload_bytes, sidecar_fields) =
            prepare_edge_record_replacement(execution, *edge, fields)?;
        for (property_id, _) in store.edge_properties(*edge) {
            store.remove_edge_property(*edge, property_id);
        }
        if let Some(bytes) = payload_bytes {
            store.update_edge_payload_at_handle(*edge, &bytes)?;
        }
        for (property_id, value) in sidecar_fields {
            store
                .set_edge_property(*edge, property_id, value)
                .map_err(GraphStoreError::from)?;
        }
        return Ok(());
    }

    Err(PlanMutationError::MissingElementBinding {
        variable: variable.to_string(),
    })
}

fn execute_remove_item(
    store: &GraphStore,
    item: &RemovePlanItem,
    execution: &GqlExecutionContext,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    match item {
        RemovePlanItem::Property { variable, property } => {
            let Some(property_id) = execution.resolved_property_id(property.as_ref()) else {
                return Ok(());
            };

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()).copied() {
                // ADR 0030 slice 5b / slice 10: removing a constrained property frees its reserved
                // value — capture the `Release` (federated outbox, and `ShardLocalGlobal` local
                // table) before the value is gone. Owner resolution is shared across both passes.
                if !execution.constrained_properties.is_empty()
                    || !execution.local_constrained_properties.is_empty()
                {
                    let element_id_key = crate::element_id_encoding::resolve_or_host_fixture(
                        execution.element_id_encoding_key(),
                    );
                    let mut owner_cache: Option<Vec<u8>> = None;
                    capture_remove_property_releases(
                        store,
                        &element_id_key,
                        vertex_id,
                        property_id,
                        &execution.constrained_properties,
                        &mut owner_cache,
                        &mut bindings.released_unique_values,
                    );
                    capture_remove_property_releases(
                        store,
                        &element_id_key,
                        vertex_id,
                        property_id,
                        &execution.local_constrained_properties,
                        &mut owner_cache,
                        &mut bindings.released_local_unique_values,
                    );
                }
                store.remove_vertex_property(vertex_id, property_id);
                return Ok(());
            }

            if let Some(edge) = bindings.edges.get(variable.as_ref()) {
                if is_inline_edge_property(execution, *edge, property_id) {
                    return Err(PlanMutationError::CannotRemoveInlineProperty {
                        property: property.to_string(),
                    });
                }
                store.remove_edge_property(*edge, property_id);
                return Ok(());
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
        RemovePlanItem::Label { variable, label } => {
            let label_id = execution
                .resolved_vertex_label_id(label.as_ref())
                .ok_or_else(|| PlanMutationError::MissingResolvedLabel {
                    namespace: "node",
                    name: label.to_string(),
                })?;

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()).copied() {
                let vertex = store.vertex(vertex_id).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                let had_label = store.vertex_has_label(vertex_id, vertex, label_id);
                // ADR 0030 slice 5b: dropping a label removes the applicability of its
                // `(label, property)` constraints — capture each freed value before the label is
                // gone (only when the vertex actually carried the label).
                if had_label
                    && (!execution.constrained_properties.is_empty()
                        || !execution.local_constrained_properties.is_empty())
                {
                    let element_id_key = crate::element_id_encoding::resolve_or_host_fixture(
                        execution.element_id_encoding_key(),
                    );
                    let mut owner_cache: Option<Vec<u8>> = None;
                    capture_remove_label_releases(
                        store,
                        &element_id_key,
                        vertex_id,
                        label_id,
                        &execution.constrained_properties,
                        &mut owner_cache,
                        &mut bindings.released_unique_values,
                    );
                    capture_remove_label_releases(
                        store,
                        &element_id_key,
                        vertex_id,
                        label_id,
                        &execution.local_constrained_properties,
                        &mut owner_cache,
                        &mut bindings.released_local_unique_values,
                    );
                }
                let vertex = store.remove_vertex_label(vertex_id, vertex, label_id);
                store
                    .set_vertex(vertex_id, vertex)
                    .map_err(GraphStoreError::from)?;
                if had_label {
                    bindings.add_vertex_label_delta(label_id, -1);
                }
                return Ok(());
            }

            if bindings.edges.contains_key(variable.as_ref()) {
                return Err(PlanMutationError::UnsupportedRemoveItem("EdgeLabel"));
            }

            Err(PlanMutationError::MissingElementBinding {
                variable: variable.to_string(),
            })
        }
    }
}

fn record_forward_edge_insert(bindings: &mut PlanMutationBindings, src_id: VertexId) {
    *bindings
        .forward_edge_insert_counts
        .entry(src_id)
        .or_insert(0) += 1;
}

fn finish_hot_forward_vertices(bindings: &mut PlanMutationBindings) {
    use gleaph_graph_kernel::federation::HOT_FORWARD_EDGE_INSERT_THRESHOLD;

    bindings.hot_forward_vertices = bindings
        .forward_edge_insert_counts
        .iter()
        .filter(|(_, count)| **count >= HOT_FORWARD_EDGE_INSERT_THRESHOLD)
        .map(|(vid, _)| *vid)
        .collect();
    bindings
        .hot_forward_vertices
        .sort_by_key(|vid| u32::from(*vid));
    bindings.hot_forward_vertices.dedup();
}

fn project_procedure_yields(
    columns: &[ProjectColumn],
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    if bindings.procedure_yields.is_empty() {
        return Ok(());
    }

    let mut row = BTreeMap::new();
    if columns.is_empty() {
        row.extend(bindings.procedure_yields.clone());
    } else {
        for column in columns {
            let (default_name, value) = procedure_yield_projection_value(&column.expr, bindings)?;
            let name = column
                .alias
                .as_ref()
                .map(Str::to_string)
                .unwrap_or(default_name);
            row.insert(name, value);
        }
    }
    bindings.procedure_rows = vec![row];
    Ok(())
}

fn procedure_yield_projection_value(
    expr: &gleaph_gql::ast::Expr,
    bindings: &PlanMutationBindings,
) -> Result<(String, Value), PlanMutationError> {
    match &expr.kind {
        ExprKind::Variable(name) => bindings
            .procedure_yields
            .get(name)
            .cloned()
            .map(|value| (name.clone(), value))
            .ok_or_else(|| PlanMutationError::MissingProcedureYield {
                variable: name.clone(),
            }),
        ExprKind::Literal(value) => Ok(("expr".to_owned(), value.clone())),
        _ => Err(PlanMutationError::UnsupportedExpression {
            property: "CALL YIELD".to_owned(),
        }),
    }
}

fn is_mutation_op(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::InsertVertex { .. }
            | PlanOp::InsertEdge { .. }
            | PlanOp::SetProperties { .. }
            | PlanOp::RemoveProperties { .. }
            | PlanOp::DeleteVertex { .. }
            | PlanOp::DetachDeleteVertex { .. }
            | PlanOp::DeleteEdge { .. }
    )
}

fn plan_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::SetProperties { .. } => "SetProperties",
        PlanOp::RemoveProperties { .. } => "RemoveProperties",
        PlanOp::DeleteVertex { .. } => "DeleteVertex",
        PlanOp::DetachDeleteVertex { .. } => "DetachDeleteVertex",
        PlanOp::DeleteEdge { .. } => "DeleteEdge",
        PlanOp::CallProcedure { .. } => "CallProcedure",
        _ => "PlanOp",
    }
}

fn add_label_delta<T>(deltas: &mut Vec<(T, i64)>, label: T, delta: i64)
where
    T: Copy + Eq,
{
    if delta == 0 {
        return;
    }
    if let Some((_, existing)) = deltas.iter_mut().find(|(id, _)| *id == label) {
        *existing += delta;
        if *existing == 0 {
            deltas.retain(|(_, value)| *value != 0);
        }
        return;
    }
    deltas.push((label, delta));
}

fn edge_label_delta_for_handle(store: &GraphStore, handle: EdgeHandle) -> Option<EdgeLabelId> {
    let canonical = store.canonical_edge_handle_for_sidecar(handle);
    crate::facade::catalog_edge_label_from_wire(canonical.label_id)
}

fn collect_vertex_delete_label_deltas(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    vertex_id: VertexId,
    include_incident_edges: bool,
    constrained: &[ConstrainedPropertyDispatch],
    local_constrained: &[ConstrainedPropertyDispatch],
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    if let Some(vertex) = store.vertex(vertex_id) {
        let labels = store.vertex_labels(vertex_id, vertex);
        for &label_id in &labels {
            bindings.add_vertex_label_delta(label_id, -1);
        }
        // ADR 0030 slice 5b / slice 10: capture the freed values for both the federated (outbox
        // `Release`) and `ShardLocalGlobal` (local-table) paths. The owner element id is resolved
        // once per vertex and shared across both passes.
        let mut owner_cache: Option<Vec<u8>> = None;
        if !constrained.is_empty() {
            collect_vertex_release_effects(
                store,
                element_id_key,
                vertex_id,
                &labels,
                constrained,
                &mut owner_cache,
                &mut bindings.released_unique_values,
            );
        }
        if !local_constrained.is_empty() {
            collect_vertex_release_effects(
                store,
                element_id_key,
                vertex_id,
                &labels,
                local_constrained,
                &mut owner_cache,
                &mut bindings.released_local_unique_values,
            );
        }
    }
    if include_incident_edges {
        collect_detach_delete_edge_label_deltas(store, vertex_id, bindings)?;
    }
    Ok(())
}

/// Captures the `Release` effects a constrained vertex delete frees (ADR 0030 slice 5b), **before**
/// the canonical delete so the property value and owner element id are still readable. Every
/// constraint applicable to the vertex (the vertex carries the constrained label) frees its value.
fn collect_vertex_release_effects(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    vertex_id: VertexId,
    labels: &[VertexLabelId],
    constrained: &[ConstrainedPropertyDispatch],
    owner_cache: &mut Option<Vec<u8>>,
    out: &mut Vec<PendingUniqueRelease>,
) {
    let label_set: BTreeSet<VertexLabelId> = labels.iter().copied().collect();
    for entry in constrained {
        if label_set.contains(&entry.vertex_label_id) {
            capture_constrained_release(store, element_id_key, vertex_id, entry, owner_cache, out);
        }
    }
}

/// Captures the `Release` for a `REMOVE n.prop` on a constrained property (ADR 0030 slice 5b), for
/// each constraint on `(a label the vertex carries, property_id)`. Called before the property value
/// is removed.
fn capture_remove_property_releases(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    vertex_id: VertexId,
    property_id: PropertyId,
    constrained: &[ConstrainedPropertyDispatch],
    owner_cache: &mut Option<Vec<u8>>,
    out: &mut Vec<PendingUniqueRelease>,
) {
    let Some(vertex) = store.vertex(vertex_id) else {
        return;
    };
    let label_set: BTreeSet<VertexLabelId> =
        store.vertex_labels(vertex_id, vertex).into_iter().collect();
    for entry in constrained {
        if entry.property_id == property_id && label_set.contains(&entry.vertex_label_id) {
            capture_constrained_release(store, element_id_key, vertex_id, entry, owner_cache, out);
        }
    }
}

/// Captures the `Release`s a `REMOVE n:Label` frees (ADR 0030 slice 5b): each constraint on
/// `(label_id, property)` no longer applies once the label is dropped, so its reserved value (if the
/// vertex still holds the property) is freed. Called before the label is removed.
fn capture_remove_label_releases(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    vertex_id: VertexId,
    label_id: VertexLabelId,
    constrained: &[ConstrainedPropertyDispatch],
    owner_cache: &mut Option<Vec<u8>>,
    out: &mut Vec<PendingUniqueRelease>,
) {
    for entry in constrained {
        if entry.vertex_label_id == label_id {
            capture_constrained_release(store, element_id_key, vertex_id, entry, owner_cache, out);
        }
    }
}

/// Records one pending `Release` for a constrained value, reading the value and owner element id
/// before the canonical mutation removes them (ADR 0030 slice 5b). The `encoded_value` is the
/// canonical key the Router reserved for the matching `Acquire`, so the Router's reconciliation keys
/// the same reservation and matches it by `owner_element_id`. No-op when the property is absent or
/// NULL/non-keyable (no value was ever reserved). `owner_cache` resolves the owner once per element.
/// A constrained vertex whose owner element id is unavailable is an invariant violation and traps,
/// rolling back the atomic segment rather than emitting a release the Router can never match.
fn capture_constrained_release(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    vertex_id: VertexId,
    entry: &ConstrainedPropertyDispatch,
    owner_cache: &mut Option<Vec<u8>>,
    out: &mut Vec<PendingUniqueRelease>,
) {
    let Some(value) = store.vertex_property(vertex_id, entry.property_id) else {
        return;
    };
    let encoded_value = match encode_unique_value(&value) {
        UniqueKeyOutcome::Claim(bytes) => bytes,
        UniqueKeyOutcome::NoClaim | UniqueKeyOutcome::Rejected(_) => return,
    };
    let owner = owner_cache.get_or_insert_with(|| {
        store
            .path_vertex_element_id(element_id_key, vertex_id)
            .map(|id| id.to_bytes().to_vec())
            .unwrap_or_else(|| {
                unique_release_trap(
                    "owner element id unavailable for a constrained vertex being modified",
                )
            })
    });
    out.push(PendingUniqueRelease {
        constraint_id: entry.constraint_id,
        encoded_value,
        owner_element_id: owner.clone(),
    });
}

fn unique_release_trap(message: &str) -> ! {
    let message =
        format!("unique-effect Release capture failed inside DML atomic section: {message}");
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::trap(&message);
    }
    #[cfg(not(target_family = "wasm"))]
    {
        panic!("{message}");
    }
}

fn collect_detach_delete_edge_label_deltas(
    store: &GraphStore,
    vertex_id: VertexId,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    let mut seen = BTreeSet::new();
    let mut visit = |owner: VertexId, label_raw: u16, slot_index: u32| {
        if seen.insert((u32::from(owner), label_raw, slot_index))
            && let Some(label_id) = crate::facade::catalog_edge_label_from_wire(
                ic_stable_lara::BucketLabelKey::from_raw(label_raw),
            )
        {
            bindings.add_edge_label_delta(label_id, -1);
        }
    };

    store.for_each_directed_out_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
        visit(vertex_id, edge.label_id, edge.edge_slot_index.raw());
    })?;
    store.for_each_directed_in_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
        visit(
            edge.neighbor_vid(),
            edge.label_id,
            edge.edge_slot_index.raw(),
        );
    })?;
    store.for_each_undirected_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
        let owner = if u32::from(vertex_id) <= u32::from(edge.neighbor_vid()) {
            vertex_id
        } else {
            edge.neighbor_vid()
        };
        visit(owner, edge.label_id, edge.edge_slot_index.raw());
    })?;
    Ok(())
}

/// Packed fixed-width payload bytes plus the profile used to encode them.
#[derive(Clone, Debug)]
struct InlineScalarPayload {
    payload_bytes: Vec<u8>,
    payload_profile: EdgePayloadProfile,
}

/// Preflight every sidecar property: reserved property ids are rejected and the value must be
/// encodable as binary bytes. Failures are reported before any canonical adjacency or payload write
/// so invalid sidecar values cannot leave a partially initialized edge or a torn replacement.
fn validate_sidecar_properties(
    execution: &GqlExecutionContext,
    sidecar: Vec<(PropertyId, Value)>,
) -> Result<Vec<(PropertyId, Value)>, PlanMutationError> {
    for (property_id, value) in &sidecar {
        ensure_property_id(*property_id).map_err(|_| {
            PlanMutationError::InvalidSidecarPropertyValue {
                property: property_name_or_id(execution, *property_id),
                reason: "reserved property id".to_owned(),
            }
        })?;
        ensure_persistable(value).map_err(|err| {
            PlanMutationError::InvalidSidecarPropertyValue {
                property: property_name_or_id(execution, *property_id),
                reason: err.to_string(),
            }
        })?;
    }
    Ok(sidecar)
}

/// Classify evaluated edge-property assignments into at most one inline scalar payload plus the
/// remaining sidecar assignments. Rejects duplicate inline assignments, missing required inline
/// values, and `NULL`.
fn classify_edge_assignments(
    execution: &GqlExecutionContext,
    label: Option<EdgeLabelId>,
    assignments: Vec<(PropertyId, Value)>,
) -> Result<(Option<InlineScalarPayload>, Vec<(PropertyId, Value)>), PlanMutationError> {
    let Some(label_id) = label else {
        return Ok((None, validate_sidecar_properties(execution, assignments)?));
    };
    let Some((inline_property_id, profile)) =
        execution.resolved_edge_label_inline_property(label_id)
    else {
        return Ok((None, validate_sidecar_properties(execution, assignments)?));
    };

    let mut inline_value: Option<Value> = None;
    let mut sidecar = Vec::new();
    for (property_id, value) in assignments {
        if property_id == inline_property_id {
            if inline_value.is_some() {
                return Err(PlanMutationError::DuplicateInlinePropertyAssignment {
                    property: property_name_or_id(execution, inline_property_id),
                });
            }
            inline_value = Some(value);
        } else {
            sidecar.push((property_id, value));
        }
    }

    let Some(value) = inline_value else {
        return Err(PlanMutationError::MissingRequiredInlineProperty {
            label: label_name_or_id(execution, label_id),
            property: property_name_or_id(execution, inline_property_id),
        });
    };

    if matches!(value, Value::Null) {
        return Err(PlanMutationError::NullInlineProperty {
            property: property_name_or_id(execution, inline_property_id),
        });
    }

    let payload_bytes = encode_edge_payload_scalar(&profile, &value).map_err(|err| {
        PlanMutationError::InvalidInlinePropertyValue {
            property: property_name_or_id(execution, inline_property_id),
            reason: err.to_string(),
        }
    })?;

    Ok((
        Some(InlineScalarPayload {
            payload_bytes,
            payload_profile: profile,
        }),
        validate_sidecar_properties(execution, sidecar)?,
    ))
}

/// Returns `true` when `property_id` is the Router-resolved inline scalar property for the edge
/// label carried by `handle`.
fn is_inline_edge_property(
    execution: &GqlExecutionContext,
    handle: EdgeHandle,
    property_id: PropertyId,
) -> bool {
    let Some(label_id) = crate::facade::catalog_edge_label_from_wire(handle.label_id) else {
        return false;
    };
    execution
        .resolved_edge_label_inline_property(label_id)
        .is_some_and(|(inline_property_id, _)| inline_property_id == property_id)
}

/// Encode a single inline property value for an existing edge, failing closed on any schema or
/// value mismatch.
fn encode_inline_edge_property(
    execution: &GqlExecutionContext,
    handle: EdgeHandle,
    property_id: PropertyId,
    value: &Value,
) -> Result<Vec<u8>, PlanMutationError> {
    let Some(label_id) = crate::facade::catalog_edge_label_from_wire(handle.label_id) else {
        return Err(PlanMutationError::InvalidInlinePropertyValue {
            property: property_name_or_id(execution, property_id),
            reason: "edge label has no inline schema".to_owned(),
        });
    };
    let Some((inline_property_id, profile)) =
        execution.resolved_edge_label_inline_property(label_id)
    else {
        return Err(PlanMutationError::InvalidInlinePropertyValue {
            property: property_name_or_id(execution, property_id),
            reason: "edge label has no inline schema".to_owned(),
        });
    };
    if inline_property_id != property_id {
        return Err(PlanMutationError::InvalidInlinePropertyValue {
            property: property_name_or_id(execution, property_id),
            reason: "property is not the inline scalar for this edge label".to_owned(),
        });
    }
    if matches!(value, Value::Null) {
        return Err(PlanMutationError::NullInlineProperty {
            property: property_name_or_id(execution, property_id),
        });
    }
    encode_edge_payload_scalar(&profile, value).map_err(|err| {
        PlanMutationError::InvalidInlinePropertyValue {
            property: property_name_or_id(execution, property_id),
            reason: err.to_string(),
        }
    })
}

/// Preflight an all-properties replacement on a bound edge: resolve ids, classify the inline
/// scalar, encode it, and return the sidecar replacement list. No storage write occurs here.
fn prepare_edge_record_replacement(
    execution: &GqlExecutionContext,
    edge: EdgeHandle,
    fields: Vec<(String, Value)>,
) -> Result<(Option<Vec<u8>>, Vec<(PropertyId, Value)>), PlanMutationError> {
    let label = crate::facade::catalog_edge_label_from_wire(edge.label_id);
    let inline = label.and_then(|label_id| execution.resolved_edge_label_inline_property(label_id));

    let mut inline_value: Option<Value> = None;
    let mut sidecar = Vec::new();

    for (name, value) in fields {
        let property_id = resolve_property_id(execution, &name)?;
        if let Some((expected_inline_id, _)) = inline
            && property_id == expected_inline_id
        {
            if inline_value.is_some() {
                return Err(PlanMutationError::DuplicateInlinePropertyAssignment {
                    property: property_name_or_id(execution, expected_inline_id),
                });
            }
            inline_value = Some(value);
            continue;
        }
        sidecar.push((property_id, value));
    }

    let payload_bytes = if let Some((expected_inline_id, profile)) = inline {
        let Some(value) = inline_value else {
            return Err(PlanMutationError::MissingRequiredInlineProperty {
                label: label
                    .and_then(|id| execution.resolved_edge_label_name(id))
                    .unwrap_or_default(),
                property: property_name_or_id(execution, expected_inline_id),
            });
        };
        if matches!(value, Value::Null) {
            return Err(PlanMutationError::NullInlineProperty {
                property: property_name_or_id(execution, expected_inline_id),
            });
        }
        Some(encode_edge_payload_scalar(&profile, &value).map_err(|err| {
            PlanMutationError::InvalidInlinePropertyValue {
                property: property_name_or_id(execution, expected_inline_id),
                reason: err.to_string(),
            }
        })?)
    } else {
        None
    };

    Ok((
        payload_bytes,
        validate_sidecar_properties(execution, sidecar)?,
    ))
}

fn insert_directed_edge_with_inline(
    store: &GraphStore,
    source: VertexId,
    target: VertexId,
    label: Option<EdgeLabelId>,
    inline_payload: Option<&InlineScalarPayload>,
    sidecar_properties: Vec<(PropertyId, Value)>,
) -> Result<EdgeHandle, PlanMutationError> {
    if let Some(payload) = inline_payload {
        GraphMutationExecutor::insert_directed_edge_with_payload_bytes(
            store,
            source,
            target,
            label,
            &payload.payload_bytes,
            sidecar_properties,
        )
        .map_err(PlanMutationError::from)
    } else {
        GraphMutationExecutor::insert_directed_edge_with(
            store,
            source,
            target,
            label,
            sidecar_properties,
        )
        .map_err(PlanMutationError::from)
    }
}

fn insert_undirected_edge_with_inline(
    store: &GraphStore,
    endpoint_a: VertexId,
    endpoint_b: VertexId,
    label: Option<EdgeLabelId>,
    inline_payload: Option<&InlineScalarPayload>,
    sidecar_properties: Vec<(PropertyId, Value)>,
) -> Result<EdgeHandle, PlanMutationError> {
    if let Some(payload) = inline_payload {
        GraphMutationExecutor::insert_undirected_edge_with_payload_bytes(
            store,
            endpoint_a,
            endpoint_b,
            label,
            &payload.payload_bytes,
            sidecar_properties,
        )
        .map_err(PlanMutationError::from)
    } else {
        GraphMutationExecutor::insert_undirected_edge_with(
            store,
            endpoint_a,
            endpoint_b,
            label,
            sidecar_properties,
        )
        .map_err(PlanMutationError::from)
    }
}

fn property_name_or_id(execution: &GqlExecutionContext, property_id: PropertyId) -> String {
    execution
        .resolved_properties
        .as_ref()
        .and_then(|table| {
            table
                .properties
                .iter()
                .find(|p| p.id == property_id)
                .map(|p| p.name.clone())
        })
        .unwrap_or_else(|| property_id.raw().to_string())
}

fn label_name_or_id(execution: &GqlExecutionContext, label_id: EdgeLabelId) -> String {
    execution
        .resolved_edge_label_name(label_id)
        .unwrap_or_else(|| label_id.raw().to_string())
}

fn resolve_vertex_labels(
    execution: &GqlExecutionContext,
    labels: &[gleaph_gql_planner::NodeLabelRef],
) -> Result<Vec<VertexLabelId>, PlanMutationError> {
    labels
        .iter()
        .map(|label| {
            execution
                .resolved_vertex_label_id(label.as_ref())
                .ok_or_else(|| PlanMutationError::MissingResolvedLabel {
                    namespace: "node",
                    name: label.to_string(),
                })
        })
        .collect()
}

fn resolve_edge_label(
    execution: &GqlExecutionContext,
    label: Option<&gleaph_gql_planner::EdgeLabelRef>,
) -> Result<Option<EdgeLabelId>, PlanMutationError> {
    label
        .map(|label| {
            execution
                .resolved_edge_label_id(label.as_ref())
                .ok_or_else(|| PlanMutationError::MissingResolvedLabel {
                    namespace: "edge",
                    name: label.to_string(),
                })
        })
        .transpose()
}

fn resolve_property_id(
    execution: &GqlExecutionContext,
    name: &str,
) -> Result<PropertyId, PlanMutationError> {
    execution
        .resolved_property_id(name)
        .ok_or_else(|| PlanMutationError::MissingResolvedProperty {
            name: name.to_owned(),
        })
}

fn resolve_mutation_properties(
    execution: &GqlExecutionContext,
    properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
) -> Result<Vec<(PropertyId, Value)>, PlanMutationError> {
    properties
        .into_iter()
        .map(|(name, value)| resolve_property_id(execution, name.as_ref()).map(|id| (id, value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::EdgeHandle;
    use super::*;
    use crate::facade::canonical_undirected_owner;
    use crate::gql_execution_context::GqlExecutionContext;
    use crate::test_labels::install_test_edge_payload_profile;
    use gleaph_gql::ast::{BinaryOp, CmpOp, Expr, ExprKind, TruthValue, UnaryOp};
    use gleaph_gql::types::Decimal;
    use gleaph_gql::{ExtensionValue, Value};
    use gleaph_gql_planner::plan::{PlanDiagnostics, PropertyAssignment};
    use gleaph_graph_kernel::entry::{EdgeSlotIndex, PropertyId};
    use ic_stable_lara::BucketLabelKey as LaraLabelId;
    use ic_stable_lara::traits::CsrEdge;
    use std::{any::Any, fmt};

    #[test]
    fn executes_insert_vertex_and_edge_ops() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec!["Person".into()],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                    }],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec!["Person".into()],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Bob".into()))),
                    }],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["KNOWS".into()],
                    properties: vec![PropertyAssignment {
                        name: "since".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(2026))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute plan mutations");

        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];
        let edge = bindings.edges["e"];
        let name = crate::test_labels::property_id_for_name("name");
        let since = crate::test_labels::property_id_for_name("since");

        assert_eq!(
            store.vertex_property(a, name),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(edge.owner_vertex_id, a);
        assert_eq!(store.edge_property(edge, since), Some(Value::Int64(2026)));
        assert!(
            store
                .directed_out_edges(a)
                .unwrap()
                .iter()
                .any(|candidate| {
                    candidate.neighbor_vid() == b
                        && candidate.edge_slot_index == EdgeSlotIndex::from_raw(edge.slot_index)
                })
        );
    }

    #[test]
    fn parameters_can_supply_property_values() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec!["Person".into()],
                properties: vec![PropertyAssignment {
                    name: "name".into(),
                    value: Expr::new(ExprKind::Parameter("name".into())),
                }],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };
        let mut parameters = BTreeMap::new();
        parameters.insert("name".to_owned(), Value::Text("Ada".into()));

        let bindings = execute_ops(
            &store,
            &plan.ops,
            &parameters,
            GqlExecutionContext::default(),
        )
        .expect("execute with params");
        let property = crate::test_labels::property_id_for_name("name");

        assert_eq!(
            store.vertex_property(bindings.vertices["n"], property),
            Some(Value::Text("Ada".into()))
        );
    }

    #[test]
    fn set_properties_updates_vertex_and_edge_bindings() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::SetProperties {
                    items: vec![
                        SetPlanItem::Property {
                            variable: "a".into(),
                            property: "name".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                        },
                        SetPlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute set properties");
        let name = crate::test_labels::property_id_for_name("name");
        let weight = crate::test_labels::property_id_for_name("weight");
        let edge = bindings.edges["e"];

        assert_eq!(
            store.vertex_property(bindings.vertices["a"], name),
            Some(Value::Text("Alice".into()))
        );
        assert_eq!(store.edge_property(edge, weight), Some(Value::Int64(7)));
    }

    #[test]
    fn remove_properties_removes_vertex_and_edge_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                    }],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "weight".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
                PlanOp::RemoveProperties {
                    items: vec![
                        RemovePlanItem::Property {
                            variable: "a".into(),
                            property: "name".into(),
                        },
                        RemovePlanItem::Property {
                            variable: "e".into(),
                            property: "weight".into(),
                        },
                        RemovePlanItem::Property {
                            variable: "a".into(),
                            property: "missing_property_is_noop".into(),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute remove properties");
        let name = crate::test_labels::property_id_for_name("name");
        let weight = crate::test_labels::property_id_for_name("weight");
        let edge = bindings.edges["e"];

        assert_eq!(store.vertex_property(bindings.vertices["a"], name), None);
        assert_eq!(store.edge_property(edge, weight), None);
    }

    #[test]
    fn set_all_properties_replaces_vertex_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![
                        PropertyAssignment {
                            name: "name".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                        },
                        PropertyAssignment {
                            name: "stale".into(),
                            value: Expr::new(ExprKind::Literal(Value::Bool(true))),
                        },
                    ],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::AllProperties {
                        variable: "a".into(),
                        value: Expr::new(ExprKind::RecordLiteral(vec![
                            (
                                "name".into(),
                                Expr::new(ExprKind::Literal(Value::Text("Bob".into()))),
                            ),
                            ("age".into(), Expr::new(ExprKind::Literal(Value::Int64(42)))),
                        ])),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute set all properties");
        let vertex_id = bindings.vertices["a"];
        let name = crate::test_labels::property_id_for_name("name");
        let age = crate::test_labels::property_id_for_name("age");
        let stale = crate::test_labels::property_id_for_name("stale");

        assert_eq!(
            store.vertex_property(vertex_id, name),
            Some(Value::Text("Bob".into()))
        );
        assert_eq!(
            store.vertex_property(vertex_id, age),
            Some(Value::Int64(42))
        );
        assert_eq!(store.vertex_property(vertex_id, stale), None);
    }

    #[test]
    fn set_all_properties_replaces_edge_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![
                        PropertyAssignment {
                            name: "weight".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                        PropertyAssignment {
                            name: "stale".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("old".into()))),
                        },
                    ],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::AllProperties {
                        variable: "e".into(),
                        value: Expr::new(ExprKind::RecordLiteral(vec![(
                            "weight".into(),
                            Expr::new(ExprKind::Literal(Value::Int64(9))),
                        )])),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute edge set all properties");
        let edge = bindings.edges["e"];
        let weight = crate::test_labels::property_id_for_name("weight");
        let stale = crate::test_labels::property_id_for_name("stale");

        assert_eq!(store.edge_property(edge, weight), Some(Value::Int64(9)));
        assert_eq!(store.edge_property(edge, stale), None);
    }

    #[test]
    fn set_all_properties_empty_record_clears_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::Literal(Value::Text("Alice".into()))),
                    }],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::AllProperties {
                        variable: "a".into(),
                        value: Expr::new(ExprKind::RecordLiteral(vec![])),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute set empty properties");
        let vertex_id = bindings.vertices["a"];

        assert!(store.vertex_properties(vertex_id).is_empty());
    }

    #[test]
    fn set_all_properties_rejects_non_record_value() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::AllProperties {
                        variable: "a".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(1))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("non-record replacement should fail");

        assert!(matches!(
            err,
            PlanMutationError::InvalidPropertyReplacement { variable } if variable == "a"
        ));
    }

    #[test]
    fn set_and_remove_vertex_labels() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec!["Person".into()],
                    properties: vec![],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::Label {
                        variable: "a".into(),
                        label: "Employee".into(),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute set label");
        let person = crate::test_labels::vertex_label_id_for_name("Person");
        let employee = crate::test_labels::vertex_label_id_for_name("Employee");
        let vertex_id = bindings.vertices["a"];
        let vertex = store.vertex(vertex_id).expect("read vertex");

        let labels = store.vertex_labels(vertex_id, vertex);
        assert!(labels.contains(&person));
        assert!(labels.contains(&employee));
        assert_eq!(labels.len(), 2);

        let remove = PhysicalPlan {
            ops: vec![PlanOp::RemoveProperties {
                items: vec![
                    RemovePlanItem::Label {
                        variable: "a".into(),
                        label: "Person".into(),
                    },
                    RemovePlanItem::Label {
                        variable: "a".into(),
                        label: "MissingLabelIsNoop".into(),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };
        let mut existing_bindings = bindings;
        execute_ops_with_bindings(
            &store,
            &remove.ops,
            &BTreeMap::new(),
            GqlExecutionContext::default(),
            &mut existing_bindings,
        )
        .expect("execute remove label");
        let vertex = store.vertex(vertex_id).expect("read updated vertex");

        assert_eq!(store.vertex_labels(vertex_id, vertex), vec![employee]);
    }

    #[test]
    fn label_stats_delta_tracks_vertex_and_edge_inserts() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec!["TelemetryPerson".into()],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec!["TelemetryPerson".into()],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["TelemetryRel".into()],
                    properties: vec![],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute insert telemetry plan");
        assert_eq!(
            bindings.label_stats_delta.vertex,
            vec![(
                crate::test_labels::vertex_label_id_for_name("TelemetryPerson"),
                2
            )]
        );
        assert_eq!(
            bindings.label_stats_delta.edge,
            vec![(
                crate::test_labels::edge_label_id_for_name("TelemetryRel"),
                1
            )]
        );
    }

    #[test]
    fn label_stats_delta_tracks_set_remove_and_noops() {
        let store = GraphStore::new();
        let vertex_id = store
            .insert_vertex_named(["TelemetryBase"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let mut bindings = PlanMutationBindings::default();
        bindings.vertices.insert("a".into(), vertex_id);
        let ops = vec![
            PlanOp::SetProperties {
                items: vec![
                    SetPlanItem::Label {
                        variable: "a".into(),
                        label: "TelemetryAdded".into(),
                    },
                    SetPlanItem::Label {
                        variable: "a".into(),
                        label: "TelemetryAdded".into(),
                    },
                ],
            },
            PlanOp::RemoveProperties {
                items: vec![
                    RemovePlanItem::Label {
                        variable: "a".into(),
                        label: "TelemetryBase".into(),
                    },
                    RemovePlanItem::Label {
                        variable: "a".into(),
                        label: "TelemetryMissing".into(),
                    },
                ],
            },
        ];

        execute_ops_with_bindings(
            &store,
            &ops,
            &BTreeMap::new(),
            GqlExecutionContext::default(),
            &mut bindings,
        )
        .expect("execute set/remove telemetry ops");
        assert_eq!(
            bindings.label_stats_delta.vertex,
            vec![
                (
                    crate::test_labels::vertex_label_id_for_name("TelemetryAdded"),
                    1
                ),
                (
                    crate::test_labels::vertex_label_id_for_name("TelemetryBase"),
                    -1
                ),
            ]
        );
        assert!(bindings.label_stats_delta.edge.is_empty());
    }

    #[test]
    fn label_stats_delta_tracks_delete_edge() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["TelemetryDeleteA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["TelemetryDeleteB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        let edge = store
            .insert_directed_edge_named(
                a,
                b,
                Some("TelemetryDeleteRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");
        let mut bindings = PlanMutationBindings::default();
        bindings.edges.insert("e".into(), edge);

        execute_ops_with_bindings(
            &store,
            &[PlanOp::DeleteEdge {
                variable: "e".into(),
            }],
            &BTreeMap::new(),
            GqlExecutionContext::default(),
            &mut bindings,
        )
        .expect("delete edge telemetry");
        assert_eq!(
            bindings.label_stats_delta.edge,
            vec![(
                crate::test_labels::edge_label_id_for_name("TelemetryDeleteRel"),
                -1
            )]
        );
    }

    #[test]
    fn label_stats_delta_tracks_detach_delete_vertex_and_incident_edges() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["TelemetryDetachA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["TelemetryDetachB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("TelemetryDetachRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("a->b");
        let mut bindings = PlanMutationBindings::default();
        bindings.vertices.insert("a".into(), a);

        execute_ops_with_bindings(
            &store,
            &[PlanOp::DetachDeleteVertex {
                variable: "a".into(),
            }],
            &BTreeMap::new(),
            GqlExecutionContext::default(),
            &mut bindings,
        )
        .expect("detach delete telemetry");
        assert_eq!(
            bindings.label_stats_delta.vertex,
            vec![(
                crate::test_labels::vertex_label_id_for_name("TelemetryDetachA"),
                -1
            )]
        );
        assert_eq!(
            bindings.label_stats_delta.edge,
            vec![(
                crate::test_labels::edge_label_id_for_name("TelemetryDetachRel"),
                -1
            )]
        );
    }

    #[test]
    fn delete_captures_release_for_constrained_property() {
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["RelUser"], vec![("email", Value::Text("a@x".into()))])
            .expect("insert constrained vertex");
        let expected_owner = store
            .path_vertex_element_id(&ElementIdEncodingKey::host_test_fixture(), vid)
            .expect("owner id")
            .to_bytes()
            .to_vec();
        let constrained = vec![ConstrainedPropertyDispatch {
            vertex_label_id: crate::test_labels::vertex_label_id_for_name("RelUser"),
            property_id: crate::test_labels::property_id_for_name("email"),
            constraint_id: ConstraintNameId::from_raw(1),
        }];
        let mut bindings = PlanMutationBindings::default();

        collect_vertex_delete_label_deltas(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            vid,
            false,
            &constrained,
            &[],
            &mut bindings,
        )
        .expect("collect release");

        let expected_value = match encode_unique_value(&Value::Text("a@x".into())) {
            UniqueKeyOutcome::Claim(bytes) => bytes,
            other => panic!("expected a claim, got {other:?}"),
        };
        assert_eq!(bindings.released_unique_values.len(), 1);
        let release = &bindings.released_unique_values[0];
        assert_eq!(release.constraint_id, ConstraintNameId::from_raw(1));
        assert_eq!(
            release.encoded_value, expected_value,
            "release must carry the same canonical key the Acquire reserved"
        );
        assert_eq!(
            release.owner_element_id, expected_owner,
            "release owner must be the deleted vertex's canonical id"
        );
    }

    #[test]
    fn delete_of_absent_constrained_property_captures_no_release() {
        // The vertex carries the constrained label but not the constrained property, so no value was
        // ever reserved — nothing to release.
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["RelUser2"], vec![("name", Value::Text("x".into()))])
            .expect("insert");
        let constrained = vec![ConstrainedPropertyDispatch {
            vertex_label_id: crate::test_labels::vertex_label_id_for_name("RelUser2"),
            property_id: crate::test_labels::property_id_for_name("email"),
            constraint_id: ConstraintNameId::from_raw(1),
        }];
        let mut bindings = PlanMutationBindings::default();

        collect_vertex_delete_label_deltas(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            vid,
            false,
            &constrained,
            &[],
            &mut bindings,
        )
        .expect("collect");

        assert!(
            bindings.released_unique_values.is_empty(),
            "absent constrained property must not capture a release"
        );
    }

    #[test]
    fn remove_property_captures_release_for_constrained_value() {
        // ADR 0030 slice 5b: `REMOVE n.email` on a constrained property frees its reserved value.
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(
                ["RelRemoveProp"],
                vec![("email", Value::Text("a@x".into()))],
            )
            .expect("insert constrained vertex");
        let expected_owner = store
            .path_vertex_element_id(&ElementIdEncodingKey::host_test_fixture(), vid)
            .expect("owner id")
            .to_bytes()
            .to_vec();
        let email = crate::test_labels::property_id_for_name("email");
        let constrained = vec![ConstrainedPropertyDispatch {
            vertex_label_id: crate::test_labels::vertex_label_id_for_name("RelRemoveProp"),
            property_id: email,
            constraint_id: ConstraintNameId::from_raw(7),
        }];
        let mut bindings = PlanMutationBindings::default();

        let mut owner_cache: Option<Vec<u8>> = None;
        capture_remove_property_releases(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            vid,
            email,
            &constrained,
            &mut owner_cache,
            &mut bindings.released_unique_values,
        );

        assert_eq!(bindings.released_unique_values.len(), 1);
        let release = &bindings.released_unique_values[0];
        assert_eq!(release.constraint_id, ConstraintNameId::from_raw(7));
        assert_eq!(release.owner_element_id, expected_owner);
    }

    #[test]
    fn remove_property_for_unconstrained_label_captures_no_release() {
        // The vertex holds `email` but not the label the constraint is defined on, so the value was
        // never reserved under this constraint — removing it frees nothing.
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(
                ["RelRemovePropOther"],
                vec![("email", Value::Text("a@x".into()))],
            )
            .expect("insert");
        let email = crate::test_labels::property_id_for_name("email");
        let constrained = vec![ConstrainedPropertyDispatch {
            vertex_label_id: crate::test_labels::vertex_label_id_for_name("SomeOtherLabel"),
            property_id: email,
            constraint_id: ConstraintNameId::from_raw(7),
        }];
        let mut bindings = PlanMutationBindings::default();

        let mut owner_cache: Option<Vec<u8>> = None;
        capture_remove_property_releases(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            vid,
            email,
            &constrained,
            &mut owner_cache,
            &mut bindings.released_unique_values,
        );

        assert!(bindings.released_unique_values.is_empty());
    }

    #[test]
    fn remove_label_captures_release_for_each_dropped_constraint() {
        // ADR 0030 slice 5b: dropping a label removes its `(label, property)` constraints, freeing
        // the value the vertex still holds.
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(
                ["RelRemoveLabel"],
                vec![("email", Value::Text("a@x".into()))],
            )
            .expect("insert constrained vertex");
        let expected_owner = store
            .path_vertex_element_id(&ElementIdEncodingKey::host_test_fixture(), vid)
            .expect("owner id")
            .to_bytes()
            .to_vec();
        let label = crate::test_labels::vertex_label_id_for_name("RelRemoveLabel");
        let constrained = vec![ConstrainedPropertyDispatch {
            vertex_label_id: label,
            property_id: crate::test_labels::property_id_for_name("email"),
            constraint_id: ConstraintNameId::from_raw(9),
        }];
        let mut bindings = PlanMutationBindings::default();

        let mut owner_cache: Option<Vec<u8>> = None;
        capture_remove_label_releases(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            vid,
            label,
            &constrained,
            &mut owner_cache,
            &mut bindings.released_unique_values,
        );

        assert_eq!(bindings.released_unique_values.len(), 1);
        assert_eq!(
            bindings.released_unique_values[0].owner_element_id,
            expected_owner
        );
    }

    #[test]
    fn remove_label_without_the_property_captures_no_release() {
        // Dropping the constrained label when the vertex never held the constrained property frees
        // nothing (no value was ever reserved).
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(
                ["RelRemoveLabelNoProp"],
                vec![("name", Value::Text("x".into()))],
            )
            .expect("insert");
        let label = crate::test_labels::vertex_label_id_for_name("RelRemoveLabelNoProp");
        let constrained = vec![ConstrainedPropertyDispatch {
            vertex_label_id: label,
            property_id: crate::test_labels::property_id_for_name("email"),
            constraint_id: ConstraintNameId::from_raw(9),
        }];
        let mut bindings = PlanMutationBindings::default();

        let mut owner_cache: Option<Vec<u8>> = None;
        capture_remove_label_releases(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            vid,
            label,
            &constrained,
            &mut owner_cache,
            &mut bindings.released_unique_values,
        );

        assert!(bindings.released_unique_values.is_empty());
    }

    #[test]
    fn label_stats_delta_collects_all_incident_edges_before_detach_delete() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["TelemetryDetachCollectA"], Vec::<(&str, Value)>::new())
            .expect("insert a");
        let b = store
            .insert_vertex_named(["TelemetryDetachCollectB"], Vec::<(&str, Value)>::new())
            .expect("insert b");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("TelemetryDetachCollectRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("a->b");
        store
            .insert_directed_edge_named(
                b,
                a,
                Some("TelemetryDetachCollectRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("b->a");
        store
            .insert_undirected_edge_named(
                a,
                b,
                Some("TelemetryDetachCollectRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("a-b");
        let mut bindings = PlanMutationBindings::default();

        collect_vertex_delete_label_deltas(
            &store,
            &ElementIdEncodingKey::host_test_fixture(),
            a,
            true,
            &[],
            &[],
            &mut bindings,
        )
        .expect("collect detach delete telemetry");

        assert_eq!(
            bindings.label_stats_delta.vertex,
            vec![(
                crate::test_labels::vertex_label_id_for_name("TelemetryDetachCollectA"),
                -1
            )]
        );
        assert_eq!(
            bindings.label_stats_delta.edge,
            vec![(
                crate::test_labels::edge_label_id_for_name("TelemetryDetachCollectRel"),
                -3
            )]
        );
    }

    #[test]
    fn evaluates_simple_arithmetic_property_expressions() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![
                    PropertyAssignment {
                        name: "score".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Int64(4)))),
                            op: BinaryOp::Mul,
                            right: Box::new(Expr::new(ExprKind::BinaryOp {
                                left: Box::new(Expr::new(ExprKind::Parameter("base".into()))),
                                op: BinaryOp::Add,
                                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(3)))),
                            })),
                        }),
                    },
                    PropertyAssignment {
                        name: "name".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Text("Ada".into())))),
                            op: BinaryOp::Add,
                            right: Box::new(Expr::new(ExprKind::Literal(Value::Text(
                                " Lovelace".into(),
                            )))),
                        }),
                    },
                    PropertyAssignment {
                        name: "negative".into(),
                        value: Expr::new(ExprKind::UnaryOp {
                            op: UnaryOp::Neg,
                            expr: Box::new(Expr::new(ExprKind::Literal(Value::Int64(9)))),
                        }),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };
        let mut parameters = BTreeMap::new();
        parameters.insert("base".to_owned(), Value::Int64(2));

        let bindings = execute_ops(
            &store,
            &plan.ops,
            &parameters,
            GqlExecutionContext::default(),
        )
        .expect("execute arithmetic");
        let vertex = bindings.vertices["n"];
        let score = crate::test_labels::property_id_for_name("score");
        let name = crate::test_labels::property_id_for_name("name");
        let negative = crate::test_labels::property_id_for_name("negative");

        assert_eq!(store.vertex_property(vertex, score), Some(Value::Int64(20)));
        assert_eq!(
            store.vertex_property(vertex, name),
            Some(Value::Text("Ada Lovelace".into()))
        );
        assert_eq!(
            store.vertex_property(vertex, negative),
            Some(Value::Int64(-9))
        );
    }

    #[test]
    fn preserves_decimal_arithmetic() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![
                    PropertyAssignment {
                        name: "price".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                                Decimal::parse("10.50").expect("decimal"),
                            )))),
                            op: BinaryOp::Add,
                            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                        }),
                    },
                    PropertyAssignment {
                        name: "ratio".into(),
                        value: Expr::new(ExprKind::BinaryOp {
                            left: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                                Decimal::parse("1.00").expect("decimal"),
                            )))),
                            op: BinaryOp::Div,
                            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(4)))),
                        }),
                    },
                    PropertyAssignment {
                        name: "negative".into(),
                        value: Expr::new(ExprKind::UnaryOp {
                            op: UnaryOp::Neg,
                            expr: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(
                                Decimal::parse("3.25").expect("decimal"),
                            )))),
                        }),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute decimal arithmetic");
        let vertex = bindings.vertices["n"];
        let price = crate::test_labels::property_id_for_name("price");
        let ratio = crate::test_labels::property_id_for_name("ratio");
        let negative = crate::test_labels::property_id_for_name("negative");

        assert_eq!(
            store.vertex_property(vertex, price),
            Some(Value::Decimal(Decimal::parse("12.5").expect("decimal")))
        );
        assert_eq!(
            store.vertex_property(vertex, ratio),
            Some(Value::Decimal(Decimal::parse("0.25").expect("decimal")))
        );
        assert_eq!(
            store.vertex_property(vertex, negative),
            Some(Value::Decimal(Decimal::parse("-3.25").expect("decimal")))
        );
    }

    #[test]
    fn evaluates_boolean_comparison_and_constructed_values() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![
                    PropertyAssignment {
                        name: "logic".into(),
                        value: Expr::new(ExprKind::Or(
                            Box::new(Expr::new(ExprKind::And(
                                Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
                                Box::new(Expr::new(ExprKind::Literal(Value::Null))),
                            ))),
                            Box::new(Expr::new(ExprKind::Compare {
                                left: Box::new(Expr::new(ExprKind::Literal(Value::Int64(3)))),
                                op: CmpOp::Lt,
                                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(4)))),
                            })),
                        )),
                    },
                    PropertyAssignment {
                        name: "is_unknown".into(),
                        value: Expr::new(ExprKind::IsTruth {
                            expr: Box::new(Expr::new(ExprKind::Literal(Value::Null))),
                            value: TruthValue::Unknown,
                            negated: false,
                        }),
                    },
                    PropertyAssignment {
                        name: "nickname".into(),
                        value: Expr::new(ExprKind::Coalesce(vec![
                            Expr::new(ExprKind::Literal(Value::Null)),
                            Expr::new(ExprKind::Literal(Value::Text("Ada".into()))),
                        ])),
                    },
                    PropertyAssignment {
                        name: "list".into(),
                        value: Expr::new(ExprKind::ListLiteral(vec![
                            Expr::new(ExprKind::Literal(Value::Int64(1))),
                            Expr::new(ExprKind::Literal(Value::Text("two".into()))),
                        ])),
                    },
                    PropertyAssignment {
                        name: "record".into(),
                        value: Expr::new(ExprKind::RecordLiteral(vec![(
                            "ok".into(),
                            Expr::new(ExprKind::Literal(Value::Bool(true))),
                        )])),
                    },
                    PropertyAssignment {
                        name: "bytes".into(),
                        value: Expr::new(ExprKind::Concat(
                            Box::new(Expr::new(ExprKind::Literal(Value::Bytes(vec![1, 2])))),
                            Box::new(Expr::new(ExprKind::Literal(Value::Bytes(vec![3])))),
                        )),
                    },
                ],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("execute constructed values");
        let vertex = bindings.vertices["n"];
        let logic = crate::test_labels::property_id_for_name("logic");
        let is_unknown = crate::test_labels::property_id_for_name("is_unknown");
        let nickname = crate::test_labels::property_id_for_name("nickname");
        let list = crate::test_labels::property_id_for_name("list");
        let record = crate::test_labels::property_id_for_name("record");
        let bytes = crate::test_labels::property_id_for_name("bytes");

        assert_eq!(
            store.vertex_property(vertex, logic),
            Some(Value::Bool(true))
        );
        assert_eq!(
            store.vertex_property(vertex, is_unknown),
            Some(Value::Bool(true))
        );
        assert_eq!(
            store.vertex_property(vertex, nickname),
            Some(Value::Text("Ada".into()))
        );
        assert_eq!(
            store.vertex_property(vertex, list),
            Some(Value::List(vec![
                Value::Int64(1),
                Value::Text("two".into())
            ]))
        );
        assert_eq!(
            store.vertex_property(vertex, record),
            Some(Value::Record(vec![("ok".into(), Value::Bool(true))]))
        );
        assert_eq!(
            store.vertex_property(vertex, bytes),
            Some(Value::Bytes(vec![1, 2, 3]))
        );
    }

    #[test]
    fn delete_vertex_fails_when_vertex_has_incident_edges() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: None,
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::DeleteVertex {
                    variable: "a".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("delete vertex with edges should fail");
        assert!(matches!(
            err,
            PlanMutationError::Store(GraphStoreError::VertexNotDetached { .. })
        ));
    }

    #[test]
    fn delete_vertex_succeeds_for_isolated_vertex() {
        let store = GraphStore::new();
        let before = store.vertex_count();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "k".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(1))),
                    }],
                },
                PlanOp::DeleteVertex {
                    variable: "a".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("delete isolated vertex");
        let before_u32: u32 = before.into();
        let vid = VertexId::from(before_u32);
        let k = crate::test_labels::property_id_for_name("k");
        assert_eq!(store.vertex_property(vid, k), None);
    }

    #[test]
    fn detach_delete_vertex_clears_incident_edge_sidecars() {
        let store = GraphStore::new();
        let before_a = store.vertex_count();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "w".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(2))),
                    }],
                },
                PlanOp::DetachDeleteVertex {
                    variable: "a".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("detach delete vertex");
        let b = bindings.vertices["b"];
        let w = crate::test_labels::property_id_for_name("w");
        let e = bindings.edges["e"];

        assert_eq!(store.edge_property(e, w), None);
        assert!(store.directed_in_edges(b).expect("in edges").is_empty());
        assert!(store.directed_out_edges(b).expect("out edges").is_empty());

        let before_a_u32: u32 = before_a.into();
        let deleted = VertexId::from(before_a_u32);
        assert!(
            matches!(
                store.directed_out_edges(deleted),
                Err(GraphStoreError::Graph(
                    ic_stable_lara::DeferredBidirectionalLabeledError::VertexOutOfRange { vid, .. }
                )) if vid == deleted
            ) || store.directed_out_edges(deleted).unwrap().is_empty(),
            "deleted vertex should not expose outgoing edges"
        );
    }

    #[test]
    fn delete_edge_removes_directed_edge_and_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "w".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(5))),
                    }],
                },
                PlanOp::DeleteEdge {
                    variable: "e".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("delete directed edge");
        let a = bindings.vertices["a"];
        assert!(!bindings.edges.contains_key("e"));

        assert!(
            store
                .directed_out_edges(a)
                .expect("out edges after delete")
                .is_empty()
        );
        assert_eq!(
            store.edge_properties(EdgeHandle {
                owner_vertex_id: a,
                label_id: LaraLabelId::from_raw(0),
                slot_index: 1,
            }),
            Vec::<(PropertyId, Value)>::new()
        );
    }

    #[test]
    fn delete_edge_removes_undirected_edge_and_properties() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::Undirected,
                    labels: vec![],
                    properties: vec![PropertyAssignment {
                        name: "w".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(9))),
                    }],
                },
                PlanOp::DeleteEdge {
                    variable: "e".into(),
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("delete undirected edge");
        let low = bindings.vertices["a"];
        let high = bindings.vertices["b"];
        let owner = canonical_undirected_owner(low, high);

        assert!(store.directed_out_edges(low).unwrap().is_empty());
        assert!(store.directed_out_edges(high).unwrap().is_empty());
        assert_eq!(
            store.edge_properties(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(0),
                slot_index: 1,
            }),
            Vec::<(PropertyId, Value)>::new()
        );
    }

    #[test]
    fn tracks_hot_forward_vertices_from_repeated_edge_inserts() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("src".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("dst1".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("dst2".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: None,
                    src: "src".into(),
                    dst: "dst1".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: None,
                    src: "src".into(),
                    dst: "dst2".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("insert hub edges");
        let src = bindings.vertices["src"];
        assert_eq!(bindings.hot_forward_vertices, vec![src]);
    }

    fn vertex_list_expr(var: &str) -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName {
                parts: vec!["GLEAPH".into(), "VERTEX_LIST".into()],
            },
            args: vec![Expr::var(var)],
            distinct: false,
        })
    }

    fn empty_vertex_list_expr() -> Expr {
        Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName {
                parts: vec!["GLEAPH".into(), "VERTEX_LIST".into()],
            },
            args: vec![],
            distinct: false,
        })
    }

    #[test]
    fn call_finalize_bulk_ingest_makes_hot_forward_span_dense() {
        use gleaph_gql_planner::plan::YieldColumn;
        use ic_stable_lara::labeled::LabeledEdgePayloadBatchScratch;

        let store = GraphStore::new();
        let src = setup_finalize_call_hub_graph(&store);
        let mut bindings = PlanMutationBindings::default();
        bindings.vertices.insert("src".into(), src);

        execute_ops_with_bindings(
            &store,
            &[PlanOp::CallProcedure {
                name: vec!["GLEAPH".into(), "FINALIZE_BULK_INGEST".into()],
                args: vec![vertex_list_expr("src"), empty_vertex_list_expr()],
                yield_columns: Some(vec![YieldColumn {
                    name: "queued_forward".into(),
                    alias: None,
                }]),
                optional: false,
            }],
            &BTreeMap::new(),
            GqlExecutionContext::default(),
            &mut bindings,
        )
        .expect("finalize via CALL");
        assert_eq!(
            bindings.procedure_yields.get("queued_forward"),
            Some(&Value::Int64(1))
        );

        let road = crate::test_labels::edge_label_id_for_name("GqlFinalizeRoad");
        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut dense = None;
        store
            .visit_directed_out_edge_payload_batches_for_label(
                src,
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| dense = Some(batch.dense),
            )
            .expect("payload batches");
        assert_eq!(dense, Some(true));
    }

    fn setup_finalize_call_hub_graph(store: &GraphStore) -> VertexId {
        use gleaph_graph_kernel::entry::{EdgePayloadProfile, EdgeWeightProfile, WeightEncoding};

        let src = store.insert_vertex().expect("src");
        let hub = store.insert_vertex().expect("hub");
        let label = crate::test_labels::edge_label_id_for_name("GqlFinalizeRoad");
        install_test_edge_payload_profile(
            label,
            EdgePayloadProfile::from(EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            }),
        );

        let mut prefixes = Vec::new();
        for _ in 0..48 {
            prefixes.push(store.insert_vertex().expect("prefix"));
        }
        for &prefix in &prefixes {
            store
                .insert_directed_edge_with_payload_bytes(
                    prefix,
                    hub,
                    Some(label),
                    &1u16.to_le_bytes(),
                )
                .expect("prefix->hub");
        }
        for (i, &prefix) in prefixes.iter().enumerate() {
            store
                .insert_directed_edge_with_payload_bytes(
                    src,
                    prefix,
                    Some(label),
                    &((i % 10) as u16 + 1).to_le_bytes(),
                )
                .expect("src->prefix");
        }
        src
    }

    #[test]
    fn call_drain_deferred_maintenance_records_yield_columns() {
        use gleaph_gql_planner::plan::{ProjectColumn, YieldColumn};

        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::CallProcedure {
                    name: vec!["GLEAPH".into(), "DRAIN_DEFERRED_MAINTENANCE".into()],
                    args: vec![],
                    yield_columns: Some(vec![YieldColumn {
                        name: "remaining_queue_len".into(),
                        alias: None,
                    }]),
                    optional: false,
                },
                PlanOp::Project {
                    columns: vec![ProjectColumn {
                        expr: Expr::var("remaining_queue_len"),
                        alias: Some("remaining".into()),
                    }],
                    distinct: false,
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("drain via CALL");
        assert_eq!(
            bindings.procedure_yields.get("remaining_queue_len"),
            Some(&Value::Int64(0))
        );
        assert_eq!(
            bindings.procedure_rows,
            vec![BTreeMap::from([("remaining".into(), Value::Int64(0))])]
        );
    }

    #[test]
    fn call_procedure_rejects_unknown_gleaph_procedure() {
        let store = GraphStore::new();
        let plan = PhysicalPlan {
            ops: vec![PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: None,
                optional: false,
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("unknown procedure");
        assert!(matches!(
            err,
            PlanMutationError::UnknownGleaphProcedure { name } if name == "db.labels"
        ));
    }
    // --- ADR 0034 Slice 22: inline edge scalar mutation packing ---

    fn install_inline_road_fixture() -> (EdgeLabelId, PropertyId) {
        use gleaph_graph_kernel::entry::EdgePayloadEncoding;

        let label = crate::test_labels::edge_label_id_for_name("InlineRoad");
        let property = crate::test_labels::property_id_for_name("distance");
        crate::test_labels::install_test_edge_payload_profile(
            label,
            EdgePayloadProfile {
                byte_width: 2,
                encoding: EdgePayloadEncoding::RawU16,
            },
        );
        crate::test_labels::install_test_edge_inline_property(label, property);
        (label, property)
    }

    fn find_in_edge_payload(
        store: &GraphStore,
        target: VertexId,
        source: VertexId,
    ) -> Option<Vec<u8>> {
        use ic_stable_lara::traits::CsrEdge;
        store
            .directed_in_edges(target)
            .ok()?
            .into_iter()
            .find(|edge| edge.neighbor_vid() == source)
            .map(|edge| edge.payload_bytes().to_vec())
    }

    fn find_out_edge_payload(
        store: &GraphStore,
        source: VertexId,
        target: VertexId,
    ) -> Option<Vec<u8>> {
        use ic_stable_lara::traits::CsrEdge;
        store
            .directed_out_edges(source)
            .ok()?
            .into_iter()
            .find(|edge| edge.neighbor_vid() == target)
            .map(|edge| edge.payload_bytes().to_vec())
    }

    fn find_undirected_edge_payload(
        store: &GraphStore,
        endpoint: VertexId,
        other: VertexId,
    ) -> Option<Vec<u8>> {
        use ic_stable_lara::traits::CsrEdge;
        store
            .undirected_edges(endpoint)
            .ok()?
            .into_iter()
            .find(|edge| edge.neighbor_vid() == other)
            .map(|edge| edge.payload_bytes().to_vec())
    }

    fn inline_edge_scalar_insert_directed() {
        let store = GraphStore::new();
        let (_road_label, _distance) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("insert inline edge");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        assert_eq!(
            find_out_edge_payload(&store, a, b),
            Some(7u16.to_le_bytes().to_vec())
        );
        // Sidecar must not contain the inline property.
        assert_eq!(store.edge_properties(bindings.edges["e"]), Vec::new());
    }

    #[test]
    fn inline_edge_scalar_insert_pointing_left() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingLeft,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("insert inline edge left");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        // Logical direction is b -> a; physical reverse mirror carries the payload too.
        assert_eq!(
            find_out_edge_payload(&store, b, a),
            Some(7u16.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn inline_edge_scalar_insert_undirected() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::Undirected,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("insert inline undirected edge");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        assert_eq!(
            find_undirected_edge_payload(&store, a, b),
            Some(7u16.to_le_bytes().to_vec())
        );
        assert_eq!(
            find_undirected_edge_payload(&store, b, a),
            Some(7u16.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn inline_edge_scalar_set_updates_mirrors() {
        let store = GraphStore::new();
        let (_road_label, _distance) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::Property {
                        variable: "e".into(),
                        property: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(9))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("set inline property");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        assert_eq!(
            find_out_edge_payload(&store, a, b),
            Some(9u16.to_le_bytes().to_vec())
        );
        // Reverse mirror was updated by the same commit.
        assert_eq!(
            find_in_edge_payload(&store, b, a),
            Some(9u16.to_le_bytes().to_vec())
        );
        assert_eq!(store.edge_properties(bindings.edges["e"]), Vec::new());
    }

    #[test]
    fn inline_edge_scalar_insert_mixed_sidecar() {
        let store = GraphStore::new();
        let (_road_label, _distance) = install_inline_road_fixture();
        let note = crate::test_labels::property_id_for_name("note");
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![
                        PropertyAssignment {
                            name: "distance".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                        PropertyAssignment {
                            name: "note".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("hello".into()))),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("insert mixed inline edge");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        assert_eq!(
            find_out_edge_payload(&store, a, b),
            Some(7u16.to_le_bytes().to_vec())
        );
        let sidecar = store.edge_properties(bindings.edges["e"]);
        assert_eq!(sidecar, vec![(note, Value::Text("hello".into()))]);
    }

    #[test]
    fn inline_edge_scalar_missing_aborts_insert() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("missing inline value");
        assert!(
            matches!(err, PlanMutationError::MissingRequiredInlineProperty { .. }),
            "got {err:?}"
        );
        // No edge should have been created.
        assert!(
            store
                .directed_out_edges(VertexId::from(0u32))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn inline_edge_scalar_duplicate_aborts_insert() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![
                        PropertyAssignment {
                            name: "distance".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                        PropertyAssignment {
                            name: "distance".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(8))),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("duplicate inline value");
        assert!(
            matches!(
                err,
                PlanMutationError::DuplicateInlinePropertyAssignment { .. }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn inline_edge_scalar_null_aborts_insert() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Null)),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("null inline value");
        assert!(
            matches!(err, PlanMutationError::NullInlineProperty { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inline_edge_scalar_overflow_aborts_insert() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(65536))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("overflow inline value");
        assert!(
            matches!(err, PlanMutationError::InvalidInlinePropertyValue { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn inline_edge_scalar_all_properties_replacement() {
        let store = GraphStore::new();
        let (_road_label, _distance) = install_inline_road_fixture();
        let note = crate::test_labels::property_id_for_name("note");
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![
                        PropertyAssignment {
                            name: "distance".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                        PropertyAssignment {
                            name: "note".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("old".into()))),
                        },
                    ],
                },
                PlanOp::SetProperties {
                    items: vec![SetPlanItem::AllProperties {
                        variable: "e".into(),
                        value: Expr::new(ExprKind::RecordLiteral(vec![
                            (
                                "distance".into(),
                                Expr::new(ExprKind::Literal(Value::Int64(9))),
                            ),
                            (
                                "note".into(),
                                Expr::new(ExprKind::Literal(Value::Text("new".into()))),
                            ),
                        ])),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("replace edge properties");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        assert_eq!(
            find_out_edge_payload(&store, a, b),
            Some(9u16.to_le_bytes().to_vec())
        );
        let sidecar = store.edge_properties(bindings.edges["e"]);
        assert_eq!(sidecar, vec![(note, Value::Text("new".into()))]);
    }

    #[test]
    fn inline_edge_scalar_remove_rejects() {
        let store = GraphStore::new();
        let (_road_label, _) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "distance".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
                PlanOp::RemoveProperties {
                    items: vec![RemovePlanItem::Property {
                        variable: "e".into(),
                        property: "distance".into(),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("remove inline property");
        assert!(
            matches!(err, PlanMutationError::CannotRemoveInlineProperty { .. }),
            "got {err:?}"
        );
        // Payload unchanged.
        let a = VertexId::from(0u32);
        let b = VertexId::from(1u32);
        assert_eq!(
            find_out_edge_payload(&store, a, b),
            Some(7u16.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn inline_edge_scalar_non_inline_label_sidecar() {
        let store = GraphStore::new();
        let _plain_label = crate::test_labels::edge_label_id_for_name("PlainRoad");
        let weight = crate::test_labels::property_id_for_name("weight");
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["PlainRoad".into()],
                    properties: vec![PropertyAssignment {
                        name: "weight".into(),
                        value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                    }],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("insert plain edge");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];

        // No payload profile installed, so the edge is inserted with empty payload.
        assert_eq!(find_out_edge_payload(&store, a, b), Some(Vec::new()));
        assert_eq!(
            store.edge_properties(bindings.edges["e"]),
            vec![(weight, Value::Int64(7))]
        );
    }
    /// Extension value that deliberately cannot be persisted to the primary store. Used to prove
    /// that invalid sidecar values fail closed before any edge/payload write.
    #[derive(Debug, Clone)]
    struct UnpersistableSidecarExtension;

    impl fmt::Display for UnpersistableSidecarExtension {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "UnpersistableSidecarExtension")
        }
    }

    impl ExtensionValue for UnpersistableSidecarExtension {
        fn type_name(&self) -> &str {
            "unpersistable_sidecar"
        }

        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }

        fn eq_ext(&self, _other: &dyn ExtensionValue) -> bool {
            false
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn unpersistable_value() -> Value {
        Value::Extension(Box::new(UnpersistableSidecarExtension))
    }

    #[test]
    fn inline_edge_scalar_unpersistable_sidecar_aborts_insert() {
        let store = GraphStore::new();
        let (_road_label, _distance) = install_inline_road_fixture();
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![
                        PropertyAssignment {
                            name: "distance".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                        PropertyAssignment {
                            name: "note".into(),
                            value: Expr::new(ExprKind::Literal(unpersistable_value())),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let err = store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect_err("unpersistable sidecar value");
        assert!(
            matches!(err, PlanMutationError::InvalidSidecarPropertyValue { .. }),
            "got {err:?}"
        );

        // Vertices were created, but no edge or payload must exist.
        let a = VertexId::from(0u32);
        let b = VertexId::from(1u32);
        assert_eq!(find_out_edge_payload(&store, a, b), None);
        assert!(store.directed_out_edges(a).unwrap().is_empty());
    }

    #[test]
    fn inline_edge_scalar_unpersistable_sidecar_aborts_replacement() {
        let store = GraphStore::new();
        let (_road_label, _distance) = install_inline_road_fixture();
        let note = crate::test_labels::property_id_for_name("note");

        let insert_plan = PhysicalPlan {
            ops: vec![
                PlanOp::InsertVertex {
                    variable: Some("a".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertVertex {
                    variable: Some("b".into()),
                    labels: vec![],
                    properties: vec![],
                },
                PlanOp::InsertEdge {
                    variable: Some("e".into()),
                    src: "a".into(),
                    dst: "b".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec!["InlineRoad".into()],
                    properties: vec![
                        PropertyAssignment {
                            name: "distance".into(),
                            value: Expr::new(ExprKind::Literal(Value::Int64(7))),
                        },
                        PropertyAssignment {
                            name: "note".into(),
                            value: Expr::new(ExprKind::Literal(Value::Text("old".into()))),
                        },
                    ],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };

        let bindings = store
            .execute_plan_mutations(&insert_plan, GqlExecutionContext::default())
            .expect("insert edge with sidecar");
        let a = bindings.vertices["a"];
        let b = bindings.vertices["b"];
        let edge_handle = bindings.edges["e"];

        let replace_ops = vec![PlanOp::SetProperties {
            items: vec![SetPlanItem::AllProperties {
                variable: "e".into(),
                value: Expr::new(ExprKind::RecordLiteral(vec![
                    (
                        "distance".into(),
                        Expr::new(ExprKind::Literal(Value::Int64(9))),
                    ),
                    (
                        "note".into(),
                        Expr::new(ExprKind::Literal(unpersistable_value())),
                    ),
                ])),
            }],
        }];
        let parameters = BTreeMap::<String, Value>::new();
        let mut replace_bindings = PlanMutationBindings::default();
        replace_bindings.vertices.insert("a".into(), a);
        replace_bindings.vertices.insert("b".into(), b);
        replace_bindings.edges.insert("e".into(), edge_handle);

        let err = execute_ops_with_bindings(
            &store,
            &replace_ops,
            &parameters,
            GqlExecutionContext::default(),
            &mut replace_bindings,
        )
        .expect_err("unpersistable sidecar value during replacement");
        assert!(
            matches!(err, PlanMutationError::InvalidSidecarPropertyValue { .. }),
            "got {err:?}"
        );

        // Both payload and sidecar must remain unchanged.
        assert_eq!(
            find_out_edge_payload(&store, a, b),
            Some(7u16.to_le_bytes().to_vec())
        );
        assert_eq!(
            store.edge_properties(edge_handle),
            vec![(note, Value::Text("old".into()))]
        );
    }
}
