//! Provision canister public types and stable-memory encodings.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
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
