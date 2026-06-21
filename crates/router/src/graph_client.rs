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

/// Smallest tracked unapplied `mutation_id` whose graph-index postings are still in the
/// shard's repair journal (ADR 0029 Phase 2/3). `None` means all tracked index work
/// drained: a read for mutation `M` is index-satisfied on this shard iff this is `None`
/// or `M < value`.
#[cfg(target_family = "wasm")]
pub async fn index_pending_min_mutation_id(graph: Principal) -> Result<Option<MutationId>, String> {
    use ic_cdk::call::Call;

    Call::bounded_wait(graph, "index_pending_min_mutation_id")
        .await
        .map_err(|e| format!("graph index_pending_min_mutation_id call failed: {e}"))?
        .candid()
        .map_err(|e| format!("graph index_pending_min_mutation_id decode failed: {e}"))
}

#[cfg(not(target_family = "wasm"))]
pub async fn index_pending_min_mutation_id(
    _graph: Principal,
) -> Result<Option<MutationId>, String> {
    Err("graph index_pending_min_mutation_id unavailable in native builds".to_string())
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
    catalog: gleaph_graph_kernel::index::IndexedPropertyCatalog,
) -> Result<PostingBackfillResult, String> {
    let req = gleaph_graph_kernel::federation::VertexPropertyBackfillRequest { args, catalog };
    call_graph_result(graph, "backfill_vertex_property_postings", req).await
}

pub async fn backfill_edge_property_postings(
    graph: Principal,
    args: gleaph_graph_kernel::federation::EdgePostingBackfillArgs,
    catalog: gleaph_graph_kernel::index::IndexedPropertyCatalog,
) -> Result<gleaph_graph_kernel::federation::EdgePostingBackfillResult, String> {
    let req = gleaph_graph_kernel::federation::EdgePropertyBackfillRequest { args, catalog };
    call_graph_result(graph, "backfill_edge_property_postings", req).await
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

    use gleaph_graph_kernel::federation::ElementIdEncodingKey;
    use gleaph_graph_kernel::plan_exec::GqlExecutionMode;

    #[test]
    fn native_build_graph_client_returns_unavailable() {
        let fut = execute_plan_on_graph(
            Principal::anonymous(),
            ExecutePlanArgs {
                target_shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
                mutation_id: None,
                plan_blob: vec![],
                params_blob: vec![],
                mode: GqlExecutionMode::Query,
                seed_bindings_blob: None,
                resolved_labels: None,
                resolved_properties: None,
                indexed_properties: None,
            },
        );
        let err = futures::executor::block_on(fut).expect_err("native unavailable");
        assert!(err.contains("unavailable"));
    }
}
