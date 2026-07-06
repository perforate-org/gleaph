//! PocketIC E2E for ADR 0035 Slice 5: Router outbound accept_envelope send.
//!
//! One fresh PocketIC instance, three named scenarios:
//!   1. install + bootstrap: install Router + Provision with a bootstrap binding that
//!      authorizes the Router principal.
//!   2. router outbound fresh admission: call `provision_graph` as the Router admin and
//!      assert an `Accepted` response, then a second identical call asserts `Replay`.
//!   3. post-upgrade durable binding: upgrade the Router with `provision_canister: None`
//!      and assert the durable stable binding still routes the next outbound call.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::ProvisionableResourceKind;
use gleaph_graph_kernel::provisioning::wire::{ProvisionJobSummary, ProvisionableResource};
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::types::DeploymentBinding;
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
