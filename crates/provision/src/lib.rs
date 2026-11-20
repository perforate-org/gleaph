//! Gleaph Provision canister — ADR 0035.
//!
//! Owns durable job/receipt state and the deployment trust binding.
//! Does not own graph topology, tenancy, or routing catalogs.

#![cfg_attr(not(test), allow(dead_code))]

use candid::Principal;
use gleaph_graph_kernel::provisioning::DeploymentId;

pub mod stable;
pub mod types;

pub mod canister;

#[cfg(test)]
#[path = "candid.rs"]
mod candid_tests;

use crate::canister::{
    ProvisionIngressResult, ProvisionJobView, RouterRegistrationAckResult, handlers,
};
use crate::types::UpsertDeploymentGrantError;
use crate::types::{
    AdmissionQuote, AdmissionQuoteError, ArtifactAuditEntry, ArtifactError, ArtifactId,
    ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload, ArtifactUploadChunkArgs,
    BootstrapAuthEntry, DeploymentOwnerUpdateError, DeploymentQueryError, DeploymentRecord,
    DictCatalogError, DictCatalogFinalizeArgs, DictCatalogStatus, DictCatalogUploadChunkArgs,
    GrantCreditArgs, GrantCreditError, GrantCreditResult, InitializeDeploymentArgs,
    InitializeDeploymentError, InstallError, ProvisionRequest, ReleaseActivateArgs,
    ReleaseActivateResult, ReleaseError, ReleaseInstallArgs, ReleaseInstallResult, ReleaseManifest,
    ReleasePublishArgs, ReverseCreditArgs, ReverseCreditError, ReverseCreditResult,
    RouterRegistrationAck, SelfCustodyTransferError, ServiceConfig, ServiceConfigUpdateError,
    TakeOwnershipError, UpsertDeploymentGrantArgs, WithdrawArgs, WithdrawError, WithdrawResult,
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
fn admin_set_service_config(
    config: ServiceConfig,
) -> Result<ServiceConfig, ServiceConfigUpdateError> {
    let caller = ic_cdk::api::msg_caller();
    crate::canister::set_service_config_with_caller(
        caller,
        &crate::stable::store::ProvisionJobStore::new(),
        config,
    )
}

#[update]
async fn admin_transfer_self_custody(
    deployment_id: DeploymentId,
    new_owner: Principal,
) -> Result<DeploymentRecord, SelfCustodyTransferError> {
    handlers::transfer_self_custody_handler(deployment_id, new_owner).await
}

#[update]
fn admin_set_deployment_owner(
    deployment_id: DeploymentId,
    new_owner: Principal,
) -> Result<DeploymentRecord, DeploymentOwnerUpdateError> {
    handlers::set_deployment_owner_handler(deployment_id, new_owner)
}

#[update]
fn initialize_deployment(
    args: InitializeDeploymentArgs,
) -> Result<DeploymentRecord, InitializeDeploymentError> {
    handlers::initialize_deployment_handler(args)
}

#[update]
async fn accept_envelope(req: ProvisionRequest) -> ProvisionIngressResult {
    handlers::accept_envelope_handler(req).await
}

#[update]
async fn take_ownership(
    deployment_id: DeploymentId,
) -> Result<DeploymentRecord, TakeOwnershipError> {
    handlers::take_ownership_handler(deployment_id).await
}

#[update]
fn grant_credit(args: GrantCreditArgs) -> Result<GrantCreditResult, GrantCreditError> {
    handlers::grant_credit_handler(args)
}

#[update]
fn reverse_credit(args: ReverseCreditArgs) -> Result<ReverseCreditResult, ReverseCreditError> {
    handlers::reverse_credit_handler(args)
}

#[update]
async fn withdraw_fees(args: WithdrawArgs) -> Result<WithdrawResult, WithdrawError> {
    handlers::withdraw_handler(args).await
}

#[query]
fn get_deployment(deployment_id: DeploymentId) -> Result<DeploymentRecord, DeploymentQueryError> {
    handlers::deployment_handler(deployment_id)
}

#[query]
fn get_deployment_canisters(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::DeploymentCanister>, DeploymentQueryError> {
    handlers::deployment_canisters_handler(deployment_id)
}

#[update]
async fn repair_failed_deployment_canisters(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::FailedCanisterRepair>, DeploymentQueryError> {
    handlers::repair_failed_deployment_canisters_handler(deployment_id).await
}

#[query]
fn deployment_jobs(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::DeploymentJobSummary>, DeploymentQueryError> {
    handlers::deployment_jobs_handler(deployment_id)
}

#[query]
fn deployment_job(
    deployment_id: DeploymentId,
    request_id: [u8; 32],
) -> Result<Option<crate::types::ProvisionJobView>, DeploymentQueryError> {
    handlers::deployment_job_handler(deployment_id, request_id)
}

#[query]
fn billing_grants(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::BillingGrantView>, DeploymentQueryError> {
    handlers::billing_grants_handler(deployment_id)
}

#[query]
fn billing_grant_status(
    authority: Principal,
    idempotency_key: [u8; 32],
) -> Result<Option<crate::types::BillingGrantView>, DeploymentQueryError> {
    handlers::billing_grant_status_handler(authority, idempotency_key)
}

#[update]
async fn request_deployment_termination(
    deployment_id: DeploymentId,
    reason: String,
) -> Result<crate::types::DeploymentTermination, crate::types::DeploymentTerminationError> {
    handlers::request_deployment_termination_handler(deployment_id, reason).await
}

#[update]
async fn delete_deployment_canisters(
    deployment_id: DeploymentId,
    confirmation: String,
    force: bool,
) -> Result<crate::types::DeploymentTermination, crate::types::DeploymentTerminationError> {
    handlers::delete_deployment_canisters_handler(deployment_id, confirmation, force).await
}

#[update]
async fn cancel_deployment_termination(
    deployment_id: DeploymentId,
) -> Result<crate::types::DeploymentTermination, crate::types::DeploymentTerminationError> {
    handlers::cancel_deployment_termination_handler(deployment_id).await
}

#[query]
fn deployment_termination(
    deployment_id: DeploymentId,
) -> Result<Option<crate::types::DeploymentTermination>, DeploymentQueryError> {
    handlers::deployment_termination_handler(deployment_id)
}

#[query]
fn get_service_ledger() -> Result<crate::types::ServiceLedger, WithdrawError> {
    handlers::service_ledger_handler()
}

#[query]
fn get_withdrawal(
    idempotency_key: [u8; 32],
) -> Result<Option<crate::types::WithdrawalRecord>, WithdrawError> {
    handlers::withdrawal_handler(idempotency_key)
}

#[query]
fn quote_admission(resource_count: u32) -> Result<AdmissionQuote, AdmissionQuoteError> {
    handlers::quote_admission_handler(resource_count)
}

#[query]
fn query_job(request_id: [u8; 32], deployment_id: DeploymentId) -> Option<ProvisionJobView> {
    handlers::query_job_handler(request_id, deployment_id)
}

#[update]
fn complete_graph_registration(ack: RouterRegistrationAck) -> RouterRegistrationAckResult {
    handlers::complete_graph_registration_handler(ack)
}

#[update]
fn upsert_deployment_grant(
    args: UpsertDeploymentGrantArgs,
) -> Result<BootstrapAuthEntry, UpsertDeploymentGrantError> {
    handlers::upsert_deployment_grant_handler(args)
}

#[update]
fn artifact_publish_metadata(
    args: ArtifactPublishMetadataArgs,
) -> Result<ArtifactMetadata, ArtifactError> {
    handlers::artifact_publish_metadata_handler(args)
}

#[update]
fn artifact_upload_chunk(args: ArtifactUploadChunkArgs) -> Result<ArtifactUpload, ArtifactError> {
    handlers::artifact_upload_chunk_handler(args)
}

#[query]
fn artifact_get_status(artifact_id: ArtifactId) -> Option<ArtifactUpload> {
    handlers::artifact_get_status_handler(artifact_id)
}

#[update]
fn release_publish(args: ReleasePublishArgs) -> Result<ReleaseManifest, ReleaseError> {
    handlers::release_publish_handler(args)
}

#[update]
fn release_activate(args: ReleaseActivateArgs) -> Result<ReleaseActivateResult, ReleaseError> {
    handlers::release_activate_handler(args)
}

#[query]
fn release_get_active() -> Option<ReleaseActivateResult> {
    handlers::release_get_active_handler()
}

#[query]
fn artifact_audit_history() -> Result<Vec<ArtifactAuditEntry>, ArtifactError> {
    handlers::artifact_audit_history_handler()
}

#[update]
async fn release_install(args: ReleaseInstallArgs) -> Result<ReleaseInstallResult, InstallError> {
    handlers::release_install_handler(args).await
}

#[update]
fn admin_upload_dict_catalog_chunk(
    args: DictCatalogUploadChunkArgs,
) -> Result<DictCatalogStatus, DictCatalogError> {
    handlers::admin_upload_dict_catalog_chunk_handler(args)
}

#[update]
fn admin_finalize_dict_catalog(
    args: DictCatalogFinalizeArgs,
) -> Result<DictCatalogStatus, DictCatalogError> {
    handlers::admin_finalize_dict_catalog_handler(args)
}

#[query]
fn admin_get_dict_catalog_status(key: crate::types::DictCatalogKey) -> Option<DictCatalogStatus> {
    handlers::admin_get_dict_catalog_status_handler(key)
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
/// provisioning transition timestamps; also used by unit tests that drive
/// `*_with_caller` directly.
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
