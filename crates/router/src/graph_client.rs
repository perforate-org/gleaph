//! Inter-canister calls from router to graph shards.

use candid::Principal;
use gleaph_graph_kernel::federation::{
    BulkIngestFinalizeArgs, BulkIngestFinalizeResult, PostingBackfillArgs, PostingBackfillResult,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, GraphMutationJournalEntryWire, LabelStatsDeltaEventWire,
    MutationId, ShardEventSeq,
};

#[cfg(target_family = "wasm")]
async fn call_graph<T: candid::CandidType, R: candid::CandidType + serde::de::DeserializeOwned>(
    graph: Principal,
    method: &str,
    args: T,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    Call::bounded_wait(graph, method)
        .with_arg(&args)
        .await
        .map_err(|e| format!("graph {method} call failed: {e}"))?
        .candid()
        .map_err(|e| format!("graph {method} decode failed: {e}"))
}

/// Graph canister methods that return `Result<R, text>` on the wire (not a bare `R` tuple).
#[cfg(target_family = "wasm")]
async fn call_graph_result<
    T: candid::CandidType,
    R: candid::CandidType + serde::de::DeserializeOwned,
>(
    graph: Principal,
    method: &str,
    args: T,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    let reply: Result<R, String> = Call::bounded_wait(graph, method)
        .with_arg(&args)
        .await
        .map_err(|e| format!("graph {method} call failed: {e}"))?
        .candid()
        .map_err(|e| format!("graph {method} decode failed: {e}"))?;
    reply
}

#[cfg(not(target_family = "wasm"))]
async fn call_graph_result<T: candid::CandidType, R: candid::CandidType>(
    _graph: Principal,
    method: &str,
    _args: T,
) -> Result<R, String> {
    Err(format!("graph {method} unavailable in native builds"))
}

#[cfg(target_family = "wasm")]
async fn call_graph_args<
    T: candid::utils::ArgumentEncoder,
    R: candid::CandidType + serde::de::DeserializeOwned,
>(
    graph: Principal,
    method: &str,
    args: &T,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    Call::bounded_wait(graph, method)
        .with_args(args)
        .await
        .map_err(|e| format!("graph {method} call failed: {e}"))?
        .candid()
        .map_err(|e| format!("graph {method} decode failed: {e}"))
}

#[cfg(not(target_family = "wasm"))]
async fn call_graph<T: candid::CandidType, R: candid::CandidType>(
    _graph: Principal,
    method: &str,
    _args: T,
) -> Result<R, String> {
    Err(format!("graph {method} unavailable in native builds"))
}

#[cfg(not(target_family = "wasm"))]
async fn call_graph_args<T, R: candid::CandidType>(
    _graph: Principal,
    method: &str,
    _args: &T,
) -> Result<R, String> {
    Err(format!("graph {method} unavailable in native builds"))
}

pub async fn execute_plan_on_graph(
    graph: Principal,
    args: ExecutePlanArgs,
) -> Result<ExecutePlanResult, String> {
    let method = match args.mode {
        gleaph_graph_kernel::plan_exec::GqlExecutionMode::Query => "execute_plan_query",
        gleaph_graph_kernel::plan_exec::GqlExecutionMode::Update => "execute_plan_update",
    };
    call_graph_result(graph, method, args).await
}

pub async fn ack_label_stats_deltas_through(
    graph: Principal,
    through_seq: ShardEventSeq,
) -> Result<(), String> {
    call_graph(graph, "ack_label_stats_deltas_through", through_seq).await
}

pub async fn list_pending_label_stats_deltas(
    graph: Principal,
    from_seq: ShardEventSeq,
    limit: u32,
) -> Result<Vec<LabelStatsDeltaEventWire>, String> {
    call_graph_args(graph, "list_pending_label_stats_deltas", &(from_seq, limit)).await
}

pub async fn get_mutation_journal_entry(
    graph: Principal,
    mutation_id: MutationId,
) -> Result<Option<GraphMutationJournalEntryWire>, String> {
    call_graph(graph, "get_mutation_journal_entry", mutation_id).await
}

pub async fn backfill_label_postings(
    graph: Principal,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    call_graph_result(graph, "backfill_label_postings", args).await
}

pub async fn backfill_vertex_property_postings(
    graph: Principal,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    call_graph_result(graph, "backfill_vertex_property_postings", args).await
}

pub async fn register_indexed_property(
    graph: Principal,
    args: gleaph_graph_kernel::index::RegisterIndexedPropertyArgs,
) -> Result<(), String> {
    call_graph_result(graph, "register_indexed_property", args).await
}

pub async fn unregister_indexed_property(
    graph: Principal,
    args: gleaph_graph_kernel::index::RegisterIndexedPropertyArgs,
) -> Result<(), String> {
    call_graph_result(graph, "unregister_indexed_property", args).await
}

pub async fn register_indexed_edge_index(
    graph: Principal,
    args: gleaph_graph_kernel::index::RegisterIndexedEdgeIndexArgs,
) -> Result<(), String> {
    call_graph_result(graph, "register_indexed_edge_index", args).await
}

pub async fn unregister_indexed_edge_index(
    graph: Principal,
    args: gleaph_graph_kernel::index::RegisterIndexedEdgeIndexArgs,
) -> Result<(), String> {
    call_graph_result(graph, "unregister_indexed_edge_index", args).await
}

pub async fn backfill_edge_property_postings(
    graph: Principal,
    args: gleaph_graph_kernel::federation::EdgePostingBackfillArgs,
) -> Result<gleaph_graph_kernel::federation::EdgePostingBackfillResult, String> {
    call_graph_result(graph, "backfill_edge_property_postings", args).await
}

pub async fn finalize_bulk_ingest(
    graph: Principal,
    args: BulkIngestFinalizeArgs,
) -> Result<BulkIngestFinalizeResult, String> {
    call_graph_result(graph, "finalize_bulk_ingest", args).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_build_graph_client_returns_unavailable() {
        let fut = execute_plan_on_graph(
            Principal::anonymous(),
            ExecutePlanArgs {
                target_shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                mutation_id: None,
                plan_blob: vec![],
                params_blob: vec![],
                mode: gleaph_graph_kernel::plan_exec::GqlExecutionMode::Query,
                seed_bindings_blob: None,
                resolved_labels: None,
                resolved_properties: None,
            },
        );
        let err = futures::executor::block_on(fut).expect_err("native unavailable");
        assert!(err.contains("unavailable"));
    }
}
