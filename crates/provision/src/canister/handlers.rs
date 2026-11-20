//! Provision canister IC-runtime entry-point shims (ADR 0035 Slice 4 + Slice 7).
//!
//! This is the only module that reads `ic_cdk::api::msg_caller()`. The thin
//! wrappers forward to the existing `*_with_caller(...)` functions, which remain
//! `pub(crate)` so the 33+ unit tests and the candid export tests can drive every branch without WASM.

use candid::Principal;
use gleaph_graph_kernel::provisioning::DeploymentId;

use crate::canister::{
    ArtifactUpload, ProvisionIngressResult, ProvisionJobView, RouterRegistrationAckResult,
    accept_envelope_with_caller_and_cycles, admin_finalize_dict_catalog_with_caller,
    admin_get_dict_catalog_status, admin_upload_dict_catalog_chunk_with_caller,
    artifact_audit_history_with_caller, artifact_get_status, artifact_publish_metadata_with_caller,
    artifact_upload_chunk_with_caller, billing_grant_status_with_caller,
    billing_grants_with_caller, cancel_deployment_termination_with_caller,
    complete_graph_registration_with_caller, delete_deployment_canisters_with_caller,
    deployment_canisters_with_caller, deployment_job_with_caller, deployment_jobs_with_caller,
    deployment_termination_with_caller, deployment_with_caller,
    expire_deployment_suspensions_with_caller, grant_credit_with_caller,
    initialize_deployment_with_caller, query_job_with_caller, quote_admission,
    release_activate_with_caller, release_get_active, release_install_with_caller,
    release_publish_with_caller, repair_failed_deployment_canisters_with_caller,
    request_deployment_termination_with_caller, reverse_credit_with_caller,
    service_ledger_with_caller, set_deployment_owner_with_caller, take_ownership_with_caller,
    transfer_self_custody_with_caller, upsert_deployment_grant_with_caller, withdraw_with_caller,
    withdrawal_with_caller,
};
use crate::stable::store::{DeploymentGrantStore, ProvisionJobStore};
use crate::types::{
    AdmissionQuote, AdmissionQuoteError, ArtifactAuditEntry, ArtifactError, ArtifactId,
    ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs,
    DeploymentOwnerUpdateError, DeploymentQueryError, DeploymentRecord, GrantCreditArgs,
    GrantCreditError, GrantCreditResult, InitializeDeploymentArgs, InitializeDeploymentError,
    InstallError, ProvisionRequest, ReleaseActivateArgs, ReleaseActivateResult, ReleaseError,
    ReleaseInstallArgs, ReleaseInstallResult, ReleaseManifest, ReleasePublishArgs,
    ReverseCreditArgs, ReverseCreditError, ReverseCreditResult, RouterRegistrationAck,
    SelfCustodyTransferError, TakeOwnershipError, UpsertDeploymentGrantArgs, WithdrawArgs,
    WithdrawError, WithdrawResult, WithdrawalRecord,
};

/// Bootstrap the deployment trust store from init args.
pub fn init_handler(args: crate::canister::init::ProvisionInitArgs) {
    crate::canister::init::init(args);
}

/// No-op: all stable stores survive upgrades via stable memory already.
pub fn post_upgrade_handler() {}

/// Authorize and pre-register a deployment before its first job.
pub fn initialize_deployment_handler(
    args: InitializeDeploymentArgs,
) -> Result<DeploymentRecord, InitializeDeploymentError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentGrantStore::new();
    initialize_deployment_with_caller(caller, &store, &deployment_store, args)
}

/// Authorize `accept_envelope` from the IC runtime and forward to the handler.
pub async fn accept_envelope_handler(req: ProvisionRequest) -> ProvisionIngressResult {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentGrantStore::new();
    let deployment_id = req.deployment_id;
    let attached_cycles = ic_cdk::api::msg_cycles_available();
    let result = accept_envelope_with_caller_and_cycles(
        caller,
        &store,
        &deployment_store,
        req,
        crate::ic_time_ns(),
        attached_cycles,
        true,
    )
    .await;
    match result {
        Ok(v) => {
            // The ledger credits `attached_cycles`; accept exactly that amount only after the
            // job/credit transaction succeeded. Unaccepted cycles are refunded by the IC on an
            // error return, so the error path deliberately does not call accept.
            if attached_cycles > 0 {
                let accepted = ic_cdk::api::msg_cycles_accept(attached_cycles);
                if accepted < attached_cycles {
                    drop(
                        store.debit_deployment(
                            deployment_id,
                            attached_cycles.saturating_sub(accepted),
                        ),
                    );
                }
            }
            ProvisionIngressResult::Ok(v)
        }
        Err(e) => ProvisionIngressResult::Err(e),
    }
}

/// Authorize an explicit Managed -> SelfCustody handover.
pub async fn take_ownership_handler(
    deployment_id: DeploymentId,
) -> Result<DeploymentRecord, TakeOwnershipError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    take_ownership_with_caller(caller, &store, deployment_id).await
}

/// Governance-only repair of a Managed deployment's owner.
pub fn set_deployment_owner_handler(
    deployment_id: DeploymentId,
    new_owner: candid::Principal,
) -> Result<DeploymentRecord, DeploymentOwnerUpdateError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    set_deployment_owner_with_caller(caller, &store, deployment_id, new_owner)
}

/// Authorize a governance-only SelfCustody owner transfer and forward to the controller handover.
pub async fn transfer_self_custody_handler(
    deployment_id: DeploymentId,
    new_owner: Principal,
) -> Result<DeploymentRecord, SelfCustodyTransferError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    transfer_self_custody_with_caller(caller, &store, deployment_id, new_owner).await
}

/// Authorize a billing credit grant from the configured billing authority.
pub fn grant_credit_handler(args: GrantCreditArgs) -> Result<GrantCreditResult, GrantCreditError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    grant_credit_with_caller(caller, &store, args, crate::ic_time_ns())
}

/// Authorize `reverse_credit` from the IC runtime and forward to the handler.
pub fn reverse_credit_handler(
    args: ReverseCreditArgs,
) -> Result<ReverseCreditResult, ReverseCreditError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    reverse_credit_with_caller(caller, &store, args, crate::ic_time_ns())
}

/// Authorize a deployment read from the IC runtime.
pub fn deployment_handler(
    deployment_id: DeploymentId,
) -> Result<DeploymentRecord, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    deployment_with_caller(caller, &store, deployment_id)
}

/// Recorded canisters for one deployment (read-only reconciliation input).
pub fn deployment_canisters_handler(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::DeploymentCanister>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    deployment_canisters_with_caller(caller, &store, deployment_id)
}

/// All jobs for one deployment (read-only).
pub fn deployment_jobs_handler(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::DeploymentJobSummary>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    deployment_jobs_with_caller(caller, &store, deployment_id)
}

/// Operator/governance repair of failed-job canisters.
pub async fn repair_failed_deployment_canisters_handler(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::FailedCanisterRepair>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    repair_failed_deployment_canisters_with_caller(caller, &store, deployment_id).await
}

/// One job detail for the deployment read authority.
pub fn deployment_job_handler(
    deployment_id: DeploymentId,
    request_id: [u8; 32],
) -> Result<Option<crate::types::ProvisionJobView>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    deployment_job_with_caller(caller, &store, deployment_id, request_id)
}

/// Billing grants recorded for one deployment.
pub fn billing_grants_handler(
    deployment_id: DeploymentId,
) -> Result<Vec<crate::types::BillingGrantView>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    billing_grants_with_caller(caller, &store, deployment_id)
}

/// One billing grant by authority and idempotency key.
pub fn billing_grant_status_handler(
    authority: Principal,
    idempotency_key: [u8; 32],
) -> Result<Option<crate::types::BillingGrantView>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    billing_grant_status_with_caller(caller, &store, authority, idempotency_key)
}

/// Request a reversible deployment suspension.
pub async fn request_deployment_termination_handler(
    deployment_id: DeploymentId,
    reason: String,
) -> Result<crate::types::DeploymentTermination, crate::types::DeploymentTerminationError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    request_deployment_termination_with_caller(
        caller,
        &store,
        deployment_id,
        reason,
        crate::ic_time_ns(),
    )
    .await
}

/// Cancel a suspension and start the recorded canisters again.
pub async fn cancel_deployment_termination_handler(
    deployment_id: DeploymentId,
) -> Result<crate::types::DeploymentTermination, crate::types::DeploymentTerminationError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    cancel_deployment_termination_with_caller(caller, &store, deployment_id, crate::ic_time_ns())
        .await
}

/// Mark grace-expired suspensions (operator or governance); no deletion.
pub fn expire_deployment_suspensions_handler(
    limit: u32,
) -> Result<Vec<crate::types::DeploymentTermination>, crate::types::DeploymentTerminationError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    expire_deployment_suspensions_with_caller(caller, &store, crate::ic_time_ns(), limit)
}

/// Delete a suspended deployment after grace and credit settlement.
pub async fn delete_deployment_canisters_handler(
    deployment_id: DeploymentId,
    confirmation: String,
    force: bool,
) -> Result<crate::types::DeploymentTermination, crate::types::DeploymentTerminationError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    delete_deployment_canisters_with_caller(
        caller,
        &store,
        deployment_id,
        confirmation,
        force,
        crate::ic_time_ns(),
    )
    .await
}

/// Read the suspension record under the deployment read authority.
pub fn deployment_termination_handler(
    deployment_id: DeploymentId,
) -> Result<Option<crate::types::DeploymentTermination>, DeploymentQueryError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    deployment_termination_with_caller(caller, &store, deployment_id)
}

/// Operator/governance read of the current withheld pools.
pub fn service_ledger_handler() -> Result<crate::types::ServiceLedger, WithdrawError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    service_ledger_with_caller(caller, &store)
}

/// Operator read of one withdrawal record, keyed by the caller and idempotency key.
pub fn withdrawal_handler(
    idempotency_key: [u8; 32],
) -> Result<Option<WithdrawalRecord>, WithdrawError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    withdrawal_with_caller(caller, &store, idempotency_key)
}

/// Authorize an operator withdrawal from one withheld ledger pool and transfer the cycles.
pub async fn withdraw_handler(args: WithdrawArgs) -> Result<WithdrawResult, WithdrawError> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    withdraw_with_caller(caller, &store, args).await
}

/// Public price quote; it exposes only the configured fee policy, not deployment state.
pub fn quote_admission_handler(resource_count: u32) -> Result<AdmissionQuote, AdmissionQuoteError> {
    quote_admission(resource_count)
}

/// Authorize `query_job` from the IC runtime and map all errors to `None`.
///
/// The wire surface returns `opt ProvisionJobView` per `provision.did`; callers do not
/// distinguish NotAuthorized from NotFound. The auth check still runs inside
/// `query_job_with_caller` before the mapping.
pub fn query_job_handler(
    request_id: [u8; 32],
    deployment_id: DeploymentId,
) -> Option<ProvisionJobView> {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentGrantStore::new();
    query_job_with_caller(caller, &store, &deployment_store, request_id, deployment_id).ok()
}

/// Authorize Router registration completion from the IC runtime and forward to the handler.
pub fn complete_graph_registration_handler(
    ack: RouterRegistrationAck,
) -> RouterRegistrationAckResult {
    let caller = ic_cdk::api::msg_caller();
    let store = ProvisionJobStore::new();
    let deployment_store = DeploymentGrantStore::new();
    match complete_graph_registration_with_caller(
        caller,
        &store,
        &deployment_store,
        ack,
        crate::ic_time_ns(),
    ) {
        Ok(v) => RouterRegistrationAckResult::Ok(v),
        Err(e) => RouterRegistrationAckResult::Err(e),
    }
}

/// Authorize `upsert_deployment_grant` from the IC runtime and forward to the handler.
pub fn upsert_deployment_grant_handler(
    args: UpsertDeploymentGrantArgs,
) -> Result<crate::types::BootstrapAuthEntry, crate::types::ProvisionAdminError> {
    let caller = ic_cdk::api::msg_caller();
    upsert_deployment_grant_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `artifact_publish_metadata` from the IC runtime and forward to the handler.
pub fn artifact_publish_metadata_handler(
    args: ArtifactPublishMetadataArgs,
) -> Result<ArtifactMetadata, ArtifactError> {
    let caller = ic_cdk::api::msg_caller();
    artifact_publish_metadata_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `artifact_upload_chunk` from the IC runtime and forward to the handler.
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
pub fn release_publish_handler(args: ReleasePublishArgs) -> Result<ReleaseManifest, ReleaseError> {
    let caller = ic_cdk::api::msg_caller();
    release_publish_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `release_activate` from the IC runtime and forward to the handler.
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

/// Authorize `artifact_audit_history` from the IC runtime and forward to the handler.
pub fn artifact_audit_history_handler() -> Result<Vec<ArtifactAuditEntry>, ArtifactError> {
    let caller = ic_cdk::api::msg_caller();
    artifact_audit_history_with_caller(caller)
}

/// Authorize `release_install` from the IC runtime and forward to the handler.
pub async fn release_install_handler(
    args: ReleaseInstallArgs,
) -> Result<ReleaseInstallResult, InstallError> {
    let caller = ic_cdk::api::msg_caller();
    release_install_with_caller(caller, args, crate::ic_time_ns()).await
}

/// Authorize `admin_upload_dict_catalog_chunk` from the IC runtime and forward to the handler.
pub fn admin_upload_dict_catalog_chunk_handler(
    args: crate::types::DictCatalogUploadChunkArgs,
) -> Result<crate::types::DictCatalogStatus, crate::types::DictCatalogError> {
    let caller = ic_cdk::api::msg_caller();
    admin_upload_dict_catalog_chunk_with_caller(caller, args, crate::ic_time_ns())
}

/// Authorize `admin_finalize_dict_catalog` from the IC runtime and forward to the handler.
pub fn admin_finalize_dict_catalog_handler(
    args: crate::types::DictCatalogFinalizeArgs,
) -> Result<crate::types::DictCatalogStatus, crate::types::DictCatalogError> {
    let caller = ic_cdk::api::msg_caller();
    admin_finalize_dict_catalog_with_caller(caller, args, crate::ic_time_ns())
}

/// Forward `admin_get_dict_catalog_status` (read-only, any caller) to the handler.
pub fn admin_get_dict_catalog_status_handler(
    key: crate::types::DictCatalogKey,
) -> Option<crate::types::DictCatalogStatus> {
    admin_get_dict_catalog_status(key)
}
