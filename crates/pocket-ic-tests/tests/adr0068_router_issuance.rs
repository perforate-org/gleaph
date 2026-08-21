//! PocketIC E2E for ADR 0068 lazy Router issuance through the Provision artifact catalog.
//!
//! Proves that `LogicalResource::Router` via `accept_envelope` installs a real, functioning Router
//! canister: the Account authorizes the first-Router issuance, Provision publishes the Router
//! artifact + release, installs a Router from it, and the issued Router responds to a query. This
//! is the production path that replaces the dev-mode manual `gleaph deploy` install.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse;
use gleaph_pocket_ic_tests::{install_account_canister, install_provision_canister, new_pocket_ic};
use gleaph_provision::types::{
    sha256, ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    DeploymentBinding, ReleaseActivateArgs, ReleaseId, ReleasePublishArgs,
};

const CHUNK_SIZE: usize = 1024 * 1024;

struct Env {
    pic: pocket_ic::PocketIc,
    user: Principal,
    admin: Principal,
    account: Principal,
    provision: Principal,
}

fn user_principal() -> Principal {
    Principal::from_slice(&[0x30; 29])
}

fn admin_principal() -> Principal {
    Principal::from_slice(&[0x64; 29])
}

/// Install Account + Provision with the user's Account as the bootstrap trust subject. The Router
/// principal is unknown before issuance (it is created by the Provision artifact install), so the
/// binding's `router_principal` is the account principal as a placeholder; only the bootstrap
/// principal matters for the first issuance.
fn env() -> Env {
    let pic = new_pocket_ic();
    let user = user_principal();
    let admin = admin_principal();
    let account = install_account_canister(&pic);
    let binding = DeploymentBinding {
        deployment_id: user.to_text(),
        router_principal: account,
        governance_principal: admin,
        binding_version: 1,
        bootstrap_principal: Some(account),
    };
    let provision = install_provision_canister(&pic, binding);
    pic.add_cycles(provision, 100_000_000_000_000);
    Env {
        pic,
        user,
        admin,
        account,
        provision,
    }
}

/// A tiny valid WebAssembly module with `canister_init` and `memory` exports, usable as a dummy
/// artifact for the three release kinds that this test never installs.
fn minimal_canister_wasm() -> Vec<u8> {
    vec![
        0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, // magic + version
        0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: () -> ()
        0x03, 0x02, 0x01, 0x00, // function section
        0x05, 0x03, 0x01, 0x00, 0x01, // memory section: 1 page
        0x07, 0x1A, // export section
        0x02, // 2 exports
        0x0D, 0x63, 0x61, 0x6E, 0x69, 0x73, 0x74, 0x65, 0x72, 0x5F, 0x69, 0x6E, 0x69, 0x74, 0x00,
        0x00, // func export "canister_init"
        0x06, 0x6D, 0x65, 0x6D, 0x6F, 0x72, 0x79, 0x02, 0x00, // memory export "memory"
        0x0A, 0x04, 0x01, 0x02, 0x00, 0x0B, // code section
    ]
}

/// Publish one single-chunk artifact (metadata + upload) into the Provision store. The caller
/// passes a wasm whose SHA-256 is recorded; the release manifest then references this id.
fn publish_artifact(env: &Env, kind: CanisterKind, version: &str, wasm: &[u8]) -> ArtifactId {
    let full_sha = sha256(wasm);
    let id = ArtifactId::new(kind.clone(), version.to_owned(), full_sha);
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_publish_metadata",
            Encode!(&ArtifactPublishMetadataArgs {
                canister_kind: kind,
                semantic_version: version.to_owned(),
                sha256: full_sha,
                byte_length: wasm.len() as u64,
                chunk_hashes: vec![full_sha],
            })
            .expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_publish_metadata");
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_upload_chunk",
            Encode!(&ArtifactUploadChunkArgs {
                artifact_id: id.clone(),
                chunk_index: 0,
                bytes: wasm.to_vec(),
            })
            .expect("encode artifact_upload_chunk"),
        )
        .unwrap_or_else(|e| panic!("artifact_upload_chunk: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_upload_chunk");
    id
}

/// Publish the real Router artifact and activate a release so the Provision canister can install
/// a Router on demand.
fn activate_release(env: &Env) {
    let router_wasm = gleaph_pocket_ic_tests::wasm_bytes("ROUTER_WASM");
    let full_sha = sha256(&router_wasm);
    let mut chunk_hashes = Vec::new();
    let mut offset = 0;
    while offset < router_wasm.len() {
        let end = (offset + CHUNK_SIZE).min(router_wasm.len());
        chunk_hashes.push(sha256(&router_wasm[offset..end]));
        offset = end;
    }
    let router_id = ArtifactId::new(CanisterKind::Router, "0.1.0".to_owned(), full_sha);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "artifact_publish_metadata",
            Encode!(&ArtifactPublishMetadataArgs {
                canister_kind: CanisterKind::Router,
                semantic_version: "0.1.0".to_owned(),
                sha256: full_sha,
                byte_length: router_wasm.len() as u64,
                chunk_hashes: chunk_hashes.clone(),
            })
            .expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata: {e:?}"));
    let _: Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> =
        Decode!(&bytes, Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>)
            .expect("decode artifact_publish_metadata");

    // Upload each chunk individually.
    let mut offset = 0;
    let mut chunk_index = 0u32;
    while offset < router_wasm.len() {
        let end = (offset + CHUNK_SIZE).min(router_wasm.len());
        let bytes = env
            .pic
            .update_call(
                env.provision,
                env.admin,
                "artifact_upload_chunk",
                Encode!(&ArtifactUploadChunkArgs {
                    artifact_id: router_id.clone(),
                    chunk_index,
                    bytes: router_wasm[offset..end].to_vec(),
                })
                .expect("encode artifact_upload_chunk"),
            )
            .unwrap_or_else(|e| panic!("artifact_upload_chunk: {e:?}"));
        let _: Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError> =
            Decode!(&bytes, Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError>)
                .expect("decode artifact_upload_chunk");
        offset = end;
        chunk_index += 1;
    }

    // Publish the release with all four kinds (the manifest requires them). The other three kinds
    // use a minimal valid wasm that is never installed by this test.
    let minimal = minimal_canister_wasm();
    let minimal_sha = sha256(&minimal);
    let graph_id = ArtifactId::new(CanisterKind::Graph, "0.1.0".to_owned(), minimal_sha);
    publish_artifact(env, CanisterKind::Graph, "0.1.0", &minimal);
    let prop_id = ArtifactId::new(CanisterKind::PropertyIndex, "0.1.0".to_owned(), minimal_sha);
    publish_artifact(env, CanisterKind::PropertyIndex, "0.1.0", &minimal);
    let vec_id = ArtifactId::new(
        CanisterKind::VectorCanister,
        "0.1.0".to_owned(),
        minimal_sha,
    );
    publish_artifact(env, CanisterKind::VectorCanister, "0.1.0", &minimal);

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "release_publish",
            Encode!(&ReleasePublishArgs {
                release_id: ReleaseId("release-router".to_owned()),
                artifact_ids: vec![router_id, graph_id, prop_id, vec_id],
            })
            .expect("encode release_publish"),
        )
        .unwrap_or_else(|e| panic!("release_publish: {e:?}"));
    let _: Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError> =
        Decode!(&bytes, Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError>)
            .expect("decode release_publish");

    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "release_activate",
            Encode!(&ReleaseActivateArgs {
                release_id: ReleaseId("release-router".to_owned()),
            })
            .expect("encode release_activate"),
        )
        .unwrap_or_else(|e| panic!("release_activate: {e:?}"));
    let result: Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError> =
        Decode!(&bytes, Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError>)
            .expect("decode release_activate");
    assert!(result.is_ok(), "release_activate must succeed: {result:?}");
}

#[test]
fn router_issuance_installs_a_functioning_router() {
    let env = env();
    activate_release(&env);

    // User registers a Personal account.
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "create_account",
            Encode!(&"alice".to_owned()).unwrap(),
        )
        .expect("create_account call");
    let created: Result<gleaph_account::types::Account, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<gleaph_account::types::Account, gleaph_account::types::AccountError>)
            .expect("decode create_account");
    let account_id = created.expect("create_account").id();

    // User authorizes the first-Router issuance (Account -> Provision accept_envelope). The
    // endpoint takes three separate Candid arguments.
    let bytes = env
        .pic
        .update_call(
            env.account,
            env.user,
            "authorize_router_issuance",
            candid::encode_args((&account_id.clone(), &"default".to_owned(), &env.provision))
                .expect("encode authorize_router_issuance args"),
        )
        .expect("authorize_router_issuance call");
    let result: Result<ProvisionAcceptResponse, gleaph_account::types::AccountError> =
        Decode!(&bytes, Result<ProvisionAcceptResponse, gleaph_account::types::AccountError>)
            .expect("decode authorize_router_issuance");
    let response = result.expect("authorize_router_issuance");
    let created_resources = match &response {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => created_resources,
        _ => panic!("expected Accepted, got {response:?}"),
    };
    assert!(
        created_resources.len() == 1,
        "must create exactly one Router, got {created_resources:?}"
    );
    let router_canister = created_resources[0].canister_id;
    assert!(
        matches!(
            created_resources[0].logical_resource,
            gleaph_graph_kernel::provisioning::LogicalResource::Router
        ),
        "created resource must be the Router, got {:?}",
        created_resources[0].logical_resource
    );

    // The issued Router must be a live canister that answers the `whoami` query.
    let bytes = env
        .pic
        .query_call(router_canister, env.user, "whoami", Encode!(&()).unwrap())
        .expect("whoami on issued Router");
    let caller: Principal = Decode!(&bytes, Principal).expect("decode whoami");
    assert_eq!(caller, env.user, "issued Router must respond to whoami");
}
