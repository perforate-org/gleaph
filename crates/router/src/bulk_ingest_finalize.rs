//! Router orchestration for post-DML bulk-ingest finalize on graph shards.

use candid::Principal;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::federation::{
    BULK_INGEST_FINALIZE_MAX_DRAIN_RETRIES, BulkIngestFinalizeArgs, BulkIngestFinalizeResult,
    LocalVertexId, ShardId, is_gleaph_finalize_procedure_name,
};

use crate::graph_client;
use crate::state::RouterError;

/// True when any plan already contains an explicit Gleaph finalize `CALL`.
pub fn plans_include_explicit_finalize(plans: &[PhysicalPlan]) -> bool {
    plans
        .iter()
        .any(|plan| plan_ops_include_gleaph_finalize(&plan.ops))
}

pub async fn maybe_finalize_hot_vertices_after_dml(
    graph: Principal,
    shard_id: ShardId,
    plans: &[PhysicalPlan],
    hot_forward_vertices: &[LocalVertexId],
) -> Result<(), RouterError> {
    if !plans.iter().any(PhysicalPlan::has_dml) {
        return Ok(());
    }
    if plans_include_explicit_finalize(plans) || hot_forward_vertices.is_empty() {
        return Ok(());
    }
    finalize_hot_vertices_on_shard(graph, shard_id, hot_forward_vertices)
        .await
        .map_err(RouterError::InvalidArgument)
}

pub async fn finalize_hot_vertices_on_shard(
    graph: Principal,
    shard_id: ShardId,
    hot_forward_vertices: &[LocalVertexId],
) -> Result<(), String> {
    if hot_forward_vertices.is_empty() {
        return Ok(());
    }

    let mut report = graph_client::finalize_bulk_ingest(
        graph,
        BulkIngestFinalizeArgs {
            target_shard_id: shard_id,
            forward_vertices: hot_forward_vertices.to_vec(),
            reverse_vertices: Vec::new(),
            enqueue: true,
        },
    )
    .await?;

    let mut retries = 0u32;
    while report.instruction_budget_exhausted
        && report.remaining_queue_len > 0
        && retries < BULK_INGEST_FINALIZE_MAX_DRAIN_RETRIES
    {
        report = drain_finalize_on_shard(graph, shard_id).await?;
        retries += 1;
    }
    Ok(())
}

async fn drain_finalize_on_shard(
    graph: Principal,
    shard_id: ShardId,
) -> Result<BulkIngestFinalizeResult, String> {
    graph_client::finalize_bulk_ingest(
        graph,
        BulkIngestFinalizeArgs {
            target_shard_id: shard_id,
            forward_vertices: Vec::new(),
            reverse_vertices: Vec::new(),
            enqueue: false,
        },
    )
    .await
}

fn plan_ops_include_gleaph_finalize(ops: &[PlanOp]) -> bool {
    ops.iter().any(|op| match op {
        PlanOp::CallProcedure { name, .. } => is_gleaph_finalize_procedure_name(
            &name.iter().map(|part| part.as_ref()).collect::<Vec<_>>(),
        ),
        PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => plan_ops_include_gleaph_finalize(sub_plan),
        PlanOp::HashJoin { left, right, .. } => {
            plan_ops_include_gleaph_finalize(left) || plan_ops_include_gleaph_finalize(right)
        }
        PlanOp::CartesianProduct { left, right } => {
            plan_ops_include_gleaph_finalize(left) || plan_ops_include_gleaph_finalize(right)
        }
        PlanOp::SetOperation { right, .. } => plan_ops_include_gleaph_finalize(&right.ops),
        PlanOp::OptionalMatch { sub_plan } => plan_ops_include_gleaph_finalize(sub_plan),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            plan_ops_include_gleaph_finalize(&sub_plan.ops)
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_planner::plan::{PlanDiagnostics, YieldColumn};

    #[test]
    fn detects_explicit_finalize_call_in_plan() {
        let plan = PhysicalPlan {
            ops: vec![PlanOp::CallProcedure {
                name: vec!["GLEAPH".into(), "FINALIZE_BULK_INGEST".into()],
                args: vec![],
                yield_columns: Some(vec![YieldColumn {
                    name: "queued_forward".into(),
                    alias: None,
                }]),
                optional: false,
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };
        assert!(plans_include_explicit_finalize(std::slice::from_ref(&plan)));
    }

    #[test]
    fn insert_only_plan_does_not_count_as_explicit_finalize() {
        let plan = PhysicalPlan {
            ops: vec![PlanOp::InsertVertex {
                variable: Some("n".into()),
                labels: vec![],
                properties: vec![],
            }],
            diagnostics: PlanDiagnostics::default(),
            annotations: Default::default(),
            ..Default::default()
        };
        assert!(!plans_include_explicit_finalize(std::slice::from_ref(
            &plan
        )));
    }
}
