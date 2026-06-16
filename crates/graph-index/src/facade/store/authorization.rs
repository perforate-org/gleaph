//! Router authorization and shard/canister attachment registry.

use super::IndexStore;
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;

use crate::facade::stable::memory::ShardCanisterCatalogInsertError;
use crate::facade::stable::{
    INDEX_ADMINS, INDEX_EDGE_POSTINGS, INDEX_LABEL_POSTINGS, INDEX_POSTINGS, INDEX_ROUTER,
    INDEX_SHARD_CANISTER_CATALOG,
};

impl IndexStore {
    /// Clears admins, shard/canister catalog, postings; seeds admins and router principal from init args.
    pub fn init_from_args(&self, args: &IndexInitArgs) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            admins.clear();
            for p in &args.controllers {
                admins.insert(*p);
            }
        });
        INDEX_SHARD_CANISTER_CATALOG.with_borrow_mut(|catalog| catalog.clear_new());
        INDEX_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_LABEL_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_EDGE_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_ROUTER.with_borrow_mut(|router| {
            router.set(args.router_canister);
        });
    }

    pub fn bootstrap_admins(&self, principals: &[Principal]) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            for p in principals {
                admins.insert(*p);
            }
        });
    }

    pub(super) fn commit_attach_shard_canister(
        &self,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), IndexError> {
        if shard_canister_principal == Principal::anonymous() {
            return Err(IndexError::InvalidPrincipalInRegistry);
        }
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
    }

    pub fn admin_attach_shard_canister(
        &self,
        caller: Principal,
        shard_id: ShardId,
        shard_canister_principal: Principal,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        self.commit_attach_shard_canister(shard_id, shard_canister_principal)
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
