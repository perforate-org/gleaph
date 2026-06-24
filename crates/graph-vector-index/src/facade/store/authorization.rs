//! Router authorization and shard/canister attachment registry for the vector index.

use super::VectorIndexStore;
use crate::facade::stable::memory::{ShardCanisterCatalogInsertError, VectorIndexOwnershipConfig};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, OWNERSHIP_CONFIG, SHARD_CANISTER_CATALOG, VECTOR_ID_TO_SLOT,
    VECTOR_INDEX_DEFS, VECTOR_INDEX_ROUTER, VECTOR_PAGE, VECTOR_PARTITION_HEADS,
    VECTOR_SUBJECT_TO_ID,
};
use crate::init::VectorIndexInitArgs;
use crate::records::SubjectKey;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{
    ShardDetachCursor, ShardDetachPhase, ShardDetachStepResult, ShardId,
};
use gleaph_graph_kernel::vector_index::VectorIndexError;
use ic_stable_structures::Storable;
use std::borrow::Cow;
use std::ops::Bound;

/// Upper bound on subject keys examined per detach step, keeping one message within the canister
/// instruction / stable read budget regardless of total index size.
const MAX_DETACH_EXAMINE_PER_STEP: u32 = 20_000;

impl VectorIndexStore {
    /// Clears all derived state and seeds the router principal from init args.
    ///
    /// Rejects an anonymous router before mutating any stable state.
    pub fn init_from_args(&self, args: &VectorIndexInitArgs) -> Result<(), VectorIndexError> {
        if args.router_canister == Principal::anonymous() {
            return Err(VectorIndexError::AnonymousRouter);
        }
        SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| catalog.clear_new());
        VECTOR_INDEX_DEFS.with_borrow_mut(|m| m.clear_new());
        IVF_CENTROID_META.with_borrow_mut(|m| m.clear_new());
        IVF_CENTROIDS.with_borrow_mut(|m| m.clear_new());
        VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| m.clear_new());
        VECTOR_ID_TO_SLOT.with_borrow_mut(|m| m.clear_new());
        VECTOR_PARTITION_HEADS.with_borrow_mut(|m| m.clear_new());
        VECTOR_PAGE.with_borrow_mut(|m| m.clear_new());
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
        index_group_size: u32,
        group_index: u32,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), VectorIndexError> {
        if shard_canister_principal == Principal::anonymous() {
            return Err(VectorIndexError::InvalidPrincipalInRegistry);
        }
        if index_group_size == 0 {
            return Err(VectorIndexError::InvalidIndexGroupConfig);
        }
        let group_start = u64::from(group_index) * u64::from(index_group_size);
        let group_end = group_start + u64::from(index_group_size);
        let shard_raw = u64::from(shard_id.raw());
        if shard_raw < group_start || shard_raw >= group_end {
            return Err(VectorIndexError::ShardOutOfRangeForGroup);
        }
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            let mut cfg = cell.get().clone();
            if !cfg.initialized {
                cfg.initialized = true;
                cfg.graph_id = graph_id;
                cfg.index_group_size = index_group_size;
                cfg.group_index = group_index;
                cell.set(cfg);
                return Ok(());
            }
            if cfg.graph_id != graph_id
                || cfg.index_group_size != index_group_size
                || cfg.group_index != group_index
            {
                return Err(VectorIndexError::GraphOwnershipMismatch);
            }
            Ok(())
        })?;
        SHARD_CANISTER_CATALOG
            .with_borrow_mut(|catalog| catalog.insert(shard_id, shard_canister_principal))
            .map_err(|e| match e {
                ShardCanisterCatalogInsertError::ShardAlreadyAttached
                | ShardCanisterCatalogInsertError::CanisterAlreadyAttached => {
                    VectorIndexError::ShardCanisterAlreadyAttached
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

    /// Scans up to `budget` subject keys (resuming after `resume_key`), removing all rows owned by
    /// `shard_id`: for live rows, tombstones the slot and drops the id→slot entry; then removes the
    /// subject clock (the shard is gone). Collects matches before removing to avoid mutating the map
    /// mid-iteration.
    fn purge_subjects_step(
        &self,
        shard_id: ShardId,
        resume_key: &[u8],
        budget: u32,
    ) -> SubjectPurgeStep {
        let mut examined = 0u32;
        let mut to_remove: Vec<SubjectKey> = Vec::new();
        let mut last_key: Option<SubjectKey> = None;
        let mut exhausted = true;
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            let lower = if resume_key.is_empty() {
                Bound::Unbounded
            } else {
                Bound::Excluded(SubjectKey::from_bytes(Cow::Borrowed(resume_key)))
            };
            for entry in subjects.range((lower, Bound::Unbounded)) {
                if examined >= budget {
                    exhausted = false;
                    break;
                }
                examined += 1;
                let key = entry.key();
                if key.subject.shard_id() == shard_id {
                    to_remove.push(*key);
                }
                last_key = Some(*key);
            }
        });

        let removed = u32::try_from(to_remove.len()).unwrap_or(u32::MAX);
        for key in &to_remove {
            let entry = VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(key));
            if let Some(entry) = entry {
                if !entry.deleted {
                    if let Some(slot) = entry.slot {
                        self.tombstone_slot(key.index_id, slot);
                    }
                    if let Some(vector_id) = entry.vector_id {
                        VECTOR_ID_TO_SLOT.with_borrow_mut(|m| {
                            m.remove(&crate::records::VectorIdKey::new(key.index_id, vector_id))
                        });
                    }
                }
            }
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| m.remove(key));
        }

        let resume_key = if exhausted {
            None
        } else {
            last_key.map(Storable::into_bytes)
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
        index_group_size: u32,
        group_index: u32,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), VectorIndexError> {
        self.assert_router_caller(caller)?;
        self.commit_attach_shard_canister(
            graph_id,
            index_group_size,
            group_index,
            shard_id,
            shard_canister_principal,
        )
    }

    /// Performs one bounded step of a shard subject purge. The first call (`resume == None`) also
    /// drops the shard's auth mapping. The router resumes from [`ShardDetachStepResult::next`].
    pub fn admin_detach_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
    ) -> Result<ShardDetachStepResult, VectorIndexError> {
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

    pub(super) fn assert_router_caller(&self, caller: Principal) -> Result<(), VectorIndexError> {
        if caller == Principal::anonymous() {
            return Err(VectorIndexError::Unauthorized);
        }
        let router = VECTOR_INDEX_ROUTER.with_borrow(|r| *r.get());
        if caller != router {
            return Err(VectorIndexError::Unauthorized);
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

/// Convenience for test setup: attach a shard with a single-shard group at `group_index = 0`.
#[cfg(test)]
impl VectorIndexStore {
    pub(crate) fn attach_single_shard_for_test(
        &self,
        router: Principal,
        shard_id: ShardId,
        shard_canister: Principal,
    ) {
        let index_group_size = shard_id.raw() + 1;
        let group_index = 0;
        self.admin_attach_shard_canister(
            router,
            GraphId::from_raw(1),
            index_group_size,
            group_index,
            shard_id,
            shard_canister,
        )
        .expect("attach shard canister");
    }
}
