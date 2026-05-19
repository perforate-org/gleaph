//! Inter-canister clients for federated graph shard queries.

use crate::plan::PlanQueryError;
use candid::Principal;
use gleaph_graph_kernel::federation::{
    FederatedExpandNeighbor, FederatedIncomingExpandArgs, FederatedOutgoingExpandArgs,
};

#[cfg(target_family = "wasm")]
pub async fn federated_incoming_expand(
    graph_canister: Principal,
    args: FederatedIncomingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    use ic_cdk::call::Call;

    let hits: Vec<FederatedExpandNeighbor> =
        Call::bounded_wait(graph_canister, "federated_incoming_expand")
            .with_args(&(args,))
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_incoming_expand",
                detail: format!("{e:?}"),
            })?
            .candid()
            .map_err(|_| PlanQueryError::FederatedIndexCall {
                op: "federated_incoming_expand",
                detail: "candid decode failed".into(),
            })?;
    Ok(hits)
}

#[cfg(not(target_family = "wasm"))]
pub async fn federated_incoming_expand(
    _graph_canister: Principal,
    _args: FederatedIncomingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    Err(PlanQueryError::UnsupportedOp(
        "federated_incoming_expand is only available on wasm",
    ))
}

#[cfg(target_family = "wasm")]
pub async fn federated_outgoing_expand(
    graph_canister: Principal,
    args: FederatedOutgoingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    use ic_cdk::call::Call;

    let hits: Vec<FederatedExpandNeighbor> =
        Call::bounded_wait(graph_canister, "federated_outgoing_expand")
            .with_args(&(args,))
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_outgoing_expand",
                detail: format!("{e:?}"),
            })?
            .candid()
            .map_err(|_| PlanQueryError::FederatedIndexCall {
                op: "federated_outgoing_expand",
                detail: "candid decode failed".into(),
            })?;
    Ok(hits)
}

#[cfg(not(target_family = "wasm"))]
pub async fn federated_outgoing_expand(
    _graph_canister: Principal,
    _args: FederatedOutgoingExpandArgs,
) -> Result<Vec<FederatedExpandNeighbor>, PlanQueryError> {
    Err(PlanQueryError::UnsupportedOp(
        "federated_outgoing_expand is only available on wasm",
    ))
}
