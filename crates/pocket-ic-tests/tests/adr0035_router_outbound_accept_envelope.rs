//! PocketIC E2E for ADR 0035 Slices 5 and 6: Router outbound accept_envelope send and
//! symmetric Provision -> Router `router_ack` callback.
//!
//! One fresh PocketIC instance, six named scenarios:
//!   1. install + bootstrap: install Router + Provision with a bootstrap binding that
//!      authorizes the Router principal.
//!   2. router outbound fresh admission: call `provision_graph` as the Router admin and
//!      assert an `Accepted` response, then a second identical call asserts `Replay`.
//!   3. post-upgrade durable binding: upgrade the Router with `provision_canister: None`
//!      and assert the durable stable binding still routes the next outbound call.
//!   4. provision -> router ack: call `router_ack` as the Provision canister principal with
//!      `accepted_registry_version=7` and assert `Ok(RouterAckResponse { accepted_registry_version: 7 })`.
//!   5. router ack idempotent replay: repeat the same ack and assert `Ok` with the same version.
//!   6. router ack version conflict: call `router_ack` with `accepted_registry_version=8` and
//!      assert `Err(AckConflict { stored: 7 })`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::ProvisionableResourceKind;
use gleaph_graph_kernel::provisioning::wire::{
    ProvisionJobSummary, ProvisionableResource, RouterAckResponse, RouterProvisionAck,
};
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::types::{
    AdminInstallDeploymentBindingArgs, AdminInstallError, BootstrapAuthAction, BootstrapAuthEntry,
    DeploymentBinding,
};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::{ProvisionGraphArgs, ProvisionGraphResponse};

struct Env {
    pic: pocket_ic::PocketIc,
    admin: Principal,
    router: Principal,
    provision: Principal,
}

fn install_router_and_provision() -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    // Install Provision canister first so we know its principal for Router init.
    let binding = DeploymentBinding {
        deployment_id: "deploy-p0058".to_owned(),
        router_principal: router,
        governance_principal: admin,
        binding_version: 1,
    };
    let provision = install_provision_canister(&pic, binding);

    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: Some(provision),
        })
        .expect("encode router init"),
        None,
    );

    Env {
        pic,
        admin,
        router,
        provision,
    }
}

fn call_provision_graph(
    env: &Env,
    args: &ProvisionGraphArgs,
) -> Result<ProvisionGraphResponse, gleaph_graph_kernel::federation::RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "provision_graph",
            Encode!(args).expect("encode provision_graph"),
        )
        .unwrap_or_else(|e| panic!("provision_graph on router: {e:?}"));

    Decode!(
        &bytes,
        Result<ProvisionGraphResponse, gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode provision_graph response")
}

fn call_router_ack(
    env: &Env,
    ack: &RouterProvisionAck,
) -> Result<RouterAckResponse, gleaph_graph_kernel::federation::RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.provision,
            "router_ack",
            Encode!(ack).expect("encode router_ack"),
        )
        .unwrap_or_else(|e| panic!("router_ack on router: {e:?}"));

    Decode!(
        &bytes,
        Result<RouterAckResponse, gleaph_graph_kernel::federation::RouterError>
    )
    .expect("decode router_ack response")
}

fn call_admin_install(
    env: &Env,
    caller: Principal,
    args: &AdminInstallDeploymentBindingArgs,
) -> Result<BootstrapAuthEntry, AdminInstallError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "admin_install_deployment_binding",
            Encode!(args).expect("encode admin_install_deployment_binding"),
        )
        .unwrap_or_else(|e| panic!("admin_install_deployment_binding on provision: {e:?}"));

    Decode!(
        &bytes,
        Result<BootstrapAuthEntry, AdminInstallError>
    )
    .expect("decode admin_install_deployment_binding response")
}

#[test]
fn router_outbound_accept_envelope_fresh_admission_replay_and_upgrade_durability() {
    let env = install_router_and_provision();
    let _ = env.provision;

    let args = ProvisionGraphArgs {
        deployment_id: "deploy-p0058".to_owned(),
        request_fingerprint: "fp-fresh-1".to_owned(),
        graph_name: "p0058.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-0".to_owned(),
        }],
        authorized_caller: env.admin,
        release_id: "rel-1".to_owned(),
    };

    // Scenario 1: fresh admission returns Accepted.
    let first = call_provision_graph(&env, &args).expect("first provision_graph accepted");
    let (state, intent_lock_count) = match first {
        ProvisionGraphResponse::Accepted {
            job_view: ProvisionJobSummary { state, .. },
            intent_lock_count,
        } => (state, intent_lock_count),
        ProvisionGraphResponse::Replay { .. } => panic!("first call must be Accepted"),
        ProvisionGraphResponse::Completed { .. } => panic!("first call must not be Completed"),
    };
    assert_eq!(
        state, "Reserved",
        "fresh admission reserves the intent lock before returning"
    );
    assert_eq!(intent_lock_count, 1, "one intent lock for one resource");

    // Scenario 2: identical retry returns Replay.
    let second = call_provision_graph(&env, &args).expect("second provision_graph accepted");
    assert!(
        matches!(second, ProvisionGraphResponse::Replay { .. }),
        "second call must be Replay"
    );

    // Scenario 4: Provision -> Router ack with accepted_registry_version=7.
    let ack = RouterProvisionAck {
        deployment_id: "deploy-p0058".to_owned(),
        request_id: "p0058.graph-fp-fresh-1".to_owned(),
        accepted_registry_version: 7,
    };
    let ack_response = call_router_ack(&env, &ack).expect("router_ack accepted");
    assert_eq!(
        ack_response.accepted_registry_version, 7,
        "router_ack returns the accepted registry version"
    );

    // Scenario 5: idempotent replay returns the same version.
    let ack_replay = call_router_ack(&env, &ack).expect("router_ack replay accepted");
    assert_eq!(
        ack_replay.accepted_registry_version, 7,
        "router_ack replay returns the stored registry version"
    );

    // Scenario 6: differing registry version returns AckConflict.
    let bad_ack = RouterProvisionAck {
        deployment_id: "deploy-p0058".to_owned(),
        request_id: "p0058.graph-fp-fresh-1".to_owned(),
        accepted_registry_version: 8,
    };
    let conflict =
        call_router_ack(&env, &bad_ack).expect_err("router_ack must conflict on version mismatch");
    assert_eq!(
        conflict,
        gleaph_graph_kernel::federation::RouterError::AckConflict { stored: 7 },
        "router_ack conflict must report the stored version"
    );

    // Scenario 7: admin_install_deployment_binding succeeds when called as the bootstrap
    // governance principal seeded at init; a follow-up Router outbound call for the new
    // deployment is no longer rejected with UnknownDeployment.
    let admin_install_args = AdminInstallDeploymentBindingArgs {
        deployment_id: "deploy-admin-1".to_owned(),
        router_principal: env.router,
        governance_principal: env.admin,
        binding_version: 2,
    };
    let admin_install_result = call_admin_install(&env, env.admin, &admin_install_args)
        .expect("bootstrap governance admin_install must succeed");
    assert_eq!(
        admin_install_result.action,
        BootstrapAuthAction::AdminInstall
    );
    assert_eq!(admin_install_result.caller, env.admin);

    let admin_installed_args = ProvisionGraphArgs {
        deployment_id: "deploy-admin-1".to_owned(),
        request_fingerprint: "fp-admin-1".to_owned(),
        graph_name: "admin1.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-admin-1".to_owned(),
        }],
        authorized_caller: env.admin,
        release_id: "rel-admin-1".to_owned(),
    };
    let admin_installed = call_provision_graph(&env, &admin_installed_args)
        .expect("outbound call for admin-installed deployment must be accepted");
    assert!(
        matches!(admin_installed, ProvisionGraphResponse::Accepted { .. }),
        "admin-installed deployment must accept fresh admission"
    );

    // Scenario 8: admin_install as a non-bootstrap, non-stored principal against a missing
    // deployment returns UnknownDeployment and does not install a binding.
    let wrong_principal = Principal::from_slice(&[0xCD; 29]);
    let missing_install_args = AdminInstallDeploymentBindingArgs {
        deployment_id: "deploy-admin-missing".to_owned(),
        router_principal: env.router,
        governance_principal: wrong_principal,
        binding_version: 3,
    };
    let reject = call_admin_install(&env, wrong_principal, &missing_install_args)
        .expect_err("unauthorized admin_install must be rejected");
    assert_eq!(
        reject,
        AdminInstallError::UnknownDeployment("deploy-admin-missing".to_owned())
    );

    let missing_args = ProvisionGraphArgs {
        deployment_id: "deploy-admin-missing".to_owned(),
        request_fingerprint: "fp-missing-1".to_owned(),
        graph_name: "missing.graph".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-missing-1".to_owned(),
        }],
        authorized_caller: env.admin,
        release_id: "rel-missing-1".to_owned(),
    };
    let missing_result = call_provision_graph(&env, &missing_args)
        .expect_err("outbound call for rejected deployment must still be rejected");
    assert!(
        matches!(
            missing_result,
            gleaph_graph_kernel::federation::RouterError::UnknownDeployment(_)
        ),
        "expected UnknownDeployment, got {missing_result:?}"
    );

    // Scenario 3: upgrade the Router with `provision_canister: None`; the durable stable
    // binding must keep the outbound path reachable.
    env.pic
        .upgrade_canister(
            env.router,
            wasm_bytes("ROUTER_WASM"),
            Encode!(&RouterInitArgs {
                issuing_principal: env.admin,
                initial_admins: vec![],
                provision_canister: None,
            })
            .expect("encode router upgrade args"),
            None,
        )
        .expect("upgrade router canister");

    let post_args = ProvisionGraphArgs {
        request_fingerprint: "fp-post-upgrade-1".to_owned(),
        requested_resources: vec![ProvisionableResource {
            kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-1".to_owned(),
        }],
        ..args
    };
    let third =
        call_provision_graph(&env, &post_args).expect("post-upgrade provision_graph accepted");
    assert!(
        matches!(third, ProvisionGraphResponse::Accepted { .. }),
        "post-upgrade call must still reach the original Provision canister"
    );
}
