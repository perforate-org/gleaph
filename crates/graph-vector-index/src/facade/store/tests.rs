//! Unit tests for the degenerate `ivf_flat` mutation store (ADR 0031 Slice 2).

use super::VectorIndexStore;
use crate::init::VectorIndexInitArgs;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorIndexError, VectorSubject,
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

fn upsert_op(vertex_id: u32, embedding_version: u64, fill: u8) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        embedding_version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        bytes: vec![fill; STRIDE],
        remove: false,
    }
}

fn remove_op(vertex_id: u32, embedding_version: u64) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: subject(vertex_id),
        embedding_version,
        encoding: VectorEncoding::F32,
        dims: DIMS,
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
fn upsert_to_deleted_subject_resurrects_regardless_of_version() {
    // The canister trusts delivered upserts: a tombstoned subject is resurrected with a fresh
    // VectorId even when the upsert version is <= the tombstone clock. Stale-replay protection
    // lives in the graph repair-drain (canonical re-derivation), not here, because the canonical
    // embedding_version resets on re-insert and cannot be ordered against the clock.
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB))
        .unwrap();
    store
        .vector_remove(shard_canister(), &remove_op(7, 2))
        .unwrap();
    // Re-insert at version 1 (canonical reset) lands behind a clock of 2: still resurrects.
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();

    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "delivered upsert resurrects the subject");
    assert_eq!(entry.stored_embedding_version, 1);
    let new_id = entry.vector_id.expect("resurrected entry has a vector_id");
    assert_eq!(new_id, 2, "fresh VectorId allocated; old id retired");
    assert!(store.id_to_slot_for_test(INDEX_ID, new_id).is_some());
}

#[test]
fn upsert_after_missing_remove_clock_resurrects() {
    // A remove on a never-inserted subject writes a tombstone clock; a subsequent upsert is a
    // delivered re-insert and resurrects with a fresh id (graph drain guards against stale ones).
    let store = fresh_store();
    store
        .vector_remove(shard_canister(), &remove_op(7, 5))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    let entry = store.subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "delivered upsert resurrects after a clock");
    assert_eq!(entry.stored_embedding_version, 1);
    assert!(entry.vector_id.is_some());
}

#[test]
fn reinsert_after_delete_allocates_fresh_vector_id() {
    let store = fresh_store();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA))
        .unwrap();
    let first_id = store
        .subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .vector_id
        .unwrap();
    assert_eq!(first_id, 1);

    store
        .vector_remove(shard_canister(), &remove_op(7, 2))
        .unwrap();
    store
        .vector_upsert(shard_canister(), &upsert_op(7, 3, 0xCC))
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
fn attach_rejects_anonymous_principal_and_out_of_range() {
    let store = fresh_store();
    assert_eq!(
        store
            .admin_attach_shard_canister(
                router(),
                GraphId::from_raw(1),
                1,
                0,
                ShardId::new(0),
                Principal::anonymous(),
            )
            .unwrap_err(),
        VectorIndexError::InvalidPrincipalInRegistry
    );
    // group [0,1) does not contain shard 5.
    assert_eq!(
        store
            .admin_attach_shard_canister(
                router(),
                GraphId::from_raw(1),
                1,
                0,
                ShardId::new(5),
                Principal::from_slice(&[7]),
            )
            .unwrap_err(),
        VectorIndexError::ShardOutOfRangeForGroup
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
                1,
                0,
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
