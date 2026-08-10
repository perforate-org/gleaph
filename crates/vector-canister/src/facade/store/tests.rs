//! Unit tests for the degenerate `ivf_flat` mutation store (ADR 0031 Slice 2).

use super::VectorCanisterStore;
use crate::init::VectorCanisterInitArgs;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_FILTER_CANDIDATES, MAX_VECTOR_SEARCH_TOP_K, VectorCanisterError,
    VectorEmbeddingSyncOp, VectorEncoding, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMetric, VectorPartitionPageHealth, VectorSearchRequest,
    VectorSubject,
};

const INDEX_ID: u32 = 1;
const DIMS: u16 = 4;
const STRIDE: usize = 16; // dims * 4 for F32

fn router() -> Principal {
    Principal::from_slice(&[9])
}

fn shard_canister() -> Principal {
    Principal::from_slice(&[1])
}

/// Initializes a fresh store (clears all per-thread stable state) and attaches shard 0.
fn fresh_store() -> VectorCanisterStore {
    let store = VectorCanisterStore::new();
    store
        .init_from_args(&VectorCanisterInitArgs {
            router_canister: router(),
        })
        .expect("init");
    store.attach_single_shard_for_test(router(), ShardId::new(0), shard_canister());
    store
}

fn subject(vertex_id: u32) -> VectorSubject {
    VectorSubject::Vertex {
        shard_id: ShardId::new(0),
        vertex_id,
    }
}

/// Upsert at an explicit `mutation_id` stamp (ADR 0064 §5).
fn upsert_op(vertex_id: u32, mutation_id: u64, fill: u8) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        mutation_id,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: vec![fill; STRIDE],
        remove: false,
    }
}

/// Remove at an explicit `mutation_id` stamp (ADR 0064 §5).
fn remove_op(vertex_id: u32, mutation_id: u64) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        mutation_id,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: Vec::new(),
        remove: true,
    }
}

#[test]
fn upsert_new_creates_def_slot_and_clock() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .expect("upsert");

    let def = store.def_for_test(INDEX_ID).expect("def created lazily");
    assert_eq!(def.active_index_version, 1);
    assert_eq!(def.dims, DIMS);
    assert_eq!(def.stride_bytes, STRIDE as u32);

    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .expect("clock");
    assert!(!entry.deleted);
    assert_eq!(entry.stamp, 1);
    let slot = entry.slot.expect("live slot");
    assert_eq!(slot.slot, 0, "first row lands at slot 0");
}

#[test]
fn upsert_same_version_identical_payload_is_noop() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .expect("idempotent no-op");
    let head = store.partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 1, "no new slot appended");
}

#[test]
fn upsert_same_version_different_payload_conflicts() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    let err = store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xBB))
        .expect_err("conflict");
    assert_eq!(err, VectorCanisterError::MutationStampConflict);
}

#[test]
fn upsert_older_version_is_noop() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 5, 0xAA))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 3, 0xBB))
        .expect("stale no-op");
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert_eq!(entry.stamp, 5);
}

#[test]
fn upsert_newer_version_live_appends_and_tombstones_old_slot() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    let old_slot = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .unwrap();

    store
        .vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB))
        .unwrap();
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert_eq!(entry.stamp, 2);
    let new_slot = entry.slot.unwrap();
    assert_ne!(
        new_slot.slot, old_slot.slot,
        "newer version appends a fresh slot"
    );
    let head = store.partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 1, "append +1, tombstone -1");
}

#[test]
fn remove_live_tombstones_and_advances_clock() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 2))
        .unwrap();

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted);
    assert_eq!(entry.stamp, 2);
    assert_eq!(entry.slot, None);
    let head = store.partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 0);
}

#[test]
fn remove_missing_subject_writes_tombstone_clock() {
    let store = fresh_store();
    // No def yet; remove on a never-inserted subject still writes a clock.
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .expect("clock written");
    assert!(entry.deleted);
    assert_eq!(entry.stamp, 1);
}

#[test]
fn same_incarnation_upsert_to_deleted_subject_is_noop() {
    // Under incarnation fencing, an upsert at the *same* incarnation as a tombstone is a stale
    // replay: a genuine reinsert carries a strictly greater incarnation. So it must NOT resurrect.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    // Stale same-incarnation upsert (e.g. a journaled replay) lands behind the tombstone clock.
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .expect("stale replay no-op");

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted, "same-incarnation upsert cannot resurrect");
}

#[test]
fn newer_incarnation_upsert_resurrects_with_fresh_slot() {
    // Resurrection requires a strictly greater incarnation, mirroring the canonical store bumping
    // the incarnation on each delete/reinsert. The fresh incarnation lands a brand-new slot.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    let old_slot = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    // Reinsert at incarnation 2, version 1 (canonical version reset): resurrects.
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB))
        .unwrap();

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "newer-incarnation upsert resurrects");
    assert_eq!(entry.stamp, 2);
    let new_slot = entry.slot.expect("resurrected live slot");
    assert_ne!(
        new_slot.slot, old_slot.slot,
        "resurrection appends a fresh slot"
    );
}

#[test]
fn newer_incarnation_upsert_after_missing_remove_clock_resurrects() {
    // A remove on a never-inserted subject writes a tombstone clock at its incarnation; only a
    // strictly newer incarnation resurrects (a same-incarnation replay stays a no-op).
    let store = fresh_store();
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .expect("same-incarnation replay no-op");
    assert!(
        store
            .subject_entry_for_test(INDEX_ID, subject(7))
            .unwrap()
            .deleted
    );
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 2, 0xAA))
        .unwrap();
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "newer incarnation resurrects after a clock");
    assert_eq!(entry.stamp, 2);
}

#[test]
fn reinsert_after_delete_appends_fresh_slot() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    let first_slot = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .unwrap();

    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    // The canonical reinsert bumps the incarnation to 2.
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 2, 0xCC))
        .unwrap();

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted);
    let new_slot = entry.slot.unwrap();
    assert_ne!(
        new_slot.slot, first_slot.slot,
        "reinsert appends a fresh slot"
    );
}

#[test]
fn stale_older_incarnation_remove_cannot_tombstone_newer_live() {
    // The reverse-orphan race: a late repair-drain remove for the *deleted* incarnation arrives
    // after a newer reinsert already advanced the clock. The incarnation fence makes it a no-op.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    // Reinsert at incarnation 2 (live again, fresh slot).
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB))
        .unwrap();

    // Late blind remove for the OLD incarnation with the authoritative max version: must no-op.
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .expect("stale older-incarnation remove is fenced");

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(
        !entry.deleted,
        "newer live incarnation survives a stale remove"
    );
    assert_eq!(entry.stamp, 2);
    assert!(entry.slot.is_some(), "newer live slot survives");
}

#[test]
fn newer_incarnation_remove_on_live_tombstones() {
    // A remove for a strictly newer incarnation than the live clock authoritatively tombstones the
    // live slot (e.g. the upsert for that incarnation never arrived).
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 2))
        .unwrap();
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted);
    assert_eq!(entry.stamp, 2);
    assert_eq!(entry.slot, None);
}

#[test]
fn page_capacity_rolls_to_new_page_at_slots_per_page() {
    let store = fresh_store();
    // d = 4 F32: pad stride 16, meta 4, single shard. A 80-byte budget fits exactly 2 rows.
    store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 80)
        .expect("create");
    assert_eq!(store.def_for_test(INDEX_ID).unwrap().slots_per_page, 2);

    for v in 0..3u32 {
        store
            .vector_upsert(shard_canister(), &upsert_op(v, 1, v as u8))
            .unwrap();
    }
    let head = store.partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.page_count, 2, "third insert rolls to a new page");
    assert_eq!(head.next_page_id, 2);
    assert_eq!(head.live_len, 3);
}

#[test]
fn create_index_rejects_capacity_below_one_slot() {
    let store = fresh_store();
    // d = 4 F32 needs 64 bytes for a single row; a 40-byte budget fits no row.
    let err = store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 40)
        .expect_err("reject");
    assert_eq!(err, VectorCanisterError::InvalidPageCapacity);
}

#[test]
fn upsert_dimension_and_byte_width_mismatch() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();

    let mut wrong_dims = upsert_op(8, 1, 0xAA);
    wrong_dims.dims = DIMS + 1;
    assert_eq!(
        store
            .vector_upsert(shard_canister(), &wrong_dims)
            .unwrap_err(),
        VectorCanisterError::DimensionMismatch
    );

    let mut wrong_bytes = upsert_op(9, 1, 0xAA);
    wrong_bytes.bytes = vec![0u8; STRIDE - 1];
    assert_eq!(
        store
            .vector_upsert(shard_canister(), &wrong_bytes)
            .unwrap_err(),
        VectorCanisterError::ByteWidthMismatch
    );
}

#[test]
fn vector_upsert_rejects_remove_flag() {
    let store = fresh_store();
    let mut op = upsert_op(7, 1, 0xAA);
    op.remove = true;
    assert_eq!(
        store.vector_upsert(shard_canister(), &op).unwrap_err(),
        VectorCanisterError::MutationKindMismatch
    );
    // The contradictory op must not have mutated any state.
    assert!(store.subject_entry_for_test(INDEX_ID, subject(7)).is_none());
}

#[test]
fn vector_remove_rejects_insert_flag() {
    let store = fresh_store();
    let mut op = remove_op(7, 1);
    op.remove = false;
    assert_eq!(
        store.vector_remove(shard_canister(), &op).unwrap_err(),
        VectorCanisterError::MutationKindMismatch
    );
    assert!(store.subject_entry_for_test(INDEX_ID, subject(7)).is_none());
}

#[test]
fn mutation_auth_rejects_unattached_and_cross_shard() {
    let store = fresh_store();
    let stranger = Principal::from_slice(&[2]);
    assert_eq!(
        store
            .vector_upsert(stranger, &upsert_op(7, 1, 0xAA))
            .unwrap_err(),
        VectorCanisterError::ShardNotAttached
    );

    // Caller attached to shard 0 but op targets shard 1.
    let mut cross = upsert_op(7, 1, 0xAA);
    cross.subject = VectorSubject::Vertex {
        shard_id: ShardId::new(1),
        vertex_id: 7,
    };
    assert_eq!(
        store.vector_upsert(shard_canister(), &cross).unwrap_err(),
        VectorCanisterError::ShardMismatch
    );
}

#[test]
fn router_can_persist_any_shard_subject() {
    let store = fresh_store();
    // The Router is the trusted coordinator (ADR 0064 §6): it persists ops for any shard, so it must
    // not be rejected as an unattached caller. This is the path `vector_sync_batch` exercises.
    store
        .vector_upsert(router(), &upsert_op(7, 1, 0xAA))
        .expect("Router upsert for shard 0");

    let mut cross = upsert_op(8, 2, 0xBB);
    cross.subject = VectorSubject::Vertex {
        shard_id: ShardId::new(1),
        vertex_id: 8,
    };
    store
        .vector_upsert(router(), &cross)
        .expect("Router upsert for a shard it is not attached to");

    store
        .vector_remove(router(), &remove_op(7, 3))
        .expect("Router remove");
}

#[test]
fn init_rejects_anonymous_router() {
    let store = VectorCanisterStore::new();
    let err = store
        .init_from_args(&VectorCanisterInitArgs {
            router_canister: Principal::anonymous(),
        })
        .expect_err("anonymous router rejected");
    assert_eq!(err, VectorCanisterError::AnonymousRouter);
}

#[test]
fn attach_rejects_anonymous_principal() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_attach_shard_canister(
                router(),
                GraphId::from_raw(1),
                ShardId::new(0),
                Principal::anonymous(),
            )
            .unwrap_err(),
        VectorCanisterError::InvalidPrincipalInRegistry
    );
}

#[test]
fn single_target_owns_all_shards_of_one_graph() {
    let store = VectorCanisterStore::new();
    store
        .init_from_args(&VectorCanisterInitArgs {
            router_canister: router(),
        })
        .expect("init");
    let graph = GraphId::from_raw(1);
    // One vector target owns *every* shard of the graph (ADR 0031 Slice 4 target model B). Shard 0
    // pins the graph; a *different* shard of the SAME graph must also attach (the old property-index
    // group model rejected this with GraphOwnershipMismatch — the bug this guards against).
    store
        .admin_attach_shard_canister(
            router(),
            graph,
            ShardId::new(0),
            Principal::from_slice(&[10]),
        )
        .expect("attach shard 0");
    store
        .admin_attach_shard_canister(
            router(),
            graph,
            ShardId::new(1),
            Principal::from_slice(&[11]),
        )
        .expect("attach shard 1 to the same single target");
    // A shard belonging to a *different* graph is rejected — one target per graph.
    assert_eq!(
        store
            .admin_attach_shard_canister(
                router(),
                GraphId::from_raw(2),
                ShardId::new(0),
                Principal::from_slice(&[12]),
            )
            .unwrap_err(),
        VectorCanisterError::GraphOwnershipMismatch
    );
}

#[test]
fn attach_rejects_non_router_caller() {
    let store = fresh_store();
    let not_router = Principal::from_slice(&[123]);
    assert_eq!(
        store
            .admin_attach_shard_canister(
                not_router,
                GraphId::from_raw(1),
                ShardId::new(0),
                shard_canister(),
            )
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

#[test]
fn detach_purges_shard_subjects_and_slots() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(8, 1, 0xBB))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(9, 1))
        .unwrap(); // tombstone clock

    let result = store.detach_shard_step_for_test(ShardId::new(0), None, 20_000);
    assert!(result.done);
    assert!(result.removed >= 3);

    assert!(store.subject_entry_for_test(INDEX_ID, subject(7)).is_none());
    assert!(store.subject_entry_for_test(INDEX_ID, subject(8)).is_none());
    assert!(store.subject_entry_for_test(INDEX_ID, subject(9)).is_none());
}

#[test]
fn def_and_heads_persist_across_store_handles() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(8, 1, 0xBB))
        .unwrap();

    // A fresh stateless handle reads the same durable stable state ("reopen").
    let reopened = VectorCanisterStore::new();
    let def = reopened.def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.dims, DIMS);
    let head = reopened.partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 2);
}

// --- ADR 0031 Slice 5: exact ivf_flat search (live subject-map scan) ---

/// `DIMS` little-endian `f32` components, each equal to `value`, so L2 distance to a constant query
/// `q` is `DIMS * (value - q)^2` — exact and easy to order in tests.
fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(STRIDE);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn upsert_vec(vertex_id: u32, mutation_id: u64, value: f32) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        mutation_id,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: vec_bytes(value),
        remove: false,
    }
}

fn search_value(value: f32, top_k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(value),
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        top_k,
        candidate_subjects: None,
    }
}

/// A query vector that is guaranteed finite and non-zero for both L2 and cosine metrics.
fn search_nonzero(value: f32, top_k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(value + 0.5),
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        top_k,
        candidate_subjects: None,
    }
}

/// Encode a custom `f32` vector of `DIMS` components (little-endian).
fn vec_bytes_from(values: &[f32]) -> Vec<u8> {
    assert_eq!(values.len(), DIMS as usize, "component count mismatch");
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn upsert_vec_from(
    vertex_id: u32,
    mutation_id: u64,
    values: &[f32],
    metric: VectorMetric,
) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        mutation_id,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric,
        bytes: vec_bytes_from(values),
        remove: false,
    }
}

fn search_metric_from(values: &[f32], top_k: u32, metric: VectorMetric) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes_from(values),
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric,
        top_k,
        candidate_subjects: None,
    }
}

#[test]
fn search_returns_inserted_vector() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    let hit = &result.hits[0];
    assert_eq!(hit.subject, subject(7));
    assert_eq!(hit.distance, 0.0);
}

#[test]
fn search_top_k_orders_by_distance_and_bounds_results() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(8, 1, 2.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(9, 1, 3.0))
        .unwrap();
    let result = store.vector_search(&search_value(1.0, 2)).expect("search");
    let subjects: Vec<_> = result.hits.iter().map(|h| h.subject).collect();
    assert_eq!(
        subjects,
        vec![subject(7), subject(8)],
        "nearest two, ordered"
    );
    assert!(result.hits[0].distance < result.hits[1].distance);
}

#[test]
fn search_tie_break_is_subject_ascending() {
    let store = fresh_store();
    // Both are equidistant (|1-0| == |1-2|) from the query 1.0; the tie-break must be deterministic
    // on the subject key ascending.
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 0.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(8, 1, 2.0))
        .unwrap();
    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(result.hits[0].distance, result.hits[1].distance);
    assert_eq!(
        result.hits.iter().map(|h| h.subject).collect::<Vec<_>>(),
        vec![subject(7), subject(8)]
    );
}

#[test]
fn search_skips_deleted_subject() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 2))
        .unwrap();
    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(result.hits.is_empty(), "deleted subject must not appear");
}

#[test]
fn search_returns_newest_slot_only() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 2, 5.0))
        .unwrap();
    // Query the newest value: exactly one hit, distance 0, at the newest version.
    let result = store.vector_search(&search_value(5.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].distance, 0.0);
    // The superseded (tombstoned) generation's value 1.0 is never scored.
    let stale = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(stale.hits.len(), 1);
    assert!(stale.hits[0].distance > 0.0);
}

#[test]
fn search_reinsert_after_delete_returns_newer_incarnation_only() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 1))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 2, 9.0))
        .unwrap();
    let result = store.vector_search(&search_value(9.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].distance, 0.0);
}

#[test]
fn search_does_not_read_rows_of_a_different_index() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    // Seed a second index with the same subject/value; a search over INDEX_ID must not read it.
    let other_index = INDEX_ID + 1;
    store
        .vector_upsert(
            shard_canister(),
            &VectorEmbeddingSyncOp {
                index_id: other_index,
                embedding_name_id: 0,
                subject: subject(8),
                mutation_id: 1,
                encoding: VectorEncoding::F32,
                dims: DIMS,
                metric: VectorMetric::L2Squared,
                bytes: vec_bytes(1.0),
                remove: false,
            },
        )
        .unwrap();
    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1, "only INDEX_ID rows are scanned");
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn search_scores_non_tombstoned_row_regardless_of_subject_map() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{FixedSubjectMapEntry, SubjectKey};

    let store = fresh_store();
    // Seed a valid live vector so the def, a page row, and a real slot all exist.
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .expect("live entry");
    assert!(entry.slot.is_some());

    // The search no longer consults the subject map: it scores every non-tombstoned row, relying on
    // the write-path invariant that a non-tombstoned row is the subject's current live slot. Even if
    // the subject-map entry is corrupted (no resolvable slot), the non-tombstoned row is still scored.
    let drifted = FixedSubjectMapEntry {
        slot: None,
        shadow_slot: None,
        ..entry
    };
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
        m.insert(SubjectKey::new(INDEX_ID, subject(7)), drifted)
            .expect("insert drifted entry");
    });

    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(
        result.hits.iter().map(|h| h.subject).collect::<Vec<_>>(),
        vec![subject(7)],
        "the non-tombstoned row is scored regardless of the subject-map entry"
    );
}

#[test]
fn search_rejects_dimension_mismatch() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let req = VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec![0u8; (DIMS as usize + 1) * 4],
        encoding: VectorEncoding::F32,
        dims: DIMS + 1,
        metric: VectorMetric::L2Squared,
        top_k: 10,
        candidate_subjects: None,
    };
    assert_eq!(
        store.vector_search(&req).unwrap_err(),
        VectorCanisterError::DimensionMismatch
    );
}

#[test]
fn search_rejects_byte_width_mismatch() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let req = VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec![0u8; STRIDE - 4],
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        top_k: 10,
        candidate_subjects: None,
    };
    assert_eq!(
        store.vector_search(&req).unwrap_err(),
        VectorCanisterError::ByteWidthMismatch
    );
}

#[test]
fn search_rejects_invalid_top_k() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    assert_eq!(
        store.vector_search(&search_value(1.0, 0)).unwrap_err(),
        VectorCanisterError::InvalidSearchTopK
    );
    assert_eq!(
        store
            .vector_search(&search_value(1.0, MAX_VECTOR_SEARCH_TOP_K + 1))
            .unwrap_err(),
        VectorCanisterError::InvalidSearchTopK
    );
}

#[test]
fn search_missing_physical_def_returns_empty() {
    // The physical def is created lazily on first upsert; a Router-registered, activated index with
    // no embeddings yet has no def but is a known-empty index, not an unknown one.
    let store = fresh_store();
    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(result.hits.is_empty());
}

#[test]
fn search_empty_index_returns_no_hits() {
    let store = fresh_store();
    store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 64 * 1024)
        .expect("create index");
    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(result.hits.is_empty());
}

// --- ADR 0034 Slice 4: cosine metric-specific scoring and fail-closed paths ---

#[test]
fn cosine_exact_scan_orders_by_one_minus_similarity() {
    let store = fresh_store();
    // Three distinct unit-direction vectors; query aligns with the first.
    let v7 = vec![1.0f32, 0.0, 0.0, 0.0];
    let v8 = vec![0.0f32, 1.0, 0.0, 0.0];
    let v9 = vec![1.0f32, 1.0, 0.0, 0.0];
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &v7, VectorMetric::Cosine),
        )
        .unwrap();
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(8, 1, &v8, VectorMetric::Cosine),
        )
        .unwrap();
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(9, 1, &v9, VectorMetric::Cosine),
        )
        .unwrap();

    let result = store
        .vector_search(&search_metric_from(
            &[1.0f32, 0.0, 0.0, 0.0],
            10,
            VectorMetric::Cosine,
        ))
        .expect("cosine search");
    assert_eq!(result.hits.len(), 3);
    assert_eq!(
        result.hits[0].subject,
        subject(7),
        "identical direction is nearest"
    );
    assert!((result.hits[0].distance).abs() < 1e-6);
    assert_eq!(result.hits[1].subject, subject(9));
    let expected_v9 = 1.0 - 1.0f32 / 2.0f32.sqrt(); // 1 - cos(45 deg)
    assert!((result.hits[1].distance - expected_v9).abs() < 1e-5);
    assert_eq!(result.hits[2].subject, subject(8));
    assert!(
        (result.hits[2].distance - 1.0).abs() < 1e-6,
        "orthogonal vector raw is 1"
    );
}

#[test]
fn cosine_zero_norm_query_fails_closed() {
    let store = fresh_store();
    // Create the physical def as a cosine index first.
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &[1.0f32; DIMS as usize], VectorMetric::Cosine),
        )
        .unwrap();
    let err = store
        .vector_search(&search_metric_from(
            &[0.0f32; DIMS as usize],
            10,
            VectorMetric::Cosine,
        ))
        .expect_err("zero-norm cosine query must fail");
    assert!(matches!(err, VectorCanisterError::InvalidQueryVector));
}

#[test]
fn cosine_nonfinite_query_fails_closed() {
    let store = fresh_store();
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &[1.0f32; DIMS as usize], VectorMetric::Cosine),
        )
        .unwrap();
    let err = store
        .vector_search(&search_metric_from(
            &[f32::NAN; DIMS as usize],
            10,
            VectorMetric::Cosine,
        ))
        .expect_err("non-finite cosine query must fail");
    assert!(matches!(err, VectorCanisterError::InvalidQueryVector));
}

#[test]
fn cosine_zero_norm_indexed_vector_is_rejected() {
    let store = fresh_store();
    // Zero-norm vectors have no cosine similarity; cosine ingest rejects them fail-closed instead
    // of storing a non-normalizable row.
    let err = store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &[0.0f32; DIMS as usize], VectorMetric::Cosine),
        )
        .expect_err("zero-norm cosine ingest must fail");
    assert!(matches!(err, VectorCanisterError::InvalidQueryVector));
}

#[test]
fn cosine_upsert_stores_unit_normalized_row() {
    let store = fresh_store();
    let v = [3.0f32, 4.0, 0.0, 0.0];
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &v, VectorMetric::Cosine),
        )
        .expect("cosine upsert");
    let slot = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .expect("live");
    let stored = store.read_slot_bytes(INDEX_ID, slot).expect("stored bytes");
    let decoded = super::search::decode_f32(&stored);
    // Stored row is unit-normalized and points in the input direction ([3,4,0,0]/5 = [0.6,0.8,0,0]).
    let norm_sq: f32 = decoded.iter().map(|x| x * x).sum();
    assert!(
        (norm_sq - 1.0).abs() < 1e-5,
        "stored row is unit, got {norm_sq}"
    );
    assert!((decoded[0] - 0.6).abs() < 1e-5 && (decoded[1] - 0.8).abs() < 1e-5);
}

#[test]
fn cosine_upsert_same_bytes_replay_is_noop() {
    let store = fresh_store();
    let v = [3.0f32, 4.0, 0.0, 0.0];
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &v, VectorMetric::Cosine),
        )
        .expect("first upsert");
    let slot1 = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .expect("live");
    // Replaying the same stamp + bytes is an idempotent no-op (the normalized comparison matches the
    // stored unit row).
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &v, VectorMetric::Cosine),
        )
        .expect("idempotent replay");
    let slot2 = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .expect("live");
    assert_eq!(slot2, slot1, "no new slot on idempotent replay");
}

#[test]
fn cosine_nonfinite_indexed_vector_is_skipped() {
    let store = fresh_store();
    let mut bad = vec![1.0f32; DIMS as usize];
    bad[0] = f32::NAN;
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &bad, VectorMetric::Cosine),
        )
        .unwrap();
    let result = store
        .vector_search(&search_metric_from(
            &[1.0f32; DIMS as usize],
            10,
            VectorMetric::Cosine,
        ))
        .expect("search");
    assert!(
        result.hits.is_empty(),
        "non-finite indexed vector must not produce NaN distance"
    );
}

#[test]
fn l2_nonfinite_indexed_vector_is_skipped_for_consistency() {
    let store = fresh_store();
    let mut bad = vec![1.0f32; DIMS as usize];
    bad[0] = f32::NAN;
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    store
        .vector_upsert(
            shard_canister(),
            &VectorEmbeddingSyncOp {
                metric: VectorMetric::L2Squared,
                bytes: vec_bytes_from(&bad),
                ..upsert_vec(8, 1, 2.0)
            },
        )
        .unwrap();
    let result = store
        .vector_search(&search_value(1.0, 10))
        .expect("l2 search");
    assert_eq!(
        result.hits.len(),
        1,
        "non-finite indexed vector must be skipped, not returned"
    );
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn cosine_metric_mismatch_on_later_upsert_fails_closed() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let err = store
        .vector_upsert(
            shard_canister(),
            &VectorEmbeddingSyncOp {
                metric: VectorMetric::Cosine,
                ..upsert_vec(8, 1, 2.0)
            },
        )
        .expect_err("metric mismatch must fail");
    assert!(matches!(err, VectorCanisterError::MetricMismatch));
}

#[test]
fn l2_metric_mismatch_on_later_upsert_fails_closed() {
    let store = fresh_store();
    store
        .vector_upsert(
            shard_canister(),
            &upsert_vec_from(7, 1, &[1.0f32; DIMS as usize], VectorMetric::Cosine),
        )
        .unwrap();
    let err = store
        .vector_upsert(shard_canister(), &upsert_vec(8, 1, 2.0))
        .expect_err("metric mismatch must fail");
    assert!(matches!(err, VectorCanisterError::MetricMismatch));
}

#[test]
fn cosine_partition_scan_returns_cosine_ordered_rows() {
    let store = fresh_store();
    // Unit centroids along the axes so L2-based selection is cosine-ordered (L2²(q,c) = 2 − 2cos
    // on unit vectors).
    let centroids = vec![vec![1.0f32, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]];
    // Rows in distinct directions (normalized at append); two per partition.
    let rows: Vec<(VectorSubject, Vec<f32>)> = [
        (1u32, [2.0f32, 0.1, 0.0, 0.0]), // near centroid 0 (+x)
        (2, [0.1, 2.0, 0.0, 0.0]),       // near centroid 1 (+y)
        (3, [1.5, 0.2, 0.0, 0.0]),       // near centroid 0
        (4, [0.2, 1.5, 0.0, 0.0]),       // near centroid 1
    ]
    .map(|(v, dir)| (subject(v), dir.to_vec()))
    .to_vec();
    store.seed_ivf_with_metric_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        VectorMetric::Cosine,
        &centroids,
        &rows,
    );
    // Query along +x: nearest centroid is partition 0. With the default eps=0 only partition 0 is
    // scanned, so the top hits are rows 1 and 3 (both x-direction), ordered by 1 − cos.
    let result = store
        .vector_search(&search_metric_from(
            &[1.0f32, 0.0, 0.0, 0.0],
            10,
            VectorMetric::Cosine,
        ))
        .expect("partitioned cosine search");
    assert_eq!(result.hits[0].subject, subject(1));
    assert_eq!(result.hits[1].subject, subject(3));
    // A full scan (eps = INF) returns all four, still cosine-ordered (x-rows first).
    let all = store
        .vector_search_tuned(
            &search_metric_from(&[1.0f32, 0.0, 0.0, 0.0], 10, VectorMetric::Cosine),
            SearchTuning {
                eps_query: f32::INFINITY,
            },
        )
        .expect("full cosine scan");
    assert_eq!(all.hits.len(), 4);
    assert_eq!(all.hits[0].subject, subject(1));
}

#[test]
fn cosine_rebuild_succeeds_with_spherical_kmeans() {
    let store = fresh_store();
    // Distinct non-zero cosine directions so a rebuild can form >= 2 clusters.
    for (v, dir) in [
        (1u32, [1.0f32, 0.0, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0, 0.0]),
        (3, [-1.0, 0.0, 0.0, 0.0]),
        (4, [0.0, -1.0, 0.0, 0.0]),
    ] {
        store
            .vector_upsert(
                shard_canister(),
                &upsert_vec_from(v, 1, &dir, VectorMetric::Cosine),
            )
            .unwrap();
    }
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("cosine rebuild starts");
    let status = drive_steps(&store, INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
    // Spherical k-means stores unit-normalized centroids.
    let centroids =
        super::search::read_centroids_at(INDEX_ID, TARGET_V, 2, DIMS).expect("centroids");
    assert_eq!(centroids.len(), 2);
    for c in &centroids {
        let norm_sq: f32 = c.iter().map(|x| x * x).sum();
        assert!(
            (norm_sq - 1.0).abs() < 1e-4,
            "centroid is unit, got {norm_sq}"
        );
    }
    // Every live subject got a shadow slot at the target version.
    for v in 1..=4u32 {
        let entry = store.subject_entry_for_test(INDEX_ID, subject(v)).unwrap();
        assert_eq!(
            entry.shadow_slot.map(|s| s.index_version),
            Some(TARGET_V as u32)
        );
    }
    // Publish, then the nlist>1 cosine index uses the partition scan (Phase 2) and still returns
    // 1 − cos ordering.
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    drive_cleanup(&store, INDEX_ID);
    let result = store
        .vector_search(&search_metric_from(
            &[1.0f32; DIMS as usize],
            10,
            VectorMetric::Cosine,
        ))
        .expect("partitioned cosine search");
    assert!(!result.hits.is_empty());
}

#[test]
fn lazy_def_inherits_metric_from_first_op() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let def = store.def_for_test(INDEX_ID).expect("def");
    assert_eq!(def.metric, VectorMetric::L2Squared);
}

// --- ADR 0031 Slice 6: partition-page search over seeded ivf_flat fixtures ---

use super::search::SearchTuning;

/// A constant-valued `f32` vector of `DIMS` components (mirrors `vec_bytes`).
fn cvec(value: f32) -> Vec<f32> {
    vec![value; DIMS as usize]
}

/// Centroids at 0 and 10: vectors near 0 land in partition 0, vectors near 10 in partition 1.
fn two_clusters() -> Vec<Vec<f32>> {
    vec![cvec(0.0), cvec(10.0)]
}

/// (subjects 1,2 cluster near centroid 0; subjects 3,4 cluster near centroid 1).
fn clustered_vectors() -> Vec<(VectorSubject, Vec<f32>)> {
    vec![
        (subject(1), cvec(0.0)),
        (subject(2), cvec(1.0)),
        (subject(3), cvec(9.0)),
        (subject(4), cvec(10.0)),
    ]
}

fn tuned(eps_query: f32) -> SearchTuning {
    SearchTuning { eps_query }
}

#[test]
fn partition_scan_parity_with_exact_at_eps_infinity() {
    let store = fresh_store();
    // Index 1: partitioned (nlist = 2).
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Index 2: degenerate (nlist = 1) so vector_search uses the exact subject-map scan.
    let exact_index = INDEX_ID + 100;
    let exact_vectors: Vec<_> = clustered_vectors();
    store.seed_ivf_for_test(
        exact_index,
        VectorEncoding::F32,
        DIMS,
        &[cvec(0.0)],
        &exact_vectors,
    );

    let partitioned = store
        .vector_search_tuned(&search_value(0.5, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    let mut exact_req = search_value(0.5, 10);
    exact_req.index_id = exact_index;
    let exact = store.vector_search(&exact_req).expect("exact scan");

    let p: Vec<_> = partitioned
        .hits
        .iter()
        .map(|h| (h.subject, h.distance))
        .collect();
    let e: Vec<_> = exact.hits.iter().map(|h| (h.subject, h.distance)).collect();
    assert_eq!(p, e, "eps_query = INF partition scan equals exact scan");
    assert_eq!(p.len(), 4, "all seeded vectors returned");
}

#[test]
fn partition_scan_eps_zero_selects_single_partition() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Query near centroid 0: eps_query = 0 selects partition 0 only.
    let result = store
        .vector_search_tuned(&search_nonzero(0.0, 10), tuned(0.0))
        .expect("partition scan");
    let subjects: Vec<_> = result.hits.iter().map(|h| h.subject).collect();
    assert_eq!(
        subjects,
        vec![subject(1), subject(2)],
        "only partition 0 members"
    );
    assert!(!subjects.contains(&subject(3)));
    assert!(!subjects.contains(&subject(4)));
}

#[test]
fn partition_scan_isolation_other_partition_not_scored() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Query near centroid 1: eps_query = 0 selects partition 1 only.
    let result = store
        .vector_search_tuned(&search_value(10.0, 10), tuned(0.0))
        .expect("partition scan");
    let subjects: Vec<_> = result.hits.iter().map(|h| h.subject).collect();
    assert_eq!(
        subjects,
        vec![subject(4), subject(3)],
        "only partition 1 members, nearest first"
    );
}

#[test]
fn partition_scan_default_eps_zero_used_by_vector_search() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Default eps_query = 0.0 scans only the nearest partition (query 0.5 is nearest centroid 0).
    let result = store
        .vector_search(&search_value(0.5, 10))
        .expect("default search");
    assert_eq!(
        result.hits.len(),
        2,
        "default scans only partition 0 (subjects 1,2)"
    );
    assert!(
        result
            .hits
            .iter()
            .all(|h| h.subject == subject(1) || h.subject == subject(2))
    );
}

#[test]
fn partition_scan_eps_zero_loses_boundary_recall_that_eps_positive_recovers() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // A query midway between the two clusters (5.5) is nearest centroid 1 (dist 81 vs 121), so
    // eps_query = 0 scans only partition 1 and drops the partition-0 members that are still in the
    // true top-4. Raising eps past the boundary distance (121 / 81 - 1 ≈ 0.49) recovers full recall,
    // matching the exact scan. This is the recall/cost tradeoff slice 6 tunes the default by.
    let req = search_value(5.5, 4);
    let eps0 = store.vector_search(&req).expect("default eps=0 search");
    let eps05 = store
        .vector_search_tuned(&req, tuned(0.5))
        .expect("eps=0.5 search");
    let exact = store
        .vector_search_tuned(&req, tuned(f32::INFINITY))
        .expect("eps=INF exact-parity search");
    assert_eq!(
        eps0.hits.len(),
        2,
        "eps=0 scans only the nearer partition and loses boundary recall"
    );
    let e: Vec<_> = exact.hits.iter().map(|h| (h.subject, h.distance)).collect();
    let p: Vec<_> = eps05.hits.iter().map(|h| (h.subject, h.distance)).collect();
    assert_eq!(
        p, e,
        "eps=0.5 scans both partitions and matches the exact top-4"
    );
}

#[test]
fn partition_scan_scores_non_tombstoned_row_regardless_of_deleted_subject() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{FixedSubjectMapEntry, SubjectKey};

    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(1))
        .expect("seeded entry");
    // The search no longer consults the subject map: it scores every non-tombstoned row, relying on
    // the write-path invariant. Even if the subject-map entry is marked deleted (without the row being
    // tombstoned, which the write path would do), the non-tombstoned row is still scored.
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
        m.insert(
            SubjectKey::new(INDEX_ID, subject(1)),
            FixedSubjectMapEntry {
                deleted: true,
                ..entry
            },
        )
        .expect("insert deleted entry");
    });
    let result = store
        .vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

/// The search scores every non-tombstoned row, relying on the write-path invariant that a
/// non-tombstoned row in the active version is the subject's current live slot. A row with no
/// `VECTOR_SUBJECT_TO_ID` entry (which the write path would never leave) is still scored.
#[test]
fn partition_scan_scores_non_tombstoned_row_without_subject_entry() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::SubjectKey;

    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Drop the subject-map entry for subject 1: its slab row is still non-tombstoned and is scored.
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| m.remove(&SubjectKey::new(INDEX_ID, subject(1))));
    let result = store
        .vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

// --- ADR 0034 Slice 6: candidate allowlist ---
#[test]
fn partition_scan_scores_non_tombstoned_row_despite_slot_drift() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{FixedSubjectMapEntry, SlotRef, SubjectKey};

    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(1))
        .expect("seeded entry");
    let live_slot = entry.slot.expect("live slot");
    // Point the subject map at an out-of-range slot (positional drift). The search no longer consults
    // the subject map, so the non-tombstoned row at the seeded position is still scored.
    let drifted = SlotRef {
        slot: live_slot.slot + 10_000,
        ..live_slot
    };
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
        m.insert(
            SubjectKey::new(INDEX_ID, subject(1)),
            FixedSubjectMapEntry {
                slot: Some(drifted),
                ..entry
            },
        )
        .expect("insert drifted entry");
    });
    let result = store
        .vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

#[test]
fn stale_centroids_fall_back_to_exact_scan() {
    use crate::facade::stable::IVF_CENTROID_META;
    use crate::records::IvfCentroidMeta;

    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Mark the centroids stale (trained against a different index version): search must fall back to
    // the exact subject-map scan, which ignores nprobe and scans every live subject.
    IVF_CENTROID_META.with_borrow_mut(|m| {
        m.insert(
            INDEX_ID,
            IvfCentroidMeta {
                centroid_ready: true,
                centroid_epoch: 1,
                trained_index_version: 999,
            },
        )
    });
    // eps_query = 0 would restrict to one partition if the partition scan ran; the exact fallback
    // returns all four regardless.
    let result = store
        .vector_search_tuned(&search_nonzero(0.0, 10), tuned(0.0))
        .expect("exact fallback");
    assert_eq!(
        result.hits.len(),
        4,
        "stale centroids => exact scan over all subjects"
    );
}

#[test]
fn exact_scan_chunk_boundary_accumulates_top_k_across_chunks() {
    let store = fresh_store();
    // More live subjects than the exact scan's SCAN_CHUNK (4096), valued by vertex id. A query at
    // 4096.0 puts the nearest hit (v4096) in the second chunk and a top-k hit (v4095) in the first
    // chunk, so the global top-3 must accumulate correctly across the chunk flush.
    for v in 0..5000u32 {
        store
            .vector_upsert(shard_canister(), &upsert_vec(v, 1, v as f32))
            .expect("upsert");
    }
    let result = store
        .vector_search(&search_value(4096.0, 3))
        .expect("exact scan");
    let subjects: Vec<_> = result.hits.iter().map(|h| h.subject).collect();
    assert_eq!(
        subjects,
        vec![subject(4096), subject(4095), subject(4097)],
        "chunked exact scan accumulates the global top-3 across the SCAN_CHUNK boundary"
    );
}

#[test]
#[should_panic(expected = "must be >= 0")]
fn tuned_negative_eps_query_panics() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let _ = store.vector_search_tuned(&search_nonzero(0.0, 10), tuned(-1.0));
}

// --- ADR 0031 Slice 7: production shadow-version rebuild + dual-write ---

use crate::facade::stable::{IVF_CENTROIDS, PAGE_STORE, VECTOR_PARTITION_HEADS};
use crate::records::PartitionKey;
use gleaph_graph_kernel::vector_index::VectorRebuildPhase;

/// Index version of the first production rebuild's shadow (active starts at 1).
const TARGET_V: u64 = 2;

/// Seeds `count` live subjects via production upserts with distinct values `0.0..count` so a rebuild
/// can sample distinct centroids. Returns nothing; subjects are `subject(1..=count)`.
fn seed_distinct(store: &VectorCanisterStore, count: u32) {
    for v in 1..=count {
        store
            .vector_upsert(shard_canister(), &upsert_vec(v, 1, (v - 1) as f32))
            .expect("seed upsert");
    }
}

/// Drives `admin_vector_rebuild_step` (small batch to exercise cursor resumption) until the phase
/// leaves `Sampling`/`Building`, returning the terminal status.
fn drive_steps(
    store: &VectorCanisterStore,
    index_id: u32,
) -> gleaph_graph_kernel::vector_index::VectorRebuildStatus {
    for _ in 0..100_000 {
        let status = store
            .admin_vector_rebuild_step(router(), index_id, 1)
            .expect("step");
        match status.phase {
            VectorRebuildPhase::Sampling
            | VectorRebuildPhase::Training
            | VectorRebuildPhase::Building => continue,
            _ => return status,
        }
    }
    panic!("rebuild steps did not terminate");
}

/// Drives steps through `Sampling` + `Training` until the phase first reaches `Building` (centroids
/// written, no subjects shadowed yet), returning that status. Panics if it terminates earlier (e.g.
/// `Failed`).
fn drive_into_building(
    store: &VectorCanisterStore,
    index_id: u32,
) -> gleaph_graph_kernel::vector_index::VectorRebuildStatus {
    for _ in 0..100_000 {
        let status = store
            .admin_vector_rebuild_step(router(), index_id, 100)
            .expect("step");
        match status.phase {
            VectorRebuildPhase::Sampling | VectorRebuildPhase::Training => continue,
            VectorRebuildPhase::Building => return status,
            other => panic!("expected Building, reached {other:?}"),
        }
    }
    panic!("rebuild did not reach Building");
}

/// Drives `admin_vector_rebuild_cleanup_step` (one unit at a time) until `Idle`, returning the step
/// count so a test can assert teardown was bounded across multiple messages.
fn drive_cleanup(store: &VectorCanisterStore, index_id: u32) -> u32 {
    for steps in 1..=100_000u32 {
        let status = store
            .admin_vector_rebuild_cleanup_step(router(), index_id, 1)
            .expect("cleanup");
        if status.phase == VectorRebuildPhase::Idle {
            return steps;
        }
    }
    panic!("cleanup did not finish");
}

fn target_centroid_count(index_id: u32, version: u64, nlist: u32) -> u32 {
    IVF_CENTROIDS.with_borrow(|m| {
        (0..nlist)
            .filter(|p| m.get(&PartitionKey::new(index_id, version, *p)).is_some())
            .count() as u32
    })
}

#[test]
fn rebuild_start_is_o1_and_enters_sampling() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let status = store
        .admin_vector_rebuild_status(router(), INDEX_ID)
        .expect("status");
    assert_eq!(status.phase, VectorRebuildPhase::Sampling);
    assert_eq!(status.target_index_version, TARGET_V);
    assert_eq!(
        status.candidates_collected, 0,
        "start collects no candidates"
    );
    assert_eq!(
        target_centroid_count(INDEX_ID, TARGET_V, 2),
        0,
        "start writes no centroids"
    );
}

#[test]
fn rebuild_sampling_writes_nlist_centroids_then_builds_to_ready() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let status = drive_steps(&store, INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
    assert_eq!(
        target_centroid_count(INDEX_ID, TARGET_V, 2),
        2,
        "exactly nlist centroids written"
    );
    // Every live subject has a shadow slot at the target version.
    for v in 1..=4u32 {
        let entry = store.subject_entry_for_test(INDEX_ID, subject(v)).unwrap();
        let shadow = entry.shadow_slot.expect("shadow slot");
        assert_eq!(shadow.index_version, TARGET_V as u32);
    }
}

#[test]
fn rebuild_start_rejects_invalid_params() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    // nlist < 2
    assert_eq!(
        store
            .admin_start_vector_rebuild(router(), INDEX_ID, 1, 100)
            .unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
    // sample_limit < nlist
    assert_eq!(
        store
            .admin_start_vector_rebuild(router(), INDEX_ID, 4, 3)
            .unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
    // nlist > MAX_NLIST
    assert_eq!(
        store
            .admin_start_vector_rebuild(
                router(),
                INDEX_ID,
                super::MAX_NLIST + 1,
                super::MAX_NLIST + 1
            )
            .unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
}

#[test]
fn rebuild_start_rejects_oversized_combined_state() {
    let store = fresh_store();
    // A large-dim index whose `2 * nlist * stride + overhead` (candidate-pool floor + trained
    // centroids + encoding overhead) exceeds the combined rebuild-state envelope even though
    // `nlist <= MAX_NLIST`, because `stride_bytes` scales with dims (ADR 0031 Slice 8, P2/P3).
    let big_dims: u16 = 2100; // stride = 8400 bytes (F32)
    let stride = big_dims as usize * 4;
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(1),
        mutation_id: 1,
        encoding: VectorEncoding::F32,
        dims: big_dims,
        metric: VectorMetric::L2Squared,
        bytes: vec![0u8; stride],
        remove: false,
    };
    store
        .vector_upsert(shard_canister(), &op)
        .expect("seed large-dim upsert");
    assert!(
        2 * super::MAX_NLIST as u64 * stride as u64 + super::MAX_REBUILD_STATE_OVERHEAD_BYTES
            > super::MAX_REBUILD_STATE_BYTES,
        "fixture must exceed the combined-state cap"
    );
    assert_eq!(
        store
            .admin_start_vector_rebuild(router(), INDEX_ID, super::MAX_NLIST, super::MAX_NLIST)
            .unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
}

#[test]
fn rebuild_step_and_cleanup_accept_oversized_caller_budget() {
    // A huge caller budget (`u32::MAX`) is clamped, never rejected: step/cleanup still succeed and
    // drive the rebuild to completion. (The exact `1..=MAX_REBUILD_STEP_WORK` clamp is unit-tested in
    // `rebuild::tests::clamp_step_work_bounds_caller_budget`.)
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let mut status = store
        .admin_vector_rebuild_step(router(), INDEX_ID, u32::MAX)
        .expect("step accepts u32::MAX");
    while matches!(
        status.phase,
        VectorRebuildPhase::Sampling | VectorRebuildPhase::Training | VectorRebuildPhase::Building
    ) {
        status = store
            .admin_vector_rebuild_step(router(), INDEX_ID, u32::MAX)
            .expect("step accepts u32::MAX");
    }
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    for _ in 0..100_000 {
        let status = store
            .admin_vector_rebuild_cleanup_step(router(), INDEX_ID, u32::MAX)
            .expect("cleanup accepts u32::MAX");
        if status.phase == VectorRebuildPhase::Idle {
            return;
        }
    }
    panic!("clamped cleanup did not finish");
}

#[test]
fn rebuild_step_is_bounded_by_per_step_vector_bytes() {
    // With a tiny injected byte budget (one vector's worth), each `Sampling`/`Building` step buffers
    // exactly one vector and breaks, so the contract "a step does not finish in one message; a cursor
    // survives" is observable on a small fixture (no `MAX_REBUILD_STEP_WORK`-sized seeding needed).
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let one_vector = STRIDE as u64;

    // First sampling step buffers exactly one vector -> one distinct candidate (the per-step byte
    // budget truncates work; the pool keeps filling across steps).
    let status = store
        .rebuild_step_with_budget(INDEX_ID, u32::MAX, one_vector)
        .expect("sampling step");
    assert_eq!(status.phase, VectorRebuildPhase::Sampling);
    assert_eq!(
        status.candidates_collected, 1,
        "byte budget truncates sampling to one buffered vector per step"
    );

    // Drive the byte-bounded Sampling -> Training -> Building pipeline to completion. Every step is
    // bounded; the run still reaches ReadyToPublish.
    let mut status = status;
    for _ in 0..1000 {
        if status.phase == VectorRebuildPhase::ReadyToPublish {
            break;
        }
        assert!(
            matches!(
                status.phase,
                VectorRebuildPhase::Sampling
                    | VectorRebuildPhase::Training
                    | VectorRebuildPhase::Building
            ),
            "unexpected phase {:?}",
            status.phase
        );
        status = store
            .rebuild_step_with_budget(INDEX_ID, u32::MAX, one_vector)
            .expect("bounded step");
    }
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);

    // The byte-bounded build is equivalent to an unbounded one: parity after publish holds.
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    drive_cleanup(&store, INDEX_ID);
    for v in 1..=4u32 {
        let entry = store.subject_entry_for_test(INDEX_ID, subject(v)).unwrap();
        let slot = entry.slot.expect("collapsed live slot");
        assert_eq!(slot.index_version, TARGET_V as u32);
        assert_eq!(entry.shadow_slot, None);
    }
}

#[test]
fn rebuild_already_active_is_rejected() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    assert_eq!(
        store
            .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
            .unwrap_err(),
        VectorCanisterError::RebuildAlreadyActive
    );
}

#[test]
fn rebuild_sampling_fails_on_insufficient_distinct_vectors_then_recovers() {
    let store = fresh_store();
    // Three live subjects but only ONE distinct value: cannot form 2 distinct centroids.
    store
        .vector_upsert(shard_canister(), &upsert_vec(1, 1, 5.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(2, 1, 5.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(3, 1, 5.0))
        .unwrap();
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let status = drive_steps(&store, INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Failed);

    // Failed recovers to Idle via abort (O(1), nothing persisted), then a new rebuild can start.
    store
        .admin_abort_vector_rebuild(router(), INDEX_ID)
        .expect("abort failed");
    assert_eq!(
        store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .unwrap()
            .phase,
        VectorRebuildPhase::Idle
    );
    // Add two distinct values so a fresh rebuild can now sample 2 centroids.
    store
        .vector_upsert(shard_canister(), &upsert_vec(10, 1, 0.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(11, 1, 1.0))
        .unwrap();
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("restart after recovery");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
}

#[test]
fn publish_rejected_before_ready() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    // Still Sampling.
    assert_eq!(
        store
            .admin_publish_vector_rebuild(router(), INDEX_ID)
            .unwrap_err(),
        VectorCanisterError::RebuildNotReadyToPublish
    );
}

#[test]
fn publish_switches_to_partition_search_with_exact_parity() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    let before = store.vector_search(&search_value(1.5, 10)).expect("exact");

    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");

    let def = store.def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.active_index_version, TARGET_V);
    assert_eq!(def.nlist, 2);

    // Default search now runs the partition scan. The default `eps_query = 0.0` is a recall knob
    // (nearest partition only), so exact parity is asserted at the full scan (`eps_query = INFINITY`),
    // which is independent of the candidate-pool iteration order.
    let after = store
        .vector_search_tuned(&search_value(1.5, 10), tuned(f32::INFINITY))
        .expect("partition full scan");
    assert_eq!(after.hits, before.hits);
}

#[test]
fn upsert_during_building_survives_publish() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    // Reach Building (centroids written), then insert a new subject mid-rebuild.
    let status = drive_into_building(&store, INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Building);
    store
        .vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0))
        .expect("dual-write upsert");
    let entry = store.subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    assert!(
        entry.shadow_slot.is_some(),
        "dual-write created a shadow slot"
    );

    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    let after = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(
        after.hits.iter().any(|h| h.subject == subject(99)),
        "subject inserted during Building is searchable after publish"
    );
}

#[test]
fn dual_write_shadow_append_failure_rolls_back_insert() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    drive_into_building(&store, INDEX_ID); // -> Building (dual-write)

    let live_before = store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Inject a slab `grow` failure for the shadow append: the active append (1st) succeeds, the
    // shadow append (2nd) fails. This is the StableGrowFailed branch normal unit tests cannot reach.
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = store
        .vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);

    // Insert path commits the id/subject maps only after both appends succeed, so a new subject must
    // leave no map entry behind.
    assert!(
        store
            .subject_entry_for_test(INDEX_ID, subject(99))
            .is_none(),
        "no subject map entry created on rollback"
    );
    // The active row was appended then tombstoned, so live accounting is restored (not a live-counted
    // orphan polluting partition health).
    assert_eq!(
        store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "active live_len restored after rollback"
    );
}

#[test]
fn dual_write_shadow_append_failure_rolls_back_update() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    drive_into_building(&store, INDEX_ID); // -> Building (dual-write)

    let before = store.subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    let old_slot = before.slot.expect("seeded subject is live");
    let live_before = store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Inject a slab `grow` failure for the shadow append (active append succeeds first).
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = store
        .vector_upsert(shard_canister(), &upsert_vec(1, 2, 0.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);

    // The subject clock still points at the original live slot — no partial commit to a
    // tombstoned/new slot.
    let after = store.subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    assert_eq!(after.slot, Some(old_slot), "old slot stays live");
    assert_eq!(after.shadow_slot, None, "no shadow recorded");
    // The new active row was appended then tombstoned: net live_len unchanged.
    assert_eq!(
        store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "active live_len restored after rollback"
    );
}

#[test]
fn newer_stamp_upsert_commit_failure_keeps_old_slot_live() {
    // GAP-2026-08-07-001 regression: a newer-stamp upsert whose subject-map commit fails must leave
    // the old slot live (it is tombstoned only after a successful commit), not pointing at a
    // tombstoned row.
    let store = fresh_store();
    seed_distinct(&store, 4);
    let before = store.subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    let old_slot = before.slot.expect("seeded subject is live");
    let live_before = store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Force the subject-map commit to fail after the new active row is appended.
    crate::facade::store::mutation::arm_subject_insert_failure(0);
    let err = store
        .vector_upsert(shard_canister(), &upsert_vec(1, 2, 0.0))
        .expect_err("subject-map commit failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);

    // The old subject entry and its live slot are preserved — the old slot must NOT be tombstoned.
    let after = store.subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    assert_eq!(after.slot, Some(old_slot), "old slot stays live");
    assert!(
        store.read_slot_bytes(INDEX_ID, old_slot).is_some(),
        "old row remains live and searchable"
    );
    // The appended-then-tombstoned new row restores live accounting.
    assert_eq!(
        store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "live_len restored after commit rollback"
    );
}

#[test]
fn remove_during_building_does_not_resurrect_after_publish() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    drive_into_building(&store, INDEX_ID); // -> Building
    // Remove subject 4 while dual-writing.
    store
        .vector_remove(shard_canister(), &remove_op(4, 2))
        .expect("remove during building");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    let after = store.vector_search(&search_value(3.0, 10)).expect("search");
    assert!(
        !after.hits.iter().any(|h| h.subject == subject(4)),
        "removed subject must not resurrect after publish"
    );
}

#[test]
fn mutation_during_cleaning_collapses_on_touch() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    // Now in Cleaning; subject 2 is not yet collapsed (slot @ old version, shadow @ target).
    let pre = store.subject_entry_for_test(INDEX_ID, subject(2)).unwrap();
    assert_eq!(pre.slot.unwrap().index_version, 1);
    assert_eq!(pre.shadow_slot.unwrap().index_version, TARGET_V as u32);

    // Touch subject 2: a newer-version upsert must operate on the target version and collapse it.
    store
        .vector_upsert(shard_canister(), &upsert_vec(2, 2, 1.0))
        .expect("upsert during cleaning");
    let post = store.subject_entry_for_test(INDEX_ID, subject(2)).unwrap();
    assert_eq!(
        post.slot.unwrap().index_version,
        TARGET_V as u32,
        "collapsed to target"
    );
    assert_eq!(post.shadow_slot, None, "shadow cleared on touch");

    // Cleanup finishes and search stays correct.
    drive_cleanup(&store, INDEX_ID);
    let after = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(after.hits.iter().any(|h| h.subject == subject(2)));
}

#[test]
fn cleanup_is_bounded_and_resumable_to_idle() {
    let store = fresh_store();
    seed_distinct(&store, 6);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    let steps = drive_cleanup(&store, INDEX_ID);
    assert!(steps > 1, "teardown spanned multiple bounded steps");
    // Old-version page meta is gone; the index is fully on the target version.
    let old_pages = PAGE_STORE.with_borrow(|s| s.version_page_count(INDEX_ID, 1));
    assert_eq!(old_pages, 0, "old-version page meta dropped");
    let after = store
        .vector_search_tuned(&search_value(2.0, 10), tuned(f32::INFINITY))
        .expect("search");
    assert_eq!(after.hits.len(), 6);
}

#[test]
fn abort_during_building_is_bounded_and_leaves_active_unchanged() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    let before = store.vector_search(&search_value(1.5, 10)).expect("exact");
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let status = drive_into_building(&store, INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Building);
    store
        .admin_abort_vector_rebuild(router(), INDEX_ID)
        .expect("abort");
    drive_cleanup(&store, INDEX_ID);

    // Active version unchanged; shadow pages and centroids gone.
    let def = store.def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.active_index_version, 1);
    assert_eq!(def.nlist, 1);
    assert_eq!(target_centroid_count(INDEX_ID, TARGET_V, 2), 0);
    let after = store.vector_search(&search_value(1.5, 10)).expect("exact");
    assert_eq!(after.hits, before.hits, "active search unchanged by abort");
    // A fresh rebuild can start again.
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("restart after abort");
}

#[test]
fn abort_from_sampling_is_immediate_idle() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    store
        .admin_abort_vector_rebuild(router(), INDEX_ID)
        .expect("abort from sampling");
    assert_eq!(
        store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .unwrap()
            .phase,
        VectorRebuildPhase::Idle
    );
}

#[test]
fn post_publish_nlist_gt_1_upsert_assigns_nearest_partition() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    drive_cleanup(&store, INDEX_ID);

    // Index is now published nlist=2 with no active rebuild. A new upsert must assign by centroid.
    store
        .vector_upsert(shard_canister(), &upsert_vec(50, 1, 0.0))
        .expect("post-publish upsert");
    let entry = store.subject_entry_for_test(INDEX_ID, subject(50)).unwrap();
    let slot = entry.slot.unwrap();
    assert_eq!(slot.index_version, TARGET_V as u32);
    // Furthest-point seeding on values {0..3} with nlist=2 gives centroids [2.5 (p0), 0.5 (p1)], so
    // a value-0 upsert lands in the nearest (0.5) partition, p1 — not the degenerate partition 0.
    assert_eq!(
        slot.partition_id, 1,
        "value 0 lands in the nearest-centroid partition"
    );
    let after = store
        .vector_search(&search_nonzero(0.0, 10))
        .expect("search");
    assert!(after.hits.iter().any(|h| h.subject == subject(50)));
}

#[test]
fn second_rebuild_from_partitioned_active() {
    let store = fresh_store();
    seed_distinct(&store, 6);
    // First rebuild to nlist=2 and fully publish + clean.
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start 1");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish 1");
    drive_cleanup(&store, INDEX_ID);
    // Full-scan parity baseline (eps_query = INFINITY), independent of the candidate-pool order.
    let before = store
        .vector_search_tuned(&search_value(2.5, 10), tuned(f32::INFINITY))
        .expect("full scan");

    // Second rebuild to nlist=3 from the partitioned (nlist=2) active version.
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 3, 100)
        .expect("start 2");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish 2");
    let def = store.def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.active_index_version, 3);
    assert_eq!(def.nlist, 3);
    drive_cleanup(&store, INDEX_ID);

    // Parity to the pre-second-rebuild result at nprobe = nlist (full scan).
    let after = store
        .vector_search_tuned(&search_value(2.5, 10), tuned(f32::INFINITY))
        .expect("tuned");
    assert_eq!(after.hits, before.hits);
}

#[test]
fn publish_succeeds_with_an_empty_partition() {
    let store = fresh_store();
    // Subjects: values 0, 10, 5, 0, 10. The val-5 subject (3) becomes centroid 2's source but is
    // removed during Building, leaving centroid 2's partition empty.
    store
        .vector_upsert(shard_canister(), &upsert_vec(1, 1, 0.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(2, 1, 10.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(3, 1, 5.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(4, 1, 0.0))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec(5, 1, 10.0))
        .unwrap();

    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 3, 100)
        .expect("start");
    // Sampling collects the 3 distinct candidates [0, 10, 5]; Training writes the 3 centroids and
    // enters Building (each distinct candidate seeds and stays its own centroid).
    let status = drive_into_building(&store, INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Building);
    // Remove the val-5 subject so no live vector is nearest to centroid 2.
    store
        .vector_remove(shard_canister(), &remove_op(3, 2))
        .expect("remove val-5");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );

    // Partition 2 received no vector: no head materialized for it (empty partition is valid).
    let head_p2 =
        VECTOR_PARTITION_HEADS.with_borrow(|m| m.get(&PartitionKey::new(INDEX_ID, TARGET_V, 2)));
    assert!(head_p2.is_none(), "empty partition materializes no head");

    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish tolerates empty partition");
    // Full-scan search returns the four remaining live subjects.
    let after = store
        .vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("search");
    assert_eq!(after.hits.len(), 4);
}

// --- ADR 0031 Slice 8: bounded training quality + partition health ---

#[test]
fn sampling_collects_more_than_nlist_candidates() {
    let store = fresh_store();
    // Eight distinct live vectors but only nlist=2: sampling collects the whole bounded pool, not
    // just two, before entering Training (ADR 0031 Slice 8, P3).
    seed_distinct(&store, 8);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    // One large sampling step exhausts the (8-subject) range -> Training with all 8 candidates.
    let status = store
        .admin_vector_rebuild_step(router(), INDEX_ID, 100)
        .expect("sampling step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    assert_eq!(
        status.candidates_collected, 8,
        "sampling collects the whole distinct pool, not just nlist"
    );
    assert_eq!(status.training_iteration, 0);
}

#[test]
fn training_produces_nlist_valid_centroids() {
    let store = fresh_store();
    seed_distinct(&store, 8);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 3, 100)
        .expect("start");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    assert_eq!(
        target_centroid_count(INDEX_ID, TARGET_V, 3),
        3,
        "exactly nlist centroids written"
    );
    IVF_CENTROIDS.with_borrow(|m| {
        for p in 0..3 {
            let bytes = m
                .get(&PartitionKey::new(INDEX_ID, TARGET_V, p))
                .expect("centroid present");
            assert_eq!(bytes.len(), STRIDE, "centroid {p} is dims-valid");
        }
    });
}

#[test]
fn training_is_deterministic() {
    fn run() -> Vec<Vec<u8>> {
        let store = fresh_store();
        seed_distinct(&store, 8);
        store
            .admin_start_vector_rebuild(router(), INDEX_ID, 3, 100)
            .expect("start");
        assert_eq!(
            drive_steps(&store, INDEX_ID).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        IVF_CENTROIDS.with_borrow(|m| {
            (0..3)
                .map(|p| {
                    m.get(&PartitionKey::new(INDEX_ID, TARGET_V, p))
                        .expect("centroid")
                })
                .collect()
        })
    }
    // `fresh_store` clears the shared thread-local state, so two sequential runs over the same seed
    // must yield byte-identical centroids.
    let first = run();
    let second = run();
    assert_eq!(
        first, second,
        "k-means-lite training is deterministic for the same sample order"
    );
}

#[test]
fn training_writes_no_pages_or_centroids() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    // One step completes Sampling and enters Training (iteration 0).
    let status = store
        .admin_vector_rebuild_step(router(), INDEX_ID, 100)
        .expect("step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    // Centroids live in the durable state record until the transition to Building; nothing is
    // published to IVF_CENTROIDS or VECTOR_PAGE_META during Training.
    assert_eq!(
        target_centroid_count(INDEX_ID, TARGET_V, 2),
        0,
        "no centroids written during Training"
    );
    let target_pages = PAGE_STORE.with_borrow(|s| s.version_page_count(INDEX_ID, TARGET_V));
    assert_eq!(target_pages, 0, "Training writes no shadow pages");
}

#[test]
fn abort_from_training_is_immediate_idle() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let status = store
        .admin_vector_rebuild_step(router(), INDEX_ID, 100)
        .expect("step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    store
        .admin_abort_vector_rebuild(router(), INDEX_ID)
        .expect("abort from training");
    assert_eq!(
        store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .unwrap()
            .phase,
        VectorRebuildPhase::Idle
    );
    // O(1) recovery: a fresh rebuild can start again.
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("restart after abort");
}

#[test]
fn upsert_during_training_is_active_only_then_shadowed_by_building() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    let status = store
        .admin_vector_rebuild_step(router(), INDEX_ID, 100)
        .expect("step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    // A new subject upserted during Training is active-only (no shadow slot yet).
    store
        .vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0))
        .expect("active-only upsert");
    let entry = store.subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    assert!(
        entry.shadow_slot.is_none(),
        "mutation during Training is active-only"
    );
    // Building walks every live subject and shadows it; publish makes it searchable.
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    let entry = store.subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    assert!(
        entry.shadow_slot.is_some(),
        "Building shadows the Training-era mutation"
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    let after = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(after.hits.iter().any(|h| h.subject == subject(99)));
}

#[test]
fn partition_health_reports_skew_and_empty_partitions() {
    let store = fresh_store();
    // Three centroids [0, 10, 20]; populate only the first two (3 rows near 0, 1 row near 10), so
    // partition 2 stays empty and partition 0 is the skew peak.
    let centroids = vec![cvec(0.0), cvec(10.0), cvec(20.0)];
    let vectors = vec![
        (subject(1), cvec(0.0)),
        (subject(2), cvec(0.1)),
        (subject(3), cvec(0.2)),
        (subject(4), cvec(10.0)),
    ];
    store.seed_ivf_for_test(INDEX_ID, VectorEncoding::F32, DIMS, &centroids, &vectors);

    let health = store
        .admin_vector_partition_health(router(), INDEX_ID)
        .expect("health");
    assert_eq!(health.nlist, 3);
    assert_eq!(
        health.partitions_examined, 2,
        "empty partition 2 materializes no head"
    );
    assert_eq!(health.live_rows, 4);
    assert_eq!(
        health.max_partition_live_rows, 3,
        "skew peak is the 3-row partition"
    );
    assert!(
        health.page_count >= 2,
        "at least one page per non-empty partition"
    );
}

#[test]
fn partition_health_unknown_index_errors() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_vector_partition_health(router(), 999)
            .unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
}

#[test]
fn slab_stats_dual_write_rollback_keeps_live_and_counts_tombstone() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    drive_into_building(&store, INDEX_ID); // -> Building (dual-write)

    let before = store
        .admin_vector_slab_stats(router(), Some(INDEX_ID))
        .expect("stats");
    // Force the shadow append to fail; the active append succeeds first and is then rolled back
    // (tombstoned) by vector_upsert.
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = store
        .vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);
    let after = store
        .admin_vector_slab_stats(router(), Some(INDEX_ID))
        .expect("stats");

    assert_eq!(
        after.scope.physical_live_row_count, before.scope.physical_live_row_count,
        "rolled-back active row is not counted as physically live"
    );
    assert_eq!(
        after.scope.tombstone_row_count,
        before.scope.tombstone_row_count + 1,
        "the compensated active row is counted as a tombstone"
    );
}

#[test]
fn slab_stats_rejects_non_router_caller() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_vector_slab_stats(shard_canister(), None)
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

#[test]
fn slab_stats_step_rejects_non_router_caller() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_vector_slab_stats_step(shard_canister(), None, 10, None)
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

// --- ADR 0031 Slice 9: facade page-meta health step + rebuild-if-recommended trigger ---

/// Tombstone-dominant policy (skew disabled by an unreachable threshold) for trigger tests.
fn trigger_policy() -> VectorMaintenancePolicy {
    VectorMaintenancePolicy {
        recommended_tombstone_ratio_bps: 2_000, // 20%
        required_tombstone_ratio_bps: 5_000,    // 50%
        recommended_skew_ratio_bps: u32::MAX,
        required_skew_ratio_bps: u32::MAX,
        min_total_rows: 100,
        min_tombstoned_rows: 10,
    }
}

/// Page-meta health scoped to the active version with the given tombstone load.
fn attested_page(
    store: &VectorCanisterStore,
    total_rows: u64,
    tombstoned_rows: u64,
) -> VectorPartitionPageHealth {
    let def = store.def_for_test(INDEX_ID).expect("def");
    VectorPartitionPageHealth {
        index_id: INDEX_ID,
        index_version: def.active_index_version,
        page_count: 1,
        total_rows,
        physical_live_rows: total_rows - tombstoned_rows,
        tombstoned_rows,
    }
}

#[test]
fn partition_health_step_facade_resolves_active_version_and_merges() {
    let store = fresh_store();
    seed_distinct(&store, 4); // 4 live rows, version 1, degenerate partition 0
    // Re-upsert subject 1 at a newer embedding_version: tombstones the old row, appends a new one.
    store
        .vector_upsert(shard_canister(), &upsert_vec(1, 2, 5.0))
        .expect("re-upsert");

    let mut merged = VectorPartitionPageHealth::default();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let step = store
            .admin_vector_partition_health_step(router(), INDEX_ID, cursor.clone(), 1)
            .expect("health step");
        merged.index_id = step.partial.index_id;
        merged.index_version = step.partial.index_version;
        merged.page_count += step.partial.page_count;
        merged.total_rows += step.partial.total_rows;
        merged.physical_live_rows += step.partial.physical_live_rows;
        merged.tombstoned_rows += step.partial.tombstoned_rows;
        let done = step.exhausted;
        cursor = step.cursor.clone();
        if done {
            break;
        }
    }
    assert_eq!(merged.index_id, INDEX_ID);
    assert_eq!(merged.index_version, 1);
    assert_eq!(merged.total_rows, 5, "4 original + 1 appended");
    assert_eq!(merged.physical_live_rows, 4, "4 live after re-upsert");
    assert_eq!(
        merged.tombstoned_rows, 1,
        "the superseded row is tombstoned"
    );
}

#[test]
fn partition_health_step_facade_rejects_non_router_and_unknown_index() {
    let store = fresh_store();
    seed_distinct(&store, 2);
    assert_eq!(
        store
            .admin_vector_partition_health_step(shard_canister(), INDEX_ID, None, 10)
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        store
            .admin_vector_partition_health_step(router(), 999, None, 10)
            .unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
}

#[test]
fn trigger_healthy_does_not_start_rebuild() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    let rec = store
        .admin_start_vector_rebuild_if_recommended(
            router(),
            INDEX_ID,
            attested_page(&store, 1_000, 100), // 10% < recommended
            trigger_policy(),
            Some(2),
            100,
        )
        .expect("trigger");
    assert_eq!(rec, VectorMaintenanceRecommendation::Healthy);
    assert_eq!(
        store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase,
        VectorRebuildPhase::Idle,
        "a healthy report must not start a rebuild"
    );
}

#[test]
fn trigger_required_starts_rebuild_at_target_nlist() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    let rec = store
        .admin_start_vector_rebuild_if_recommended(
            router(),
            INDEX_ID,
            attested_page(&store, 1_000, 600), // 60% >= required
            trigger_policy(),
            Some(2),
            100,
        )
        .expect("trigger");
    assert_eq!(rec, VectorMaintenanceRecommendation::RebuildRequired);
    let status = store
        .admin_vector_rebuild_status(router(), INDEX_ID)
        .expect("status");
    assert_eq!(status.phase, VectorRebuildPhase::Sampling);
    assert_eq!(status.target_index_version, TARGET_V);
}

#[test]
fn trigger_recommended_starts_rebuild() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    let rec = store
        .admin_start_vector_rebuild_if_recommended(
            router(),
            INDEX_ID,
            attested_page(&store, 1_000, 300), // 30%: recommended band
            trigger_policy(),
            Some(2),
            100,
        )
        .expect("trigger");
    assert_eq!(rec, VectorMaintenanceRecommendation::RebuildRecommended);
    assert_eq!(
        store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase,
        VectorRebuildPhase::Sampling
    );
}

#[test]
fn trigger_degenerate_nlist_without_target_is_rejected() {
    let store = fresh_store();
    seed_distinct(&store, 4); // def.nlist == 1
    assert_eq!(
        store
            .admin_start_vector_rebuild_if_recommended(
                router(),
                INDEX_ID,
                attested_page(&store, 1_000, 600),
                trigger_policy(),
                None, // no target, and def.nlist == 1 -> cannot default
                100,
            )
            .unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
}

#[test]
fn trigger_rejects_stale_page_health() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    // Wrong active version on the page health is rejected (the skew summary is recomputed
    // server-side, so it has no stale surface of its own).
    let mut stale_page = attested_page(&store, 1_000, 600);
    stale_page.index_version = 999;
    assert_eq!(
        store
            .admin_start_vector_rebuild_if_recommended(
                router(),
                INDEX_ID,
                stale_page,
                trigger_policy(),
                Some(2),
                100,
            )
            .unwrap_err(),
        VectorCanisterError::StaleMaintenanceHealth
    );
    // Wrong index_id on the page health is likewise rejected.
    let mut foreign_page = attested_page(&store, 1_000, 600);
    foreign_page.index_id = INDEX_ID + 1;
    assert_eq!(
        store
            .admin_start_vector_rebuild_if_recommended(
                router(),
                INDEX_ID,
                foreign_page,
                trigger_policy(),
                Some(2),
                100,
            )
            .unwrap_err(),
        VectorCanisterError::StaleMaintenanceHealth
    );
}

#[test]
fn trigger_rejects_invalid_policy() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    let mut bad = trigger_policy();
    bad.recommended_tombstone_ratio_bps = 6_000;
    bad.required_tombstone_ratio_bps = 5_000;
    assert_eq!(
        store
            .admin_start_vector_rebuild_if_recommended(
                router(),
                INDEX_ID,
                attested_page(&store, 1_000, 600),
                bad,
                Some(2),
                100,
            )
            .unwrap_err(),
        VectorCanisterError::InvalidMaintenancePolicy
    );
}

#[test]
fn trigger_rejects_non_router_and_unknown_index() {
    let store = fresh_store();
    seed_distinct(&store, 4);
    assert_eq!(
        store
            .admin_start_vector_rebuild_if_recommended(
                shard_canister(),
                INDEX_ID,
                attested_page(&store, 1_000, 600),
                trigger_policy(),
                Some(2),
                100,
            )
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    let empty = fresh_store();
    assert_eq!(
        empty
            .admin_start_vector_rebuild_if_recommended(
                router(),
                INDEX_ID,
                VectorPartitionPageHealth::default(),
                trigger_policy(),
                Some(2),
                100,
            )
            .unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
}

// --- ADR 0031 Slice 9: heap centroid cache ---

#[test]
fn centroid_cache_warmup_then_status_reports_one_entry() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let before = store
        .admin_vector_centroid_cache_status(router())
        .expect("status");
    assert_eq!(before.entries, 0);
    assert_eq!(before.bytes, 0);
    assert_eq!(before.max_bytes, 8 * 1024 * 1024);

    let after = store
        .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
        .expect("warmup");
    assert_eq!(after.entries, 1);
    assert!(after.bytes > 0, "a warmed nlist=2 set occupies heap bytes");
}

#[test]
fn centroid_cache_search_parity_cold_vs_warm() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let cold = store
        .vector_search_tuned(&search_value(0.5, 10), tuned(f32::INFINITY))
        .expect("cold scan");
    store
        .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
        .expect("warmup");
    let warm = store
        .vector_search_tuned(&search_value(0.5, 10), tuned(f32::INFINITY))
        .expect("warm scan");
    let cold_hits: Vec<_> = cold.hits.iter().map(|h| (h.subject, h.distance)).collect();
    let warm_hits: Vec<_> = warm.hits.iter().map(|h| (h.subject, h.distance)).collect();
    assert_eq!(cold_hits, warm_hits, "warm cache yields identical results");
}

#[test]
fn centroid_cache_clear_empties() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    store
        .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
        .expect("warmup");
    let cleared = store
        .admin_vector_centroid_cache_clear(router())
        .expect("clear");
    assert_eq!(cleared.entries, 0);
    assert_eq!(cleared.bytes, 0);
}

#[test]
fn centroid_cache_warmup_skips_degenerate_index() {
    let store = fresh_store();
    seed_distinct(&store, 4); // degenerate nlist = 1
    let status = store
        .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
        .expect("warmup");
    assert_eq!(
        status.entries, 0,
        "a degenerate index has no centroid set to cache"
    );
}

#[test]
fn centroid_cache_warmup_unknown_index_errors() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_vector_centroid_cache_warmup(router(), 999)
            .unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
}

#[test]
fn centroid_cache_endpoints_reject_non_router() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_vector_centroid_cache_warmup(shard_canister(), INDEX_ID)
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        store
            .admin_vector_centroid_cache_clear(shard_canister())
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        store
            .admin_vector_centroid_cache_status(shard_canister())
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

#[test]
fn centroid_cache_publish_invalidates_warmed_entry() {
    let store = fresh_store();
    seed_distinct(&store, 6); // degenerate nlist = 1, version 1
    // First rebuild to nlist = 2 and publish so the active set is partitioned + ready.
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    drive_cleanup(&store, INDEX_ID);
    assert_eq!(
        store
            .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
            .expect("warmup")
            .entries,
        1
    );
    // A second rebuild + publish flips the active generation and must drop the warmed entry.
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, 2, 100)
        .expect("start 2");
    assert_eq!(
        drive_steps(&store, INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish 2");
    assert_eq!(
        store
            .admin_vector_centroid_cache_status(router())
            .expect("status")
            .entries,
        0,
        "publishing a new generation invalidates the warmed centroid entry"
    );
}

// --- ADR 0031 Slice 10: maintenance execution state machine ---

use crate::facade::stable::VECTOR_INDEX_DEFS;
use gleaph_graph_kernel::vector_index::{
    VectorMaintenanceState, VectorMaintenanceStepRequest, VectorMaintenanceStepResult,
};

/// Tombstone-dominant maintenance policy with low row gates so small fixtures can cross thresholds.
fn maint_policy() -> VectorMaintenancePolicy {
    VectorMaintenancePolicy {
        recommended_tombstone_ratio_bps: 2_000, // 20%
        required_tombstone_ratio_bps: 5_000,    // 50%
        recommended_skew_ratio_bps: u32::MAX,   // skew disabled for these tests
        required_skew_ratio_bps: u32::MAX,
        min_total_rows: 1,
        min_tombstoned_rows: 1,
    }
}

/// A maintenance step request with `nlist = 2` target and generous bounded budgets.
fn maint_req() -> VectorMaintenanceStepRequest {
    VectorMaintenanceStepRequest {
        policy: maint_policy(),
        target_nlist: Some(2),
        sample_limit: 100,
        scan_max_pages: 100,
        rebuild_max_subjects: 100,
        cleanup_max_work: 100,
    }
}

/// Seeds `live` distinct live rows then creates `tombstones` extra tombstoned rows by re-upserting
/// subject 1 at increasing embedding_versions (each re-upsert tombstones the prior row).
fn seed_live_and_tombstones(store: &VectorCanisterStore, live: u32, tombstones: u32) {
    seed_distinct(store, live);
    for k in 0..tombstones {
        store
            .vector_upsert(
                shard_canister(),
                &upsert_vec(1, 2 + k as u64, 100.0 + k as f32),
            )
            .expect("tombstone re-upsert");
    }
}

fn set_active_version(index_id: u32, version: u64) {
    VECTOR_INDEX_DEFS.with_borrow_mut(|defs| {
        let mut def = defs.get(&index_id).expect("def");
        def.active_index_version = version;
        defs.insert(index_id, def);
    });
}

#[test]
fn maintenance_step_scans_then_reports_healthy_and_resets() {
    let store = fresh_store();
    seed_distinct(&store, 4); // 4 live rows, no tombstones -> healthy

    // First step (from Idle) runs one scan step; the single degenerate page exhausts immediately.
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("scan step"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );
    // Second step recommends from the exhausted scan: healthy -> reset to Idle.
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("recommend step"),
        VectorMaintenanceStepResult::Healthy
    );
    assert_eq!(
        store
            .admin_vector_maintenance_status(router(), INDEX_ID)
            .expect("status"),
        VectorMaintenanceState::Idle
    );
}

#[test]
fn maintenance_step_drives_required_rebuild_to_awaiting_publish_then_publishes() {
    let store = fresh_store();
    seed_live_and_tombstones(&store, 4, 4); // 50% tombstones -> RebuildRequired

    // Scan exhausts, then the recommendation starts a rebuild at the target nlist.
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("scan"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("recommend"),
        VectorMaintenanceStepResult::RebuildStarted(
            VectorMaintenanceRecommendation::RebuildRequired
        )
    );
    // Starting the rebuild clears the scan state (the rebuild state machine now drives).
    assert_eq!(
        store
            .admin_vector_maintenance_status(router(), INDEX_ID)
            .expect("status"),
        VectorMaintenanceState::Idle
    );

    // Each step drives one bounded rebuild unit until it stops at ReadyToPublish (publish is explicit).
    let mut awaiting = false;
    for _ in 0..100_000 {
        match store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("rebuild step")
        {
            VectorMaintenanceStepResult::RebuildAdvanced(_) => continue,
            VectorMaintenanceStepResult::AwaitingPublish(status) => {
                assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
                awaiting = true;
                break;
            }
            other => panic!("unexpected step result: {other:?}"),
        }
    }
    assert!(awaiting, "rebuild reached ReadyToPublish");

    // The step must never auto-publish: another step still reports AwaitingPublish.
    assert!(matches!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("still awaiting"),
        VectorMaintenanceStepResult::AwaitingPublish(_)
    ));
    assert_eq!(
        store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase,
        VectorRebuildPhase::ReadyToPublish,
        "no auto-publish"
    );

    // Explicit publish flips the active generation; subsequent steps drive cleanup to Idle.
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
    let def = store.def_for_test(INDEX_ID).expect("def");
    assert_eq!(def.active_index_version, TARGET_V);
    assert_eq!(def.nlist, 2);

    let mut cleaned = false;
    for _ in 0..100_000 {
        let result = store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("cleanup step");
        if store
            .admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase
            == VectorRebuildPhase::Idle
        {
            // Once the rebuild is fully torn down, the step either finished cleanup or began a new scan.
            assert!(matches!(
                result,
                VectorMaintenanceStepResult::CleanupAdvanced(_)
                    | VectorMaintenanceStepResult::Scanning { .. }
            ));
            cleaned = true;
            break;
        }
        assert!(matches!(
            result,
            VectorMaintenanceStepResult::CleanupAdvanced(_)
        ));
    }
    assert!(cleaned, "cleanup drained to Idle");
}

#[test]
fn maintenance_step_fails_closed_then_recovers_via_reset() {
    let store = fresh_store();
    seed_live_and_tombstones(&store, 4, 4); // 50% tombstones, degenerate nlist = 1

    // No explicit target on a degenerate (nlist=1) index: the rebuild start rejects nlist < 2.
    let req = VectorMaintenanceStepRequest {
        target_nlist: None,
        ..maint_req()
    };
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, req)
            .expect("scan"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );
    match store
        .admin_vector_maintenance_step(router(), INDEX_ID, req)
        .expect("failing recommend")
    {
        VectorMaintenanceStepResult::Failed(failure) => {
            assert_eq!(failure.code, VectorCanisterError::InvalidRebuildParams);
            assert!(!failure.message.is_empty());
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(matches!(
        store
            .admin_vector_maintenance_status(router(), INDEX_ID)
            .expect("status"),
        VectorMaintenanceState::Failed(_)
    ));

    // A failed state is a no-op until an explicit reset.
    assert!(matches!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, req)
            .expect("no-op"),
        VectorMaintenanceStepResult::Failed(_)
    ));

    store
        .admin_vector_maintenance_reset(router(), INDEX_ID)
        .expect("reset");
    assert_eq!(
        store
            .admin_vector_maintenance_status(router(), INDEX_ID)
            .expect("status"),
        VectorMaintenanceState::Idle
    );
    // Maintenance resumes after reset (with a valid target this time).
    assert!(matches!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("resume"),
        VectorMaintenanceStepResult::Scanning { .. }
    ));
}

#[test]
fn maintenance_scan_restarts_on_stale_cursor_after_version_flip() {
    let store = fresh_store();
    // 1 slot/page so 4 rows span 4 pages, forcing a multi-step (non-exhausting) scan.
    store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 80)
        .expect("create");
    seed_distinct(&store, 4);

    // One bounded page (scan_max_pages = 1): the scan does not exhaust and persists a Some cursor.
    let req = VectorMaintenanceStepRequest {
        scan_max_pages: 1,
        ..maint_req()
    };
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, req)
            .expect("scan 1"),
        VectorMaintenanceStepResult::Scanning { exhausted: false }
    );

    // The active version flips: the persisted cursor is now scoped to a stale generation.
    set_active_version(INDEX_ID, 2);

    // The next scan step sees InvalidStatsCursor and restarts cleanly from the lower bound.
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, req)
            .expect("restart"),
        VectorMaintenanceStepResult::Scanning { exhausted: false }
    );
    match store
        .admin_vector_maintenance_status(router(), INDEX_ID)
        .expect("status")
    {
        VectorMaintenanceState::Scanning {
            cursor,
            exhausted,
            merged,
        } => {
            assert!(cursor.is_none(), "restarted scan has no cursor");
            assert!(!exhausted);
            assert_eq!(merged.index_version, 0, "merged counters reset on restart");
        }
        other => panic!("expected Scanning, got {other:?}"),
    }
}

#[test]
fn maintenance_exhausted_scan_restarts_on_version_flip_before_recommending() {
    let store = fresh_store();
    seed_distinct(&store, 4); // single degenerate page -> scan exhausts in one step

    // Drive the scan to exhausted (recommendation would happen on the next step).
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("scan"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );

    // The active version flips after exhaustion (no cursor remains to scope-check).
    set_active_version(INDEX_ID, 2);

    // The generation guard at the exhausted->recommend boundary catches the flip and restarts the
    // scan instead of recommending against the stale merged page health.
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("restart"),
        VectorMaintenanceStepResult::Scanning { exhausted: false }
    );
    match store
        .admin_vector_maintenance_status(router(), INDEX_ID)
        .expect("status")
    {
        VectorMaintenanceState::Scanning {
            cursor,
            exhausted,
            merged,
        } => {
            assert!(cursor.is_none());
            assert!(!exhausted, "exhausted flag is distinct from cursor == None");
            assert_eq!(merged.index_version, 0);
        }
        other => panic!("expected Scanning, got {other:?}"),
    }

    // A freshly restarted scan (cursor=None, exhausted=false) performs a scan step, not a
    // recommendation, even though it could otherwise immediately judge an (empty) new version.
    assert!(matches!(
        store
            .admin_vector_maintenance_step(router(), INDEX_ID, maint_req())
            .expect("scan again"),
        VectorMaintenanceStepResult::Scanning { .. }
    ));
}

#[test]
fn maintenance_endpoints_reject_non_router_and_unknown_index() {
    let store = fresh_store();
    seed_distinct(&store, 2);
    assert_eq!(
        store
            .admin_vector_maintenance_step(shard_canister(), INDEX_ID, maint_req())
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        store
            .admin_vector_maintenance_step(router(), 999, maint_req())
            .unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
    assert_eq!(
        store
            .admin_vector_maintenance_status(shard_canister(), INDEX_ID)
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        store
            .admin_vector_maintenance_reset(shard_canister(), INDEX_ID)
            .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

// --- ADR 0034 Slice 6: candidate-restricted vector search tests ---
#[test]
fn candidate_search_accepts_vertex_only_subjects() {
    let store = fresh_store();
    let mut req = search_value(0.0, 10);
    // VectorSubject currently only has the Vertex variant; this smoke test confirms the typed
    // contract is accepted. When a non-vertex variant is added, add an explicit rejection test.
    req.candidate_subjects = Some(vec![VectorSubject::Vertex {
        shard_id: ShardId::new(0),
        vertex_id: 0,
    }]);
    let result = store.vector_search(&req).expect("vertex-only is accepted");
    assert!(result.hits.is_empty());
}

#[test]
fn candidate_search_validates_shape_before_physical_def() {
    let store = fresh_store();
    // No upsert, so there is no physical def for INDEX_ID. An oversized allowlist must still fail.
    let mut req = search_value(0.0, 10);
    let too_many: Vec<VectorSubject> = (0..MAX_VECTOR_SEARCH_FILTER_CANDIDATES as u32 + 1)
        .map(|i| VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: i,
        })
        .collect();
    req.candidate_subjects = Some(too_many);
    let err = store
        .vector_search(&req)
        .expect_err("oversized on empty index");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));

    // Duplicate candidates on an empty index also fail.
    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![subject(7), subject(7)]);
    let err = store
        .vector_search(&req)
        .expect_err("duplicate on empty index");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));
}

#[test]
fn candidate_search_restricts_top_k_to_allowlist() {
    let store = fresh_store();
    // Three vectors at 0.0, 1.0, 2.0. Query at 0.0, top_k=2.
    // Unrestricted would return vertices 7 (distance 0) and 8 (distance 1).
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 0.0))
        .expect("upsert 7");
    store
        .vector_upsert(shard_canister(), &upsert_vec(8, 1, 1.0))
        .expect("upsert 8");
    store
        .vector_upsert(shard_canister(), &upsert_vec(9, 1, 2.0))
        .expect("upsert 9");

    let mut req = search_value(0.0, 2);
    req.candidate_subjects = Some(vec![subject(8), subject(9)]);
    let result = store.vector_search(&req).expect("candidate search");
    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].subject, subject(8));
    assert_eq!(result.hits[1].subject, subject(9));
    // Vertex 7 is nearer but outside the allowlist.
    assert!(!result.hits.iter().any(|h| h.subject == subject(7)));
}

#[test]
fn candidate_search_empty_allowlist_returns_no_hits() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .expect("upsert");
    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![]);
    let result = store.vector_search(&req).expect("empty candidate search");
    assert!(result.hits.is_empty());
}

#[test]
fn candidate_search_skips_absent_and_deleted_subjects() {
    let store = fresh_store();
    // Live subject 7.
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .expect("upsert 7");
    // Absent subject 8 is not in the index.
    // Deleted subject 9.
    store
        .vector_upsert(shard_canister(), &upsert_vec(9, 1, 2.0))
        .expect("upsert 9");
    store
        .vector_remove(shard_canister(), &remove_op(9, 2))
        .expect("remove 9");

    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![subject(7), subject(8), subject(9)]);
    let result = store.vector_search(&req).expect("candidate search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn candidate_search_preserves_none_as_unrestricted_path() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 0.0))
        .expect("upsert 7");
    store
        .vector_upsert(shard_canister(), &upsert_vec(8, 1, 1.0))
        .expect("upsert 8");

    let req = search_value(0.0, 10);
    assert!(req.candidate_subjects.is_none());
    let result = store.vector_search(&req).expect("unrestricted search");
    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn candidate_scan_page_major_matches_exact_scan_across_pages() {
    let store = fresh_store();
    // d = 4 F32: pad stride 16, meta 4. A small page budget forces 2 rows per page, so five rows
    // span three pages ([0,1], [2,3], [4]) and the candidate scan must bulk-read every page.
    store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 80)
        .expect("create");
    assert_eq!(store.def_for_test(INDEX_ID).unwrap().slots_per_page, 2);

    for (v, value) in [(0u32, 0.0f32), (1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)] {
        store
            .vector_upsert(shard_canister(), &upsert_vec(v, 1, value))
            .expect("upsert");
    }

    // Query at 4.0 puts vertex 4 (the last row, last page) in the top-3, so a page-major scan that
    // drops or misreads a later page would diverge from the exact (read_row_bytes) scan.
    let allowlist: Vec<VectorSubject> = (0..5).map(subject).collect();
    let mut req = search_value(4.0, 3);
    req.candidate_subjects = Some(allowlist);
    let batched = store.vector_search(&req).expect("batched candidate scan");
    let exact = store
        .vector_search(&search_value(4.0, 3))
        .expect("exact scan");
    assert_eq!(
        batched.hits, exact.hits,
        "page-major candidate scan must equal the exact read_row_bytes scan across pages"
    );
    assert_eq!(
        batched.hits[0].subject,
        subject(4),
        "last-page vertex is nearest"
    );
    assert_eq!(batched.hits[1].subject, subject(3));
    assert_eq!(batched.hits[2].subject, subject(2));
}

#[test]
fn candidate_scan_with_membership_matches_resolve_based() {
    let store = fresh_store();
    // Distinct subjects 1..8 (values 0.0..7.0); a large allowlist (>= live/2) is the scan-with-
    // membership regime, and it must produce the same top-k as the resolve-based path.
    seed_distinct(&store, 8);
    let allowlist: Vec<VectorSubject> = (0..8).map(|v| subject(v + 1)).collect();
    let query = search_value(4.0, 5);
    let qv = super::search::decode_f32(&query.query);
    let resolve = store
        .candidate_subject_scan(
            INDEX_ID,
            1,
            &qv,
            VectorMetric::L2Squared,
            VectorEncoding::F32,
            &allowlist,
            5,
            0.0,
        )
        .expect("resolve-based");
    let membership = super::search::candidate_scan_with_membership(
        INDEX_ID,
        1,
        1,
        &qv,
        VectorMetric::L2Squared,
        VectorEncoding::F32,
        0.0,
        &allowlist,
        5,
    );
    assert_eq!(
        resolve.hits, membership.hits,
        "scan-with-membership must match the resolve-based candidate scan"
    );
}

#[test]
fn candidate_search_rejects_oversized_allowlist() {
    let store = fresh_store();
    let mut req = search_value(0.0, 10);
    let too_many: Vec<VectorSubject> = (0..MAX_VECTOR_SEARCH_FILTER_CANDIDATES as u32 + 1)
        .map(|i| VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: i,
        })
        .collect();
    req.candidate_subjects = Some(too_many);
    let err = store.vector_search(&req).expect_err("oversized allowlist");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));
}

#[test]
fn candidate_search_rejects_duplicate_subjects() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .expect("upsert");
    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![subject(7), subject(7)]);
    let err = store.vector_search(&req).expect_err("duplicate candidates");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));
}

/// I8 scalar-quantization tests (B1+A1: per-row scale, F32 wire query; `VectorEncoding::I8`).
mod i8_tests {
    use super::*;
    use crate::facade::stable::{PAGE_STORE, VECTOR_INDEX_DEFS};
    use crate::records::SlotRef;

    /// A distinct index id so an `I8` def is created without colliding with the F32 `INDEX_ID` fixtures.
    const I8_INDEX: u32 = 7;

    fn i8_bytes(values: &[f32]) -> Vec<u8> {
        assert_eq!(values.len(), DIMS as usize, "component count mismatch");
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn i8_upsert(
        store: &VectorCanisterStore,
        index_id: u32,
        vertex_id: u32,
        stamp: u64,
        values: &[f32],
        metric: VectorMetric,
    ) {
        store
            .vector_upsert(
                shard_canister(),
                &VectorEmbeddingSyncOp {
                    index_id,
                    embedding_name_id: 0,
                    subject: subject(vertex_id),
                    mutation_id: stamp,
                    encoding: VectorEncoding::I8,
                    dims: DIMS,
                    metric,
                    bytes: i8_bytes(values),
                    remove: false,
                },
            )
            .expect("i8 upsert");
    }

    fn i8_search(
        store: &VectorCanisterStore,
        index_id: u32,
        values: &[f32],
        metric: VectorMetric,
        top_k: u32,
    ) -> Vec<u32> {
        let res = store
            .vector_search(&VectorSearchRequest {
                index_id,
                query: i8_bytes(values),
                encoding: VectorEncoding::I8,
                dims: DIMS,
                metric,
                top_k,
                candidate_subjects: None,
            })
            .expect("i8 search");
        res.hits
            .iter()
            .map(|h| match h.subject {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect()
    }

    fn row_payload_aux(
        store: &VectorCanisterStore,
        index_id: u32,
        slot: SlotRef,
    ) -> (Vec<u8>, [u8; 8]) {
        let _ = store;
        PAGE_STORE
            .with_borrow(|s| s.read_row_bytes(index_id, slot))
            .map(|(_, bytes, aux)| (bytes, aux))
            .expect("row present")
    }

    #[test]
    fn i8_ingest_and_search_parity_with_f32() {
        let store = fresh_store();
        let vectors: Vec<Vec<f32>> = vec![
            vec![1.0, 2.0, 3.0, 4.0],
            vec![4.0, 3.0, 2.0, 1.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![2.0, 2.0, 2.0, 0.0],
        ];
        for (i, v) in vectors.iter().enumerate() {
            store
                .vector_upsert(
                    shard_canister(),
                    &upsert_vec_from((i + 1) as u32, 1, v, VectorMetric::L2Squared),
                )
                .unwrap();
            i8_upsert(
                &store,
                I8_INDEX,
                (i + 1) as u32,
                1,
                v,
                VectorMetric::L2Squared,
            );
        }
        let q = [2.0f32, 2.0, 2.0, 2.0];
        let f32_top = store
            .vector_search(&VectorSearchRequest {
                index_id: INDEX_ID,
                query: i8_bytes(&q),
                encoding: VectorEncoding::F32,
                dims: DIMS,
                metric: VectorMetric::L2Squared,
                top_k: 4,
                candidate_subjects: None,
            })
            .unwrap();
        let f32_order: Vec<u32> = f32_top
            .hits
            .iter()
            .map(|h| match h.subject {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect();
        let i8_order = i8_search(&store, I8_INDEX, &q, VectorMetric::L2Squared, 4);
        assert_eq!(f32_order, i8_order, "I8 top-k ordering matches F32");
    }

    #[test]
    fn i8_def_uses_meta8_and_consistent_slots_per_page() {
        let store = fresh_store();
        i8_upsert(
            &store,
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        let def = VECTOR_INDEX_DEFS.with_borrow(|d| d.get(&I8_INDEX)).unwrap();
        assert_eq!(def.encoding, VectorEncoding::I8);
        // I8 stores `dims` payload bytes plus a 4-byte scale in row-meta aux (meta stride 8).
        assert_eq!(def.stride_bytes, DIMS as u32);
        assert_eq!(def.meta_stride_bytes, 8);
        assert_eq!(def.pad_stride_bytes, 16);
        // `slots_per_page` is derived from `meta 8`; the page must fit at least one row.
        assert!(def.slots_per_page >= 1);
        // The I8 page meta reopens with `meta_stride 8` (encoding-agnostic open validation).
        let page_meta = PAGE_STORE
            .with_borrow(|s| s.page_meta_for_test(I8_INDEX, 1, 0, 0))
            .expect("i8 page meta");
        assert_eq!(page_meta.meta_stride, 8);
    }

    #[test]
    fn i8_zero_l2_vector_is_accepted_and_nearest() {
        let store = fresh_store();
        i8_upsert(
            &store,
            I8_INDEX,
            1,
            1,
            &[0.0, 0.0, 0.0, 0.0],
            VectorMetric::L2Squared,
        );
        let top = i8_search(
            &store,
            I8_INDEX,
            &[1.0, 1.0, 1.0, 1.0],
            VectorMetric::L2Squared,
            1,
        );
        assert_eq!(top, vec![1], "zero L2 I8 vector is nearest");
    }

    #[test]
    fn i8_ingest_rejects_wrong_wire_width() {
        let store = fresh_store();
        i8_upsert(
            &store,
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        let err = store
            .vector_upsert(
                shard_canister(),
                &VectorEmbeddingSyncOp {
                    index_id: I8_INDEX,
                    embedding_name_id: 0,
                    subject: subject(2),
                    mutation_id: 1,
                    encoding: VectorEncoding::I8,
                    dims: DIMS,
                    metric: VectorMetric::L2Squared,
                    bytes: vec![0u8; 3],
                    remove: false,
                },
            )
            .unwrap_err();
        assert_eq!(err, VectorCanisterError::ByteWidthMismatch);
    }

    #[test]
    fn i8_idempotency_noop_then_conflict_on_different_payload() {
        let store = fresh_store();
        i8_upsert(
            &store,
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        // Byte-identical replay at the same stamp: no-op (no MutationStampConflict).
        i8_upsert(
            &store,
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        // Same stamp, different payload: conflict.
        let err = store
            .vector_upsert(
                shard_canister(),
                &VectorEmbeddingSyncOp {
                    index_id: I8_INDEX,
                    embedding_name_id: 0,
                    subject: subject(1),
                    mutation_id: 1,
                    encoding: VectorEncoding::I8,
                    dims: DIMS,
                    metric: VectorMetric::L2Squared,
                    bytes: i8_bytes(&[5.0, 6.0, 7.0, 8.0]),
                    remove: false,
                },
            )
            .unwrap_err();
        assert_eq!(err, VectorCanisterError::MutationStampConflict);
    }

    #[test]
    fn i8_rebuild_building_carries_bytes_and_scale() {
        let store = fresh_store();
        for (v, vals) in [
            (1u32, [1.0f32, 2.0, 3.0, 4.0]),
            (2, [4.0, 3.0, 2.0, 1.0]),
            (3, [0.0, 1.0, 2.0, 3.0]),
            (4, [3.0, 2.0, 1.0, 0.0]),
        ] {
            i8_upsert(&store, I8_INDEX, v, 1, &vals, VectorMetric::L2Squared);
        }
        store
            .admin_start_vector_rebuild(router(), I8_INDEX, 2, 100)
            .expect("start");
        let status = drive_steps(&store, I8_INDEX);
        assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
        // Every live subject's shadow row must carry the SAME (bytes, scale) as its active row: a
        // double-quantize or a scale recompute would make these differ.
        for v in 1..=4u32 {
            let entry = store.subject_entry_for_test(I8_INDEX, subject(v)).unwrap();
            let active = entry.slot.expect("active slot");
            let shadow = entry.shadow_slot.expect("shadow slot");
            let (active_bytes, active_aux) = row_payload_aux(&store, I8_INDEX, active);
            let (shadow_bytes, shadow_aux) = row_payload_aux(&store, I8_INDEX, shadow);
            assert_eq!(
                active_bytes, shadow_bytes,
                "I8 bytes carried forward (no double-quantize)"
            );
            assert_eq!(
                active_aux, shadow_aux,
                "I8 scale carried forward (not recomputed)"
            );
        }
    }

    // -------------------------------------------------------------------------------------------
    // I8 vs F32 recall measurement. Adoption of I8 is an operator decision; this reports the
    // quantization recall@k (top-k subject overlap against the F32 exact-scan ground truth) on
    // representative synthetic distributions, and keeps a conservative floor as a regression guard.
    // -------------------------------------------------------------------------------------------

    /// Deterministic xorshift64 PRNG so the recall measurement is reproducible.
    struct XorShift64(u64);

    impl XorShift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn unit(&mut self) -> f32 {
            (self.next() as f32) / (u64::MAX as f32)
        }

        /// Standard-normal sample via Box–Muller.
        fn gauss(&mut self) -> f32 {
            let u1 = self.unit().max(f32::MIN_POSITIVE);
            let u2 = self.unit();
            (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
        }
    }

    fn f32_upsert(
        store: &VectorCanisterStore,
        index_id: u32,
        vertex_id: u32,
        values: &[f32],
        metric: VectorMetric,
    ) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        store
            .vector_upsert(
                shard_canister(),
                &VectorEmbeddingSyncOp {
                    index_id,
                    embedding_name_id: 0,
                    subject: subject(vertex_id),
                    mutation_id: 1,
                    encoding: VectorEncoding::F32,
                    dims: values.len() as u16,
                    metric,
                    bytes,
                    remove: false,
                },
            )
            .expect("f32 upsert");
    }

    fn f32_topk(
        store: &VectorCanisterStore,
        index_id: u32,
        values: &[f32],
        metric: VectorMetric,
        top_k: u32,
    ) -> Vec<u32> {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let res = store
            .vector_search(&VectorSearchRequest {
                index_id,
                query: bytes,
                encoding: VectorEncoding::F32,
                dims: values.len() as u16,
                metric,
                top_k,
                candidate_subjects: None,
            })
            .expect("f32 search");
        res.hits
            .iter()
            .map(|h| match h.subject {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect()
    }

    fn i8_upsert_d(
        store: &VectorCanisterStore,
        index_id: u32,
        vertex_id: u32,
        values: &[f32],
        metric: VectorMetric,
    ) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        store
            .vector_upsert(
                shard_canister(),
                &VectorEmbeddingSyncOp {
                    index_id,
                    embedding_name_id: 0,
                    subject: subject(vertex_id),
                    mutation_id: 1,
                    encoding: VectorEncoding::I8,
                    dims: values.len() as u16,
                    metric,
                    bytes,
                    remove: false,
                },
            )
            .expect("i8 upsert");
    }

    fn i8_search_d(
        store: &VectorCanisterStore,
        index_id: u32,
        values: &[f32],
        metric: VectorMetric,
        top_k: u32,
    ) -> Vec<u32> {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let res = store
            .vector_search(&VectorSearchRequest {
                index_id,
                query: bytes,
                encoding: VectorEncoding::I8,
                dims: values.len() as u16,
                metric,
                top_k,
                candidate_subjects: None,
            })
            .expect("i8 search");
        res.hits
            .iter()
            .map(|h| match h.subject {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect()
    }

    fn overlap(a: &[u32], b: &[u32]) -> usize {
        let set: std::collections::HashSet<u32> = b.iter().copied().collect();
        a.iter().filter(|x| set.contains(x)).count()
    }

    /// Reports I8 vs F32 recall@k and asserts a conservative floor (regression guard; adoption of
    /// I8 is the operator's decision, informed by the reported number).
    fn run_recall(
        store: &VectorCanisterStore,
        dims: u16,
        rng_seed: u64,
        metric: VectorMetric,
    ) -> (f32, f32) {
        let n = 512u32;
        let queries = 32u32;
        let mut rng = XorShift64(rng_seed);
        for v in 0..n {
            let g: Vec<f32> = (0..dims).map(|_| rng.gauss()).collect();
            let vals: Vec<f32> = if metric == VectorMetric::Cosine {
                let norm = g.iter().map(|x| x * x).sum::<f32>().sqrt();
                g.iter().map(|x| x / norm).collect()
            } else {
                g
            };
            f32_upsert(store, INDEX_ID, v, &vals, metric);
            i8_upsert_d(store, I8_INDEX, v, &vals, metric);
        }
        let (mut r10, mut r100) = (0.0f32, 0.0f32);
        for _ in 0..queries {
            let g: Vec<f32> = (0..dims).map(|_| rng.gauss()).collect();
            let q: Vec<f32> = if metric == VectorMetric::Cosine {
                let norm = g.iter().map(|x| x * x).sum::<f32>().sqrt();
                g.iter().map(|x| x / norm).collect()
            } else {
                g
            };
            let f = f32_topk(store, INDEX_ID, &q, metric, 100);
            let i = i8_search_d(store, I8_INDEX, &q, metric, 100);
            r10 += overlap(&f[..10], &i[..10]) as f32 / 10.0;
            r100 += overlap(&f, &i) as f32 / 100.0;
        }
        (r10 / queries as f32, r100 / queries as f32)
    }

    #[test]
    fn i8_recall_vs_f32_gaussian_l2() {
        let store = fresh_store();
        let (r10, r100) = run_recall(&store, 256, 0xDEAD_BEEF, VectorMetric::L2Squared);
        eprintln!("I8 recall L2 gaussian d=256: recall@10={r10:.4} recall@100={r100:.4}");
        assert!(
            r10 >= 0.90 && r100 >= 0.90,
            "I8 L2 recall too low: @10={r10} @100={r100}"
        );
    }

    #[test]
    fn i8_recall_vs_f32_unit_sphere_cosine() {
        let store = fresh_store();
        let (r10, r100) = run_recall(&store, 256, 0xFEED_FACE, VectorMetric::Cosine);
        eprintln!("I8 recall cosine unit-sphere d=256: recall@10={r10:.4} recall@100={r100:.4}");
        assert!(
            r10 >= 0.90 && r100 >= 0.90,
            "I8 cosine recall too low: @10={r10} @100={r100}"
        );
    }
}
