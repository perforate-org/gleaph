//! Router authorization and shard/canister attachment registry for the vector index.

use super::VectorCanisterStore;
use crate::facade::stable::definition_store;
use crate::facade::stable::memory::{
    ActiveShardDetach, ShardCanisterCatalogInsertError, VectorIndexOwnershipConfig,
};
use crate::facade::stable::subject_store;
#[cfg(any(test, feature = "canbench"))]
use crate::facade::stable::{DefinitionDomainResetError, reset_definition_domain};
use crate::facade::stable::{
    OWNERSHIP_CONFIG, SHARD_CANISTER_CATALOG, VECTOR_DELETED_SUBJECTS, VECTOR_INDEX_ROUTER,
};
use crate::init::VectorCanisterInitArgs;
use crate::records::SubjectScanScope;
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

/// Maximum number of distinct shard detach sessions retained in the ownership control cell.
pub(super) const MAX_CONCURRENT_SHARD_DETACHES: usize = 64;

impl VectorCanisterStore {
    fn assert_attach_allowed(&self, shard_id: ShardId) -> Result<(), VectorCanisterError> {
        OWNERSHIP_CONFIG.with_borrow(|cell| {
            let active = cell.get().active_detaches.as_deref().unwrap_or_default();
            if active.iter().any(|detach| detach.shard_id == shard_id) {
                return Err(VectorCanisterError::DetachInProgress);
            }
            Ok(())
        })
    }

    fn begin_or_resume_detach(&self, shard_id: ShardId) -> Result<u64, VectorCanisterError> {
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            let mut config = cell.get().clone();
            if let Some(generation) = config
                .active_detaches
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find_map(|detach| (detach.shard_id == shard_id).then_some(detach.generation))
            {
                return Ok(generation);
            }
            if config.active_detaches.as_ref().map_or(0, Vec::len) >= MAX_CONCURRENT_SHARD_DETACHES
            {
                return Err(VectorCanisterError::TooManyActiveDetaches);
            }

            let generation = config.next_detach_generation.unwrap_or(1);
            let next_generation = generation
                .checked_add(1)
                .ok_or(VectorCanisterError::DetachGenerationExhausted)?;
            config
                .active_detaches
                .get_or_insert_with(Vec::new)
                .push(ActiveShardDetach {
                    shard_id,
                    generation,
                });
            config.next_detach_generation = Some(next_generation);
            cell.set(config);
            Ok(generation)
        })
    }

    fn validate_detach_resume(
        &self,
        shard_id: ShardId,
        generation: Option<u64>,
    ) -> Result<u64, VectorCanisterError> {
        let generation = generation.ok_or(VectorCanisterError::LegacyOrStaleDetachCursor)?;
        OWNERSHIP_CONFIG.with_borrow(|cell| {
            let valid = cell
                .get()
                .active_detaches
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|detach| detach.shard_id == shard_id && detach.generation == generation);
            valid
                .then_some(generation)
                .ok_or(VectorCanisterError::LegacyOrStaleDetachCursor)
        })
    }

    fn finish_detach(&self, shard_id: ShardId, generation: u64) -> Result<(), VectorCanisterError> {
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            let mut config = cell.get().clone();
            let active = config
                .active_detaches
                .as_mut()
                .ok_or(VectorCanisterError::LegacyOrStaleDetachCursor)?;
            let position = active
                .iter()
                .position(|detach| detach.shard_id == shard_id && detach.generation == generation)
                .ok_or(VectorCanisterError::LegacyOrStaleDetachCursor)?;
            active.remove(position);
            cell.set(config);
            Ok(())
        })
    }

    /// Strictly creates the definition region and seeds the router principal from init args.
    ///
    /// Rejects an anonymous router before mutating any stable state.
    pub fn init_from_args(&self, args: &VectorCanisterInitArgs) -> Result<(), VectorCanisterError> {
        if args.router_canister == Principal::anonymous() {
            return Err(VectorCanisterError::AnonymousRouter);
        }
        definition_store::create_for_install(args.definition_map_seed)
            .map_err(super::legacy_definition_store_error)?;
        subject_store::create_for_install(args.subject_map_seed)
            .map_err(super::legacy_subject_store_error)?;
        super::centroid_cache::clear_all();
        VECTOR_INDEX_ROUTER.with_borrow_mut(|router| {
            router.set(args.router_canister);
        });
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            cell.set(VectorIndexOwnershipConfig::default());
        });
        Ok(())
    }

    /// Test/benchmark fixture reset only.  Production install is strict-create only; coordinated
    /// destructive reset of every Vector region remains a separately authorized lifecycle action.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn reset_for_test_or_bench(
        &self,
        args: &VectorCanisterInitArgs,
    ) -> Result<(), VectorCanisterError> {
        if args.router_canister == Principal::anonymous() {
            return Err(VectorCanisterError::AnonymousRouter);
        }
        definition_store::bind_for_fixture(args.definition_map_seed)
            .map_err(super::legacy_definition_store_error)?;
        subject_store::bind_for_fixture(args.subject_map_seed)
            .map_err(super::legacy_subject_store_error)?;
        let expected_incarnation = definition_store::incarnation_for_test_or_bench()
            .map_err(super::legacy_definition_store_error)?;
        let subject_incarnation = subject_store::incarnation_for_test_or_bench()
            .map_err(super::legacy_subject_store_error)?;
        self.reset_definition_domain(expected_incarnation, subject_incarnation)
            .map_err(|error| match error {
                DefinitionDomainResetError::Definition(error) => {
                    super::legacy_definition_store_error(error)
                }
                DefinitionDomainResetError::Subject(error) => {
                    super::legacy_subject_store_error(error)
                }
                DefinitionDomainResetError::RegionHandleUnavailable(_) => {
                    VectorCanisterError::StableGrowFailed
                }
            })?;
        SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| catalog.clear_new());
        VECTOR_INDEX_ROUTER.with_borrow_mut(|router| {
            router.set(args.router_canister);
        });
        OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            cell.set(VectorIndexOwnershipConfig::default());
        });
        Ok(())
    }

    /// Owner-only reset of state whose interpretation depends on the definition table.
    ///
    /// Router authority, graph ownership, shard attachments, ingestion watermarks, and the GC
    /// cursor are independent lifecycle state and deliberately survive this operation.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn reset_definition_domain(
        &self,
        expected_incarnation: u64,
        subject_incarnation: u64,
    ) -> Result<u64, DefinitionDomainResetError> {
        super::centroid_cache::preflight_clear()
            .map_err(|()| DefinitionDomainResetError::RegionHandleUnavailable("CENTROID_CACHE"))?;
        let ticket = definition_store::prepare_reset(expected_incarnation)?;
        let subject_ticket = subject_store::prepare_reset(subject_incarnation)?;
        let incarnation = reset_definition_domain(ticket, subject_ticket)?;
        super::centroid_cache::clear_all();
        Ok(incarnation)
    }

    /// Binds the definition owner after an upgrade without creating or resetting MemoryId 4.
    pub(crate) fn open_definition_store_after_upgrade(&self) {
        let _ = definition_store::open_after_upgrade();
        super::centroid_cache::clear_all();
    }

    /// Binds the subject owner after an upgrade without creating or resetting MemoryId 7.
    pub(crate) fn open_subject_store_after_upgrade(&self) {
        let _ = subject_store::open_after_upgrade();
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
        self.assert_attach_allowed(shard_id)?;
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
    ) -> Result<ShardDetachStepResult, VectorCanisterError> {
        let (cursor, generation) = match resume {
            Some(cursor) => {
                let generation = self.validate_detach_resume(shard_id, cursor.detach_generation)?;
                if cursor.phase != ShardDetachPhase::Vertex {
                    return Err(VectorCanisterError::LegacyOrStaleDetachCursor);
                }
                (cursor, generation)
            }
            None => {
                let generation = self.begin_or_resume_detach(shard_id)?;
                // Drop the auth mapping first so the shard can no longer write while the bounded
                // purge runs across steps.
                SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| {
                    catalog.remove_shard(shard_id);
                });
                (
                    ShardDetachCursor {
                        detach_generation: Some(generation),
                        phase: ShardDetachPhase::Vertex,
                        resume_key: Vec::new(),
                    },
                    generation,
                )
            }
        };

        let step = self.purge_subjects_step(shard_id, &cursor.resume_key, budget)?;
        let next = step.resume_key.map(|resume_key| ShardDetachCursor {
            detach_generation: Some(generation),
            phase: ShardDetachPhase::Vertex,
            resume_key,
        });

        if next.is_none() {
            self.finish_detach(shard_id, generation)
                .expect("validated detach session remains active until finalization");
        }

        Ok(ShardDetachStepResult {
            done: next.is_none(),
            next,
            examined: step.examined,
            removed: step.removed,
        })
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
    ) -> Result<SubjectPurgeStep, VectorCanisterError> {
        let scope = SubjectScanScope::Detach {
            shard_id: shard_id.raw(),
        };
        let cursor = if resume_key.is_empty() {
            subject_store::scan_start(scope)
        } else {
            SubjectScanCursor::decode_bytes(scope, resume_key).map_err(|_| {
                subject_store::SubjectStoreError::Scan(
                    subject_store::SubjectStoreScanError::InvalidCursor,
                )
            })
        };
        let cursor = match cursor {
            Ok(cursor) => Ok(cursor),
            Err(subject_store::SubjectStoreError::Scan(
                subject_store::SubjectStoreScanError::RestartRequired,
            )) => subject_store::scan_start(scope),
            Err(error) => Err(error),
        };
        let cursor = cursor.map_err(super::legacy_subject_store_error)?;
        let page = match subject_store::scan_step(scope, cursor, u64::from(budget)) {
            Ok(page) => Ok(page),
            Err(subject_store::SubjectStoreError::Scan(
                subject_store::SubjectStoreScanError::RestartRequired,
            )) => {
                let fresh =
                    subject_store::scan_start(scope).map_err(super::legacy_subject_store_error)?;
                subject_store::scan_step(scope, fresh, u64::from(budget))
            }
            Err(subject_store::SubjectStoreError::Scan(
                subject_store::SubjectStoreScanError::InProgress,
            )) => {
                return Ok(SubjectPurgeStep {
                    examined: 0,
                    removed: 0,
                    resume_key: (!resume_key.is_empty()).then(|| resume_key.to_vec()),
                });
            }
            Err(error) => Err(error),
        }
        .map_err(super::legacy_subject_store_error)?;
        let examined = u32::try_from(page.examined_slots).unwrap_or(u32::MAX);
        let to_remove: Vec<SubjectKey> = page
            .entries
            .iter()
            .filter_map(|(key, _)| (key.subject.shard_id() == shard_id).then_some(*key))
            .collect();

        let removed = u32::try_from(to_remove.len()).unwrap_or(u32::MAX);
        for key in &to_remove {
            let entry = subject_store::get(key).map_err(super::legacy_subject_store_error)?;
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
                }
                if let Some(slot) = entry.slot {
                    self.tombstone_slot(key.index_id, slot);
                }
                if let Some(shadow_slot) = entry
                    .shadow_slot
                    .filter(|shadow_slot| Some(*shadow_slot) != entry.slot)
                {
                    self.tombstone_slot(key.index_id, shadow_slot);
                }
            }
            subject_store::remove(key).map_err(super::legacy_subject_store_error)?;
        }

        let resume_key = if page.exhausted {
            None
        } else {
            Some(page.next_cursor.encode_bytes())
        };
        Ok(SubjectPurgeStep {
            examined,
            removed,
            resume_key,
        })
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

    /// Performs one bounded step of a generation-fenced shard subject purge. `resume == None`
    /// begins or restarts the shard's active session before dropping authorization. The router
    /// resumes from [`ShardDetachStepResult::next`].
    pub fn admin_detach_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
    ) -> Result<ShardDetachStepResult, VectorCanisterError> {
        self.assert_router_caller(caller)?;
        self.commit_detach_shard_step_with_budget(shard_id, resume, MAX_DETACH_EXAMINE_PER_STEP)
    }

    #[cfg(test)]
    pub(crate) fn detach_shard_step_for_test(
        &self,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
        budget: u32,
    ) -> Result<ShardDetachStepResult, VectorCanisterError> {
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

#[cfg(test)]
mod coordinated_reset_tests;

/// Outcome of purging up to `budget` subject keys for one shard.
struct SubjectPurgeStep {
    examined: u32,
    removed: u32,
    resume_key: Option<Vec<u8>>,
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
