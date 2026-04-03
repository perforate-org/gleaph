use std::fs;
use std::path::{Path, PathBuf};

use candid::{Principal, decode_one, encode_args};
use pocket_ic::PocketIc;

use gleaph_graph_registry::{
    CreateGraphRequest, GraphEntry, GraphResolution, ListGraphsResponse, RegistryError,
};

fn wasm_path(crate_stem: &str) -> PathBuf {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    let release = workspace_root
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(format!("{crate_stem}.wasm"));
    if release.exists() {
        return release;
    }
    workspace_root
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("debug")
        .join(format!("{crate_stem}.wasm"))
}

fn load_wasm(crate_stem: &str) -> Vec<u8> {
    fs::read(wasm_path(crate_stem)).unwrap_or_else(|e| {
        panic!(
            "failed to read wasm for {crate_stem}; build it first with `cargo build -p {} --target wasm32-unknown-unknown --release` (preferred) or `cargo build -p {} --target wasm32-unknown-unknown`: {e}",
            crate_stem.replace('_', "-"),
            crate_stem.replace('_', "-")
        )
    })
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn registry_create_graph_installs_queryable_child_canister() {
    let (pic, registry_id, sender) = install_registry_canister();
    let created = create_graph(&pic, registry_id, sender, "tenant.main");

    assert_eq!(created.graph_name, "tenant.main");
    assert_eq!(created.owner, sender);

    let child_bytes = pic
        .query_call(
            created.canister_id,
            sender,
            "list_prepared",
            encode_args(()).expect("list_prepared args"),
        )
        .unwrap_or_else(|e| panic!("query child canister failed: {e:?}"));
    let child_result: Result<Vec<gleaph_graph::PreparedQueryInfo>, String> =
        decode_one(&child_bytes).expect("decode child response");
    let prepared = child_result.unwrap_or_else(|e| panic!("child canister returned error: {e}"));
    assert!(prepared.is_empty(), "new graph canister should start empty");
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn registry_create_graph_is_visible_via_resolve_and_list() {
    let (pic, registry_id, sender) = install_registry_canister();
    let created = create_graph(&pic, registry_id, sender, "tenant.main");

    let resolve_bytes = pic
        .query_call(
            registry_id,
            sender,
            "resolve_graph",
            encode_args(("tenant.main".to_owned(),)).expect("resolve_graph args"),
        )
        .unwrap_or_else(|e| panic!("resolve_graph failed: {e:?}"));
    let resolved: Result<GraphResolution, RegistryError> =
        decode_one(&resolve_bytes).expect("decode resolve_graph response");
    let resolved = resolved.unwrap_or_else(|e| panic!("resolve_graph returned error: {e}"));
    assert_eq!(resolved.graph_name, created.graph_name);
    assert_eq!(resolved.canister_id, created.canister_id);

    let list_bytes = pic
        .query_call(
            registry_id,
            sender,
            "list_graphs",
            encode_args(()).expect("list_graphs args"),
        )
        .unwrap_or_else(|e| panic!("list_graphs failed: {e:?}"));
    let listed: ListGraphsResponse = decode_one(&list_bytes).expect("decode list_graphs response");
    assert_eq!(listed.items.len(), 1);
    assert_eq!(listed.items[0].graph_name, "tenant.main");
    assert_eq!(listed.items[0].canister_id, created.canister_id);
}

fn install_registry_canister() -> (PocketIc, Principal, Principal) {
    let pic = PocketIc::new();
    let sender = Principal::self_authenticating(b"gleaph-registry-e2e-sender");
    let registry_id = pic.create_canister();
    pic.add_cycles(registry_id, 2_000_000_000_000);
    pic.install_canister(
        registry_id,
        load_wasm("gleaph_graph_registry"),
        encode_args(()).expect("registry init arg"),
        Some(sender),
    );
    (pic, registry_id, sender)
}

fn create_graph(
    pic: &PocketIc,
    registry_id: Principal,
    sender: Principal,
    graph_name: &str,
) -> GraphEntry {
    let bytes = pic
        .update_call(
            registry_id,
            sender,
            "create_graph",
            encode_args((CreateGraphRequest {
                graph_name: graph_name.to_owned(),
                owner: None,
                admins: Vec::new(),
                status: None,
            },))
            .expect("encode create_graph"),
        )
        .unwrap_or_else(|e| panic!("registry create_graph failed: {e:?}"));
    let result: Result<GraphEntry, RegistryError> =
        decode_one(&bytes).expect("decode create_graph response");
    result.unwrap_or_else(|e| panic!("registry create_graph returned error: {e}"))
}
