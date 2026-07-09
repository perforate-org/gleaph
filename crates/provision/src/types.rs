//! Provision canister public types and stable-memory encodings.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;

// Re-exports for the catalog key and the lock key (P2-5, P2-A).
// P1-A: `IntentLockMarker` is `pub(crate)` in router and cannot be re-exported.
// Use the provision-local marker defined below.
pub use gleaph_graph_kernel::provisioning::{ProvisionableResourceKind, ProvisioningIntentKey};

pub use gleaph_graph_kernel::provisioning::wire::{
    CreatedResource, ProvisionRequest, ProvisionResult, ProvisionResultOutcome,
    ProvisionableResource, RouterProvisionAck,
};

// === Provision-local intent lock marker (P1-A) ===
//
// Mirrors the router's `IntentLockMarker` (zero bytes, Unbounded) but is
// owned by the provision crate so it does not require a cross-crate visibility change to
// router. Encoded as an empty byte string; the assertion in `from_bytes` is fail-closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProvisionIntentLockMarker;

impl Storable for ProvisionIntentLockMarker {
    // P2-D: router's `IntentLockMarker` uses `StorableBound::Unbounded` deliberately
    // (the unit-struct comment at types.rs:735 avoids the `()` Storable ambiguity
    // for the Bounded-0 path; Unbounded with zero-byte payload is the established pattern).
    // Match it exactly so Map 3's on-disk layout stays consistent with Router Map 47.
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&[])
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::new()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        assert!(
            bytes.as_ref().is_empty(),
            "ProvisionIntentLockMarker is zero bytes"
        );
        Self
    }
}

// === Deployment binding (stable region 0) ===

/// Bootstrap trust binding for a deployment. Written only by governance principal.
/// This is authentication configuration, not graph topology or tenancy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DeploymentBinding {
    pub deployment_id: String,
    /// Router principal authorized to send envelopes for this deployment.
    pub router_principal: Principal,
    /// Governance/recovery principal that can update this binding.
    pub governance_principal: Principal,
    /// Registry version at time of binding install.
    pub binding_version: u64,
}

impl Storable for DeploymentBinding {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&DeploymentBindingStableRecord::V1(self.clone()))
                .expect("encode DeploymentBinding"),
        )
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&DeploymentBindingStableRecord::V1(self)).expect("encode DeploymentBinding")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), DeploymentBindingStableRecord)
            .expect("decode DeploymentBinding")
        {
            DeploymentBindingStableRecord::V1(v1) => v1,
        }
    }
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
pub(crate) enum DeploymentBindingStableRecord {
    V1(DeploymentBinding),
}

// === Job state machine (stable regions 1–3) ===

/// Per-resource entry inside a `ProvisionJobRecord`. `current_state` lives on the parent
/// record (one linear machine per slice, indexed by `active_resource_index`); there is no
/// separate per-resource `ResourceJobState` enum in this slice. When a resource completes,
/// the parent `JobState` advances and `active_resource_index` moves to the next entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ResourceJobEntry {
    pub resource_kind: ProvisionableResourceKind,
    pub logical_resource_key: String,
    /// None until canister creation completes.
    pub canister_id: Option<Principal>,
    /// Artifact hash set after install.
    pub artifact_hash: Option<String>,
}

// ResourceJobEntry is nested inside ProvisionJobRecord; it does not need its own Storable
// impl because it is encoded via the outer candid Encode!/Decode! body of
// `ProvisionJobStableRecord::V1` (P2-1).

/// Canonical job record persisted by Provision (ADR 0035 §Durable job state).
/// Stable key: (request_id, deployment_id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobRecord {
    pub request_id: String,
    pub deployment_id: String,
    pub request_fingerprint: String,
    pub intent_key: ProvisioningIntentKey,
    pub reserved_graph_id: Option<GraphId>,
    pub graph_name: String,
    pub authorized_caller: Principal,
    pub release_id: String,
    pub router_callback_principal: Principal,
    pub resources: Vec<ResourceJobEntry>,
    pub current_state: JobState,
    /// Tracks which resource is currently being processed (index into resources).
    pub active_resource_index: usize,
    /// Number of management-canister calls already completed for this job.
    pub completed_effect_count: u32,
    /// Registry version accepted by the Router ack; None until the ack arrives.
    pub accepted_registry_version: Option<u64>,
    /// Timestamp (IC NNS timestamp) when the job was created.
    pub created_at_ns: u64,
    /// Timestamp when the job last transitioned state.
    pub last_transition_ns: u64,
}

impl Storable for ProvisionJobRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;
    // Encode/Decode via ProvisionJobStableRecord::V1 wrapper (P2-1).
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&ProvisionJobStableRecord::V1(self.clone()))
                .expect("encode ProvisionJobRecord"),
        )
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ProvisionJobStableRecord::V1(self)).expect("encode ProvisionJobRecord")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ProvisionJobStableRecord).expect("decode ProvisionJobRecord")
        {
            ProvisionJobStableRecord::V1(v1) => v1,
        }
    }
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
pub(crate) enum ProvisionJobStableRecord {
    V1(ProvisionJobRecord),
}

/// Top-level job state (ADR 0035 full linear machine; P1-3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum JobState {
    Submitted,
    Reserved,
    CreatePending,
    CanisterCreated,
    InstallPending,
    Installed,
    RouterRegistrationPending,
    RouterAckPending,
    Completed,
    Failed { reason: String },
}

pub(crate) fn state_name(state: &JobState) -> &'static str {
    match state {
        JobState::Submitted => "Submitted",
        JobState::Reserved => "Reserved",
        JobState::CreatePending => "CreatePending",
        JobState::CanisterCreated => "CanisterCreated",
        JobState::InstallPending => "InstallPending",
        JobState::Installed => "Installed",
        JobState::RouterRegistrationPending => "RouterRegistrationPending",
        JobState::RouterAckPending => "RouterAckPending",
        JobState::Completed => "Completed",
        JobState::Failed { .. } => "Failed",
    }
}

pub(crate) fn is_terminal_state(state: &JobState) -> bool {
    matches!(state, JobState::Completed | JobState::Failed { .. })
}

// === Legal `JobState` transition table (P2-E) ===
//
// Derived from ADR 0035 §Durable job state. The table is:
//
//   1. Linear forward sequence (the only path to `Completed`):
//        Submitted                          -> Reserved
//        Reserved                           -> CreatePending
//        CreatePending                      -> CanisterCreated
//        CanisterCreated                    -> InstallPending
//        InstallPending                     -> Installed
//        Installed                          -> RouterRegistrationPending
//        RouterRegistrationPending          -> RouterAckPending
//        RouterAckPending                   -> Completed
//
//   2. Failure path: any non-terminal state may transition to `Failed { reason }`. The
//      transition MUST record the reason. `Completed` and `Failed` are terminal — no
//      transition leaves a terminal state.
//
// Explicitly illegal: skipping a forward step, reversing a forward step, leaving a terminal
// state (Completed / Failed -> anything).
//
// Caller (`advance_state`) treats the result as a hard gate: `false` becomes
// `Err(JobAdvanceError::InvalidTransition)` and the record is not mutated.
pub(crate) fn is_legal_transition(from: &JobState, to: &JobState) -> bool {
    use JobState::*;
    if matches!(from, Completed | Failed { .. }) {
        return false;
    }
    if let Failed { .. } = to {
        // Any non-terminal -> Failed { reason } is legal.
        return true;
    }
    matches!(
        (from, to),
        (Submitted, Reserved)
            | (Reserved, CreatePending)
            | (CreatePending, CanisterCreated)
            | (CanisterCreated, InstallPending)
            | (InstallPending, Installed)
            | (Installed, RouterRegistrationPending)
            | (RouterRegistrationPending, RouterAckPending)
            | (RouterAckPending, Completed)
    )
}

// === Supporting stable key types ===

/// Canonical primary key for Map 1: `(request_id, deployment_id) -> ProvisionJobRecord`.
/// Mirrors `gleaph_router::types::ProvisioningRequestKey` (P2-3).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProvisionJobRequestKey {
    pub request_id: String,
    pub deployment_id: String,
}

impl ProvisionJobRequestKey {
    pub fn new(request_id: &str, deployment_id: &str) -> Self {
        Self {
            request_id: request_id.to_owned(),
            deployment_id: deployment_id.to_owned(),
        }
    }
}

impl Storable for ProvisionJobRequestKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.request_id.len() + self.deployment_id.len());
        out.extend_from_slice(&(self.request_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.request_id.as_bytes());
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut offset = 0usize;
        let request_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("request_id len"),
        ) as usize;
        offset += 4;
        let request_id = String::from_utf8(bytes[offset..offset + request_id_len].to_vec())
            .expect("request_id utf8");
        offset += request_id_len;
        let deployment_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("deployment_id len"),
        ) as usize;
        offset += 4;
        let deployment_id = String::from_utf8(bytes[offset..offset + deployment_id_len].to_vec())
            .expect("deployment_id utf8");
        Self {
            request_id,
            deployment_id,
        }
    }
}

// P2-A: JobByDeploymentKey is removed. Map 2 derived key is the re-exported
// `gleaph_graph_kernel::provisioning::ProvisioningIntentKey` (same fields, same encoding, same SSOT
// principle as the accepted P2-5 resolution for Map 3). Map 2 = `(intent) -> ProvisionJobRequestKey`.

// P2-5: `JobIntentLockKey` was removed. The stable region 3 lock key is the re-exported
// `gleaph_graph_kernel::provisioning::ProvisioningIntentKey`, matching Router Map 47.
// === Bootstrap authority + audit types (ADR 0035 Slice 7) ===

/// Durable bootstrap authority singleton stored in PROVISION_BOOTSTRAP_AUTH (MemoryId 4).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BootstrapAuthorityRecord {
    pub governance_principal: Principal,
    pub binding_version_at_seed: u64,
    pub seeded_at_ns: u64,
}

impl Storable for BootstrapAuthorityRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode BootstrapAuthorityRecord"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode BootstrapAuthorityRecord")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode BootstrapAuthorityRecord")
    }
}

/// Action recorded for every bootstrap-authority decision.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BootstrapAuthAction {
    InitialSeed,
    AdminInstall,
    RejectUnknownDeployment,
    RejectAlreadyExists,
    RejectInvalidState,
}

/// One durable audit row in PROVISION_BOOTSTRAP_AUDIT_LOG (MemoryId 5).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BootstrapAuthEntry {
    pub caller: Principal,
    pub deployment_id: Option<String>,
    pub action: BootstrapAuthAction,
    pub timestamp_ns: u64,
    pub registry_version: Option<u64>,
}

impl Storable for BootstrapAuthEntry {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode BootstrapAuthEntry"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode BootstrapAuthEntry")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode BootstrapAuthEntry")
    }
}

/// Per-governance audit history wrapper persisted in the audit BTreeMap value.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BootstrapAuthHistory {
    pub entries: Vec<BootstrapAuthEntry>,
}

impl Storable for BootstrapAuthHistory {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode BootstrapAuthHistory"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode BootstrapAuthHistory")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode BootstrapAuthHistory")
    }
}

/// Arguments for `admin_install_deployment_binding`.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminInstallDeploymentBindingArgs {
    pub deployment_id: String,
    pub router_principal: Principal,
    pub governance_principal: Principal,
    pub binding_version: u64,
}

/// Error returned by `admin_install_deployment_binding`.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AdminInstallError {
    UnknownDeployment(String),
    AlreadyExists {
        deployment_id: String,
        existing_governance: Principal,
    },
    InvalidState(String),
}

/// Internal alias used by handler code and unit tests.
pub type ProvisionAdminError = AdminInstallError;

// === Artifact catalog types (ADR 0036 Slice 8a) =============================

/// Kind of canister that an artifact can be installed into.
/// Provision itself is EXPLICITLY excluded — self-upgrade is forbidden per ADR 0036.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub enum CanisterKind {
    Router,
    Graph,
    PropertyIndex,
    VectorIndex,
}

/// Composite stable key identifying one published artifact.
/// The SHA-256 is part of identity, not a value field.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ArtifactId {
    pub canister_kind: CanisterKind,
    pub semantic_version: String,
    pub sha256: [u8; 32],
}

impl ArtifactId {
    pub fn new(canister_kind: CanisterKind, semantic_version: String, sha256: [u8; 32]) -> Self {
        Self {
            canister_kind,
            semantic_version,
            sha256,
        }
    }
}

/// Immutable artifact metadata published by governance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactMetadata {
    pub artifact_id: ArtifactId,
    pub byte_length: u64,
    pub chunk_hashes: Vec<[u8; 32]>,
    pub created_at_ns: u64,
}

/// Mutable upload-progress state for an artifact. Reclaimed from stable memory once the artifact
/// reaches `Verified`; verified canonical chunks remain in region 8 until explicit GC is designed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactUpload {
    pub artifact_id: ArtifactId,
    pub state: ArtifactUploadState,
    pub received_chunks: std::collections::BTreeSet<u32>,
    pub started_at_ns: u64,
    pub verified_at_ns: Option<u64>,
}

/// Composite stable key for one chunk of one artifact.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ArtifactChunkKey {
    pub artifact_id: ArtifactId,
    pub chunk_index: u32,
}

/// Verified canonical chunk bytes. Named-field wrapper required by the stable Storable contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactChunk {
    pub bytes: Vec<u8>,
}

/// Lifecycle of an artifact upload. Receiving -> Verifying -> (Verified | Failed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ArtifactUploadState {
    Receiving,
    Verifying,
    Verified { verified_at_ns: u64 },
    Failed { reason: String },
}

/// Errors returned by artifact catalog ingress methods.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ArtifactError {
    UnknownArtifact(ArtifactId),
    ConflictingMetadata {
        existing: ArtifactId,
        requested: ArtifactId,
    },
    ChunkOutOfRange {
        artifact_id: ArtifactId,
        chunk_index: u32,
        declared: u32,
    },
    ChunkHashMismatch {
        artifact_id: ArtifactId,
        chunk_index: u32,
    },
    FullSha256Mismatch {
        artifact_id: ArtifactId,
        expected: [u8; 32],
        actual: [u8; 32],
    },
    Unauthorized,
    NotProvision(CanisterKind),
}

/// Arguments for `artifact_publish_metadata`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactPublishMetadataArgs {
    pub canister_kind: CanisterKind,
    pub semantic_version: String,
    pub sha256: [u8; 32],
    pub byte_length: u64,
    pub chunk_hashes: Vec<[u8; 32]>,
}

/// Arguments for `artifact_upload_chunk`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactUploadChunkArgs {
    pub artifact_id: ArtifactId,
    pub chunk_index: u32,
    pub bytes: Vec<u8>,
}

// === Artifact audit log + release install types (ADR 0036 Slice 8c) ==========

/// One durable audit row in PROVISION_ARTIFACT_AUDIT_LOG (MemoryId 11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactAuditEntry {
    pub caller: Principal,
    pub action: ArtifactAuditAction,
    pub artifact_id: Option<ArtifactId>,
    pub release_id: Option<ReleaseId>,
    pub deployment_id: Option<String>,
    pub target_canister: Option<Principal>,
    pub timestamp_ns: u64,
    pub outcome: ArtifactAuditOutcome,
    pub reason: Option<String>,
}

/// Action recorded for every artifact/release plan-level operation.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ArtifactAuditAction {
    PublishArtifact,
    UploadChunk,
    VerifyArtifact,
    PublishRelease,
    ActivateRelease,
    InstallRelease,
}

/// Outcome of an audited operation.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ArtifactAuditOutcome {
    Success,
    Rejected,
    Failed,
}

/// Arguments for `release_install`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseInstallArgs {
    pub target_canister_kind: CanisterKind,
    pub target_canister_id: Option<Principal>,
    pub install_args: Vec<u8>,
    pub registry_version: u64,
}

/// Return value of `release_install`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseInstallResult {
    pub release_id: ReleaseId,
    pub target_canister_id: Principal,
    pub installed_chunks: u32,
    pub install_chunked_code_hash: [u8; 32],
    pub installed_at_ns: u64,
}

/// Errors returned by `release_install`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum InstallError {
    NoActiveRelease,
    ArtifactNotFound(ArtifactId),
    ArtifactNotVerified(ArtifactId),
    TargetCanisterKindForbidden(CanisterKind),
    ManagementCanisterCallFailed(String),
    ChunkStoreNotReconciled,
    Unauthorized,
    NoBootstrapAuthority,
}

// === Release manifest + active release types (ADR 0036 Slice 8b) =============

/// Opaque release identifier (e.g. "release-2026-07-08").
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ReleaseId(pub String);

/// Immutable release manifest: exactly one `ArtifactId` per non-Provision canister kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseManifest {
    pub release_id: ReleaseId,
    pub router_artifact: ArtifactId,
    pub graph_artifact: ArtifactId,
    pub property_index_artifact: ArtifactId,
    pub vector_index_artifact: ArtifactId,
}

/// Return value of `release_activate` confirming the active release that was swapped.
/// The `previous_release_id` field records the active release before the swap and
/// enforces the non-retroactivity invariant: no job/receipt region is mutated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseActivateResult {
    pub release_id: ReleaseId,
    pub activated_at_ns: u64,
    pub previous_release_id: Option<ReleaseId>,
}

/// Errors returned by release publish/activate ingress methods.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ReleaseError {
    UnknownRelease(ReleaseId),
    ConflictingRelease {
        existing: ReleaseId,
        requested: ReleaseId,
    },
    NoBootstrapAuthority,
    Unauthorized,
    ArtifactNotFound(ArtifactId),
    ArtifactNotVerified(ArtifactId),
    ProvisionKindForbidden(ArtifactId),
    IncompleteManifest {
        release_id: ReleaseId,
        missing: Vec<ArtifactId>,
    },
    NotUniquePerKind {
        release_id: ReleaseId,
        kind: CanisterKind,
        conflicting: Vec<ArtifactId>,
    },
}

/// Arguments for `release_publish`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleasePublishArgs {
    pub release_id: ReleaseId,
    pub artifact_ids: Vec<ArtifactId>,
}

/// Arguments for `release_activate`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseActivateArgs {
    pub release_id: ReleaseId,
}

//
// All stable-collection keys and values use Candid encoding with StorableBound::Unbounded
// (Plan 0061a R10 composite stable-key compatibility; round-trip verified by test (j)).

impl Storable for ArtifactId {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactId"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactId")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactId")
    }
}

impl Storable for ArtifactMetadata {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactMetadata"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactMetadata")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactMetadata")
    }
}

impl Storable for ArtifactUpload {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactUpload"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactUpload")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactUpload")
    }
}

impl Storable for ArtifactChunkKey {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactChunkKey"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactChunkKey")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactChunkKey")
    }
}

impl Storable for ArtifactChunk {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactChunk"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactChunk")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactChunk")
    }
}

impl Storable for ArtifactUploadState {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactUploadState"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactUploadState")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactUploadState")
    }
}

// === Stable encodings for release types (ADR 0036 Slice 8b) =============

impl Storable for ReleaseId {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ReleaseId"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ReleaseId")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ReleaseId")
    }
}

impl Storable for ReleaseManifest {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ReleaseManifest"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ReleaseManifest")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ReleaseManifest")
    }
}

impl Storable for ReleaseActivateResult {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ReleaseActivateResult"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ReleaseActivateResult")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ReleaseActivateResult")
    }
}

impl Storable for ReleaseError {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ReleaseError"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ReleaseError")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ReleaseError")
    }
}

// === Stable encodings for audit log + install types (ADR 0036 Slice 8c) ====

impl Storable for ArtifactAuditEntry {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactAuditEntry"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactAuditEntry")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactAuditEntry")
    }
}

impl Storable for ArtifactAuditAction {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactAuditAction"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactAuditAction")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactAuditAction")
    }
}

impl Storable for ArtifactAuditOutcome {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ArtifactAuditOutcome"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ArtifactAuditOutcome")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ArtifactAuditOutcome")
    }
}

impl Storable for ReleaseInstallResult {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ReleaseInstallResult"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ReleaseInstallResult")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ReleaseInstallResult")
    }
}

impl Storable for InstallError {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode InstallError"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode InstallError")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode InstallError")
    }
}

/// Compute the SHA-256 digest of `bytes`. Used by handlers and tests.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}
