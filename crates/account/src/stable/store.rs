//! Account canister stable-memory store facade (ADR 0068 Slice 1).

use crate::stable::memory;
use crate::types::{Account, AccountError, Role, RouterEntry};
use candid::Principal;

/// Durable account store. Keyed by `account_id`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AccountStore;

impl AccountStore {
    pub fn new() -> Self {
        Self
    }

    pub fn get(&self, account_id: &str) -> Option<Account> {
        memory::ACCOUNTS.with_borrow(|map| map.get(&account_id.to_owned()))
    }

    /// Insert an account. Returns `AlreadyExists` if the id is taken.
    pub fn insert(&self, account: Account) -> Result<(), AccountError> {
        let id = account.id();
        let existed = memory::ACCOUNTS.with_borrow_mut(|map| map.insert(id, account).is_some());
        if existed {
            return Err(AccountError::AlreadyExists);
        }
        Ok(())
    }

    pub fn remove(&self, account_id: &str) -> Option<Account> {
        memory::ACCOUNTS.with_borrow_mut(|map| map.remove(&account_id.to_owned()))
    }

    /// Apply a membership change to an existing Org account. Returns NotFound / NotAuthorized /
    /// InvalidRole as appropriate.
    pub fn upsert_member(
        &self,
        account_id: &str,
        caller: Principal,
        target: Principal,
        role: Role,
    ) -> Result<(), AccountError> {
        memory::ACCOUNTS.with_borrow_mut(|map| {
            let mut account = map
                .get(&account_id.to_owned())
                .ok_or(AccountError::NotFound)?;
            if !account.is_owner(&caller) {
                return Err(AccountError::NotAuthorized);
            }
            match &mut account {
                Account::Personal { .. } => return Err(AccountError::NotAuthorized),
                Account::Org { members, .. } => {
                    members.insert(target, role);
                }
            }
            map.insert(account_id.to_owned(), account);
            Ok(())
        })
    }

    /// Remove a member from an existing Org account.
    pub fn remove_member(
        &self,
        account_id: &str,
        caller: Principal,
        target: Principal,
    ) -> Result<(), AccountError> {
        memory::ACCOUNTS.with_borrow_mut(|map| {
            let mut account = map
                .get(&account_id.to_owned())
                .ok_or(AccountError::NotFound)?;
            if !account.is_owner(&caller) {
                return Err(AccountError::NotAuthorized);
            }
            match &mut account {
                Account::Personal { .. } => return Err(AccountError::NotAuthorized),
                Account::Org { members, .. } => {
                    members.remove(&target);
                }
            }
            map.insert(account_id.to_owned(), account);
            Ok(())
        })
    }

    /// List account ids whose member set contains `caller` (owner/admin/member).
    pub fn accounts_of(&self, caller: Principal) -> Vec<String> {
        memory::ACCOUNTS.with_borrow(|map| {
            map.iter()
                .filter(|entry| entry.value().is_member(&caller))
                .map(|entry| entry.key().clone())
                .collect()
        })
    }

    /// Register a Router under an account. Owner/admin only. Returns AlreadyExists if the
    /// `router_id` is taken.
    pub fn register_router(
        &self,
        account_id: &str,
        caller: Principal,
        router: RouterEntry,
    ) -> Result<(), AccountError> {
        memory::ACCOUNTS.with_borrow_mut(|map| {
            let mut account = map
                .get(&account_id.to_owned())
                .ok_or(AccountError::NotFound)?;
            if !account.is_owner_or_admin(&caller) {
                return Err(AccountError::NotAuthorized);
            }
            let routers = match &mut account {
                Account::Personal { routers, .. } => routers,
                Account::Org { routers, .. } => routers,
            };
            if routers.contains_key(&router.router_id) {
                return Err(AccountError::AlreadyExists);
            }
            routers.insert(router.router_id.clone(), router);
            map.insert(account_id.to_owned(), account);
            Ok(())
        })
    }

    /// Unregister a Router. Owner only.
    pub fn unregister_router(
        &self,
        account_id: &str,
        caller: Principal,
        router_id: &str,
    ) -> Result<(), AccountError> {
        memory::ACCOUNTS.with_borrow_mut(|map| {
            let mut account = map
                .get(&account_id.to_owned())
                .ok_or(AccountError::NotFound)?;
            if !account.is_owner(&caller) {
                return Err(AccountError::NotAuthorized);
            }
            let routers = match &mut account {
                Account::Personal { routers, .. } => routers,
                Account::Org { routers, .. } => routers,
            };
            routers.remove(router_id);
            map.insert(account_id.to_owned(), account);
            Ok(())
        })
    }

    /// List the account's Routers. Any member.
    pub fn list_routers(
        &self,
        account_id: &str,
        caller: Principal,
    ) -> Result<Vec<RouterEntry>, AccountError> {
        let account = self.get(account_id).ok_or(AccountError::NotFound)?;
        if !account.is_member(&caller) {
            return Err(AccountError::NotAuthorized);
        }
        Ok(match &account {
            Account::Personal { routers, .. } => routers.values().cloned().collect(),
            Account::Org { routers, .. } => routers.values().cloned().collect(),
        })
    }

    /// Resolve a Router's canister id. Any member. Returns NotFound if the Router is not issued.
    pub fn resolve_router(
        &self,
        account_id: &str,
        caller: Principal,
        router_id: &str,
    ) -> Result<Principal, AccountError> {
        let account = self.get(account_id).ok_or(AccountError::NotFound)?;
        if !account.is_member(&caller) {
            return Err(AccountError::NotAuthorized);
        }
        let routers = match &account {
            Account::Personal { routers, .. } => routers,
            Account::Org { routers, .. } => routers,
        };
        routers
            .get(router_id)
            .map(|r| r.router_canister)
            .ok_or(AccountError::NotFound)
    }
}
