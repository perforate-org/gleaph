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
