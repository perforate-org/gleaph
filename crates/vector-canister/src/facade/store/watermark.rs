//! Per-shard watermark advance and subject-map GC (ADR 0064 §5).
//!
//! The subject map (`VECTOR_SUBJECT_TO_ID`) retains deleted clocks for the mutation-ordering fence.
//! Two per-shard watermarks are maintained for conservative tombstone GC:
//!
//! - `graph_watermark`: highest graph→vector acked stamp (advanced by the graph shard's typed
//!   `vector_sync_batch_outcome` endpoint).
//! - `router_watermark`: a contiguous Router-owned acknowledgement floor, advanced only by the
//!   Router frontier endpoint after exact lane validation.

use crate::facade::stable::subject_store;
use crate::facade::stable::{
    SHARD_CANISTER_CATALOG, VECTOR_DELETED_SUBJECTS, VECTOR_GC_CURSOR, VECTOR_INDEX_ROUTER,
    VECTOR_SHARD_WATERMARKS,
};
use crate::records::{DeletedSubjectKey, SubjectKey};
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{VectorCanisterError, VectorSubject};
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
use std::ops::Bound;

use super::VectorCanisterStore;

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

impl VectorCanisterStore {
    /// Applies a contiguous Router frontier for one exact attached shard, then performs one
    /// bounded tombstone-GC step in the same message.
    ///
    /// The caller and attachment checks intentionally live below the public Candid guard. Native
    /// unit tests bypass the IC guard, and the storage owner must not trust an intermediate caller
    /// check before mutating the durable watermark.
    pub(crate) fn advance_router_frontier(
        &self,
        caller: Principal,
        shard_id: ShardId,
        frontier: u64,
    ) -> Result<(), VectorCanisterError> {
        self.assert_router_caller(caller)?;
        let attached =
            SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.contains_shard(shard_id));
        if !attached {
            return Err(VectorCanisterError::ShardNotAttached);
        }

        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            let mut current = watermarks.get(&shard_id).unwrap_or_default();
            current.router_watermark = current.router_watermark.max(frontier);
            watermarks.insert(shard_id, current);
        });
        super::gc_subjects_step(crate::canister::GC_SUBJECTS_BUDGET);
        Ok(())
    }

    /// Read the exact state relevant to one tombstone and the durable GC cursor. The tuple is a
    /// feature-gated test seam only; each read stays behind its owning stable-storage facade.
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn test_vector_frontier_probe(
        &self,
        index_id: u32,
        shard_id: ShardId,
        vertex_id: u32,
    ) -> Result<
        (
            u64,
            u64,
            Option<(u64, bool)>,
            bool,
            Option<(u32, u64, u32, u32, u32)>,
        ),
        String,
    > {
        let subject_key = SubjectKey::new(
            index_id,
            VectorSubject::Vertex {
                shard_id,
                vertex_id,
            },
        );
        let watermarks = VECTOR_SHARD_WATERMARKS
            .with_borrow(|watermarks| watermarks.get(&shard_id).unwrap_or_default());
        let subject_clock = subject_store::get(&subject_key)
            .map_err(|error| format!("subject probe read failed: {error:?}"))?
            .map(|entry| (entry.stamp, entry.deleted));
        let deleted_list_contains = VECTOR_DELETED_SUBJECTS
            .with_borrow(|deleted| deleted.keys().any(|key| key.subject == subject_key));
        let gc_cursor = VECTOR_GC_CURSOR.with_borrow(|cursor| {
            cursor.get().map(|key| {
                let subject_vertex_id = match key.subject.subject {
                    VectorSubject::Vertex { vertex_id, .. } => vertex_id,
                };
                (
                    key.shard_id.raw(),
                    key.stamp,
                    key.subject.index_id,
                    0,
                    subject_vertex_id,
                )
            })
        });
        Ok((
            watermarks.graph_watermark,
            watermarks.router_watermark,
            subject_clock,
            deleted_list_contains,
            gc_cursor,
        ))
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
    use crate::facade::stable::memory;
    use crate::init::{
        VectorCanisterInitArgs, DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED,
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

    #[derive(Debug, PartialEq, Eq)]
    struct FrontierStateSnapshot {
        watermarks: Vec<(ShardId, ShardWatermarks)>,
        subject_clock: Option<FixedSubjectMapEntry>,
        deleted_subjects: Vec<(DeletedSubjectKey, u8)>,
        gc_cursor: Option<DeletedSubjectKey>,
    }

    fn frontier_state_snapshot(key: &SubjectKey) -> FrontierStateSnapshot {
        FrontierStateSnapshot {
            watermarks: VECTOR_SHARD_WATERMARKS.with_borrow(|watermarks| {
                watermarks
                    .iter()
                    .map(|entry| (*entry.key(), entry.value()))
                    .collect()
            }),
            subject_clock: subject_store::get(key).expect("subject clock snapshot"),
            deleted_subjects: VECTOR_DELETED_SUBJECTS.with_borrow(|deleted| {
                deleted
                    .iter()
                    .map(|entry| (*entry.key(), entry.value()))
                    .collect()
            }),
            gc_cursor: VECTOR_GC_CURSOR.with_borrow(|cursor| *cursor.get()),
        }
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
    fn unknown_caller_preserves_frontier_state_exactly() {
        fresh_store();
        let key = SubjectKey::new(INDEX_ID, subject(7));
        subject_store::insert(
            key,
            FixedSubjectMapEntry {
                stamp: 8,
                deleted: true,
                slot: None,
                shadow_slot: None,
            },
        )
        .expect("seed subject clock");
        VECTOR_DELETED_SUBJECTS.with_borrow_mut(|deleted| {
            deleted.insert(DeletedSubjectKey::new(ShardId::new(0), 8, key), 0);
        });
        let cursor = DeletedSubjectKey::new(ShardId::new(0), 8, key);
        VECTOR_GC_CURSOR.with_borrow_mut(|gc_cursor| gc_cursor.set(Some(cursor)));
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: 9,
                    router_watermark: 4,
                },
            );
        });
        let before = frontier_state_snapshot(&key);
        let stranger = Principal::from_slice(&[2]);
        assert!(!advance_watermark(stranger, &[upsert_op(7, 9)]));
        assert_eq!(frontier_state_snapshot(&key), before);
    }

    #[test]
    fn router_frontier_requires_configured_router_and_attached_shard() {
        let store = fresh_store();
        let shard_id = ShardId::new(0);
        store
            .vector_upsert(shard_canister(), &upsert_op(7, 1))
            .expect("upsert");
        store
            .vector_remove(shard_canister(), &remove_op(7, 2))
            .expect("remove");
        let before = ShardWatermarks {
            graph_watermark: 9,
            router_watermark: 4,
        };
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(shard_id, before);
        });
        let key = SubjectKey::new(INDEX_ID, subject(7));
        let cursor = DeletedSubjectKey::new(shard_id, 2, key);
        VECTOR_GC_CURSOR.with_borrow_mut(|gc_cursor| gc_cursor.set(Some(cursor)));

        let before_anonymous = frontier_state_snapshot(&key);
        assert_eq!(
            store.advance_router_frontier(Principal::anonymous(), shard_id, 100),
            Err(VectorCanisterError::Unauthorized)
        );
        assert_eq!(frontier_state_snapshot(&key), before_anonymous);

        let stranger = Principal::from_slice(&[2]);
        let before_unauthorized = frontier_state_snapshot(&key);
        assert_eq!(
            store.advance_router_frontier(stranger, shard_id, 100),
            Err(VectorCanisterError::Unauthorized)
        );
        assert_eq!(frontier_state_snapshot(&key), before_unauthorized);

        let before_shard_caller = frontier_state_snapshot(&key);
        assert_eq!(
            store.advance_router_frontier(shard_canister(), shard_id, 100),
            Err(VectorCanisterError::Unauthorized)
        );
        assert_eq!(frontier_state_snapshot(&key), before_shard_caller);

        let detached = ShardId::new(1);
        let before_detached = frontier_state_snapshot(&key);
        assert_eq!(
            store.advance_router_frontier(router(), detached, 100),
            Err(VectorCanisterError::ShardNotAttached)
        );
        assert_eq!(frontier_state_snapshot(&key), before_detached);

        store
            .advance_router_frontier(router(), shard_id, 7)
            .expect("configured Router may advance an attached shard");
        assert_eq!(
            watermarks(shard_id),
            ShardWatermarks {
                graph_watermark: 9,
                router_watermark: 7,
            }
        );
    }

    #[test]
    fn router_frontier_is_monotonic_and_runs_bounded_gc() {
        let store = fresh_store();
        for (vertex_id, mutation_id) in [(7, 2), (8, 4), (9, 7)] {
            store
                .vector_upsert(shard_canister(), &upsert_op(vertex_id, mutation_id - 1))
                .expect("upsert");
            store
                .vector_remove(shard_canister(), &remove_op(vertex_id, mutation_id))
                .expect("remove");
        }
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: 100,
                    router_watermark: 0,
                },
            );
        });

        store
            .advance_router_frontier(router(), ShardId::new(0), 5)
            .expect("advance frontier");
        assert_eq!(watermarks(ShardId::new(0)).router_watermark, 5);
        assert!(subject_entry(7).is_none(), "frontier GC removes stamp 2");
        assert!(subject_entry(8).is_none(), "frontier GC removes stamp 4");
        assert!(
            subject_entry(9).expect("stamp 7 tombstone").deleted,
            "frontier below the tombstone retains its replay fence"
        );

        store
            .vector_upsert(shard_canister(), &upsert_op(10, 2))
            .expect("upsert lower-retry tombstone");
        store
            .vector_remove(shard_canister(), &remove_op(10, 3))
            .expect("remove lower-retry tombstone");
        store
            .advance_router_frontier(router(), ShardId::new(0), 4)
            .expect("lower retry");
        assert_eq!(
            watermarks(ShardId::new(0)).router_watermark,
            5,
            "a lower frontier must not lower the durable Router watermark"
        );
        assert!(
            subject_entry(10).is_none(),
            "a lower retry still runs one bounded GC step"
        );

        store
            .vector_upsert(shard_canister(), &upsert_op(11, 3))
            .expect("upsert equal-retry tombstone");
        store
            .vector_remove(shard_canister(), &remove_op(11, 4))
            .expect("remove equal-retry tombstone");
        store
            .advance_router_frontier(router(), ShardId::new(0), 5)
            .expect("equal retry");
        assert_eq!(watermarks(ShardId::new(0)).router_watermark, 5);
        assert!(subject_entry(9).expect("stamp 7 tombstone").deleted);
        assert!(
            subject_entry(11).is_none(),
            "an equal retry still runs one bounded GC step"
        );

        store
            .advance_router_frontier(router(), ShardId::new(0), 7)
            .expect("advance through final tombstone");
        assert_eq!(watermarks(ShardId::new(0)).router_watermark, 7);
        assert!(
            subject_entry(9).is_none(),
            "bounded GC eventually examines the fence"
        );
    }

    #[test]
    fn watermark_is_monotonic() {
        fresh_store();
        advance_watermark(shard_canister(), &[upsert_op(7, 5)]);
        advance_watermark(shard_canister(), &[upsert_op(7, 3)]);
        assert_eq!(
            watermarks(ShardId::new(0)).graph_watermark,
            5,
            "a lower graph watermark call must not lower the durable value"
        );
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
        let budget = crate::canister::GC_SUBJECTS_BUDGET;
        let last_vertex = budget;
        for vertex in 0..=last_vertex {
            let key = SubjectKey::new(INDEX_ID, subject(vertex));
            let stamp = u64::from(vertex) + 1;
            subject_store::insert(
                key,
                FixedSubjectMapEntry {
                    stamp,
                    deleted: true,
                    slot: None,
                    shadow_slot: None,
                },
            )
            .expect("seed lightweight tombstone");
            VECTOR_DELETED_SUBJECTS.with_borrow_mut(|deleted| {
                deleted.insert(DeletedSubjectKey::new(ShardId::new(0), stamp, key), 0);
            });
        }
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: u64::MAX,
                    router_watermark: 0,
                },
            );
        });

        let first_key = DeletedSubjectKey::new(
            ShardId::new(0),
            u64::from(budget),
            SubjectKey::new(INDEX_ID, subject(budget - 1)),
        );
        let last_key = DeletedSubjectKey::new(
            ShardId::new(0),
            u64::from(last_vertex) + 1,
            SubjectKey::new(INDEX_ID, subject(last_vertex)),
        );

        // Exactly budget entries are examined and deleted. The cursor identifies the budget-th
        // key; the one remaining list key and subject clock prove that the budget was not ignored.
        store
            .advance_router_frontier(router(), ShardId::new(0), u64::MAX)
            .expect("router frontier runs one bounded GC step");
        assert_eq!(
            VECTOR_GC_CURSOR.with_borrow(|cursor| *cursor.get()),
            Some(first_key),
            "the first bounded step must persist the budget-th examined key"
        );
        assert_eq!(
            VECTOR_DELETED_SUBJECTS.with_borrow(|deleted| {
                deleted
                    .iter()
                    .map(|entry| (*entry.key(), entry.value()))
                    .collect::<Vec<_>>()
            }),
            vec![(last_key, 0)],
            "the first bounded step must leave exactly the suffix after its cursor"
        );
        assert!(subject_store::get(&first_key.subject)
            .expect("first examined subject")
            .is_none());
        assert!(subject_store::get(&last_key.subject)
            .expect("remaining subject")
            .is_some());
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

    /// Reopens every production frontier initializer over the same backing MemoryManager bytes.
    /// This proves that the MemoryId 7 subject clock, MemoryId 15 watermark map, MemoryId 16 GC
    /// cursor, and MemoryId 17 deleted-subject list retain the complete GC fence, not only the
    /// independent definition-store bytes.
    #[test]
    fn production_frontier_initializers_reopen_over_same_backing_memory() {
        let store = fresh_store();
        let key = SubjectKey::new(INDEX_ID, subject(7));
        store
            .vector_upsert(shard_canister(), &upsert_op(7, 3))
            .expect("upsert");
        store
            .vector_remove(shard_canister(), &remove_op(7, 8))
            .expect("remove");
        VECTOR_SHARD_WATERMARKS.with_borrow_mut(|watermarks| {
            watermarks.insert(
                ShardId::new(0),
                ShardWatermarks {
                    graph_watermark: 13,
                    router_watermark: 11,
                },
            );
        });
        let cursor = DeletedSubjectKey::new(ShardId::new(0), 8, key);
        VECTOR_GC_CURSOR.with_borrow_mut(|gc_cursor| gc_cursor.set(Some(cursor)));
        let expected = frontier_state_snapshot(&key);

        // These are the same production initializer functions used by the thread-local handles;
        // each resolves its region from the shared MemoryManager, so the reopen reads the bytes
        // written above rather than a fresh native backing store.
        let reopened_watermarks = memory::init_shard_watermarks();
        let reopened_cursor = memory::init_gc_cursor();
        let reopened_deleted = memory::init_deleted_subjects();
        subject_store::reopen_for_test().expect("exact-open subject region");

        assert_eq!(
            reopened_watermarks
                .iter()
                .map(|entry| (*entry.key(), entry.value()))
                .collect::<Vec<_>>(),
            expected.watermarks,
            "MemoryId 15 frontier must survive production initializer reopen"
        );
        assert_eq!(
            *reopened_cursor.get(),
            expected.gc_cursor,
            "MemoryId 16 cursor must survive production initializer reopen"
        );
        assert_eq!(
            reopened_deleted
                .iter()
                .map(|entry| (*entry.key(), entry.value()))
                .collect::<Vec<_>>(),
            expected.deleted_subjects,
            "MemoryId 17 deleted-subject list must survive production initializer reopen"
        );
        assert_eq!(
            subject_store::get(&key).expect("reopened subject clock"),
            expected.subject_clock,
            "MemoryId 7 subject fence must survive production initializer reopen"
        );
    }
}
