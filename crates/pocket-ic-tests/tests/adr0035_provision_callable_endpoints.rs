//! PocketIC coverage for ADR 0035 Slice 4: Provision callable canister endpoints.
//!
//! Two fixture-family tests: one fresh canister covers the install/auth/idempotency
//! scenarios; one fresh canister covers upgrade durability. The seven named scenario
//! contracts from Plan 0057 are preserved as assertion labels.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::ProvisionableResourceKind;
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::canister::init::ProvisionInitArgs;
use gleaph_provision::canister::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionIngressResult, ProvisionJobView,
};
use gleaph_provision::types::{DeploymentBinding, ProvisionRequest};

fn router_principal() -> Principal {
    Principal::from_slice(&[0x10; 29])
}

fn governance_principal() -> Principal {
    Principal::from_slice(&[0x64; 29])
}

fn other_principal() -> Principal {
    Principal::from_slice(&[0x20; 29])
}

fn deployment_binding() -> DeploymentBinding {
    DeploymentBinding {
        deployment_id: "d1".to_owned(),
        router_principal: router_principal(),
        governance_principal: governance_principal(),
        binding_version: 1,
    }
}

fn test_request(request_id: &str, logical_key: &str) -> ProvisionRequest {
    use gleaph_graph_kernel::provisioning::ProvisioningIntentKey;
    ProvisionRequest {
        deployment_id: "d1".to_owned(),
        request_id: request_id.to_owned(),
        request_fingerprint: format!("fp-{request_id}"),
        intent_key: ProvisioningIntentKey::new(
            "d1",
            ProvisionableResourceKind::GraphShard,
            logical_key,
        ),
        reserved_graph_id: None,
        graph_name: "g1".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: logical_key.to_owned(),
        }],
        authorized_caller: Principal::from_slice(&[0x30; 29]),
        release_id: "rel1".to_owned(),
        router_callback_principal: Principal::from_slice(&[0x40; 29]),
    }
}

fn accept_envelope(
    pic: &pocket_ic::PocketIc,
    canister: Principal,
    caller: Principal,
    req: &ProvisionRequest,
) -> ProvisionIngressResult {
    let bytes = pic
        .update_call(
            canister,
            caller,
            "accept_envelope",
            Encode!(req).expect("encode accept_envelope"),
        )
        .expect("accept_envelope call");
    Decode!(&bytes, ProvisionIngressResult).expect("decode accept_envelope result")
}

fn query_job(
    pic: &pocket_ic::PocketIc,
    canister: Principal,
    caller: Principal,
    request_id: &str,
    deployment_id: &str,
) -> Option<ProvisionJobView> {
    let bytes = pic
        .query_call(
            canister,
            caller,
            "query_job",
            Encode!(&request_id.to_owned(), &deployment_id.to_owned()).expect("encode query_job"),
        )
        .expect("query_job call");
    Decode!(&bytes, Option<ProvisionJobView>).expect("decode query_job result")
}

/// Fixture family 1: fresh canister covering scenarios 1-6.
#[test]
fn provision_callable_endpoints_install_auth_and_idempotency() {
    let pic = new_pocket_ic();
    let provision = install_provision_canister(&pic, deployment_binding());

    // Scenario 1: install with one bootstrap binding.
    // (install_provision_canister already asserts the install succeeds.)

    // Scenario 2: wrong principal accept_envelope -> NotAuthorized.
    let wrong_accept_req = test_request("r-wrong-accept", "shard-wrong-accept");
    let wrong_accept = accept_envelope(&pic, provision, other_principal(), &wrong_accept_req);
    assert!(
        matches!(
            wrong_accept,
            ProvisionIngressResult::Err(ProvisionIngressError::NotAuthorized)
        ),
        "scenario 2: wrong principal accept must be NotAuthorized, got {wrong_accept:?}"
    );

    // Scenario 3: Router accept_envelope admits a fresh request.
    let fresh_req = test_request("r1", "shard1");
    let fresh = accept_envelope(&pic, provision, router_principal(), &fresh_req);
    match fresh {
        ProvisionIngressResult::Ok(ProvisionAcceptResponse::Accepted {
            job_view,
            intent_lock_count,
        }) => {
            assert_eq!(job_view.deployment_id, "d1", "scenario 3 deployment_id");
            assert_eq!(job_view.request_id, "r1", "scenario 3 request_id");
            assert_eq!(job_view.state, "Reserved", "scenario 3 state");
            assert_eq!(intent_lock_count, 1, "scenario 3 intent_lock_count");
        }
        other => panic!("scenario 3: expected Accepted fresh response, got {other:?}"),
    }

    // Scenario 4: idempotent replay returns Replay for same id + fingerprint.
    let replay = accept_envelope(&pic, provision, router_principal(), &fresh_req);
    assert!(
        matches!(
            replay,
            ProvisionIngressResult::Ok(ProvisionAcceptResponse::Replay { .. })
        ),
        "scenario 4: replay must be Replay, got {replay:?}"
    );

    // Scenario 5: wrong principal query_job maps to None.
    let wrong_query = query_job(&pic, provision, other_principal(), "r1", "d1");
    assert!(
        wrong_query.is_none(),
        "scenario 5: wrong principal query must map to None"
    );

    // Scenario 6: Router query_job returns Some(view).
    let view = query_job(&pic, provision, router_principal(), "r1", "d1");
    assert!(
        view.is_some(),
        "scenario 6: router query must return Some(view)"
    );
    let view = view.unwrap();
    assert_eq!(view.request_id, "r1", "scenario 6 request_id");
    assert_eq!(view.deployment_id, "d1", "scenario 6 deployment_id");
    assert_eq!(view.state_name, "Reserved", "scenario 6 state_name");
}

/// Fixture family 2: fresh canister covering scenario 10 (upgrade durability).
#[test]
fn provision_callable_endpoints_upgrade_durability() {
    let pic = new_pocket_ic();
    let provision = install_provision_canister(&pic, deployment_binding());

    // Pre-upgrade admission.
    let pre_req = test_request("r-pre", "shard-pre");
    let before = accept_envelope(&pic, provision, router_principal(), &pre_req);
    assert!(
        matches!(
            before,
            ProvisionIngressResult::Ok(ProvisionAcceptResponse::Accepted { .. })
        ),
        "scenario 10 pre-upgrade admission must succeed, got {before:?}"
    );

    // Upgrade with empty init args to prove the durable binding survived via stable memory.
    pic.upgrade_canister(
        provision,
        wasm_bytes("PROVISION_WASM"),
        Encode!(&ProvisionInitArgs {
            bootstrap_bindings: vec![],
        })
        .expect("encode provision upgrade args"),
        None,
    )
    .expect("scenario 10: upgrade provision canister");

    // Post-upgrade admission with a distinct intent so it is not blocked by the pre-upgrade lock.
    let post_req = test_request("r-post", "shard-post");
    let after = accept_envelope(&pic, provision, router_principal(), &post_req);
    assert!(
        matches!(
            after,
            ProvisionIngressResult::Ok(ProvisionAcceptResponse::Accepted { .. })
        ),
        "scenario 10: post-upgrade admission must succeed using durable binding, got {after:?}"
    );
}
