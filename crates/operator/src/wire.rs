//! Mirrored Provision wire types for the operations outside the ingestion pipeline.
//!
//! Authority: `crates/provision/provision.did` (and `crates/provision/src/types.rs`). Each
//! item cites its source lines and keeps the did's declaration order; candid encodes records
//! and variants by field-name hash, so name equality is what guarantees wire compatibility —
//! the matching order is kept so a reviewer can diff this file against the did line by line,
//! exactly like `crates/artifact-api/src/types.rs`.
//!
//! The mirrors consumed by the typed [`ProvisionClient`] (`release_install`,
//! `upsert_deployment_grant`, `artifact_audit_history`) live in the shared
//! `gleaph-ingress-client` crate next to the client and are re-exported here so operator
//! import paths (`crate::wire::InstallError`, the PocketIC E2E's `gleaph_operator::wire`)
//! keep working. This file keeps only the bootstrap-tier management-canister mirrors that
//! the client does not touch (`canister_status` reply shapes for `bootstrap.rs`).

use candid::CandidType;
use serde::{Deserialize, Serialize};

pub use gleaph_ingress_client::wire::{
    ArtifactAuditAction, ArtifactAuditEntry, ArtifactAuditOutcome, BootstrapAuthAction,
    BootstrapAuthEntry, InstallError, ReleaseInstallArgs, ReleaseInstallResult,
    UpsertDeploymentGrantArgs, UpsertDeploymentGrantError,
};

// === IC management canister: canister_status reply ==========================

/// Hand-mirrored `canister_status_result` (IC management canister,
/// `canister_status : (record { canister_id }) -> (canister_status_result) query`;
/// official management did at docs.internetcomputer.org/references/ic.did,
/// `canister_status_result` / `definite_canister_settings` / `memory_metrics` blocks).
///
/// Why a hand mirror while every other management shape uses `ic-management-canister-types`
/// (SSOT decision recorded in `crates/operator/src/bootstrap.rs`): candid rejects missing
/// required fields, and the dependency's current schema requires settings fields
/// (`status_visibility`, …) and metrics fields (`log_memory_store_size`) that the replica
/// generation this tool is validated against does not send — proven empirically by decoding
/// one live reply both ways in the PocketIC E2E (`adr0087_bootstrap_tier`). The mirror
/// carries exactly the displayed fields, with volatile ones optional, so it decodes across
/// replica generations that add or drop metrics/settings fields (extra wire fields are
/// ignored by candid). Request shapes stay on the dependency: those matched the official did
/// exactly and are accepted by real replicas end to end.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct CanisterStatusReply {
    /// Lifecycle state (always present on the wire).
    pub status: ManagementStatusKind,
    /// SHA-256 of the installed module; `None` while the canister is empty.
    pub module_hash: Option<Vec<u8>>,
    /// Canister version (monotonic counter).
    pub version: Option<u64>,
    /// Current cycle balance.
    pub cycles: Option<candid::Nat>,
    /// Cycles reserved by storage allocations.
    pub reserved_cycles: Option<candid::Nat>,
    /// Estimated daily idle burn.
    pub idle_cycles_burned_per_day: Option<candid::Nat>,
    /// Total memory size.
    pub memory_size: Option<candid::Nat>,
    /// Effective settings (only the controllers are displayed).
    pub settings: Option<ManagementDefiniteCanisterSettings>,
}

/// Hand-mirrored `definite_canister_settings`, reduced to the displayed field; all fields
/// optional for cross-generation decode tolerance (see [`CanisterStatusReply`]).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ManagementDefiniteCanisterSettings {
    /// Controllers of the canister (`vec principal` in the did).
    pub controllers: Option<Vec<candid::Principal>>,
}

/// Hand-mirrored `canister_status_type` (`variant { running; stopping; stopped }`).
///
/// Source: official management did, `canister_status_type`. Variant order follows the did;
/// the serde renames pin the lowercase did names, which the candid derive uses as wire
/// labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ManagementStatusKind {
    /// The canister executes calls.
    #[serde(rename = "running")]
    Running,
    /// The canister is draining before stopping.
    #[serde(rename = "stopping")]
    Stopping,
    /// The canister is stopped (required before upgrade).
    #[serde(rename = "stopped")]
    Stopped,
}

impl ManagementStatusKind {
    /// Lowercase did variant name, for output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }
}

// === Release install ========================================================

// Release-install mirrors (ReleaseInstallArgs / ReleaseInstallResult / InstallError),
// deployment-grant mirrors, and artifact-audit mirrors moved to
// `gleaph_ingress_client::wire` and are re-exported above.
