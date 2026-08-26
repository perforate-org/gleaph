//! Transport surface for the Provision artifact-ingestion protocol.
//!
//! Implementations adapt the five candid ingress methods to a concrete caller (ic-agent in
//! `gleaph-operator`, an in-memory fake in tests, a canister-to-canister caller later). The
//! methods use native async-fn-in-trait — return-position `impl Future`, no `async-trait`
//! boxing — and carry `+ Send` so callers can drive them inside ordinary runtimes such as
//! tokio. Implementations are still selected statically via generics: no `dyn` compatibility
//! exists or is needed.
//!
//! The explicit `impl Future<...> + Send` form follows the repo's generated-SDK trait
//! precedent (`crates/codegen/src/rust/shared.rs`).

use std::future::Future;

use crate::types::{
    ArtifactError, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload,
    ArtifactUploadChunkArgs, ReleaseActivateArgs, ReleaseActivateResult, ReleaseError,
    ReleaseManifest, ReleasePublishArgs,
};

/// The five Provision catalog/release operations consumed by the ingestion pipeline.
///
/// Method semantics mirror the Provision handlers (`crates/provision/provision.did:319-337`):
/// each method maps to exactly one candid ingress call and returns the server's typed
/// `Ok`/`Err` pair unchanged.
pub trait ArtifactTransport {
    /// Publish immutable artifact metadata
    /// (candid `artifact_publish_metadata`, `provision.did:326`).
    ///
    /// Rejects with [`ArtifactError::ConflictingMetadata`] when the exact identity already
    /// exists — including identical re-publishes; there is no replay-Ok path.
    fn publish_metadata(
        &self,
        args: ArtifactPublishMetadataArgs,
    ) -> impl Future<Output = Result<ArtifactMetadata, ArtifactError>> + Send;

    /// Upload one chunk (candid `artifact_upload_chunk`, `provision.did:327`). Accepting the
    /// final declared chunk triggers full SHA-256 verification inside the same call, so the
    /// returned state is already [`crate::types::ArtifactUploadState::Verified`] on success.
    fn upload_chunk(
        &self,
        args: ArtifactUploadChunkArgs,
    ) -> impl Future<Output = Result<ArtifactUpload, ArtifactError>> + Send;

    /// Query current upload progress (candid `artifact_get_status`, `provision.did:325`,
    /// query). Returns `Ok(None)` both before publication and after verification reclaimed
    /// the upload row.
    fn get_status(
        &self,
        artifact_id: ArtifactId,
    ) -> impl Future<Output = Result<Option<ArtifactUpload>, ArtifactError>> + Send;

    /// Publish a release manifest (candid `release_publish`, `provision.did:336`); returns the
    /// canonicalized manifest.
    fn release_publish(
        &self,
        args: ReleasePublishArgs,
    ) -> impl Future<Output = Result<ReleaseManifest, ReleaseError>> + Send;

    /// Activate a published release (candid `release_activate`, `provision.did:333`).
    fn release_activate(
        &self,
        args: ReleaseActivateArgs,
    ) -> impl Future<Output = Result<ReleaseActivateResult, ReleaseError>> + Send;
}
