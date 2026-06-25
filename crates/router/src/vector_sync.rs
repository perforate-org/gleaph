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
use gleaph_graph_kernel::vector_index::{
    VectorCentroidCacheStatus, VectorMaintenancePolicy, VectorMaintenanceRecommendation,
    VectorMaintenanceState, VectorMaintenanceStepRequest, VectorMaintenanceStepResult,
    VectorPartitionHealthStep, VectorPartitionHealthSummary, VectorPartitionPageHealth,
    VectorRebuildStatus, VectorSearchRequest, VectorSearchResult, VectorSlabStats,
    VectorSlabStatsStep,
};

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

// --- ADR 0031 Slice 10: Router-forwarded vector maintenance surface ---
//
// Each helper forwards to a router-guarded vector-canister admin endpoint (which returns
// `Result<T, String>`). Reads use `bounded_wait` (query), mutators/drivers use `unbounded_wait`
// (update). Under native builds the helpers fail closed: actual forwarding is verified by PocketIC
// e2e, while native unit tests only cover Router policy validation / CRUD / RBAC / readiness gating.

/// Generates one Router→vector forward helper. Defined twice (wasm real call / native stub) via a
/// cfg gate on the macro itself, so the call body never needs a cfg attribute on an inner block.
#[cfg(target_family = "wasm")]
macro_rules! forward_vector {
    ($fn:ident, $method:literal, $waiter:ident, (), $ret:ty) => {
        pub async fn $fn(canister: Principal) -> Result<$ret, String> {
            use ic_cdk::call::Call;
            Call::$waiter(canister, $method)
                .await
                .map_err(|e| format!(concat!("vector ", $method, " call failed: {}"), e))?
                .candid::<Result<$ret, String>>()
                .map_err(|e| format!(concat!("vector ", $method, " decode failed: {}"), e))?
        }
    };
    ($fn:ident, $method:literal, $waiter:ident, ($($arg:ident: $aty:ty),+ $(,)?), $ret:ty) => {
        pub async fn $fn(canister: Principal, $($arg: $aty),+) -> Result<$ret, String> {
            use ic_cdk::call::Call;
            Call::$waiter(canister, $method)
                .with_args(&($($arg,)+))
                .await
                .map_err(|e| format!(concat!("vector ", $method, " call failed: {}"), e))?
                .candid::<Result<$ret, String>>()
                .map_err(|e| format!(concat!("vector ", $method, " decode failed: {}"), e))?
        }
    };
}

#[cfg(not(target_family = "wasm"))]
macro_rules! forward_vector {
    ($fn:ident, $method:literal, $waiter:ident, (), $ret:ty) => {
        pub async fn $fn(canister: Principal) -> Result<$ret, String> {
            let _ = &canister;
            Err(concat!("vector ", $method, " is unavailable in native builds").to_string())
        }
    };
    ($fn:ident, $method:literal, $waiter:ident, ($($arg:ident: $aty:ty),+ $(,)?), $ret:ty) => {
        pub async fn $fn(canister: Principal, $($arg: $aty),+) -> Result<$ret, String> {
            let _ = &canister;
            $(let _ = &$arg;)+
            Err(concat!("vector ", $method, " is unavailable in native builds").to_string())
        }
    };
}

// Reads (composite-query forwards): bounded_wait query calls.
forward_vector!(
    forward_admin_vector_partition_health,
    "admin_vector_partition_health",
    bounded_wait,
    (index_id: u32),
    VectorPartitionHealthSummary
);
forward_vector!(
    forward_admin_vector_partition_health_step,
    "admin_vector_partition_health_step",
    bounded_wait,
    (index_id: u32, cursor: Option<Vec<u8>>, max_pages: u32),
    VectorPartitionHealthStep
);
forward_vector!(
    forward_admin_vector_rebuild_status,
    "admin_vector_rebuild_status",
    bounded_wait,
    (index_id: u32),
    VectorRebuildStatus
);
forward_vector!(
    forward_admin_vector_slab_stats,
    "admin_vector_slab_stats",
    bounded_wait,
    (index_id: Option<u32>),
    VectorSlabStats
);
forward_vector!(
    forward_admin_vector_slab_stats_step,
    "admin_vector_slab_stats_step",
    bounded_wait,
    (cursor: Option<Vec<u8>>, max_pages: u32, index_id: Option<u32>),
    VectorSlabStatsStep
);
forward_vector!(
    forward_admin_vector_centroid_cache_status,
    "admin_vector_centroid_cache_status",
    bounded_wait,
    (),
    VectorCentroidCacheStatus
);
forward_vector!(
    forward_admin_vector_maintenance_status,
    "admin_vector_maintenance_status",
    bounded_wait,
    (index_id: u32),
    VectorMaintenanceState
);

// Mutators / drivers: unbounded_wait update calls.
forward_vector!(
    forward_admin_start_vector_rebuild,
    "admin_start_vector_rebuild",
    unbounded_wait,
    (index_id: u32, nlist: u32, sample_limit: u32),
    ()
);
forward_vector!(
    forward_admin_start_vector_rebuild_if_recommended,
    "admin_start_vector_rebuild_if_recommended",
    unbounded_wait,
    (
        index_id: u32,
        attested_page_health: VectorPartitionPageHealth,
        policy: VectorMaintenancePolicy,
        target_nlist: Option<u32>,
        sample_limit: u32,
    ),
    VectorMaintenanceRecommendation
);
forward_vector!(
    forward_admin_vector_rebuild_step,
    "admin_vector_rebuild_step",
    unbounded_wait,
    (index_id: u32, max_subjects: u32),
    VectorRebuildStatus
);
forward_vector!(
    forward_admin_publish_vector_rebuild,
    "admin_publish_vector_rebuild",
    unbounded_wait,
    (index_id: u32),
    ()
);
forward_vector!(
    forward_admin_abort_vector_rebuild,
    "admin_abort_vector_rebuild",
    unbounded_wait,
    (index_id: u32),
    ()
);
forward_vector!(
    forward_admin_vector_rebuild_cleanup_step,
    "admin_vector_rebuild_cleanup_step",
    unbounded_wait,
    (index_id: u32, max_work: u32),
    VectorRebuildStatus
);
forward_vector!(
    forward_admin_vector_centroid_cache_warmup,
    "admin_vector_centroid_cache_warmup",
    unbounded_wait,
    (index_id: u32),
    VectorCentroidCacheStatus
);
forward_vector!(
    forward_admin_vector_centroid_cache_clear,
    "admin_vector_centroid_cache_clear",
    unbounded_wait,
    (),
    VectorCentroidCacheStatus
);
forward_vector!(
    forward_admin_vector_maintenance_step,
    "admin_vector_maintenance_step",
    unbounded_wait,
    (index_id: u32, req: VectorMaintenanceStepRequest),
    VectorMaintenanceStepResult
);
forward_vector!(
    forward_admin_vector_maintenance_reset,
    "admin_vector_maintenance_reset",
    unbounded_wait,
    (index_id: u32),
    ()
);
