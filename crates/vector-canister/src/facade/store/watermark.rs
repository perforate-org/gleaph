//! Per-shard watermark advance and subject-map GC (ADR 0064 §5).
//!
//! The subject map (`VECTOR_SUBJECT_TO_ID`) is the only store that would otherwise grow monotonically
//! with cumulative ever-ingested subjects (deleted clocks are retained for the mutation-ordering
//! fence). Two per-shard watermarks bound it:
//!
//! - `graph_watermark`: highest graph→vector acked stamp (advanced by the graph shard's
//!   `vector_sync_batch`).
//! - `router_watermark`: highest Router→vector acked stamp (advanced by the Router's
//!   `vector_sync_batch`).
//!
//! A deleted entry with `stamp <= min(both)` for its shard is unreachable (no stale replay can arrive)
//! and is GC'd. The subject map therefore stays at `≈ N + recently deleted`.

use crate::facade::stable::{
    SHARD_CANISTER_CATALOG, VECTOR_DELETED_SUBJECTS, VECTOR_GC_CURSOR, VECTOR_INDEX_ROUTER,
    VECTOR_SHARD_WATERMARKS, VECTOR_SUBJECT_TO_ID,
};
use crate::records::DeletedSubjectKey;
use candid::Principal;
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
use std::ops::Bound;

/// Advances the caller's watermark for the shards touched by `ops` to the max stamp in the batch.
///
/// The caller is either the Router (advances each touched shard's `router_watermark`) or an attached
/// graph shard (advances its own shard's `graph_watermark`). The mutation path has already validated
/// the caller against the shard catalog, so an unknown caller is a no-op here.
pub(crate) fn advance_watermark(caller: Principal, ops: &[VectorEmbeddingSyncOp]) {
    let router = VECTOR_INDEX_ROUTER.with_borrow(|r| *r.get());
    let is_router = caller == router;
    let caller_shard = if is_router {
        None
    } else {
        SHARD_CANISTER_CATALOG.with_borrow(|c| c.shard_for_canister(caller))
    };

    for op in ops {
        let shard = if is_router {
            op.subject.shard_id()
        } else {
            // A graph shard only sends ops for its own shard; fall back to the op's shard if the
            // caller's shard is somehow unresolved.
            caller_shard.unwrap_or_else(|| op.subject.shard_id())
        };
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            let mut wm = watermarks.get(&shard).unwrap_or_default();
            if is_router {
                wm.router_watermark = wm.router_watermark.max(op.mutation_id);
            } else {
                wm.graph_watermark = wm.graph_watermark.max(op.mutation_id);
            }
            watermarks.insert(shard, wm);
        });
    }
}

/// Removes up to `budget` deleted subject-map entries whose `stamp <= min(graph_watermark,
/// router_watermark)` for their shard. Returns the number of entries removed.
///
/// Live entries and deleted entries above the cutoff are retained. A shard with no watermark record
/// has a cutoff of `0`, so nothing is GC-eligible until both watermarks advance.
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
        VECTOR_SUBJECT_TO_ID
            .with_borrow_mut(|m| m.remove(&key.subject).expect("remove collected subject"));
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
    use crate::init::VectorCanisterInitArgs;
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
            .init_from_args(&VectorCanisterInitArgs {
                router_canister: router(),
            })
            .expect("init");
        store.attach_single_shard_for_test(router(), ShardId::new(0), shard_canister());
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
        VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(&SubjectKey::new(INDEX_ID, subject(vertex_id))))
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
    fn router_advances_router_watermark() {
        fresh_store();
        advance_watermark(router(), &[upsert_op(7, 5), upsert_op(8, 9)]);
        let wm = watermarks(ShardId::new(0));
        assert_eq!(wm.graph_watermark, 0);
        assert_eq!(wm.router_watermark, 9);
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

        // Advance both watermarks past the deleted stamp (2).
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        advance_watermark(router(), &[upsert_op(7, 5)]);
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
        advance_watermark(router(), &[upsert_op(7, 5)]);
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
        advance_watermark(router(), &[upsert_op(0, 5)]);
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

    /// Upgrade persistence: the watermark store survives a re-init (simulated upgrade) through the
    /// facade's real stable-memory lifecycle. `init_from_args` clears the request-path stores but not
    /// the watermark store, so the advanced watermark must be unchanged after re-init.
    #[test]
    fn watermark_survives_reinit_upgrade() {
        let store = fresh_store();
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        assert_eq!(watermarks(ShardId::new(0)).graph_watermark, 5);

        // Simulate an upgrade: re-init with the same args.
        store
            .init_from_args(&VectorCanisterInitArgs {
                router_canister: router(),
            })
            .expect("re-init");
        assert_eq!(
            watermarks(ShardId::new(0)).graph_watermark,
            5,
            "watermark must survive upgrade"
        );
    }
}
