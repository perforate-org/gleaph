//! Unit tests for the degenerate `ivf_flat` mutation store (ADR 0031 Slice 2).

use super::VectorIndexStore;
use crate::init::VectorIndexInitArgs;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_TOP_K, VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorMetric,
    VectorSearchRequest, VectorSubject,
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
fn fresh_store() -> VectorIndexStore {
    let store = VectorIndexStore::new();
    store
        .init_from_args(&VectorIndexInitArgs {
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

/// Upsert at an explicit `(incarnation, version)` clock (ADR 0031 Slice 4).
fn upsert_op_inc(
    vertex_id: u32,
    embedding_incarnation: u64,
    embedding_version: u64,
    fill: u8,
) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        embedding_incarnation,
        embedding_version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        bytes: vec![fill; STRIDE],
        remove: false,
    }
}

/// Remove at an explicit `(incarnation, version)` clock (ADR 0031 Slice 4).
fn remove_op_inc(
    vertex_id: u32,
    embedding_incarnation: u64,
    embedding_version: u64,
) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        embedding_incarnation,
        embedding_version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        bytes: Vec::new(),
        remove: true,
    }
}

/// First-incarnation upsert (the common single-incarnation case).
fn upsert_op(vertex_id: u32, embedding_version: u64, fill: u8) -> VectorEmbeddingSyncOp {
    upsert_op_inc(vertex_id, 1, embedding_version, fill)
}

/// First-incarnation remove (the common single-incarnation case).
fn remove_op(vertex_id: u32, embedding_version: u64) -> VectorEmbeddingSyncOp {
    remove_op_inc(vertex_id, 1, embedding_version)
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
    assert_eq!(def.next_vector_id, 2, "one id allocated, next is 2");

    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .expect("clock");
    assert!(!entry.deleted);
    assert_eq!(entry.stored_embedding_version, 1);
    assert_eq!(entry.vector_id, Some(1));
    let slot = entry.slot.expect("live slot");
    assert_eq!(slot.generation, 1);
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 1), Some(slot));
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
    let def = store.def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.next_vector_id, 2, "no new id allocated");
    let head = store.partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 1);
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
    assert_eq!(err, VectorIndexError::EmbeddingVersionConflict);
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
    assert_eq!(entry.stored_embedding_version, 5);
}

#[test]
fn upsert_newer_version_live_appends_and_tombstones_reusing_id() {
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
    assert_eq!(entry.stored_embedding_version, 2);
    assert_eq!(entry.vector_id, Some(1), "same live vector_id reused");
    let new_slot = entry.slot.unwrap();
    assert_eq!(new_slot.generation, 2, "generation bumped on new slot");
    assert_ne!(new_slot.slot, old_slot.slot);
    // id→slot points at the new slot.
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 1), Some(new_slot));
    // No new VectorId allocated (next stays 2).
    assert_eq!(store.def_for_test(INDEX_ID).unwrap().next_vector_id, 2);
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
    assert_eq!(entry.stored_embedding_version, 2);
    assert_eq!(entry.slot, None);
    assert_eq!(entry.vector_id, None);
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 1), None);
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
    assert_eq!(entry.stored_embedding_version, 1);
    assert_eq!(entry.vector_id, None);
}

#[test]
fn same_incarnation_upsert_to_deleted_subject_is_noop() {
    // Under incarnation fencing, an upsert at the *same* incarnation as a tombstone is a stale
    // replay: a genuine reinsert carries a strictly greater incarnation. So it must NOT resurrect.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, u64::MAX))
        .unwrap();
    // Stale same-incarnation upsert (e.g. a journaled replay) lands behind the tombstone clock.
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .expect("stale replay no-op");

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted, "same-incarnation upsert cannot resurrect");
    assert_eq!(entry.vector_id, None);
}

#[test]
fn newer_incarnation_upsert_resurrects_with_fresh_id() {
    // Resurrection requires a strictly greater incarnation, mirroring the canonical store bumping
    // the incarnation on each delete/reinsert. The fresh incarnation lands a brand-new VectorId.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, u64::MAX))
        .unwrap();
    // Reinsert at incarnation 2, version 1 (canonical version reset): resurrects.
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 2, 1, 0xBB))
        .unwrap();

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "newer-incarnation upsert resurrects");
    assert_eq!(entry.embedding_incarnation, 2);
    assert_eq!(entry.stored_embedding_version, 1);
    let new_id = entry.vector_id.expect("resurrected entry has a vector_id");
    assert_eq!(new_id, 2, "fresh VectorId allocated; old id retired");
    assert!(store.id_to_slot_for_test(INDEX_ID, new_id).is_some());
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 1), None);
}

#[test]
fn newer_incarnation_upsert_after_missing_remove_clock_resurrects() {
    // A remove on a never-inserted subject writes a tombstone clock at its incarnation; only a
    // strictly newer incarnation resurrects (a same-incarnation replay stays a no-op).
    let store = fresh_store();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, 5))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .expect("same-incarnation replay no-op");
    assert!(
        store
            .subject_entry_for_test(INDEX_ID, subject(7))
            .unwrap()
            .deleted
    );
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 2, 1, 0xAA))
        .unwrap();
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "newer incarnation resurrects after a clock");
    assert_eq!(entry.embedding_incarnation, 2);
    assert!(entry.vector_id.is_some());
}

#[test]
fn reinsert_after_delete_allocates_fresh_vector_id() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .unwrap();
    let first_id = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .vector_id
        .unwrap();
    assert_eq!(first_id, 1);

    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, 2))
        .unwrap();
    // The canonical reinsert bumps the incarnation to 2.
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 2, 1, 0xCC))
        .unwrap();

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted);
    let new_id = entry.vector_id.unwrap();
    assert_ne!(new_id, first_id, "old VectorId is retired, not reused");
    assert_eq!(new_id, 2);
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, first_id), None);
    assert!(store.id_to_slot_for_test(INDEX_ID, new_id).is_some());
}

#[test]
fn stale_older_incarnation_remove_cannot_tombstone_newer_live() {
    // The reverse-orphan race: a late repair-drain remove for the *deleted* incarnation arrives
    // after a newer reinsert already advanced the clock. The incarnation fence makes it a no-op.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, u64::MAX))
        .unwrap();
    // Reinsert at incarnation 2 (live again, fresh id).
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 2, 1, 0xBB))
        .unwrap();
    let live_id = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .vector_id
        .unwrap();

    // Late blind remove for the OLD incarnation with the authoritative max version: must no-op.
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, u64::MAX))
        .expect("stale older-incarnation remove is fenced");

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(
        !entry.deleted,
        "newer live incarnation survives a stale remove"
    );
    assert_eq!(entry.embedding_incarnation, 2);
    assert_eq!(entry.vector_id, Some(live_id));
    assert!(store.id_to_slot_for_test(INDEX_ID, live_id).is_some());
}

#[test]
fn newer_incarnation_remove_on_live_tombstones() {
    // A remove for a strictly newer incarnation than the live clock authoritatively tombstones the
    // live slot (e.g. the upsert for that incarnation never arrived).
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op_inc(7, 1, 1, 0xAA))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 2, u64::MAX))
        .unwrap();
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted);
    assert_eq!(entry.embedding_incarnation, 2);
    assert_eq!(entry.slot, None);
    assert_eq!(entry.vector_id, None);
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 1), None);
}

#[test]
fn page_capacity_rolls_to_new_page_at_slots_per_page() {
    let store = fresh_store();
    // header(64) + 2 slots * stride(16) = 96 bytes budget yields slots_per_page = 2.
    store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 64 + 2 * STRIDE as u32)
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
    // budget below header + one stride yields slots_per_page < 1.
    let err = store
        .create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 64 + 8)
        .expect_err("reject");
    assert_eq!(err, VectorIndexError::InvalidPageCapacity);
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
        VectorIndexError::DimensionMismatch
    );

    let mut wrong_bytes = upsert_op(9, 1, 0xAA);
    wrong_bytes.bytes = vec![0u8; STRIDE - 1];
    assert_eq!(
        store
            .vector_upsert(shard_canister(), &wrong_bytes)
            .unwrap_err(),
        VectorIndexError::ByteWidthMismatch
    );
}

#[test]
fn vector_upsert_rejects_remove_flag() {
    let store = fresh_store();
    let mut op = upsert_op(7, 1, 0xAA);
    op.remove = true;
    assert_eq!(
        store.vector_upsert(shard_canister(), &op).unwrap_err(),
        VectorIndexError::MutationKindMismatch
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
        VectorIndexError::MutationKindMismatch
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
        VectorIndexError::ShardNotAttached
    );

    // Caller attached to shard 0 but op targets shard 1.
    let mut cross = upsert_op(7, 1, 0xAA);
    cross.subject = VectorSubject::Vertex {
        shard_id: ShardId::new(1),
        vertex_id: 7,
    };
    assert_eq!(
        store.vector_upsert(shard_canister(), &cross).unwrap_err(),
        VectorIndexError::ShardMismatch
    );
}

#[test]
fn init_rejects_anonymous_router() {
    let store = VectorIndexStore::new();
    let err = store
        .init_from_args(&VectorIndexInitArgs {
            router_canister: Principal::anonymous(),
        })
        .expect_err("anonymous router rejected");
    assert_eq!(err, VectorIndexError::AnonymousRouter);
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
        VectorIndexError::InvalidPrincipalInRegistry
    );
}

#[test]
fn single_target_owns_all_shards_of_one_graph() {
    let store = VectorIndexStore::new();
    store
        .init_from_args(&VectorIndexInitArgs {
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
        VectorIndexError::GraphOwnershipMismatch
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
        VectorIndexError::Unauthorized
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
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 1), None);
    assert_eq!(store.id_to_slot_for_test(INDEX_ID, 2), None);
}

#[test]
fn durable_allocators_persist_across_store_handles() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(8, 1, 0xBB))
        .unwrap();

    // A fresh stateless handle reads the same durable stable state ("reopen").
    let reopened = VectorIndexStore::new();
    let def = reopened.def_for_test(INDEX_ID).unwrap();
    assert_eq!(
        def.next_vector_id, 3,
        "two ids allocated, monotonic across handles"
    );
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

fn upsert_vec_inc(
    vertex_id: u32,
    incarnation: u64,
    version: u64,
    value: f32,
) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        embedding_incarnation: incarnation,
        embedding_version: version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        bytes: vec_bytes(value),
        remove: false,
    }
}

fn upsert_vec(vertex_id: u32, version: u64, value: f32) -> VectorEmbeddingSyncOp {
    upsert_vec_inc(vertex_id, 1, version, value)
}

fn search_value(value: f32, top_k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(value),
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        top_k,
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
    assert_eq!(hit.embedding_incarnation, 1);
    assert_eq!(hit.embedding_version, 1);
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
    assert_eq!(result.hits[0].embedding_version, 2);
    // The superseded (tombstoned) generation's value 1.0 is never scored.
    let stale = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(stale.hits.len(), 1);
    assert!(stale.hits[0].distance > 0.0);
}

#[test]
fn search_reinsert_after_delete_returns_newer_incarnation_only() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec_inc(7, 1, 1, 1.0))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, 2))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_vec_inc(7, 2, 1, 9.0))
        .unwrap();
    let result = store.vector_search(&search_value(9.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].distance, 0.0);
    assert_eq!(result.hits[0].embedding_incarnation, 2);
    assert_eq!(result.hits[0].embedding_version, 1);
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
                embedding_incarnation: 1,
                embedding_version: 1,
                encoding: VectorEncoding::F32,
                dims: DIMS,
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
fn search_skips_live_entry_with_missing_vector_id() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{SubjectKey, SubjectMapEntry};

    let store = fresh_store();
    // Seed a valid live vector so the def, a page row, and a real slot all exist.
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    let entry = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .expect("live entry");
    assert!(entry.slot.is_some() && entry.vector_id.is_some());

    // Corrupt the entry into inconsistent drift: still live (slot Some, not deleted) but with no
    // vector_id. The freshness guard must skip it rather than score a row it cannot verify.
    let drifted = SubjectMapEntry {
        vector_id: None,
        ..entry
    };
    VECTOR_SUBJECT_TO_ID
        .with_borrow_mut(|m| m.insert(SubjectKey::new(INDEX_ID, subject(7)), drifted));

    let result = store.vector_search(&search_value(1.0, 10)).expect("search");
    assert!(
        result.hits.is_empty(),
        "inconsistent slot Some / vector_id None row must not be scored"
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
    };
    assert_eq!(
        store.vector_search(&req).unwrap_err(),
        VectorIndexError::DimensionMismatch
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
    };
    assert_eq!(
        store.vector_search(&req).unwrap_err(),
        VectorIndexError::ByteWidthMismatch
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
        VectorIndexError::InvalidSearchTopK
    );
    assert_eq!(
        store
            .vector_search(&search_value(1.0, MAX_VECTOR_SEARCH_TOP_K + 1))
            .unwrap_err(),
        VectorIndexError::InvalidSearchTopK
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
