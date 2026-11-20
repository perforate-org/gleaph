//! Provision canister public types and stable-memory encodings.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
pub use gleaph_graph_kernel::provisioning::ControllerMode;
use gleaph_graph_kernel::provisioning::DeploymentId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;

// Re-exports for the catalog key and the lock key (P2-5, P2-A).
// P1-A: `IntentLockMarker` is `pub(crate)` in router and cannot be re-exported.
// Use the provision-local marker defined below.
pub use gleaph_graph_kernel::provisioning::{LogicalResource, ProvisioningIntentKey};

pub use gleaph_graph_kernel::provisioning::wire::{
    AdmissionQuote, AdmissionQuoteError, CreatedResource, ProvisionRequest, ProvisionResult,
    ProvisionResultOutcome, ProvisionableResource, RouterRegistrationAck,
    RouterRegistrationAckResponse, RouterRegistrationAckResult,
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

// === Deployment grants (stable region 0) ===
//
// A deployment grant is a single principal authorized to request issuance. The store is a set of
// authorized issuers, not a deployment binding: an own-deployment issuer (a Router) names itself,
// while a trusted relay (the Account canister) names the account it issues for and carries the
// end user in `authorized_caller`. See `DeploymentGrantStore` (stable/store.rs).

// === Job state machine (stable regions 1–3) ===

/// Per-resource entry inside a `ProvisionJobRecord`. `current_state` lives on the parent
/// record (one linear machine per slice, indexed by `active_resource_index`); there is no
/// separate per-resource `ResourceJobState` enum in this slice. When a resource completes,
/// the parent `JobState` advances and `active_resource_index` moves to the next entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ResourceJobEntry {
    pub logical_resource: LogicalResource,
    /// None until canister creation completes.
    pub canister_id: Option<Principal>,
    /// Artifact hash set after install.
    pub artifact_hash: Option<[u8; 32]>,
}

// ResourceJobEntry is nested inside ProvisionJobRecord; it does not need its own Storable
// impl because it is encoded via the outer candid Encode!/Decode! body of
// `ProvisionJobStableRecord::V1` (P2-1).

/// One durable Router issuance reservation. The ledger is an index, not a second job state:
/// canonical `ProvisionJobRecord` remains the source of truth for state and resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuanceRecord {
    pub request_id: [u8; 32],
}

impl Storable for IssuanceRecord {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.request_id)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.request_id.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut request_id = [0u8; 32];
        request_id.copy_from_slice(bytes.as_ref());
        Self { request_id }
    }
}

/// One admission's cycle split. The cost is consumed by resource work; the fee and reserve are
/// withheld for the operating company and compensation pool. `total` is never stored separately.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct AdmissionCharge {
    pub cost: u128,
    pub fee: u128,
    pub reserve: u128,
}

impl AdmissionCharge {
    pub fn total(self) -> Result<u128, ArithmeticOverflow> {
        self.cost
            .checked_add(self.fee)
            .and_then(|value| value.checked_add(self.reserve))
            .ok_or(ArithmeticOverflow)
    }
}

/// Global accounting for the parts of an admission that are not resource cost. This is an ledger
/// of withheld cycles, not a transfer to another canister.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ServiceLedger {
    pub platform_fees: u128,
    pub compensation_reserve: u128,
}

impl Storable for ServiceLedger {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ServiceLedger"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ServiceLedger")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(&bytes, ServiceLedger).expect("decode ServiceLedger")
    }
}

/// Which withheld pool a withdrawal debits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum WithdrawalKind {
    PlatformFees,
    CompensationReserve,
}

/// Durable status of one withdrawal request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum WithdrawalStatus {
    Pending,
    Completed,
    Failed(String),
}

/// Idempotency identity and result of one external cycles withdrawal.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, CandidType,
)]
pub struct WithdrawalKey {
    pub authority: Principal,
    pub idempotency_key: [u8; 32],
}

impl Storable for WithdrawalKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode WithdrawalKey"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode WithdrawalKey")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(&bytes, WithdrawalKey).expect("decode WithdrawalKey")
    }
}

/// One withdrawal, keyed by [`WithdrawalKey`]. `amount` is cycles reserved from the ledger
/// before the transfer and only finalized after the destination accepted them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct WithdrawalRecord {
    pub kind: WithdrawalKind,
    pub destination: Principal,
    pub amount: u128,
    pub status: WithdrawalStatus,
}

impl Storable for WithdrawalRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode WithdrawalRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode WithdrawalRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(&bytes, WithdrawalRecord).expect("decode WithdrawalRecord")
    }
}

/// Checked arithmetic failures are distinct from credit errors so callers do not guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArithmeticOverflow;

/// Durable deployment-scoped settings and cycles credit. Keyed by `DeploymentId` in MemoryId 17.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DeploymentRecord {
    pub owner: Principal,
    pub controller_mode: ControllerMode,
    pub operator: Option<Principal>,
    pub credit_cycles: u128,
}

impl Storable for DeploymentRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DeploymentRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DeploymentRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(&bytes, DeploymentRecord).expect("decode DeploymentRecord")
    }
}

/// Error surface for the governance-only service-config update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ServiceConfigUpdateError {
    Unauthorized,
    StaleVersion,
    InvalidConfig,
}

/// Request to move withheld cycles to a destination canister.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct WithdrawArgs {
    pub idempotency_key: [u8; 32],
    pub kind: WithdrawalKind,
    pub destination: Principal,
    pub amount: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct WithdrawResult {
    pub amount: u128,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum WithdrawError {
    Unauthorized,
    OperatorNotConfigured,
    AnonymousDestination,
    InvalidDestination,
    AmountZero,
    InsufficientLedger,
    IdempotencyConflict,
    PreviousAttemptFailed,
    InvalidState,
    TransferPending,
    TransferFailed(String),
}

/// Error surface of the deployment read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DeploymentQueryError {
    Unauthorized,
    UnknownDeployment,
}

/// Errors from pre-registering a deployment before any job exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum InitializeDeploymentError {
    Unauthorized,
    AnonymousOwner,
    MissingOperator,
    ControllerModeMismatch,
    OwnerMismatch,
    OperatorMismatch,
    InvalidState,
}

/// Arguments for pre-registering a deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct InitializeDeploymentArgs {
    pub deployment_id: DeploymentId,
    pub owner: Principal,
    pub controller_mode: ControllerMode,
}

/// Idempotency key for a billing credit grant. The authority is part of the key so two billing
/// services cannot collide on the same external payment id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BillingGrantKey {
    pub authority: Principal,
    pub idempotency_key: [u8; 32],
}

impl Storable for BillingGrantKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let principal = self.authority.as_slice();
        let mut out = Vec::with_capacity(1 + principal.len() + 32);
        out.push(principal.len() as u8);
        out.extend_from_slice(principal);
        out.extend_from_slice(&self.idempotency_key);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let principal_len = bytes[0] as usize;
        let principal = Principal::from_slice(&bytes[1..1 + principal_len]);
        let mut idempotency_key = [0u8; 32];
        idempotency_key.copy_from_slice(&bytes[1 + principal_len..1 + principal_len + 32]);
        Self {
            authority: principal,
            idempotency_key,
        }
    }
}

/// Durable record of one applied billing grant. The grant is a credit, not a cycle transfer; the
/// operating company has already received the fiat (or other consideration) off-chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct BillingGrantRecord {
    pub deployment_id: DeploymentId,
    pub amount: u128,
    pub credited_at_ns: u64,
    /// Set once by a full reversal; a reversed grant is never applied twice.
    pub reversed_at_ns: Option<u64>,
}

impl Storable for BillingGrantRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode BillingGrantRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode BillingGrantRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(&bytes, BillingGrantRecord).expect("decode BillingGrantRecord")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GrantCreditArgs {
    pub deployment_id: DeploymentId,
    pub amount: u128,
    pub idempotency_key: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GrantCreditResult {
    pub deployment_id: DeploymentId,
    pub amount: u128,
    pub credit_cycles: u128,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReverseCreditArgs {
    pub grant_authority: Principal,
    pub idempotency_key: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReverseCreditResult {
    pub deployment_id: DeploymentId,
    pub amount: u128,
    pub credit_cycles: u128,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ReverseCreditError {
    Unauthorized,
    BillingAuthorityNotConfigured,
    UnknownGrant,
    AnonymousGrantAuthority,
    InsufficientCredit,
    InvalidState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum GrantCreditError {
    Unauthorized,
    BillingAuthorityNotConfigured,
    UnknownDeployment,
    AmountZero,
    IdempotencyConflict,
    CreditOverflow,
    InvalidState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantOutcome {
    Granted,
    Replayed,
}

/// Versioned commercial policy. One stable cell (MemoryId 18); changes apply to new jobs only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ServiceConfig {
    pub version: u32,
    /// Production default is true. Test fixtures explicitly disable payment.
    pub require_prepaid_admission: bool,
    pub platform_fee_bps: u16,
    pub reserve_bps: u16,
    pub compensation_cap_cycles: u128,
    pub covered_failure_classes: u32,
    pub operator: Option<Principal>,
    /// The only principal allowed to grant fiat-backed credit. `None` disables grants.
    pub billing_authority: Option<Principal>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            version: 1,
            require_prepaid_admission: true,
            platform_fee_bps: 0,
            reserve_bps: 0,
            compensation_cap_cycles: 0,
            covered_failure_classes: 0,
            operator: None,
            billing_authority: None,
        }
    }
}

impl ServiceConfig {
    /// Split an admission cost into resource cost, platform fee and reserve. All arithmetic is
    /// checked; a misconfigured rate fails closed rather than rounding into a negative balance.
    pub fn admission_charge(self, cost: u128) -> Result<AdmissionCharge, ArithmeticOverflow> {
        let ten_thousand = 10_000u128;
        let fee = cost
            .checked_mul(u128::from(self.platform_fee_bps))
            .ok_or(ArithmeticOverflow)?
            / ten_thousand;
        let reserve = cost
            .checked_mul(u128::from(self.reserve_bps))
            .ok_or(ArithmeticOverflow)?
            / ten_thousand;
        let charge = AdmissionCharge { cost, fee, reserve };
        charge.total()?;
        Ok(charge)
    }
}

impl Storable for ServiceConfig {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ServiceConfig"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ServiceConfig")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(&bytes, ServiceConfig).expect("decode ServiceConfig")
    }
}

/// Canonical job record persisted by Provision (ADR 0035 §Durable job state).
/// Stable key: (request_id, deployment_id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobRecord {
    pub request_id: [u8; 32],
    pub deployment_id: DeploymentId,
    pub intent_key: ProvisioningIntentKey,
    pub reserved_graph_id: Option<GraphId>,
    pub graph_name: String,
    pub authorized_caller: Principal,
    pub release_id: String,
    /// Service policy version accepted under. Later policy changes apply only to new jobs.
    pub service_config_version: u32,
    /// The split reserved by this admission. Zero for a replay or an unpaid test admission.
    pub admission_charge: AdmissionCharge,
    /// Set once when a no-effect admission is refunded, so a second refund cannot double-credit.
    pub admission_refunded: bool,
    /// Digest of the complete immutable accepted request envelope. A same-key request with any
    /// different effect-bearing field is a conflict and cannot join this job.
    pub immutable_request_digest: [u8; 32],
    pub resources: Vec<ResourceJobEntry>,
    pub current_state: JobState,
    /// Tracks which resource is currently being processed (index into resources).
    pub active_resource_index: usize,
    /// Number of management-canister calls already completed for this job.
    pub completed_effect_count: u32,
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
    Completed,
    /// Issuance failed after an external effect. `remaining` lists created canisters whose
    /// stop/delete has not been confirmed yet; a replay of the same request resumes cleanup.
    CleanupPending {
        reason: String,
        remaining: Vec<Principal>,
    },
    Failed {
        reason: String,
    },
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
        JobState::Completed => "Completed",
        JobState::CleanupPending { .. } => "CleanupPending",
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
//        RouterRegistrationPending          -> Completed
//
//   1b. A multi-resource request re-enters the create half from `Installed`:
//        Installed -> CreatePending (the *next* resource starts its own cycle)
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
    if matches!(to, CleanupPending { .. }) {
        // Any non-terminal may start cleanup; progress on a cleanup job is also a
        // CleanupPending -> CleanupPending transition that shrinks `remaining`.
        return true;
    }
    matches!(
        (from, to),
        (Submitted, Reserved)
            | (Reserved, CreatePending)
            | (Installed, CreatePending)
            | (CreatePending, CanisterCreated)
            | (CanisterCreated, InstallPending)
            | (InstallPending, Installed)
            | (Installed, RouterRegistrationPending)
            | (RouterRegistrationPending, Completed)
    )
}

// === Supporting stable key types ===

/// Canonical primary key for Map 1: `(request_id, deployment_id) -> ProvisionJobRecord`.
/// Mirrors `gleaph_router::types::ProvisioningRequestKey` (P2-3).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProvisionJobRequestKey {
    pub request_id: [u8; 32],
    pub deployment_id: DeploymentId,
}

impl ProvisionJobRequestKey {
    pub fn new(request_id: &[u8; 32], deployment_id: DeploymentId) -> Self {
        Self {
            request_id: *request_id,
            deployment_id,
        }
    }
}

impl Storable for ProvisionJobRequestKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + 30);
        out.extend_from_slice(&self.request_id);
        out.extend_from_slice(&self.deployment_id.to_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut request_id = [0u8; 32];
        request_id.copy_from_slice(&bytes[0..32]);
        let deployment_len = 1 + bytes[32] as usize;
        let deployment_id =
            DeploymentId::from_bytes(Cow::Borrowed(&bytes[32..32 + deployment_len]));
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

pub use gleaph_provision_api::{
    BillingGrantView, BootstrapAuthAction, BootstrapAuthEntry, DeploymentCanister,
    DeploymentGrantScope, DeploymentJobSummary, DeploymentOwnerUpdateError, FailedCanisterRepair,
    ProvisionJobView, ResourceJobView, SelfCustodyTransferError, TakeOwnershipError,
    UpsertDeploymentGrantArgs, UpsertDeploymentGrantError,
};

/// Internal alias used by handler code and unit tests.
pub type ProvisionAdminError = UpsertDeploymentGrantError;

// === Artifact/release wire types (owned by gleaph-artifact-api) =============

pub use gleaph_artifact_api::types::{
    ArtifactAuditAction, ArtifactAuditEntry, ArtifactAuditOutcome, ArtifactError, ArtifactId,
    ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactStorageId, ArtifactUpload,
    ArtifactUploadChunkArgs, ArtifactUploadState, CanisterKind, InstallError, MAX_ARTIFACT_BYTES,
    MAX_ARTIFACT_CHUNKS, MAX_ARTIFACT_SEMANTIC_VERSION_LEN, ReleaseActivateArgs,
    ReleaseActivateResult, ReleaseError, ReleaseId, ReleaseInstallArgs, ReleaseInstallResult,
    ReleaseManifest, ReleasePublishArgs,
};

/// Stable key for one chunk of one artifact: fixed-length `(storage_id, chunk_index)` so the
/// compact internal storage id (not the full `ArtifactId`) is carried per chunk row.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ArtifactChunkKey {
    pub storage_id: ArtifactStorageId,
    pub chunk_index: u32,
}

/// Verified canonical chunk bytes. Named-field wrapper required by the stable Storable contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactChunk {
    pub bytes: Vec<u8>,
}

// === Dictionary catalog types (plan 0335 todo 1) ============================

/// Max length of each identifier component (`kind`, `version`) of a `DictCatalogKey`
/// (mirrors MAX_ARTIFACT_SEMANTIC_VERSION_LEN).
pub const MAX_DICT_CATALOG_ID_LEN: usize = 128;
/// Hard cap on one catalog entry's accumulated compressed bytes. The measured operating
/// point is 10,892,359 B (ipadic 2.7.0, zstd-19); this bound is fail-closed headroom.
pub const MAX_DICT_CATALOG_COMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

/// Bounded per-principal audit log entries for dictionary catalog operations (R5 strict,
/// mirrors ARTIFACT_AUDIT_LOG_PER_PRINCIPAL_CAP).
pub const MAX_DICT_CATALOG_AUDIT_PER_PRINCIPAL_CAP: usize = 1024;

/// Composite stable key identifying one dictionary catalog entry: dictionary kind plus
/// dictionary version (first expected entry: kind "ipadic", version "2.7.0").
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct DictCatalogKey {
    pub kind: String,
    pub version: String,
}

/// Lifecycle of a dictionary catalog entry: Uploading -> Finalized. Finalized is terminal;
/// digest mismatch at finalize is fail-closed and leaves the entry unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DictCatalogState {
    Uploading,
    Finalized,
}

/// Catalog entry metadata. Each frame (compressed bytes) lives as a `DictChunk` row in the
/// dedicated chunk region (ADR 0087 chunk-store shape) so the relay (todo 3) can re-stream
/// frame i as relay call i without reassembling the artifact. Frame boundaries are part of
/// the artifact identity: each row is one independent zstd frame whose digest the relay
/// forwards to the text canister for pre-decode verification (plan 0342).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictCatalogEntry {
    pub key: DictCatalogKey,
    pub state: DictCatalogState,
    /// Number of appended frames. During Uploading, the next append must target this index.
    /// Invariant: `per_frame_digests.len() == chunks_received as usize`.
    pub chunks_received: u32,
    /// Accumulated compressed byte length across all appended frames (status display).
    pub compressed_len: u64,
    /// xxh3_128 of each appended frame, verified at upload time. Length matches
    /// `chunks_received`; the relay forwards per-frame digests to the text canister.
    pub per_frame_digests: Vec<u128>,
    /// xxh3_128 over the RAW container, computed off-band by the uploader and pinned as
    /// metadata (Provision never decompresses).
    pub raw_digest: Option<u128>,
    pub raw_len: Option<u64>,
    pub started_at_ns: u64,
    pub finalized_at_ns: Option<u64>,
}

/// One compressed zstd frame of a dictionary catalog entry (≤ the kernel relay cap).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictChunk {
    pub bytes: Vec<u8>,
}

/// Stable key for one chunk of one catalog entry.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct DictChunkKey {
    pub catalog_key: DictCatalogKey,
    pub chunk_index: u32,
}

/// Bytes-free status view returned by the three dictionary catalog endpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictCatalogStatus {
    pub key: DictCatalogKey,
    pub state: DictCatalogState,
    pub chunks_received: u32,
    pub compressed_len: u64,
    /// xxh3_128 of each frame, verified at upload time (empty while Uploading with no
    /// frames yet — length always equals chunks_received).
    pub per_frame_digests: Vec<u128>,
    pub raw_digest: Option<u128>,
    pub raw_len: Option<u64>,
}

/// Arguments for `admin_upload_dict_catalog_chunk`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictCatalogUploadChunkArgs {
    pub key: DictCatalogKey,
    pub chunk_index: u32,
    /// xxh3_128 over `bytes` (frame i's digest, verified before the row is appended).
    pub frame_digest: u128,
    pub bytes: Vec<u8>,
}

/// Arguments for `admin_finalize_dict_catalog`. The former whole-stream compressed digest
/// is gone: rows are digest-verified at upload time (plan 0342), so finalize only pins the
/// raw metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictCatalogFinalizeArgs {
    pub key: DictCatalogKey,
    /// xxh3_128 over the RAW container (pinned as metadata, never verified here).
    pub raw_digest: u128,
    pub raw_len: u64,
}

/// Errors returned by dictionary catalog ingress methods.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DictCatalogError {
    Unauthorized,
    NotFound(DictCatalogKey),
    IdentifierTooLong {
        max: u32,
    },
    EmptyChunk,
    ChunkTooLarge {
        len: u64,
        max: u32,
    },
    /// Append received at an index other than the next append position.
    ChunkOutOfOrder {
        expected_chunk_index: u32,
        received: u32,
    },
    /// Replay of an already-received chunk whose bytes do not match the stored chunk.
    ChunkReplayMismatch {
        chunk_index: u32,
    },
    /// Frame digest mismatch: the appended bytes do not hash to `frame_digest`. Fail-closed
    /// for that call only, no state change (plan 0342 per-frame verification).
    ChunkDigestMismatch {
        chunk_index: u32,
    },
    CompressedTooLarge {
        len: u64,
        max: u64,
    },
    /// Finalize invariant violation (frame-digest/count mismatch): fail-closed, no change.
    FrameDigestIncomplete,
    /// Chunk append against an entry that already reached Finalized.
    AlreadyFinalized(DictCatalogKey),
    /// Replay of finalize (or metadata pin) with different values: fail-closed.
    ReplayMismatch,
    InvalidRawLen,
}

/// Action recorded for every dictionary catalog decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DictCatalogAuditAction {
    UploadChunk,
    Finalize,
}

/// One durable audit row for dictionary catalog operations (mirrors the artifact audit
/// precedent, ADR 0087; per-principal bounded).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictCatalogAuditEntry {
    pub caller: Principal,
    pub action: DictCatalogAuditAction,
    pub key: Option<DictCatalogKey>,
    pub outcome: ArtifactAuditOutcome,
    pub reason: Option<String>,
    pub timestamp_ns: u64,
}

impl Storable for DictCatalogKey {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DictCatalogKey"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DictCatalogKey")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode DictCatalogKey")
    }
}

impl Storable for DictCatalogEntry {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DictCatalogEntry"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DictCatalogEntry")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode DictCatalogEntry")
    }
}

impl Storable for DictChunkKey {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DictChunkKey"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DictChunkKey")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode DictChunkKey")
    }
}

impl Storable for DictChunk {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DictChunk"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DictChunk")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode DictChunk")
    }
}

impl Storable for DictCatalogAuditEntry {
    const BOUND: StorableBound = StorableBound::Unbounded;
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DictCatalogAuditEntry"))
    }
    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DictCatalogAuditEntry")
    }
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode DictCatalogAuditEntry")
    }
}

//
// Shared artifact/release types implement `Storable` in `gleaph-artifact-api` behind its
// `stable` feature. Only the Provision-local chunk key/value are encoded here.

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

/// Compute the SHA-256 digest of `bytes`. Used by handlers and tests.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod shared_wire_tests {
    use super::*;
    use candid::{Decode, Encode};
    use gleaph_artifact_api::types as shared;

    /// Provision and the neutral artifact API must expose the same types, not merely
    /// Candid-compatible copies. The re-exports make this a type equality; the round-trip
    /// covers the metadata field the old mirror carried as a different type.
    #[test]
    fn artifact_metadata_round_trips_between_provision_and_the_shared_contract() {
        let id = ArtifactId::new(CanisterKind::Graph, "1.0.0".to_owned(), [0x11; 32]);
        let metadata = ArtifactMetadata {
            verified: false,
            artifact_id: id.clone(),
            chunk_hashes: vec![[0x22; 32]],
            byte_length: 3,
            created_at_ns: 4,
            storage_id: ArtifactStorageId(7),
        };
        let encoded = Encode!(&metadata).expect("encode provision metadata");
        let decoded: shared::ArtifactMetadata =
            Decode!(&encoded, shared::ArtifactMetadata).expect("decode shared metadata");
        assert_eq!(decoded.storage_id, shared::ArtifactStorageId(7));
        assert_eq!(decoded.artifact_id, id);
        let back: ArtifactMetadata = Decode!(
            &Encode!(&decoded).expect("encode shared metadata"),
            ArtifactMetadata
        )
        .expect("decode provision metadata");
        assert_eq!(back, metadata);
    }

    #[test]
    fn install_and_audit_wire_types_round_trip_against_the_shared_contract() {
        let id = ArtifactId::new(CanisterKind::Router, "2.0.0".to_owned(), [0x33; 32]);
        let args = ReleaseInstallArgs {
            target_canister_kind: CanisterKind::Router,
            registry_version: 9,
            install_args: vec![1, 2, 3],
            target_canister_id: Some(Principal::anonymous()),
        };
        let shared_args: shared::ReleaseInstallArgs = Decode!(
            &Encode!(&args).expect("encode install args"),
            shared::ReleaseInstallArgs
        )
        .expect("decode shared install args");
        assert_eq!(shared_args, args);

        let entry = ArtifactAuditEntry {
            action: ArtifactAuditAction::InstallRelease,
            timestamp_ns: 10,
            artifact_id: Some(id.clone()),
            release_id: Some(ReleaseId("rel".to_owned())),
            target_canister: Some(Principal::anonymous()),
            caller: Principal::anonymous(),
            outcome: ArtifactAuditOutcome::Success,
            deployment_id: Some("dep".to_owned()),
            reason: None,
        };
        let shared_entry: shared::ArtifactAuditEntry = Decode!(
            &Encode!(&entry).expect("encode audit entry"),
            shared::ArtifactAuditEntry
        )
        .expect("decode shared audit entry");
        assert_eq!(shared_entry, entry);

        let install_error = InstallError::ArtifactNotFound(id);
        let shared_error: shared::InstallError = Decode!(
            &Encode!(&install_error).expect("encode install error"),
            shared::InstallError
        )
        .expect("decode shared install error");
        assert_eq!(shared_error, install_error);
    }
}
