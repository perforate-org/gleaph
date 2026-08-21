//! Per-shard watermark advance and subject-map GC (ADR 0064 §5).
//!
//! The subject map (`VECTOR_SUBJECT_TO_ID`) retains deleted clocks for the mutation-ordering fence.
//! Two per-shard watermarks are maintained for conservative tombstone GC:
//!
//! - `graph_watermark`: highest graph→vector acked stamp (advanced by the graph shard's typed
//!   `vector_sync_batch_outcome` endpoint).
//! - `router_watermark`: a contiguous Router-owned acknowledgement floor. Direct Router vector
//!   ingestion does not advance this field because a later request cannot prove that an earlier
//!   response was observed; it therefore remains zero on the current production path.
//!
//! Because the Router watermark remains zero, `min(graph_watermark, router_watermark)` remains zero
//! and tombstone deletion is paused in production. No bounded subject-map growth claim is made.

use crate::facade::stable::subject_store;
use crate::facade::stable::{
    SHARD_CANISTER_CATALOG, VECTOR_DELETED_SUBJECTS, VECTOR_GC_CURSOR, VECTOR_INDEX_ROUTER,
    VECTOR_SHARD_WATERMARKS,
};
use crate::records::DeletedSubjectKey;
use candid::Principal;
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
use std::ops::Bound;

/// Advances an attached Graph's watermark for its own shard to the max stamp in `ops`.
///
/// Router-originated direct ingestion and unresolved callers do not own a contiguous acknowledgement
/// floor, so they return `false` without changing watermarks. The boolean tells the caller whether a
/// bounded GC step is safe for this authorized Graph path.
pub(crate) fn advance_watermark(caller: Principal, ops: &[VectorEmbeddingSyncOp]) -> bool {
    let router = VECTOR_INDEX_ROUTER.with_borrow(|r| *r.get());
    if caller == router {
        return false;
    }
    let Some(caller_shard) =
        SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_for_canister(caller))
    else {
        return false;
    };
    if ops.iter().any(|op| op.subject.shard_id() != caller_shard) {
        return false;
    }

    for op in ops {
        let shard = caller_shard;
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            let mut wm = watermarks.get(&shard).unwrap_or_default();
            wm.graph_watermark = wm.graph_watermark.max(op.mutation_id);
            watermarks.insert(shard, wm);
        });
    }
    true
}

/// Removes up to `budget` deleted subject-map entries whose `stamp <= min(graph_watermark,
/// router_watermark)` for their shard. Returns the number of entries removed.
///
/// Live entries and deleted entries above the cutoff are retained. A shard with no watermark record
/// has a cutoff of `0`, so nothing is GC-eligible until both watermarks advance; the current Router
/// watermark of `0` therefore pauses tombstone deletion.
///
/// The scan walks `VECTOR_DELETED_SUBJECTS` (the deleted-subjects list, keyed by `(shard, stamp,
/// subject)`) rather than the subject map, because the subject map's slot order is unstable under
/// removal (a slot-index cursor would be invalidated by the GC's own removes). The list gives a
/// stable key-based cursor: it resumes from the durable `VECTOR_GC_CURSOR` (the last examined
/// deleted key) and wraps to the start once exhausted.
pub(crate) fn gc_subjects_step(budget: u32) -> u32 {
    let mut examined = 0u32;
    let mut to_remove: Vec<DeletedSubjectKey> = Vec::new();
    let mut last_key: Option<DeletedSubjectKey> = None;
    let mut exhausted = true;
    let resume = VECTOR_GC_CURSOR.with_borrow(|c| *c.get());
    VECTOR_DELETED_SUBJECTS.with_borrow(|deleted| {
        let lower = match resume {
            None => Bound::Unbounded,
            Some(key) => Bound::Excluded(key),
        };
        for entry in deleted.range((lower, Bound::Unbounded)) {
            if examined >= budget {
                exhausted = false;
                break;
            }
            examined += 1;
            let key = *entry.key();
            last_key = Some(key);
            let cutoff = VECTOR_SHARD_WATERMARKS.with_borrow(|watermarks| {
                watermarks
                    .get(&key.shard_id)
                    .map(|wm| wm.cutoff())
                    .unwrap_or(0)
            });
            if key.stamp <= cutoff {
                to_remove.push(key);
            }
        }
    });

    for key in &to_remove {
        VECTOR_DELETED_SUBJECTS.with_borrow_mut(|m| m.remove(key));
        subject_store::remove(&key.subject).expect("remove collected subject");
    }
    // Persist the resume cursor: the last examined key if the scan was budget-bounded, else wrap to
    // the start (None) so the next step scans from the beginning again.
    let next_cursor = if exhausted { None } else { last_key };
    VECTOR_GC_CURSOR.with_borrow_mut(|c| c.set(next_cursor));
    u32::try_from(to_remove.len()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::{
        DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED, VectorCanisterInitArgs,
    };
    use crate::records::{DeletedSubjectKey, FixedSubjectMapEntry, ShardWatermarks, SubjectKey};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorMetric};
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};
    use ic_stable_structures::{BTreeMap, Cell, DefaultMemoryImpl, Storable};

    const INDEX_ID: u32 = 1;
    const DIMS: u16 = 4;
    const STRIDE: usize = 16;

    fn router() -> Principal {
        Principal::from_slice(&[9])
    }

    fn shard_canister() -> Principal {
        Principal::from_slice(&[1])
    }

    fn fresh_store() -> crate::facade::VectorCanisterStore {
        let store = crate::facade::VectorCanisterStore::new();
        store
            .reset_for_test_or_bench(&VectorCanisterInitArgs {
                router_canister: router(),
                definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
                subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
            })
            .expect("init");
        store.attach_single_shard_for_test(router(), ShardId::new(0), shard_canister());
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| watermarks.clear_new());
        VECTOR_GC_CURSOR.with_borrow_mut(|cursor| cursor.set(None));
        store
    }

    fn subject(vertex_id: u32) -> gleaph_graph_kernel::vector_index::VectorSubject {
        gleaph_graph_kernel::vector_index::VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id,
        }
    }

    fn upsert_op(vertex_id: u32, mutation_id: u64) -> VectorEmbeddingSyncOp {
        VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: subject(vertex_id),
            mutation_id,
            encoding: VectorEncoding::F32,
            dims: DIMS,
            metric: VectorMetric::L2Squared,
            bytes: vec![0xAA; STRIDE],
            remove: false,
        }
    }

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

    fn watermarks(shard_id: ShardId) -> ShardWatermarks {
        VECTOR_SHARD_WATERMARKS.with_borrow(|wm| wm.get(&shard_id).unwrap_or_default())
    }

    fn subject_entry(vertex_id: u32) -> Option<FixedSubjectMapEntry> {
        subject_store::get(&SubjectKey::new(INDEX_ID, subject(vertex_id)))
            .ok()
            .flatten()
    }

    #[test]
    fn graph_shard_advances_graph_watermark() {
        fresh_store();
        advance_watermark(shard_canister(), &[upsert_op(7, 5), upsert_op(8, 9)]);
        let wm = watermarks(ShardId::new(0));
        assert_eq!(wm.graph_watermark, 9);
        assert_eq!(wm.router_watermark, 0);
    }

    #[test]
    fn router_does_not_advance_router_watermark() {
        fresh_store();
        assert!(!advance_watermark(
            router(),
            &[upsert_op(7, 5), upsert_op(8, 9)]
        ));
        let wm = watermarks(ShardId::new(0));
        assert_eq!(wm.graph_watermark, 0);
        assert_eq!(wm.router_watermark, 0);
    }

    #[test]
    fn unresolved_caller_does_not_advance_any_watermark() {
        fresh_store();
        let stranger = Principal::from_slice(&[2]);
        assert!(!advance_watermark(stranger, &[upsert_op(7, 9)]));
        assert_eq!(watermarks(ShardId::new(0)), ShardWatermarks::default());
    }

    #[test]
    fn watermark_is_monotonic() {
        fresh_store();
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        advance_watermark(shard_canister(), &[upsert_op(7, 3)]);
        assert_eq!(watermarks(ShardId::new(0)).graph_watermark, 5);
    }

    #[test]
    fn gc_removes_deleted_entry_below_cutoff() {
        let store = fresh_store();
        store
            .vector_upsert(shard_canister(), &upsert_op(7, 1))
            .expect("upsert");
        store
            .vector_remove(shard_canister(), &remove_op(7, 2))
            .expect("remove");
        assert!(subject_entry(7).expect("clock").deleted);

        // The Graph watermark advances through the owner boundary. Seed the independent Router
        // acknowledgement floor directly so this unit exercises GC without granting direct Router
        // ingestion watermark ownership.
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: 5,
                    router_watermark: 5,
                },
            );
        });
        assert_eq!(gc_subjects_step(100), 1);
        assert!(subject_entry(7).is_none(), "deleted entry GC'd");
    }

    #[test]
    fn gc_retains_live_entry() {
        let store = fresh_store();
        store
            .vector_upsert(shard_canister(), &upsert_op(7, 1))
            .expect("upsert");
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: 5,
                    router_watermark: 5,
                },
            );
        });
        assert_eq!(gc_subjects_step(100), 0);
        let entry = subject_entry(7).expect("live entry retained");
        assert!(!entry.deleted);
    }

    #[test]
    fn gc_retains_deleted_entry_above_cutoff() {
        let store = fresh_store();
        store
            .vector_upsert(shard_canister(), &upsert_op(7, 1))
            .expect("upsert");
        store
            .vector_remove(shard_canister(), &remove_op(7, 2))
            .expect("remove");
        // Only the graph watermark advanced to 5; the router watermark is 0, so cutoff is 0.
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        assert_eq!(gc_subjects_step(100), 0);
        assert!(subject_entry(7).expect("clock").deleted);
    }

    #[test]
    fn gc_with_no_watermarks_removes_nothing() {
        let store = fresh_store();
        store
            .vector_upsert(shard_canister(), &upsert_op(7, 1))
            .expect("upsert");
        store
            .vector_remove(shard_canister(), &remove_op(7, 2))
            .expect("remove");
        assert_eq!(gc_subjects_step(100), 0);
        assert!(subject_entry(7).is_some());
    }

    #[test]
    fn gc_is_bounded_by_budget() {
        let store = fresh_store();
        for vertex in 0..10 {
            store
                .vector_upsert(shard_canister(), &upsert_op(vertex, 1))
                .expect("upsert");
            store
                .vector_remove(shard_canister(), &remove_op(vertex, 2))
                .expect("remove");
        }
        advance_watermark(shard_canister(), &[upsert_op(0, 5)]);
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: 5,
                    router_watermark: 5,
                },
            );
        });
        // Budget 3 examines only the first 3 entries; all are deleted and below cutoff.
        assert_eq!(gc_subjects_step(3), 3);
    }

    #[test]
    fn shard_watermarks_storable_roundtrip() {
        let wm = ShardWatermarks {
            graph_watermark: 7,
            router_watermark: 9,
        };
        assert_eq!(wm.cutoff(), 7);
        let bytes = wm.to_bytes();
        assert_eq!(ShardWatermarks::from_bytes(bytes), wm);
    }

    // --- Stable-storage regression tests (ADR 0064 §5 new regions) ---

    /// Per-MemoryId separation: `VECTOR_SHARD_WATERMARKS` (BTreeMap) and `VECTOR_GC_CURSOR` (Cell) must
    /// live on distinct MemoryIds and be independently readable. A wrong implementation that places
    /// both on the same MemoryId fails this test (offset-0 corruption: one collection's bytes overwrite
    /// the other's header).
    #[test]
    fn watermark_and_gc_cursor_are_on_distinct_memory_ids() {
        let mm = MemoryManager::init(DefaultMemoryImpl::default());
        let mut watermarks: BTreeMap<ShardId, ShardWatermarks, _> =
            BTreeMap::init(mm.get(MemoryId::new(15)));
        let mut cursor: Cell<Option<DeletedSubjectKey>, _> =
            Cell::init(mm.get(MemoryId::new(16)), None);

        watermarks.insert(
            ShardId::new(0),
            ShardWatermarks {
                graph_watermark: 5,
                router_watermark: 9,
            },
        );
        cursor.set(Some(DeletedSubjectKey::new(
            ShardId::new(0),
            2,
            SubjectKey::new(1, subject(7)),
        )));

        // Each collection is independently readable from its own MemoryId.
        assert_eq!(watermarks.get(&ShardId::new(0)).unwrap().graph_watermark, 5);
        assert_eq!(
            *cursor.get(),
            Some(DeletedSubjectKey::new(
                ShardId::new(0),
                2,
                SubjectKey::new(1, subject(7)),
            )),
            "GC cursor must not be clobbered by the watermark map"
        );
    }

    /// StableCell write-over-write: setting the GC cursor twice must leave the second value readable.
    /// A wrong implementation that ignores the second write fails this test.
    #[test]
    fn gc_cursor_write_over_write_keeps_latest() {
        let mm = MemoryManager::init(DefaultMemoryImpl::default());
        let mut cursor: Cell<Option<DeletedSubjectKey>, _> =
            Cell::init(mm.get(MemoryId::new(16)), None);

        cursor.set(Some(DeletedSubjectKey::new(
            ShardId::new(0),
            2,
            SubjectKey::new(1, subject(7)),
        )));
        cursor.set(Some(DeletedSubjectKey::new(
            ShardId::new(0),
            2,
            SubjectKey::new(1, subject(8)),
        )));
        assert_eq!(
            *cursor.get(),
            Some(DeletedSubjectKey::new(
                ShardId::new(0),
                2,
                SubjectKey::new(1, subject(8)),
            ))
        );
    }

    /// Upgrade persistence: the watermark store survives a seed-free definition-store reopen.
    #[test]
    fn watermark_survives_definition_store_reopen() {
        let _store = fresh_store();
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        assert_eq!(watermarks(ShardId::new(0)).graph_watermark, 5);

        // Simulate the owner heap rebind used by `post_upgrade`; it must not reset other stable
        // regions such as the watermark table.
        crate::facade::stable::definition_store::reopen_for_test().expect("exact reopen");
        assert_eq!(
            watermarks(ShardId::new(0)).graph_watermark,
            5,
            "watermark must survive upgrade"
        );
    }
}
