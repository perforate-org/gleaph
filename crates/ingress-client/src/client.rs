//! The typed Provision artifact/release surface over an [`IcIngress`].
//!
//! Inherent methods return `Result<Result<T, E>, IngressError>` so callers can distinguish
//! server rejections from transport failures; the [`ArtifactTransport`] implementation feeds
//! the shared ingestion driver (`crates/artifact-api/src/driver.rs`).

use std::future::Future;
use std::sync::Arc;

use gleaph_artifact_api::ArtifactTransport;
use gleaph_artifact_api::types::{
    ArtifactError, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload,
    ArtifactUploadChunkArgs, ReleaseActivateArgs, ReleaseActivateResult, ReleaseError,
    ReleaseManifest, ReleasePublishArgs,
};

use crate::ingress::{IngressError, IcIngress};
use crate::wire::{
    ArtifactAuditEntry, BootstrapAuthEntry, InstallError, ReleaseInstallArgs,
    ReleaseInstallResult, UpsertDeploymentGrantArgs, UpsertDeploymentGrantError,
};

/// Typed Provision artifact/release surface over an [`IcIngress`].
pub struct ProvisionClient<'a> {
    ingress: &'a IcIngress,
    provision: candid::Principal,
    on_chunk_uploaded: Option<Arc<dyn Fn(u32) + Send + Sync>>,
}

impl<'a> ProvisionClient<'a> {
    /// Build a client against the Provision canister at `provision`.
    pub fn new(ingress: &'a IcIngress, provision: candid::Principal) -> Self {
        Self {
            ingress,
            provision,
            on_chunk_uploaded: None,
        }
    }

    /// Register a progress sink invoked after each accepted chunk upload with the chunk's
    /// zero-based index. Used by `artifact ingest` for simple per-chunk progress output; the
    /// total chunk count stays at the call site that knows the plan.
    pub fn set_on_chunk_uploaded(&mut self, sink: Arc<dyn Fn(u32) + Send + Sync>) {
        self.on_chunk_uploaded = Some(sink);
    }

    /// Typed `artifact_get_status` (query). Distinct from the trait method so transport
    /// failures keep their own error channel outside driver runs.
    ///
    /// The did declares `(ArtifactId) -> (opt ArtifactUpload)` — a plain option, not a
    /// `Result` envelope — so the only failure mode is transport.
    pub async fn artifact_status(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<Option<ArtifactUpload>, IngressError> {
        self.ingress
            .query_value(self.provision, "artifact_get_status", &artifact_id)
            .await
    }

    /// Typed `release_get_active` (query); plain `opt` response, not a `Result` envelope.
    pub async fn release_get_active(&self) -> Result<Option<ReleaseActivateResult>, IngressError> {
        self.ingress
            .query_value(self.provision, "release_get_active", &())
            .await
    }

    /// Typed `release_publish`.
    pub async fn release_publish(
        &self,
        args: ReleasePublishArgs,
    ) -> Result<Result<ReleaseManifest, ReleaseError>, IngressError> {
        self.ingress
            .update_result(self.provision, "release_publish", &args)
            .await
    }

    /// Typed `release_activate`.
    pub async fn release_activate(
        &self,
        args: ReleaseActivateArgs,
    ) -> Result<Result<ReleaseActivateResult, ReleaseError>, IngressError> {
        self.ingress
            .update_result(self.provision, "release_activate", &args)
            .await
    }

    /// Typed `release_install`.
    pub async fn release_install(
        &self,
        args: ReleaseInstallArgs,
    ) -> Result<Result<ReleaseInstallResult, InstallError>, IngressError> {
        self.ingress
            .update_result(self.provision, "release_install", &args)
            .await
    }

    /// Typed `upsert_deployment_grant`.
    pub async fn upsert_deployment_grant(
        &self,
        args: UpsertDeploymentGrantArgs,
    ) -> Result<Result<BootstrapAuthEntry, UpsertDeploymentGrantError>, IngressError> {
        self.ingress
            .update_result(self.provision, "upsert_deployment_grant", &args)
            .await
    }

    /// Typed `artifact_audit_history` (query).
    pub async fn artifact_audit_history(
        &self,
    ) -> Result<Result<Vec<ArtifactAuditEntry>, ArtifactError>, IngressError> {
        self.ingress
            .query_result(self.provision, "artifact_audit_history", &())
            .await
    }

    fn fail_transport(&self, method: &str, error: IngressError) -> ! {
        transport_failure(method, self.provision, &error)
    }
}

/// Terminal handler for ingress failures observed inside an
/// [`gleaph_artifact_api::ingest_artifact`] run. The trait exposes only the server's
/// typed error channel, so there is no honest wire value for "the network failed"; failing
/// loudly keeps the driver's contract intact. Ingestion is idempotent — re-running the same
/// command resumes from the server-reported state.
fn transport_failure(method: &str, provision: candid::Principal, error: &IngressError) -> ! {
    panic!(
        "transient IC failure while calling {method} on provision {provision}: {error}; \
         fix connectivity and re-run the command — ingestion resumes idempotently"
    )
}

impl ArtifactTransport for ProvisionClient<'_> {
    fn publish_metadata(
        &self,
        args: ArtifactPublishMetadataArgs,
    ) -> impl Future<Output = Result<ArtifactMetadata, ArtifactError>> + Send {
        async move {
            self.ingress
                .update_result(self.provision, "artifact_publish_metadata", &args)
                .await
                .unwrap_or_else(|error| self.fail_transport("artifact_publish_metadata", error))
        }
    }

    fn upload_chunk(
        &self,
        args: ArtifactUploadChunkArgs,
    ) -> impl Future<Output = Result<ArtifactUpload, ArtifactError>> + Send {
        async move {
            let inner = self
                .ingress
                .update_result(self.provision, "artifact_upload_chunk", &args)
                .await
                .unwrap_or_else(|error| self.fail_transport("artifact_upload_chunk", error));
            if inner.is_ok()
                && let Some(sink) = &self.on_chunk_uploaded
            {
                sink(args.chunk_index);
            }
            inner
        }
    }

    fn get_status(
        &self,
        artifact_id: ArtifactId,
    ) -> impl Future<Output = Result<Option<ArtifactUpload>, ArtifactError>> + Send {
        async move {
            // The did declares `(ArtifactId) -> (opt ArtifactUpload)`: the wire carries a
            // plain option — there is no server error channel on this method. Decode the
            // option explicitly (a Result target here would decode-fail against the plain
            // opt) and lift into the trait's error channel; any transport failure
            // terminates the run per the crate-level policy.
            let status: Option<ArtifactUpload> = self
                .ingress
                .query_value(self.provision, "artifact_get_status", &artifact_id)
                .await
                .unwrap_or_else(|error| self.fail_transport("artifact_get_status", error));
            Ok(status)
        }
    }

    fn release_publish(
        &self,
        args: ReleasePublishArgs,
    ) -> impl Future<Output = Result<ReleaseManifest, ReleaseError>> + Send {
        async move {
            self.ingress
                .update_result(self.provision, "release_publish", &args)
                .await
                .unwrap_or_else(|error| self.fail_transport("release_publish", error))
        }
    }

    fn release_activate(
        &self,
        args: ReleaseActivateArgs,
    ) -> impl Future<Output = Result<ReleaseActivateResult, ReleaseError>> + Send {
        async move {
            self.ingress
                .update_result(self.provision, "release_activate", &args)
                .await
                .unwrap_or_else(|error| self.fail_transport("release_activate", error))
        }
    }
}
