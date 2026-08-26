//! PocketIC coverage for ADR 0035 Slice 4: Provision callable canister endpoints.
//!
//! Two fixture-family tests: one fresh canister covers the install/auth/idempotency
//! scenarios; one fresh canister covers upgrade durability. The seven named scenario
//! contracts from Plan 0057 are preserved as assertion labels.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::canister::init::ProvisionInitArgs;
use gleaph_provision::canister::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionIngressResult, ProvisionJobView,
};
use gleaph_provision::types::ProvisionRequest;

fn router_principal() -> Principal {
    Principal::from_slice(&[0x10; 29])
}

fn governance_principal() -> Principal {
    Principal::from_slice(&[0x64; 29])
}

fn other_principal() -> Principal {
    Principal::from_slice(&[0x20; 29])
}

/// Under the grant model the deployment is the issuer itself: the granted Router requests
/// issuance with `deployment_id = router_principal` text.
fn deployment_id() -> String {
    router_principal().to_text()
}

fn test_request(_request_id: &str, shard: u32) -> ProvisionRequest {
    use gleaph_graph_kernel::provisioning::ProvisioningIntentKey;
    let logical_resource = LogicalResource::GraphShard(ShardId::new(shard));
    let graph_name = "g1".to_owned();
    let requested_resources = vec![ProvisionableResource { logical_resource }];
    let request_id = gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
        &graph_name,
        &requested_resources,
    );
    ProvisionRequest {
        deployment_id: deployment_id(),
        request_id,
        intent_key: ProvisioningIntentKey::new(&deployment_id(), logical_resource),
        reserved_graph_id: None,
        graph_name,
        requested_resources,
        install_args: vec![vec![0u8; 0]],
        authorized_caller: Principal::from_slice(&[0x30; 29]),
        release_id: "rel1".to_owned(),
    }
}

fn expected_request_id(shard: u32) -> [u8; 32] {
    let logical_resource = LogicalResource::GraphShard(ShardId::new(shard));
    gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
        "g1",
        &[ProvisionableResource { logical_resource }],
    )
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
    request_id: [u8; 32],
    deployment_id: &str,
) -> Option<ProvisionJobView> {
    let bytes = pic
        .query_call(
            canister,
            caller,
            "query_job",
            Encode!(&request_id, &deployment_id.to_owned()).expect("encode query_job"),
        )
        .expect("query_job call");
    Decode!(&bytes, Option<ProvisionJobView>).expect("decode query_job result")
}

/// Fixture family 1: fresh canister covering scenarios 1-6.
#[test]
fn provision_callable_endpoints_install_auth_and_idempotency() {
    let pic = new_pocket_ic();
    let provision = install_provision_canister(&pic, governance_principal(), &[router_principal()]);

    // Scenario 1: install with one bootstrap binding.
    // (install_provision_canister already asserts the install succeeds.)

    // Scenario 2: wrong principal accept_envelope -> NotAuthorized.
    let wrong_accept_req = test_request("r-wrong-accept", 7);
    let wrong_accept = accept_envelope(&pic, provision, other_principal(), &wrong_accept_req);
    assert!(
        matches!(
            wrong_accept,
            ProvisionIngressResult::Err(ProvisionIngressError::NotAuthorized)
        ),
        "scenario 2: wrong principal accept must be NotAuthorized, got {wrong_accept:?}"
    );

    // Scenario 3: Router accept_envelope admits a fresh request.
    let fresh_req = test_request("r1", 1);
    let fresh = accept_envelope(&pic, provision, router_principal(), &fresh_req);
    match fresh {
        ProvisionIngressResult::Ok(ProvisionAcceptResponse::Accepted {
            job_view,
            intent_lock_count,
            created_resources,
        }) => {
            assert_eq!(job_view.deployment_id, deployment_id(), "scenario 3 deployment_id");
            assert_eq!(
                job_view.request_id,
                expected_request_id(1),
                "scenario 3 request_id"
            );
            assert_eq!(job_view.state, "Reserved", "scenario 3 state");
            assert_eq!(intent_lock_count, 1, "scenario 3 intent_lock_count");
            assert!(
                created_resources.is_empty(),
                "scenario 3: no release seeded -> no deploy"
            );
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
    let wrong_query = query_job(
        &pic,
        provision,
        other_principal(),
        expected_request_id(1),
        &deployment_id(),    );
    assert!(
        wrong_query.is_none(),
        "scenario 5: wrong principal query must map to None"
    );

    // Scenario 6: Router query_job returns Some(view).
    let view = query_job(
        &pic,
        provision,
        router_principal(),
        expected_request_id(1),
        &deployment_id(),    );
    assert!(
        view.is_some(),
        "scenario 6: router query must return Some(view)"
    );
    let view = view.unwrap();
    assert_eq!(
        view.request_id,
        expected_request_id(1),
        "scenario 6 request_id"
    );
    assert_eq!(view.deployment_id, deployment_id(), "scenario 6 deployment_id");
    assert_eq!(view.state_name, "Reserved", "scenario 6 state_name");
}

/// Fixture family 2: fresh canister covering scenario 10 (upgrade durability).
#[test]
fn provision_callable_endpoints_upgrade_durability() {
    let pic = new_pocket_ic();
    let provision = install_provision_canister(&pic, governance_principal(), &[router_principal()]);

    // Pre-upgrade admission.
    let pre_req = test_request("r-pre", 5);
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
            governance_principal: governance_principal(),
        })
        .expect("encode provision upgrade args"),
        None,
    )
    .expect("scenario 10: upgrade provision canister");

    // Post-upgrade admission with a distinct intent so it is not blocked by the pre-upgrade lock.
    let post_req = test_request("r-post", 6);
    let after = accept_envelope(&pic, provision, router_principal(), &post_req);
    assert!(
        matches!(
            after,
            ProvisionIngressResult::Ok(ProvisionAcceptResponse::Accepted { .. })
        ),
        "scenario 10: post-upgrade admission must succeed using durable binding, got {after:?}"
    );
}
