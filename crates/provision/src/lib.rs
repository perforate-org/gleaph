//! Gleaph Provision canister — ADR 0035.
//!
//! Owns durable job/receipt state and the deployment trust binding.
//! Does not own graph topology, tenancy, or routing catalogs.

#![cfg_attr(not(test), allow(dead_code))]

pub mod stable;
pub mod types;

pub mod canister;

#[cfg(test)]
mod candid;

use crate::canister::{ProvisionIngressResult, ProvisionJobView, RouterAckResult, handlers};
use crate::types::AdminInstallError;
use crate::types::{
    AdminInstallDeploymentBindingArgs, ArtifactError, ArtifactId, ArtifactMetadata,
    ArtifactPublishMetadataArgs, ArtifactUpload, ArtifactUploadChunkArgs, BootstrapAuthEntry,
    ProvisionRequest, RouterProvisionAck,
};
use ic_cdk_macros::{init, post_upgrade, query, update};

#[init]
fn init(args: crate::canister::init::ProvisionInitArgs) {
    handlers::init_handler(args);
}

#[post_upgrade]
fn post_upgrade() {
    handlers::post_upgrade_handler();
}

#[update]
fn accept_envelope(req: ProvisionRequest) -> ProvisionIngressResult {
    handlers::accept_envelope_handler(req)
}

#[query]
fn query_job(request_id: String, deployment_id: String) -> Option<ProvisionJobView> {
    handlers::query_job_handler(request_id, deployment_id)
}

#[update]
fn router_ack(ack: RouterProvisionAck) -> RouterAckResult {
    handlers::router_ack_handler(ack)
}

#[update]
fn admin_install_deployment_binding(
    args: AdminInstallDeploymentBindingArgs,
) -> Result<BootstrapAuthEntry, AdminInstallError> {
    handlers::admin_install_deployment_binding_handler(args)
}

#[allow(clippy::result_large_err)]
#[update]
fn artifact_publish_metadata(
    args: ArtifactPublishMetadataArgs,
) -> Result<ArtifactMetadata, ArtifactError> {
    handlers::artifact_publish_metadata_handler(args)
}

#[allow(clippy::result_large_err)]
#[update]
fn artifact_upload_chunk(args: ArtifactUploadChunkArgs) -> Result<ArtifactUpload, ArtifactError> {
    handlers::artifact_upload_chunk_handler(args)
}

#[query]
fn artifact_get_status(artifact_id: ArtifactId) -> Option<ArtifactUpload> {
    handlers::artifact_get_status_handler(artifact_id)
}

#[cfg(test)]
pub fn export_service_string() -> String {
    __export_service()
}

ic_cdk::export_candid!();

/// IC NNS timestamp in nanoseconds.
///
/// Mirrors `crates/router/src/facade/store.rs:121-128`: returns `ic_cdk::api::time()` on
/// `wasm`, `0` on `not(target_family = "wasm")`. Used by the `handlers` module for
/// `accept_envelope` and `router_ack` transition timestamps; also used by unit tests that drive
/// `*_with_caller` directly.
#[allow(dead_code)]
pub(crate) fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}
