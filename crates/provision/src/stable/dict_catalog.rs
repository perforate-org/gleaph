//! Provision canister dictionary catalog facade (plan 0335 todo 1).
//!
//! Wraps two dedicated stable regions:
//! - `PROVISION_DICT_CATALOG` (MemoryId 13): `DictCatalogKey -> DictCatalogEntry`.
//! - `PROVISION_DICT_CHUNKS` (MemoryId 14): append-oriented `DictChunkKey -> DictChunk`
//!   holding the ≤1 MiB compressed chunks (ADR 0087 chunk-store shape).
//!
//! The facade is a regular struct (not a singleton) so handlers instantiate it per call,
//! mirroring `ProvisionArtifactStore`. Controller checks live in the handler, not here.
//! Provision stores and verifies compressed bytes only — compression and decompression are
//! off-band (the text canister owns both).

use super::memory::{
    StableDictCatalogMap, StableDictChunksMap, init_dict_catalog, init_dict_chunks,
};
use crate::types::{
    DictCatalogAuditEntry, DictCatalogEntry, DictCatalogKey, DictChunk, DictChunkKey,
    MAX_DICT_CATALOG_AUDIT_PER_PRINCIPAL_CAP,
};
use candid::Principal;
use std::cell::RefCell;

thread_local! {
    static DICT_CATALOG: RefCell<StableDictCatalogMap> = RefCell::new(init_dict_catalog());
    static DICT_CHUNKS: RefCell<StableDictChunksMap> = RefCell::new(init_dict_chunks());
    static DICT_AUDIT_LOG: RefCell<crate::stable::memory::StableDictAuditLogMap> =
        RefCell::new(crate::stable::memory::init_dict_audit_log());
}

/// Test-only helper to clear the dictionary catalog regions. Must be called at the start of
/// any test that mutates dict state to avoid thread-local interference.
#[cfg(test)]
pub(crate) fn reset_dict_catalog_maps() {
    DICT_CATALOG.with_borrow_mut(|map| map.clear_new());
    DICT_CHUNKS.with_borrow_mut(|map| map.clear_new());
    DICT_AUDIT_LOG.with_borrow_mut(|map| map.clear_new());
}

/// Test-only: re-open the dict catalog maps over their existing stable memories, mirroring
/// a canister upgrade without clearing any persisted rows.
#[cfg(test)]
pub(crate) fn reopen_dict_catalog_regions_for_test() {
    DICT_CATALOG.with(|slot| slot.replace(init_dict_catalog()));
    DICT_CHUNKS.with(|slot| slot.replace(init_dict_chunks()));
}

/// Regular facade over the dictionary catalog, its chunk store, and the audit log.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProvisionDictCatalogStore;

impl ProvisionDictCatalogStore {
    pub fn new() -> Self {
        Self
    }

    /// Return the entry for `key`, if present.
    pub fn get_entry(&self, key: &DictCatalogKey) -> Option<DictCatalogEntry> {
        DICT_CATALOG.with_borrow(|map| map.get(key))
    }

    /// Overwrite the entry row.
    pub fn put_entry(&self, entry: DictCatalogEntry) {
        DICT_CATALOG.with_borrow_mut(|map| {
            map.insert(entry.key.clone(), entry);
        });
    }

    /// Return one appended chunk, if present.
    pub fn get_chunk(&self, key: &DictChunkKey) -> Option<DictChunk> {
        DICT_CHUNKS.with_borrow(|map| map.get(key))
    }

    /// Append (or idempotently overwrite) one chunk row.
    pub fn put_chunk(&self, key: DictChunkKey, chunk: DictChunk) {
        DICT_CHUNKS.with_borrow_mut(|map| {
            map.insert(key, chunk);
        });
    }

    /// Remove one chunk row.
    #[cfg(test)]
    pub(crate) fn remove_chunk(&self, key: &DictChunkKey) {
        DICT_CHUNKS.with_borrow_mut(|map| {
            map.remove(key);
        });
    }

    /// Return the next monotonic sequence number for `principal` in the audit log.
    fn next_audit_sequence(&self, principal: Principal) -> u64 {
        DICT_AUDIT_LOG.with_borrow(|map| {
            let end = (principal, u64::MAX);
            map.range((principal, 0u64)..=end)
                .last()
                .map(|e| e.key().1.saturating_add(1))
                .unwrap_or(0)
        })
    }

    /// Append one audit row for `entry.caller`. Enforces the per-principal cap by evicting
    /// the oldest (lowest-sequence) entry when the bound is exceeded (artifact audit precedent).
    pub fn append_audit_entry(&self, entry: DictCatalogAuditEntry) {
        let principal = entry.caller;
        let seq = self.next_audit_sequence(principal);
        DICT_AUDIT_LOG.with_borrow_mut(|map| {
            map.insert((principal, seq), entry);

            let start = (principal, 0u64);
            let end = (principal, u64::MAX);
            let count = map.range(start..=end).count();
            if count > MAX_DICT_CATALOG_AUDIT_PER_PRINCIPAL_CAP {
                let to_evict = count - MAX_DICT_CATALOG_AUDIT_PER_PRINCIPAL_CAP;
                let evict_seqs: Vec<u64> = map
                    .range(start..=end)
                    .map(|e| e.key().1)
                    .take(to_evict)
                    .collect();
                for evict_seq in evict_seqs {
                    map.remove(&(principal, evict_seq));
                }
            }
        });
    }

    /// Return the bounded audit history for `principal` in sequence order.
    #[cfg(test)]
    pub(crate) fn audit_history(&self, principal: Principal) -> Vec<DictCatalogAuditEntry> {
        DICT_AUDIT_LOG.with_borrow(|map| {
            let start = (principal, 0u64);
            let end = (principal, u64::MAX);
            map.range(start..=end).map(|e| e.value().clone()).collect()
        })
    }
}
