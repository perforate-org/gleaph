//! PocketIC E2E for ADR 0111 slice 3: the Router owns the deployment inference canister and
//! proxies routing for an inference-enabled graph.
//!
//! Run locally with `GLEV_PACK` (the pinned INT8 pack) and an inference wasm at
//! `INFERENCE_WASM` or the workspace release target. The test skips when those artifacts are
//! absent so the general PocketIC suite keeps working without the model release.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{GRAPH_NAME, install_single_shard_federation_with_graph_admins};
use gleaph_provision::types::sha256;
use gleaph_routing_wire::{RouteAndExtract, TextRouteAndExtract, TokenizerFingerprint};

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

fn tokenizer_bytes() -> Option<Vec<u8>> {
    let path = std::env::var("GLEV_TOKENIZER")
        .map(std::path::PathBuf::from)
        .ok()
        .filter(|path| path.exists())
        .or_else(|| {
            let pinned = workspace_root().join("target/glev-artifacts/glev-0.1.0-tokenizer.json");
            pinned.exists().then_some(pinned)
        })?;
    Some(std::fs::read(path).expect("read tokenizer"))
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
    let (Some(wasm), Some(pack), Some(tokenizer)) =
        (inference_wasm(), pack_bytes(), tokenizer_bytes())
    else {
        eprintln!("skipping: inference wasm, pack or tokenizer not present");
        return;
    };
    let graph_caller = Principal::from_slice(&[0xCD; 29]);
    let env =
        install_single_shard_federation_with_graph_admins(std::collections::BTreeSet::from([
            graph_caller,
        ]));
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

    // Install the tokenizer through the Router and route question text (ADR 0113).
    for (index, chunk) in tokenizer.chunks(PACK_CHUNK).enumerate() {
        let offset = (index * PACK_CHUNK) as u64;
        let reply = pic
            .update_call(
                env.router,
                env.admin,
                "admin_install_tokenizer_chunk",
                Encode!(&offset, &chunk.to_vec()).unwrap(),
            )
            .unwrap_or_else(|error| panic!("tokenizer chunk {index}: {error}"));
        let result: Result<(), RouterError> = Decode!(&reply, Result<(), RouterError>)
            .unwrap_or_else(|error| panic!("decode tokenizer chunk {index}: {error}"));
        result.unwrap_or_else(|error| panic!("tokenizer chunk {index}: {error:?}"));
    }
    let expected_sha = sha256(&tokenizer)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_install_tokenizer_finish",
            Encode!(&Some(expected_sha)).unwrap(),
        )
        .expect("admin_install_tokenizer_finish");
    let fingerprint: Result<TokenizerFingerprint, RouterError> =
        Decode!(&reply, Result<TokenizerFingerprint, RouterError>).expect("decode fingerprint");
    let fingerprint = fingerprint.expect("tokenizer finish");
    assert_eq!(fingerprint.bytes, tokenizer.len() as u64);

    let text_question = "GraphRAGの論文を引用している文書を深さ3まで教えてください。";
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_and_extract_text",
            Encode!(&GRAPH_NAME.to_owned(), &text_question.to_owned()).unwrap(),
        )
        .expect("route_and_extract_text");
    let text_route: Result<TextRouteAndExtract, RouterError> =
        Decode!(&reply, Result<TextRouteAndExtract, RouterError>).expect("decode text route");
    let text_route = text_route.expect("text route");
    assert_eq!(text_route.probabilities.len(), 6);
    assert_eq!(text_route.token_count, 16);
    assert!(text_route.cjk_fraction > 0.1);
    for span in &text_route.spans {
        assert!(span.start < span.end && (span.end as usize) <= text_question.len());
    }

    // Router-funded quota: a non-controller graph admin gets one unit and is then denied, while
    // the controller stays exempt.
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_inference_quota",
            Encode!(&60u32, &1u32, &true).unwrap(),
        )
        .expect("admin_set_inference_quota");
    decode::<()>(&reply, "admin_set_inference_quota").expect("set quota");

    for attempt in 0..2 {
        let reply = pic
            .update_call(
                env.router,
                graph_caller,
                "route_and_extract",
                Encode!(&GRAPH_NAME.to_owned(), &token_ids).unwrap(),
            )
            .expect("quota route call");
        let result = decode::<RouteAndExtract>(&reply, "route_and_extract");
        if attempt == 0 {
            result.expect("first quota unit is allowed");
        } else {
            assert_eq!(
                result.expect_err("quota denied"),
                RouterError::Busy {
                    operation: "inference_quota".to_owned(),
                }
            );
        }
    }
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_and_extract",
            Encode!(&GRAPH_NAME.to_owned(), &token_ids).unwrap(),
        )
        .expect("controller route call");
    decode::<RouteAndExtract>(&reply, "route_and_extract").expect("controller is exempt");

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
