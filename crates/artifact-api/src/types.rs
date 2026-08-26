//! Mirrored Provision artifact-catalog wire types and bounds constants.
//!
//! Authority: `crates/provision/provision.did` and `crates/provision/src/types.rs`. Each item
//! cites its source lines and keeps the did's declaration order. Do not rename or reorder
//! fields here without making the identical change in Provision.

use std::collections::BTreeSet;

use candid::CandidType;
use serde::{Deserialize, Serialize};

// === Client-side mirror constants ===========================================
//
// Verification aids only: the Provision canister is the SOLE enforcer of catalog bounds.
// It rejects violations at ingress (handler guards in
// `crates/provision/src/canister/mod.rs:635-659`); this crate can only pre-reject hopeless
// plans before network calls are spent and size chunks to server expectations. Update these
// values only in lockstep with their authority.

/// Upper bound for one chunk produced by the canonical pipeline's split step.
///
/// Authority: ADR 0087 §Canonical upload pipeline ("split ≤1 MiB chunks"). The server's
/// `artifact_upload_chunk` imposes no explicit per-chunk length guard — actual chunk size is
/// bounded by IC ingress message limits plus per-chunk hash and full SHA-256 verification on
/// the server (`crates/provision/src/canister/mod.rs:710-974`). This constant therefore shapes
/// the client split only; it is not a server-enforced bound.
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// Mirror of `MAX_ARTIFACT_SEMANTIC_VERSION_LEN`
/// (`crates/provision/src/types.rs:481`). Server-enforced; client-side check is advisory.
pub const MAX_ARTIFACT_SEMANTIC_VERSION_LEN: usize = 128;

/// Mirror of `MAX_ARTIFACT_BYTES` (`crates/provision/src/types.rs:482`).
/// Server-enforced; client-side check is advisory.
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Mirror of `MAX_ARTIFACT_CHUNKS` (`crates/provision/src/types.rs:483`).
/// Server-enforced; client-side check is advisory.
pub const MAX_ARTIFACT_CHUNKS: u32 = 4096;

// === Kind + identity ========================================================

/// Kind of canister that an artifact can be installed into.
/// Provision itself is EXPLICITLY excluded — self-upgrade is forbidden per ADR 0036.
///
/// Source: `crates/provision/provision.did:127-129`,
/// `crates/provision/src/types.rs:385-396`.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub enum CanisterKind {
    /// Router data-plane canister.
    Router,
    /// Graph execution canister.
    Graph,
    /// Property index canister.
    PropertyIndex,
    /// Vector index canister.
    VectorCanister,
    /// Text index canister.
    TextCanister,
}

/// Composite stable key identifying one published artifact.
/// The SHA-256 is part of identity, not a value field.
///
/// Source: `crates/provision/provision.did:65-71`,
/// `crates/provision/src/types.rs:398-407`. Field order follows the did.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ArtifactId {
    /// Full SHA-256 of the artifact bytes.
    pub sha256: [u8; 32],
    /// Semantic version string of the artifact.
    pub semantic_version: String,
    /// Target canister kind.
    pub canister_kind: CanisterKind,
}

impl ArtifactId {
    /// Build an id in `(kind, version, sha256)` argument order, mirroring
    /// `ArtifactId::new` at `crates/provision/src/types.rs:409-417`.
    pub fn new(canister_kind: CanisterKind, semantic_version: String, sha256: [u8; 32]) -> Self {
        Self {
            sha256,
            semantic_version,
            canister_kind,
        }
    }
}

// === Publish / status / upload arguments ====================================

/// Immutable artifact metadata published by governance (result of `artifact_publish_metadata`).
/// `verified` records that the uploaded chunks passed full SHA-256 verification; it flips once
/// on the final chunk and is then durable.
///
/// Source: `crates/provision/provision.did:72-80`,
/// `crates/provision/src/types.rs:428-441`. Field order follows the did. The did carries
/// `storage_id : nat64`; server-side it is wrapped by `ArtifactStorageId`
/// (`crates/provision/src/types.rs:419-426`), which stays internal to Provision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactMetadata {
    /// Durable flag set once full SHA-256 verification succeeds.
    pub verified: bool,
    /// Canonical wire identity of the artifact.
    pub artifact_id: ArtifactId,
    /// Declared per-chunk SHA-256 hashes.
    pub chunk_hashes: Vec<[u8; 32]>,
    /// Total declared byte length.
    pub byte_length: u64,
    /// Creation timestamp (IC NNS nanoseconds).
    pub created_at_ns: u64,
    /// Internal fixed-length storage id used as the chunk-store key prefix.
    pub storage_id: u64,
}

/// Arguments for `artifact_publish_metadata`.
///
/// Source: `crates/provision/provision.did:81-88`,
/// `crates/provision/src/types.rs:522-530`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactPublishMetadataArgs {
    /// Full SHA-256 of the complete artifact bytes.
    pub sha256: [u8; 32],
    /// Per-chunk SHA-256 hashes, one per declared chunk, in index order.
    pub chunk_hashes: Vec<[u8; 32]>,
    /// Semantic version string.
    pub semantic_version: String,
    /// Total artifact byte length.
    pub byte_length: u64,
    /// Target canister kind.
    pub canister_kind: CanisterKind,
}

/// Arguments for `artifact_upload_chunk`.
///
/// Source: `crates/provision/provision.did:98-103`,
/// `crates/provision/src/types.rs:532-538`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactUploadChunkArgs {
    /// Zero-based chunk index, strictly below the declared chunk count.
    pub chunk_index: u32,
    /// Canonical wire identity of the artifact.
    pub artifact_id: ArtifactId,
    /// Chunk bytes (must hash to the declared per-chunk value).
    pub bytes: Vec<u8>,
}

// === Upload progress state ==================================================

/// Mutable upload-progress state for an artifact (result of `artifact_upload_chunk` /
/// `artifact_get_status`). The row is reclaimed once the artifact reaches `Verified`, so a
/// `None` status means either "not published" or "verified"; see the driver docs for how the
/// ambiguity is resolved.
///
/// Source: `crates/provision/provision.did:89-97`,
/// `crates/provision/src/types.rs:443-452`. Field order follows the did. The did declares
/// `received_chunks : vec nat32`; this mirror uses `BTreeSet<u32>` like the server's canonical
/// state (`crates/provision/src/types.rs:449`) — candid encodes sets as `vec`, so the wire
/// shape is identical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ArtifactUpload {
    /// Timestamp when upload progress was first recorded.
    pub started_at_ns: u64,
    /// Canonical wire identity of the artifact.
    pub artifact_id: ArtifactId,
    /// Chunk indices accepted so far.
    pub received_chunks: BTreeSet<u32>,
    /// Set once verification succeeded (just before the row is reclaimed).
    pub verified_at_ns: Option<u64>,
    /// Current lifecycle phase.
    pub state: ArtifactUploadState,
}

/// Lifecycle of an artifact upload. Receiving -> Verifying -> (Verified | Failed).
///
/// Source: `crates/provision/provision.did:104-110`,
/// `crates/provision/src/types.rs:470-477`. Variant order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ArtifactUploadState {
    /// Full verification failed; the upload is terminal until governance republishes.
    Failed {
        /// Human-readable failure reason.
        reason: String,
    },
    /// Chunks still arriving.
    Receiving,
    /// Full verification succeeded; the durable flag is set and the row is about to be
    /// reclaimed.
    Verified {
        /// Verification completion timestamp.
        verified_at_ns: u64,
    },
    /// Server-side streaming verification in progress (transient within one upload call).
    Verifying,
}

// === Catalog errors =========================================================

/// Errors returned by artifact catalog ingress methods.
///
/// Source: `crates/provision/provision.did:41-64`,
/// `crates/provision/src/types.rs:485-520`. Variant and inner-field order follow the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ArtifactError {
    /// Declared or reached chunk count violates the chunk-count bound (also used with
    /// `declared: 0` when metadata publishes zero chunks).
    TooManyChunks {
        /// Maximum accepted chunk count.
        max: u32,
        /// Offending declared count.
        declared: u32,
    },
    /// Provision-kind artifacts are forbidden (self-upgrade exclusion).
    NotProvision(
        /// The rejected kind.
        CanisterKind,
    ),
    /// No metadata exists under this identity.
    UnknownArtifact(
        /// The unknown identity.
        ArtifactId,
    ),
    /// Caller is not the resolved governance authority.
    Unauthorized,
    /// A chunk failed its per-chunk hash check.
    ChunkHashMismatch {
        /// Index of the rejected chunk.
        chunk_index: u32,
        /// Artifact identity.
        artifact_id: ArtifactId,
    },
    /// Chunk index outside the declared range.
    ChunkOutOfRange {
        /// Offending index.
        chunk_index: u32,
        /// Artifact identity.
        artifact_id: ArtifactId,
        /// Declared chunk count.
        declared: u32,
    },
    /// Metadata already exists under this identity.
    ConflictingMetadata {
        /// Identity requested by the caller.
        requested: ArtifactId,
        /// Identity already stored (equal to `requested` iff the exact identity exists).
        existing: ArtifactId,
    },
    /// Streaming full-artifact SHA-256 mismatch during final verification.
    FullSha256Mismatch {
        /// Digest actually computed from received chunks.
        actual: [u8; 32],
        /// Artifact identity.
        artifact_id: ArtifactId,
        /// Digest declared in the identity.
        expected: [u8; 32],
    },
    /// Declared byte length exceeds the artifact size bound.
    ArtifactTooLarge {
        /// Maximum accepted byte length.
        max: u64,
        /// Offending declared byte length.
        byte_length: u64,
    },
    /// Semantic version string exceeds the length bound.
    SemanticVersionTooLong {
        /// Maximum accepted length.
        max: u32,
    },
}

// === Release publish / activate =============================================

/// Opaque release identifier (e.g. "release-2026-07-08").
///
/// Source: `crates/provision/provision.did` (carried as plain `text`, e.g. line 252),
/// `crates/provision/src/types.rs:609-613`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ReleaseId(pub String);

/// Arguments for `release_publish`.
///
/// Source: `crates/provision/provision.did:301-305`,
/// `crates/provision/src/types.rs:660-665`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleasePublishArgs {
    /// One artifact identity per included canister kind.
    pub artifact_ids: Vec<ArtifactId>,
    /// Release identifier assigned by the operator.
    pub release_id: ReleaseId,
}

/// Result of `release_publish`: the immutable release manifest as canonicalized by Provision.
///
/// Source: `crates/provision/provision.did:292-300`,
/// `crates/provision/src/types.rs:615-624`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseManifest {
    /// Graph-kind artifact of the release.
    pub graph_artifact: ArtifactId,
    /// Router-kind artifact of the release.
    pub router_artifact: ArtifactId,
    /// Vector-canister-kind artifact of the release.
    pub vector_canister_artifact: ArtifactId,
    /// Release identifier.
    pub release_id: ReleaseId,
    /// Text-canister-kind artifact of the release.
    pub text_canister_artifact: ArtifactId,
    /// Property-index-kind artifact of the release.
    pub property_index_artifact: ArtifactId,
}

/// Arguments for `release_activate`.
///
/// Source: `crates/provision/provision.did:251-252`,
/// `crates/provision/src/types.rs:667-671`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseActivateArgs {
    /// Release identifier to activate.
    pub release_id: ReleaseId,
}

/// Result of `release_activate` confirming the active release that was swapped. The
/// `previous_release_id` field records the active release before the swap and enforces the
/// non-retroactivity invariant: no job/receipt region is mutated.
///
/// Source: `crates/provision/provision.did:253-260`,
/// `crates/provision/src/types.rs:626-634`. Field order follows the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ReleaseActivateResult {
    /// Activation timestamp.
    pub activated_at_ns: u64,
    /// Previously active release, if any.
    pub previous_release_id: Option<ReleaseId>,
    /// Newly activated release identifier.
    pub release_id: ReleaseId,
}

/// Errors returned by release publish/activate ingress methods.
///
/// Source: `crates/provision/provision.did:261-276`,
/// `crates/provision/src/types.rs:636-658`. Variant and inner-field order follow the did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ReleaseError {
    /// Referenced artifact missing from the catalog.
    ArtifactNotFound(
        /// The missing identity.
        ArtifactId,
    ),
    /// Manifest does not cover every non-Provision kind.
    IncompleteManifest {
        /// Identities that were expected but absent.
        missing: Vec<ArtifactId>,
        /// Release identifier.
        release_id: ReleaseId,
    },
    /// Provision-kind artifacts are forbidden inside a release manifest.
    ProvisionKindForbidden(
        /// The offending identity.
        ArtifactId,
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
    /// No such release exists.
    UnknownRelease(
        /// Release identifier that was not found.
        ReleaseId,
    ),
    /// A different release already exists under the conflicting identity.
    ConflictingRelease {
        /// Identity requested by the caller.
        requested: ReleaseId,
        /// Identity already stored.
        existing: ReleaseId,
    },
    /// More than one artifact supplied for one canister kind.
    NotUniquePerKind {
        /// The duplicated kind.
        kind: CanisterKind,
        /// The conflicting identities.
        conflicting: Vec<ArtifactId>,
        /// Release identifier.
        release_id: ReleaseId,
    },
}
