//! PocketIC E2E for plan 0297 slice 1: `CREATE TEXT INDEX` provisioning through the ADR 0035
//! issuance protocol (standalone `TextIndex` resource, no GraphShard in the request).
//!
//! Flow: install Router + Provision with a bootstrap binding, publish a release containing all
//! five artifacts (the TextCanister artifact is the REAL `text-canister` wasm so the install
//! exercises its init-args contract), register an indexless graph, then issue the admin TEXT
//! DDL endpoint. The on-demand path provisions a text canister with the Router wired as its
//! controller and registers the definition as `Ready`.
//!
//! Scenarios (one bootstrap): (a) issue → canister created + definition registered ready;
//! (b) identical re-issue → same canister id, no second creation; (c) anonymous caller rejected.
//!
//! Run note: when `POCKET_IC_SKIP_FEDERATION_WASM=1` is set (federation sources mid-change),
//! this target self-builds the router/provision wasms in an isolated target dir, mirroring the
//! `text_index_lifecycle` escape hatch. Artifact paths may be supplied via
//! `ROUTER_WASM` / `PROVISION_WASM` / `TEXT_INDEX_WASM`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_pocket_ic_tests::new_pocket_ic;
use gleaph_provision::types::{
    ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    DeploymentBinding, ReleaseActivateArgs, ReleaseId, ReleasePublishArgs, sha256,
};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::{RegisterGraphArgs, TextIndexInfo, TextIndexStatusView};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const GRAPH_NAME: &str = "text0297.graph";
const INDEX_NAME: &str = "doc_title_text_idx";
/// Matches the shared 1 MiB install-chunk bound (`MAX_INSTALL_CHUNK_BYTES`) in Provision.
const PUBLISH_CHUNK_BYTES: usize = 1024 * 1024;

struct Env {
    pic: pocket_ic::PocketIc,
    admin: Principal,
    router: Principal,
    provision: Principal,
}

// -- Wasm acquisition -------------------------------------------------------------------------

/// Reads a wasm artifact from `env_var` when set (the build.rs-managed fast path), otherwise
/// builds the named packages in an isolated target dir so this target also runs under
/// `POCKET_IC_SKIP_FEDERATION_WASM=1`. Raw cargo output is installed directly (the shared
/// postprocess step adds deploy metadata this E2E does not read).
fn ensure_wasm(env_var: &str, packages: &[&str], features: &[&str], cache_dir: &str) -> Vec<u8> {
    if let Ok(path) = std::env::var(env_var) {
        return std::fs::read(&path).unwrap_or_else(|e| panic!("read {env_var} {}: {e}", path));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("workspace root above crates/");
    let target_dir = workspace_root.join("target").join(cache_dir);
    let mut args = vec![
        "build".to_owned(),
        "--release".to_owned(),
        "--target".to_owned(),
        "wasm32-unknown-unknown".to_owned(),
    ];
    for package in packages {
        args.push("--package".to_owned());
        args.push((*package).to_owned());
    }
    if !features.is_empty() {
        args.push("--features".to_owned());
        args.push(features.join(","));
    }
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(&args)
        .status()
        .expect("spawn cargo build for PocketIC wasm");
    assert!(status.success(), "wasm build for {packages:?} failed");
    let artifact = match packages {
        ["gleaph-router"] => "gleaph_router.wasm",
        ["gleaph-provision"] => "gleaph_provision.wasm",
        ["text-canister"] => "text_canister.wasm",
        other => panic!("unexpected single-artifact package set {other:?}"),
    };
    let wasm_path = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(artifact);
    std::fs::read(&wasm_path).unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()))
}

fn router_wasm() -> Vec<u8> {
    ensure_wasm(
        "ROUTER_WASM",
        &["gleaph-router"],
        &["gleaph-router/pocket-ic-e2e"],
        "pocket-ic-text-provision-wasm",
    )
}

fn provision_wasm() -> Vec<u8> {
    ensure_wasm(
        "PROVISION_WASM",
        &["gleaph-provision"],
        &[],
        "pocket-ic-text-provision-wasm",
    )
}

fn text_wasm() -> Vec<u8> {
    // Same cache directory as `text_index_lifecycle` so both targets share one build.
    ensure_wasm(
        "TEXT_INDEX_WASM",
        &["text-canister"],
        &[],
        "pocket-ic-text-wasm",
    )
}

// -- Bootstrap --------------------------------------------------------------------------------

fn bootstrap() -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    // `deployment_id` derives from the admin principal (ADR 0068), matching the Router's
    // issuance caller.
    let binding = DeploymentBinding {
        deployment_id: admin.to_text(),
        router_principal: router,
        governance_principal: admin,
        binding_version: 1,
        bootstrap_principal: None,
    };
    let provision_canister = pic.create_canister();
    pic.add_cycles(provision_canister, 100_000_000_000_000);
    pic.install_canister(
        provision_canister,
        provision_wasm(),
        Encode!(&gleaph_provision::canister::init::ProvisionInitArgs {
            bootstrap_bindings: vec![binding],
        })
        .expect("encode provision init"),
        None,
    );

    pic.install_canister(
        router,
        router_wasm(),
        Encode!(&RouterInitArgs {
            issuing_principal: admin,
            initial_admins: vec![],
            provision_canister: Some(provision_canister),
        })
        .expect("encode router init"),
        None,
    );

    Env {
        pic,
        admin,
        router,
        provision: provision_canister,
    }
}

// -- Release publication ----------------------------------------------------------------------

#[allow(clippy::result_large_err)]
fn call_on_provision<
    R: candid::CandidType + serde::de::DeserializeOwned,
    E: candid::CandidType + serde::de::DeserializeOwned,
>(
    env: &Env,
    method: &str,
    args: &impl candid::CandidType,
) -> Result<R, E> {
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            method,
            Encode!(args).expect("encode args"),
        )
        .unwrap_or_else(|e| panic!("{method} on provision: {e:?}"));
    Decode!(&bytes, Result<R, E>).expect("decode provision response")
}

/// Publish one verified artifact split into bounded chunks (mirrors the outbound-envelope
/// harness's publish helper, chunked to stay within Provision's install-chunk budget).
fn publish_verified_artifact(
    env: &Env,
    kind: CanisterKind,
    version: &str,
    wasm: &[u8],
) -> ArtifactId {
    let full_sha = sha256(wasm);
    let chunks: Vec<&[u8]> = if wasm.len() <= PUBLISH_CHUNK_BYTES {
        vec![wasm]
    } else {
        wasm.chunks(PUBLISH_CHUNK_BYTES).collect()
    };
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|c| sha256(c)).collect();
    let id = ArtifactId::new(kind.clone(), version.to_owned(), full_sha);
    let _: gleaph_provision::types::ArtifactMetadata =
        call_on_provision::<_, gleaph_provision::types::ArtifactError>(
            env,
            "artifact_publish_metadata",
            &ArtifactPublishMetadataArgs {
                canister_kind: kind,
                semantic_version: version.to_owned(),
                sha256: full_sha,
                byte_length: wasm.len() as u64,
                chunk_hashes: chunk_hashes.clone(),
            },
        )
        .map_err(|e| panic!("artifact_publish_metadata rejected: {e:?}"))
        .expect("metadata ok");
    for (index, chunk) in chunks.iter().enumerate() {
        let _: gleaph_provision::types::ArtifactUpload =
            call_on_provision::<_, gleaph_provision::types::ArtifactError>(
                env,
                "artifact_upload_chunk",
                &ArtifactUploadChunkArgs {
                    artifact_id: id.clone(),
                    chunk_index: index as u32,
                    bytes: chunk.to_vec(),
                },
            )
            .map_err(|e| panic!("artifact_upload_chunk rejected: {e:?}"))
            .expect("upload ok");
    }
    id
}

/// Publish all five release kinds (Text carries the real text-canister wasm) and activate.
fn activate_release(env: &Env) {
    let text_wasm = text_wasm();
    let ids = vec![
        publish_verified_artifact(
            env,
            CanisterKind::Router,
            "0.1.0",
            &[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00],
        ),
        publish_verified_artifact(
            env,
            CanisterKind::Graph,
            "0.1.0",
            &[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00],
        ),
        publish_verified_artifact(
            env,
            CanisterKind::PropertyIndex,
            "0.1.0",
            &[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00],
        ),
        publish_verified_artifact(
            env,
            CanisterKind::VectorCanister,
            "0.1.0",
            &[0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00],
        ),
        publish_verified_artifact(env, CanisterKind::TextCanister, "0.1.0", &text_wasm),
    ];
    let _: gleaph_provision::types::ReleaseManifest =
        call_on_provision::<_, gleaph_provision::types::ReleaseError>(
            env,
            "release_publish",
            &ReleasePublishArgs {
                release_id: ReleaseId("release-text-0297".to_owned()),
                artifact_ids: ids,
            },
        )
        .map_err(|e| panic!("release_publish rejected: {e:?}"))
        .expect("publish ok");
    let _: gleaph_provision::types::ReleaseActivateResult =
        call_on_provision::<_, gleaph_provision::types::ReleaseError>(
            env,
            "release_activate",
            &ReleaseActivateArgs {
                release_id: ReleaseId("release-text-0297".to_owned()),
            },
        )
        .map_err(|e| panic!("release_activate rejected: {e:?}"))
        .expect("activate ok");
}

// -- Router helpers ---------------------------------------------------------------------------

fn register_graph(env: &Env) {
    let intent = RegisterGraphArgs {
        graph_name: GRAPH_NAME.to_owned(),
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

fn ensure_vertex_label(env: &Env, label: &str) -> VertexLabelId {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_vertex_label",
            Encode!(&GRAPH_NAME.to_string(), &label.to_string()).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("ensure_vertex_label on router: {e:?}"));
    Decode!(
        &bytes,
        Result<VertexLabelId, RouterError>
    )
    .expect("decode ensure_vertex_label")
    .expect("ensure_vertex_label ok")
}

fn ensure_property(env: &Env, property: &str) -> PropertyId {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_properties",
            Encode!(&GRAPH_NAME.to_string(), &vec![property.to_string()]).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("ensure_properties on router: {e:?}"));
    Decode!(&bytes, Result<Vec<PropertyId>, RouterError>)
        .expect("decode ensure_properties")
        .expect("ensure_properties ok")
        .into_iter()
        .next()
        .expect("one property id")
}

fn create_text_index(
    env: &Env,
    caller: Principal,
    index_name: &str,
    label: &str,
    property: &str,
) -> Result<TextIndexInfo, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "create_text_index",
            Encode!(
                &GRAPH_NAME.to_string(),
                &index_name.to_string(),
                &label.to_string(),
                &property.to_string()
            )
            .expect("encode create_text_index"),
        )
        .unwrap_or_else(|e| panic!("create_text_index on router: {e:?}"));
    Decode!(&bytes, Result<TextIndexInfo, RouterError>).expect("decode create_text_index")
}

fn get_text_index(
    env: &Env,
    caller: Principal,
    index_name: &str,
) -> Result<TextIndexInfo, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "get_text_index",
            Encode!(&GRAPH_NAME.to_string(), &index_name.to_string())
                .expect("encode get_text_index"),
        )
        .unwrap_or_else(|e| panic!("get_text_index on router: {e:?}"));
    Decode!(&bytes, Result<TextIndexInfo, RouterError>).expect("decode get_text_index")
}

/// Controller-guarded `admin_flush` on the provisioned canister, called as `from`.
fn admin_flush_as(env: &Env, from: Principal) -> text_canister::FlushReport {
    let bytes = env
        .pic
        .update_call(
            first_created_canister(env),
            from,
            "admin_flush",
            Encode!(&()).expect("encode admin_flush"),
        )
        .unwrap_or_else(|e| panic!("admin_flush: {e:?}"));
    Decode!(&bytes, text_canister::FlushReport).expect("decode admin_flush")
}

fn first_created_canister(env: &Env) -> Principal {
    get_text_index(env, env.admin, INDEX_NAME)
        .expect("definition registered")
        .canister
        .expect("provisioned canister attached")
}

// -- Scenarios --------------------------------------------------------------------------------

/// One bootstrap serves all three scenarios in order.
#[test]
fn text_index_provisions_replays_and_guards() {
    let env = bootstrap();
    activate_release(&env);
    register_graph(&env);
    ensure_vertex_label(&env, "Document");
    ensure_property(&env, "title");

    // --- (a) issue → canister created with Text kind + definition registered + ready ---
    let info = create_text_index(&env, env.admin, INDEX_NAME, "Document", "title")
        .expect("issue must succeed");
    let canister = info.canister.expect("provisioned canister attached");
    assert_ne!(canister, Principal::anonymous());
    assert_eq!(info.status, TextIndexStatusView::Ready);
    assert_eq!(
        info.analyzer_id,
        text_canister::ANALYZER_ID,
        "the v1 admission pins the production analyzer"
    );

    // The definition is durably registered and readable through the query surface.
    assert_eq!(
        get_text_index(&env, env.admin, INDEX_NAME).expect("stored"),
        info
    );

    // The created canister is controlled by [Provision, governance] (ADR 0035 convention) and
    // runs the real text-canister wasm.
    let status = env
        .pic
        .canister_status(canister, Some(env.admin))
        .expect("provisioned text canister status");
    assert!(
        status.cycles > 0u128,
        "newly provisioned canister must retain an initial cycle balance"
    );
    assert_eq!(
        status.settings.controllers,
        vec![env.provision, env.admin],
        "newly provisioned canister must be controlled by [Provision, governance]"
    );
    let stats_bytes = env
        .pic
        .query_call(
            canister,
            env.admin,
            "get_stats",
            Encode!(&()).expect("encode"),
        )
        .expect("get_stats");
    let stats: text_canister::TextIndexStats =
        Decode!(&stats_bytes, text_canister::TextIndexStats).expect("decode get_stats");
    assert_eq!(
        stats.analyzer_id,
        text_canister::ANALYZER_ID,
        "the installed wasm is the real text canister"
    );

    // The Router principal was wired as controller through Provision's install args: the
    // guarded admin surface accepts the Router and rejects everyone else.
    let flushed = admin_flush_as(&env, env.router);
    assert!(flushed.done, "empty pending log flushes to done");
    let denied = env
        .pic
        .update_call(
            first_created_canister(&env),
            env.admin,
            "admin_flush",
            Encode!(&()).expect("encode admin_flush"),
        )
        .err()
        .expect("non-controller must be denied");
    assert!(
        denied
            .reject_message
            .contains("is not the text index controller"),
        "unexpected denial reason: {}",
        denied.reject_message
    );

    // --- (b) identical re-issue → same canister id, no second creation ---
    let replay = create_text_index(&env, env.admin, INDEX_NAME, "Document", "title")
        .expect("identical re-issue is a no-op returning the existing resource");
    assert_eq!(replay.text_index_id, info.text_index_id);
    assert_eq!(replay.canister, Some(canister), "same canister id");
    assert_eq!(replay.status, TextIndexStatusView::Ready);
    assert_eq!(
        get_text_index(&env, env.admin, INDEX_NAME).expect("single definition"),
        info,
        "no second creation: the original row survives unchanged"
    );

    // --- (c) anonymous caller rejected per guard conventions ---
    let err = create_text_index(
        &env,
        Principal::anonymous(),
        "anon_text_idx",
        "Document",
        "title",
    )
    .expect_err("anonymous caller must be rejected");
    assert!(
        matches!(err, RouterError::Forbidden),
        "expected Forbidden, got {err:?}"
    );
}
