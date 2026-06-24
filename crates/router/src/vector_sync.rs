//! Inter-canister calls from the router to a derived vector-index canister and to graph shards for
//! the vector attach handshake (ADR 0031 Slice 4).
//!
//! The vector attach handshake is ordered so the graph shard's **local** routing is the source of
//! truth: the router first sets the shard's `FederationRouting.vector_index_canister`
//! ([`admin_set_graph_vector_index_canister`]), then attaches the shard to the vector canister
//! ([`admin_attach_shard_to_vector`]), and only then flips its durable `vector_index_attached`
//! registry bit. This mirrors the property-index attach in [`crate::index_sync`].

use candid::Principal;
#[cfg(not(feature = "pocket-ic-e2e"))]
use gleaph_graph_kernel::entry::GraphId;
#[cfg(not(feature = "pocket-ic-e2e"))]
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{VectorSearchRequest, VectorSearchResult};

// The attach-handshake helpers below are only driven by `finish_shard_vector_attach`, which is itself
// `#[cfg(not(pocket-ic-e2e))]` (the e2e harness drives the handshake legs from the test instead). The
// search helper, by contrast, is the real read path and must stay live under e2e.

/// Router → graph shard: set the shard's local derived vector-index target (handshake step 1).
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
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

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
pub async fn admin_set_graph_vector_index_canister(
    _graph_canister: Principal,
    _vector_index_canister: Principal,
) -> Result<(), String> {
    Ok(())
}

/// Router → vector canister: attach a graph shard so the vector index accepts its subject sync
/// (handshake step 2). A vector canister is the single target for the whole graph (ADR 0031 Slice 4
/// target model B), so ownership is keyed by `graph_id` alone — no property-index group descriptor.
#[cfg(all(target_family = "wasm", not(feature = "pocket-ic-e2e")))]
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

#[cfg(all(not(target_family = "wasm"), not(feature = "pocket-ic-e2e")))]
pub async fn admin_attach_shard_to_vector(
    _vector_index_canister: Principal,
    _graph_id: GraphId,
    _shard_id: ShardId,
    _shard_canister_principal: Principal,
) -> Result<(), String> {
    Ok(())
}

/// Router → vector canister: read-only exact `ivf_flat` search (ADR 0031 Slice 5). Invoked from the
/// Router composite query as a query call, mirroring [`crate::index_client::RouterIndexClient`].
#[cfg(target_family = "wasm")]
pub async fn vector_search(
    vector_index_canister: Principal,
    req: VectorSearchRequest,
) -> Result<VectorSearchResult, String> {
    use ic_cdk::call::Call;

    Call::bounded_wait(vector_index_canister, "vector_search")
        .with_args(&(req,))
        .await
        .map_err(|e| format!("vector vector_search call failed: {e}"))?
        .candid::<Result<VectorSearchResult, gleaph_graph_kernel::vector_index::VectorIndexError>>()
        .map_err(|e| format!("vector vector_search decode failed: {e}"))?
        .map_err(|e| format!("vector vector_search rejected: {e}"))
}

#[cfg(not(target_family = "wasm"))]
pub async fn vector_search(
    _vector_index_canister: Principal,
    _req: VectorSearchRequest,
) -> Result<VectorSearchResult, String> {
    Ok(VectorSearchResult { hits: Vec::new() })
}
