//! PocketIC E2E for ADR 0114 slice 3: the Router builds a caller-visible context from the graph
//! type + prepared metadata and routes through the inference canister's general route.

use candid::{Decode, Deserialize, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    GRAPH_NAME, gql_mutate_as_admin, install_single_shard_federation, prepare_batch_as_admin,
};
use gleaph_prepared_api::{
    OperationKind, Parameter, PreparedOperation, PreparedRegistration, ResultSchema, SemanticType,
};
use gleaph_routing_wire::GeneralRouteResult;

const CHUNK: usize = 1_900_000;

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
        .then(|| std::fs::read(path).expect("read inference wasm"))
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

fn decode<T: candid::CandidType + for<'de> Deserialize<'de>>(
    bytes: &[u8],
    method: &str,
) -> Result<T, RouterError> {
    let result: Result<T, RouterError> = Decode!(bytes, Result<T, RouterError>)
        .unwrap_or_else(|error| panic!("decode {method}: {error}"));
    result
}

fn upload_tokenizer(
    pic: &pocket_ic::PocketIc,
    router: Principal,
    admin: Principal,
    tokenizer: &[u8],
) {
    for (index, chunk) in tokenizer.chunks(CHUNK).enumerate() {
        let offset = (index * CHUNK) as u64;
        let reply = pic
            .update_call(
                router,
                admin,
                "admin_install_tokenizer_chunk",
                Encode!(&offset, &chunk.to_vec()).expect("encode tokenizer chunk"),
            )
            .unwrap_or_else(|error| panic!("tokenizer chunk {index}: {error}"));
        let result: Result<(), RouterError> = Decode!(&reply, Result<(), RouterError>)
            .unwrap_or_else(|error| panic!("decode tokenizer chunk {index}: {error}"));
        result.unwrap_or_else(|error| panic!("tokenizer chunk {index}: {error:?}"));
    }
    let reply = pic
        .update_call(
            router,
            admin,
            "admin_install_tokenizer_finish",
            Encode!(&Option::<String>::None).expect("encode tokenizer finish"),
        )
        .expect("admin_install_tokenizer_finish");
    let result: Result<gleaph_routing_wire::TokenizerFingerprint, RouterError> =
        Decode!(&reply, Result<gleaph_routing_wire::TokenizerFingerprint, RouterError>)
            .expect("decode tokenizer finish");
    result.expect("tokenizer finish");
}

fn operation() -> PreparedOperation {
    PreparedOperation {
        name: "find-films".to_owned(),
        description: Some("Find films whose synopsis matches a text query.".to_owned()),
        kind: OperationKind::Query,
        parameters: vec![Parameter {
            name: "query".to_owned(),
            description: None,
            required: true,
            nullable: false,
            semantic_type: SemanticType::Text,
        }],
        result: ResultSchema {
            columns: vec![gleaph_prepared_api::Column {
                name: "title".to_owned(),
                semantic_type: SemanticType::Text,
                nullable: false,
            }],
        },
        supports_consistency: false,
        supports_idempotency: false,
        allowed_sorts: vec![],
        text_fusion: None,
    }
}

#[test]
fn router_builds_context_and_routes_general_catalog() {
    let (Some(wasm), Some(tokenizer)) = (inference_wasm(), tokenizer_bytes()) else {
        eprintln!("skipping: inference wasm or tokenizer not present");
        return;
    };
    let env = install_single_shard_federation();
    let pic = &env.pic;
    pic.set_controllers(env.router, None, vec![env.admin])
        .expect("router controller");

    // The default federation graph is untyped; bind an inline graph type to it for this test.
    gql_mutate_as_admin(
        &env,
        "CREATE OR REPLACE GRAPH gleaph.pocket_ic { NODE Film { title STRING, synopsis STRING }, NODE Person { name STRING }, DIRECTED EDGE ActedIn LABEL ACTED_IN CONNECTING (Person -> Film) }",
        "general-route-type-1",
    );

    prepare_batch_as_admin(
        &env,
        &[PreparedRegistration {
            name: "find-films".to_owned(),
            query: "MATCH (f:Film) WHERE f.synopsis CONTAINS $query RETURN f.title AS title"
                .to_owned(),
            metadata: Some(operation()),
        }],
    )
    .expect("prepared metadata");

    let inference = pic.create_canister();
    pic.add_cycles(inference, 100_000_000_000_000);
    pic.set_controllers(inference, None, vec![env.admin, env.router])
        .expect("inference controllers");
    pic.install_canister(inference, wasm, Encode!().unwrap(), Some(env.admin));
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_inference_canister",
            Encode!(&Some(inference)).unwrap(),
        )
        .expect("admin_set_inference_canister");
    decode::<()>(&reply, "admin_set_inference_canister").expect("bind inference");
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_enable_graph_inference",
            Encode!(&GRAPH_NAME.to_owned()).unwrap(),
        )
        .expect("admin_enable_graph_inference");
    decode::<Principal>(&reply, "admin_enable_graph_inference").expect("enable inference");
    upload_tokenizer(pic, env.router, env.admin, &tokenizer);

    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_general",
            Encode!(
                &GRAPH_NAME.to_owned(),
                &"Find films about artificial intelligence.".to_owned()
            )
            .expect("encode route_general"),
        )
        .expect("route_general");
    let result = decode::<GeneralRouteResult>(&reply, "route_general").expect("route_general");
    assert_eq!(result.operation.as_deref(), Some("find-films"));
    assert!(!result.abstain);
    assert!(result.slots.iter().any(|slot| slot.name == "query"));
    assert_eq!(result.context_fingerprint.len(), 32);
}

#[test]
fn router_abstains_on_out_of_scope_text() {
    let (Some(wasm), Some(tokenizer)) = (inference_wasm(), tokenizer_bytes()) else {
        eprintln!("skipping: inference wasm or tokenizer not present");
        return;
    };
    let env = install_single_shard_federation();
    let pic = &env.pic;
    pic.set_controllers(env.router, None, vec![env.admin])
        .expect("router controller");
    gql_mutate_as_admin(
        &env,
        "CREATE OR REPLACE GRAPH gleaph.pocket_ic { NODE Film { title STRING }, DIRECTED EDGE ActedIn LABEL ACTED_IN CONNECTING (Film -> Film) }",
        "general-route-type-2",
    );
    prepare_batch_as_admin(
        &env,
        &[PreparedRegistration {
            name: "find-films".to_owned(),
            query: "MATCH (f:Film) RETURN f.title AS title".to_owned(),
            metadata: Some(operation()),
        }],
    )
    .expect("prepared metadata");
    let inference = pic.create_canister();
    pic.add_cycles(inference, 100_000_000_000_000);
    pic.set_controllers(inference, None, vec![env.admin, env.router])
        .expect("inference controllers");
    pic.install_canister(inference, wasm, Encode!().unwrap(), Some(env.admin));
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_set_inference_canister",
            Encode!(&Some(inference)).unwrap(),
        )
        .expect("admin_set_inference_canister");
    decode::<()>(&reply, "admin_set_inference_canister").expect("bind inference");
    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "admin_enable_graph_inference",
            Encode!(&GRAPH_NAME.to_owned()).unwrap(),
        )
        .expect("admin_enable_graph_inference");
    decode::<Principal>(&reply, "admin_enable_graph_inference").expect("enable inference");
    upload_tokenizer(pic, env.router, env.admin, &tokenizer);

    let reply = pic
        .update_call(
            env.router,
            env.admin,
            "route_general",
            Encode!(
                &GRAPH_NAME.to_owned(),
                &"Delete the film document.".to_owned()
            )
            .expect("encode route_general"),
        )
        .expect("route_general");
    let result = decode::<GeneralRouteResult>(&reply, "route_general").expect("route_general");
    assert!(result.abstain);
    assert!(result.operation.is_none());
}
