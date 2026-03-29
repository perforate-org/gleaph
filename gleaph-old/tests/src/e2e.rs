use std::fs;
use std::path::{Path, PathBuf};

use candid::{Principal, decode_one, encode_args};
use pocket_ic::PocketIc;

fn wasm_path(crate_stem: &str) -> PathBuf {
    let release = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(format!("{crate_stem}.wasm"));
    if release.exists() {
        return release;
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
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

struct Harness {
    pic: PocketIc,
    graph_id: Principal,
    registry_id: Principal,
    sender: Principal,
}

impl Harness {
    fn new() -> Self {
        let pic = PocketIc::new();
        let sender = Principal::anonymous();

        let graph_id = pic.create_canister();
        let registry_id = pic.create_canister();
        pic.add_cycles(graph_id, 2_000_000_000_000);
        pic.add_cycles(registry_id, 2_000_000_000_000);

        pic.install_canister(
            graph_id,
            load_wasm("gleaph_graph"),
            encode_args((Some(64u32), Some(0u64))).expect("graph init arg"),
            Some(sender),
        );
        pic.install_canister(
            registry_id,
            load_wasm("gleaph_registry"),
            encode_args(()).expect("registry init arg"),
            Some(sender),
        );

        Self {
            pic,
            graph_id,
            registry_id,
            sender,
        }
    }

    fn graph_update<
        T: candid::CandidType,
        R: candid::CandidType + for<'de> candid::Deserialize<'de>,
    >(
        &self,
        method: &str,
        args: T,
    ) -> R {
        let bytes = self
            .pic
            .update_call(
                self.graph_id,
                self.sender,
                method,
                encode_args((args,)).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("update_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode update response")
    }

    fn graph_query<
        T: candid::CandidType,
        R: candid::CandidType + for<'de> candid::Deserialize<'de>,
    >(
        &self,
        method: &str,
        args: T,
    ) -> R {
        let bytes = self
            .pic
            .query_call(
                self.graph_id,
                self.sender,
                method,
                encode_args((args,)).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("query_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode query response")
    }

    fn canister_query<
        T: candid::CandidType,
        R: candid::CandidType + for<'de> candid::Deserialize<'de>,
    >(
        &self,
        canister_id: Principal,
        method: &str,
        args: T,
    ) -> R {
        let bytes = self
            .pic
            .query_call(
                canister_id,
                self.sender,
                method,
                encode_args((args,)).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("query_call {method} failed: {e:?}"));
        decode_one(&bytes).expect("decode query response")
    }
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn deploy_graph_canister_add_edges_query_neighbors() {
    let h = Harness::new();

    let r1: Result<u64, String> = h.graph_update(
        "add_edge",
        gleaph_types::EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 10,
        },
    );
    assert!(r1.is_ok());

    let r2: Result<u64, String> = h.graph_update(
        "add_edge",
        gleaph_types::EdgeData {
            src: 0,
            dst: 2,
            weight: 2.0,
            timestamp: 20,
        },
    );
    assert!(r2.is_ok());

    let neighbors: Vec<gleaph_types::EdgeInfo> = h.graph_query("get_neighbors", 0u32);
    assert_eq!(neighbors.len(), 2);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn bulk_insert_edges_and_verify_stats() {
    let h = Harness::new();

    let edges = vec![
        gleaph_types::EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        },
        gleaph_types::EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        },
        gleaph_types::EdgeData {
            src: 2,
            dst: 3,
            weight: 1.0,
            timestamp: 3,
        },
    ];
    let result: Result<u64, String> = h.graph_update("bulk_insert_edges", edges);
    assert!(matches!(result, Ok(3)));

    let stats: gleaph_types::GraphStats = h.graph_query("get_stats", ());
    assert_eq!(stats.num_edges, 3);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn canister_upgrade_preserves_graph_data() {
    let h = Harness::new();
    let _: Result<u64, String> = h.graph_update(
        "add_edge",
        gleaph_types::EdgeData {
            src: 0,
            dst: 9,
            weight: 1.0,
            timestamp: 9,
        },
    );

    h.pic
        .upgrade_canister(
            h.graph_id,
            load_wasm("gleaph_graph"),
            encode_args((Option::<u32>::None, Option::<u64>::None)).expect("upgrade arg"),
            Some(h.sender),
        )
        .expect("upgrade graph canister");

    let neighbors: Vec<gleaph_types::EdgeInfo> = h.graph_query("get_neighbors", 0u32);
    assert!(neighbors.iter().any(|e| e.target == 9));
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn registry_creates_graph_canister_and_returns_canister_id() {
    let h = Harness::new();
    let payload = gleaph_types::GraphConfig {
        name: "test".to_string(),
        initial_vertex_capacity: 128,
        initial_edge_capacity: 0,
    };
    let bytes = h
        .pic
        .update_call(
            h.registry_id,
            h.sender,
            "create_graph",
            encode_args((payload,)).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("registry create_graph failed: {e:?}"));
    let info: Result<gleaph_types::GraphInfo, String> =
        decode_one(&bytes).expect("decode graph info");
    let info = info.unwrap_or_else(|e| panic!("registry create_graph returned error: {e}"));
    assert_eq!(info.name, "test");
    assert!(info.canister_id.is_some());
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn registry_create_graph_applies_initial_edge_capacity() {
    let h = Harness::new();
    let requested_capacity = 10_000u64;
    let payload = gleaph_types::GraphConfig {
        name: "capacity-test".to_string(),
        initial_vertex_capacity: 128,
        initial_edge_capacity: requested_capacity,
    };
    let bytes = h
        .pic
        .update_call(
            h.registry_id,
            h.sender,
            "create_graph",
            encode_args((payload,)).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("registry create_graph failed: {e:?}"));
    let info: Result<gleaph_types::GraphInfo, String> =
        decode_one(&bytes).expect("decode graph info");
    let info = info.unwrap_or_else(|e| panic!("registry create_graph returned error: {e}"));
    let child_id = info.canister_id.expect("child graph canister id");

    let stats: gleaph_types::GraphStats = h.canister_query(child_id, "get_stats", ());
    assert!(
        stats.elem_capacity >= requested_capacity,
        "elem_capacity={} requested={requested_capacity}",
        stats.elem_capacity
    );
    assert_eq!(stats.num_vertices, 128);
}

#[test]
#[ignore = "requires local wasm build artifacts and PocketIC runtime"]
fn ecommerce_scenario_multi_insert_and_queries() {
    let h = Harness::new();
    let edges = vec![
        gleaph_types::EdgeData {
            src: 100,
            dst: 200,
            weight: 1.0,
            timestamp: 1,
        }, // user -> product
        gleaph_types::EdgeData {
            src: 100,
            dst: 201,
            weight: 1.0,
            timestamp: 2,
        },
        gleaph_types::EdgeData {
            src: 101,
            dst: 200,
            weight: 1.0,
            timestamp: 3,
        },
    ];
    let result: Result<u64, String> = h.graph_update("bulk_insert_edges", edges);
    assert!(result.is_ok());

    let neighbors_100: Vec<gleaph_types::EdgeInfo> = h.graph_query("get_neighbors", 100u32);
    let neighbors_101: Vec<gleaph_types::EdgeInfo> = h.graph_query("get_neighbors", 101u32);
    assert_eq!(neighbors_100.len(), 2);
    assert_eq!(neighbors_101.len(), 1);
}
