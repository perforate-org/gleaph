//! Account canister stable-memory wiring (ADR 0068 Slice 1).
//!
//! Keyed by `account_id` string (Personal principal text or Org generated id).

use crate::types::Account;
use ic_stable_memory_backend::{DefaultMemoryImpl, default_memory_impl};
use ic_stable_structures::{
    StableBTreeMap,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};
use std::cell::RefCell;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) const ACCOUNT_BY_ID: MemoryId = MemoryId::new(0);

pub(crate) type StableAccountById = StableBTreeMap<String, Account, Memory>;

pub(crate) fn init_account_by_id() -> StableAccountById {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(ACCOUNT_BY_ID)))
}

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(default_memory_impl()));
    pub(crate) static ACCOUNTS: RefCell<StableAccountById> = RefCell::new(init_account_by_id());
}

/// Test-only: clear all account state. Call at the start of any test that mutates state to avoid
/// thread-local interference between tests on the same thread.
#[cfg(test)]
pub(crate) fn reset_all() {
    ACCOUNTS.with_borrow_mut(|map| map.clear_new());
}
