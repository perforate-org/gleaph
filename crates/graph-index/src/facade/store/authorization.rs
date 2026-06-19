//! Router authorization and shard/canister attachment registry.

use super::IndexStore;
use crate::edge_key::EdgePostingKey;
use crate::init::IndexInitArgs;
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{
    ShardDetachCursor, ShardDetachPhase, ShardDetachStepResult, ShardId,
};
use ic_stable_structures::{BTreeSet, Memory, Storable};
use std::borrow::Cow;
use std::ops::Bound;

use crate::facade::stable::memory::ShardCanisterCatalogInsertError;
use crate::facade::stable::{
    INDEX_EDGE_POSTINGS, INDEX_OWNERSHIP_CONFIG, INDEX_ROUTER, INDEX_SHARD_CANISTER_CATALOG,
    INDEX_VERTEX_LABEL_POSTINGS, INDEX_VERTEX_POSTINGS,
};

/// Upper bound on posting keys examined per [`IndexStore::admin_detach_shard_canister`]
/// step. Keeps a single detach message within the canister instruction and
/// stable read/write budgets regardless of total index size; the router resumes
/// until the purge reports `done`.
const MAX_DETACH_EXAMINE_PER_STEP: u32 = 20_000;

/// A posting key carrying the `shard_id` used to scope detach purges.
trait ShardScopedPostingKey: Storable + Ord + Clone {
    fn key_shard_id(&self) -> ShardId;
}

impl ShardScopedPostingKey for PostingKey {
    fn key_shard_id(&self) -> ShardId {
        self.shard_id
    }
}

impl ShardScopedPostingKey for LabelPostingKey {
    fn key_shard_id(&self) -> ShardId {
        self.shard_id
    }
}

impl ShardScopedPostingKey for EdgePostingKey {
    fn key_shard_id(&self) -> ShardId {
        self.shard_id
    }
}

/// Outcome of purging up to `budget` keys from one posting set.
struct PhaseStep {
    examined: u32,
    removed: u32,
    /// Resume key when the set was not fully scanned, or `None` when exhausted.
    resume_key: Option<Vec<u8>>,
}

/// Scans up to `budget` keys of `set` (resuming after `resume_key`), removing
/// those owned by `shard_id`. Collects matches before removing so the scan does
/// not mutate the set mid-iteration.
fn purge_postings_step<K: ShardScopedPostingKey, M: Memory>(
    set: &mut BTreeSet<K, M>,
    shard_id: ShardId,
    resume_key: &[u8],
    budget: u32,
) -> PhaseStep {
    let mut examined = 0u32;
    let mut to_remove: Vec<K> = Vec::new();
    let mut last_key: Option<K> = None;
    let mut exhausted = true;
    {
        let lower = if resume_key.is_empty() {
            Bound::Unbounded
        } else {
            Bound::Excluded(K::from_bytes(Cow::Borrowed(resume_key)))
        };
        for key in set.range((lower, Bound::Unbounded)) {
            if examined >= budget {
                exhausted = false;
                break;
            }
            examined += 1;
            if key.key_shard_id() == shard_id {
                to_remove.push(key.clone());
            }
            last_key = Some(key);
        }
    }
    let removed = u32::try_from(to_remove.len()).unwrap_or(u32::MAX);
    for key in &to_remove {
        set.remove(key);
    }
    let resume_key = if exhausted {
        None
    } else {
        last_key.map(Storable::into_bytes)
    };
    PhaseStep {
        examined,
        removed,
        resume_key,
    }
}

impl IndexStore {
    /// Clears shard/canister catalog and postings; seeds router principal from init args.
    ///
    /// Validates `router_canister` before mutating any stable state: an anonymous router is
    /// rejected up front so a failed init never clears the catalog/postings or persists an
    /// anonymous (and therefore untrusted) router principal.
    pub fn init_from_args(&self, args: &IndexInitArgs) -> Result<(), IndexError> {
        if args.router_canister == Principal::anonymous() {
            return Err(IndexError::AnonymousRouter);
        }
        INDEX_SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| catalog.clear_new());
        INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_ROUTER.with_borrow_mut(|router| {
            router.set(args.router_canister);
        });
        INDEX_OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
            cell.set(crate::facade::stable::memory::IndexOwnershipConfig::default());
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
    ) -> Result<(), IndexError> {
        if shard_canister_principal == Principal::anonymous() {
            return Err(IndexError::InvalidPrincipalInRegistry);
        }
        if index_group_size == 0 {
            return Err(IndexError::InvalidIndexGroupConfig);
        }
        let group_start = u64::from(group_index) * u64::from(index_group_size);
        let group_end = group_start + u64::from(index_group_size);
        let shard_raw = u64::from(shard_id.raw());
        if shard_raw < group_start || shard_raw >= group_end {
            return Err(IndexError::ShardOutOfRangeForGroup);
        }
        INDEX_OWNERSHIP_CONFIG.with_borrow_mut(|cell| {
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
                return Err(IndexError::GraphOwnershipMismatch);
            }
            Ok(())
        })?;
        INDEX_SHARD_CANISTER_CATALOG
            .with_borrow_mut(|catalog| catalog.insert(shard_id, shard_canister_principal))
            .map_err(|e| match e {
                ShardCanisterCatalogInsertError::ShardAlreadyAttached { .. } => {
                    IndexError::ShardCanisterAlreadyAttached
                }
                ShardCanisterCatalogInsertError::CanisterAlreadyAttached { .. } => {
                    IndexError::ShardCanisterAlreadyAttached
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
                // Drop the auth mapping first so the shard canister can no longer
                // insert postings while the bounded purge runs across steps.
                INDEX_SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| {
                    catalog.remove_shard(shard_id);
                });
                ShardDetachCursor {
                    phase: ShardDetachPhase::Vertex,
                    resume_key: Vec::new(),
                }
            }
        };

        let step = match cursor.phase {
            ShardDetachPhase::Vertex => INDEX_VERTEX_POSTINGS.with_borrow_mut(|set| {
                purge_postings_step(set, shard_id, &cursor.resume_key, budget)
            }),
            ShardDetachPhase::Label => INDEX_VERTEX_LABEL_POSTINGS.with_borrow_mut(|set| {
                purge_postings_step(set, shard_id, &cursor.resume_key, budget)
            }),
            ShardDetachPhase::Edge => INDEX_EDGE_POSTINGS.with_borrow_mut(|set| {
                purge_postings_step(set, shard_id, &cursor.resume_key, budget)
            }),
        };

        let next = match step.resume_key {
            Some(resume_key) => Some(ShardDetachCursor {
                phase: cursor.phase,
                resume_key,
            }),
            None => match cursor.phase {
                ShardDetachPhase::Vertex => Some(ShardDetachCursor {
                    phase: ShardDetachPhase::Label,
                    resume_key: Vec::new(),
                }),
                ShardDetachPhase::Label => Some(ShardDetachCursor {
                    phase: ShardDetachPhase::Edge,
                    resume_key: Vec::new(),
                }),
                ShardDetachPhase::Edge => None,
            },
        };

        ShardDetachStepResult {
            done: next.is_none(),
            next,
            examined: step.examined,
            removed: step.removed,
        }
    }

    pub(super) fn commit_detach_shard_step(
        &self,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
    ) -> ShardDetachStepResult {
        self.commit_detach_shard_step_with_budget(shard_id, resume, MAX_DETACH_EXAMINE_PER_STEP)
    }

    pub fn admin_attach_shard_canister(
        &self,
        caller: Principal,
        graph_id: GraphId,
        index_group_size: u32,
        group_index: u32,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        self.commit_attach_shard_canister(
            graph_id,
            index_group_size,
            group_index,
            shard_id,
            shard_canister_principal,
        )
    }

    /// Performs one bounded step of a shard posting purge. The first call
    /// (`resume == None`) also drops the shard's auth mapping. The router resumes
    /// from [`ShardDetachStepResult::next`] until `done`.
    pub fn admin_detach_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
    ) -> Result<ShardDetachStepResult, IndexError> {
        self.assert_router_caller(caller)?;
        Ok(self.commit_detach_shard_step(shard_id, resume))
    }

    #[cfg(test)]
    pub fn detach_shard_step_for_test(
        &self,
        shard_id: ShardId,
        resume: Option<ShardDetachCursor>,
        budget: u32,
    ) -> ShardDetachStepResult {
        self.commit_detach_shard_step_with_budget(shard_id, resume, budget)
    }

    pub(super) fn assert_router_caller(&self, caller: Principal) -> Result<(), IndexError> {
        // Defense in depth: the anonymous principal is never the trusted router, even if a corrupt
        // router record named it.
        if caller == Principal::anonymous() {
            return Err(IndexError::NotAuthorized);
        }
        let router = INDEX_ROUTER.with_borrow(|r| *r.get());
        if caller != router {
            return Err(IndexError::NotAuthorized);
        }
        Ok(())
    }

    pub(super) fn assert_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
    ) -> Result<(), IndexError> {
        let registered =
            INDEX_SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_canister(shard_id));
        let Some(reg) = registered else {
            return Err(IndexError::UnknownShard);
        };
        if caller != reg {
            return Err(IndexError::WrongShardCanister);
        }
        Ok(())
    }
}
