//! PocketIC coverage for the ADR 0031 Slice 9 vector-index maintenance surface: the bounded
//! page-meta tombstone-health step, the rebuild-if-recommended trigger, and the heap centroid-cache
//! admin endpoints. All Slice 9 admin endpoints are `guard_router_canister`-guarded, so the harness
//! drives them directly on the vector canister with `sender = router` (the precedent established by
//! the Slice 7/8 rebuild/health/slab-stats endpoints), and asserts a non-router caller is rejected.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorPartitionHealthStep, VectorPartitionHealthSummary,
    VectorPartitionPageHealth, VectorRebuildPhase, VectorRebuildStatus, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, install_federation, install_vector_canister,
};

const INDEX_ID: u32 = 1;
const DIMS: u16 = 4;

/// `DIMS` little-endian `f32` components each equal to `value`.
fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn subject(vertex_id: u32) -> VectorSubject {
    VectorSubject::Vertex {
        shard_id: ShardId::new(0),
        vertex_id,
    }
}

fn router_graph_id(env: &FederationEnv) -> GraphId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "lookup_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode lookup_graph_id"),
        )
        .expect("lookup_graph_id call");
    Decode!(
        &bytes,
        Result<GraphId, gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode lookup_graph_id")
    .expect("graph id")
}

/// Attach shard 0 (owned by `graph_source`) to the vector canister so it accepts that shard's
/// `vector_upsert`s — the minimal wiring the maintenance tests need (no router activation flag).
fn attach_shard_to_vector(env: &FederationEnv, vector: Principal, graph_id: GraphId) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &ShardId::new(0), &env.graph_source).expect("encode vector attach"),
        )
        .expect("vector admin_attach_shard_canister call");
    Decode!(&bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard 0");
}

/// Seed one embedding by calling `vector_upsert` as the owning shard (sender = `graph_source`).
fn seed_embedding(
    env: &FederationEnv,
    vector: Principal,
    vertex_id: u32,
    version: u64,
    value: f32,
) {
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        embedding_incarnation: 1,
        embedding_version: version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        bytes: vec_bytes(value),
        remove: false,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            env.graph_source,
            "vector_upsert",
            Encode!(&op).expect("encode upsert op"),
        )
        .expect("vector_upsert call");
    Decode!(&bytes, Result<(), VectorIndexError>)
        .expect("decode upsert result")
        .expect("upsert ok");
}

/// Installs a vector canister, attaches shard 0, and seeds four subjects plus a re-upsert of subject
/// 1 (which tombstones its prior row): 5 physical rows, 4 live, 1 tombstoned at active version 1.
fn ready_vector_with_tombstone(env: &FederationEnv) -> Principal {
    let vector = install_vector_canister(&env.pic, env.router);
    let graph_id = router_graph_id(env);
    attach_shard_to_vector(env, vector, graph_id);
    for v in 1..=4u32 {
        seed_embedding(env, vector, v, 1, (v - 1) as f32);
    }
    seed_embedding(env, vector, 1, 2, 9.0); // tombstones subject 1's v1 row
    vector
}

fn partition_health_summary(
    env: &FederationEnv,
    vector: Principal,
) -> VectorPartitionHealthSummary {
    let bytes = env
        .pic
        .query_call(
            vector,
            env.router,
            "admin_vector_partition_health",
            Encode!(&INDEX_ID).expect("encode health args"),
        )
        .expect("partition health call");
    Decode!(&bytes, Result<VectorPartitionHealthSummary, String>)
        .expect("decode health")
        .expect("health ok")
}

fn health_step(
    env: &FederationEnv,
    vector: Principal,
    cursor: Option<Vec<u8>>,
    max_pages: u32,
) -> VectorPartitionHealthStep {
    let bytes = env
        .pic
        .query_call(
            vector,
            env.router,
            "admin_vector_partition_health_step",
            Encode!(&INDEX_ID, &cursor, &max_pages).expect("encode step args"),
        )
        .expect("health step call");
    Decode!(&bytes, Result<VectorPartitionHealthStep, String>)
        .expect("decode step")
        .expect("step ok")
}

/// Drives the bounded page-meta health scan to exhaustion and sums the additive partials.
fn merged_page_health(env: &FederationEnv, vector: Principal) -> VectorPartitionPageHealth {
    let mut merged = VectorPartitionPageHealth::default();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let step = health_step(env, vector, cursor, 1);
        merged.index_id = step.partial.index_id;
        merged.index_version = step.partial.index_version;
        merged.page_count += step.partial.page_count;
        merged.total_rows += step.partial.total_rows;
        merged.physical_live_rows += step.partial.physical_live_rows;
        merged.tombstoned_rows += step.partial.tombstoned_rows;
        if step.exhausted {
            break;
        }
        cursor = step.cursor;
    }
    merged
}

fn rebuild_if_recommended(
    env: &FederationEnv,
    vector: Principal,
    page: &VectorPartitionPageHealth,
    policy: &VectorMaintenancePolicy,
    target_nlist: Option<u32>,
    sample_limit: u32,
) -> Result<VectorMaintenanceRecommendation, String> {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_start_vector_rebuild_if_recommended",
            Encode!(&INDEX_ID, page, policy, &target_nlist, &sample_limit)
                .expect("encode trigger args"),
        )
        .expect("trigger call");
    Decode!(&bytes, Result<VectorMaintenanceRecommendation, String>).expect("decode trigger")
}

fn rebuild_status(env: &FederationEnv, vector: Principal) -> VectorRebuildStatus {
    let bytes = env
        .pic
        .query_call(
            vector,
            env.router,
            "admin_vector_rebuild_status",
            Encode!(&INDEX_ID).expect("encode status args"),
        )
        .expect("status call");
    Decode!(&bytes, Result<VectorRebuildStatus, String>)
        .expect("decode status")
        .expect("status ok")
}

/// A tombstone-dominant policy that flags `tombstoned/total >= 20%` as required; skew is disabled by
/// an unreachable threshold so the degenerate (`nlist = 1`) fixture is judged on tombstones alone.
fn tombstone_required_policy() -> VectorMaintenancePolicy {
    VectorMaintenancePolicy {
        recommended_tombstone_ratio_bps: 1_000,
        required_tombstone_ratio_bps: 2_000,
        recommended_skew_ratio_bps: u32::MAX,
        required_skew_ratio_bps: u32::MAX,
        min_total_rows: 1,
        min_tombstoned_rows: 1,
    }
}

#[test]
fn partition_health_step_merges_tombstone_accounting() {
    let env = install_federation();
    let vector = ready_vector_with_tombstone(&env);

    let merged = merged_page_health(&env, vector);
    assert_eq!(merged.index_id, INDEX_ID);
    assert_eq!(merged.index_version, 1, "scoped to the active generation");
    assert_eq!(merged.total_rows, 5, "4 seeded + 1 appended on re-upsert");
    assert_eq!(merged.physical_live_rows, 4, "4 live subjects");
    assert_eq!(
        merged.tombstoned_rows, 1,
        "subject 1's prior row is tombstoned"
    );

    // The head-only skew summary remains available and consistent (4 live in one partition).
    let summary = partition_health_summary(&env, vector);
    assert_eq!(summary.nlist, 1);
    assert_eq!(summary.live_rows, 4);
}

#[test]
fn rebuild_if_recommended_starts_rebuild_and_rejects_stale() {
    let env = install_federation();
    let vector = ready_vector_with_tombstone(&env);

    let page = merged_page_health(&env, vector);
    let policy = tombstone_required_policy();

    // 1 tombstone / 5 rows = 20% >= required: a rebuild starts at the requested nlist (the
    // degenerate def.nlist = 1 cannot be defaulted, hence an explicit target_nlist). The skew
    // summary is recomputed server-side, so only the page health is passed in.
    let rec =
        rebuild_if_recommended(&env, vector, &page, &policy, Some(2), 10).expect("trigger ok");
    assert_eq!(rec, VectorMaintenanceRecommendation::RebuildRequired);
    assert_eq!(
        rebuild_status(&env, vector).phase,
        VectorRebuildPhase::Sampling,
        "a required recommendation begins the rebuild"
    );

    // Stale page health (wrong active version) is rejected by the freshness guard.
    let mut stale = page;
    stale.index_version = 999;
    let err = rebuild_if_recommended(&env, vector, &stale, &policy, Some(2), 10)
        .expect_err("stale health rejected");
    assert!(
        err.contains("does not match the active index generation"),
        "expected StaleMaintenanceHealth message, got {err:?}"
    );
}

#[test]
fn centroid_cache_endpoints_roundtrip_and_guard() {
    let env = install_federation();
    let vector = ready_vector_with_tombstone(&env);

    // Status is reachable and starts empty.
    let status: Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, String> = {
        let bytes = env
            .pic
            .query_call(
                vector,
                env.router,
                "admin_vector_centroid_cache_status",
                Encode!().expect("encode status"),
            )
            .expect("cache status call");
        Decode!(
            &bytes,
            Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, String>
        )
        .expect("decode cache status")
    };
    let status = status.expect("status ok");
    assert_eq!(status.entries, 0);
    assert_eq!(status.max_bytes, 8 * 1024 * 1024);

    // Warmup on a degenerate (nlist = 1) index caches nothing.
    let warmed = {
        let bytes = env
            .pic
            .update_call(
                vector,
                env.router,
                "admin_vector_centroid_cache_warmup",
                Encode!(&INDEX_ID).expect("encode warmup"),
            )
            .expect("cache warmup call");
        Decode!(
            &bytes,
            Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, String>
        )
        .expect("decode warmup")
        .expect("warmup ok")
    };
    assert_eq!(
        warmed.entries, 0,
        "a degenerate index has no centroid set to warm"
    );

    // Clear is reachable.
    let cleared = {
        let bytes = env
            .pic
            .update_call(
                vector,
                env.router,
                "admin_vector_centroid_cache_clear",
                Encode!().expect("encode clear"),
            )
            .expect("cache clear call");
        Decode!(
            &bytes,
            Result<gleaph_graph_kernel::vector_index::VectorCentroidCacheStatus, String>
        )
        .expect("decode clear")
        .expect("clear ok")
    };
    assert_eq!(cleared.entries, 0);

    // A non-router caller is rejected by the guard (the message is rejected, not an Ok result).
    let rejected = env.pic.query_call(
        vector,
        env.admin,
        "admin_vector_centroid_cache_status",
        Encode!().expect("encode status"),
    );
    assert!(
        rejected.is_err(),
        "non-router caller must be rejected by guard_router_canister"
    );
}
