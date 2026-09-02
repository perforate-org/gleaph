//! Unit tests for the degenerate `ivf_flat` mutation store (ADR 0031 Slice 2).

use crate::facade::stable::definition_store;
use crate::facade::stable::memory::{ActiveShardDetach, VectorIndexOwnershipConfig};
use crate::facade::stable::{
    OWNERSHIP_CONFIG, SHARD_CANISTER_CATALOG, VECTOR_GC_CURSOR, VECTOR_SHARD_WATERMARKS,
};
use crate::init::{DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED, VectorCanisterInitArgs};
use crate::records::{PartitionHeadRecord, ShardWatermarks, VectorRebuildStateRecord};
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachPhase, ShardId};
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_FILTER_CANDIDATES, MAX_VECTOR_SEARCH_TOP_K, VectorCanisterError,
    VectorEmbeddingSyncOp, VectorEncoding, VectorMaintenancePolicy,
    VectorMaintenanceRecommendation, VectorMetric, VectorPartitionPageHealth, VectorSearchRequest,
    VectorSubject,
};

const INDEX_ID: u32 = 1;
const DIMS: u16 = 4;
const STRIDE: usize = 16; // dims * 4 for F32

use super::mutation::read_slot_bytes;
use super::rebuild::rebuild_step_with_budget;
use super::search::{candidate_subject_scan, vector_search_tuned};
use super::*;

fn router() -> Principal {
    Principal::from_slice(&[9])
}

fn shard_canister() -> Principal {
    Principal::from_slice(&[1])
}

/// Resets the fixture-only store and attaches shard 0.
fn fresh_store() {
    reset_for_test_or_bench(&VectorCanisterInitArgs {
        router_canister: router(),
        definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
        subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
    })
    .expect("init");
    attach_single_shard_for_test(router(), ShardId::new(0), shard_canister());
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
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("upsert");

    let def = def_for_test(INDEX_ID).expect("def created lazily");
    assert_eq!(def.active_index_version, 1);
    assert_eq!(def.dims, DIMS);
    assert_eq!(def.stride_bytes, STRIDE as u32);

    let entry = subject_entry_for_test(INDEX_ID, subject(7)).expect("clock");
    assert!(!entry.deleted);
    assert_eq!(entry.stamp, 1);
    let slot = entry.slot.expect("live slot");
    assert_eq!(slot.slot, 0, "first row lands at slot 0");
}

#[test]
fn typed_sync_unavailable_before_first_operation_is_outer_error_without_write() {
    fresh_store();
    let op = upsert_op(45, 1, 0xA5);
    definition_store::unbind_for_test().expect("unbind ready definition owner");

    let result = crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &[op]);

    assert!(matches!(
        result,
        Err(crate::canister::VectorSyncBatchOutcomeDriverError::StoreUnavailable)
    ));
    assert!(def_for_test(INDEX_ID).is_none());
    assert!(subject_entry_for_test(INDEX_ID, subject(45)).is_none());
    definition_store::reopen_for_test().expect("restore exact-open definition owner");
}

#[test]
fn typed_sync_replay_after_discarded_success_is_canonical_noop() {
    fresh_store();
    let op = upsert_op(46, 1, 0xA6);

    // The caller loses the first successful response and retries the exact wire operation.
    let _discarded = crate::canister::vector_sync_batch_outcome_for_caller(
        shard_canister(),
        std::slice::from_ref(&op),
    )
    .expect("initial typed sync");
    let before_entry = subject_entry_for_test(INDEX_ID, subject(46)).expect("live subject entry");
    let before_slot = before_entry.slot.expect("live subject slot");
    let before_bytes = read_slot_bytes(INDEX_ID, before_slot).expect("canonical vector bytes");
    let before_head = partition_head_for_test(INDEX_ID, 1).expect("partition head");
    let before_stats = admin_vector_slab_stats(router(), Some(INDEX_ID)).expect("slab stats");

    let replay = crate::canister::vector_sync_batch_outcome_for_caller(
        shard_canister(),
        std::slice::from_ref(&op),
    )
    .expect("replayed typed sync");
    assert_eq!(
        replay,
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 1 }
    );
    replay.validate(1).expect("valid replay outcome");

    assert_eq!(
        subject_entry_for_test(INDEX_ID, subject(46)),
        Some(before_entry),
        "replay preserves the canonical subject clock and slot"
    );
    assert_eq!(
        read_slot_bytes(INDEX_ID, before_slot),
        Some(before_bytes),
        "replay preserves canonical vector bytes"
    );
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1),
        Some(before_head),
        "replay does not allocate another slot"
    );
    assert_eq!(
        admin_vector_slab_stats(router(), Some(INDEX_ID)),
        Ok(before_stats),
        "replay preserves physical allocation and row counters"
    );
}

#[test]
fn typed_batch_watermark_uses_applied_prefix() {
    fresh_store();
    VECTOR_GC_CURSOR.with_borrow_mut(|cursor| cursor.set(None));
    vector_upsert(shard_canister(), &upsert_op(100, 1, 0xA5))
        .expect("create first deleted subject definition");
    vector_remove(shard_canister(), &remove_op(100, 2)).expect("write first deleted subject clock");
    vector_upsert(shard_canister(), &upsert_op(101, 39, 0xA6))
        .expect("create second deleted subject definition");
    vector_remove(shard_canister(), &remove_op(101, 40))
        .expect("write second deleted subject clock");
    // Seed the independent Router acknowledgement floor so the Graph-owned prefix test can make
    // one tombstone eligible while retaining the later tombstone.
    VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
        watermarks.insert(
            ShardId::new(0),
            ShardWatermarks {
                graph_watermark: 0,
                router_watermark: 100,
            },
        );
    });

    let mut operations: Vec<_> = (0..32)
        .map(|vertex_id| upsert_op(vertex_id, u64::from(vertex_id) + 1, 0xA7))
        .collect();
    operations.push(upsert_op(32, 100, 0xA8));

    // Submit only the committed prefix. A later request carrying stamp 100 must not be able to
    // advance the Graph watermark or collect deleted clocks before that suffix is admitted.
    let progress =
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations[..32])
            .expect("typed batch progress");
    assert_eq!(
        progress,
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 32 }
    );
    assert!(subject_entry_for_test(INDEX_ID, subject(31)).is_some());
    assert!(subject_entry_for_test(INDEX_ID, subject(32)).is_none());

    let watermarks_before_suffix = VECTOR_SHARD_WATERMARKS
        .with_borrow(|watermarks| watermarks.get(&ShardId::new(0)).expect("watermark record"));
    assert_eq!(watermarks_before_suffix.graph_watermark, 32);
    assert_eq!(watermarks_before_suffix.router_watermark, 100);

    let watermarks = VECTOR_SHARD_WATERMARKS
        .with_borrow(|watermarks| watermarks.get(&ShardId::new(0)).expect("watermark record"));
    assert_eq!(watermarks.graph_watermark, 32);
    assert_eq!(watermarks.router_watermark, 100);
    assert!(
        subject_entry_for_test(INDEX_ID, subject(100)).is_none(),
        "GC removes the tombstone below the applied Graph watermark"
    );
    assert!(
        subject_entry_for_test(INDEX_ID, subject(101))
            .expect("later deleted subject clock")
            .deleted,
        "the unapplied suffix must not make the later tombstone GC-eligible"
    );
}

#[test]
fn typed_batch_replay_preserves_all_canonical_rows_and_page_allocation() {
    fresh_store();
    let operations = vec![
        upsert_op(110, 1, 0xA1),
        upsert_op(111, 1, 0xA2),
        upsert_op(112, 1, 0xA3),
    ];

    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("initial typed batch"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 3 }
    );
    let before_entries: Vec<_> = operations
        .iter()
        .map(|op| subject_entry_for_test(INDEX_ID, op.subject).expect("initial subject entry"))
        .collect();
    let before_head = partition_head_for_test(INDEX_ID, 1).expect("initial partition head");
    let before_stats =
        admin_vector_slab_stats(router(), Some(INDEX_ID)).expect("initial slab stats");

    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("same-ID replay"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 3 }
    );
    let after_entries: Vec<_> = operations
        .iter()
        .map(|op| subject_entry_for_test(INDEX_ID, op.subject).expect("replayed subject entry"))
        .collect();

    assert_eq!(after_entries, before_entries);
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1).expect("replayed partition head"),
        before_head
    );
    assert_eq!(
        admin_vector_slab_stats(router(), Some(INDEX_ID)),
        Ok(before_stats)
    );
}

#[test]
fn typed_batch_preserves_order_when_a_subject_repeats() {
    fresh_store();
    let operations = vec![upsert_op(113, 1, 0xB1), upsert_op(113, 2, 0xB2)];

    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("ordered typed batch"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 2 }
    );
    let entry = subject_entry_for_test(INDEX_ID, subject(113)).expect("final subject entry");
    assert_eq!(entry.stamp, 2);
    assert!(!entry.deleted);
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1)
            .expect("final partition head")
            .live_len,
        1,
        "the first row is tombstoned before the ordered update becomes live"
    );
}

#[test]
fn typed_batch_batches_live_updates_with_fresh_rows() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(114, 1, 0xC1)).expect("seed live subject");
    let old_slot = subject_entry_for_test(INDEX_ID, subject(114))
        .expect("seed entry")
        .slot
        .expect("seed slot");

    let operations = vec![
        upsert_op(114, 2, 0xC2),
        upsert_op(115, 1, 0xC3),
        upsert_op(116, 1, 0xC4),
    ];
    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("mixed typed batch"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 3 }
    );

    let updated = subject_entry_for_test(INDEX_ID, subject(114)).expect("updated entry");
    assert_eq!(updated.stamp, 2);
    assert_ne!(updated.slot.expect("updated slot"), old_slot);
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1)
            .expect("mixed partition head")
            .live_len,
        3
    );
}

#[test]
fn typed_batch_rejects_dimension_mismatch_without_appending_the_bad_row() {
    fresh_store();
    let mut operations: Vec<_> = (0..64)
        .map(|index| upsert_op(120 + index, u64::from(index) + 1, index as u8))
        .collect();
    let wrong_dims = &mut operations[33];
    wrong_dims.dims = DIMS + 1;
    wrong_dims.bytes = vec![0xC3; wrong_dims.dims as usize * 4];

    assert!(matches!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations),
        Err(crate::canister::VectorSyncBatchOutcomeDriverError::Fatal(
            VectorCanisterError::DimensionMismatch
        ))
    ));
    assert!(def_for_test(INDEX_ID).is_none());
    assert!(
        subject_entry_for_test(INDEX_ID, subject(120)).is_none(),
        "fatal validation must complete before any earlier chunk can commit"
    );
    assert!(subject_entry_for_test(INDEX_ID, subject(153)).is_none());
    assert!(
        subject_entry_for_test(INDEX_ID, subject(183)).is_none(),
        "the suffix is not attempted after a fatal operation"
    );
    assert!(partition_head_for_test(INDEX_ID, 1).is_none());
}

#[test]
fn typed_batch_terminal_in_second_chunk_reports_global_prefix_and_replays_safely() {
    fresh_store();
    let operations: Vec<_> = (0..68)
        .map(|index| upsert_op(200 + index, u64::from(index) + 1, index as u8))
        .collect();

    // The first 32-row chunk and the first row of chunk two commit. The next subject-map commit
    // reports terminal pressure, so both public indices must be global (33/33), not chunk-local
    // (1/1). The suffix is retained for an exact replay.
    crate::facade::store::mutation::arm_typed_subject_table_pressure(33);
    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("typed terminal outcome"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Terminal {
            applied: 33,
            failed_index: 33,
            error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::SubjectTablePressure,
        }
    );

    for (index, operation) in operations.iter().enumerate() {
        let entry = subject_entry_for_test(INDEX_ID, operation.subject);
        if index < 33 {
            let entry = entry.expect("committed prefix subject");
            assert!(!entry.deleted, "committed prefix remains live at {index}");
        } else {
            assert!(
                entry.is_none(),
                "failed/suffix subject is not acknowledged at {index}"
            );
        }
    }
    let terminal_stats =
        admin_vector_slab_stats(router(), Some(INDEX_ID)).expect("terminal slab stats");
    assert_eq!(terminal_stats.scope.physical_live_row_count, 33);
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1)
            .expect("terminal partition head")
            .live_len,
        33
    );
    let prefix_entries: Vec<_> = operations[..33]
        .iter()
        .map(|operation| subject_entry_for_test(INDEX_ID, operation.subject).expect("prefix entry"))
        .collect();

    // The failed operation and suffix are replayed with their original IDs. The acknowledged
    // prefix must remain byte-for-byte canonical and must not allocate duplicate live rows.
    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("exact suffix replay"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 68 }
    );
    let replayed_prefix: Vec<_> = operations[..33]
        .iter()
        .map(|operation| {
            subject_entry_for_test(INDEX_ID, operation.subject).expect("replayed prefix entry")
        })
        .collect();
    assert_eq!(replayed_prefix, prefix_entries);
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1)
            .expect("replayed partition head")
            .live_len,
        68
    );
    let replay_stats =
        admin_vector_slab_stats(router(), Some(INDEX_ID)).expect("replayed slab stats");
    assert_eq!(replay_stats.scope.physical_live_row_count, 68);
    assert_eq!(
        replay_stats.scope.row_count,
        terminal_stats.scope.row_count + 35,
        "replay appends only the 35 unacknowledged operations"
    );
}

#[test]
fn typed_batch_terminal_pressure_acknowledges_only_the_committed_prefix() {
    fresh_store();
    let operations = vec![
        upsert_op(117, 1, 0xD1),
        upsert_op(118, 1, 0xD2),
        upsert_op(119, 1, 0xD3),
    ];
    crate::facade::store::mutation::arm_typed_subject_table_pressure(1);

    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("typed terminal outcome"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Terminal {
            applied: 1,
            failed_index: 1,
            error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::SubjectTablePressure,
        }
    );
    assert!(subject_entry_for_test(INDEX_ID, subject(117)).is_some());
    assert!(subject_entry_for_test(INDEX_ID, subject(118)).is_none());
    assert!(subject_entry_for_test(INDEX_ID, subject(119)).is_none());
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1)
            .expect("terminal partition head")
            .live_len,
        1
    );

    // Retrying the exact request converges the failed suffix without duplicating the acknowledged
    // first row.
    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &operations)
            .expect("terminal suffix retry"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Progress { applied: 3 }
    );
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1)
            .expect("replayed partition head")
            .live_len,
        3
    );
}

#[test]
fn typed_batch_zero_prefix_terminal_does_not_run_graph_gc() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(123, 1, 0xD0)).expect("seed deleted clock");
    vector_remove(shard_canister(), &remove_op(123, 2)).expect("seed deleted clock");
    VECTOR_GC_CURSOR.with_borrow_mut(|cursor| cursor.set(None));
    VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
        watermarks.insert(
            ShardId::new(0),
            ShardWatermarks {
                graph_watermark: 100,
                router_watermark: 100,
            },
        );
    });

    crate::facade::store::mutation::arm_typed_subject_table_pressure(0);
    let pressure_op = upsert_op(124, 1, 0xD1);
    assert_eq!(
        crate::canister::vector_sync_batch_outcome_for_caller(
            shard_canister(),
            std::slice::from_ref(&pressure_op),
        )
        .expect("zero-prefix terminal outcome"),
        gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome::Terminal {
            applied: 0,
            failed_index: 0,
            error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::SubjectTablePressure,
        }
    );
    assert!(
        subject_entry_for_test(INDEX_ID, subject(123))
            .expect("deleted clock")
            .deleted,
        "a zero-length committed prefix must not trigger Graph GC"
    );
}

#[test]
fn m10_response_loss_retains_m11_fence_until_exact_resolution() {
    fresh_store();
    VECTOR_GC_CURSOR.with_borrow_mut(|cursor| cursor.set(None));
    VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
        watermarks.insert(ShardId::new(0), ShardWatermarks::default());
    });

    // Router m10 applies a vector, but the response is lost. A later Router request cannot prove
    // that this stamp is part of a contiguous acknowledged prefix.
    let m10 = upsert_op(7, 10, 0xA0);
    let _lost_response =
        crate::canister::vector_sync_batch_outcome_for_caller(router(), std::slice::from_ref(&m10))
            .expect("Router m10 apply");
    assert_eq!(
        subject_entry_for_test(INDEX_ID, subject(7))
            .expect("m10 subject")
            .stamp,
        10
    );

    // Graph m11 tombstones the m10 subject. The Graph watermark is valid, but the Router floor is
    // still zero because the Router did not acknowledge a contiguous prefix.
    let m11 = remove_op(7, 11);
    crate::canister::vector_sync_batch_outcome_for_caller(
        shard_canister(),
        std::slice::from_ref(&m11),
    )
    .expect("Graph m11 delete");

    // A newer Router m12 operation for another subject must not convert the lost m10 response into
    // a Router watermark or make the m11 tombstone GC-eligible.
    let m12 = upsert_op(8, 12, 0xA2);
    crate::canister::vector_sync_batch_outcome_for_caller(router(), std::slice::from_ref(&m12))
        .expect("Router m12 apply");

    let watermarks = VECTOR_SHARD_WATERMARKS
        .with_borrow(|watermarks| watermarks.get(&ShardId::new(0)).expect("watermark record"));
    assert_eq!(watermarks.graph_watermark, 11);
    assert_eq!(watermarks.router_watermark, 0);
    assert_eq!(crate::facade::gc_subjects_step(20_000), 0);
    let tombstone = subject_entry_for_test(INDEX_ID, subject(7)).expect("m11 tombstone retained");
    assert!(tombstone.deleted);
    assert_eq!(tombstone.stamp, 11);
    assert_eq!(tombstone.slot, None);

    // Replay of the lost Router m10 response is fenced by the retained m11 clock and cannot
    // resurrect the subject.
    crate::canister::vector_sync_batch_outcome_for_caller(router(), std::slice::from_ref(&m10))
        .expect("stale Router m10 replay");
    let replayed =
        subject_entry_for_test(INDEX_ID, subject(7)).expect("tombstone after stale replay");
    assert!(replayed.deleted);
    assert_eq!(replayed.stamp, 11);
    assert_eq!(replayed.slot, None);

    // The Router now observes the exact m10 outcome and publishes the contiguous frontier through
    // the guarded Vector owner. Only after that observed resolution may the m11 fence be collected.
    advance_router_frontier(router(), ShardId::new(0), 11)
        .expect("exact m10 resolution advances the safe frontier");
    let watermarks = VECTOR_SHARD_WATERMARKS
        .with_borrow(|watermarks| watermarks.get(&ShardId::new(0)).expect("watermark record"));
    assert_eq!(watermarks.router_watermark, 11);
    assert!(
        subject_entry_for_test(INDEX_ID, subject(7)).is_none(),
        "the m11 fence is collected only after exact m10 resolution"
    );
}

#[test]
fn upsert_same_version_identical_payload_is_noop() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("idempotent no-op");
    let head = partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 1, "no new slot appended");
}

#[test]
fn upsert_same_version_different_payload_conflicts() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    let err = vector_upsert(shard_canister(), &upsert_op(7, 1, 0xBB)).expect_err("conflict");
    assert_eq!(err, VectorCanisterError::MutationStampConflict);
}

#[test]
fn upsert_older_version_is_noop() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 5, 0xAA)).unwrap();
    vector_upsert(shard_canister(), &upsert_op(7, 3, 0xBB)).expect("stale no-op");
    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert_eq!(entry.stamp, 5);
}

#[test]
fn upsert_newer_version_live_appends_and_tombstones_old_slot() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    let old_slot = subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .unwrap();

    vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB)).unwrap();
    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert_eq!(entry.stamp, 2);
    let new_slot = entry.slot.unwrap();
    assert_ne!(
        new_slot.slot, old_slot.slot,
        "newer version appends a fresh slot"
    );
    let head = partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 1, "append +1, tombstone -1");
}

#[test]
fn remove_live_tombstones_and_advances_clock() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_remove(shard_canister(), &remove_op(7, 2)).unwrap();

    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted);
    assert_eq!(entry.stamp, 2);
    assert_eq!(entry.slot, None);
    let head = partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.live_len, 0);
}

#[test]
fn remove_missing_subject_writes_tombstone_clock() {
    fresh_store();
    // No def yet; remove on a never-inserted subject still writes a clock.
    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    let entry = subject_entry_for_test(INDEX_ID, subject(7)).expect("clock written");
    assert!(entry.deleted);
    assert_eq!(entry.stamp, 1);
}

#[test]
fn same_incarnation_upsert_to_deleted_subject_is_noop() {
    // Under incarnation fencing, an upsert at the *same* incarnation as a tombstone is a stale
    // replay: a genuine reinsert carries a strictly greater incarnation. So it must NOT resurrect.
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    // Stale same-incarnation upsert (e.g. a journaled replay) lands behind the tombstone clock.
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("stale replay no-op");

    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted, "same-incarnation upsert cannot resurrect");
}

#[test]
fn newer_incarnation_upsert_resurrects_with_fresh_slot() {
    // Resurrection requires a strictly greater incarnation, mirroring the canonical store bumping
    // the incarnation on each delete/reinsert. The fresh incarnation lands a brand-new slot.
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    let old_slot = subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .unwrap();
    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    // Reinsert at incarnation 2, version 1 (canonical version reset): resurrects.
    vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB)).unwrap();

    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
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
    fresh_store();
    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("same-incarnation replay no-op");
    assert!(
        subject_entry_for_test(INDEX_ID, subject(7))
            .unwrap()
            .deleted
    );
    vector_upsert(shard_canister(), &upsert_op(7, 2, 0xAA)).unwrap();
    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(!entry.deleted, "newer incarnation resurrects after a clock");
    assert_eq!(entry.stamp, 2);
}

#[test]
fn reinsert_after_delete_appends_fresh_slot() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    let first_slot = subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .unwrap();

    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    // The canonical reinsert bumps the incarnation to 2.
    vector_upsert(shard_canister(), &upsert_op(7, 2, 0xCC)).unwrap();

    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
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
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    // Reinsert at incarnation 2 (live again, fresh slot).
    vector_upsert(shard_canister(), &upsert_op(7, 2, 0xBB)).unwrap();

    // Late blind remove for the OLD incarnation with the authoritative max version: must no-op.
    vector_remove(shard_canister(), &remove_op(7, 1))
        .expect("stale older-incarnation remove is fenced");

    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
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
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_remove(shard_canister(), &remove_op(7, 2)).unwrap();
    let entry = subject_entry_for_test(INDEX_ID, subject(7)).unwrap();
    assert!(entry.deleted);
    assert_eq!(entry.stamp, 2);
    assert_eq!(entry.slot, None);
}

#[test]
fn page_capacity_rolls_to_new_page_at_slots_per_page() {
    fresh_store();
    // d = 4 F32: pad stride 16, meta 4, single shard. A 80-byte budget fits exactly 2 rows.
    create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 80).expect("create");
    assert_eq!(def_for_test(INDEX_ID).unwrap().slots_per_page, 2);

    for v in 0..3u32 {
        vector_upsert(shard_canister(), &upsert_op(v, 1, v as u8)).unwrap();
    }
    let head = partition_head_for_test(INDEX_ID, 1).unwrap();
    assert_eq!(head.page_count, 2, "third insert rolls to a new page");
    assert_eq!(head.next_page_id, 2);
    assert_eq!(head.live_len, 3);
}

#[test]
fn create_index_rejects_capacity_below_one_slot() {
    fresh_store();
    // d = 4 F32 needs 64 bytes for a single row; a 40-byte budget fits no row.
    let err = create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 40).expect_err("reject");
    assert_eq!(err, VectorCanisterError::InvalidPageCapacity);
}

#[test]
fn upsert_dimension_and_byte_width_mismatch() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();

    let mut wrong_dims = upsert_op(8, 1, 0xAA);
    wrong_dims.dims = DIMS + 1;
    assert_eq!(
        vector_upsert(shard_canister(), &wrong_dims).unwrap_err(),
        VectorCanisterError::DimensionMismatch
    );

    let mut wrong_bytes = upsert_op(9, 1, 0xAA);
    wrong_bytes.bytes = vec![0u8; STRIDE - 1];
    assert_eq!(
        vector_upsert(shard_canister(), &wrong_bytes).unwrap_err(),
        VectorCanisterError::ByteWidthMismatch
    );
}

#[test]
fn vector_upsert_rejects_remove_flag() {
    fresh_store();
    let mut op = upsert_op(7, 1, 0xAA);
    op.remove = true;
    assert_eq!(
        vector_upsert(shard_canister(), &op).unwrap_err(),
        VectorCanisterError::MutationKindMismatch
    );
    // The contradictory op must not have mutated any state.
    assert!(subject_entry_for_test(INDEX_ID, subject(7)).is_none());
}

#[test]
fn vector_remove_rejects_insert_flag() {
    fresh_store();
    let mut op = remove_op(7, 1);
    op.remove = false;
    assert_eq!(
        vector_remove(shard_canister(), &op).unwrap_err(),
        VectorCanisterError::MutationKindMismatch
    );
    assert!(subject_entry_for_test(INDEX_ID, subject(7)).is_none());
}

#[test]
fn mutation_auth_rejects_unattached_and_cross_shard() {
    fresh_store();
    let stranger = Principal::from_slice(&[2]);
    assert_eq!(
        vector_upsert(stranger, &upsert_op(7, 1, 0xAA)).unwrap_err(),
        VectorCanisterError::ShardNotAttached
    );

    // Caller attached to shard 0 but op targets shard 1.
    let mut cross = upsert_op(7, 1, 0xAA);
    cross.subject = VectorSubject::Vertex {
        shard_id: ShardId::new(1),
        vertex_id: 7,
    };
    assert_eq!(
        vector_upsert(shard_canister(), &cross).unwrap_err(),
        VectorCanisterError::ShardMismatch
    );
}

#[test]
fn router_can_persist_any_shard_subject() {
    fresh_store();
    // The Router is the trusted coordinator (ADR 0064 §6): it persists ops for any shard, so it must
    // not be rejected as an unattached caller. This is the path the typed batch endpoint exercises.
    vector_upsert(router(), &upsert_op(7, 1, 0xAA)).expect("Router upsert for shard 0");

    let mut cross = upsert_op(8, 2, 0xBB);
    cross.subject = VectorSubject::Vertex {
        shard_id: ShardId::new(1),
        vertex_id: 8,
    };
    vector_upsert(router(), &cross).expect("Router upsert for a shard it is not attached to");

    vector_remove(router(), &remove_op(7, 3)).expect("Router remove");
}

#[test]
fn init_rejects_anonymous_router() {
    let err = init_from_args(&VectorCanisterInitArgs {
        router_canister: Principal::anonymous(),
        definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
        subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
    })
    .expect_err("anonymous router rejected");
    assert_eq!(err, VectorCanisterError::AnonymousRouter);
}

#[test]
fn attach_rejects_anonymous_principal() {
    fresh_store();
    assert_eq!(
        admin_attach_shard_canister(
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
    reset_for_test_or_bench(&VectorCanisterInitArgs {
        router_canister: router(),
        definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
        subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
    })
    .expect("init");
    let graph = GraphId::from_raw(1);
    // One vector target owns *every* shard of the graph (ADR 0031 Slice 4 target model B). Shard 0
    // pins the graph; a *different* shard of the SAME graph must also attach (the old property-index
    // group model rejected this with GraphOwnershipMismatch — the bug this guards against).
    admin_attach_shard_canister(
        router(),
        graph,
        ShardId::new(0),
        Principal::from_slice(&[10]),
    )
    .expect("attach shard 0");
    admin_attach_shard_canister(
        router(),
        graph,
        ShardId::new(1),
        Principal::from_slice(&[11]),
    )
    .expect("attach shard 1 to the same single target");
    // A shard belonging to a *different* graph is rejected — one target per graph.
    assert_eq!(
        admin_attach_shard_canister(
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
    fresh_store();
    let not_router = Principal::from_slice(&[123]);
    assert_eq!(
        admin_attach_shard_canister(
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
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_upsert(shard_canister(), &upsert_op(8, 1, 0xBB)).unwrap();
    vector_remove(shard_canister(), &remove_op(9, 1)).unwrap(); // tombstone clock

    let result = detach_shard_step_for_test(ShardId::new(0), None, 20_000).expect("detach step");
    assert!(result.done);
    assert!(result.removed >= 3);

    assert!(subject_entry_for_test(INDEX_ID, subject(7)).is_none());
    assert!(subject_entry_for_test(INDEX_ID, subject(8)).is_none());
    assert!(subject_entry_for_test(INDEX_ID, subject(9)).is_none());
}

#[test]
fn detach_sessions_for_distinct_shards_are_independent_and_not_cross_replayable() {
    fresh_store();
    let shard1 = Principal::from_slice(&[2]);
    admin_attach_shard_canister(router(), GraphId::from_raw(1), ShardId::new(1), shard1)
        .expect("attach shard 1");
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("seed shard 0");
    let mut shard1_op = upsert_op(8, 1, 0xBB);
    shard1_op.subject = VectorSubject::Vertex {
        shard_id: ShardId::new(1),
        vertex_id: 8,
    };
    vector_upsert(shard1, &shard1_op).expect("seed shard 1");

    let shard0_step =
        detach_shard_step_for_test(ShardId::new(0), None, 1).expect("begin shard-0 detach");
    let shard0_cursor = shard0_step.next.expect("shard-0 detach remains bounded");
    let shard1_step =
        detach_shard_step_for_test(ShardId::new(1), None, 1).expect("begin shard-1 detach");
    let shard1_cursor = shard1_step.next.expect("shard-1 detach remains bounded");
    assert_ne!(
        shard0_cursor.detach_generation,
        shard1_cursor.detach_generation
    );
    assert_eq!(
        detach_shard_step_for_test(ShardId::new(1), Some(shard0_cursor.clone()), 1,),
        Err(VectorCanisterError::LegacyOrStaleDetachCursor)
    );
    let mut wrong_phase = shard0_cursor.clone();
    wrong_phase.phase = ShardDetachPhase::Label;
    assert_eq!(
        detach_shard_step_for_test(ShardId::new(0), Some(wrong_phase), 1),
        Err(VectorCanisterError::LegacyOrStaleDetachCursor)
    );
    assert!(
        detach_shard_step_for_test(ShardId::new(0), Some(shard0_cursor), 1).is_ok(),
        "one shard's active session does not invalidate another"
    );
}

#[test]
fn legacy_detach_cursor_is_rejected_before_subject_cursor_decode_or_state_change() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("seed live subject");
    let before_config = OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone());
    let before_entry = subject_entry_for_test(INDEX_ID, subject(7)).expect("live subject entry");
    let slot = before_entry.slot.expect("live subject slot");
    let before_bytes = read_slot_bytes(INDEX_ID, slot);
    let legacy = ShardDetachCursor {
        detach_generation: None,
        phase: ShardDetachPhase::Vertex,
        // Deliberately not a SubjectScanCursor encoding: the owner must reject first.
        resume_key: vec![0xff],
    };

    assert_eq!(
        detach_shard_step_for_test(ShardId::new(0), Some(legacy), 1),
        Err(VectorCanisterError::LegacyOrStaleDetachCursor)
    );
    assert_eq!(
        OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone()),
        before_config
    );
    assert_eq!(
        SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_for_canister(shard_canister())),
        Some(ShardId::new(0))
    );
    assert_eq!(
        subject_entry_for_test(INDEX_ID, subject(7)),
        Some(before_entry)
    );
    assert_eq!(read_slot_bytes(INDEX_ID, slot), before_bytes);
}

#[test]
fn detach_generation_exhaustion_is_an_exact_no_write_failure() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("seed live subject");
    OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
        let mut config = cell.get().clone();
        config.next_detach_generation = Some(u64::MAX);
        config.active_detaches = Some(Vec::new());
        cell.set(config);
    });
    let before_config = OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone());
    let before_entry = subject_entry_for_test(INDEX_ID, subject(7));
    let slot = before_entry
        .as_ref()
        .and_then(|entry| entry.slot)
        .expect("live subject slot");
    let before_bytes = read_slot_bytes(INDEX_ID, slot);

    assert_eq!(
        detach_shard_step_for_test(ShardId::new(0), None, 1),
        Err(VectorCanisterError::DetachGenerationExhausted)
    );
    assert_eq!(
        OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone()),
        before_config
    );
    assert_eq!(
        SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_for_canister(shard_canister())),
        Some(ShardId::new(0))
    );
    assert_eq!(subject_entry_for_test(INDEX_ID, subject(7)), before_entry);
    assert_eq!(read_slot_bytes(INDEX_ID, slot), before_bytes);
}

#[test]
fn detach_capacity_is_an_exact_no_write_failure() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).expect("seed live subject");
    OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
        let mut config = cell.get().clone();
        config.next_detach_generation = Some(65);
        config.active_detaches = Some(
            (0..super::authorization::MAX_CONCURRENT_SHARD_DETACHES)
                .map(|offset| ActiveShardDetach {
                    shard_id: ShardId::new(1_000 + u32::try_from(offset).expect("bounded offset")),
                    generation: u64::try_from(offset + 1).expect("bounded generation"),
                })
                .collect(),
        );
        cell.set(config);
    });
    let before_config: VectorIndexOwnershipConfig =
        OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone());
    let before_entry = subject_entry_for_test(INDEX_ID, subject(7));
    let slot = before_entry
        .as_ref()
        .and_then(|entry| entry.slot)
        .expect("live subject slot");
    let before_bytes = read_slot_bytes(INDEX_ID, slot);

    assert_eq!(
        detach_shard_step_for_test(ShardId::new(0), None, 1),
        Err(VectorCanisterError::TooManyActiveDetaches)
    );
    assert_eq!(
        OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone()),
        before_config
    );
    assert_eq!(
        SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_for_canister(shard_canister())),
        Some(ShardId::new(0))
    );
    assert_eq!(subject_entry_for_test(INDEX_ID, subject(7)), before_entry);
    assert_eq!(read_slot_bytes(INDEX_ID, slot), before_bytes);
}

#[test]
fn def_and_heads_persist_across_store_handles() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_op(7, 1, 0xAA)).unwrap();
    vector_upsert(shard_canister(), &upsert_op(8, 1, 0xBB)).unwrap();

    // A fresh stateless handle reads the same durable stable state ("reopen").
    let def = def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.dims, DIMS);
    let head = partition_head_for_test(INDEX_ID, 1).unwrap();
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
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    let hit = &result.hits[0];
    assert_eq!(hit.subject, subject(7));
    assert_eq!(hit.distance, 0.0);
}

#[test]
fn search_top_k_orders_by_distance_and_bounds_results() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(8, 1, 2.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(9, 1, 3.0)).unwrap();
    let result = vector_search(&search_value(1.0, 2)).expect("search");
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
    fresh_store();
    // Both are equidistant (|1-0| == |1-2|) from the query 1.0; the tie-break must be deterministic
    // on the subject key ascending.
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 0.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(8, 1, 2.0)).unwrap();
    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(result.hits[0].distance, result.hits[1].distance);
    assert_eq!(
        result.hits.iter().map(|h| h.subject).collect::<Vec<_>>(),
        vec![subject(7), subject(8)]
    );
}

#[test]
fn search_skips_deleted_subject() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    vector_remove(shard_canister(), &remove_op(7, 2)).unwrap();
    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(result.hits.is_empty(), "deleted subject must not appear");
}

#[test]
fn search_returns_newest_slot_only() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(7, 2, 5.0)).unwrap();
    // Query the newest value: exactly one hit, distance 0, at the newest version.
    let result = vector_search(&search_value(5.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].distance, 0.0);
    // The superseded (tombstoned) generation's value 1.0 is never scored.
    let stale = vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(stale.hits.len(), 1);
    assert!(stale.hits[0].distance > 0.0);
}

#[test]
fn search_reinsert_after_delete_returns_newer_incarnation_only() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    vector_remove(shard_canister(), &remove_op(7, 1)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(7, 2, 9.0)).unwrap();
    let result = vector_search(&search_value(9.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].distance, 0.0);
}

#[test]
fn search_does_not_read_rows_of_a_different_index() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    // Seed a second index with the same subject/value; a search over INDEX_ID must not read it.
    let other_index = INDEX_ID + 1;
    vector_upsert(
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
    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(result.hits.len(), 1, "only INDEX_ID rows are scanned");
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn search_scores_non_tombstoned_row_regardless_of_subject_map() {
    use crate::facade::stable::subject_store;
    use crate::records::{FixedSubjectMapEntry, SubjectKey};
    fresh_store();
    // Seed a valid live vector so the def, a page row, and a real slot all exist.
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    let entry = subject_entry_for_test(INDEX_ID, subject(7)).expect("live entry");
    assert!(entry.slot.is_some());

    // The search no longer consults the subject map: it scores every non-tombstoned row, relying on
    // the write-path invariant that a non-tombstoned row is the subject's current live slot. Even if
    // the subject-map entry is corrupted (no resolvable slot), the non-tombstoned row is still scored.
    let drifted = FixedSubjectMapEntry {
        slot: None,
        shadow_slot: None,
        ..entry
    };
    subject_store::insert(SubjectKey::new(INDEX_ID, subject(7)), drifted)
        .expect("insert drifted entry");

    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert_eq!(
        result.hits.iter().map(|h| h.subject).collect::<Vec<_>>(),
        vec![subject(7)],
        "the non-tombstoned row is scored regardless of the subject-map entry"
    );
}

#[test]
fn search_rejects_dimension_mismatch() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
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
        vector_search(&req).unwrap_err(),
        VectorCanisterError::DimensionMismatch
    );
}

#[test]
fn search_rejects_byte_width_mismatch() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
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
        vector_search(&req).unwrap_err(),
        VectorCanisterError::ByteWidthMismatch
    );
}

#[test]
fn search_rejects_invalid_top_k() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    assert_eq!(
        vector_search(&search_value(1.0, 0)).unwrap_err(),
        VectorCanisterError::InvalidSearchTopK
    );
    assert_eq!(
        vector_search(&search_value(1.0, MAX_VECTOR_SEARCH_TOP_K + 1)).unwrap_err(),
        VectorCanisterError::InvalidSearchTopK
    );
}

#[test]
fn search_missing_physical_def_returns_empty() {
    // The physical def is created lazily on first upsert; a Router-registered, activated index with
    // no embeddings yet has no def but is a known-empty index, not an unknown one.
    fresh_store();
    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(result.hits.is_empty());
}

#[test]
fn search_empty_index_returns_no_hits() {
    fresh_store();
    create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 64 * 1024).expect("create index");
    let result = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(result.hits.is_empty());
}

// --- ADR 0034 Slice 4: cosine metric-specific scoring and fail-closed paths ---

#[test]
fn cosine_exact_scan_orders_by_one_minus_similarity() {
    fresh_store();
    // Three distinct unit-direction vectors; query aligns with the first.
    let v7 = vec![1.0f32, 0.0, 0.0, 0.0];
    let v8 = vec![0.0f32, 1.0, 0.0, 0.0];
    let v9 = vec![1.0f32, 1.0, 0.0, 0.0];
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &v7, VectorMetric::Cosine),
    )
    .unwrap();
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(8, 1, &v8, VectorMetric::Cosine),
    )
    .unwrap();
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(9, 1, &v9, VectorMetric::Cosine),
    )
    .unwrap();

    let result = vector_search(&search_metric_from(
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
    fresh_store();
    // Create the physical def as a cosine index first.
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &[1.0f32; DIMS as usize], VectorMetric::Cosine),
    )
    .unwrap();
    let err = vector_search(&search_metric_from(
        &[0.0f32; DIMS as usize],
        10,
        VectorMetric::Cosine,
    ))
    .expect_err("zero-norm cosine query must fail");
    assert!(matches!(err, VectorCanisterError::InvalidQueryVector));
}

#[test]
fn cosine_nonfinite_query_fails_closed() {
    fresh_store();
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &[1.0f32; DIMS as usize], VectorMetric::Cosine),
    )
    .unwrap();
    let err = vector_search(&search_metric_from(
        &[f32::NAN; DIMS as usize],
        10,
        VectorMetric::Cosine,
    ))
    .expect_err("non-finite cosine query must fail");
    assert!(matches!(err, VectorCanisterError::InvalidQueryVector));
}

#[test]
fn cosine_zero_norm_indexed_vector_is_rejected() {
    fresh_store();
    // Zero-norm vectors have no cosine similarity; cosine ingest rejects them fail-closed instead
    // of storing a non-normalizable row.
    let err = vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &[0.0f32; DIMS as usize], VectorMetric::Cosine),
    )
    .expect_err("zero-norm cosine ingest must fail");
    assert!(matches!(err, VectorCanisterError::InvalidQueryVector));
}

#[test]
fn cosine_upsert_stores_unit_normalized_row() {
    fresh_store();
    let v = [3.0f32, 4.0, 0.0, 0.0];
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &v, VectorMetric::Cosine),
    )
    .expect("cosine upsert");
    let slot = subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .expect("live");
    let stored = read_slot_bytes(INDEX_ID, slot).expect("stored bytes");
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
    fresh_store();
    let v = [3.0f32, 4.0, 0.0, 0.0];
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &v, VectorMetric::Cosine),
    )
    .expect("first upsert");
    let slot1 = subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .expect("live");
    // Replaying the same stamp + bytes is an idempotent no-op (the normalized comparison matches the
    // stored unit row).
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &v, VectorMetric::Cosine),
    )
    .expect("idempotent replay");
    let slot2 = subject_entry_for_test(INDEX_ID, subject(7))
        .unwrap()
        .slot
        .expect("live");
    assert_eq!(slot2, slot1, "no new slot on idempotent replay");
}

#[test]
fn cosine_nonfinite_indexed_vector_is_skipped() {
    fresh_store();
    let mut bad = vec![1.0f32; DIMS as usize];
    bad[0] = f32::NAN;
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &bad, VectorMetric::Cosine),
    )
    .unwrap();
    let result = vector_search(&search_metric_from(
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
    fresh_store();
    let mut bad = vec![1.0f32; DIMS as usize];
    bad[0] = f32::NAN;
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    vector_upsert(
        shard_canister(),
        &VectorEmbeddingSyncOp {
            metric: VectorMetric::L2Squared,
            bytes: vec_bytes_from(&bad),
            ..upsert_vec(8, 1, 2.0)
        },
    )
    .unwrap();
    let result = vector_search(&search_value(1.0, 10)).expect("l2 search");
    assert_eq!(
        result.hits.len(),
        1,
        "non-finite indexed vector must be skipped, not returned"
    );
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn cosine_metric_mismatch_on_later_upsert_fails_closed() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    let err = vector_upsert(
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
    fresh_store();
    vector_upsert(
        shard_canister(),
        &upsert_vec_from(7, 1, &[1.0f32; DIMS as usize], VectorMetric::Cosine),
    )
    .unwrap();
    let err = vector_upsert(shard_canister(), &upsert_vec(8, 1, 2.0))
        .expect_err("metric mismatch must fail");
    assert!(matches!(err, VectorCanisterError::MetricMismatch));
}

#[test]
fn cosine_partition_scan_returns_cosine_ordered_rows() {
    fresh_store();
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
    seed_ivf_with_metric_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        VectorMetric::Cosine,
        &centroids,
        &rows,
    );
    // Query along +x: nearest centroid is partition 0. With the default eps=0 only partition 0 is
    // scanned, so the top hits are rows 1 and 3 (both x-direction), ordered by 1 − cos.
    let result = vector_search(&search_metric_from(
        &[1.0f32, 0.0, 0.0, 0.0],
        10,
        VectorMetric::Cosine,
    ))
    .expect("partitioned cosine search");
    assert_eq!(result.hits[0].subject, subject(1));
    assert_eq!(result.hits[1].subject, subject(3));
    // A full scan (eps = INF) returns all four, still cosine-ordered (x-rows first).
    let all = vector_search_tuned(
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
    fresh_store();
    // Distinct non-zero cosine directions so a rebuild can form >= 2 clusters.
    for (v, dir) in [
        (1u32, [1.0f32, 0.0, 0.0, 0.0]),
        (2, [0.0, 1.0, 0.0, 0.0]),
        (3, [-1.0, 0.0, 0.0, 0.0]),
        (4, [0.0, -1.0, 0.0, 0.0]),
    ] {
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(v, 1, &dir, VectorMetric::Cosine),
        )
        .unwrap();
    }
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("cosine rebuild starts");
    let status = drive_steps(INDEX_ID);
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
        let entry = subject_entry_for_test(INDEX_ID, subject(v)).unwrap();
        assert_eq!(
            entry.shadow_slot.map(|s| s.index_version),
            Some(TARGET_V as u32)
        );
    }
    // Publish, then the nlist>1 cosine index uses the partition scan (Phase 2) and still returns
    // 1 − cos ordering.
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);
    let result = vector_search(&search_metric_from(
        &[1.0f32; DIMS as usize],
        10,
        VectorMetric::Cosine,
    ))
    .expect("partitioned cosine search");
    assert!(!result.hits.is_empty());
}

#[test]
fn lazy_def_inherits_metric_from_first_op() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).unwrap();
    let def = def_for_test(INDEX_ID).expect("def");
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
    fresh_store();
    // Index 1: partitioned (nlist = 2).
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Index 2: degenerate (nlist = 1) so vector_search uses the exact subject-map scan.
    let exact_index = INDEX_ID + 100;
    let exact_vectors: Vec<_> = clustered_vectors();
    seed_ivf_for_test(
        exact_index,
        VectorEncoding::F32,
        DIMS,
        &[cvec(0.0)],
        &exact_vectors,
    );

    let partitioned =
        vector_search_tuned(&search_value(0.5, 10), tuned(f32::INFINITY)).expect("partition scan");
    let mut exact_req = search_value(0.5, 10);
    exact_req.index_id = exact_index;
    let exact = vector_search(&exact_req).expect("exact scan");

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
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Query near centroid 0: eps_query = 0 selects partition 0 only.
    let result = vector_search_tuned(&search_nonzero(0.0, 10), tuned(0.0)).expect("partition scan");
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
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Query near centroid 1: eps_query = 0 selects partition 1 only.
    let result = vector_search_tuned(&search_value(10.0, 10), tuned(0.0)).expect("partition scan");
    let subjects: Vec<_> = result.hits.iter().map(|h| h.subject).collect();
    assert_eq!(
        subjects,
        vec![subject(4), subject(3)],
        "only partition 1 members, nearest first"
    );
}

#[test]
fn partition_scan_default_eps_zero_used_by_vector_search() {
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Default eps_query = 0.0 scans only the nearest partition (query 0.5 is nearest centroid 0).
    let result = vector_search(&search_value(0.5, 10)).expect("default search");
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
    fresh_store();
    seed_ivf_for_test(
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
    let eps0 = vector_search(&req).expect("default eps=0 search");
    let eps05 = vector_search_tuned(&req, tuned(0.5)).expect("eps=0.5 search");
    let exact =
        vector_search_tuned(&req, tuned(f32::INFINITY)).expect("eps=INF exact-parity search");
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
    use crate::facade::stable::subject_store;
    use crate::records::{FixedSubjectMapEntry, SubjectKey};
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let entry = subject_entry_for_test(INDEX_ID, subject(1)).expect("seeded entry");
    // The search no longer consults the subject map: it scores every non-tombstoned row, relying on
    // the write-path invariant. Even if the subject-map entry is marked deleted (without the row being
    // tombstoned, which the write path would do), the non-tombstoned row is still scored.
    subject_store::insert(
        SubjectKey::new(INDEX_ID, subject(1)),
        FixedSubjectMapEntry {
            deleted: true,
            ..entry
        },
    )
    .expect("insert deleted entry");
    let result = vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

/// The search scores every non-tombstoned row, relying on the write-path invariant that a
/// non-tombstoned row in the active version is the subject's current live slot. A row with no
/// `VECTOR_SUBJECT_TO_ID` entry (which the write path would never leave) is still scored.
#[test]
fn partition_scan_scores_non_tombstoned_row_without_subject_entry() {
    use crate::facade::stable::subject_store;
    use crate::records::SubjectKey;
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    // Drop the subject-map entry for subject 1: its slab row is still non-tombstoned and is scored.
    subject_store::remove(&SubjectKey::new(INDEX_ID, subject(1)))
        .expect("remove subject-map fixture");
    let result = vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

// --- ADR 0034 Slice 6: candidate allowlist ---
#[test]
fn partition_scan_scores_non_tombstoned_row_despite_slot_drift() {
    use crate::facade::stable::subject_store;
    use crate::records::{FixedSubjectMapEntry, SlotRef, SubjectKey};
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let entry = subject_entry_for_test(INDEX_ID, subject(1)).expect("seeded entry");
    let live_slot = entry.slot.expect("live slot");
    // Point the subject map at an out-of-range slot (positional drift). The search no longer consults
    // the subject map, so the non-tombstoned row at the seeded position is still scored.
    let drifted = SlotRef {
        slot: live_slot.slot + 10_000,
        ..live_slot
    };
    subject_store::insert(
        SubjectKey::new(INDEX_ID, subject(1)),
        FixedSubjectMapEntry {
            slot: Some(drifted),
            ..entry
        },
    )
    .expect("insert drifted entry");
    let result = vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY))
        .expect("partition scan");
    assert!(result.hits.iter().any(|h| h.subject == subject(1)));
}

#[test]
fn stale_centroids_fall_back_to_exact_scan() {
    use crate::facade::stable::IVF_CENTROID_META;
    use crate::records::IvfCentroidMeta;
    fresh_store();
    seed_ivf_for_test(
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
                trained_index_version: 999,
            },
        )
    });
    // eps_query = 0 would restrict to one partition if the partition scan ran; the exact fallback
    // returns all four regardless.
    let result = vector_search_tuned(&search_nonzero(0.0, 10), tuned(0.0)).expect("exact fallback");
    assert_eq!(
        result.hits.len(),
        4,
        "stale centroids => exact scan over all subjects"
    );
}

#[test]
fn exact_scan_chunk_boundary_accumulates_top_k_across_chunks() {
    fresh_store();
    // More live subjects than the exact scan's SCAN_CHUNK (4096), valued by vertex id. A query at
    // 4096.0 puts the nearest hit (v4096) in the second chunk and a top-k hit (v4095) in the first
    // chunk, so the global top-3 must accumulate correctly across the chunk flush.
    for v in 0..5000u32 {
        vector_upsert(shard_canister(), &upsert_vec(v, 1, v as f32)).expect("upsert");
    }
    let result = vector_search(&search_value(4096.0, 3)).expect("exact scan");
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
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let _ = vector_search_tuned(&search_nonzero(0.0, 10), tuned(-1.0));
}

// --- ADR 0031 Slice 7: production shadow-version rebuild + dual-write ---

use crate::facade::stable::{IVF_CENTROIDS, PAGE_STORE, VECTOR_PARTITION_HEADS};
use crate::records::PartitionKey;
use gleaph_graph_kernel::vector_index::VectorRebuildPhase;

/// Index version of the first production rebuild's shadow (active starts at 1).
const TARGET_V: u64 = 2;

/// Seeds `count` live subjects via production upserts with distinct values `0.0..count` so a rebuild
/// can sample distinct centroids. Returns nothing; subjects are `subject(1..=count)`.
fn seed_distinct(count: u32) {
    for v in 1..=count {
        vector_upsert(shard_canister(), &upsert_vec(v, 1, (v - 1) as f32)).expect("seed upsert");
    }
}

/// Drives `admin_vector_rebuild_step` (small batch to exercise cursor resumption) until the phase
/// leaves `Sampling`/`Building`, returning the terminal status.
fn drive_steps(index_id: u32) -> gleaph_graph_kernel::vector_index::VectorRebuildStatus {
    for _ in 0..100_000 {
        let status = admin_vector_rebuild_step(router(), index_id, 1).expect("step");
        match status.phase {
            VectorRebuildPhase::Sampling
            | VectorRebuildPhase::Training
            // Two-level training pipeline phases advance the same way (Slice 5).
            | VectorRebuildPhase::TrainCoarse
            | VectorRebuildPhase::TrainFine { .. }
            | VectorRebuildPhase::Building => continue,
            _ => return status,
        }
    }
    panic!("rebuild steps did not terminate");
}

/// Drives steps through `Sampling` + `Training` until the phase first reaches `Building` (centroids
/// written, no subjects shadowed yet), returning that status. Panics if it terminates earlier (e.g.
/// `Failed`).
fn drive_into_building(index_id: u32) -> gleaph_graph_kernel::vector_index::VectorRebuildStatus {
    for _ in 0..100_000 {
        let status = admin_vector_rebuild_step(router(), index_id, 100).expect("step");
        match status.phase {
            VectorRebuildPhase::Sampling
            | VectorRebuildPhase::Training
            // Two-level training pipeline phases precede Building too (Slice 5).
            | VectorRebuildPhase::TrainCoarse
            | VectorRebuildPhase::TrainFine { .. } => continue,
            VectorRebuildPhase::Building => return status,
            other => panic!("expected Building, reached {other:?}"),
        }
    }
    panic!("rebuild did not reach Building");
}

/// Drives `admin_vector_rebuild_cleanup_step` (one unit at a time) until `Idle`, returning the step
/// count so a test can assert teardown was bounded across multiple messages.
fn drive_cleanup(index_id: u32) -> u32 {
    for steps in 1..=100_000u32 {
        let status = admin_vector_rebuild_cleanup_step(router(), index_id, 1).expect("cleanup");
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
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = admin_vector_rebuild_status(router(), INDEX_ID).expect("status");
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
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = drive_steps(INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
    assert_eq!(
        target_centroid_count(INDEX_ID, TARGET_V, 2),
        2,
        "exactly nlist centroids written"
    );
    // Every live subject has a shadow slot at the target version.
    for v in 1..=4u32 {
        let entry = subject_entry_for_test(INDEX_ID, subject(v)).unwrap();
        let shadow = entry.shadow_slot.expect("shadow slot");
        assert_eq!(shadow.index_version, TARGET_V as u32);
    }
}

#[test]
fn rebuild_building_link_failure_tombstones_unlinked_suffix_and_retry_converges() {
    fresh_store();
    for vertex_id in 1..=3 {
        vector_upsert(shard_canister(), &upsert_vec(vertex_id, 1, 0.0))
            .expect("seed first cluster");
    }
    for vertex_id in 4..=6 {
        vector_upsert(shard_canister(), &upsert_vec(vertex_id, 1, 10.0))
            .expect("seed second cluster");
    }
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_into_building(INDEX_ID).phase,
        VectorRebuildPhase::Building
    );

    // Each deterministic target cluster contains three subjects. Fail the second subject-map link
    // in the first appended partition batch, leaving one linked prefix row and two unlinked rows
    // that the Building compensation must tombstone.
    crate::facade::store::mutation::arm_subject_insert_failure(1);
    let error = rebuild_step_with_budget(INDEX_ID, 100, u64::MAX)
        .expect_err("second Building subject link fails");
    assert_eq!(error, VectorCanisterError::StableGrowFailed);

    let linked_slots: Vec<_> = (1..=6)
        .filter_map(|vertex_id| {
            subject_entry_for_test(INDEX_ID, subject(vertex_id))
                .expect("seeded subject")
                .shadow_slot
        })
        .collect();
    assert_eq!(linked_slots.len(), 1, "only the committed prefix is linked");
    assert_eq!(linked_slots[0].index_version, TARGET_V as u32);
    assert!(
        read_slot_bytes(INDEX_ID, linked_slots[0]).is_some(),
        "the linked prefix remains live"
    );
    let target_live_after_failure = VECTOR_PARTITION_HEADS.with_borrow(|heads| {
        (0..2)
            .filter_map(|partition| {
                heads
                    .get(&PartitionKey::new(INDEX_ID, TARGET_V, partition))
                    .expect("partition head get")
            })
            .map(|record| match record {
                PartitionHeadRecord::Head(head) => head.live_len,
                other => panic!("partition heads: unexpected record kind: {other:?}"),
            })
            .sum::<u64>()
    });
    assert_eq!(
        target_live_after_failure,
        linked_slots.len() as u64,
        "no live shadow row exists without a subject link"
    );

    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish retry");
    drive_cleanup(INDEX_ID);

    let target_live_after_cleanup = VECTOR_PARTITION_HEADS.with_borrow(|heads| {
        (0..2)
            .filter_map(|partition| {
                heads
                    .get(&PartitionKey::new(INDEX_ID, TARGET_V, partition))
                    .expect("partition head get")
            })
            .map(|record| match record {
                PartitionHeadRecord::Head(head) => head.live_len,
                other => panic!("partition heads: unexpected record kind: {other:?}"),
            })
            .sum::<u64>()
    });
    assert_eq!(target_live_after_cleanup, 6, "one live row per subject");
    let result = vector_search_tuned(&search_value(5.0, 10), tuned(f32::INFINITY))
        .expect("search after cleanup");
    assert_eq!(result.hits.len(), 6);
    for vertex_id in 1..=6 {
        assert_eq!(
            result
                .hits
                .iter()
                .filter(|hit| hit.subject == subject(vertex_id))
                .count(),
            1,
            "subject {vertex_id} appears exactly once"
        );
        let entry =
            subject_entry_for_test(INDEX_ID, subject(vertex_id)).expect("subject survives cleanup");
        assert_eq!(
            entry.slot.map(|slot| slot.index_version),
            Some(TARGET_V as u32)
        );
        assert_eq!(entry.shadow_slot, None);
    }
}

#[test]
fn rebuild_start_rejects_invalid_params() {
    fresh_store();
    seed_distinct(4);
    // nlist < 2
    assert_eq!(
        admin_start_vector_rebuild(router(), INDEX_ID, 1, 100).unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
    // sample_limit < nlist
    assert_eq!(
        admin_start_vector_rebuild(router(), INDEX_ID, 4, 3).unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
    // nlist > MAX_NLIST
    assert_eq!(
        admin_start_vector_rebuild(
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
fn rebuild_start_rejects_geometry_exceeding_pool_region() {
    fresh_store();
    // A large-dim index whose minimum viable pool region footprint — `nlist` candidate rows
    // (pad-stride + aux) plus `nlist` trained f32 centroids — exceeds the dedicated pool-region
    // budget even though `nlist <= MAX_NLIST`, because both arrays scale with dims (ADR 0031
    // Slice 8 P2; storage relocated to the raw region per the ADR 0033 implementation).
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
    vector_upsert(shard_canister(), &op).expect("seed large-dim upsert");
    let min_rows = 2u64 * super::MAX_NLIST as u64;
    assert!(
        min_rows.saturating_mul(stride as u64) > rebuild_pool::REGION_BYTES,
        "fixture must exceed the pool-region budget"
    );
    assert!(
        rebuild_pool::pool_capacity_for(stride as u32, super::MAX_NLIST, stride as u32, false)
            .is_none(),
        "fixture geometry cannot host the pool"
    );
    assert_eq!(
        admin_start_vector_rebuild(router(), INDEX_ID, super::MAX_NLIST, super::MAX_NLIST)
            .unwrap_err(),
        VectorCanisterError::InvalidRebuildParams
    );
}

// --- ADR 0033 implementation: durable rebuild-pool region ---

use crate::facade::stable::VECTOR_REBUILD_STATE;
use crate::facade::stable::memory::rebuild_pool_memory;
use crate::facade::stable::rebuild_pool;
use ic_stable_structures::Memory as _;

/// Reads the live pool header plus every accumulated candidate row byte, so two runs can prove
/// they resume from an exactly identical durable starting point.
fn pool_image() -> Vec<u8> {
    let mem = rebuild_pool_memory();
    let mut header = [0u8; rebuild_pool::POOL_HEADER_SIZE as usize];
    mem.read(0, &mut header);
    // F32 d4 fixture geometry: pad stride 16 + 8 aux bytes per row.
    let pool_len = u64::from_le_bytes(header[12..20].try_into().expect("pool_len"));
    let mut image = header.to_vec();
    let rows_bytes = pool_len * (16 + 8);
    let mut rows = vec![0u8; rows_bytes as usize];
    mem.read(rebuild_pool::POOL_HEADER_SIZE, &mut rows);
    image.extend_from_slice(&rows);
    image
}

#[test]
fn rebuild_step_fails_closed_on_corrupt_pool_header() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    // One step exhausts the range and reaches `Training` with the frozen pool in the region.
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("sampling step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    assert_eq!(status.candidates_collected, 4);

    // Corrupt the bound pad-stride field so resume validation must reject before any mutation.
    let mem = rebuild_pool_memory();
    mem.write(8, &999u32.to_le_bytes());
    assert_eq!(
        admin_vector_rebuild_step(router(), INDEX_ID, 100).unwrap_err(),
        VectorCanisterError::RebuildPoolInvalid
    );
    // The failed attempt left the durable lifecycle record untouched.
    let status = admin_vector_rebuild_status(router(), INDEX_ID).expect("status");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    assert_eq!(status.training_iteration, 0);
    assert_eq!(status.candidates_collected, 4);

    // Restore the field, then corrupt the magic bytes instead: same fail-closed outcome.
    mem.write(8, &16u32.to_le_bytes());
    mem.write(0, b"XXX");
    assert_eq!(
        admin_vector_rebuild_step(router(), INDEX_ID, 100).unwrap_err(),
        VectorCanisterError::RebuildPoolInvalid
    );
    let status = admin_vector_rebuild_status(router(), INDEX_ID).expect("status");
    assert_eq!(status.phase, VectorRebuildPhase::Training);

    // A fresh start rebinds the region (overwriting the corrupt bytes) and proceeds.
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort clears corrupt state");
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("restart");
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("step after restart");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
}

#[test]
fn aborting_and_cleaning_release_the_pool_region() {
    // Cleaning path: the pool survives publish (dead state) and is released when teardown
    // completes.
    fresh_store();
    seed_distinct(6);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_steps(INDEX_ID);
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    assert_eq!(
        rebuild_pool::bound_index(),
        Some(INDEX_ID),
        "the pool binding outlives publish until Cleaning completes"
    );
    drive_cleanup(INDEX_ID);
    assert!(
        rebuild_pool::bound_index().is_none(),
        "Cleaning completion releases the pool"
    );

    // Aborting path: entering `Aborting` from `Building` releases the pool immediately.
    fresh_store();
    seed_distinct(6);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_into_building(INDEX_ID);
    assert_eq!(rebuild_pool::bound_index(), Some(INDEX_ID));
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort");
    assert!(
        rebuild_pool::bound_index().is_none(),
        "abort entry releases the pool"
    );

    // Straight-to-Idle aborts release too, and a released region validates as absent.
    fresh_store();
    seed_distinct(6);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("early abort");
    assert!(rebuild_pool::bound_index().is_none());
}

#[test]
fn training_resume_from_durable_intermediate_state_is_deterministic() {
    /// Drives the rebuild from the current durable state to published centroids.
    fn finish_to_centroids() -> Vec<Vec<u8>> {
        loop {
            let status = admin_vector_rebuild_step(router(), INDEX_ID, 1).expect("stepped");
            match status.phase {
                VectorRebuildPhase::Sampling
                | VectorRebuildPhase::Training
                | VectorRebuildPhase::Building => {}
                VectorRebuildPhase::ReadyToPublish => break,
                other => panic!("unexpected phase {other:?}"),
            }
        }
        admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
        IVF_CENTROIDS.with_borrow(|m| {
            (0..3)
                .map(|p| {
                    m.get(&PartitionKey::new(INDEX_ID, TARGET_V, p))
                        .expect("centroid")
                })
                .collect()
        })
    }

    fresh_store();
    // A linear ramp is the k-means worst case for convergence, so Training runs several bounded
    // iterations before an intermediate durable state can be rewound to.
    seed_distinct(16);
    admin_start_vector_rebuild(router(), INDEX_ID, 3, 100).expect("start");
    // Enter Training and advance at least one bounded iteration so the centroid work area is
    // populated in the snapshot.
    loop {
        let status = admin_vector_rebuild_step(router(), INDEX_ID, 1).expect("stepped");
        match status.phase {
            VectorRebuildPhase::Sampling => {}
            VectorRebuildPhase::Training if status.training_iteration >= 1 => break,
            VectorRebuildPhase::Training => {}
            other => panic!("expected Training, reached {other:?}"),
        }
    }
    let before = admin_vector_rebuild_status(router(), INDEX_ID).expect("status");
    assert_eq!(before.phase, VectorRebuildPhase::Training);

    // Snapshot the exact durable starting point: lifecycle scalars + pool-region image.
    let snapshot_record = VectorRebuildStateRecord::Training {
        target_index_version: before.target_index_version,
        nlist: before.nlist,
        sample_limit: 100,
        iteration: before.training_iteration,
        pool_len: before.candidates_collected,
        levels: crate::records::LEVELS_FLAT,
        nlist_fine: 1,
    };
    let snapshot_pool = pool_image();

    let centroids_first = finish_to_centroids();

    // Rewind to the exact durable starting point and resume: same starting point must yield the
    // same result (training determinism parity).
    use ic_stable_structures::storable::Storable as _;
    VECTOR_REBUILD_STATE.with_borrow_mut(|m| {
        m.insert(
            INDEX_ID,
            crate::records::RawRebuildState(snapshot_record.clone().into_bytes()),
        )
    });
    let mem = rebuild_pool_memory();
    mem.write(0, &snapshot_pool);

    let after_restore = admin_vector_rebuild_status(router(), INDEX_ID).expect("status");
    assert_eq!(
        after_restore, before,
        "restore reproduces the same starting scalars"
    );

    let centroids_second = finish_to_centroids();
    assert_eq!(
        centroids_first, centroids_second,
        "resuming from the byte-identical durable intermediate state yields identical centroids"
    );
}

#[test]
fn sampling_dedup_excludes_duplicates_within_one_step() {
    fresh_store();
    // Five live subjects but only three distinct stored forms: both duplicates are sampled in the
    // SAME step, so their region slots are unwritten while dedup runs.
    for (v, value) in [(1u32, 0.0f32), (2, 10.0), (3, 5.0), (4, 0.0), (5, 10.0)] {
        vector_upsert(shard_canister(), &upsert_vec(v, 1, value)).expect("seed");
    }
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("sampling step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    assert_eq!(
        status.candidates_collected, 3,
        "same-step duplicates must not enter the candidate pool"
    );
}

#[test]
fn rebuild_step_and_cleanup_accept_oversized_caller_budget() {
    // A huge caller budget (`u32::MAX`) is clamped, never rejected: step/cleanup still succeed and
    // drive the rebuild to completion. (The exact `1..=MAX_REBUILD_STEP_WORK` clamp is unit-tested in
    // `rebuild::tests::clamp_step_work_bounds_caller_budget`.)
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let mut status =
        admin_vector_rebuild_step(router(), INDEX_ID, u32::MAX).expect("step accepts u32::MAX");
    while matches!(
        status.phase,
        VectorRebuildPhase::Sampling | VectorRebuildPhase::Training | VectorRebuildPhase::Building
    ) {
        status =
            admin_vector_rebuild_step(router(), INDEX_ID, u32::MAX).expect("step accepts u32::MAX");
    }
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    for _ in 0..100_000 {
        let status = admin_vector_rebuild_cleanup_step(router(), INDEX_ID, u32::MAX)
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
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let one_vector = STRIDE as u64;

    // First sampling step buffers exactly one vector -> one distinct candidate (the per-step byte
    // budget truncates work; the pool keeps filling across steps).
    let status = rebuild_step_with_budget(INDEX_ID, u32::MAX, one_vector).expect("sampling step");
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
        status = rebuild_step_with_budget(INDEX_ID, u32::MAX, one_vector).expect("bounded step");
    }
    assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);

    // The byte-bounded build is equivalent to an unbounded one: parity after publish holds.
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);
    for v in 1..=4u32 {
        let entry = subject_entry_for_test(INDEX_ID, subject(v)).unwrap();
        let slot = entry.slot.expect("collapsed live slot");
        assert_eq!(slot.index_version, TARGET_V as u32);
        assert_eq!(entry.shadow_slot, None);
    }
}

#[test]
fn rebuild_already_active_is_rejected() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).unwrap_err(),
        VectorCanisterError::RebuildAlreadyActive
    );
}

#[test]
fn rebuild_sampling_fails_on_insufficient_distinct_vectors_then_recovers() {
    fresh_store();
    // Three live subjects but only ONE distinct value: cannot form 2 distinct centroids.
    vector_upsert(shard_canister(), &upsert_vec(1, 1, 5.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(2, 1, 5.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(3, 1, 5.0)).unwrap();
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = drive_steps(INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Failed);

    // Failed recovers to Idle via abort (O(1), nothing persisted), then a new rebuild can start.
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort failed");
    assert_eq!(
        admin_vector_rebuild_status(router(), INDEX_ID)
            .unwrap()
            .phase,
        VectorRebuildPhase::Idle
    );
    // Add two distinct values so a fresh rebuild can now sample 2 centroids.
    vector_upsert(shard_canister(), &upsert_vec(10, 1, 0.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(11, 1, 1.0)).unwrap();
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("restart after recovery");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
}

#[test]
fn publish_rejected_before_ready() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    // Still Sampling.
    assert_eq!(
        admin_publish_vector_rebuild(router(), INDEX_ID).unwrap_err(),
        VectorCanisterError::RebuildNotReadyToPublish
    );
}

#[test]
fn publish_switches_to_partition_search_with_exact_parity() {
    fresh_store();
    seed_distinct(4);
    let before = vector_search(&search_value(1.5, 10)).expect("exact");

    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");

    let def = def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.active_index_version, TARGET_V);
    assert_eq!(def.nlist, 2);

    // Default search now runs the partition scan. The default `eps_query = 0.0` is a recall knob
    // (nearest partition only), so exact parity is asserted at the full scan (`eps_query = INFINITY`),
    // which is independent of the candidate-pool iteration order.
    let after = vector_search_tuned(&search_value(1.5, 10), tuned(f32::INFINITY))
        .expect("partition full scan");
    assert_eq!(after.hits, before.hits);
}

#[test]
fn upsert_during_building_survives_publish() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    // Reach Building (centroids written), then insert a new subject mid-rebuild.
    let status = drive_into_building(INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Building);
    vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0)).expect("dual-write upsert");
    let entry = subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    assert!(
        entry.shadow_slot.is_some(),
        "dual-write created a shadow slot"
    );

    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    let after = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(
        after.hits.iter().any(|h| h.subject == subject(99)),
        "subject inserted during Building is searchable after publish"
    );
}

#[test]
fn detach_during_building_purges_active_and_shadow_slots() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_into_building(INDEX_ID);
    vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0)).expect("dual-write upsert");

    let entry = subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    let active_slot = entry.slot.expect("active slot");
    let shadow_slot = entry.shadow_slot.expect("shadow slot");
    assert_ne!(
        active_slot, shadow_slot,
        "test requires distinct physical rows"
    );

    let result = detach_shard_step_for_test(ShardId::new(0), None, 20_000).expect("detach step");
    assert!(result.done);
    assert!(
        read_slot_bytes(INDEX_ID, active_slot).is_none(),
        "detach tombstones the active row"
    );
    assert!(
        read_slot_bytes(INDEX_ID, shadow_slot).is_none(),
        "detach tombstones the shadow row"
    );
    assert!(
        subject_entry_for_test(INDEX_ID, subject(99)).is_none(),
        "detach removes the subject row"
    );

    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    let after_publish = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(
        after_publish.hits.is_empty(),
        "all detached-shard subjects stay excluded after publish"
    );

    drive_cleanup(INDEX_ID);
    let after_cleanup = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(
        after_cleanup.hits.is_empty(),
        "all detached-shard subjects stay excluded after cleanup"
    );
}

#[test]
fn detach_generation_fences_reattach_and_preserves_d2_active_shadow_rows_from_d1() {
    fresh_store();
    seed_distinct(4);

    let d1_first = detach_shard_step_for_test(ShardId::new(0), None, 1).expect("begin D1");
    let d1_cursor = d1_first.next.expect("D1 remains bounded");
    let d1_generation = d1_cursor
        .detach_generation
        .expect("fresh D1 cursor has a generation");
    assert_eq!(
        admin_attach_shard_canister(
            router(),
            GraphId::from_raw(1),
            ShardId::new(0),
            shard_canister(),
        ),
        Err(VectorCanisterError::DetachInProgress)
    );

    let d1_restart = detach_shard_step_for_test(ShardId::new(0), None, 1)
        .expect("restart D1 from the beginning");
    assert_eq!(
        d1_restart
            .next
            .as_ref()
            .and_then(|cursor| cursor.detach_generation),
        Some(d1_generation),
        "resume=None reuses the active D1 generation"
    );
    let mut resume = d1_restart.next;
    let mut steps = 0u32;
    while let Some(cursor) = resume {
        resume = detach_shard_step_for_test(ShardId::new(0), Some(cursor), 20_000)
            .expect("complete D1")
            .next;
        steps += 1;
        assert!(steps < 100, "D1 did not converge");
    }
    assert_eq!(
        detach_shard_step_for_test(ShardId::new(0), Some(d1_cursor.clone()), 1),
        Err(VectorCanisterError::LegacyOrStaleDetachCursor),
        "a completed D1 rejects its last issued cursor"
    );

    admin_attach_shard_canister(
        router(),
        GraphId::from_raw(1),
        ShardId::new(0),
        shard_canister(),
    )
    .expect("reattach after D1 completion");
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start rebuild");
    assert_eq!(
        drive_into_building(INDEX_ID).phase,
        VectorRebuildPhase::Building
    );
    for vertex_id in 100..120 {
        vector_upsert(
            shard_canister(),
            &upsert_vec(vertex_id, 1, vertex_id as f32),
        )
        .expect("dual-write post-reattach subject");
    }

    let d2 = detach_shard_step_for_test(ShardId::new(0), None, 1).expect("begin D2");
    let d2_cursor = d2.next.expect("D2 remains bounded");
    let d2_generation = d2_cursor
        .detach_generation
        .expect("fresh D2 cursor has a generation");
    assert!(d2_generation > d1_generation);
    assert_eq!(
        admin_attach_shard_canister(
            router(),
            GraphId::from_raw(1),
            ShardId::new(0),
            shard_canister(),
        ),
        Err(VectorCanisterError::DetachInProgress)
    );

    let (survivor_id, before_entry) = (100..120)
        .find_map(|vertex_id| {
            let entry = subject_entry_for_test(INDEX_ID, subject(vertex_id))?;
            (entry.slot.is_some() && entry.shadow_slot.is_some() && entry.slot != entry.shadow_slot)
                .then_some((vertex_id, entry))
        })
        .expect("a budget-1 D2 leaves a dual-written subject live");
    let active_slot = before_entry.slot.expect("survivor active slot");
    let shadow_slot = before_entry.shadow_slot.expect("survivor shadow slot");
    let before_active_bytes = read_slot_bytes(INDEX_ID, active_slot);
    let before_shadow_bytes = read_slot_bytes(INDEX_ID, shadow_slot);
    let before_config = OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone());
    let mut stale_d1 = d1_cursor;
    stale_d1.resume_key = vec![0xff];

    assert_eq!(
        detach_shard_step_for_test(ShardId::new(0), Some(stale_d1), 1),
        Err(VectorCanisterError::LegacyOrStaleDetachCursor)
    );
    assert_eq!(
        OWNERSHIP_CONFIG.with_borrow(|cell| cell.get().clone()),
        before_config,
        "stale D1 does not change the active D2 lifecycle"
    );
    assert_eq!(
        subject_entry_for_test(INDEX_ID, subject(survivor_id)),
        Some(before_entry),
        "stale D1 does not remove the reattached subject"
    );
    assert_eq!(
        read_slot_bytes(INDEX_ID, active_slot),
        before_active_bytes,
        "stale D1 leaves the active row live"
    );
    assert_eq!(
        read_slot_bytes(INDEX_ID, shadow_slot),
        before_shadow_bytes,
        "stale D1 leaves the distinct shadow row live"
    );
}

#[test]
fn dual_write_shadow_append_failure_rolls_back_insert() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_into_building(INDEX_ID); // -> Building (dual-write)

    let live_before = partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Inject a slab `grow` failure for the shadow append: the active append (1st) succeeds, the
    // shadow append (2nd) fails. This is the StableGrowFailed branch normal unit tests cannot reach.
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);

    // Insert path commits the id/subject maps only after both appends succeed, so a new subject must
    // leave no map entry behind.
    assert!(
        subject_entry_for_test(INDEX_ID, subject(99)).is_none(),
        "no subject map entry created on rollback"
    );
    // The active row was appended then tombstoned, so live accounting is restored (not a live-counted
    // orphan polluting partition health).
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "active live_len restored after rollback"
    );
}

#[test]
fn dual_write_shadow_append_failure_rolls_back_update() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_into_building(INDEX_ID); // -> Building (dual-write)

    let before = subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    let old_slot = before.slot.expect("seeded subject is live");
    let live_before = partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Inject a slab `grow` failure for the shadow append (active append succeeds first).
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = vector_upsert(shard_canister(), &upsert_vec(1, 2, 0.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);

    // The subject clock still points at the original live slot — no partial commit to a
    // tombstoned/new slot.
    let after = subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    assert_eq!(after.slot, Some(old_slot), "old slot stays live");
    assert_eq!(after.shadow_slot, None, "no shadow recorded");
    // The new active row was appended then tombstoned: net live_len unchanged.
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "active live_len restored after rollback"
    );
}

#[test]
fn newer_stamp_upsert_commit_failure_keeps_old_slot_live() {
    // GAP-2026-08-07-001 regression: a newer-stamp upsert whose subject-map commit fails must leave
    // the old slot live (it is tombstoned only after a successful commit), not pointing at a
    // tombstoned row.
    fresh_store();
    seed_distinct(4);
    let before = subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    let old_slot = before.slot.expect("seeded subject is live");
    let live_before = partition_head_for_test(INDEX_ID, 1).unwrap().live_len;

    // Force the subject-map commit to fail after the new active row is appended.
    crate::facade::store::mutation::arm_subject_insert_failure(0);
    let err = vector_upsert(shard_canister(), &upsert_vec(1, 2, 0.0))
        .expect_err("subject-map commit failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);

    // The old subject entry and its live slot are preserved — the old slot must NOT be tombstoned.
    let after = subject_entry_for_test(INDEX_ID, subject(1)).unwrap();
    assert_eq!(after.slot, Some(old_slot), "old slot stays live");
    assert!(
        read_slot_bytes(INDEX_ID, old_slot).is_some(),
        "old row remains live and searchable"
    );
    // The appended-then-tombstoned new row restores live accounting.
    assert_eq!(
        partition_head_for_test(INDEX_ID, 1).unwrap().live_len,
        live_before,
        "live_len restored after commit rollback"
    );
}

#[test]
fn remove_during_building_does_not_resurrect_after_publish() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_into_building(INDEX_ID); // -> Building
    // Remove subject 4 while dual-writing.
    vector_remove(shard_canister(), &remove_op(4, 2)).expect("remove during building");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    let after = vector_search(&search_value(3.0, 10)).expect("search");
    assert!(
        !after.hits.iter().any(|h| h.subject == subject(4)),
        "removed subject must not resurrect after publish"
    );
}

#[test]
fn mutation_during_cleaning_collapses_on_touch() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    // Now in Cleaning; subject 2 is not yet collapsed (slot @ old version, shadow @ target).
    let pre = subject_entry_for_test(INDEX_ID, subject(2)).unwrap();
    assert_eq!(pre.slot.unwrap().index_version, 1);
    assert_eq!(pre.shadow_slot.unwrap().index_version, TARGET_V as u32);

    // Touch subject 2: a newer-version upsert must operate on the target version and collapse it.
    vector_upsert(shard_canister(), &upsert_vec(2, 2, 1.0)).expect("upsert during cleaning");
    let post = subject_entry_for_test(INDEX_ID, subject(2)).unwrap();
    assert_eq!(
        post.slot.unwrap().index_version,
        TARGET_V as u32,
        "collapsed to target"
    );
    assert_eq!(post.shadow_slot, None, "shadow cleared on touch");

    // Cleanup finishes and search stays correct.
    drive_cleanup(INDEX_ID);
    let after = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(after.hits.iter().any(|h| h.subject == subject(2)));
}

#[test]
fn cleanup_is_bounded_and_resumable_to_idle() {
    fresh_store();
    seed_distinct(6);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    let steps = drive_cleanup(INDEX_ID);
    assert!(steps > 1, "teardown spanned multiple bounded steps");
    // Old-version page meta is gone; the index is fully on the target version.
    let old_pages = PAGE_STORE.with_borrow(|s| s.version_page_count(INDEX_ID, 1));
    assert_eq!(old_pages, 0, "old-version page meta dropped");
    let after = vector_search_tuned(&search_value(2.0, 10), tuned(f32::INFINITY)).expect("search");
    assert_eq!(after.hits.len(), 6);
}

#[test]
fn abort_during_building_is_bounded_and_leaves_active_unchanged() {
    fresh_store();
    seed_distinct(4);
    let before = vector_search(&search_value(1.5, 10)).expect("exact");
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = drive_into_building(INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Building);
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort");
    drive_cleanup(INDEX_ID);

    // Active version unchanged; shadow pages and centroids gone.
    let def = def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.active_index_version, 1);
    assert_eq!(def.nlist, 1);
    assert_eq!(target_centroid_count(INDEX_ID, TARGET_V, 2), 0);
    let after = vector_search(&search_value(1.5, 10)).expect("exact");
    assert_eq!(after.hits, before.hits, "active search unchanged by abort");
    // A fresh rebuild can start again.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("restart after abort");
}

#[test]
fn abort_from_sampling_is_immediate_idle() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort from sampling");
    assert_eq!(
        admin_vector_rebuild_status(router(), INDEX_ID)
            .unwrap()
            .phase,
        VectorRebuildPhase::Idle
    );
}

#[test]
fn post_publish_nlist_gt_1_upsert_assigns_nearest_partition() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);

    // Index is now published nlist=2 with no active rebuild. A new upsert must assign by centroid.
    vector_upsert(shard_canister(), &upsert_vec(50, 1, 0.0)).expect("post-publish upsert");
    let entry = subject_entry_for_test(INDEX_ID, subject(50)).unwrap();
    let slot = entry.slot.unwrap();
    assert_eq!(slot.index_version, TARGET_V as u32);
    // Furthest-point seeding on values {0..3} with nlist=2 gives centroids [2.5 (p0), 0.5 (p1)], so
    // a value-0 upsert lands in the nearest (0.5) partition, p1 — not the degenerate partition 0.
    assert_eq!(
        slot.partition_id, 1,
        "value 0 lands in the nearest-centroid partition"
    );
    let after = vector_search(&search_nonzero(0.0, 10)).expect("search");
    assert!(after.hits.iter().any(|h| h.subject == subject(50)));
}

#[test]
fn second_rebuild_from_partitioned_active() {
    fresh_store();
    seed_distinct(6);
    // First rebuild to nlist=2 and fully publish + clean.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start 1");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish 1");
    drive_cleanup(INDEX_ID);
    // Full-scan parity baseline (eps_query = INFINITY), independent of the candidate-pool order.
    let before =
        vector_search_tuned(&search_value(2.5, 10), tuned(f32::INFINITY)).expect("full scan");

    // Second rebuild to nlist=3 from the partitioned (nlist=2) active version.
    admin_start_vector_rebuild(router(), INDEX_ID, 3, 100).expect("start 2");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish 2");
    let def = def_for_test(INDEX_ID).unwrap();
    assert_eq!(def.active_index_version, 3);
    assert_eq!(def.nlist, 3);
    drive_cleanup(INDEX_ID);

    // Parity to the pre-second-rebuild result at nprobe = nlist (full scan).
    let after = vector_search_tuned(&search_value(2.5, 10), tuned(f32::INFINITY)).expect("tuned");
    assert_eq!(after.hits, before.hits);
}

/// Slice 8 scalar block bound: on a rebuilt two-partition index, a full-walk (ε₂ = INF) L2 query
/// whose nearest cluster fills the heap must **skip** the far partition's page before any slab
/// read, while the returned top-k stays exactly the true nearest subjects.
#[test]
fn slice8_block_bound_skips_far_page_and_full_walk_stays_exact() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);

    // Values {0,1,2,3}; furthest-point seeding puts the high centroid on partition 0, so a query
    // at 3.0 fills the heap from partition 0 (scanned first) and arms the gate for partition 1.
    crate::facade::store::reset_page_skip_stats();
    let result =
        vector_search_tuned(&search_value(3.0, 2), tuned(f32::INFINITY)).expect("gated full walk");
    let (skipped, considered) = crate::facade::store::page_skip_stats();
    assert!(
        skipped >= 1 && considered >= 2,
        "the far partition's page must be skipped before any slab read (skipped={skipped}, \
         considered={considered})"
    );
    println!(
        "[slice8] deterministic clustered-fixture page skip rate: {skipped}/{considered} = {:.1}%",
        100.0 * skipped as f64 / considered as f64
    );

    // The gated walk stays exactly correct: seed_distinct maps subjects 1..4 to values 0..3, so
    // the two nearest subjects to value 3.0 are 4 (exact hit) then 3.
    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].subject, subject(4));
    assert_eq!(result.hits[1].subject, subject(3));

    // Cosine non-regression guard within the same fixture shape: an ungated metric still answers.
    // (Cosine rows are unit-normalized at write; the bound never gates their pages.)
    let _ = vector_search_tuned(&search_value(3.0, 1), tuned(f32::INFINITY));
}

#[test]
fn publish_succeeds_with_an_empty_partition() {
    fresh_store();
    // Subjects: values 0, 10, 5, 0, 10. The val-5 subject (3) becomes one target centroid's source
    // but is removed during Building, leaving that centroid's partition empty.
    vector_upsert(shard_canister(), &upsert_vec(1, 1, 0.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(2, 1, 10.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(3, 1, 5.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(4, 1, 0.0)).unwrap();
    vector_upsert(shard_canister(), &upsert_vec(5, 1, 10.0)).unwrap();

    admin_start_vector_rebuild(router(), INDEX_ID, 3, 100).expect("start");
    // Sampling collects the three distinct candidates; Training writes the three centroids and enters
    // Building (each distinct candidate seeds and stays its own centroid).
    let status = drive_into_building(INDEX_ID);
    assert_eq!(status.phase, VectorRebuildPhase::Building);
    // Remove the val-5 subject so no live vector is nearest to the 5.0 centroid.
    vector_remove(shard_canister(), &remove_op(3, 2)).expect("remove val-5");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );

    // Physical subject-map scan order is not a partition-id contract. Locate the specific centroid
    // whose only source was removed, then prove its partition has no materialized head while the
    // remaining centroid partitions retain their two live rows each.
    let removed_centroid = vec_bytes(5.0);
    let empty_partition = IVF_CENTROIDS
        .with_borrow(|centroids| {
            (0..3).find(|&partition| {
                centroids
                    .get(&PartitionKey::new(INDEX_ID, TARGET_V, partition))
                    .is_some_and(|centroid| centroid.as_slice() == removed_centroid.as_slice())
            })
        })
        .expect("removed vector remains a target centroid");
    let empty_head = VECTOR_PARTITION_HEADS
        .with_borrow(|heads| heads.get(&PartitionKey::new(INDEX_ID, TARGET_V, empty_partition)))
        .expect("partition head get");
    assert!(empty_head.is_none(), "empty partition materializes no head");
    for partition in (0..3).filter(|partition| *partition != empty_partition) {
        let head = VECTOR_PARTITION_HEADS
            .with_borrow(|heads| heads.get(&PartitionKey::new(INDEX_ID, TARGET_V, partition)))
            .expect("partition head get")
            .expect("non-removed centroid partition materializes a head");
        let PartitionHeadRecord::Head(head) = head else {
            panic!("expected a head record under the leaf key")
        };
        assert_eq!(head.live_len, 2, "remaining partition keeps two live rows");
    }

    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish tolerates empty partition");
    // Full-scan search returns the four remaining live subjects.
    let after =
        vector_search_tuned(&search_nonzero(0.0, 10), tuned(f32::INFINITY)).expect("search");
    assert_eq!(after.hits.len(), 4);
}

// --- ADR 0031 Slice 8: bounded training quality + partition health ---

#[test]
fn sampling_collects_more_than_nlist_candidates() {
    fresh_store();
    // Eight distinct live vectors but only nlist=2: sampling collects the whole bounded pool, not
    // just two, before entering Training (ADR 0031 Slice 8, P3).
    seed_distinct(8);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    // One large sampling step exhausts the (8-subject) range -> Training with all 8 candidates.
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("sampling step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    assert_eq!(
        status.candidates_collected, 8,
        "sampling collects the whole distinct pool, not just nlist"
    );
    assert_eq!(status.training_iteration, 0);
}

#[test]
fn training_produces_nlist_valid_centroids() {
    fresh_store();
    seed_distinct(8);
    admin_start_vector_rebuild(router(), INDEX_ID, 3, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
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
        fresh_store();
        seed_distinct(8);
        admin_start_vector_rebuild(router(), INDEX_ID, 3, 100).expect("start");
        assert_eq!(
            drive_steps(INDEX_ID).phase,
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
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    // One step completes Sampling and enters Training (iteration 0).
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("step");
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
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort from training");
    assert_eq!(
        admin_vector_rebuild_status(router(), INDEX_ID)
            .unwrap()
            .phase,
        VectorRebuildPhase::Idle
    );
    // O(1) recovery: a fresh rebuild can start again.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("restart after abort");
}

#[test]
fn upsert_during_training_is_active_only_then_shadowed_by_building() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("step");
    assert_eq!(status.phase, VectorRebuildPhase::Training);
    // A new subject upserted during Training is active-only (no shadow slot yet).
    vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0)).expect("active-only upsert");
    let entry = subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    assert!(
        entry.shadow_slot.is_none(),
        "mutation during Training is active-only"
    );
    // Building walks every live subject and shadows it; publish makes it searchable.
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    let entry = subject_entry_for_test(INDEX_ID, subject(99)).unwrap();
    assert!(
        entry.shadow_slot.is_some(),
        "Building shadows the Training-era mutation"
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    let after = vector_search(&search_value(1.0, 10)).expect("search");
    assert!(after.hits.iter().any(|h| h.subject == subject(99)));
}

#[test]
fn partition_health_reports_skew_and_empty_partitions() {
    fresh_store();
    // Three centroids [0, 10, 20]; populate only the first two (3 rows near 0, 1 row near 10), so
    // partition 2 stays empty and partition 0 is the skew peak.
    let centroids = vec![cvec(0.0), cvec(10.0), cvec(20.0)];
    let vectors = vec![
        (subject(1), cvec(0.0)),
        (subject(2), cvec(0.1)),
        (subject(3), cvec(0.2)),
        (subject(4), cvec(10.0)),
    ];
    seed_ivf_for_test(INDEX_ID, VectorEncoding::F32, DIMS, &centroids, &vectors);

    let health = admin_vector_partition_health(router(), INDEX_ID).expect("health");
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
    fresh_store();
    assert_eq!(
        admin_vector_partition_health(router(), 999).unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
}

#[test]
fn slab_stats_dual_write_rollback_keeps_live_and_counts_tombstone() {
    fresh_store();
    seed_distinct(4);
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    drive_into_building(INDEX_ID); // -> Building (dual-write)

    let before = admin_vector_slab_stats(router(), Some(INDEX_ID)).expect("stats");
    // Force the shadow append to fail; the active append succeeds first and is then rolled back
    // (tombstoned) by vector_upsert.
    crate::facade::stable::page_store::arm_append_failure(1);
    let err = vector_upsert(shard_canister(), &upsert_vec(99, 1, 1.0))
        .expect_err("shadow grow failure propagates");
    assert_eq!(err, VectorCanisterError::StableGrowFailed);
    let after = admin_vector_slab_stats(router(), Some(INDEX_ID)).expect("stats");

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
    fresh_store();
    assert_eq!(
        admin_vector_slab_stats(shard_canister(), None).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

#[test]
fn slab_compact_endpoints_reject_non_router_caller() {
    fresh_store();
    assert_eq!(
        admin_start_vector_slab_compact(shard_canister()).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        admin_vector_slab_compact_step(shard_canister(), 10, 4096).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        admin_vector_slab_compact_status(shard_canister()).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

#[test]
fn slab_compact_driver_runs_to_idle_through_public_api() {
    use gleaph_graph_kernel::vector_index::VectorSlabCompactionPhase;
    fresh_store();
    seed_distinct(8);

    // Idle status before any compaction.
    let idle = admin_vector_slab_compact_status(router()).expect("idle status");
    assert_eq!(idle.phase, VectorSlabCompactionPhase::Idle);
    assert_eq!(
        admin_vector_slab_compact_step(router(), 10, 4096).unwrap_err(),
        VectorCanisterError::NoActiveCompaction
    );

    // Start snapshots the range; a second start is rejected while active.
    admin_start_vector_slab_compact(router()).expect("start");
    let active = admin_vector_slab_compact_status(router()).expect("active status");
    assert_eq!(active.phase, VectorSlabCompactionPhase::Compacting);
    assert_eq!(
        admin_start_vector_slab_compact(router()).unwrap_err(),
        VectorCanisterError::CompactionAlreadyActive
    );

    // Drive to finalize: the fixture is all-live, so the tail is unchanged and the phase clears.
    loop {
        let status = admin_vector_slab_compact_step(router(), u32::MAX, u64::MAX).expect("step");
        if status.phase == VectorSlabCompactionPhase::Idle {
            break;
        }
    }
    let done = admin_vector_slab_compact_status(router()).expect("done status");
    assert_eq!(done.phase, VectorSlabCompactionPhase::Idle);
    assert_eq!(done.write_cursor, 0, "Idle zeroes the cursors");

    // The state cleared: a fresh compaction can start again.
    admin_start_vector_slab_compact(router()).expect("restart after finalize");
}

#[test]
fn slab_stats_step_rejects_non_router_caller() {
    fresh_store();
    assert_eq!(
        admin_vector_slab_stats_step(shard_canister(), None, 10, None).unwrap_err(),
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
fn attested_page(total_rows: u64, tombstoned_rows: u64) -> VectorPartitionPageHealth {
    let def = def_for_test(INDEX_ID).expect("def");
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
    fresh_store();
    seed_distinct(4); // 4 live rows, version 1, degenerate partition 0
    // Re-upsert subject 1 at a newer embedding_version: tombstones the old row, appends a new one.
    vector_upsert(shard_canister(), &upsert_vec(1, 2, 5.0)).expect("re-upsert");

    let mut merged = VectorPartitionPageHealth::default();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let step = admin_vector_partition_health_step(router(), INDEX_ID, cursor.clone(), 1)
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
    fresh_store();
    seed_distinct(2);
    assert_eq!(
        admin_vector_partition_health_step(shard_canister(), INDEX_ID, None, 10).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        admin_vector_partition_health_step(router(), 999, None, 10).unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
}

#[test]
fn trigger_healthy_does_not_start_rebuild() {
    fresh_store();
    seed_distinct(4);
    let rec = admin_start_vector_rebuild_if_recommended(
        router(),
        INDEX_ID,
        attested_page(1_000, 100), // 10% < recommended
        trigger_policy(),
        Some(2),
        100,
    )
    .expect("trigger");
    assert_eq!(rec, VectorMaintenanceRecommendation::Healthy);
    assert_eq!(
        admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase,
        VectorRebuildPhase::Idle,
        "a healthy report must not start a rebuild"
    );
}

#[test]
fn trigger_required_starts_rebuild_at_target_nlist() {
    fresh_store();
    seed_distinct(4);
    let rec = admin_start_vector_rebuild_if_recommended(
        router(),
        INDEX_ID,
        attested_page(1_000, 600), // 60% >= required
        trigger_policy(),
        Some(2),
        100,
    )
    .expect("trigger");
    assert_eq!(rec, VectorMaintenanceRecommendation::RebuildRequired);
    let status = admin_vector_rebuild_status(router(), INDEX_ID).expect("status");
    assert_eq!(status.phase, VectorRebuildPhase::Sampling);
    assert_eq!(status.target_index_version, TARGET_V);
}

#[test]
fn trigger_recommended_starts_rebuild() {
    fresh_store();
    seed_distinct(4);
    let rec = admin_start_vector_rebuild_if_recommended(
        router(),
        INDEX_ID,
        attested_page(1_000, 300), // 30%: recommended band
        trigger_policy(),
        Some(2),
        100,
    )
    .expect("trigger");
    assert_eq!(rec, VectorMaintenanceRecommendation::RebuildRecommended);
    assert_eq!(
        admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase,
        VectorRebuildPhase::Sampling
    );
}

#[test]
fn trigger_degenerate_nlist_without_target_is_rejected() {
    fresh_store();
    seed_distinct(4); // def.nlist == 1
    assert_eq!(
        admin_start_vector_rebuild_if_recommended(
            router(),
            INDEX_ID,
            attested_page(1_000, 600),
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
    fresh_store();
    seed_distinct(4);
    // Wrong active version on the page health is rejected (the skew summary is recomputed
    // server-side, so it has no stale surface of its own).
    let mut stale_page = attested_page(1_000, 600);
    stale_page.index_version = 999;
    assert_eq!(
        admin_start_vector_rebuild_if_recommended(
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
    let mut foreign_page = attested_page(1_000, 600);
    foreign_page.index_id = INDEX_ID + 1;
    assert_eq!(
        admin_start_vector_rebuild_if_recommended(
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
    fresh_store();
    seed_distinct(4);
    let mut bad = trigger_policy();
    bad.recommended_tombstone_ratio_bps = 6_000;
    bad.required_tombstone_ratio_bps = 5_000;
    assert_eq!(
        admin_start_vector_rebuild_if_recommended(
            router(),
            INDEX_ID,
            attested_page(1_000, 600),
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
    fresh_store();
    seed_distinct(4);
    assert_eq!(
        admin_start_vector_rebuild_if_recommended(
            shard_canister(),
            INDEX_ID,
            attested_page(1_000, 600),
            trigger_policy(),
            Some(2),
            100,
        )
        .unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    fresh_store();
    assert_eq!(
        admin_start_vector_rebuild_if_recommended(
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

use std::sync::Arc;

use super::centroid_cache::{lookup, warm_all, warm_index};

/// Seeds a ready `nlist`-partition centroid set + matching def directly (no rows/pages): enough
/// state for the cache warm paths, which read only defs, centroid metadata, and `IVF_CENTROIDS`.
fn seed_ready_centroids_only(index_id: u32, nlist: u32, dims: u16) {
    use crate::facade::stable::IVF_CENTROID_META;
    use crate::records::{IvfCentroidMeta, VectorIndexDef};
    use gleaph_graph_kernel::vector_index::VectorIndexKind;

    let value = vec![0.25f32; dims as usize];
    IVF_CENTROIDS.with_borrow_mut(|m| {
        for p in 0..nlist {
            m.insert(
                PartitionKey::new(index_id, INITIAL_INDEX_VERSION, p),
                super::search::encode_f32(&value),
            );
        }
    });
    IVF_CENTROID_META.with_borrow_mut(|meta| {
        meta.insert(
            index_id,
            IvfCentroidMeta {
                centroid_ready: true,
                trained_index_version: INITIAL_INDEX_VERSION,
            },
        )
    });
    definition_store::insert(
        index_id,
        VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding: VectorEncoding::F32,
            dims,
            metric: VectorMetric::L2Squared,
            nlist,
            active_index_version: INITIAL_INDEX_VERSION,
            stride_bytes: u32::from(dims) * 4,
            pad_stride_bytes: u32::from(dims) * 4,
            meta_stride_bytes: 4,
            run_capacity: 1,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            slots_per_page: 1,
            levels: crate::records::LEVELS_FLAT,
            nlist_fine: 1,
            code_tier: false,
            code_stride_bytes: 0,
            rotation_seed: 0,
            eps_query_bps: 0,
            eps_fine_bps: 0,
        },
    )
    .expect("seed def");
}

#[test]
fn centroid_cache_lookup_shares_one_allocation_across_calls() {
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    warm_index(INDEX_ID);
    let first = lookup(INDEX_ID, INITIAL_INDEX_VERSION, 2, DIMS).expect("warmed");
    let second = lookup(INDEX_ID, INITIAL_INDEX_VERSION, 2, DIMS).expect("warmed");
    assert!(
        Arc::ptr_eq(&first, &second),
        "lookup must hand out the same allocation (an Arc handle clone), not copied payloads"
    );
    assert!(
        lookup(INDEX_ID, INITIAL_INDEX_VERSION + 1, 2, DIMS).is_none(),
        "a different generation must miss even while an entry is resident"
    );
}

#[test]
fn warm_all_restores_ready_indexes_and_evicts_over_budget() {
    fresh_store();
    // Two ~4.2 MiB sets cannot share the 8 MiB cap; a third small ready set must survive alongside
    // the newest large one.
    seed_ready_centroids_only(1, 1024, 1024);
    seed_ready_centroids_only(2, 1024, 1024);
    seed_ivf_for_test(
        3,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );

    // The single oversized-but-under-cap set is resident alone...
    let solo = warm_index(1);
    assert_eq!(solo.entries, 1);

    // ...and warming every ready index evicts the lowest-id large set to fit the budget.
    warm_all();
    let status = admin_vector_centroid_cache_status(router()).expect("status");
    assert_eq!(status.max_bytes, 8 * 1024 * 1024);
    assert_eq!(
        status.entries, 2,
        "the newest large set and the small set are resident; the lowest-id set was evicted"
    );
    assert!(lookup(1, INITIAL_INDEX_VERSION, 1024, 1024).is_none());
    assert!(lookup(2, INITIAL_INDEX_VERSION, 1024, 1024).is_some());
    assert!(lookup(3, INITIAL_INDEX_VERSION, 2, DIMS).is_some());
    assert!(status.bytes <= status.max_bytes);
}

#[test]
fn warm_index_skips_unknown_and_degenerate_without_error() {
    fresh_store();
    assert_eq!(warm_index(INDEX_ID).entries, 0, "unknown index stays cold");
    seed_distinct(4); // degenerate nlist = 1
    assert_eq!(
        warm_index(INDEX_ID).entries,
        0,
        "a degenerate index has no centroid set to cache"
    );
}

#[test]
fn upsert_populates_active_generation_into_cache() {
    fresh_store();
    seed_distinct(6);
    // Publish one generation so steady-state upserts take the nlist > 1 path.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);
    assert_eq!(
        admin_vector_centroid_cache_status(router())
            .expect("status")
            .entries,
        0,
        "publish invalidated the rebuild window; nothing is warm yet"
    );

    // The first post-publish upsert reads the active generation through the cache and leaves it
    // resident (the update commits the heap write).
    vector_upsert(shard_canister(), &upsert_op(9, 100, 0xAA)).expect("upsert");
    let status = admin_vector_centroid_cache_status(router()).expect("status");
    assert_eq!(
        status.entries, 1,
        "update-path centroid assignment populated the cache"
    );
    assert!(
        lookup(INDEX_ID, TARGET_V, 2, DIMS).is_some(),
        "the cached entry is the newly active generation"
    );
}

#[test]
fn shadow_rebuild_reads_leave_the_cache_cold() {
    fresh_store();
    seed_distinct(6);
    // First publish so the second rebuild runs against a partitioned active generation.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);

    // Second rebuild: Building assigns rows against the SHADOW generation's centroids. Those reads
    // target a non-active version, so the version-scope rule must keep them out of the cache.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("rebuild 2");
    assert_eq!(
        drive_into_building(INDEX_ID).phase,
        VectorRebuildPhase::Building
    );
    assert!(
        lookup(INDEX_ID, TARGET_V + 1, 2, DIMS).is_none(),
        "shadow-generation centroids must not be cached"
    );
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    assert_eq!(
        admin_vector_centroid_cache_status(router())
            .expect("status")
            .entries,
        0,
        "no shadow/old-generation read may pollute the cache"
    );

    // Publish still invalidates (defensively): the cache ends the flow empty.
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish 2");
    assert_eq!(
        admin_vector_centroid_cache_status(router())
            .expect("status")
            .entries,
        0
    );
}

#[test]
fn centroid_cache_search_parity_cold_vs_warm() {
    fresh_store();
    seed_ivf_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        DIMS,
        &two_clusters(),
        &clustered_vectors(),
    );
    let cold =
        vector_search_tuned(&search_value(0.5, 10), tuned(f32::INFINITY)).expect("cold scan");
    warm_index(INDEX_ID);
    let warm =
        vector_search_tuned(&search_value(0.5, 10), tuned(f32::INFINITY)).expect("warm scan");
    let cold_hits: Vec<_> = cold.hits.iter().map(|h| (h.subject, h.distance)).collect();
    let warm_hits: Vec<_> = warm.hits.iter().map(|h| (h.subject, h.distance)).collect();
    assert_eq!(cold_hits, warm_hits, "warm cache yields identical results");
}

#[test]
fn centroid_cache_publish_invalidates_warmed_entry() {
    fresh_store();
    seed_distinct(6); // degenerate nlist = 1, version 1
    // First rebuild to nlist = 2 and publish so the active set is partitioned + ready.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);
    assert_eq!(warm_index(INDEX_ID).entries, 1);
    // A second rebuild + publish flips the active generation and must drop the warmed entry.
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("start 2");
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish 2");
    assert_eq!(
        admin_vector_centroid_cache_status(router())
            .expect("status")
            .entries,
        0,
        "publishing a new generation invalidates the warmed centroid entry"
    );
}

#[test]
fn centroid_cache_status_endpoint_rejects_non_router() {
    fresh_store();
    assert_eq!(
        admin_vector_centroid_cache_status(shard_canister()).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

// --- ADR 0031 Slice 10: maintenance execution state machine ---

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
        target_fine_nlist: None,
        code_tier: None,
        eps_query_bps: None,
        eps_fine_bps: None,
    }
}

/// Seeds `live` distinct live rows then creates `tombstones` extra tombstoned rows by re-upserting
/// subject 1 at increasing embedding_versions (each re-upsert tombstones the prior row).
fn seed_live_and_tombstones(live: u32, tombstones: u32) {
    seed_distinct(live);
    for k in 0..tombstones {
        vector_upsert(
            shard_canister(),
            &upsert_vec(1, 2 + k as u64, 100.0 + k as f32),
        )
        .expect("tombstone re-upsert");
    }
}

fn set_active_version(index_id: u32, version: u64) {
    let mut def = definition_store::get(index_id)
        .expect("definition store available")
        .expect("def");
    def.active_index_version = version;
    definition_store::insert(index_id, def).expect("set active version");
}

#[test]
fn maintenance_step_scans_then_reports_healthy_and_resets() {
    fresh_store();
    seed_distinct(4); // 4 live rows, no tombstones -> healthy

    // First step (from Idle) runs one scan step; the single degenerate page exhausts immediately.
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("scan step"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );
    // Second step recommends from the exhausted scan: healthy -> reset to Idle.
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("recommend step"),
        VectorMaintenanceStepResult::Healthy
    );
    assert_eq!(
        admin_vector_maintenance_status(router(), INDEX_ID).expect("status"),
        VectorMaintenanceState::Idle
    );
}

#[test]
fn maintenance_step_drives_required_rebuild_to_awaiting_publish_then_publishes() {
    fresh_store();
    seed_live_and_tombstones(4, 4); // 50% tombstones -> RebuildRequired

    // Scan exhausts, then the recommendation starts a rebuild at the target nlist.
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("scan"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("recommend"),
        VectorMaintenanceStepResult::RebuildStarted(
            VectorMaintenanceRecommendation::RebuildRequired
        )
    );
    // Starting the rebuild clears the scan state (the rebuild state machine now drives).
    assert_eq!(
        admin_vector_maintenance_status(router(), INDEX_ID).expect("status"),
        VectorMaintenanceState::Idle
    );

    // Each step drives one bounded rebuild unit until it stops at ReadyToPublish (publish is explicit).
    let mut awaiting = false;
    for _ in 0..100_000 {
        match admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("rebuild step")
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
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("still awaiting"),
        VectorMaintenanceStepResult::AwaitingPublish(_)
    ));
    assert_eq!(
        admin_vector_rebuild_status(router(), INDEX_ID)
            .expect("status")
            .phase,
        VectorRebuildPhase::ReadyToPublish,
        "no auto-publish"
    );

    // Explicit publish flips the active generation; subsequent steps drive cleanup to Idle.
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    let def = def_for_test(INDEX_ID).expect("def");
    assert_eq!(def.active_index_version, TARGET_V);
    assert_eq!(def.nlist, 2);

    let mut cleaned = false;
    for _ in 0..100_000 {
        let result =
            admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("cleanup step");
        if admin_vector_rebuild_status(router(), INDEX_ID)
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
    fresh_store();
    seed_live_and_tombstones(4, 4); // 50% tombstones, degenerate nlist = 1

    // No explicit target on a degenerate (nlist=1) index: the rebuild start rejects nlist < 2.
    let req = VectorMaintenanceStepRequest {
        target_nlist: None,
        ..maint_req()
    };
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, req).expect("scan"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );
    match admin_vector_maintenance_step(router(), INDEX_ID, req).expect("failing recommend") {
        VectorMaintenanceStepResult::Failed(failure) => {
            assert_eq!(failure.code, VectorCanisterError::InvalidRebuildParams);
            assert!(!failure.message.is_empty());
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(matches!(
        admin_vector_maintenance_status(router(), INDEX_ID).expect("status"),
        VectorMaintenanceState::Failed(_)
    ));

    // A failed state is a no-op until an explicit reset.
    assert!(matches!(
        admin_vector_maintenance_step(router(), INDEX_ID, req).expect("no-op"),
        VectorMaintenanceStepResult::Failed(_)
    ));

    admin_vector_maintenance_reset(router(), INDEX_ID).expect("reset");
    assert_eq!(
        admin_vector_maintenance_status(router(), INDEX_ID).expect("status"),
        VectorMaintenanceState::Idle
    );
    // Maintenance resumes after reset (with a valid target this time).
    assert!(matches!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("resume"),
        VectorMaintenanceStepResult::Scanning { .. }
    ));
}

#[test]
fn maintenance_scan_restarts_on_stale_cursor_after_version_flip() {
    fresh_store();
    // 1 slot/page so 4 rows span 4 pages, forcing a multi-step (non-exhausting) scan.
    create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 80).expect("create");
    seed_distinct(4);

    // One bounded page (scan_max_pages = 1): the scan does not exhaust and persists a Some cursor.
    let req = VectorMaintenanceStepRequest {
        scan_max_pages: 1,
        ..maint_req()
    };
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, req).expect("scan 1"),
        VectorMaintenanceStepResult::Scanning { exhausted: false }
    );

    // The active version flips: the persisted cursor is now scoped to a stale generation.
    set_active_version(INDEX_ID, 2);

    // The next scan step sees InvalidStatsCursor and restarts cleanly from the lower bound.
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, req).expect("restart"),
        VectorMaintenanceStepResult::Scanning { exhausted: false }
    );
    match admin_vector_maintenance_status(router(), INDEX_ID).expect("status") {
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
    fresh_store();
    seed_distinct(4); // single degenerate page -> scan exhausts in one step

    // Drive the scan to exhausted (recommendation would happen on the next step).
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("scan"),
        VectorMaintenanceStepResult::Scanning { exhausted: true }
    );

    // The active version flips after exhaustion (no cursor remains to scope-check).
    set_active_version(INDEX_ID, 2);

    // The generation guard at the exhausted->recommend boundary catches the flip and restarts the
    // scan instead of recommending against the stale merged page health.
    assert_eq!(
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("restart"),
        VectorMaintenanceStepResult::Scanning { exhausted: false }
    );
    match admin_vector_maintenance_status(router(), INDEX_ID).expect("status") {
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
        admin_vector_maintenance_step(router(), INDEX_ID, maint_req()).expect("scan again"),
        VectorMaintenanceStepResult::Scanning { .. }
    ));
}

#[test]
fn maintenance_endpoints_reject_non_router_and_unknown_index() {
    fresh_store();
    seed_distinct(2);
    assert_eq!(
        admin_vector_maintenance_step(shard_canister(), INDEX_ID, maint_req()).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        admin_vector_maintenance_step(router(), 999, maint_req()).unwrap_err(),
        VectorCanisterError::UnknownIndex
    );
    assert_eq!(
        admin_vector_maintenance_status(shard_canister(), INDEX_ID).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
    assert_eq!(
        admin_vector_maintenance_reset(shard_canister(), INDEX_ID).unwrap_err(),
        VectorCanisterError::Unauthorized
    );
}

// --- ADR 0034 Slice 6: candidate-restricted vector search tests ---
#[test]
fn candidate_search_accepts_vertex_only_subjects() {
    fresh_store();
    let mut req = search_value(0.0, 10);
    // VectorSubject currently only has the Vertex variant; this smoke test confirms the typed
    // contract is accepted. When a non-vertex variant is added, add an explicit rejection test.
    req.candidate_subjects = Some(vec![VectorSubject::Vertex {
        shard_id: ShardId::new(0),
        vertex_id: 0,
    }]);
    let result = vector_search(&req).expect("vertex-only is accepted");
    assert!(result.hits.is_empty());
}

#[test]
fn candidate_search_validates_shape_before_physical_def() {
    fresh_store();
    // No upsert, so there is no physical def for INDEX_ID. An oversized allowlist must still fail.
    let mut req = search_value(0.0, 10);
    let too_many: Vec<VectorSubject> = (0..MAX_VECTOR_SEARCH_FILTER_CANDIDATES as u32 + 1)
        .map(|i| VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: i,
        })
        .collect();
    req.candidate_subjects = Some(too_many);
    let err = vector_search(&req).expect_err("oversized on empty index");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));

    // Duplicate candidates on an empty index also fail.
    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![subject(7), subject(7)]);
    let err = vector_search(&req).expect_err("duplicate on empty index");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));
}

#[test]
fn candidate_search_restricts_top_k_to_allowlist() {
    fresh_store();
    // Three vectors at 0.0, 1.0, 2.0. Query at 0.0, top_k=2.
    // Unrestricted would return vertices 7 (distance 0) and 8 (distance 1).
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 0.0)).expect("upsert 7");
    vector_upsert(shard_canister(), &upsert_vec(8, 1, 1.0)).expect("upsert 8");
    vector_upsert(shard_canister(), &upsert_vec(9, 1, 2.0)).expect("upsert 9");

    let mut req = search_value(0.0, 2);
    req.candidate_subjects = Some(vec![subject(8), subject(9)]);
    let result = vector_search(&req).expect("candidate search");
    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].subject, subject(8));
    assert_eq!(result.hits[1].subject, subject(9));
    // Vertex 7 is nearer but outside the allowlist.
    assert!(!result.hits.iter().any(|h| h.subject == subject(7)));
}

#[test]
fn candidate_search_empty_allowlist_returns_no_hits() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).expect("upsert");
    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![]);
    let result = vector_search(&req).expect("empty candidate search");
    assert!(result.hits.is_empty());
}

#[test]
fn candidate_search_skips_absent_and_deleted_subjects() {
    fresh_store();
    // Live subject 7.
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).expect("upsert 7");
    // Absent subject 8 is not in the index.
    // Deleted subject 9.
    vector_upsert(shard_canister(), &upsert_vec(9, 1, 2.0)).expect("upsert 9");
    vector_remove(shard_canister(), &remove_op(9, 2)).expect("remove 9");

    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![subject(7), subject(8), subject(9)]);
    let result = vector_search(&req).expect("candidate search");
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn candidate_search_preserves_none_as_unrestricted_path() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 0.0)).expect("upsert 7");
    vector_upsert(shard_canister(), &upsert_vec(8, 1, 1.0)).expect("upsert 8");

    let req = search_value(0.0, 10);
    assert!(req.candidate_subjects.is_none());
    let result = vector_search(&req).expect("unrestricted search");
    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].subject, subject(7));
}

#[test]
fn candidate_scan_page_major_matches_exact_scan_across_pages() {
    fresh_store();
    // d = 4 F32: pad stride 16, meta 4. A small page budget forces 2 rows per page, so five rows
    // span three pages ([0,1], [2,3], [4]) and the candidate scan must bulk-read every page.
    create_index_for_test(INDEX_ID, VectorEncoding::F32, DIMS, 80).expect("create");
    assert_eq!(def_for_test(INDEX_ID).unwrap().slots_per_page, 2);

    for (v, value) in [(0u32, 0.0f32), (1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)] {
        vector_upsert(shard_canister(), &upsert_vec(v, 1, value)).expect("upsert");
    }

    // Query at 4.0 puts vertex 4 (the last row, last page) in the top-3, so a page-major scan that
    // drops or misreads a later page would diverge from the exact (read_row_bytes) scan.
    let allowlist: Vec<VectorSubject> = (0..5).map(subject).collect();
    let mut req = search_value(4.0, 3);
    req.candidate_subjects = Some(allowlist);
    let batched = vector_search(&req).expect("batched candidate scan");
    let exact = vector_search(&search_value(4.0, 3)).expect("exact scan");
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
    fresh_store();
    // Distinct subjects 1..8 (values 0.0..7.0); a large allowlist (>= live/2) is the scan-with-
    // membership regime, and it must produce the same top-k as the resolve-based path.
    seed_distinct(8);
    let allowlist: Vec<VectorSubject> = (0..8).map(|v| subject(v + 1)).collect();
    let query = search_value(4.0, 5);
    let qv = super::search::decode_f32(&query.query);
    let resolve = candidate_subject_scan(
        INDEX_ID,
        1,
        &qv,
        VectorMetric::L2Squared,
        VectorEncoding::F32,
        &allowlist,
        5,
        0.0,
        &[],
        1.0,
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
        &[],
        1.0,
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
    fresh_store();
    let mut req = search_value(0.0, 10);
    let too_many: Vec<VectorSubject> = (0..MAX_VECTOR_SEARCH_FILTER_CANDIDATES as u32 + 1)
        .map(|i| VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: i,
        })
        .collect();
    req.candidate_subjects = Some(too_many);
    let err = vector_search(&req).expect_err("oversized allowlist");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));
}

#[test]
fn candidate_search_rejects_duplicate_subjects() {
    fresh_store();
    vector_upsert(shard_canister(), &upsert_vec(7, 1, 1.0)).expect("upsert");
    let mut req = search_value(0.0, 10);
    req.candidate_subjects = Some(vec![subject(7), subject(7)]);
    let err = vector_search(&req).expect_err("duplicate candidates");
    assert!(matches!(err, VectorCanisterError::InvalidSearchCandidates));
}

/// I8 scalar-quantization tests (B1+A1: per-row scale, F32 wire query; `VectorEncoding::I8`).
mod i8_tests {
    use super::*;
    use crate::facade::stable::rebuild_pool;
    use crate::facade::stable::{PAGE_STORE, definition_store};
    use crate::facade::store::MAX_REBUILD_TRAINING_DISTANCE_OPS;
    use crate::records::SlotRef;

    /// A distinct index id so an `I8` def is created without colliding with the F32 `INDEX_ID` fixtures.
    const I8_INDEX: u32 = 7;

    fn i8_bytes(values: &[f32]) -> Vec<u8> {
        assert_eq!(values.len(), DIMS as usize, "component count mismatch");
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn i8_upsert(index_id: u32, vertex_id: u32, stamp: u64, values: &[f32], metric: VectorMetric) {
        vector_upsert(
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

    fn i8_search(index_id: u32, values: &[f32], metric: VectorMetric, top_k: u32) -> Vec<u32> {
        let res = vector_search(&VectorSearchRequest {
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

    fn row_payload_aux(index_id: u32, slot: SlotRef) -> (Vec<u8>, [u8; 8]) {
        PAGE_STORE
            .with_borrow(|s| s.read_row_bytes(index_id, slot))
            .map(|(_, bytes, aux)| (bytes, aux))
            .expect("row present")
    }

    #[test]
    fn i8_ingest_and_search_parity_with_f32() {
        fresh_store();
        let vectors: Vec<Vec<f32>> = vec![
            vec![1.0, 2.0, 3.0, 4.0],
            vec![4.0, 3.0, 2.0, 1.0],
            vec![0.0, 0.0, 1.0, 0.0],
            vec![2.0, 2.0, 2.0, 0.0],
        ];
        for (i, v) in vectors.iter().enumerate() {
            vector_upsert(
                shard_canister(),
                &upsert_vec_from((i + 1) as u32, 1, v, VectorMetric::L2Squared),
            )
            .unwrap();
            i8_upsert(I8_INDEX, (i + 1) as u32, 1, v, VectorMetric::L2Squared);
        }
        let q = [2.0f32, 2.0, 2.0, 2.0];
        let f32_top = vector_search(&VectorSearchRequest {
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
        let i8_order = i8_search(I8_INDEX, &q, VectorMetric::L2Squared, 4);
        assert_eq!(f32_order, i8_order, "I8 top-k ordering matches F32");
    }

    #[test]
    fn i8_def_uses_meta8_and_consistent_slots_per_page() {
        fresh_store();
        i8_upsert(
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        let def = definition_store::get(I8_INDEX)
            .expect("definition store available")
            .expect("def");
        assert_eq!(def.encoding, VectorEncoding::I8);
        // I8 stores `dims` payload bytes plus a 4-byte scale in row-meta aux (meta stride 8).
        assert_eq!(def.stride_bytes, DIMS as u32);
        assert_eq!(def.meta_stride_bytes, 8);
        assert_eq!(def.pad_stride_bytes, 16);
        // `slots_per_page` is derived from `meta 8`; the page must fit at least one row.
        assert!(def.slots_per_page >= 1);
        // The I8 page reopens under `meta_stride 8`: open cross-checks the on-slab header against
        // the def above (the only geometry owner), and the seeded row is present.
        let page_meta = PAGE_STORE
            .with_borrow(|s| s.page_meta_for_test(I8_INDEX, 1, 0, 0))
            .expect("i8 page meta");
        assert_eq!(page_meta.row_count, 1);
    }

    #[test]
    fn i8_d1536_rows_are_quarter_width_and_page_capacity_rises_fourfold() {
        const D1536_F32: u32 = 8;
        const D1536_I8: u32 = 9;
        fresh_store();
        create_index_for_test(D1536_F32, VectorEncoding::F32, 1536, 64 * 1024)
            .expect("f32 d1536 index");
        create_index_for_test(D1536_I8, VectorEncoding::I8, 1536, 64 * 1024)
            .expect("i8 d1536 index");
        let f32_def = definition_store::get(D1536_F32)
            .expect("definition store available")
            .expect("f32 def");
        let i8_def = definition_store::get(D1536_I8)
            .expect("definition store available")
            .expect("i8 def");
        // I8 rows occupy a quarter of the F32 row width at d = 1536 (row stride 1536 vs 6144).
        assert_eq!(f32_def.stride_bytes, 6144);
        assert_eq!(f32_def.pad_stride_bytes, 6144);
        assert_eq!(i8_def.stride_bytes, 1536);
        assert_eq!(i8_def.pad_stride_bytes, 1536);
        assert_eq!(i8_def.meta_stride_bytes, 8);
        // The same 64 KiB page budget therefore holds roughly four times as many rows
        // (42 vs 10 under the checked layout solver).
        assert_eq!(f32_def.slots_per_page, 10);
        assert_eq!(i8_def.slots_per_page, 42);
    }

    #[test]
    fn i8_zero_l2_vector_is_accepted_and_nearest() {
        fresh_store();
        i8_upsert(
            I8_INDEX,
            1,
            1,
            &[0.0, 0.0, 0.0, 0.0],
            VectorMetric::L2Squared,
        );
        let top = i8_search(I8_INDEX, &[1.0, 1.0, 1.0, 1.0], VectorMetric::L2Squared, 1);
        assert_eq!(top, vec![1], "zero L2 I8 vector is nearest");
    }

    #[test]
    fn i8_ingest_rejects_wrong_wire_width() {
        fresh_store();
        i8_upsert(
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        let err = vector_upsert(
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
        fresh_store();
        i8_upsert(
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        // Byte-identical replay at the same stamp: no-op (no MutationStampConflict).
        i8_upsert(
            I8_INDEX,
            1,
            1,
            &[1.0, 2.0, 3.0, 4.0],
            VectorMetric::L2Squared,
        );
        // Same stamp, different payload: conflict.
        let err = vector_upsert(
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
        fresh_store();
        for (v, vals) in [
            (1u32, [1.0f32, 2.0, 3.0, 4.0]),
            (2, [4.0, 3.0, 2.0, 1.0]),
            (3, [0.0, 1.0, 2.0, 3.0]),
            (4, [3.0, 2.0, 1.0, 0.0]),
        ] {
            i8_upsert(I8_INDEX, v, 1, &vals, VectorMetric::L2Squared);
        }
        admin_start_vector_rebuild(router(), I8_INDEX, 2, 100).expect("start");
        let status = drive_steps(I8_INDEX);
        assert_eq!(status.phase, VectorRebuildPhase::ReadyToPublish);
        // Every live subject's shadow row must carry the SAME (bytes, scale) as its active row: a
        // double-quantize or a scale recompute would make these differ.
        for v in 1..=4u32 {
            let entry = subject_entry_for_test(I8_INDEX, subject(v)).unwrap();
            let active = entry.slot.expect("active slot");
            let shadow = entry.shadow_slot.expect("shadow slot");
            let (active_bytes, active_aux) = row_payload_aux(I8_INDEX, active);
            let (shadow_bytes, shadow_aux) = row_payload_aux(I8_INDEX, shadow);
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

    #[test]
    fn i8_rebuild_start_accepts_nlist_above_the_f32_pool_ceiling() {
        fresh_store();
        const D1536_I8: u32 = 9;
        create_index_for_test(D1536_I8, VectorEncoding::I8, 1536, 64 * 1024)
            .expect("i8 d1536 index");
        let dims = 1536u16;
        // An I8 d1536 stored row is 1536 bytes wide; its trained centroids are canonical f32.
        let pool_stride = 1536u64;
        let centroid_stride = u64::from(dims) * 4;
        let nlist = 700u32;
        // The region budget charges candidate rows at their native width (+ aux) and centroids at
        // f32 width inside `rebuild_pool::REGION_BYTES`; charging the pool at f32 width instead
        // would exceed that same budget and reject this nlist.
        assert!(
            2 * u64::from(nlist) * centroid_stride > rebuild_pool::REGION_BYTES,
            "fixture must be rejected by an all-f32 pool charge"
        );
        let capacity = rebuild_pool::pool_capacity_for(
            pool_stride as u32,
            nlist,
            centroid_stride as u32,
            false,
        )
        .expect("fixture must satisfy the split row/centroid budget");
        assert!(
            u64::from(nlist) <= capacity
                && rebuild_pool::POOL_HEADER_SIZE
                    + u64::from(nlist) * (pool_stride + 8)
                    + u64::from(nlist) * centroid_stride
                    <= rebuild_pool::REGION_BYTES,
            "fixture must fit the pool-region budget with room for >= nlist candidates"
        );
        assert!(
            u64::from(nlist) * u64::from(nlist) * u64::from(dims)
                <= MAX_REBUILD_TRAINING_DISTANCE_OPS,
            "fixture must satisfy the per-iteration op budget"
        );
        admin_start_vector_rebuild(router(), D1536_I8, nlist, nlist)
            .expect("I8 rebuild accepted above the all-f32 pool ceiling");
        assert_eq!(
            admin_vector_rebuild_status(router(), D1536_I8)
                .expect("status")
                .phase,
            VectorRebuildPhase::Sampling
        );
    }

    #[test]
    fn i8_rebuild_frozen_candidates_resume_deterministically_mid_training() {
        fn run() -> Vec<Vec<u8>> {
            fresh_store();
            for (v, vals) in [
                (1u32, [1.0f32, 2.0, 3.0, 4.0]),
                (2, [4.0, 3.0, 2.0, 1.0]),
                (3, [0.0, 1.0, 2.0, 3.0]),
                (4, [3.0, 2.0, 1.0, 0.0]),
                (5, [2.0, 0.0, 1.0, 2.0]),
                (6, [0.0, 3.0, 0.0, 1.0]),
            ] {
                i8_upsert(I8_INDEX, v, 1, &vals, VectorMetric::L2Squared);
            }
            admin_start_vector_rebuild(router(), I8_INDEX, 3, 100).expect("start");
            // One subject per message: every phase transition persists the frozen stored-form pool
            // into `VECTOR_REBUILD_STATE` and the next message reloads it, so Training always runs
            // over candidates that round-tripped through Candid put/get.
            let mut saw_training_pool = false;
            loop {
                let status = admin_vector_rebuild_step(router(), I8_INDEX, 1).expect("stepped");
                match status.phase {
                    VectorRebuildPhase::Training => {
                        saw_training_pool = true;
                        assert_eq!(
                            status.candidates_collected, 6,
                            "the frozen pool survives each persist/reload intact"
                        );
                    }
                    VectorRebuildPhase::Sampling | VectorRebuildPhase::Building => {}
                    VectorRebuildPhase::ReadyToPublish => break,
                    other => panic!("unexpected phase {other:?}"),
                }
            }
            assert!(saw_training_pool, "rebuild must pass through Training");
            admin_publish_vector_rebuild(router(), I8_INDEX).expect("publish");
            IVF_CENTROIDS.with_borrow(|m| {
                (0..3)
                    .map(|p| {
                        m.get(&PartitionKey::new(I8_INDEX, TARGET_V, p))
                            .expect("centroid")
                    })
                    .collect()
            })
        }
        let first = run();
        let second = run();
        assert_eq!(
            first, second,
            "I8 rebuild from reloaded stored-form candidates is deterministic"
        );
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

    fn f32_upsert(index_id: u32, vertex_id: u32, values: &[f32], metric: VectorMetric) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        vector_upsert(
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

    fn f32_topk(index_id: u32, values: &[f32], metric: VectorMetric, top_k: u32) -> Vec<u32> {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let res = vector_search(&VectorSearchRequest {
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

    fn i8_upsert_d(index_id: u32, vertex_id: u32, values: &[f32], metric: VectorMetric) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        vector_upsert(
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

    fn i8_search_d(index_id: u32, values: &[f32], metric: VectorMetric, top_k: u32) -> Vec<u32> {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let res = vector_search(&VectorSearchRequest {
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
    fn run_recall(dims: u16, rng_seed: u64, metric: VectorMetric) -> (f32, f32) {
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
            f32_upsert(INDEX_ID, v, &vals, metric);
            i8_upsert_d(I8_INDEX, v, &vals, metric);
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
            let f = f32_topk(INDEX_ID, &q, metric, 100);
            let i = i8_search_d(I8_INDEX, &q, metric, 100);
            r10 += overlap(&f[..10], &i[..10]) as f32 / 10.0;
            r100 += overlap(&f, &i) as f32 / 100.0;
        }
        (r10 / queries as f32, r100 / queries as f32)
    }

    #[test]
    fn i8_recall_vs_f32_gaussian_l2() {
        fresh_store();
        let (r10, r100) = run_recall(256, 0xDEAD_BEEF, VectorMetric::L2Squared);
        eprintln!("I8 recall L2 gaussian d=256: recall@10={r10:.4} recall@100={r100:.4}");
        assert!(
            r10 >= 0.90 && r100 >= 0.90,
            "I8 L2 recall too low: @10={r10} @100={r100}"
        );
    }

    #[test]
    fn i8_recall_vs_f32_unit_sphere_cosine() {
        fresh_store();
        let (r10, r100) = run_recall(256, 0xFEED_FACE, VectorMetric::Cosine);
        eprintln!("I8 recall cosine unit-sphere d=256: recall@10={r10:.4} recall@100={r100:.4}");
        assert!(
            r10 >= 0.90 && r100 >= 0.90,
            "I8 cosine recall too low: @10={r10} @100={r100}"
        );
    }

    fn i8_search_tuned(
        index_id: u32,
        values: &[f32],
        metric: VectorMetric,
        top_k: u32,
        eps: f32,
    ) -> Vec<u32> {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let res = vector_search_tuned(
            &VectorSearchRequest {
                index_id,
                query: bytes,
                encoding: VectorEncoding::I8,
                dims: values.len() as u16,
                metric,
                top_k,
                candidate_subjects: None,
            },
            tuned(eps),
        )
        .expect("i8 tuned search");
        res.hits
            .iter()
            .map(|h| match h.subject {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect()
    }

    /// I8 rebuild runs the full lifecycle (start -> sampling -> training -> building ->
    /// ReadyToPublish -> publish -> cleanup -> Idle) and search stays exact across publish/cleanup
    /// (partition full scan at `eps = INF` equals the pre-publish exact scan).
    #[test]
    fn i8_rebuild_publish_cleanup_full_lifecycle() {
        fresh_store();
        for (v, vals) in [
            (1u32, [1.0f32, 2.0, 3.0, 4.0]),
            (2, [4.0, 3.0, 2.0, 1.0]),
            (3, [0.0, 1.0, 2.0, 3.0]),
            (4, [3.0, 2.0, 1.0, 0.0]),
        ] {
            i8_upsert_d(I8_INDEX, v, &vals, VectorMetric::L2Squared);
        }
        // Exact scan before rebuild (nlist=1).
        let before = i8_search_d(I8_INDEX, &[1.5, 1.5, 1.5, 1.5], VectorMetric::L2Squared, 10);
        // Rebuild to nlist=2 and drive to ReadyToPublish.
        admin_start_vector_rebuild(router(), I8_INDEX, 2, 100).expect("start");
        assert_eq!(
            drive_steps(I8_INDEX).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        // Publish switches active to the rebuilt nlist=2 partition scan.
        admin_publish_vector_rebuild(router(), I8_INDEX).expect("publish");
        // Full partition scan (eps=INF) matches the pre-publish exact scan.
        let after = i8_search_tuned(
            I8_INDEX,
            &[1.5, 1.5, 1.5, 1.5],
            VectorMetric::L2Squared,
            10,
            f32::INFINITY,
        );
        assert_eq!(
            before, after,
            "I8 search parity across publish (full partition scan)"
        );
        // Cleanup to Idle (old version dropped).
        drive_cleanup(I8_INDEX);
        // Search still works and matches after cleanup.
        let after_cleanup = i8_search_tuned(
            I8_INDEX,
            &[1.5, 1.5, 1.5, 1.5],
            VectorMetric::L2Squared,
            10,
            f32::INFINITY,
        );
        assert_eq!(before, after_cleanup, "I8 search survives cleanup");
    }
}

/// Two-level (`levels = 2`) hierarchy coverage (Slice 5). The flat lifecycle above is untouched by
/// every change these tests pin; each test drives the public facade only.
mod two_level_tests {
    use super::*;
    use crate::facade::stable::{IVF_CENTROID_META, subject_store};
    use crate::records::{IvfCentroidMeta, LEVELS_TWO, PartitionKey, SubjectKey};
    use gleaph_graph_kernel::vector_index::{
        VECTOR_EPS_BPS_INFINITY, VectorIndexKind, VectorRebuildPhase,
    };

    // Re-exported from `search` via the store module for centroid byte fixtures.
    fn encode_f32(vector: &[f32]) -> Vec<u8> {
        super::search::encode_f32(vector)
    }

    /// Coarse count / branching factor of the deterministic fixture.
    const C: u32 = 2;
    const F: u32 = 2;
    const SAMPLE_LIMIT: u32 = 100;

    /// Starts a two-level rebuild of the degenerate fixture at `(nlist, nlist_fine) = (C, F)`.
    fn start_two_level() {
        admin_start_vector_rebuild_with_fine(
            router(),
            INDEX_ID,
            C,
            SAMPLE_LIMIT,
            Some(F),
            None,
            None,
            None,
        )
        .expect("two-level start");
    }

    /// Seeds two well-separated 4-dim clusters (4 distinct vectors each): cluster `c` sits around
    /// `12.0 * c` with a tiny per-vertex offset so every stored row is byte-distinct.
    fn seed_two_clusters() {
        for c in 0..2u32 {
            for j in 0..4u32 {
                let base = 12.0f32 * c as f32 + 0.01 * j as f32;
                let vals = [base, base, base, base];
                vector_upsert(
                    shard_canister(),
                    &upsert_vec_from(c * 4 + j + 1, 1, &vals, VectorMetric::L2Squared),
                )
                .expect("seed upsert");
            }
        }
    }

    /// Reads the published shape off the durable definition.
    fn def_shape() -> (u8, u32, u32) {
        let def = definition_store::get(INDEX_ID)
            .expect("definition readable")
            .expect("definition present");
        (def.levels, def.nlist, def.nlist_fine)
    }

    fn active_version() -> u64 {
        definition_store::get(INDEX_ID)
            .expect("definition readable")
            .expect("definition present")
            .active_index_version
    }

    /// Snapshot of one generation's centroid bytes at both levels, in key order — the
    /// deterministic-identity fingerprint of a completed training pipeline.
    fn centroid_fingerprint(version: u64) -> Vec<Option<Vec<u8>>> {
        IVF_CENTROIDS.with_borrow(|m| {
            (0..C)
                .map(|p| m.get(&PartitionKey::coarse(INDEX_ID, version, p)))
                .chain((0..C * F).map(|p| m.get(&PartitionKey::new(INDEX_ID, version, p))))
                .collect()
        })
    }

    /// Vertex-id hits of an eps-bounded search.
    fn hit_ids(req: &VectorSearchRequest, eps: f32) -> Vec<u32> {
        vector_search_tuned(req, tuned(eps))
            .expect("search")
            .hits
            .iter()
            .map(|h| match h.subject {
                VectorSubject::Vertex { vertex_id, .. } => vertex_id,
            })
            .collect()
    }

    /// Contract ①: the full two-level lifecycle is **deterministically identical** across two
    /// independent runs (byte-identical trained centroids at both levels), and the published
    /// full-leaf scan equals the pre-rebuild exact ground truth.
    #[test]
    fn two_level_lifecycle_is_deterministic_and_matches_exact_ground_truth() {
        // Run 1: start -> publish -> search -> clean.
        fresh_store();
        seed_two_clusters();
        // Ground truth: exact scan on the degenerate generation before any rebuild.
        let query = vec_bytes_from(&[6.0, 6.0, 6.0, 6.0]);
        let ground_truth = vector_search(&VectorSearchRequest {
            index_id: INDEX_ID,
            query: query.clone(),
            encoding: VectorEncoding::F32,
            dims: DIMS,
            metric: VectorMetric::L2Squared,
            top_k: 8,
            candidate_subjects: None,
        })
        .expect("ground truth");
        start_two_level();
        assert_eq!(
            drive_steps(INDEX_ID).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
        assert_eq!(def_shape(), (LEVELS_TWO, C, F), "published shape");
        let target = active_version();
        let run1 = centroid_fingerprint(target);
        assert_eq!(run1.len(), (C + C * F) as usize);
        assert!(
            run1.iter().all(|slot| slot.is_some()),
            "every coarse and leaf centroid written"
        );
        // Full-leaf scan (eps INF) must equal the exact pre-rebuild result exactly.
        let full_leaf = vector_search_tuned(
            &search_metric_from(&[6.0, 6.0, 6.0, 6.0], 8, VectorMetric::L2Squared),
            tuned(f32::INFINITY),
        )
        .expect("full-leaf scan");
        assert_eq!(
            full_leaf.hits, ground_truth.hits,
            "full-leaf scan reproduces the flat ground truth exactly"
        );
        drive_cleanup(INDEX_ID);

        // Run 2: identical inputs from a fresh store must produce identical bytes.
        fresh_store();
        seed_two_clusters();
        start_two_level();
        assert_eq!(
            drive_steps(INDEX_ID).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
        let target2 = active_version();
        assert_eq!(target, target2, "same active version in both runs");
        assert_eq!(
            centroid_fingerprint(target2),
            run1,
            "training is byte-for-byte deterministic"
        );
    }

    /// Contract ④ (Slice 9): the published def freezes the per-level ε₂ bps, and the default
    /// search path (no tuning override) derives the coarse stage from `eps_query_bps` and the
    /// leaf stage from `eps_fine_bps` **independently**.
    #[test]
    fn two_level_default_search_uses_frozen_per_level_eps() {
        fresh_store();
        // Seed a two-level generation directly (no rebuild): coarse0 at 0, coarse1 at 100; each
        // coarse has two leaves. The def freezes coarse eps = 0 (nearest coarse only) and leaf
        // eps = ∞ (every leaf of the selected coarse).
        let active = 5u64;
        IVF_CENTROIDS.with_borrow_mut(|m| {
            m.insert(
                PartitionKey::coarse(INDEX_ID, active, 0),
                encode_f32(&[0.0, 0.0, 0.0, 0.0]),
            );
            m.insert(
                PartitionKey::coarse(INDEX_ID, active, 1),
                encode_f32(&[100.0, 100.0, 100.0, 100.0]),
            );
            m.insert(
                PartitionKey::new(INDEX_ID, active, 0),
                encode_f32(&[0.0, 0.0, 0.0, 0.0]),
            );
            m.insert(
                PartitionKey::new(INDEX_ID, active, 1),
                encode_f32(&[1.0, 1.0, 1.0, 1.0]),
            );
            m.insert(
                PartitionKey::new(INDEX_ID, active, 2),
                encode_f32(&[100.0, 100.0, 100.0, 100.0]),
            );
            m.insert(
                PartitionKey::new(INDEX_ID, active, 3),
                encode_f32(&[101.0, 101.0, 101.0, 101.0]),
            );
        });
        IVF_CENTROID_META.with_borrow_mut(|meta| {
            meta.insert(
                INDEX_ID,
                IvfCentroidMeta {
                    centroid_ready: true,
                    trained_index_version: active,
                },
            )
        });
        let mut def = fixture_def();
        def.active_index_version = active;
        def.eps_query_bps = 0;
        def.eps_fine_bps = VECTOR_EPS_BPS_INFINITY;
        definition_store::insert(INDEX_ID, def).expect("seed def");

        // Rows: A near leaf 0, B near leaf 1 (both under coarse 0), C near leaf 2 (coarse 1).
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(1, 1, &[0.1, 0.1, 0.1, 0.1], VectorMetric::L2Squared),
        )
        .expect("upsert A");
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(2, 1, &[0.9, 0.9, 0.9, 0.9], VectorMetric::L2Squared),
        )
        .expect("upsert B");
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(3, 1, &[100.1, 100.1, 100.1, 100.1], VectorMetric::L2Squared),
        )
        .expect("upsert C");

        // Default search (no tuning override): coarse eps = 0 selects only the nearest coarse
        // subtree (coarse 0), leaf eps = ∞ selects every leaf of it — rows A and B. A single
        // shared eps of 0 would have returned only the nearest leaf's subset, so this pins the
        // per-level independence. Query 0.3 is nearest coarse 0 and closer to A (0.1) than B
        // (0.9), so the hits are ordered `[A, B]`.
        let hits = vector_search(&VectorSearchRequest {
            index_id: INDEX_ID,
            query: vec_bytes_from(&[0.3, 0.3, 0.3, 0.3]),
            encoding: VectorEncoding::F32,
            dims: DIMS,
            metric: VectorMetric::L2Squared,
            top_k: 8,
            candidate_subjects: None,
        })
        .expect("default search")
        .hits
        .into_iter()
        .map(|h| match h.subject {
            VectorSubject::Vertex { vertex_id, .. } => vertex_id,
        })
        .collect::<Vec<_>>();
        assert_eq!(
            hits,
            vec![1, 2],
            "coarse=0 selects coarse 0, fine=∞ selects both its leaves (A and B)"
        );
    }

    /// Minimal F32 d4 two-level def used by the pure-rule fixture below.
    fn fixture_def() -> crate::records::VectorIndexDef {
        crate::records::VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            nlist: 2,
            active_index_version: 2,
            stride_bytes: 16,
            pad_stride_bytes: 16,
            meta_stride_bytes: 4,
            run_capacity: 1,
            max_page_bytes: 64 * 1024,
            slots_per_page: 64,
            levels: LEVELS_TWO,
            nlist_fine: 2,
            code_tier: false,
            code_stride_bytes: 0,
            rotation_seed: 0,
            eps_query_bps: 0,
            eps_fine_bps: 0,
        }
    }

    /// Contract ②: the empty/insufficient subtree rules are pure and deterministic — coarse
    /// replication for an empty subtree and nearest-to-member-mean fill with lowest-id
    /// tie-breaks otherwise.
    #[test]
    fn subtree_rules_are_deterministic_with_lowest_id_tiebreaks() {
        use super::super::rebuild::{complete_subtree_leaf_centroids, member_mean_bytes};
        let coarse_centroid = encode_f32(&[7.0; 4]);

        // Empty subtree (`member_mean` is None): every leaf replicates the coarse centroid.
        let empty = complete_subtree_leaf_centroids(Vec::new(), None, &coarse_centroid, 4);
        assert_eq!(empty, vec![coarse_centroid.clone(); 4], "empty replication");

        // Insufficient subtree: one trained centroid fills every missing slot deterministically.
        let members = vec![crate::records::RebuildCandidate {
            stored: encode_f32(&[0.5; 4]),
            aux: [0; 8],
        }];
        let mean = member_mean_bytes(&fixture_def(), &members).expect("mean exists");
        let trained = vec![encode_f32(&[1.0; 4])];
        let filled =
            complete_subtree_leaf_centroids(trained.clone(), Some(&mean), &coarse_centroid, 3);
        assert_eq!(filled.len(), 3);
        assert_eq!(filled[0], trained[0], "trained slot kept");
        assert_eq!(filled[1], trained[0], "fill copies the nearest centroid");
        assert_eq!(filled[2], trained[0]);

        // Exact tie: centroids symmetric around the member mean are equidistant, so BOTH fills
        // must resolve to the same source — the lowest-id slot.
        let mid_mean = encode_f32(&[128.0; 4]);
        let tied = vec![encode_f32(&[129.0; 4]), encode_f32(&[127.0; 4])];
        let filled_tied =
            complete_subtree_leaf_centroids(tied, Some(&mid_mean), &coarse_centroid, 4);
        assert_eq!(filled_tied.len(), 4);
        assert_eq!(
            filled_tied[2], filled_tied[3],
            "tie-break is deterministic across fill order"
        );
        assert_eq!(
            filled_tied[2],
            encode_f32(&[129.0; 4]),
            "equidistant tie resolves to the lowest id"
        );

        // Nearest selection without ties picks the genuinely closer centroid.
        let near = vec![encode_f32(&[127.9; 4]), encode_f32(&[130.0; 4])];
        let filled_near =
            complete_subtree_leaf_centroids(near, Some(&mid_mean), &coarse_centroid, 4);
        assert_eq!(filled_near[2], encode_f32(&[127.9; 4]));
    }

    /// Contract ③: within one subtree, duplicated leaf centroids collapse rows onto the lowest
    /// duplicate id (the best-leaf rule shared by Building and mutation assignment).
    #[test]
    fn building_leaf_assignment_prefers_lowest_duplicate_leaf() {
        // Seed a published two-level generation directly: 2 coarses x 2 leaves; leaves 2 and 3
        // (subtree 1) share ONE centroid, so any row assigned to subtree 1 must land on leaf 2.
        fresh_store();
        let active = 5u64;
        IVF_CENTROIDS.with_borrow_mut(|m| {
            m.insert(
                PartitionKey::new(INDEX_ID, active, 0),
                encode_f32(&[0.0, 0.0, 0.0, 0.0]),
            );
            m.insert(
                PartitionKey::new(INDEX_ID, active, 1),
                encode_f32(&[20.0, 20.0, 20.0, 20.0]),
            );
            m.insert(
                PartitionKey::new(INDEX_ID, active, 2),
                encode_f32(&[10.0, 10.0, 10.0, 10.0]),
            );
            // Duplicate of leaf 2: same centroid, higher id.
            m.insert(
                PartitionKey::new(INDEX_ID, active, 3),
                encode_f32(&[10.0, 10.0, 10.0, 10.0]),
            );
            m.insert(
                PartitionKey::coarse(INDEX_ID, active, 0),
                encode_f32(&[5.0, 5.0, 5.0, 5.0]),
            );
            m.insert(
                PartitionKey::coarse(INDEX_ID, active, 1),
                encode_f32(&[15.0, 15.0, 15.0, 15.0]),
            );
        });
        IVF_CENTROID_META.with_borrow_mut(|meta| {
            meta.insert(
                INDEX_ID,
                IvfCentroidMeta {
                    centroid_ready: true,
                    trained_index_version: active,
                },
            )
        });
        let mut def = fixture_def();
        def.active_index_version = active;
        definition_store::insert(INDEX_ID, def).expect("seed def");

        // A row near [11,11,11,11]: nearest coarse is 1 (its children live at leaves 2..3); both
        // leaves are equidistant duplicates, so the row must land on leaf 2.
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(99, 1, &[11.0, 11.0, 11.0, 11.0], VectorMetric::L2Squared),
        )
        .expect("upsert");
        let entry = subject_store::get(&SubjectKey::new(INDEX_ID, subject(99)))
            .expect("subject readable")
            .expect("subject entry present");
        let slot = entry.current_slot_for(active).expect("live slot");
        assert_eq!(
            slot.partition_id, 2,
            "duplicate leaves collapse to the lowest id"
        );

        // And the partitioned scan reaches the collapsed leaf (eps 0 selects it).
        let hits = hit_ids(
            &search_metric_from(&[11.0, 11.0, 11.0, 11.0], 4, VectorMetric::L2Squared),
            0.0,
        );
        assert_eq!(hits, vec![99]);
    }

    /// Whether `version` holds no centroid keys at `level_coarse` (or leaf) and no leaf heads.
    fn generation_is_gone(version: u64, level_coarse: bool) -> bool {
        let centroids_empty = IVF_CENTROIDS.with_borrow(|m| {
            (0..C).all(|p| {
                m.get(&if level_coarse {
                    PartitionKey::coarse(INDEX_ID, version, p)
                } else {
                    PartitionKey::new(INDEX_ID, version, p)
                })
                .is_none()
            })
        });
        let heads_empty = VECTOR_PARTITION_HEADS.with_borrow(|heads| {
            (0..C * F).all(|p| {
                heads
                    .get(&PartitionKey::new(INDEX_ID, version, p))
                    .expect("head get")
                    .is_none()
            })
        });
        centroids_empty && heads_empty
    }

    /// Contract ④: cleanup drops the OLD generation's keys at BOTH levels while the new
    /// generation keeps coarse + leaf keys; aborting a two-level rebuild clears the shadow at
    /// both levels too.
    #[test]
    fn teardown_clears_both_levels_of_the_dead_generation() {
        fresh_store();
        seed_two_clusters();
        let old_version = active_version();
        start_two_level();
        assert_eq!(
            drive_steps(INDEX_ID).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
        drive_cleanup(INDEX_ID);

        // The old generation lost everything at both levels...
        assert!(generation_is_gone(old_version, true), "old coarse dropped");
        assert!(generation_is_gone(old_version, false), "old leaves dropped");
        // ...while the published generation stays searchable at both levels.
        let new_version = active_version();
        assert!(!generation_is_gone(new_version, true), "new coarse kept");
        assert!(!generation_is_gone(new_version, false), "new leaves kept");

        // Abort path: start another two-level rebuild, drive into Building (both levels of the
        // shadow exist by then), abort, clean — the shadow disappears entirely.
        start_two_level();
        let shadow = new_version + 1;
        loop {
            let status = admin_vector_rebuild_step(router(), INDEX_ID, 100).expect("step");
            if status.phase == VectorRebuildPhase::Building {
                break;
            }
            assert_ne!(status.phase, VectorRebuildPhase::Failed, "rebuild failed");
        }
        admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort");
        drive_cleanup(INDEX_ID);
        assert!(generation_is_gone(shadow, true), "shadow coarse dropped");
        assert!(generation_is_gone(shadow, false), "shadow leaves dropped");
        // The active generation survived untouched.
        assert_eq!(active_version(), new_version);
    }

    /// Contract ⑤: only the level-0 coarse set becomes cache-resident; fine child sets are always
    /// stable reads.
    #[test]
    fn cache_holds_coarse_set_only() {
        use crate::facade::store::centroid_cache;
        fresh_store();
        seed_two_clusters();
        start_two_level();
        assert_eq!(
            drive_steps(INDEX_ID).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
        // An update-path assignment populates the cache through read_active.
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(88, 1, &[12.5, 12.5, 12.5, 12.5], VectorMetric::L2Squared),
        )
        .expect("post-publish upsert");
        let def = definition_store::get(INDEX_ID)
            .expect("def")
            .expect("def present");
        assert!(def.is_two_level());
        let resident =
            centroid_cache::lookup(INDEX_ID, def.active_index_version, def.nlist, def.dims);
        assert!(resident.is_some(), "coarse set cached");
        assert_eq!(
            resident.expect("set").len() as u32,
            def.nlist,
            "cached set is the coarse set"
        );
        // No entry can exist for a leaf-count lookup: fine sets are never cached.
        assert!(
            centroid_cache::lookup(
                INDEX_ID,
                def.active_index_version,
                def.nlist * def.nlist_fine,
                def.dims,
            )
            .is_none(),
            "fine set never cached"
        );
    }

    /// Contract ⑥: MAX_LEAVES and shape feasibility fail closed at start; the flat entry point is
    /// unchanged.
    #[test]
    fn max_leaves_and_shape_feasibility_fail_closed() {
        fresh_store();
        // Create the physical definition (lazy, as in production) so shape checks are reachable.
        vector_upsert(shard_canister(), &upsert_vec(1, 1, 1.0)).expect("def-creating upsert");
        // Leaves beyond MAX_LEAVES are rejected outright.
        assert_eq!(
            admin_start_vector_rebuild_with_fine(
                router(),
                INDEX_ID,
                1024,
                2000,
                Some(65),
                None,
                None,
                None
            )
            .unwrap_err(),
            VectorCanisterError::InvalidRebuildParams,
            "1024 * 65 > MAX_LEAVES"
        );
        // A single-child hierarchy is not a meaningful shape.
        assert_eq!(
            admin_start_vector_rebuild_with_fine(
                router(),
                INDEX_ID,
                2,
                100,
                Some(1),
                None,
                None,
                None
            )
            .unwrap_err(),
            VectorCanisterError::InvalidRebuildParams
        );
        // The boundary value itself passes validation and starts (then aborts immediately).
        admin_start_vector_rebuild_with_fine(
            router(),
            INDEX_ID,
            1024,
            1025,
            Some(64),
            None,
            None,
            None,
        )
        .expect("exactly MAX_LEAVES admits");
        admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort");
        // Flat behavior unchanged: the plain signature still starts a flat rebuild.
        admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("flat start unchanged");
        admin_abort_vector_rebuild(router(), INDEX_ID).expect("abort");
    }
}

mod code_tier_tests;
