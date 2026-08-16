//! PocketIC coverage for the `gleaph deploy` dev-mode path: install the Router, graph-index,
//! and graph-shard canisters directly (no Provision), register the graph + shard through Router
//! `register_graph`, and register the Router under the caller's Account so `resolve_router`
//! succeeds. Mirrors `scripts/deploy-demo-local.sh`'s platform half and the CLI `deploy::deploy`
//! dev-mode flow.

use candid::{Decode, Encode, Principal};
use gleaph_account::types::{Account, AccountError, RouterEntry};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::provisioning::init_args::{GraphInitArgs, IndexInitArgs, RouterInitArgs};
use gleaph_pocket_ic_tests::{install_account_canister, new_pocket_ic, wasm_bytes};
use gleaph_router::types::{RegisterGraphArgs, RegisterGraphShard};

fn user_principal() -> Principal {
    Principal::from_slice(&[0x30; 29])
}

fn create_canister(pic: &pocket_ic::PocketIc) -> Principal {
    let id = pic.create_canister();
    pic.add_cycles(id, 2_000_000_000_000);
    id
}

#[test]
fn dev_mode_deploy_installs_platform_and_registers_router() {
    let pic = new_pocket_ic();
    let user = user_principal();
    let account = install_account_canister(&pic);

    // 1. Register a Personal account for the caller.
    let bytes = pic
        .update_call(
            account,
            user,
            "create_account",
            Encode!(&"alice".to_owned()).unwrap(),
        )
        .expect("create_account call");
    let created: Result<Account, AccountError> =
        Decode!(&bytes, Result<Account, AccountError>).expect("decode create_account");
    let account_id = created.expect("create_account").id();

    // 2. Install the Router with dev-mode init args (no provision_canister).
    let router = create_canister(&pic);
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: user,
            initial_admins: vec![],
            provision_canister: None,
        })
        .expect("encode router init"),
        None,
    );

    // 3. Install the graph-index canister trusting the Router.
    let index = create_canister(&pic);
    pic.install_canister(
        index,
        wasm_bytes("INDEX_WASM"),
        Encode!(&IndexInitArgs {
            router_canister: router,
        })
        .expect("encode index init"),
        None,
    );

    // 4. Install the graph-shard canister with federation routing to the Router and index.
    let graph = create_canister(&pic);
    pic.install_canister(
        graph,
        wasm_bytes("GRAPH_WASM"),
        Encode!(&GraphInitArgs {
            logical_graph_name: Some("social".into()),
            router_canister: Some(router),
            shard_id: Some(ShardId::new(0)),
            index_canister: Some(index),
        })
        .expect("encode graph init"),
        None,
    );

    // 5. Register the graph + shard through Router `register_graph` (dev mode).
    let bytes = pic
        .update_call(
            router,
            user,
            "register_graph",
            Encode!(&RegisterGraphArgs {
                graph_name: "social".into(),
                owner: user,
                admins: Default::default(),
                is_home: false,
                shards: vec![RegisterGraphShard {
                    shard_id: ShardId::new(0),
                    graph_canister: graph,
                    index_canister: index,
                }],
                requested_resources: Vec::new(),
            })
            .expect("encode register_graph"),
        )
        .expect("register_graph call");
    let result: Result<(), RouterError> =
        Decode!(&bytes, Result<(), RouterError>).expect("decode register_graph");
    assert!(
        result.is_ok(),
        "dev-mode register_graph must succeed: {result:?}"
    );

    // 6. Register the Router under the caller's account.
    let bytes = pic
        .update_call(
            account,
            user,
            "register_router",
            candid::encode_args((
                &account_id,
                &RouterEntry {
                    router_id: "default".into(),
                    router_canister: router,
                },
            ))
            .expect("encode register_router"),
        )
        .expect("register_router call");
    let result: Result<(), AccountError> =
        Decode!(&bytes, Result<(), AccountError>).expect("decode register_router");
    assert!(result.is_ok(), "register_router must succeed: {result:?}");

    // 7. Account.resolve_router("default") now returns the Router principal (the CLI resolves
    //    the Router through this path after `gleaph deploy`).
    let bytes = pic
        .query_call(
            account,
            user,
            "resolve_router",
            candid::encode_args((&account_id, &"default".to_owned())).expect("encode resolve"),
        )
        .expect("resolve_router call");
    let result: Result<Principal, AccountError> =
        Decode!(&bytes, Result<Principal, AccountError>).expect("decode resolve_router");
    assert_eq!(
        result.expect("resolve_router"),
        router,
        "resolve_router must return the issued Router principal"
    );
}
