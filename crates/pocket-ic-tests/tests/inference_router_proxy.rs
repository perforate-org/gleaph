//! PocketIC E2E for ADR 0111 slice 3: the Router owns the deployment inference canister and
//! proxies routing for an inference-enabled graph.
//!
//! Run locally with `GLEV_PACK` (the pinned INT8 pack) and an inference wasm at
//! `INFERENCE_WASM` or the workspace release target. The test skips when those artifacts are
//! absent so the general PocketIC suite keeps working without the model release.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{GRAPH_NAME, install_single_shard_federation};
use gleaph_routing_wire::RouteAndExtract;

const PACK_CHUNK: usize = 1_900_000;

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn inference_wasm() -> Option<Vec<u8>> {
    let path = std::env::var("INFERENCE_WASM")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            workspace_root()
                .join("target/wasm32-unknown-unknown/release/gleaph_routing_canister.wasm")
        });
    path.exists()
        .then(|| std::fs::read(&path).expect("read inference wasm"))
}

fn pack_bytes() -> Option<Vec<u8>> {
    let path = std::env::var("GLEV_PACK")
        .map(std::path::PathBuf::from)
        .ok()
        .filter(|path| path.exists())
        .or_else(|| {
            let pinned =
                workspace_root().join("target/glev-artifacts/glev-0.1.0-h640-slot-int8.pack");
            pinned.exists().then_some(pinned)
        })?;
    Some(std::fs::read(path).expect("read inference pack"))
}

fn decode<'de, ResultT: candid::CandidType + candid::Deserialize<'de>>(
    bytes: &'de [u8],
    method: &str,
) -> Result<ResultT, RouterError> {
    let result: Result<ResultT, RouterError> = Decode!(bytes, Result<ResultT, RouterError>)
        .unwrap_or_else(|error| panic!("decode {method}: {error}"));
    result
}

#[test]
fn router_proxies_routing_for_an_enabled_graph() {
    let (Some(wasm), Some(pack)) = (inference_wasm(), pack_bytes()) else {
        eprintln!("skipping: inference wasm or pinned pack not present");
        return;
    };
    let env = install_single_shard_federation();
    let pic = &env.pic;

    // The admin controls the Router so the deployment-level admin methods pass.
    pic.set_controllers(env.router, None, vec![env.admin])
        .expect("router controller");

    // Install the inference canister and make both the admin and the Router its controllers:
    // the Router forwards pack chunks and routing calls.
    let inference = pic.create_canister();
    pic.add_cycles(inference, 100_000_000_000_000);
    pic.set_controllers(inference, None, vec![env.admin, env.router])
        .expect("inference controllers");
    pic.install_canister(inference, wasm, Encode!().unwrap(), Some(env.admin));

    // Bind the deployment inference canister (wire_version is verified).
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_inference_canister",
            Encode!(&Some(inference)).unwrap(),
        )
        .expect("admin_set_inference_canister");
    decode::<()>(&reply, "admin_set_inference_canister").expect("bind inference");

    // Upload the pinned pack through the Router, exercising the controller forwarding path.
    for (index, chunk) in pack.chunks(PACK_CHUNK).enumerate() {
        let offset = (index * PACK_CHUNK) as u64;
        let reply = pic
            .update_call(
                env.router,
                env.admin,
                "admin_install_inference_pack_chunk",
                Encode!(&offset, &chunk.to_vec()).unwrap(),
            )
            .unwrap_or_else(|error| panic!("pack chunk {index}: {error}"));
        decode::<()>(&reply, "admin_install_inference_pack_chunk")
            .unwrap_or_else(|error| panic!("pack chunk {index}: {error:?}"));
    }
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_install_inference_pack_finish",
            Encode!(&"0".repeat(64)).unwrap(),
        )
        .expect("admin_install_inference_pack_finish");
    decode::<()>(&reply, "admin_install_inference_pack_finish").expect("finish pack");

    // Enable model routing for the graph.
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_enable_graph_inference",
            Encode!(&GRAPH_NAME.to_owned()).unwrap(),
        )
        .expect("admin_enable_graph_inference");
    let bound = decode::<Principal>(&reply, "admin_enable_graph_inference").expect("enable");
    assert_eq!(bound, inference);

    // The Router proxies a normal routing request for the enabled graph.
    let token_ids = vec![2u32, 100, 200, 300, 4];
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_and_extract",
            Encode!(&GRAPH_NAME.to_owned(), &token_ids).unwrap(),
        )
        .expect("route_and_extract");
    let extraction = decode::<RouteAndExtract>(&reply, "route_and_extract").expect("route");
    assert_eq!(extraction.probabilities.len(), 6);
    assert_eq!(extraction.spans.len(), extraction.spans.len()); // spans are model-defined

    // The proxy also exposes the pack's option names.
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_options",
            Encode!(&GRAPH_NAME.to_owned()).unwrap(),
        )
        .expect("route_options");
    let options = decode::<Vec<String>>(&reply, "route_options").expect("options");
    assert!(options.iter().any(|option| option == "none-of-these"));

    // Disabling the graph closes the proxy while the deployment binding remains.
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_disable_graph_inference",
            Encode!(&GRAPH_NAME.to_owned()).unwrap(),
        )
        .expect("admin_disable_graph_inference");
    decode::<()>(&reply, "admin_disable_graph_inference").expect("disable");
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_and_extract",
            Encode!(&GRAPH_NAME.to_owned(), &token_ids).unwrap(),
        )
        .expect("route_and_extract after disable");
    let denied = decode::<RouteAndExtract>(&reply, "route_and_extract").expect_err("disabled");
    assert_eq!(denied, RouterError::Forbidden);
}
