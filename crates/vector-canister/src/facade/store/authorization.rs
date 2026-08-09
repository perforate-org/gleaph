//! Router authorization and shard/canister attachment registry for the vector index.

use super::VectorCanisterStore;
use crate::facade::stable::memory::{ShardCanisterCatalogInsertError, VectorIndexOwnershipConfig};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, OWNERSHIP_CONFIG, PAGE_STORE, SHARD_CANISTER_CATALOG,
    VECTOR_DELETED_SUBJECTS, VECTOR_INDEX_DEFS, VECTOR_INDEX_ROUTER, VECTOR_MAINTENANCE_STATE,
    VECTOR_PARTITION_HEADS, VECTOR_REBUILD_STATE, VECTOR_SUBJECT_TO_ID,
};
use crate::init::VectorCanisterInitArgs;
use crate::records::{DeletedSubjectKey, SubjectKey, SubjectScanCursor};
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{
    ShardDetachCursor, ShardDetachPhase, ShardDetachStepResult, ShardId,
};
use gleaph_graph_kernel::vector_index::VectorCanisterError;

/// Upper bound on subject keys examined per detach step, keeping one message within the canister
/// instruction / stable read budget regardless of total index size.
const MAX_DETACH_EXAMINE_PER_STEP: u32 = 20_000;

impl VectorCanisterStore {
    /// Clears all derived state and seeds the router principal from init args.
    ///
    /// Rejects an anonymous router before mutating any stable state.
    pub fn init_from_args(&self, args: &VectorCanisterInitArgs) -> Result<(), VectorCanisterError> {
        if args.router_canister == Principal::anonymous() {
            return Err(VectorCanisterError::AnonymousRouter);
        }
        SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| catalog.clear_new());
        VECTOR_INDEX_DEFS.with_borrow_mut(|m| m.clear_new());
        IVF_CENTROID_META.with_borrow_mut(|m| m.clear_new());
        IVF_CENTROIDS.with_borrow_mut(|m| m.clear_new());
        VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| m.clear_new());
        VECTOR_DELETED_SUBJECTS.with_borrow_mut(|m| m.clear_new());
        VECTOR_REBUILD_STATE.with_borrow_mut(|m| m.clear_new());
        // Stable execution state: persists across upgrade, cleared only on this init/reset path.
        VECTOR_MAINTENANCE_STATE.with_borrow_mut(|m| m.clear_new());
        VECTOR_PARTITION_HEADS.with_borrow_mut(|m| m.clear_new());
        PAGE_STORE.with_borrow_mut(|store| store.reset());
        super::centroid_cache::clear_all();
        VECTOR_INDEX_ROUTER.with_borrow_mut(|router| {
            router.set(args.router_canister);
        });
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            cell.set(VectorIndexOwnershipConfig::default());
        });
        Ok(())
    }

    pub(super) fn commit_attach_shard_canister(
        &self,
        graph_id: GraphId,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), VectorCanisterError> {
        if shard_canister_principal == Principal::anonymous() {
            return Err(VectorCanisterError::InvalidPrincipalInRegistry);
        }
        // One vector target per graph (ADR 0031 Slice 4): this canister owns *every* shard of a
        // single graph, so ownership is fixed by `graph_id` alone. The first attach pins the graph;
        // any shard of that graph attaches; a different graph is rejected. (Property-index group
        // sharding does not apply — a single target must accept shards 0..N of the graph.)
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            let mut cfg = cell.get().clone();
            if !cfg.initialized {
                cfg.initialized = true;
                cfg.graph_id = graph_id;
                cell.set(cfg);
                return Ok(());
            }
            if cfg.graph_id != graph_id {
                return Err(VectorCanisterError::GraphOwnershipMismatch);
            }
            Ok(())
        })?;
        SHARD_CANISTER_CATALOG
            .with_borrow_mut(|catalog| catalog.insert(shard_id, shard_canister_principal))
            .map_err(|e| match e {
                ShardCanisterCatalogInsertError::ShardAlreadyAttached
                | ShardCanisterCatalogInsertError::CanisterAlreadyAttached => {
                    VectorCanisterError::ShardCanisterAlreadyAttached
                }
            })
    }

    fn commit_detach_shard_step_with_budget(
        &self,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
        budget: u32,
    ) -> ShardDetachStepResult {
        let cursor = match resume {
            Some(cursor) => cursor,
            None => {
                // Drop the auth mapping first so the shard can no longer write while the bounded
                // purge runs across steps.
                SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| {
                    catalog.remove_shard(shard_id);
                });
                ShardDetachCursor {
                    phase: ShardDetachPhase::Vertex,
                    resume_key: Vec::new(),
                }
            }
        };

        let step = self.purge_subjects_step(shard_id, &cursor.resume_key, budget);
        let next = step.resume_key.map(|resume_key| ShardDetachCursor {
            phase: ShardDetachPhase::Vertex,
            resume_key,
        });

        ShardDetachStepResult {
            done: next.is_none(),
            next,
            examined: step.examined,
            removed: step.removed,
        }
    }

    /// Scans up to `budget` subject keys (resuming after the saved slot), removing all rows owned by
    /// `shard_id`: for live rows, tombstones the slot and drops the id→slot entry; then removes the
    /// subject clock (the shard is gone). Collects matches before removing so the slot cursor stays
    /// stable across the scan (removal would otherwise shift slots).
    fn purge_subjects_step(
        &self,
        shard_id: ShardId,
        resume_key: &[u8],
        budget: u32,
    ) -> SubjectPurgeStep {
        let resume = decode_scan_cursor(resume_key);
        let mut examined = 0u32;
        let mut to_remove: Vec<SubjectKey> = Vec::new();
        let mut last_cursor: Option<SubjectScanCursor> = None;
        let mut exhausted = true;
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            let capacity = subjects.capacity();
            // Restart from slot 0 if the saved capacity no longer matches (a concurrent mutation
            // resized the map, invalidating the slot cursor).
            let start_slot = match resume {
                Some(c) if c.capacity == capacity => c.slot,
                _ => 0,
            };
            let mut iter = subjects.iter_from(start_slot);
            while let Some((key, _value)) = iter.next() {
                if examined >= budget {
                    exhausted = false;
                    break;
                }
                examined += 1;
                last_cursor = Some(SubjectScanCursor::new(iter.position(), capacity));
                if key.subject.shard_id() == shard_id {
                    to_remove.push(key);
                }
            }
        });

        let removed = u32::try_from(to_remove.len()).unwrap_or(u32::MAX);
        for key in &to_remove {
            let entry = VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(key));
            if let Some(entry) = entry {
                if entry.deleted {
                    // Drop the tombstoned row from the deleted list so the GC does not later try to
                    // remove an already-purged subject.
                    VECTOR_DELETED_SUBJECTS.with_borrow_mut(|m| {
                        m.remove(&DeletedSubjectKey::new(
                            key.subject.shard_id(),
                            entry.stamp,
                            *key,
                        ));
                    });
                } else if let Some(slot) = entry.slot {
                    self.tombstone_slot(key.index_id, slot);
                }
            }
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| m.remove(key));
        }

        let resume_key = if exhausted {
            None
        } else {
            last_cursor.map(encode_scan_cursor)
        };
        SubjectPurgeStep {
            examined,
            removed,
            resume_key,
        }
    }

    pub fn admin_attach_shard_canister(
        &self,
        caller: Principal,
        graph_id: GraphId,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), VectorCanisterError> {
        self.assert_router_caller(caller)?;
        self.commit_attach_shard_canister(graph_id, shard_id, shard_canister_principal)
    }

    /// Performs one bounded step of a shard subject purge. The first call (`resume == None`) also
    /// drops the shard's auth mapping. The router resumes from [`ShardDetachStepResult::next`].
    pub fn admin_detach_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
    ) -> Result<ShardDetachStepResult, VectorCanisterError> {
        self.assert_router_caller(caller)?;
        Ok(
            self.commit_detach_shard_step_with_budget(
                shard_id,
                resume,
                MAX_DETACH_EXAMINE_PER_STEP,
            ),
        )
    }

    #[cfg(test)]
    pub(crate) fn detach_shard_step_for_test(
        &self,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
        budget: u32,
    ) -> ShardDetachStepResult {
        self.commit_detach_shard_step_with_budget(shard_id, resume, budget)
    }

    pub(super) fn assert_router_caller(
        &self,
        caller: Principal,
    ) -> Result<(), VectorCanisterError> {
        if caller == Principal::anonymous() {
            return Err(VectorCanisterError::Unauthorized);
        }
        let router = VECTOR_INDEX_ROUTER.with_borrow(|r| *r.get());
        if caller != router {
            return Err(VectorCanisterError::Unauthorized);
        }
        Ok(())
    }
}

/// Outcome of purging up to `budget` subject keys for one shard.
struct SubjectPurgeStep {
    examined: u32,
    removed: u32,
    resume_key: Option<Vec<u8>>,
}

/// Encodes a [`SubjectScanCursor`] into the detach `resume_key` byte string.
fn encode_scan_cursor(cursor: SubjectScanCursor) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&cursor.slot.to_le_bytes());
    out.extend_from_slice(&cursor.capacity.to_le_bytes());
    out
}

/// Decodes a [`SubjectScanCursor`] from the detach `resume_key` byte string.
///
/// An empty key means "start from slot 0" (fresh purge).
fn decode_scan_cursor(resume_key: &[u8]) -> Option<SubjectScanCursor> {
    if resume_key.is_empty() {
        return None;
    }
    let slot = u64::from_le_bytes(resume_key[0..8].try_into().unwrap());
    let capacity = u64::from_le_bytes(resume_key[8..16].try_into().unwrap());
    Some(SubjectScanCursor::new(slot, capacity))
}

/// Convenience for test setup: attach a shard of graph 1 to this single vector target.
#[cfg(test)]
impl VectorCanisterStore {
    pub(crate) fn attach_single_shard_for_test(
        &self,
        router: Principal,
        shard_id: ShardId,
        shard_canister: Principal,
    ) {
        self.admin_attach_shard_canister(router, GraphId::from_raw(1), shard_id, shard_canister)
            .expect("attach shard canister");
    }
}
