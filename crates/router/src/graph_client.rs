//! Inter-canister calls from router to graph shards.

use candid::Principal;
use gleaph_graph_kernel::federation::{PostingBackfillArgs, PostingBackfillResult};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, LabelTelemetryEventWire, MutationId, MutationOutcomeWire,
    ShardEventSeq,
};

#[cfg(target_family = "wasm")]
async fn call_graph<T: candid::CandidType, R: candid::CandidType + serde::de::DeserializeOwned>(
    graph: Principal,
    method: &str,
    args: T,
) -> Result<R, String> {
    use ic_cdk::call::Call;

    let result: (R,) = Call::bounded_wait(graph, method)
        .with_arg(&args)
        .await
        .map_err(|e| format!("graph {method} call failed: {e}"))?
        .candid()
        .map_err(|e| format!("graph {method} decode failed: {e}"))?;
    Ok(result.0)
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

    let result: (R,) = Call::bounded_wait(graph, method)
        .with_args(args)
        .await
        .map_err(|e| format!("graph {method} call failed: {e}"))?
        .candid()
        .map_err(|e| format!("graph {method} decode failed: {e}"))?;
    Ok(result.0)
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

pub async fn ack_label_telemetry_event(graph: Principal, seq: ShardEventSeq) -> Result<(), String> {
    call_graph(graph, "ack_label_telemetry_event", seq).await
}

pub async fn list_pending_label_telemetry_events(
    graph: Principal,
    from_seq: ShardEventSeq,
    limit: u32,
) -> Result<Vec<LabelTelemetryEventWire>, String> {
    call_graph_args(
        graph,
        "list_pending_label_telemetry_events",
        &(from_seq, limit),
    )
    .await
}

pub async fn get_mutation_outcome(
    graph: Principal,
    mutation_id: MutationId,
) -> Result<Option<MutationOutcomeWire>, String> {
    call_graph(graph, "get_mutation_outcome", mutation_id).await
}

pub async fn backfill_label_postings(
    graph: Principal,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    call_graph_result(graph, "backfill_label_postings", args).await
}

pub async fn backfill_property_postings(
    graph: Principal,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    call_graph_result(graph, "backfill_property_postings", args).await
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

#[expect(
    dead_code,
    reason = "wired in router admin edge backfill step (ADR 0009 phase C follow-up)"
)]
pub async fn backfill_edge_property_postings(
    graph: Principal,
    args: gleaph_graph_kernel::federation::EdgePostingBackfillArgs,
) -> Result<gleaph_graph_kernel::federation::EdgePostingBackfillResult, String> {
    call_graph_result(graph, "backfill_edge_property_postings", args).await
}
