//! Inter-canister calls from router to graph shards.

use candid::Principal;
use gleaph_graph_kernel::federation::{
    AddGraphPeerArgs, BootstrapGraphPeersArgs, LabelPostingBackfillArgs,
    LabelPostingBackfillResult, RemoveGraphPeerArgs,
};
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

pub async fn bootstrap_graph_peers(graph: Principal, peers: Vec<Principal>) -> Result<(), String> {
    call_graph(
        graph,
        "bootstrap_graph_peers",
        BootstrapGraphPeersArgs { peers },
    )
    .await
}

pub async fn add_graph_peer(graph: Principal, peer: Principal) -> Result<(), String> {
    call_graph(graph, "add_graph_peer", AddGraphPeerArgs { peer }).await
}

pub async fn remove_graph_peer(graph: Principal, peer: Principal) -> Result<(), String> {
    call_graph(graph, "remove_graph_peer", RemoveGraphPeerArgs { peer }).await
}

pub async fn execute_plan_on_graph(
    graph: Principal,
    args: ExecutePlanArgs,
) -> Result<ExecutePlanResult, String> {
    let method = match args.mode {
        gleaph_graph_kernel::plan_exec::GqlExecutionMode::Query => "execute_plan_query",
        gleaph_graph_kernel::plan_exec::GqlExecutionMode::Update => "execute_plan_update",
    };
    call_graph(graph, method, args).await
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
    args: LabelPostingBackfillArgs,
) -> Result<LabelPostingBackfillResult, String> {
    call_graph(graph, "backfill_label_postings", args).await
}
