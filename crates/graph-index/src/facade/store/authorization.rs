//! Router authorization and shard/canister attachment registry.

use super::IndexStore;
use crate::edge_key::EdgePostingKey;
use crate::init::IndexInitArgs;
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;

use crate::facade::stable::memory::ShardCanisterCatalogInsertError;
use crate::facade::stable::{
    INDEX_EDGE_POSTINGS, INDEX_OWNERSHIP_CONFIG, INDEX_ROUTER, INDEX_SHARD_CANISTER_CATALOG,
    INDEX_VERTEX_LABEL_POSTINGS, INDEX_VERTEX_POSTINGS,
};

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

    pub(super) fn commit_detach_shard_canister(&self, shard_id: ShardId) {
        INDEX_SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| {
            catalog.remove_shard(shard_id);
        });
        INDEX_VERTEX_POSTINGS.with_borrow_mut(|postings| {
            let stale: Vec<PostingKey> = postings
                .iter()
                .filter(|key| key.shard_id == shard_id)
                .collect();
            for key in stale {
                postings.remove(&key);
            }
        });
        INDEX_VERTEX_LABEL_POSTINGS.with_borrow_mut(|postings| {
            let stale: Vec<LabelPostingKey> = postings
                .iter()
                .filter(|key| key.shard_id == shard_id)
                .collect();
            for key in stale {
                postings.remove(&key);
            }
        });
        INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| {
            let stale: Vec<EdgePostingKey> = postings
                .iter()
                .filter(|key| key.shard_id == shard_id)
                .collect();
            for key in stale {
                postings.remove(&key);
            }
        });
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

    pub fn admin_detach_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        self.commit_detach_shard_canister(shard_id);
        Ok(())
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
