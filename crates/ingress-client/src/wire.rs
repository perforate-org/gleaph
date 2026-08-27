//! Mirrored Provision wire types for the operations outside the ingestion pipeline.
//!
//! Authority: `crates/provision/provision.did` (and `crates/provision/src/types.rs`). Each
//! item cites its source lines and keeps the did's declaration order; candid encodes records
//! and variants by field-name hash, so name equality is what guarantees wire compatibility —
//! the matching order is kept so a reviewer can diff this file against the did line by line,
//! exactly like `crates/artifact-api/src/types.rs`.
//!
//! These mirrors live here rather than in `gleaph-artifact-api` because they are not part of
//! the ingestion pipeline contract that crate owns (`release_install`,
//! `admin_install_deployment_binding`, `artifact_audit_history`); compatibility with the real
//! canister is proven by the PocketIC E2E (`adr0087_operator_ingestion`), which round-trips
//! every one of them through both this mirror and the server's own types.

use candid::CandidType;
use gleaph_artifact_api::types::{ArtifactId, CanisterKind, ReleaseId};
use serde::{Deserialize, Serialize};

// === Release install ========================================================

/// Arguments for `release_install`.
///
/// Source: `crates/provision/provision.did:277-283`,
/// `crates/provision/src/types.rs:576-582`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseInstallArgs {
    /// Which kind of artifact from the active release to install.
    pub target_canister_kind: CanisterKind,
    /// Registry version at install time (recorded on the call; the handler resolves the
    /// active release itself).
    pub registry_version: u64,
    /// Candid-encoded init argument forwarded verbatim to `install_chunked_code`.
    pub install_args: Vec<u8>,
    /// Explicit target canister. The handler rejects `None`
    /// (`crates/provision/src/canister/mod.rs:1487-1504`).
    pub target_canister_id: Option<candid::Principal>,
}

/// Return value of `release_install`.
///
/// Source: `crates/provision/provision.did:284-291`,
/// `crates/provision/src/types.rs:585-592`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseInstallResult {
    /// Full SHA-256 of the installed chunk sequence (= the artifact digest).
    pub install_chunked_code_hash: [u8; 32],
    /// Active release that supplied the artifact.
    pub release_id: ReleaseId,
    /// Canister the code was installed into.
    pub target_canister_id: candid::Principal,
    /// Install completion timestamp (IC NNS nanoseconds).
    pub installed_at_ns: u64,
    /// Number of chunks streamed from the catalog into the management canister.
    pub installed_chunks: u32,
}

/// Errors returned by `release_install`.
///
/// Source: `crates/provision/provision.did:142-152`,
/// `crates/provision/src/types.rs:595-605`. Variant order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum InstallError {
    /// Referenced artifact missing from the catalog.
    ArtifactNotFound(
        /// The missing identity.
        ArtifactId,
    ),
    /// No release is currently active.
    NoActiveRelease,
    /// A management-canister step failed during chunk upload or `install_chunked_code`.
    ManagementCanisterCallFailed(
        /// Management-canister failure text.
        String,
    ),
    /// Chunk store has not been reconciled yet.
    ChunkStoreNotReconciled,
    /// Target kind is forbidden for installs.
    TargetCanisterKindForbidden(
        /// The rejected kind.
        CanisterKind,
    ),
    /// Bootstrap authority has not been seeded yet.
    NoBootstrapAuthority,
    /// Referenced artifact has not reached durable verified state.
    ArtifactNotVerified(
        /// The unverified identity.
        ArtifactId,
    ),
    /// Caller is not the resolved governance authority.
    Unauthorized,
}

// === Deployment grant upsert =============================================

/// Arguments for `upsert_deployment_grant`.
///
/// Source: `crates/provision/provision.did`, `crates/provision/src/types.rs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct UpsertDeploymentGrantArgs {
    /// The principal authorized to request issuance. The deployment is the issuer itself:
    /// `deployment_id = issuer = caller`.
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

// === Artifact audit history =================================================

/// One durable audit row in PROVISION_ARTIFACT_AUDIT_LOG (MemoryId 11).
///
/// Source: `crates/provision/provision.did:27-38`,
/// `crates/provision/src/types.rs:543-554`. Field order follows the did. The did declares
/// `release_id : opt text`; [`ReleaseId`] is the neutral newtype over `text`, so the wire
/// shape is identical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactAuditEntry {
    /// Plan-level action that was attempted.
    pub action: ArtifactAuditAction,
    /// Timestamp (IC NNS nanoseconds).
    pub timestamp_ns: u64,
    /// Artifact identity when the action was artifact-scoped.
    pub artifact_id: Option<ArtifactId>,
    /// Release identifier when the action was release-scoped.
    pub release_id: Option<ReleaseId>,
    /// Target canister when the action installed code.
    pub target_canister: Option<candid::Principal>,
    /// Caller that produced the row.
    pub caller: candid::Principal,
    /// Outcome of the attempt.
    pub outcome: ArtifactAuditOutcome,
    /// Related deployment id, if any.
    pub deployment_id: Option<String>,
    /// Server-provided reason for non-success outcomes.
    pub reason: Option<String>,
}

/// Action recorded for every artifact/release plan-level operation.
///
/// Source: `crates/provision/provision.did:18-26`,
/// `crates/provision/src/types.rs:557-565`. Variant order follows the did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ArtifactAuditAction {
    /// Metadata publish attempt.
    PublishArtifact,
    /// Release activation attempt.
    ActivateRelease,
    /// Release manifest publish attempt.
    PublishRelease,
    /// Chunk upload attempt.
    UploadChunk,
    /// Streaming verification attempt.
    VerifyArtifact,
    /// Release install attempt.
    InstallRelease,
}

/// Outcome of an audited operation.
///
/// Source: `crates/provision/provision.did:40`,
/// `crates/provision/src/types.rs:567-573`. Variant order follows the did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ArtifactAuditOutcome {
    /// Rejected before any effect (authorization/boundary failure).
    Failed,
    /// Rejected by an authorization or state guard.
    Rejected,
    /// Completed successfully.
    Success,
}