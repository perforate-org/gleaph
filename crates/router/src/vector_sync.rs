//! Inter-canister calls from the router to a derived vector-index canister and to graph shards for
//! the vector attach handshake (ADR 0031 Slice 4).
//!
//! The vector attach handshake is ordered so the graph shard's **local** routing is the source of
//! truth: the router first sets the shard's `FederationRouting.vector_index_canister`
//! ([`admin_set_graph_vector_index_canister`]), then attaches the shard to the vector canister
//! ([`admin_attach_shard_to_vector`]), and only then flips its durable `vector_index_attached`
//! registry bit. This mirrors the property-index attach in [`crate::index_sync`].

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;

/// Router → graph shard: set the shard's local derived vector-index target (handshake step 1).
#[cfg(target_family = "wasm")]
pub async fn admin_set_graph_vector_index_canister(
    graph_canister: Principal,
    vector_index_canister: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(graph_canister, "admin_set_vector_index_canister")
        .with_args(&(vector_index_canister,))
        .await
        .map_err(|e| format!("graph admin_set_vector_index_canister call failed: {e}"))?
        .candid::<Result<(), String>>()
        .map_err(|e| format!("graph admin_set_vector_index_canister decode failed: {e}"))?
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_set_graph_vector_index_canister(
    _graph_canister: Principal,
    _vector_index_canister: Principal,
) -> Result<(), String> {
    Ok(())
}

/// Router → vector canister: attach a graph shard so the vector index accepts its subject sync
/// (handshake step 2). A vector canister is the single target for the whole graph (ADR 0031 Slice 4
/// target model B), so ownership is keyed by `graph_id` alone — no property-index group descriptor.
#[cfg(target_family = "wasm")]
pub async fn admin_attach_shard_to_vector(
    vector_index_canister: Principal,
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    use ic_cdk::call::Call;

    Call::unbounded_wait(vector_index_canister, "admin_attach_shard_canister")
        .with_args(&(graph_id, shard_id, shard_canister_principal))
        .await
        .map_err(|e| format!("vector admin_attach_shard_canister call failed: {e}"))?
        .candid()
        .map_err(|e| format!("vector admin_attach_shard_canister decode failed: {e}"))?
}

#[cfg(not(target_family = "wasm"))]
pub async fn admin_attach_shard_to_vector(
    _vector_index_canister: Principal,
    _graph_id: GraphId,
    _shard_id: ShardId,
    _shard_canister_principal: Principal,
) -> Result<(), String> {
    Ok(())
}
