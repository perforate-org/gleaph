//! PocketIC E2E: install the inference canister, upload the pruned INT8 pack in 1.9MB chunks,
//! then run one update-call inference and check the returned decision.

use std::path::{Path, PathBuf};

use candid::{CandidType, Decode, Deserialize, Encode, Principal};
use gleaph_routing_canister::RouteAndExtract;
use pocket_ic::PocketIcBuilder;

#[derive(CandidType, Deserialize)]
struct InstallChunk {
    offset: u64,
    bytes: Vec<u8>,
}

const CHUNK: usize = 1_900_000;

fn artifact(relative: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    path.exists().then_some(path)
}

#[test]
fn installs_chunked_pack_and_infers() {
    let Some(wasm) = artifact("target/wasm32-unknown-unknown/release/gleaph_routing_canister.wasm")
    else {
        eprintln!("skipping: canister wasm not built");
        return;
    };
    let pack_path = std::env::var_os("GLEV_PACK")
        .map(PathBuf::from)
        .or_else(|| artifact("target/canister-infer/model20-distil14b-h1-pruned.int8.pack"));
    let Some(pack_path) = pack_path else {
        eprintln!("skipping: pack not found");
        return;
    };
    let server = std::env::var_os("POCKET_IC_BIN")
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .or_else(crate_bin);
    let Some(server) = server else {
        eprintln!("skipping: PocketIC server binary not found");
        return;
    };

    let pic = PocketIcBuilder::new()
        .with_server_binary(server)
        .with_application_subnet()
        .build();
    let controller = Principal::from_slice(&[7u8; 29]);
    let canister = pic.create_canister();
    pic.add_cycles(canister, 100_000_000_000_000);
    // The creator is the default controller; hand control to the principal used as sender.
    pic.set_controllers(canister, None, vec![controller])
        .expect("set controllers");
    let wasm_bytes = std::fs::read(&wasm).expect("read wasm");
    pic.install_canister(
        canister,
        wasm_bytes.clone(),
        Encode!().unwrap(),
        Some(controller),
    );

    let pack = std::fs::read(&pack_path).expect("read pack");
    let chunks: Vec<&[u8]> = pack.chunks(CHUNK).collect();
    eprintln!("pack {} bytes in {} chunks", pack.len(), chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let payload = InstallChunk {
            offset: (index * CHUNK) as u64,
            bytes: chunk.to_vec(),
        };
        let reply = pic
            .update_call(
                canister,
                controller,
                "admin_install_chunk",
                Encode!(&payload).unwrap(),
            )
            .expect("install chunk call");
        let result: Result<u64, String> = Decode!(&reply, Result<u64, String>).expect("decode");
        result.expect("install chunk");
    }
    let digest = "0".repeat(64);
    let reply = pic
        .update_call(
            canister,
            controller,
            "admin_install_finish",
            Encode!(&digest).unwrap(),
        )
        .expect("finish call");
    let result: Result<u64, String> = Decode!(&reply, Result<u64, String>).expect("decode");
    eprintln!("installed {} bytes", result.expect("finish install"));

    let reply = pic
        .query_call(
            canister,
            controller,
            "weights_installed",
            Encode!().unwrap(),
        )
        .expect("weights_installed call");
    let installed: bool = Decode!(&reply, bool).expect("decode");
    assert!(installed, "weights should be installed");

    // First blind row from the tokenized eval dump.
    let eval_path = std::env::var_os("GLEV_EVAL")
        .map(PathBuf::from)
        .or_else(|| artifact("target/canister-infer/tokenized-eval.jsonl"))
        .expect("tokenized eval");
    let mut blind = None;
    for line in std::fs::read_to_string(eval_path)
        .expect("read eval")
        .lines()
    {
        let row: serde_json::Value = serde_json::from_str(line).expect("row");
        if row["set"].as_str() == Some("dev") {
            blind = Some(row);
            break;
        }
    }
    let row = blind.expect("blind row");
    let ids: Vec<u32> = row["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    let markers: Vec<u32> = row["markers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    let reply = pic
        .update_call(
            canister,
            controller,
            "infer",
            Encode!(&ids, &markers).unwrap(),
        )
        .expect("infer call");
    let probs: Result<Vec<f32>, String> =
        Decode!(&reply, Result<Vec<f32>, String>).expect("decode");
    let probs = probs.expect("infer");
    eprintln!("probs {probs:?}");
    assert_eq!(probs.len(), 6);
    assert!(probs.iter().all(|p| p.is_finite()));
    let argmax = probs
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap()
        .0;
    let options: Vec<String> = row["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let gold = row["gold"].as_str().unwrap();
    eprintln!("canister decision {} (gold {})", options[argmax], gold);

    // Route options come from the pack, so the client never guesses the probability order.
    let reply = pic
        .query_call(canister, controller, "route_options", Encode!().unwrap())
        .expect("route_options call");
    let options: Result<Vec<String>, String> =
        Decode!(&reply, Result<Vec<String>, String>).expect("decode route options");
    let options = options.expect("route options");
    assert_eq!(options.len(), 6, "six routing options");
    assert!(options.iter().any(|option| option == "shortest-path"));

    // The slot path returns spans named by the pack's schema plus the same probabilities.
    let reply = pic
        .update_call(
            canister,
            controller,
            "route_and_extract",
            Encode!(&ids, &markers).unwrap(),
        )
        .expect("route_and_extract call");
    let extracted: Result<RouteAndExtract, String> =
        Decode!(&reply, Result<RouteAndExtract, String>).expect("decode extraction");
    let extracted = extracted.expect("route_and_extract");
    for (left, right) in probs.iter().zip(&extracted.probabilities) {
        assert!(
            (left - right).abs() < 1e-6,
            "slot path should agree with infer"
        );
    }
    assert!(!extracted.spans.is_empty(), "question should contain slots");
    let known = ["anchor", "hops", "concept", "depth", "team", "topic"];
    for span in &extracted.spans {
        assert!(span.score.is_finite() && (0.0..=1.0).contains(&span.score));
        assert!(
            known.contains(&span.tag.as_str()),
            "unknown tag {}",
            span.tag
        );
        assert!(span.start < span.end, "span must be non-empty");
        assert!(
            span.end as usize <= ids.len(),
            "span must stay inside the input"
        );
    }

    // A long question is split into 32-token windows inside the canister; spans stay inside it.
    let mut long_ids: Vec<u32> = Vec::new();
    while long_ids.len() < 96 {
        long_ids.extend_from_slice(&ids);
    }
    long_ids.truncate(96);
    let reply = pic
        .update_call(
            canister,
            controller,
            "route_and_extract",
            Encode!(&long_ids).unwrap(),
        )
        .expect("route_and_extract long call");
    let long: Result<RouteAndExtract, String> =
        Decode!(&reply, Result<RouteAndExtract, String>).expect("decode long extraction");
    let long = long.expect("long extraction");
    assert_eq!(long.probabilities.len(), 6, "six routing options");
    assert!(!long.spans.is_empty(), "long question should contain slots");
    assert!(
        long.spans.iter().any(|span| span.score > 0.0),
        "spans should carry tag confidence"
    );
    for span in &long.spans {
        assert!(span.score.is_finite() && (0.0..=1.0).contains(&span.score));
        assert!(
            span.end as usize <= long_ids.len(),
            "span outside the long input"
        );
    }

    // Second identical call must hit the decision cache and return the same probabilities.
    let reply2 = pic
        .update_call(
            canister,
            controller,
            "infer",
            Encode!(&ids, &markers).unwrap(),
        )
        .expect("infer call 2");
    let probs2: Result<Vec<f32>, String> =
        Decode!(&reply2, Result<Vec<f32>, String>).expect("decode");
    let probs2 = probs2.expect("infer 2");
    for (left, right) in probs.iter().zip(&probs2) {
        assert!(
            (left - right).abs() < 1e-6,
            "cached probabilities should be identical"
        );
    }
    let reply = pic
        .query_call(canister, controller, "cache_stats", Encode!().unwrap())
        .expect("cache_stats call");
    let (hits, misses, entries): (u64, u64, u64) = Decode!(&reply, u64, u64, u64).expect("stats");
    assert!(
        hits >= 1 && misses >= 1 && entries >= 1,
        "cache stats hits={hits} misses={misses} entries={entries}"
    );
}

fn crate_bin() -> Option<PathBuf> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../pocket-ic-tests/.pocket-ic/pocket-ic");
    path.exists().then_some(path)
}
