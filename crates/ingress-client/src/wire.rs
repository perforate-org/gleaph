//! Provision wire types for the operations outside the ingestion pipeline.
//!
//! Artifact/release/install/audit types are owned by `gleaph-artifact-api` and re-exported here;
//! this file keeps only the provisioning control-plane mirrors (`upsert_deployment_grant`,
//! bootstrap authority) that are not part of the artifact contract. Candid encodes records and
//! variants by name, so the shared artifact types are byte-identical for operator, CLI and
//! canister callers.

use candid::CandidType;
use serde::{Deserialize, Serialize};

pub use gleaph_artifact_api::types::{
    ArtifactAuditAction, ArtifactAuditEntry, ArtifactAuditOutcome, ArtifactError, ArtifactId,
    ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactStorageId, ArtifactUpload,
    ArtifactUploadChunkArgs, ArtifactUploadState, CanisterKind, InstallError, MAX_ARTIFACT_BYTES,
    MAX_ARTIFACT_CHUNKS, MAX_ARTIFACT_SEMANTIC_VERSION_LEN, ReleaseActivateArgs,
    ReleaseActivateResult, ReleaseError, ReleaseId, ReleaseInstallArgs, ReleaseInstallResult,
    ReleaseManifest, ReleasePublishArgs,
};

// === Deployment grant upsert =============================================

/// What an issuer may name as the deployment in a `ProvisionRequest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DeploymentGrantScope {
    /// The issuer is the deployment; `deployment_id` must equal the caller's principal text.
    OwnDeployment,
    /// A trusted relay (Account) may name the account it issues for.
    Relay,
}

/// Arguments for `upsert_deployment_grant`.
///
/// Source: `crates/provision/provision.did`, `crates/provision/src/types.rs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct UpsertDeploymentGrantArgs {
    /// Whether the issuer may name a deployment other than itself.
    pub scope: DeploymentGrantScope,
    /// The principal authorized to request issuance.
    pub issuer: candid::Principal,
}

/// Error returned by `upsert_deployment_grant`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum UpsertDeploymentGrantError {
    /// Bootstrap authority has not been seeded yet.
    NoBootstrapAuthority,
    /// Caller is not the governance authority (or the issuer is anonymous).
    Unauthorized,
}

/// Action recorded for every bootstrap-authority decision.
///
/// Source: `crates/provision/provision.did:111-118`,
/// `crates/provision/src/types.rs:326-333`. Variant order follows the did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum BootstrapAuthAction {
    /// First authority seed at init.
    InitialSeed,
    /// A rejected upsert before the authority was seeded.
    RejectNotSeeded,
    /// A successful grant upsert.
    Upsert,
    /// A rejected upsert by a non-governance caller (or anonymous issuer).
    RejectUnauthorized,
}

/// One durable audit row in PROVISION_BOOTSTRAP_AUDIT_LOG (MemoryId 5).
///
/// Source: `crates/provision/provision.did:119-126`,
/// `crates/provision/src/types.rs:335-343`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct BootstrapAuthEntry {
    /// Recorded action.
    pub action: BootstrapAuthAction,
    /// Timestamp (IC NNS nanoseconds).
    pub timestamp_ns: u64,
    /// Caller that produced the decision.
    pub caller: candid::Principal,
    /// Related deployment (= issuer principal text), if any.
    pub deployment_id: Option<String>,
}

// === Service ledger and withdrawals ======================================
//
// Source: `crates/provision/provision.did`, `crates/provision/src/types.rs`.

/// Current withheld fee and reserve pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ServiceLedger {
    /// Withheld platform fee cycles.
    pub platform_fees: u128,
    /// Withheld compensation reserve cycles.
    pub compensation_reserve: u128,
}

/// Which withheld pool a withdrawal debits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum WithdrawalKind {
    /// Platform fee pool.
    PlatformFees,
    /// Compensation reserve pool.
    CompensationReserve,
}

/// Durable status of one withdrawal.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum WithdrawalStatus {
    /// Transfer in flight or awaiting reconciliation.
    Pending,
    /// Transfer completed.
    Completed,
    /// Definitive failure with the restored pool.
    Failed(String),
}

/// One withdrawal record.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct WithdrawalRecord {
    /// Selected pool.
    pub kind: WithdrawalKind,
    /// Destination canister.
    pub destination: candid::Principal,
    /// Withdrawn cycles.
    pub amount: u128,
    /// Current status.
    pub status: WithdrawalStatus,
}

/// Arguments of `withdraw_fees`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct WithdrawArgs {
    /// Caller-chosen 32-byte idempotency key.
    pub idempotency_key: [u8; 32],
    /// Selected pool.
    pub kind: WithdrawalKind,
    /// Destination canister.
    pub destination: candid::Principal,
    /// Cycles to withdraw.
    pub amount: u128,
}

/// Result of `withdraw_fees`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct WithdrawResult {
    /// Recorded amount.
    pub amount: u128,
    /// True when an exact completed replay performed no transfer.
    pub replayed: bool,
}

/// Error of `withdraw_fees` and the read queries.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum WithdrawError {
    /// Caller is not the configured operator (or governance for the ledger read).
    Unauthorized,
    /// `ServiceConfig.operator` is unset.
    OperatorNotConfigured,
    /// Anonymous destination.
    AnonymousDestination,
    /// Destination is the Provision canister itself.
    InvalidDestination,
    /// Zero amount.
    AmountZero,
    /// The selected pool does not hold the amount.
    InsufficientLedger,
    /// Same key with a different payload, or a still-pending key.
    IdempotencyConflict,
    /// The key previously failed definitively; use a new key.
    PreviousAttemptFailed,
    /// The record is not in the expected state.
    InvalidState,
    /// The management reply was unavailable; the deposit may have landed.
    TransferPending,
    /// Definitive transfer failure with restored ledger.
    TransferFailed(String),
}

// === Deployment record ===================================================

/// Managed vs SelfCustody controller policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ControllerMode {
    /// `[Provision, operator]` controllers.
    Managed,
    /// `[Provision, owner]` controllers.
    SelfCustody,
}

/// One deployment's owner, controller policy, operator, and cycles credit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct DeploymentRecord {
    /// Deployment owner.
    pub owner: candid::Principal,
    /// Controller policy.
    pub controller_mode: ControllerMode,
    /// Operator snapshot for Managed deployments.
    pub operator: Option<candid::Principal>,
    /// Current prepaid cycles credit.
    pub credit_cycles: u128,
}

/// Error of `get_deployment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum DeploymentQueryError {
    /// Caller is not owner, operator, billing authority, or governance.
    Unauthorized,
    /// The deployment id has no record.
    UnknownDeployment,
}
