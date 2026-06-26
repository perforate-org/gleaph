//! Gleaph-specific `CALL` procedures for bulk-ingest finalize (mutation executor only).

use super::error::PlanMutationError;
use super::executor::PlanMutationBindings;
use crate::facade::{BulkIngestFinalizeReport, BulkIngestFinalizeSpec, GraphStore};
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql_planner::plan::{PlanOp, Str, YieldColumn};
use gleaph_graph_kernel::gql_dialect::{
    GLEAPH_DRAIN_DEFERRED_MAINTENANCE, GLEAPH_FINALIZE_BULK_INGEST,
    GLEAPH_FINALIZE_FORWARD_EDGE_SPAN, GLEAPH_VERTEX_LIST,
};
use ic_stable_lara::VertexId;
use std::collections::BTreeMap;

const MAX_FINALIZE_VERTICES: usize = 256;

/// True when `ops` contains a Gleaph finalize `CALL` that must run on the mutation executor.
pub fn plan_contains_gleaph_finalize_call(ops: &[PlanOp]) -> bool {
    ops.iter().any(
        |op| matches!(op, PlanOp::CallProcedure { name, .. } if is_gleaph_finalize_procedure(name)),
    )
}

pub fn is_gleaph_finalize_procedure(name: &[Str]) -> bool {
    GLEAPH_FINALIZE_BULK_INGEST.matches_exact(name)
        || GLEAPH_FINALIZE_FORWARD_EDGE_SPAN.matches_exact(name)
        || GLEAPH_DRAIN_DEFERRED_MAINTENANCE.matches_exact(name)
}

pub fn execute_call_procedure(
    store: &GraphStore,
    name: &[Str],
    args: &[Expr],
    yield_columns: Option<&[YieldColumn]>,
    optional: bool,
    bindings: &mut PlanMutationBindings,
) -> Result<(), PlanMutationError> {
    if optional {
        return Err(PlanMutationError::UnsupportedOp("optional CallProcedure"));
    }
    if !is_gleaph_finalize_procedure(name) {
        return Err(PlanMutationError::UnknownGleaphProcedure {
            name: format_procedure_name(name),
        });
    }

    let report = if GLEAPH_FINALIZE_BULK_INGEST.matches_exact(name) {
        if args.len() != 2 {
            return Err(invalid_args(
                "GLEAPH.FINALIZE_BULK_INGEST",
                "two vertex-list arguments",
            ));
        }
        let forward = resolve_vertex_list_expr(&args[0], bindings)?;
        let reverse = resolve_vertex_list_expr(&args[1], bindings)?;
        check_vertex_limit(forward.len() + reverse.len())?;
        store.finalize_bulk_ingest(&BulkIngestFinalizeSpec {
            forward_vertices: forward,
            reverse_vertices: reverse,
        })?
    } else if GLEAPH_FINALIZE_FORWARD_EDGE_SPAN.matches_exact(name) {
        if args.len() != 1 {
            return Err(invalid_args(
                "GLEAPH.FINALIZE_FORWARD_EDGE_SPAN",
                "one vertex-list argument",
            ));
        }
        let forward = resolve_vertex_list_expr(&args[0], bindings)?;
        if forward.len() != 1 {
            return Err(invalid_args(
                "GLEAPH.FINALIZE_FORWARD_EDGE_SPAN",
                "exactly one vertex",
            ));
        }
        store.finalize_bulk_ingest(&BulkIngestFinalizeSpec {
            forward_vertices: forward,
            reverse_vertices: vec![],
        })?
    } else if GLEAPH_DRAIN_DEFERRED_MAINTENANCE.matches_exact(name) {
        if !args.is_empty() {
            return Err(invalid_args(
                "GLEAPH.DRAIN_DEFERRED_MAINTENANCE",
                "no arguments",
            ));
        }
        let maintenance = store.run_bulk_ingest_finalize_drain()?;
        BulkIngestFinalizeReport {
            maintenance,
            queued_forward: 0,
            queued_reverse: 0,
        }
    } else {
        return Err(PlanMutationError::UnknownGleaphProcedure {
            name: format_procedure_name(name),
        });
    };

    if let Some(columns) = yield_columns {
        populate_finalize_yields(columns, &report, bindings);
    }
    Ok(())
}

fn format_procedure_name(name: &[Str]) -> String {
    name.iter()
        .map(|part| part.as_ref())
        .collect::<Vec<_>>()
        .join(".")
}

fn invalid_args(procedure: &'static str, expected: &'static str) -> PlanMutationError {
    PlanMutationError::InvalidFinalizeProcedureArgs {
        procedure,
        expected,
    }
}

fn check_vertex_limit(count: usize) -> Result<(), PlanMutationError> {
    if count > MAX_FINALIZE_VERTICES {
        return Err(PlanMutationError::TooManyFinalizeVertices {
            count,
            max: MAX_FINALIZE_VERTICES,
        });
    }
    Ok(())
}

fn resolve_vertex_list_expr(
    expr: &Expr,
    bindings: &PlanMutationBindings,
) -> Result<Vec<VertexId>, PlanMutationError> {
    match &expr.kind {
        ExprKind::Variable(var) => {
            let vertex_id = bindings.vertices.get(var).copied().ok_or_else(|| {
                PlanMutationError::MissingVertexBinding {
                    variable: var.clone(),
                }
            })?;
            Ok(vec![vertex_id])
        }
        ExprKind::FunctionCall { name, args, .. }
            if GLEAPH_VERTEX_LIST.matches_exact(&name.parts) =>
        {
            let mut vertices = Vec::with_capacity(args.len());
            for arg in args {
                let ExprKind::Variable(var) = &arg.kind else {
                    return Err(PlanMutationError::InvalidFinalizeVertexListArg);
                };
                let vertex_id = bindings.vertices.get(var).copied().ok_or_else(|| {
                    PlanMutationError::MissingVertexBinding {
                        variable: var.clone(),
                    }
                })?;
                vertices.push(vertex_id);
            }
            Ok(vertices)
        }
        _ => Err(PlanMutationError::InvalidFinalizeVertexListArg),
    }
}

fn populate_finalize_yields(
    columns: &[YieldColumn],
    report: &BulkIngestFinalizeReport,
    bindings: &mut PlanMutationBindings,
) {
    let values = finalize_yield_values(report);
    for column in columns {
        let Some(value) = values.get(column.name.as_ref()) else {
            continue;
        };
        let key = column
            .alias
            .as_ref()
            .map(|alias| alias.to_string())
            .unwrap_or_else(|| column.name.to_string());
        bindings.procedure_yields.insert(key, value.clone());
    }
}

fn finalize_yield_values(report: &BulkIngestFinalizeReport) -> BTreeMap<&'static str, Value> {
    BTreeMap::from([
        (
            "queued_forward",
            Value::Int64(i64::from(report.queued_forward)),
        ),
        (
            "queued_reverse",
            Value::Int64(i64::from(report.queued_reverse)),
        ),
        (
            "processed_work_items",
            Value::Int64(i64::from(report.maintenance.work.processed_work_items)),
        ),
        (
            "remaining_queue_len",
            Value::Int64(report.maintenance.remaining_queue_len() as i64),
        ),
        (
            "instruction_budget_exhausted",
            Value::Bool(report.maintenance.instruction_budget_exhausted),
        ),
        (
            "instructions_used",
            Value::Int64(report.maintenance.instructions_used as i64),
        ),
    ])
}
