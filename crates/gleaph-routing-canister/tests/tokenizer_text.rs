//! PocketIC E2E for ADR 0113 slice 1: install the tokenizer into the inference canister, count
//! tokens, route text with byte-offset spans, and reload the tokenizer on upgrade.

use std::path::{Path, PathBuf};

use candid::{CandidType, Decode, Deserialize, Encode, Principal};
use gleaph_routing_canister::{TextRouteAndExtract, TokenizerFingerprint};
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

fn wasm() -> Option<Vec<u8>> {
    artifact("target/wasm32-unknown-unknown/release/gleaph_routing_canister.wasm")
        .map(|path| std::fs::read(path).expect("read canister wasm"))
}

fn pack() -> Option<Vec<u8>> {
    std::env::var_os("GLEV_PACK")
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| artifact("target/glev-artifacts/glev-0.1.0-h640-slot-int8.pack"))
        .map(|path| std::fs::read(path).expect("read pack"))
}

fn tokenizer() -> Option<Vec<u8>> {
    std::env::var_os("GLEV_TOKENIZER")
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| artifact("target/glev-artifacts/glev-0.1.0-tokenizer.json"))
        .map(|path| std::fs::read(path).expect("read tokenizer"))
}

fn upload(
    pic: &pocket_ic::PocketIc,
    canister: Principal,
    controller: Principal,
    chunk_method: &str,
    bytes: &[u8],
) {
    for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
        let offset = (index * CHUNK) as u64;
        let reply = pic
            .update_call(
                canister,
                controller,
                chunk_method,
                Encode!(&InstallChunk {
                    offset,
                    bytes: chunk.to_vec(),
                })
                .expect("encode chunk"),
            )
            .unwrap_or_else(|error| panic!("{chunk_method} chunk {index}: {error}"));
        let installed: Result<u64, String> = Decode!(&reply, Result<u64, String>).expect("decode");
        installed.unwrap_or_else(|error| panic!("{chunk_method} chunk {index}: {error}"));
    }
}

fn decode_result<T: CandidType + serde::de::DeserializeOwned>(
    reply: &[u8],
    method: &str,
) -> Result<T, String> {
    let result: Result<T, String> = Decode!(reply, Result<T, String>)
        .unwrap_or_else(|error| panic!("decode {method}: {error}"));
    result
}

#[test]
fn tokenizer_text_route_round_trip_and_upgrade_reload() {
    let (Some(wasm), Some(pack), Some(tokenizer)) = (wasm(), pack(), tokenizer()) else {
        eprintln!("skipping: canister wasm, pack or tokenizer not present");
        return;
    };
    let server = std::env::var_os("POCKET_IC_BIN")
        .map(PathBuf::from)
        .filter(|path| path.exists());
    let server = server.or_else(|| artifact("crates/pocket-ic-tests/.pocket-ic/pocket-ic-launcher"));
    let Some(server) = server else {
        eprintln!("skipping: PocketIC server binary not found");
        return;
    };

    let pic = PocketIcBuilder::new()
        .with_server_binary(server)
        .with_application_subnet()
        .build();
    let controller = Principal::from_slice(&[0x51; 29]);
    let canister = pic.create_canister();
    pic.add_cycles(canister, 200_000_000_000_000);
    pic.set_controllers(canister, None, vec![controller])
        .expect("set controllers");
    pic.install_canister(canister, wasm.clone(), Encode!(&()).unwrap(), Some(controller));

    upload(&pic, canister, controller, "admin_install_chunk", &pack);
    let reply = pic
        .update_call(
            canister,
            controller,
            "admin_install_finish",
            Encode!(&"0".repeat(64)).expect("encode finish"),
        )
        .expect("pack finish");
    decode_result::<u64>(&reply, "admin_install_finish").expect("pack finish");

    upload(&pic, canister, controller, "admin_install_tokenizer_chunk", &tokenizer);
    let reply = pic
        .update_call(
            canister,
            controller,
            "admin_install_tokenizer_finish",
            Encode!(&()).expect("encode tokenizer finish"),
        )
        .expect("tokenizer finish");
    let fingerprint =
        decode_result::<TokenizerFingerprint>(&reply, "tokenizer finish").expect("tokenizer");
    assert_eq!(fingerprint.bytes, tokenizer.len() as u64);
    assert_eq!(fingerprint.sha256.len(), 32);

    let question = "GraphRAGの論文を引用している文書を深さ3まで教えてください。";
    let reply = pic
        .update_call(
            canister,
            controller,
            "tokenizer_token_count",
            Encode!(&question.to_owned()).expect("encode count"),
        )
        .expect("token count");
    let (count, cjk): (u32, f32) =
        decode_result(&reply, "tokenizer_token_count").expect("token count");
    assert!(count > 0);
    assert!(cjk > 0.1, "Japanese question should carry a CJK fraction");
    assert_eq!(count, 16);

    let reply = pic
        .update_call(
            canister,
            controller,
            "route_and_extract_text",
            Encode!(&question.to_owned()).expect("encode text route"),
        )
        .expect("text route");
    let extraction: TextRouteAndExtract =
        decode_result(&reply, "route_and_extract_text").expect("text route");
    assert_eq!(extraction.probabilities.len(), 6);
    assert_eq!(extraction.token_count, count);
    for span in &extraction.spans {
        assert!(
            span.start < span.end as u32 && (span.end as usize) <= question.len(),
            "span {span:?} is outside the question text"
        );
    }

    // Upgrade: the pack is heap state and is not persisted; the tokenizer reloads from stable.
    pic.upgrade_canister(canister, wasm, Encode!(&()).unwrap(), Some(controller))
        .expect("upgrade canister");
    let reply = pic
        .query_call(
            canister,
            controller,
            "tokenizer_fingerprint",
            Encode!(&()).unwrap(),
        )
        .expect("fingerprint query");
    let reloaded: Option<TokenizerFingerprint> =
        Decode!(&reply, Option<TokenizerFingerprint>).expect("decode fingerprint");
    assert_eq!(reloaded.as_ref().map(|fp| fp.bytes), Some(tokenizer.len() as u64));
    let reply = pic
        .update_call(
            canister,
            controller,
            "tokenizer_token_count",
            Encode!(&question.to_owned()).expect("encode count"),
        )
        .expect("token count after upgrade");
    let (count_after, _): (u32, f32) =
        decode_result(&reply, "tokenizer_token_count").expect("count after upgrade");
    assert_eq!(count_after, count);
}
