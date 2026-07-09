//! Provision canister artifact catalog facade (ADR 0036 Slice 8a + 8c).
//!
//! Wraps four dedicated stable regions:
//! - `PROVISION_ARTIFACT_CATALOG` (MemoryId 6): immutable `ArtifactId -> ArtifactMetadata`.
//! - `PROVISION_ARTIFACT_UPLOAD` (MemoryId 7): mutable `ArtifactId -> ArtifactUpload` scratch state.
//! - `PROVISION_ARTIFACT_CHUNKS` (MemoryId 8): verified canonical `ArtifactChunkKey -> ArtifactChunk`.
//! - `PROVISION_ARTIFACT_AUDIT_LOG` (MemoryId 11): append-oriented `(Principal, u64) -> ArtifactAuditEntry`.
//!
//! The facade is a regular struct (not a singleton) so handlers instantiate it per call.
//! Bootstrap authority checks live in the handler, not on this facade.

use super::memory::{
    StableArtifactAuditLogMap, StableArtifactCatalogMap, StableArtifactChunksMap,
    StableArtifactUploadMap, init_artifact_audit_log, init_artifact_catalog, init_artifact_chunks,
    init_artifact_upload,
};
use crate::types::{
    ArtifactAuditEntry, ArtifactChunk, ArtifactChunkKey, ArtifactError, ArtifactId,
    ArtifactMetadata, ArtifactUpload, ArtifactUploadState,
};
use candid::Principal;
use std::cell::RefCell;

/// Per-principal cap on artifact audit-log entries (R5 strict: bounded append-heavy log).
pub(crate) const ARTIFACT_AUDIT_LOG_PER_PRINCIPAL_CAP: usize = 1024;

thread_local! {
    static ARTIFACT_CATALOG: RefCell<StableArtifactCatalogMap> =
        RefCell::new(init_artifact_catalog());
    static ARTIFACT_UPLOAD: RefCell<StableArtifactUploadMap> =
        RefCell::new(init_artifact_upload());
    static ARTIFACT_CHUNKS: RefCell<StableArtifactChunksMap> =
        RefCell::new(init_artifact_chunks());
    static ARTIFACT_AUDIT_LOG: RefCell<StableArtifactAuditLogMap> =
        RefCell::new(init_artifact_audit_log());
}

/// Test-only helper to clear only the artifact audit log.
#[cfg(test)]
pub(crate) fn reset_artifact_audit_log() {
    ARTIFACT_AUDIT_LOG.with_borrow_mut(|map| map.clear_new());
}

/// Test-only helper to clear the four artifact stable maps. Must be called at the start of any
/// test that mutates artifact state to avoid thread-local interference.
#[cfg(test)]
pub(crate) fn reset_artifact_maps() {
    ARTIFACT_CATALOG.with_borrow_mut(|map| map.clear_new());
    ARTIFACT_UPLOAD.with_borrow_mut(|map| map.clear_new());
    ARTIFACT_CHUNKS.with_borrow_mut(|map| map.clear_new());
    reset_artifact_audit_log();
}

/// Regular (non-singleton) facade for the artifact catalog, upload scratch, chunk store, and audit log.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProvisionArtifactStore;

impl ProvisionArtifactStore {
    pub fn new() -> Self {
        Self
    }

    /// Return the immutable metadata for `artifact_id`, if published.
    pub fn get_metadata(&self, artifact_id: &ArtifactId) -> Option<ArtifactMetadata> {
        ARTIFACT_CATALOG.with_borrow(|map| map.get(artifact_id))
    }

    /// Publish immutable artifact metadata. Rejects a duplicate identity with
    /// `ArtifactError::ConflictingMetadata`.
    #[allow(clippy::result_large_err)]
    pub fn publish_metadata(
        &self,
        metadata: ArtifactMetadata,
    ) -> Result<ArtifactMetadata, ArtifactError> {
        let artifact_id = metadata.artifact_id.clone();
        ARTIFACT_CATALOG.with_borrow_mut(|map| {
            if let Some(existing) = map.get(&artifact_id) {
                return Err(ArtifactError::ConflictingMetadata {
                    existing: existing.artifact_id,
                    requested: artifact_id,
                });
            }
            map.insert(artifact_id, metadata.clone());
            Ok(metadata)
        })
    }

    /// Return the current mutable upload state, if any.
    pub fn get_upload(&self, artifact_id: &ArtifactId) -> Option<ArtifactUpload> {
        ARTIFACT_UPLOAD.with_borrow(|map| map.get(artifact_id))
    }

    /// Return an existing upload record or create a fresh `Receiving` entry.
    pub fn get_or_create_upload(&self, artifact_id: &ArtifactId, now_ns: u64) -> ArtifactUpload {
        ARTIFACT_UPLOAD.with_borrow_mut(|map| {
            if let Some(existing) = map.get(artifact_id) {
                return existing;
            }
            let upload = ArtifactUpload {
                artifact_id: artifact_id.clone(),
                state: ArtifactUploadState::Receiving,
                received_chunks: std::collections::BTreeSet::new(),
                started_at_ns: now_ns,
                verified_at_ns: None,
            };
            map.insert(artifact_id.clone(), upload.clone());
            upload
        })
    }

    /// Overwrite the mutable upload state.
    pub fn put_upload(&self, artifact_id: &ArtifactId, upload: ArtifactUpload) {
        ARTIFACT_UPLOAD.with_borrow_mut(|map| {
            map.insert(artifact_id.clone(), upload);
        });
    }

    /// Remove the mutable upload state. Used after verification succeeds.
    pub fn remove_upload(&self, artifact_id: &ArtifactId) {
        ARTIFACT_UPLOAD.with_borrow_mut(|map| {
            map.remove(artifact_id);
        });
    }

    /// Store one chunk in the verified canonical chunk store.
    pub fn put_chunk(&self, key: ArtifactChunkKey, chunk: ArtifactChunk) {
        ARTIFACT_CHUNKS.with_borrow_mut(|map| {
            map.insert(key, chunk);
        });
    }

    /// Return one chunk, if present.
    pub fn get_chunk(&self, key: &ArtifactChunkKey) -> Option<ArtifactChunk> {
        ARTIFACT_CHUNKS.with_borrow(|map| map.get(key))
    }

    /// Remove one chunk.
    pub fn remove_chunk(&self, key: &ArtifactChunkKey) {
        ARTIFACT_CHUNKS.with_borrow_mut(|map| {
            map.remove(key);
        });
    }

    /// Remove every chunk belonging to `artifact_id`. Used on verification failure.
    pub fn remove_all_chunks(&self, artifact_id: &ArtifactId) {
        let start = ArtifactChunkKey {
            artifact_id: artifact_id.clone(),
            chunk_index: 0,
        };
        let end = ArtifactChunkKey {
            artifact_id: artifact_id.clone(),
            chunk_index: u32::MAX,
        };
        ARTIFACT_CHUNKS.with_borrow_mut(|map| {
            let keys: Vec<ArtifactChunkKey> = map
                .range(start..=end)
                .map(|entry| entry.key().clone())
                .collect();
            for key in keys {
                map.remove(&key);
            }
        });
    }

    /// Read every chunk for `artifact_id` in `chunk_index` order.
    pub fn chunks_in_order(&self, artifact_id: &ArtifactId, count: u32) -> Vec<ArtifactChunk> {
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let key = ArtifactChunkKey {
                artifact_id: artifact_id.clone(),
                chunk_index: i,
            };
            if let Some(chunk) = self.get_chunk(&key) {
                out.push(chunk);
            } else {
                // Stop at first missing chunk; caller handles incomplete set.
                break;
            }
        }
        out
    }

    /// Return the next monotonic sequence number for `principal` in the audit log.
    pub fn next_sequence(&self, principal: Principal) -> u64 {
        ARTIFACT_AUDIT_LOG.with_borrow(|map| {
            let end = (principal, u64::MAX);
            map.range((principal, 0u64)..=end)
                .last()
                .map(|e| e.key().1.saturating_add(1))
                .unwrap_or(0)
        })
    }

    /// Append one audit row for `entry.caller`. Enforces the per-principal cap by evicting the
    /// oldest (lowest-sequence) entry when the bound is exceeded (R5 strict).
    pub fn append_audit_entry(&self, entry: ArtifactAuditEntry) {
        let principal = entry.caller;
        let seq = self.next_sequence(principal);
        ARTIFACT_AUDIT_LOG.with_borrow_mut(|map| {
            map.insert((principal, seq), entry);

            // Enforce per-principal cap: evict oldest entries first.
            let start = (principal, 0u64);
            let end = (principal, u64::MAX);
            let count = map.range(start..=end).count();
            if count > ARTIFACT_AUDIT_LOG_PER_PRINCIPAL_CAP {
                let to_evict = count - ARTIFACT_AUDIT_LOG_PER_PRINCIPAL_CAP;
                let evict_keys: Vec<u64> = map
                    .range(start..=end)
                    .map(|e| e.key().1)
                    .take(to_evict)
                    .collect();
                for evict_seq in evict_keys {
                    map.remove(&(principal, evict_seq));
                }
            }
        });
    }

    /// Return the bounded audit history for `principal` in sequence order.
    pub fn audit_history(&self, principal: Principal) -> Vec<ArtifactAuditEntry> {
        ARTIFACT_AUDIT_LOG.with_borrow(|map| {
            let start = (principal, 0u64);
            let end = (principal, u64::MAX);
            map.range(start..=end).map(|e| e.value().clone()).collect()
        })
    }

    /// Return the latest audit entry for `principal`, if any.
    pub fn latest_audit_entry(&self, principal: Principal) -> Option<ArtifactAuditEntry> {
        ARTIFACT_AUDIT_LOG.with_borrow(|map| {
            let end = (principal, u64::MAX);
            map.range((principal, 0u64)..=end)
                .last()
                .map(|e| e.value().clone())
        })
    }
}
