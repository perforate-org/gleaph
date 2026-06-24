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

// --- ADR 0031 Slice 6: reverse map maintenance (VECTOR_ID_TO_SUBJECT) ---

#[test]
fn reverse_map_inserted_on_upsert() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    assert_eq!(
        store.id_to_subject_for_test(INDEX_ID, 1),
        Some(subject(7)),
        "new subject inserts its reverse-map entry"
    );
}

#[test]
fn reverse_map_removed_on_remove() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 2))
        .unwrap();
    assert_eq!(
        store.id_to_subject_for_test(INDEX_ID, 1),
        None,
        "remove drops the reverse-map entry alongside the id→slot entry"
    );
}

#[test]
fn reverse_map_resurrect_drops_old_id_and_adds_fresh() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec_inc(7, 1, 1, 1.0))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op_inc(7, 1, 2))
        .unwrap();
    // Fresh incarnation resurrects with a brand-new vector_id (2).
    store
        .vector_upsert(shard_canister(), &upsert_vec_inc(7, 2, 1, 9.0))
        .unwrap();
    assert_eq!(store.id_to_subject_for_test(INDEX_ID, 1), None);
    assert_eq!(store.id_to_subject_for_test(INDEX_ID, 2), Some(subject(7)));
}

#[test]
fn reverse_map_unchanged_on_same_incarnation_newer_version() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    // Same incarnation, newer version reuses the live vector_id (1); the reverse map is untouched.
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 2, 5.0))
        .unwrap();
    assert_eq!(store.id_to_subject_for_test(INDEX_ID, 1), Some(subject(7)));
    assert_eq!(store.id_to_subject_for_test(INDEX_ID, 2), None);
}

#[test]
fn reverse_map_unchanged_on_stale_replay() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 2, 5.0))
        .unwrap();
    // Stale replay within the live incarnation (version < clock) is a no-op.
    store
        .vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0))
        .unwrap();
    assert_eq!(store.id_to_subject_for_test(INDEX_ID, 1), Some(subject(7)));
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

fn tuned(nprobe: u32) -> SearchTuning {
    SearchTuning { nprobe }
}

#[test]
fn partition_scan_parity_with_exact_at_nprobe_equals_nlist() {
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
        .vector_search_tuned(&search_value(0.5, 10), tuned(2))
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
    assert_eq!(p, e, "nprobe = nlist partition scan equals exact scan");
    assert_eq!(p.len(), 4, "all seeded vectors returned");
}

#[test]
fn partition_scan_nprobe_one_selects_single_partition() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Query near centroid 0: nprobe = 1 selects partition 0 only.
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(1))
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
    // Query near centroid 1: nprobe = 1 selects partition 1 only.
    let result = store
        .vector_search_tuned(&search_value(10.0, 10), tuned(1))
        .expect("partition scan");
    let subjects: Vec<_> = result.hits.iter().map(|h| h.subject).collect();
    assert_eq!(
        subjects,
        vec![subject(4), subject(3)],
        "only partition 1 members, nearest first"
    );
}

#[test]
fn partition_scan_default_nprobe_used_by_vector_search() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Default nprobe = min(4, nlist) = 2 = nlist, so the default path scans both partitions.
    let result = store
        .vector_search(&search_value(0.5, 10))
        .expect("default search");
    assert_eq!(result.hits.len(), 4);
}

#[test]
fn partition_scan_skips_deleted_subject_entry() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{SubjectKey, SubjectMapEntry};

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
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
        m.insert(
            SubjectKey::new(INDEX_ID, subject(1)),
            SubjectMapEntry {
                deleted: true,
                ..entry
            },
        )
    });
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(2))
        .expect("partition scan");
    assert!(result.hits.iter().all(|h| h.subject != subject(1)));
}

/// ADR 0032: the partition scan resolves the subject from the row-local `subject_locator`, so
/// dropping `VECTOR_ID_TO_SUBJECT` (retired from this hot path) no longer hides the row. Freshness is
/// instead re-validated against `VECTOR_SUBJECT_TO_ID`.
#[test]
fn partition_scan_ignores_reverse_map_after_locator_retirement() {
    use crate::facade::stable::VECTOR_ID_TO_SUBJECT;
    use crate::records::VectorIdKey;

    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Drop the reverse-map entry for vector_id 1 (subject 1): the hot path no longer reads it, so the
    // row is still resolved via its locator and remains scoreable.
    VECTOR_ID_TO_SUBJECT.with_borrow_mut(|m| m.remove(&VectorIdKey::new(INDEX_ID, 1)));
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(2))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

/// ADR 0032 meta/head drift: a row present in `VECTOR_PAGE_META`/slab but with no live
/// `VECTOR_SUBJECT_TO_ID` entry (e.g. an append that committed slab+meta but not subject state) is
/// skipped by the partition scan via the `current_slot_for` freshness check, never scored.
#[test]
fn partition_scan_skips_missing_subject_entry() {
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
    // Drop the subject-map entry for subject 1: its slab row now has no freshness backing.
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| m.remove(&SubjectKey::new(INDEX_ID, subject(1))));
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(2))
        .expect("partition scan");
    assert!(result.hits.iter().all(|h| h.subject != subject(1)));
}

#[test]
fn partition_scan_skips_vector_id_mismatch() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{SubjectKey, SubjectMapEntry};

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
    // Subject entry points at a different vector_id than the page row references.
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
        m.insert(
            SubjectKey::new(INDEX_ID, subject(1)),
            SubjectMapEntry {
                vector_id: Some(9999),
                ..entry
            },
        )
    });
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(2))
        .expect("partition scan");
    assert!(result.hits.iter().all(|h| h.subject != subject(1)));
}

#[test]
fn partition_scan_skips_slot_drift() {
    use crate::facade::stable::VECTOR_SUBJECT_TO_ID;
    use crate::records::{SlotRef, SubjectKey, SubjectMapEntry};

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
    let drifted = SlotRef {
        generation: entry.slot.unwrap().generation + 1,
        ..entry.slot.unwrap()
    };
    VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
        m.insert(
            SubjectKey::new(INDEX_ID, subject(1)),
            SubjectMapEntry {
                slot: Some(drifted),
                ..entry
            },
        )
    });
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(2))
        .expect("partition scan");
    assert!(result.hits.iter().all(|h| h.subject != subject(1)));
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
    // nprobe = 1 would restrict to one partition if the partition scan ran; the exact fallback
    // returns all four regardless.
    let result = store
        .vector_search_tuned(&search_value(0.0, 10), tuned(1))
        .expect("exact fallback");
    assert_eq!(
        result.hits.len(),
        4,
        "stale centroids => exact scan over all subjects"
    );
}

#[test]
#[should_panic(expected = "out of range")]
fn tuned_nprobe_zero_panics() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let _ = store.vector_search_tuned(&search_value(0.0, 10), tuned(0));
}

#[test]
#[should_panic(expected = "out of range")]
fn tuned_nprobe_above_nlist_panics() {
    let store = fresh_store();
    store.seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let _ = store.vector_search_tuned(&search_value(0.0, 10), tuned(3));
}

// --- ADR 0031 Slice 7: production shadow-version rebuild + dual-write ---

use crate::facade::stable::{IVF_CENTROIDS, PAGE_STORE, VECTOR_PARTITION_HEADS};
use crate::records::PartitionKey;
use gleaph_graph_kernel::vector_index::VectorRebuildPhase;

/// Index version of the first production rebuild's shadow (active starts at 1).
const TARGET_V: u64 = 2;

/// Seeds `count` live subjects via production upserts with distinct values `0.0..count` so a rebuild
/// can sample distinct centroids. Returns nothing; subjects are `subject(1..=count)`.
fn seed_distinct(store: &VectorIndexStore, count: u32) {
    for v in 1..=count {
        store
            .vector_upsert(shard_canister(), &upsert_vec(v, 1, (v - 1) as f32))
            .expect("seed upsert");
    }
}

/// Drives `admin_vector_rebuild_step` (small batch to exercise cursor resumption) until the phase
/// leaves `Sampling`/`Building`, returning the terminal status.
fn drive_steps(
    store: &VectorIndexStore,
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
    store: &VectorIndexStore,
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
fn drive_cleanup(store: &VectorIndexStore, index_id: u32) -> u32 {
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
        assert_eq!(shadow.index_version, TARGET_V);
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
        VectorIndexError::InvalidRebuildParams
    );
    // sample_limit < nlist
    assert_eq!(
        store
            .admin_start_vector_rebuild(router(), INDEX_ID, 4, 3)
            .unwrap_err(),
        VectorIndexError::InvalidRebuildParams
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
        VectorIndexError::InvalidRebuildParams
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
        embedding_incarnation: 1,
        embedding_version: 1,
        encoding: VectorEncoding::F32,
        dims: big_dims,
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
        VectorIndexError::InvalidRebuildParams
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
        assert_eq!(slot.index_version, TARGET_V);
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
        VectorIndexError::RebuildAlreadyActive
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
        VectorIndexError::RebuildNotReadyToPublish
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

    // Default search now runs the partition scan (nprobe clamps to nlist=2 == full scan == exact).
    let after = store
        .vector_search(&search_value(1.5, 10))
        .expect("partition");
    assert_eq!(after.hits, before.hits);
    // nprobe = nlist parity is explicit too.
    let tuned_after = store
        .vector_search_tuned(&search_value(1.5, 10), tuned(2))
        .expect("tuned");
    assert_eq!(tuned_after.hits, before.hits);
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
    assert_eq!(err, VectorIndexError::StableGrowFailed);

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
    let vector_id = before.vector_id.expect("seeded subject has a vector id");
    let live_before = store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Inject a slab `grow` failure for the shadow append (active append succeeds first).
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = store
        .vector_upsert(shard_canister(), &upsert_vec(1, 2, 0.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorIndexError::StableGrowFailed);

    // The subject clock and id map still point at the original live slot — no partial commit to a
    // tombstoned/new slot.
    let after = store.subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    assert_eq!(after.slot, Some(old_slot), "old slot stays live");
    assert_eq!(after.shadow_slot, None, "no shadow recorded");
    assert_eq!(after.vector_id, Some(vector_id));
    assert_eq!(
        store.id_to_slot_for_test(INDEX_ID, vector_id),
        Some(old_slot),
        "id map unchanged"
    );
    // The new active row was appended then tombstoned: net live_len unchanged.
    assert_eq!(
        store.partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "active live_len restored after rollback"
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
    assert_eq!(pre.shadow_slot.unwrap().index_version, TARGET_V);

    // Touch subject 2: a newer-version upsert must operate on the target version and collapse it.
    store
        .vector_upsert(shard_canister(), &upsert_vec(2, 2, 1.0))
        .expect("upsert during cleaning");
    let post = store.subject_entry_for_test(INDEX_ID, subject(2)).unwrap();
    assert_eq!(
        post.slot.unwrap().index_version,
        TARGET_V,
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
    let after = store.vector_search(&search_value(2.0, 10)).expect("search");
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
    assert_eq!(slot.index_version, TARGET_V);
    assert_eq!(
        slot.partition_id, 0,
        "value 0 lands in centroid-0 partition"
    );
    let after = store.vector_search(&search_value(0.0, 10)).expect("search");
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
    let before = store
        .vector_search(&search_value(2.5, 10))
        .expect("exact-ish");

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
        .vector_search_tuned(&search_value(2.5, 10), tuned(3))
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
        .vector_search_tuned(&search_value(0.0, 10), tuned(3))
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
        VectorIndexError::UnknownIndex
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
    assert_eq!(err, VectorIndexError::StableGrowFailed);
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
        VectorIndexError::Unauthorized
    );
}
