//! Router authorization and shard-owner registry.

use super::IndexStore;
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;

use crate::facade::stable::{
    INDEX_ADMINS, INDEX_LABEL_POSTINGS, INDEX_POSTINGS, INDEX_ROUTER, INDEX_SHARD_OWNERS,
};

impl IndexStore {
    /// Clears admins, shard owners, postings; seeds admins and router principal from init args.
    pub fn init_from_args(&self, args: &IndexInitArgs) {
        INDEX_ADMINS.with_borrow_mut(|admins| {
            admins.clear();
            for p in &args.controllers {
                admins.insert(*p);
            }
        });
        INDEX_SHARD_OWNERS.with_borrow_mut(|shards| shards.clear_new());
        INDEX_POSTINGS.with_borrow_mut(|postings| postings.clear());
        INDEX_LABEL_POSTINGS.with_borrow_mut(|postings| postings.clear());
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

    pub fn admin_set_shard_owner(
        &self,
        caller: Principal,
        shard_id: ShardId,
        owner_principal: Principal,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        if owner_principal == Principal::anonymous() {
            return Err(IndexError::InvalidPrincipalInRegistry);
        }
        let existing = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        if let Some(p) = existing {
            if p != owner_principal {
                return Err(IndexError::ShardAlreadyRegistered);
            }
            return Ok(());
        }
        INDEX_SHARD_OWNERS.with_borrow_mut(|shards| {
            shards.insert(shard_id, owner_principal);
        });
        Ok(())
    }

    pub fn admin_clear_shard_owner(
        &self,
        caller: Principal,
        shard_id: ShardId,
    ) -> Result<(), IndexError> {
        self.assert_router_caller(caller)?;
        INDEX_SHARD_OWNERS.with_borrow_mut(|shards| {
            shards.remove(&shard_id);
        });
        Ok(())
    }

    pub(super) fn assert_router_caller(&self, caller: Principal) -> Result<(), IndexError> {
        let router = INDEX_ROUTER.with_borrow(|r| *r.get());
        if caller != router {
            return Err(IndexError::NotAuthorized);
        }
        Ok(())
    }

    pub(super) fn assert_shard_owner(
        &self,
        caller: Principal,
        shard_id: ShardId,
    ) -> Result<(), IndexError> {
        let registered = INDEX_SHARD_OWNERS.with_borrow(|shards| shards.get(&shard_id));
        let Some(reg) = registered else {
            return Err(IndexError::UnknownShard);
        };
        if caller != reg {
            return Err(IndexError::WrongShardOwner);
        }
        Ok(())
    }
}
