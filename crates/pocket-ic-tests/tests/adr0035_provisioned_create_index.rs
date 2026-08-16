//! PocketIC E2E for ADR 0035 Slice 10: provisioned-mode `CREATE INDEX` provisions property index
//! canisters and assigns them to `index_cluster` + attaches the graph's shards.
//!
//! Flow: install Router + Provision with a bootstrap binding, publish a release containing a
//! PropertyIndex artifact, register a graph via the provisioned `register_graph` fold (creating an
//! indexless shard), then issue `index_vertex_property` (the admin DDL that routes through
//! `create_index`). The on-demand provision path provisions a PropertyIndex canister, assigns it to
//! `index_cluster`, and retrofit-attaches the shard. Dev mode (no `provision_canister`) is covered
//! by the router unit tests.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_pocket_ic_tests::{install_provision_canister, new_pocket_ic, wasm_bytes};
use gleaph_provision::types::{
    ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    DeploymentBinding, ReleaseActivateArgs, ReleaseId, ReleasePublishArgs,
};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::{RegisterGraphArgs, ShardRegistryEntry};
use std::collections::BTreeSet;

struct Env {
    pic: pocket_ic::PocketIc,
    admin: Principal,
    router: Principal,
    provision: Principal,
}

fn install_router_and_provision() -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    // Install Provision first so we know its principal for Router init. `deployment_id` derives
    // from the admin principal (ADR 0068), matching the Router's `provision_graph_flow` caller.
    let binding = DeploymentBinding {
        deployment_id: admin.to_text(),
        router_principal: router,
        governance_principal: admin,
        binding_version: 1,
        bootstrap_principal: None,
    };
    let provision = install_provision_canister(&pic, binding);
    // The provision canister stores artifacts + release and pays `create_canister` cycles; top it
    // up well beyond the 2T seed so memory growth and canister creation never run dry.
    pic.add_cycles(provision, 100_000_000_000_000);

    pic.install_canister(
        router,
        wasm_bytes("ROUTER_WASM"),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: Some(provision),
        })
        .expect("encode router init"),
        None,
    );

    Env {
        pic,
        admin,
        router,
        provision,
    }
}

/// A tiny valid WebAssembly module with `canister_init` and `memory` exports, suitable for a real
/// `install_chunked_code` call in PocketIC without exceeding the 1 MiB chunk budget.
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

#[allow(clippy::result_large_err)]
fn call_artifact_publish(
    env: &Env,
    caller: Principal,
    args: &ArtifactPublishMetadataArgs,
) -> Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "artifact_publish_metadata",
            Encode!(args).expect("encode artifact_publish_metadata"),
        )
        .unwrap_or_else(|e| panic!("artifact_publish_metadata on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ArtifactMetadata, gleaph_provision::types::ArtifactError>
    )
    .expect("decode artifact_publish_metadata response")
}

#[allow(clippy::result_large_err)]
fn call_artifact_upload(
    env: &Env,
    caller: Principal,
    args: &ArtifactUploadChunkArgs,
) -> Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "artifact_upload_chunk",
            Encode!(args).expect("encode artifact_upload_chunk"),
        )
        .unwrap_or_else(|e| panic!("artifact_upload_chunk on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ArtifactUpload, gleaph_provision::types::ArtifactError>
    )
    .expect("decode artifact_upload_chunk response")
}

fn call_release_publish(
    env: &Env,
    caller: Principal,
    args: &ReleasePublishArgs,
) -> Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "release_publish",
            Encode!(args).expect("encode release_publish"),
        )
        .unwrap_or_else(|e| panic!("release_publish on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ReleaseManifest, gleaph_provision::types::ReleaseError>
    )
    .expect("decode release_publish response")
}

fn call_release_activate(
    env: &Env,
    caller: Principal,
    args: &ReleaseActivateArgs,
) -> Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            caller,
            "release_activate",
            Encode!(args).expect("encode release_activate"),
        )
        .unwrap_or_else(|e| panic!("release_activate on provision: {e:?}"));
    Decode!(
        &bytes,
        Result<gleaph_provision::types::ReleaseActivateResult, gleaph_provision::types::ReleaseError>
    )
    .expect("decode release_activate response")
}

#[allow(clippy::result_large_err)]
fn publish_valid_artifact(env: &Env, kind: CanisterKind, version: &str) -> ArtifactId {
    let wasm = minimal_canister_wasm();
    let full_sha = gleaph_provision::types::sha256(&wasm);
    let chunk_hash = gleaph_provision::types::sha256(&wasm);
    let id = ArtifactId::new(kind.clone(), version.to_owned(), full_sha);
    call_artifact_publish(
        env,
        env.admin,
        &ArtifactPublishMetadataArgs {
            canister_kind: kind,
            semantic_version: version.to_owned(),
            sha256: full_sha,
            byte_length: wasm.len() as u64,
            chunk_hashes: vec![chunk_hash],
        },
    )
    .expect("artifact_publish_metadata");
    call_artifact_upload(
        env,
        env.admin,
        &ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: wasm,
        },
    )
    .expect("artifact_upload_chunk");
    id
}

/// Publish all four release kinds and activate the release so the Provision canister can install a
/// PropertyIndex canister on demand.
fn activate_release(env: &Env, release_id: &str) {
    let ids = vec![
        publish_valid_artifact(env, CanisterKind::Router, "0.1.0"),
        publish_valid_artifact(env, CanisterKind::Graph, "0.1.0"),
        publish_valid_artifact(env, CanisterKind::PropertyIndex, "0.1.0"),
        publish_valid_artifact(env, CanisterKind::VectorCanister, "0.1.0"),
    ];
    call_release_publish(
        env,
        env.admin,
        &ReleasePublishArgs {
            release_id: ReleaseId(release_id.to_owned()),
            artifact_ids: ids,
        },
    )
    .expect("release_publish");
    call_release_activate(
        env,
        env.admin,
        &ReleaseActivateArgs {
            release_id: ReleaseId(release_id.to_owned()),
        },
    )
    .expect("release_activate");
}

/// Register a graph through the provisioned `register_graph` fold, requesting a single GraphShard.
/// The created shard is indexless (no PropertyIndex in the batch).
fn register_graph(env: &Env, graph_name: &str) {
    let intent = RegisterGraphArgs {
        graph_name: graph_name.to_owned(),
        owner: env.admin,
        admins: BTreeSet::new(),
        is_home: false,
        shards: vec![],
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "register_graph",
            Encode!(&intent).expect("encode register_graph"),
        )
        .unwrap_or_else(|e| panic!("register_graph on router: {e:?}"));
    let result: Result<(), RouterError> =
        Decode!(&bytes, Result<(), RouterError>).expect("decode register_graph");
    assert!(result.is_ok(), "register_graph must succeed: {result:?}");
}

fn ensure_vertex_label(env: &Env, graph_name: &str, label: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_vertex_label",
            Encode!(&graph_name.to_string(), &label.to_string())
                .expect("encode ensure_vertex_label"),
        )
        .unwrap_or_else(|e| panic!("ensure_vertex_label on router: {e:?}"));
    let result: Result<VertexLabelId, RouterError> =
        Decode!(&bytes, Result<VertexLabelId, RouterError>).expect("decode ensure_vertex_label");
    assert!(
        result.is_ok(),
        "ensure_vertex_label must succeed: {result:?}"
    );
}

fn ensure_property(env: &Env, graph_name: &str, property: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_properties",
            Encode!(&graph_name.to_string(), &vec![property.to_string()])
                .expect("encode ensure_properties"),
        )
        .unwrap_or_else(|e| panic!("ensure_properties on router: {e:?}"));
    let result: Result<Vec<PropertyId>, RouterError> =
        Decode!(&bytes, Result<Vec<PropertyId>, RouterError>).expect("decode ensure_properties");
    assert!(result.is_ok(), "ensure_properties must succeed: {result:?}");
}

/// Issue the admin DDL that routes through `create_index` (and thus the on-demand provision path).
fn index_vertex_property(env: &Env, graph_name: &str, label: &str, property: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "index_vertex_property",
            Encode!(
                &graph_name.to_string(),
                &label.to_string(),
                &property.to_string()
            )
            .expect("encode index_vertex_property"),
        )
        .unwrap_or_else(|e| panic!("index_vertex_property on router: {e:?}"));
    let result: Result<(), RouterError> =
        Decode!(&bytes, Result<(), RouterError>).expect("decode index_vertex_property");
    assert!(
        result.is_ok(),
        "index_vertex_property must succeed: {result:?}"
    );
}

fn list_shards(env: &Env, graph_name: &str) -> Vec<ShardRegistryEntry> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_shards",
            Encode!(&graph_name.to_string()).expect("encode list_shards"),
        )
        .expect("list_shards");
    match Decode!(&bytes, Result<Vec<ShardRegistryEntry>, RouterError>) {
        Ok(Ok(shards)) => shards,
        Ok(Err(err)) => panic!("list_shards rejected: {err:?}"),
        Err(err) => panic!("decode list_shards: {err}"),
    }
}

#[test]
fn provisioned_create_index_assigns_index_cluster_and_attaches_shard() {
    let env = install_router_and_provision();
    activate_release(&env, "release-slice-10");
    let graph_name = "slice10.graph";
    register_graph(&env, graph_name);
    ensure_vertex_label(&env, graph_name, "Person");
    ensure_property(&env, graph_name, "age");

    // The graph shard is indexless before CREATE INDEX.
    let before = list_shards(&env, graph_name);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].index_canister, Principal::anonymous());

    // CREATE INDEX (via the admin DDL) provisions a PropertyIndex canister and assigns it.
    index_vertex_property(&env, graph_name, "Person", "age");

    let after = list_shards(&env, graph_name);
    assert_eq!(after.len(), 1);
    assert_ne!(
        after[0].index_canister,
        Principal::anonymous(),
        "CREATE INDEX must assign a provisioned index canister to the shard"
    );
    assert!(
        after[0].index_attached,
        "the shard must remain index-attached after retrofit"
    );

    let index_canister = after[0].index_canister;
    let index_status = env
        .pic
        .canister_status(index_canister, Some(env.admin))
        .expect("provisioned index canister status");
    assert!(
        index_status.cycles > 0u128,
        "newly provisioned canister must retain an initial cycle balance"
    );
    assert_eq!(
        index_status.settings.controllers,
        vec![env.provision, env.admin],
        "newly provisioned canister must be controlled by [Provision, governance]"
    );
}
