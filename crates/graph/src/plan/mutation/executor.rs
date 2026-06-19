use super::error::PlanMutationError;
use super::expr_evaluator::{MutationPropertyExprEvaluation, MutationPropertyExprEvaluator};
use super::gleaph_finalize;
use crate::facade::mutation_executor::{GraphMutationExecutor, insert_vertex_with_async};
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use crate::gql_execution_context::GqlExecutionContext;
use gleaph_gql::Value;
use gleaph_gql::ast::ExprKind;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{
    PhysicalPlan, PlanOp, ProjectColumn, RemovePlanItem, SetPlanItem, Str,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::plan_exec::LabelStatsDelta;
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

pub async fn execute_plan_mutations_async(
    store: &GraphStore,
    plan: &PhysicalPlan,
    execution: GqlExecutionContext,
) -> Result<PlanMutationBindings, PlanMutationError> {
    execute_ops_async(store, &plan.ops, &BTreeMap::new(), execution).await
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
    forward_edge_insert_counts: BTreeMap<VertexId, u32>,
    /// Forward hubs from this DML batch (sources with enough edge inserts).
    pub hot_forward_vertices: Vec<VertexId>,
}

impl PlanMutationBindings {
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

pub async fn execute_ops_async(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, gleaph_gql::Value>,
    execution: GqlExecutionContext,
) -> Result<PlanMutationBindings, PlanMutationError> {
    let mut bindings = PlanMutationBindings::default();
    Box::pin(execute_ops_with_bindings_async(
        store,
        ops,
        parameters,
        execution,
        &mut bindings,
    ))
    .await?;
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
                let handle = match direction {
                    EdgeDirection::PointingRight => store.insert_directed_edge_with(
                        src_id,
                        dst_id,
                        resolved_label,
                        property_ids,
                    )?,
                    EdgeDirection::PointingLeft => store.insert_directed_edge_with(
                        dst_id,
                        src_id,
                        resolved_label,
                        property_ids,
                    )?,
                    EdgeDirection::Undirected => store.insert_undirected_edge_with(
                        src_id,
                        dst_id,
                        resolved_label,
                        property_ids,
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
                collect_vertex_delete_label_deltas(store, vertex_id, false, bindings)?;
                store.delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DetachDeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                collect_vertex_delete_label_deltas(store, vertex_id, true, bindings)?;
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

async fn execute_ops_with_bindings_async(
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
                let handle = match direction {
                    EdgeDirection::PointingRight => store.insert_directed_edge_with(
                        src_id,
                        dst_id,
                        resolved_label,
                        property_ids,
                    )?,
                    EdgeDirection::PointingLeft => store.insert_directed_edge_with(
                        dst_id,
                        src_id,
                        resolved_label,
                        property_ids,
                    )?,
                    EdgeDirection::Undirected => store.insert_undirected_edge_with(
                        src_id,
                        dst_id,
                        resolved_label,
                        property_ids,
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
                Box::pin(execute_ops_with_bindings_async(
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
                collect_vertex_delete_label_deltas(store, vertex_id, false, bindings)?;
                store.delete_vertex(vertex_id)?;
                bindings.vertices.remove(variable.as_ref());
            }
            PlanOp::DetachDeleteVertex { variable } => {
                let vertex_id = *bindings.vertices.get(variable.as_ref()).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                collect_vertex_delete_label_deltas(store, vertex_id, true, bindings)?;
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
                store
                    .set_edge_property(*edge, property_id, value)
                    .map_err(GraphStoreError::from)?;
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
        for (property_id, _) in store.edge_properties(*edge) {
            store.remove_edge_property(*edge, property_id);
        }
        for (name, value) in fields {
            let property_id = resolve_property_id(execution, &name)?;
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

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                store.remove_vertex_property(*vertex_id, property_id);
                return Ok(());
            }

            if let Some(edge) = bindings.edges.get(variable.as_ref()) {
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

            if let Some(vertex_id) = bindings.vertices.get(variable.as_ref()) {
                let vertex = store.vertex(*vertex_id).ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: variable.to_string(),
                    }
                })?;
                let had_label = store.vertex_has_label(*vertex_id, vertex, label_id);
                let vertex = store.remove_vertex_label(*vertex_id, vertex, label_id);
                store
                    .set_vertex(*vertex_id, vertex)
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
    vertex_id: VertexId,
    include_incident_edges: bool,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    if let Some(vertex) = store.vertex(vertex_id) {
        for label_id in store.vertex_labels(vertex_id, vertex) {
            bindings.add_vertex_label_delta(label_id, -1);
        }
    }
    if include_incident_edges {
        collect_detach_delete_edge_label_deltas(store, vertex_id, bindings)?;
    }
    Ok(())
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
    use super::*;
    use crate::facade::canonical_undirected_owner;
    use crate::gql_execution_context::GqlExecutionContext;
    use crate::test_labels::install_test_edge_payload_profile;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{BinaryOp, CmpOp, Expr, ExprKind, TruthValue, UnaryOp};
    use gleaph_gql::types::Decimal;
    use gleaph_gql_planner::plan::{PlanDiagnostics, PropertyAssignment};
    use gleaph_graph_kernel::entry::{EdgeSlotIndex, PropertyId};
    use ic_stable_lara::BucketLabelKey as LaraLabelId;
    use ic_stable_lara::traits::CsrEdge;

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

        collect_vertex_delete_label_deltas(&store, a, true, &mut bindings)
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
    fn detach_delete_over_sync_limit_errors_without_mutation() {
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
                PlanOp::InsertVertex {
                    variable: Some("c".into()),
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
                PlanOp::InsertEdge {
                    variable: None,
                    src: "a".into(),
                    dst: "c".into(),
                    direction: EdgeDirection::PointingRight,
                    labels: vec![],
                    properties: vec![],
                },
            ],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };
        store
            .execute_plan_mutations(&plan, GqlExecutionContext::default())
            .expect("setup hub");
        let before_a_u32: u32 = before_a.into();
        let a = VertexId::from(before_a_u32);

        // Incident degree is 2 (two outgoing edges); a ceiling of 1 must refuse.
        let err = store
            .detach_delete_vertex_bounded(a, 1)
            .expect_err("over-limit detach delete should error, not trap");
        assert!(matches!(
            err,
            GraphStoreError::VertexDeleteTooLarge {
                incident_degree: 2,
                limit: 1,
                ..
            }
        ));
        // Guard fires before any mutation: the hub and its edges are untouched.
        assert_eq!(
            store.directed_out_edges(a).expect("out edges intact").len(),
            2
        );

        // At the ceiling (degree == limit) the delete proceeds.
        store
            .detach_delete_vertex_bounded(a, 2)
            .expect("within limit detach delete");
        assert!(
            store
                .directed_out_edges(a)
                .map(|edges| edges.is_empty())
                .unwrap_or(true),
            "deleted hub should expose no outgoing edges"
        );
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
}
