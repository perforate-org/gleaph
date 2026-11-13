//! PocketIC E2E for ADR 0111 lazy provisioning: enabling inference on a graph makes the Router
//! issue a singleton `LogicalResource::Inference` through Provision, install the inference wasm
//! from the active release and bind it. Skips when the inference wasm is absent.

use candid::{CandidType, Decode, Principal};
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_graph_kernel::provisioning::init_args::RouterInitArgs;
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_pocket_ic_tests::{GRAPH_NAME, install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::types::{
    ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    ReleaseActivateArgs, ReleaseId, ReleasePublishArgs, sha256,
};
use gleaph_router::types::RegisterGraphArgs;
use serde::de::DeserializeOwned;

const PUBLISH_CHUNK: usize = 1024 * 1024;

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn optional_artifact(env_var: &str, fallback: &str) -> Option<Vec<u8>> {
    let path = std::env::var(env_var)
        .map(std::path::PathBuf::from)
        .ok()
        .filter(|path| path.exists())
        .or_else(|| {
            let path = workspace_root().join(fallback);
            path.exists().then_some(path)
        })?;
    Some(std::fs::read(path).expect("read artifact"))
}

fn call_with_error<'de, T, E, A>(
    pic: &pocket_ic::PocketIc,
    canister: Principal,
    caller: Principal,
    method: &str,
    arg: A,
) -> T
where
    A: CandidType,
    T: CandidType + DeserializeOwned,
    E: CandidType + DeserializeOwned + std::fmt::Debug,
{
    let bytes = pic
        .update_call(
            canister,
            caller,
            method,
            candid::encode_one(&arg).expect("encode"),
        )
        .unwrap_or_else(|error| panic!("{method}: {error}"));
    let result: Result<T, E> =
        Decode!(&bytes, Result<T, E>).unwrap_or_else(|error| panic!("decode {method}: {error}"));
    result.unwrap_or_else(|error| panic!("{method} rejected: {error:?}"))
}

fn call_provision_artifact<T: CandidType + DeserializeOwned>(
    pic: &pocket_ic::PocketIc,
    provision: Principal,
    admin: Principal,
    method: &str,
    arg: impl CandidType,
) -> T {
    call_with_error::<T, gleaph_provision::types::ArtifactError, _>(
        pic, provision, admin, method, arg,
    )
}

fn call_provision_release<T: CandidType + DeserializeOwned>(
    pic: &pocket_ic::PocketIc,
    provision: Principal,
    admin: Principal,
    method: &str,
    arg: impl CandidType,
) -> T {
    call_with_error::<T, gleaph_provision::types::ReleaseError, _>(
        pic, provision, admin, method, arg,
    )
}

fn call_router<T: CandidType + DeserializeOwned>(
    pic: &pocket_ic::PocketIc,
    router: Principal,
    admin: Principal,
    method: &str,
    arg: impl CandidType,
) -> T {
    call_with_error::<T, RouterError, _>(pic, router, admin, method, arg)
}

fn publish_artifact(
    pic: &pocket_ic::PocketIc,
    provision: Principal,
    admin: Principal,
    kind: CanisterKind,
    version: &str,
    wasm: &[u8],
) -> ArtifactId {
    let full_sha = sha256(wasm);
    let chunks: Vec<&[u8]> = wasm.chunks(PUBLISH_CHUNK).collect();
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|chunk| sha256(chunk)).collect();
    let id = ArtifactId::new(kind.clone(), version.to_owned(), full_sha);
    let _: gleaph_provision::types::ArtifactMetadata = call_provision_artifact(
        pic,
        provision,
        admin,
        "artifact_publish_metadata",
        ArtifactPublishMetadataArgs {
            canister_kind: kind,
            semantic_version: version.to_owned(),
            sha256: full_sha,
            byte_length: wasm.len() as u64,
            chunk_hashes: chunk_hashes.clone(),
        },
    );
    for (index, chunk) in chunks.iter().enumerate() {
        let _: gleaph_provision::types::ArtifactUpload = call_provision_artifact(
            pic,
            provision,
            admin,
            "artifact_upload_chunk",
            ArtifactUploadChunkArgs {
                artifact_id: id.clone(),
                chunk_index: index as u32,
                bytes: chunk.to_vec(),
            },
        );
    }
    id
}

#[test]
fn enabling_inference_provisions_the_singleton_inference_canister() {
    let Some(inference_wasm) = optional_artifact(
        "INFERENCE_WASM",
        "target/wasm32-unknown-unknown/release/gleaph_routing_canister.wasm",
    ) else {
        eprintln!("skipping: inference wasm not present");
        return;
    };

    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);
    let router = pic.create_canister();
    pic.add_cycles(router, 5_000_000_000_000);
    let provision = install_provision_canister(&pic, admin, &[router]);

    // Router born wired to Provision; make the admin its controller before install.
    pic.set_controllers(router, None, vec![admin])
        .expect("router controller");
    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        candid::encode_one(RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: Some(provision),
        })
        .expect("encode router init"),
        Some(admin),
    );

    // Publish one atomic release containing all six artifacts. Only the inference wasm is
    // installed by this test; the others may be dummy bytes but must be present and verified.
    let ids = vec![
        publish_artifact(
            &pic,
            provision,
            admin,
            CanisterKind::Router,
            "0.1.0",
            &wasm_bytes("ROUTER_WASM"),
        ),
        publish_artifact(
            &pic,
            provision,
            admin,
            CanisterKind::Graph,
            "0.1.0",
            &wasm_bytes("GRAPH_WASM"),
        ),
        publish_artifact(
            &pic,
            provision,
            admin,
            CanisterKind::PropertyIndex,
            "0.1.0",
            &wasm_bytes("INDEX_WASM"),
        ),
        publish_artifact(
            &pic,
            provision,
            admin,
            CanisterKind::VectorCanister,
            "0.1.0",
            &wasm_bytes("VECTOR_INDEX_WASM"),
        ),
        publish_artifact(
            &pic,
            provision,
            admin,
            CanisterKind::TextCanister,
            "0.1.0",
            b"text-unused",
        ),
        publish_artifact(
            &pic,
            provision,
            admin,
            CanisterKind::Inference,
            "0.1.0",
            &inference_wasm,
        ),
    ];
    let _: gleaph_provision::types::ReleaseManifest = call_provision_release(
        &pic,
        provision,
        admin,
        "release_publish",
        ReleasePublishArgs {
            release_id: ReleaseId("release-inference-0111".to_owned()),
            artifact_ids: ids,
        },
    );
    let _: gleaph_provision::types::ReleaseActivateResult = call_provision_release(
        &pic,
        provision,
        admin,
        "release_activate",
        ReleaseActivateArgs {
            release_id: ReleaseId("release-inference-0111".to_owned()),
        },
    );

    // Provision the graph through the same release, then opt it into inference.
    let _: () = call_router(
        &pic,
        router,
        admin,
        "register_graph",
        RegisterGraphArgs {
            graph_name: GRAPH_NAME.to_owned(),
            owner: admin,
            admins: Default::default(),
            is_home: false,
            shards: vec![],
            requested_resources: vec![ProvisionableResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
            }],
        },
    );

    // The first enable lazily provisions the deployment inference canister.
    let bound: Principal = call_router(&pic, router, admin, "enable_ask", GRAPH_NAME.to_owned());
    assert_ne!(bound, Principal::anonymous());

    // A second enable reuses the binding instead of provisioning again.
    let again: Principal = call_router(&pic, router, admin, "enable_ask", GRAPH_NAME.to_owned());
    assert_eq!(bound, again);
}
