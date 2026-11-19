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

// === Deployment grant and bootstrap authority ============================
//
// One definition shared by Provision, this client, and the CLI.

pub use gleaph_provision_api::{
    BootstrapAuthAction, BootstrapAuthEntry, DeploymentGrantScope, UpsertDeploymentGrantArgs,
    UpsertDeploymentGrantError,
};

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

// === Billing credit grants ===============================================

/// Arguments of `grant_credit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GrantCreditArgs {
    /// Target deployment.
    pub deployment_id: candid::Principal,
    /// Cycles to add.
    pub amount: u128,
    /// Caller-chosen 32-byte idempotency key (the external payment id).
    pub idempotency_key: [u8; 32],
}

/// Result of `grant_credit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GrantCreditResult {
    /// Target deployment.
    pub deployment_id: candid::Principal,
    /// Granted amount.
    pub amount: u128,
    /// Credit after the grant.
    pub credit_cycles: u128,
    /// True for an exact replay.
    pub replayed: bool,
}

/// Error of `grant_credit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GrantCreditError {
    /// Caller is not the configured billing authority.
    Unauthorized,
    /// `ServiceConfig.billing_authority` is unset.
    BillingAuthorityNotConfigured,
    /// No deployment record.
    UnknownDeployment,
    /// Zero amount.
    AmountZero,
    /// Same key with a different payload.
    IdempotencyConflict,
    /// Credit arithmetic overflow.
    CreditOverflow,
    /// Record not in the expected state.
    InvalidState,
}

/// Arguments of `reverse_credit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ReverseCreditArgs {
    /// Original grant authority.
    pub grant_authority: candid::Principal,
    /// Original grant's idempotency key.
    pub idempotency_key: [u8; 32],
}

/// Result of `reverse_credit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ReverseCreditResult {
    /// Target deployment.
    pub deployment_id: candid::Principal,
    /// Reversed amount.
    pub amount: u128,
    /// Credit after the reversal.
    pub credit_cycles: u128,
    /// True for an exact replay.
    pub replayed: bool,
}

/// Error of `reverse_credit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ReverseCreditError {
    /// Caller is not the configured billing authority.
    Unauthorized,
    /// `ServiceConfig.billing_authority` is unset.
    BillingAuthorityNotConfigured,
    /// No grant row for the key.
    UnknownGrant,
    /// Original authority is anonymous.
    AnonymousGrantAuthority,
    /// Deployment has spent part of the grant.
    InsufficientCredit,
    /// Record not in the expected state.
    InvalidState,
}
