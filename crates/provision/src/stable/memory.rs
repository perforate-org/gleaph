//! Provision canister stable-memory map wiring.

use crate::types::{
    ArtifactChunk, ArtifactChunkKey, ArtifactId, ArtifactMetadata, ArtifactUpload,
    DeploymentBinding, ProvisionIntentLockMarker, ProvisionJobRecord, ProvisionJobRequestKey,
};
use gleaph_graph_kernel::provisioning::ProvisioningIntentKey;
use ic_stable_structures::{
    DefaultMemoryImpl, StableBTreeMap,
    memory_manager::{MemoryId, MemoryManager, VirtualMemory},
};
use std::cell::RefCell;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(crate) const DEPLOYMENT_TRUST: MemoryId = MemoryId::new(0);
pub(crate) const JOB_BY_REQUEST: MemoryId = MemoryId::new(1);
pub(crate) const JOB_BY_DEPLOYMENT: MemoryId = MemoryId::new(2);
pub(crate) const JOB_INTENT_LOCK: MemoryId = MemoryId::new(3);
// ADR 0035 Slice 7: PROVISION_BOOTSTRAP_AUTH = MemoryId::new(4) (StableCell singleton).
pub(crate) const PROVISION_BOOTSTRAP_AUTH: MemoryId = MemoryId::new(4);
// ADR 0035 Slice 7: PROVISION_BOOTSTRAP_AUDIT_LOG = MemoryId::new(5) (per-governance audit log).
pub(crate) const PROVISION_BOOTSTRAP_AUDIT_LOG: MemoryId = MemoryId::new(5);

// ADR 0036 Slice 8a: immutable artifact catalog (MemoryId 6).
pub(crate) const PROVISION_ARTIFACT_CATALOG: MemoryId = MemoryId::new(6);
// ADR 0036 Slice 8a: mutable upload-progress state (MemoryId 7).
pub(crate) const PROVISION_ARTIFACT_UPLOAD: MemoryId = MemoryId::new(7);
// ADR 0036 Slice 8a: verified canonical artifact chunk bytes (MemoryId 8).
pub(crate) const PROVISION_ARTIFACT_CHUNKS: MemoryId = MemoryId::new(8);

pub(crate) type StableDeploymentTrustMap = StableBTreeMap<String, DeploymentBinding, Memory>;
pub(crate) type StableJobByRequestMap =
    StableBTreeMap<ProvisionJobRequestKey, ProvisionJobRecord, Memory>;
// P2-A: Map 2 derived key is the re-exported `ProvisioningIntentKey` (SSOT with Map 3 / Router Map 47).
pub(crate) type StableJobByDeploymentMap =
    StableBTreeMap<ProvisioningIntentKey, ProvisionJobRequestKey, Memory>;
// P1-A: Map 3 value uses the provision-local `ProvisionIntentLockMarker` (router marker is pub(crate)).
pub(crate) type StableJobIntentLockMap =
    StableBTreeMap<ProvisioningIntentKey, ProvisionIntentLockMarker, Memory>;

pub(crate) type StableArtifactCatalogMap = StableBTreeMap<ArtifactId, ArtifactMetadata, Memory>;
pub(crate) type StableArtifactUploadMap = StableBTreeMap<ArtifactId, ArtifactUpload, Memory>;
pub(crate) type StableArtifactChunksMap = StableBTreeMap<ArtifactChunkKey, ArtifactChunk, Memory>;

pub(crate) fn init_deployment_trust() -> StableDeploymentTrustMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(DEPLOYMENT_TRUST)))
}

pub(crate) fn init_job_by_request() -> StableJobByRequestMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(JOB_BY_REQUEST)))
}

pub(crate) fn init_job_by_deployment() -> StableJobByDeploymentMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(JOB_BY_DEPLOYMENT)))
}

pub(crate) fn init_job_intent_lock() -> StableJobIntentLockMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(JOB_INTENT_LOCK)))
}

pub(crate) fn init_artifact_catalog() -> StableArtifactCatalogMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(PROVISION_ARTIFACT_CATALOG)))
}

pub(crate) fn init_artifact_upload() -> StableArtifactUploadMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(PROVISION_ARTIFACT_UPLOAD)))
}

pub(crate) fn init_artifact_chunks() -> StableArtifactChunksMap {
    StableBTreeMap::init(MEMORY_MANAGER.with(|mm| mm.borrow().get(PROVISION_ARTIFACT_CHUNKS)))
}
