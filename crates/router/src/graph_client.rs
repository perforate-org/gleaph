//! Inter-canister calls from router to graph shards.

use candid::Principal;
use gleaph_graph_kernel::federation::{
    AddGraphPeerArgs, BootstrapGraphPeersArgs, RemoveGraphPeerArgs, ShardId,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanResult, ExecuteProgramArgs, ExecuteProgramResult,
};

#[cfg(target_family = "wasm")]
async fn call_graph<T: candid::CandidType, R: candid::CandidType>(
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

#[cfg(not(target_family = "wasm"))]
async fn call_graph<T: candid::CandidType, R: candid::CandidType>(
    _graph: Principal,
    method: &str,
    _args: T,
) -> Result<R, String> {
    Err(format!("graph {method} unavailable in native builds"))
}

pub async fn execute_program_on_graph(
    graph: Principal,
    target_shard_id: ShardId,
    program_blob: Vec<u8>,
    params_blob: Vec<u8>,
    mode: gleaph_graph_kernel::plan_exec::GqlExecutionMode,
) -> Result<ExecuteProgramResult, String> {
    let args = ExecuteProgramArgs {
        target_shard_id,
        program_blob,
        params_blob,
        mode,
    };
    let method = match mode {
        gleaph_graph_kernel::plan_exec::GqlExecutionMode::Query => "execute_program_query",
        gleaph_graph_kernel::plan_exec::GqlExecutionMode::Update => "execute_program_update",
    };
    call_graph(graph, method, args).await
}

pub async fn bootstrap_graph_peers(
    graph: Principal,
    peers: Vec<Principal>,
) -> Result<(), String> {
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
    call_graph(
        graph,
        "remove_graph_peer",
        RemoveGraphPeerArgs { peer },
    )
    .await
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
