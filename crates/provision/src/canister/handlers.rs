//! Provision canister IC-runtime entry-point shims (ADR 0035 Slice 4 + Slice 7).
//!
//! This is the only module that reads `ic_cdk::api::msg_caller()`. The thin
//! wrappers forward to the existing `*_with_caller(...)` functions, which remain
//! `pub(crate)` so the 33+ unit tests and the candid export tests can drive every branch without WASM.

use crate::canister::{
    ArtifactUpload, ProvisionIngressResult, ProvisionJobView, RouterAckResult,
    accept_envelope_with_caller, admin_install_deployment_binding_with_caller, artifact_get_status,
    artifact_publish_metadata_with_caller, artifact_upload_chunk_with_caller,
    query_job_with_caller, release_activate_with_caller, release_get_active,
    release_publish_with_caller, router_ack_with_caller,
};
use crate::stable::store::{DeploymentTrustStore, ProvisionJobStore};
use crate::types::{
    AdminInstallDeploymentBindingArgs, ArtifactError, ArtifactId, ArtifactMetadata,
    ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, ProvisionRequest, ReleaseActivateArgs,
    ReleaseActivateResult, ReleaseError, ReleaseManifest, ReleasePublishArgs, RouterProvisionAck,
};

/// Bootstrap the deployment trust store from init args.
pub fn init_handler(args: crate::canister::init::ProvisionInitArgs) {
    crate::canister::init::init(args);
}

/// No-op: `DeploymentTrustStore` survives upgrades via stable memory already.
pub fn post_upgrade_handler() {}

/// Authorize `accept_envelope` from the IC runtime and forward to the handler.
pub fn accept_envelope_handler(req: ProvisionRequest) -> ProvisionIngressResult {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentTrustStore::new();
    match accept_envelope_with_caller(caller, &store, &deployment_store, req, crate::ic_time_ns()) {
        Ok(v) => ProvisionIngressResult::Ok(v),
        Err(e) => ProvisionIngressResult::Err(e),
    }
}

/// Authorize `query_job` from the IC runtime and map all errors to `None`.
///
/// The wire surface returns `opt ProvisionJobView` per `provision.did`; callers do not
/// distinguish NotAuthorized from NotFound. The auth check still runs inside
/// `query_job_with_caller` before the mapping.
pub fn query_job_handler(request_id: String, deployment_id: String) -> Option<ProvisionJobView> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentTrustStore::new();
    query_job_with_caller(caller, &store, &deployment_store, request_id, deployment_id).ok()
}

/// Authorize `router_ack` from the IC runtime and forward to the handler.
pub fn router_ack_handler(ack: RouterProvisionAck) -> RouterAckResult {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentTrustStore::new();
    match router_ack_with_caller(caller, &store, &deployment_store, ack, crate::ic_time_ns()) {
        Ok(v) => RouterAckResult::Ok(v),
        Err(e) => RouterAckResult::Err(e),
    }
}

/// Authorize `admin_install_deployment_binding` from the IC runtime and forward to the handler.
pub fn admin_install_deployment_binding_handler(
    args: AdminInstallDeploymentBindingArgs,
) -> Result<crate::types::BootstrapAuthEntry, crate::types::ProvisionAdminError> {
    let caller = ic_cdk::api::msg_caller();
    admin_install_deployment_binding_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `artifact_publish_metadata` from the IC runtime and forward to the handler.
#[allow(clippy::result_large_err)]
pub fn artifact_publish_metadata_handler(
    args: ArtifactPublishMetadataArgs,
) -> Result<ArtifactMetadata, ArtifactError> {
    let caller = ic_cdk::api::msg_caller();
    artifact_publish_metadata_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `artifact_upload_chunk` from the IC runtime and forward to the handler.
#[allow(clippy::result_large_err)]
pub fn artifact_upload_chunk_handler(
    args: ArtifactUploadChunkArgs,
) -> Result<ArtifactUpload, ArtifactError> {
    let caller = ic_cdk::api::msg_caller();
    artifact_upload_chunk_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `artifact_get_status` from the IC runtime and forward to the handler.
pub fn artifact_get_status_handler(artifact_id: ArtifactId) -> Option<ArtifactUpload> {
    artifact_get_status(artifact_id)
}

/// Authorize `release_publish` from the IC runtime and forward to the handler.
#[allow(clippy::result_large_err)]
pub fn release_publish_handler(args: ReleasePublishArgs) -> Result<ReleaseManifest, ReleaseError> {
    let caller = ic_cdk::api::msg_caller();
    release_publish_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `release_activate` from the IC runtime and forward to the handler.
#[allow(clippy::result_large_err)]
pub fn release_activate_handler(
    args: ReleaseActivateArgs,
) -> Result<ReleaseActivateResult, ReleaseError> {
    let caller = ic_cdk::api::msg_caller();
    release_activate_with_caller(caller, args, crate::ic_time_ns())
}

/// Return the currently active release, if any. Any caller.
pub fn release_get_active_handler() -> Option<ReleaseActivateResult> {
    release_get_active()
}
