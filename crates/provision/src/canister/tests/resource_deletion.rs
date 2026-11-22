//! ADR 0037 §Resource deletion: `delete_resource` stops and deletes one add-on canister that
//! this deployment's issuance job installed, records its progress before the first management
//! call, and leaves the canister out of every deployment scan once deleted.

use super::*;
use crate::canister::{
    delete_resource_with_caller, deleted_canisters_for_test, reset_management_fakes_for_test,
    set_canister_absent_for_test,
};
use crate::types::{
    ResourceDeletionError, ResourceDeletionRequest, ResourceDeletionResponse, ResourceDeletionState,
};

const TEXT: LogicalResource = LogicalResource::TextIndex(TextIndexId::new(0));

/// Issue one dictionary-free text canister through the real admission path and return the
/// stores with the canister Provision recorded for it (`RouterRegistrationPending`).
fn issued_text_canister() -> (DeploymentGrantStore, ProvisionJobStore, Principal) {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    reset_management_fakes_for_test();
    seed_active_release();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(vec![test_resource(TEXT)], vec![text_install_args(Some(1))]);
    let accepted = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .expect("text canister issued");
    let ProvisionAcceptResponse::Accepted {
        created_resources, ..
    } = accepted
    else {
        panic!("fresh admission must report Accepted");
    };
    assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
    (deployment_store, store, created_resources[0].canister_id)
}

fn intent() -> ProvisioningIntentKey {
    ProvisioningIntentKey {
        deployment_id: deployment_issuer().into(),
        logical_resource: TEXT,
    }
}

fn request(canister_id: Principal) -> ResourceDeletionRequest {
    ResourceDeletionRequest {
        deployment_id: deployment_issuer().into(),
        logical_resource: TEXT,
        canister_id,
    }
}

fn delete(
    deployment_store: &DeploymentGrantStore,
    store: &ProvisionJobStore,
    caller: Principal,
    req: ResourceDeletionRequest,
) -> Result<ResourceDeletionResponse, ResourceDeletionError> {
    block_on(delete_resource_with_caller(
        caller,
        store,
        deployment_store,
        req,
        900,
    ))
}

fn listed(store: &ProvisionJobStore, canister: Principal) -> bool {
    store
        .deployment_canister_records(deployment_issuer().into())
        .iter()
        .any(|record| record.canister == canister)
}

#[test]
fn deletes_the_issued_canister_and_releases_its_intent() {
    let (deployment_store, store, canister) = issued_text_canister();
    assert!(store.job_for_intent(&intent()).is_some());
    assert!(store.intent_locked(&intent()));
    assert!(listed(&store, canister));

    let response = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    );

    assert_eq!(response, Ok(ResourceDeletionResponse::Deleted));
    assert_eq!(deleted_canisters_for_test(), vec![canister]);
    assert_eq!(
        store
            .resource_deletion(&canister)
            .map(|record| record.state),
        Some(ResourceDeletionState::Deleted { deleted_at_ns: 900 })
    );
    assert!(store.job_for_intent(&intent()).is_none());
    assert!(!store.intent_locked(&intent()));
    assert!(!listed(&store, canister));
    assert!(
        !store
            .deployment_canisters(deployment_issuer().into())
            .contains(&canister)
    );
    // The issuance job itself is untouched: its record is the receipt of what was issued.
    assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
}

#[test]
fn a_replay_after_deletion_answers_from_the_record_without_another_management_call() {
    let (deployment_store, store, canister) = issued_text_canister();
    delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    )
    .expect("first deletion");

    let replay = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    );

    assert_eq!(replay, Ok(ResourceDeletionResponse::Replay));
    assert_eq!(deleted_canisters_for_test(), vec![canister]);
}

#[test]
fn a_failed_management_call_keeps_the_deletion_pending_and_a_retry_resumes_it() {
    let (deployment_store, store, canister) = issued_text_canister();
    set_force_cleanup_error_for_test(true);

    let failed = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    );

    assert!(matches!(
        failed,
        Err(ResourceDeletionError::ManagementCallFailed { .. })
    ));
    assert_eq!(
        store
            .resource_deletion(&canister)
            .map(|record| record.state),
        Some(ResourceDeletionState::DeletePending)
    );
    // Until the canister is gone, the deployment still owns it and the intent stays claimed.
    assert!(store.job_for_intent(&intent()).is_some());
    assert!(store.intent_locked(&intent()));
    assert!(listed(&store, canister));

    set_force_cleanup_error_for_test(false);
    reopen_provisioning_regions_for_test();
    let resumed = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    );

    assert_eq!(resumed, Ok(ResourceDeletionResponse::Deleted));
    assert_eq!(deleted_canisters_for_test(), vec![canister]);
    assert!(store.job_for_intent(&intent()).is_none());
    assert!(!listed(&store, canister));
}

#[test]
fn an_already_absent_canister_is_recorded_deleted_without_a_delete_call() {
    let (deployment_store, store, canister) = issued_text_canister();
    set_canister_absent_for_test(canister);

    let response = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    );

    assert_eq!(response, Ok(ResourceDeletionResponse::Deleted));
    assert!(deleted_canisters_for_test().is_empty());
    assert!(!listed(&store, canister));
}

/// Every refusal happens before the deletion record and before any management call, and
/// leaves the issued canister owned by its intent.
fn assert_refused_without_effect(
    store: &ProvisionJobStore,
    canister: Principal,
    result: Result<ResourceDeletionResponse, ResourceDeletionError>,
    expected: ResourceDeletionError,
) {
    assert_eq!(result, Err(expected));
    assert!(store.resource_deletion(&canister).is_none());
    assert!(deleted_canisters_for_test().is_empty());
    assert!(store.job_for_intent(&intent()).is_some());
    assert!(store.intent_locked(&intent()));
    assert!(listed(store, canister));
}

#[test]
fn an_ungranted_caller_or_another_deployment_is_refused() {
    let (deployment_store, store, canister) = issued_text_canister();

    let ungranted = delete(
        &deployment_store,
        &store,
        other_principal(),
        request(canister),
    );
    assert_refused_without_effect(
        &store,
        canister,
        ungranted,
        ResourceDeletionError::NotAuthorized,
    );

    deployment_store.insert(other_principal());
    let foreign = delete(
        &deployment_store,
        &store,
        other_principal(),
        request(canister),
    );
    assert_refused_without_effect(
        &store,
        canister,
        foreign,
        ResourceDeletionError::NotAuthorized,
    );
}

#[test]
fn a_graph_shard_or_the_router_is_not_deleted_on_its_own() {
    let (deployment_store, store, canister) = issued_text_canister();
    for logical_resource in [
        LogicalResource::GraphShard(ShardId::new(0)),
        LogicalResource::Router,
    ] {
        let result = delete(
            &deployment_store,
            &store,
            router_principal(),
            ResourceDeletionRequest {
                logical_resource,
                ..request(canister)
            },
        );
        assert_refused_without_effect(
            &store,
            canister,
            result,
            ResourceDeletionError::UnsupportedResource,
        );
    }
}

#[test]
fn an_unissued_intent_or_a_canister_the_job_did_not_record_is_refused() {
    let (deployment_store, store, canister) = issued_text_canister();

    let unissued = delete(
        &deployment_store,
        &store,
        router_principal(),
        ResourceDeletionRequest {
            logical_resource: LogicalResource::TextIndex(TextIndexId::new(7)),
            ..request(canister)
        },
    );
    assert_refused_without_effect(&store, canister, unissued, ResourceDeletionError::NotFound);

    let stranger = pid(77);
    let wrong_canister = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(stranger),
    );
    assert_refused_without_effect(
        &store,
        canister,
        wrong_canister,
        ResourceDeletionError::Conflict,
    );
    assert!(store.resource_deletion(&stranger).is_none());
}

#[test]
fn a_recorded_deletion_is_not_reused_for_another_resource() {
    let (deployment_store, store, canister) = issued_text_canister();
    set_force_cleanup_error_for_test(true);
    delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    )
    .expect_err("forced management failure");
    set_force_cleanup_error_for_test(false);

    let other_resource = delete(
        &deployment_store,
        &store,
        router_principal(),
        ResourceDeletionRequest {
            logical_resource: LogicalResource::VectorIndex(
                gleaph_graph_kernel::federation::VectorIndexId::new(0),
            ),
            ..request(canister)
        },
    );

    assert_eq!(other_resource, Err(ResourceDeletionError::Conflict));
    assert!(deleted_canisters_for_test().is_empty());
    assert_eq!(
        store
            .resource_deletion(&canister)
            .map(|record| record.state),
        Some(ResourceDeletionState::DeletePending)
    );
}

#[test]
fn a_job_that_has_not_installed_its_canister_is_refused() {
    let (deployment_store, store, canister) = issued_text_canister();
    let key =
        ProvisionJobRequestKey::new(&test_request_id("relay-req"), test_deployment_id(&dep_id()));
    let mut record = store.get_by_request_key(&key).expect("job");
    record.current_state = JobState::InstallPending;
    store.put(&key, record);

    let result = delete(
        &deployment_store,
        &store,
        router_principal(),
        request(canister),
    );

    assert_refused_without_effect(
        &store,
        canister,
        result,
        ResourceDeletionError::InvalidState,
    );
}
