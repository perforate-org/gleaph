//! Provision canister durable bootstrap authority + audit-log facade (ADR 0035 Slice 7).
//!
//! Two independent stable structures on two dedicated MemoryIds:
//! - `PROVISION_BOOTSTRAP_AUTH` (MemoryId 4): `StableCell<Option<BootstrapAuthorityRecord>>` singleton.
//! - `PROVISION_BOOTSTRAP_AUDIT_LOG` (MemoryId 5): `StableBTreeMap<Principal, BootstrapAuthHistory>`.
//!
//! Placing both structures on the same MemoryId would corrupt them at runtime because each
//! `ic_stable_structures` collection writes at offset 0 of its own `MemoryId`.

use crate::stable::memory::{
    MEMORY_MANAGER, Memory, PROVISION_BOOTSTRAP_AUDIT_LOG, PROVISION_BOOTSTRAP_AUTH,
};
use crate::types::{BootstrapAuthEntry, BootstrapAuthHistory, BootstrapAuthorityRecord};
use candid::Principal;
use ic_stable_structures::{StableBTreeMap, StableCell};
use std::cell::RefCell;

thread_local! {
    static BOOTSTRAP_AUTH: RefCell<StableCell<Option<BootstrapAuthorityRecord>, Memory>> =
        RefCell::new(init_bootstrap_auth_cell());
    static BOOTSTRAP_AUDIT_LOG: RefCell<StableBTreeMap<Principal, BootstrapAuthHistory, Memory>> =
        RefCell::new(init_bootstrap_audit_log_map());
}

fn init_bootstrap_auth_cell() -> StableCell<Option<BootstrapAuthorityRecord>, Memory> {
    StableCell::new(
        MEMORY_MANAGER.with(|mm| mm.borrow().get(PROVISION_BOOTSTRAP_AUTH)),
        None,
    )
}

fn init_bootstrap_audit_log_map() -> StableBTreeMap<Principal, BootstrapAuthHistory, Memory> {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(PROVISION_BOOTSTRAP_AUDIT_LOG)))
}

/// Facade for the durable bootstrap authority singleton and the per-governance audit log.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProvisionBootstrapAuthStore;

impl ProvisionBootstrapAuthStore {
    pub fn new() -> Self {
        Self
    }

    /// Read the singleton bootstrap authority record, if it has been seeded.
    pub fn get_authority(&self) -> Option<BootstrapAuthorityRecord> {
        BOOTSTRAP_AUTH.with_borrow(|cell| cell.get().clone())
    }

    /// First init-time write of the singleton. Uses `StableCell::init` to read any existing
    /// bytes; on a fresh canister the thread-local `StableCell::new` has already written a
    /// `None` header, so we overwrite with `set` only when the cell is currently empty. This
    /// makes the call idempotent across upgrades while still establishing the seed on first init.
    pub fn init_authority(&self, record: BootstrapAuthorityRecord) {
        let memory = MEMORY_MANAGER.with(|mm| mm.borrow().get(PROVISION_BOOTSTRAP_AUTH));
        BOOTSTRAP_AUTH.with_borrow_mut(|cell| {
            *cell = StableCell::init(memory, Some(record.clone()));
            if cell.get().is_none() {
                cell.set(Some(record));
            }
        });
    }

    /// Subsequent overwrite of the singleton (used by upgrade/re-init forward tests).
    pub fn set_authority(&self, record: BootstrapAuthorityRecord) {
        BOOTSTRAP_AUTH.with_borrow_mut(|cell| {
            cell.set(Some(record));
        });
    }

    /// Append one audit row under the caller's governance principal.
    pub fn put_record(&self, principal: Principal, entry: BootstrapAuthEntry) {
        BOOTSTRAP_AUDIT_LOG.with_borrow_mut(|map| {
            let mut history = map
                .get(&principal)
                .unwrap_or_else(|| BootstrapAuthHistory { entries: vec![] });
            history.entries.push(entry);
            map.insert(principal, history);
        });
    }

    /// Return the full audit history for a governance principal.
    pub fn history(&self, principal: Principal) -> Vec<BootstrapAuthEntry> {
        BOOTSTRAP_AUDIT_LOG
            .with_borrow(|map| map.get(&principal).map(|h| h.entries).unwrap_or_default())
    }

    /// Return the latest audit entry for a governance principal, if any.
    pub fn latest(&self, principal: Principal) -> Option<BootstrapAuthEntry> {
        BOOTSTRAP_AUDIT_LOG
            .with_borrow(|map| map.get(&principal).and_then(|h| h.entries.last().cloned()))
    }
}

/// Test-only helper to clear the bootstrap authority singleton and audit log.
#[cfg(test)]
pub(crate) fn reset_bootstrap_auth_maps() {
    BOOTSTRAP_AUTH.with_borrow_mut(|cell| {
        cell.set(None);
    });
    BOOTSTRAP_AUDIT_LOG.with_borrow_mut(|map| map.clear_new());
}
