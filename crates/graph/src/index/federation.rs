//! Inter-canister clients for federated graph shard queries.

use crate::plan::PlanQueryError;
use candid::Principal;
use gleaph_graph_kernel::federation::{FederatedExpandArgs, FederatedExpandNeighbor};

#[cfg(target_family = "wasm")]
pub async fn call_graph_federated_expand(
    graph_canister: Principal,
    args: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    use ic_cdk::call::Call;

    let hits: Vec<FederatedExpandNeighbor> = Call::bounded_wait(graph_canister, "federated_expand")
        .with_args(&(args,))
        .await
        .map_err(|e| PlanQueryError::FederatedIndexCall {
            op: "federated_expand",
            detail: format!("{e:?}"),
        })?
        .candid()
        .map_err(|_| PlanQueryError::FederatedIndexCall {
            op: "federated_expand",
            detail: "candid decode failed".into(),
        })?;
    Ok(hits)
}

#[cfg(not(target_family = "wasm"))]
pub async fn call_graph_federated_expand(
    _graph_canister: Principal,
    _args: FederatedExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    Err(PlanQueryError::UnsupportedOp(
        "federated_expand is only available on wasm",
    ))
}
