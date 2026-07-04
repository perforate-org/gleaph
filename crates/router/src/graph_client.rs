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

pub async fn backfill_vertex_embeddings(
    graph: Principal,
    args: gleaph_graph_kernel::federation::EmbeddingBackfillArgs,
    catalog: gleaph_graph_kernel::vector_index::IndexedEmbeddingCatalog,
) -> Result<gleaph_graph_kernel::federation::EmbeddingBackfillResult, String> {
    let req = gleaph_graph_kernel::federation::VertexEmbeddingBackfillRequest { args, catalog };
    call_graph_result(graph, "backfill_vertex_embeddings", req).await
}

pub async fn ingest_vertex_embedding(
    graph: Principal,
    args: gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionArgs,
) -> Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String> {
    call_graph_result(graph, "admin_ingest_vertex_embedding", args).await
}

pub async fn finalize_bulk_ingest(
    graph: Principal,
    args: BulkIngestFinalizeArgs,
) -> Result<BulkIngestFinalizeResult, String> {
    call_graph_result(graph, "finalize_bulk_ingest", args).await
}

/// Replicated `Acquire` commit proof for each claim (ADR 0030 §Timeout). An `update` call so the
/// answer is replicated: a single-replica query is insufficient evidence to act on absence.
pub async fn read_unique_effect_proof(
    graph: Principal,
    claim_ids: Vec<gleaph_graph_kernel::federation::ClaimId>,
) -> Result<Vec<gleaph_graph_kernel::federation::UniqueAcquireProof>, String> {
    call_graph(graph, "read_unique_effect_proof", claim_ids).await
}

/// One page of a mutation's pinned `Release` effects with `effect_ordinal > after_ordinal`, capped
/// at `limit` (the shard clamps to its own hard maximum), reconciled by the Router per
/// `owner_element_id` (ADR 0030 slice 5b). An `update` call so the answer is replicated.
pub async fn read_unique_release_effects(
    graph: Principal,
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    after_ordinal: Option<u32>,
    limit: u32,
) -> Result<Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt>, String> {
    call_graph_args(
        graph,
        "read_unique_release_effects",
        &(mutation_id, after_ordinal, limit),
    )
    .await
}

/// One page of **all** of a mutation's pinned effects (`Acquire` and `Release`) with `effect_ordinal
/// > after_ordinal`, capped at `limit` (the shard clamps to its own hard maximum). Backs the unified
/// slice-6 effect recovery (Driver 2), which discovers every un-acked effect — including an orphan
/// `Acquire`. An `update` call so the answer is replicated.
pub async fn read_unique_mutation_effects(
    graph: Principal,
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    after_ordinal: Option<u32>,
    limit: u32,
) -> Result<Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt>, String> {
    call_graph_args(
        graph,
        "read_unique_mutation_effects",
        &(mutation_id, after_ordinal, limit),
    )
    .await
}

/// Unpins (acks) unique effects after the Router has durably applied them (ADR 0030, per-effect).
pub async fn ack_unique_effects(
    graph: Principal,
    effect_ids: Vec<gleaph_graph_kernel::federation::EffectId>,
) -> Result<(), String> {
    call_graph(graph, "ack_unique_effects", effect_ids).await
}

/// DROP drain (ADR 0030 slice 10): purge one bounded page of a `ShardLocalGlobal` constraint's local
/// unique entries on its **owning shard** and report whether that constraint's local table is now
/// empty. An `update` so the purge is replicated. The Router gates the terminal `Removed` on
/// `Ok(true)`; an unreachable owner (`Err`) keeps the constraint `Dropping`.
pub async fn purge_local_unique_constraint(
    graph: Principal,
    constraint_id: gleaph_graph_kernel::entry::ConstraintNameId,
    budget: u32,
) -> Result<bool, String> {
    call_graph_args(
        graph,
        "purge_local_unique_constraint",
        &(constraint_id, budget),
    )
    .await
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
                unique_claims: None,
                constrained_properties: None,
                local_unique_claims: None,
                local_constrained_properties: None,
                indexed_embeddings: None,
                resolved_search_blob: None,
            },
        );
        let err = futures::executor::block_on(fut).expect_err("native unavailable");
        assert!(err.contains("unavailable"));
    }
}
