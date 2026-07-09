//! Provision canister release manifest + active-release pointer facade (ADR 0036 Slice 8b).
//!
//! Two independent stable structures on two dedicated MemoryIds:
//! - `PROVISION_RELEASE_MANIFEST` (MemoryId 9): `StableBTreeMap<ReleaseId, ReleaseManifest>`.
//! - `PROVISION_ACTIVE_RELEASE` (MemoryId 10): `StableCell<Option<ReleaseId>>` singleton.

use super::memory::{
    StableActiveReleaseCell, StableReleaseManifestMap, init_active_release, init_release_manifest,
};
use crate::types::{ReleaseId, ReleaseManifest};
use std::cell::RefCell;

thread_local! {
    static RELEASE_MANIFEST_MAP: RefCell<StableReleaseManifestMap> =
        RefCell::new(init_release_manifest());
    static ACTIVE_RELEASE_CELL: RefCell<StableActiveReleaseCell> =
        RefCell::new(init_active_release());
}

/// Test-only helper to clear the release manifest map and active-release singleton.
#[cfg(test)]
pub(crate) fn reset_release_maps() {
    RELEASE_MANIFEST_MAP.with_borrow_mut(|map| map.clear_new());
    ACTIVE_RELEASE_CELL.with_borrow_mut(|cell| {
        cell.set(None);
    });
}

/// Regular (non-singleton) facade for the release manifest and active-release pointer.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProvisionReleaseStore;

impl ProvisionReleaseStore {
    pub fn new() -> Self {
        Self
    }

    /// Read the immutable manifest for `release_id`, if any.
    pub fn get_manifest(&self, release_id: &ReleaseId) -> Option<ReleaseManifest> {
        RELEASE_MANIFEST_MAP.with_borrow(|map| map.get(release_id))
    }

    /// Insert an immutable release manifest. Returns `Err` if `release_id` already exists.
    pub fn publish_manifest(
        &self,
        manifest: ReleaseManifest,
    ) -> Result<ReleaseManifest, crate::types::ReleaseError> {
        let release_id = manifest.release_id.clone();
        RELEASE_MANIFEST_MAP.with_borrow_mut(|map| {
            if let Some(existing) = map.get(&release_id) {
                return Err(crate::types::ReleaseError::ConflictingRelease {
                    existing: existing.release_id,
                    requested: release_id,
                });
            }
            map.insert(release_id, manifest.clone());
            Ok(manifest)
        })
    }

    /// Read the currently active release id, if any.
    pub fn get_active(&self) -> Option<ReleaseId> {
        ACTIVE_RELEASE_CELL.with_borrow(|cell| cell.get().clone())
    }

    /// Atomically set the active release id.
    pub fn set_active(&self, release_id: ReleaseId) {
        ACTIVE_RELEASE_CELL.with_borrow_mut(|cell| {
            cell.set(Some(release_id));
        });
    }
}
