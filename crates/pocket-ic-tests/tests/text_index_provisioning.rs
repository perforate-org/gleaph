//! PocketIC E2E for plan 0297 slice 1: `CREATE TEXT INDEX` provisioning through the ADR 0035
//! issuance protocol (standalone `TextIndex` resource, no GraphShard in the request).
//!
//! Flow: install Router + Provision with a bootstrap grant, publish a release containing all
//! five artifacts (the TextCanister artifact is the REAL `text-canister` wasm so the install
//! exercises its init-args contract), register a graph with a provision-issued shard (the real
//! graph + index wasms are reinstalled on the issued canisters), then issue the admin TEXT
//! DDL endpoint. The on-demand path provisions a text canister with the Router wired as its
//! controller and registers the definition born `Backfilling` (ADR 0059: planner-invisible
//! until the migration ledger's convergence proof flips it to `Ready`).
//!
//! Scenarios (one bootstrap): (a) issue → canister created + definition registered Backfilling,
//! then the migration lane drives it to Applied/Ready; (b) identical re-issue → same canister
//! id, no second creation; (c) anonymous caller rejected.
//!
//! Run note: when `POCKET_IC_SKIP_FEDERATION_WASM=1` is set (federation sources mid-change),
//! this target self-builds the router/provision wasms in an isolated target dir, mirroring the
//! `text_index_lifecycle` escape hatch. Artifact paths may be supplied via
//! `ROUTER_WASM` / `PROVISION_WASM` / `TEXT_INDEX_WASM`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
    SchemaMigrationProgressPhase, SchemaMigrationRecord, SchemaMigrationRecordState,
    SchemaMigrationRecordV1,
};
use gleaph_pocket_ic_tests::new_pocket_ic;
use gleaph_provision::types::{
    sha256, ArtifactId, ArtifactPublishMetadataArgs, ArtifactUploadChunkArgs, CanisterKind,
    ReleaseActivateArgs, ReleaseId, ReleasePublishArgs,
};
use gleaph_router::types::{RegisterGraphArgs, TextIndexInfo, TextIndexStatusView};
use gleaph_router::RouterInitArgs;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const GRAPH_NAME: &str = "text0297.graph";
const INDEX_NAME: &str = "doc_title_text_idx";
/// Migration ledger id driving the provisioned definition from Backfilling to Ready.
const MIGRATION_ID: &str = "000102_text_index_provisioning_backfill";
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
        ["gleaph-graph"] => "gleaph_graph.wasm",
        ["gleaph-graph-index"] => "gleaph_graph_index.wasm",
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

fn graph_wasm() -> Vec<u8> {
    // Same feature set as the build.rs federation build so the reinstalled graph canister
    // matches the production artifact composition.
    ensure_wasm(
        "GRAPH_WASM",
        &["gleaph-graph"],
        &["gleaph-graph/pocket-ic-e2e"],
        "pocket-ic-text-provision-wasm",
    )
}

fn index_wasm() -> Vec<u8> {
    ensure_wasm(
        "INDEX_WASM",
        &["gleaph-graph-index"],
        &[],
        "pocket-ic-text-provision-wasm",
    )
}

// -- Bootstrap --------------------------------------------------------------------------------

fn bootstrap() -> Env {
    let pic = new_pocket_ic();
    let admin = Principal::from_slice(&[0xAB; 29]);

    let router = pic.create_canister();
    pic.add_cycles(router, 2_000_000_000_000);

    // Under the grant model the Router is granted as an issuer (deployment_id = router
    // principal = caller, ADR 0068).
    let provision_canister = pic.create_canister();
    pic.add_cycles(provision_canister, 100_000_000_000_000);
    pic.install_canister(
        provision_canister,
        provision_wasm(),
        Encode!(&gleaph_provision::canister::init::ProvisionInitArgs {
            governance_principal: admin,
        })
        .expect("encode provision init"),
        None,
    );
    pic.update_call(
        provision_canister,
        admin,
        "upsert_deployment_grant",
        Encode!(&gleaph_provision::types::UpsertDeploymentGrantArgs { issuer: router })
            .expect("encode upsert args"),
    )
    .expect("seed router grant");

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

/// Discovers the PROVISION-ISSUED graph shard principal from the router registry (ADR 0059
/// §Text build kind E2E finding: requesting a `GraphShard` resource makes issuance create a
/// fresh canister and the router records THAT principal as the shard's graph target).
fn issued_shard_canister(env: &Env) -> Principal {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_shards",
            Encode!(&GRAPH_NAME.to_string()).expect("encode list_shards"),
        )
        .expect("list_shards");
    let entries: Result<Vec<gleaph_graph_kernel::federation::ShardRegistryEntry>, RouterError> =
        Decode!(
            &bytes,
            Result<Vec<gleaph_graph_kernel::federation::ShardRegistryEntry>, RouterError>
        )
        .expect("decode list_shards");
    let entries = entries.expect("list_shards ok");
    let issued: Vec<Principal> = entries
        .iter()
        .filter(|entry| entry.shard_id == ShardId::new(0))
        .map(|entry| entry.graph_canister)
        .collect();
    assert_eq!(
        issued.len(),
        1,
        "expected exactly one issued GraphShard(0) entry in list_shards: {entries:?}"
    );
    issued[0]
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
    // Plan 0332: the absent `ANALYZER` clause (the bare admin endpoint carries no
    // analyzer argument) now resolves to the multilingual composite — id 0, the
    // DEFAULT. This provisioning lifecycle leg exercises exactly that default path;
    // the id-0 backfill registration requires the dictionary upload legs below.
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
// -- Plan 0332: the id-0 default requires the finalized dictionary -------------------------

/// Returns the MORPHDICT1 container bytes (identical source discipline to the
/// text_score_query legs: the pinned PyPI ipadic 1.0.0 four-image set, fail-closed).
fn fetch_mecab_container() -> Vec<u8> {
    const IMAGES: [&str; 4] = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
    const MECAB_DICT_URL: &str = "https://files.pythonhosted.org/packages/e7/4e/c459f94d62a0bef89f866857bc51b9105aff236b83928618315b41a26b7b/ipadic-1.0.0.tar.gz";
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("mecrab");
    if !dir.join("sys.dic").exists() {
        std::fs::create_dir_all(&dir).expect("create resources/mecrab");
        let tarball = dir.join("ipadic-1.0.0.tar.gz");
        let status = Command::new("curl")
            .args(["-sL", "-o"])
            .arg(&tarball)
            .arg(MECAB_DICT_URL)
            .status()
            .expect("spawn curl for the ipadic dictionary");
        assert!(status.success(), "curl fetch of {MECAB_DICT_URL} failed");
        let extracted = Command::new("tar")
            .args(["-xzf"])
            .arg(&tarball)
            .arg("-C")
            .arg(&dir)
            .status()
            .expect("spawn tar");
        assert!(
            extracted.success(),
            "tar extract of the ipadic tarball failed"
        );
        let dicdir = dir.join("ipadic-1.0.0").join("ipadic").join("dicdir");
        for name in IMAGES {
            std::fs::copy(dicdir.join(name), dir.join(name))
                .unwrap_or_else(|e| panic!("move {name} into place: {e}"));
        }
        std::fs::remove_dir_all(dir.join("ipadic-1.0.0")).expect("remove extracted tree");
        std::fs::remove_file(&tarball).expect("remove tarball");
    }
    let mut images: Vec<(String, Vec<u8>)> = Vec::new();
    for name in IMAGES {
        images.push((
            name.to_string(),
            std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}")),
        ));
    }
    morph_dict::container::build(images)
}

/// The pinned catalog compression level (plan 0335 gate 1: level 19 = 10,900,552 B on the
/// real container; `compressed_digest` pins the exact bytes, so the level is contract).
const CATALOG_ZSTD_LEVEL: i32 = 19;

/// Plan 0335 todo 4: seeds the Provision dictionary catalog with the ZSTD-compressed REAL
/// MPD container. Size correction (management-verified): the container is the FULL
/// `morph_dict::container::build` output — 52,931,159 B = image sum 52,930,923 + 236 B
/// framing (header + entry table) — so `raw_len`/`raw_digest` pin the full bytes, not the
/// four-image sum. Rows are ≤ 1 MiB per `MAX_DICT_CATALOG_CHUNK_LEN`.
fn seed_dict_catalog(env: &Env) -> (Vec<u8>, Vec<u8>) {
    use gleaph_provision::types::{
        DictCatalogFinalizeArgs, DictCatalogKey, DictCatalogUploadChunkArgs,
    };
    let raw = fetch_mecab_container();
    let compressed =
        zstd::stream::encode_all(&raw[..], CATALOG_ZSTD_LEVEL).expect("zstd-19 catalog encode");
    let key = DictCatalogKey {
        kind: "ipadic".to_owned(),
        version: "2.7.0".to_owned(),
    };
    for (chunk_index, chunk) in compressed.chunks(1024 * 1024).enumerate() {
        let bytes = env
            .pic
            .update_call(
                env.provision,
                env.admin,
                "admin_upload_dict_catalog_chunk",
                Encode!(&DictCatalogUploadChunkArgs {
                    key: key.clone(),
                    chunk_index: chunk_index as u32,
                    bytes: chunk.to_vec(),
                })
                .expect("encode catalog chunk"),
            )
            .unwrap_or_else(|e| panic!("admin_upload_dict_catalog_chunk: {e:?}"));
        let status: Result<
            gleaph_provision::types::DictCatalogStatus,
            gleaph_provision::types::DictCatalogError,
        > = Decode!(
            &bytes,
            Result<
                gleaph_provision::types::DictCatalogStatus,
                gleaph_provision::types::DictCatalogError,
            >
        )
        .expect("decode catalog upload reply");
        status.expect("catalog chunk accepted");
    }
    let bytes = env
        .pic
        .update_call(
            env.provision,
            env.admin,
            "admin_finalize_dict_catalog",
            Encode!(&DictCatalogFinalizeArgs {
                key: key.clone(),
                compressed_digest: xxhash_rust::xxh3::xxh3_128(&compressed),
                raw_digest: xxhash_rust::xxh3::xxh3_128(&raw),
                raw_len: raw.len() as u64,
            })
            .expect("encode catalog finalize"),
        )
        .unwrap_or_else(|e| panic!("admin_finalize_dict_catalog: {e:?}"));
    let status: Result<
        gleaph_provision::types::DictCatalogStatus,
        gleaph_provision::types::DictCatalogError,
    > = Decode!(
        &bytes,
        Result<
            gleaph_provision::types::DictCatalogStatus,
            gleaph_provision::types::DictCatalogError,
        >
    )
    .expect("decode catalog finalize reply");
    let status = status.expect("catalog finalize accepted");
    assert_eq!(
        status.state,
        gleaph_provision::types::DictCatalogState::Finalized,
        "catalog must be Finalized before provisioning"
    );
    (compressed, raw)
}

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

// -- Migration-lane drive (ADR 0059) ----------------------------------------------------------

/// Builds the `apply_schema_migration` payload for the provisioned text definition. The
/// statement must match the definition created by `create_text_index` so the migration operates
/// on the existing Backfilling row rather than declaring a fresh one.
fn migration_args(id: &str, statement: &str) -> ApplySchemaMigrationArgs {
    let selector = SchemaMigrationGraphSelector::Default;
    ApplySchemaMigrationArgs::V1(ApplySchemaMigrationArgsV1 {
        id: id.to_owned(),
        parent: None,
        graph_selector: selector.clone(),
        checksum: gleaph_migration_api::schema_migration_checksum(
            id,
            None,
            &selector,
            statement.as_bytes(),
        ),
        statement: statement.to_owned(),
    })
}

fn try_apply_once(
    env: &Env,
    args: &ApplySchemaMigrationArgs,
) -> Result<ApplySchemaMigrationResultV1, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "apply_schema_migration",
            Encode!(args).expect("encode apply_schema_migration"),
        )
        .unwrap_or_else(|e| panic!("apply_schema_migration on router: {e:?}"));
    let decoded: Result<ApplySchemaMigrationResult, RouterError> =
        Decode!(&bytes, Result<ApplySchemaMigrationResult, RouterError>)
            .expect("decode apply_schema_migration");
    decoded.map(|applied| match applied {
        ApplySchemaMigrationResult::V1(result) => result,
    })
}

/// Applies once, tolerating the driver's explicit Retryable/Busy verdicts (ADR 0059
/// bounded-step contract).
fn apply_retrying_busy(
    env: &Env,
    args: &ApplySchemaMigrationArgs,
    max_attempts: usize,
) -> ApplySchemaMigrationResultV1 {
    for attempt in 0..max_attempts {
        match try_apply_once(env, args) {
            Ok(result) => return result,
            Err(err @ RouterError::Busy { .. }) => {
                assert!(
                    attempt + 1 < max_attempts,
                    "apply_schema_migration still Busy after {max_attempts} attempts: {err:?}"
                );
                for _ in 0..16 {
                    env.pic.tick();
                }
            }
            Err(err) => panic!("apply_schema_migration rejected: {err:?}"),
        }
    }
    unreachable!("retry loop must return or panic")
}

// -- Scenarios --------------------------------------------------------------------------------

/// One bootstrap serves all three scenarios in order.
#[test]
fn text_index_provisions_replays_and_guards() {
    let env = bootstrap();
    activate_release(&env);
    register_graph(&env);
    // Issuance created a fresh canister for the requested GraphShard and installed the release
    // artifact — a dummy in this fixture. The text backfill drives Graph export-scope methods
    // (`admin_register_index_export_scope` / `admin_seal_index_export_scope` / page pulls) on
    // that canister, so reinstall the REAL graph wasm with shard init args; stable memory is
    // empty at this point, so reinstall is lossless (mirrors
    // finish_provision_wired_single_shard_federation).
    let graph_source = issued_shard_canister(&env);
    // The graph wasm traps at init unless index_canister accompanies router_canister + shard_id,
    // so install one. This fixture performs no data DML, so the index stays idle — it exists
    // purely to satisfy the graph shard's FederationRouting init contract.
    // The graph wasm requires a wired index canister alongside router_canister + shard_id, so
    // install one (no data DML happens here, so the index stays idle — it exists to satisfy
    // the graph shard's FederationRouting init contract).
    let index = env.pic.create_canister();
    env.pic.add_cycles(index, 2_000_000_000_000);
    env.pic.install_canister(
        index,
        index_wasm(),
        Encode!(&gleaph_pocket_ic_tests::IndexInitArgs {
            router_canister: env.router,
        })
        .expect("encode index init"),
        None,
    );
    env.pic
        .reinstall_canister(
            graph_source,
            graph_wasm(),
            Encode!(&gleaph_pocket_ic_tests::GraphInitArgs {
                logical_graph_name: Some(GRAPH_NAME.to_owned()),
                router_canister: Some(env.router),
                shard_id: Some(ShardId::new(0)),
                index_canister: Some(index),
            })
            .expect("encode graph init"),
            // Issuance provisions shard canisters with controllers [provision, governance
            // principal]; reinstall needs a controller as sender.
            Some(env.admin),
        )
        .expect("reinstall graph wasm with full shard init args");
    ensure_vertex_label(&env, "Document");
    ensure_property(&env, "title");

    // --- (a.0) fail-closed POSITIVE legs (plan 0335 todo 4): provisioning a
    // dictionary-required text index without a Finalized catalog entry must fail closed
    // with the relay's internal reason — never a silently dictionary-less canister.
    // Distinct index names keep the failed jobs clear of the main INDEX_NAME flow.
    let unseeded_err = create_text_index(&env, env.admin, "unseeded_text_idx", "Document", "title")
        .expect_err("provisioning without a seeded catalog must fail closed");
    assert!(
        matches!(unseeded_err, RouterError::Internal(ref m) if m.contains("missing from created_resources")),
        "unexpected unseeded error: {unseeded_err:?}"
    );

    // One chunk uploaded but NOT finalized → the relay must still refuse (entry is not
    // Finalized); the main seeding then completes over the same key.
    {
        use gleaph_provision::types::{DictCatalogKey, DictCatalogUploadChunkArgs};
        let raw = fetch_mecab_container();
        let compressed =
            zstd::stream::encode_all(&raw[..], CATALOG_ZSTD_LEVEL).expect("zstd-19 catalog encode");
        let key = DictCatalogKey {
            kind: "ipadic".to_owned(),
            version: "2.7.0".to_owned(),
        };
        let bytes = env
            .pic
            .update_call(
                env.provision,
                env.admin,
                "admin_upload_dict_catalog_chunk",
                Encode!(&DictCatalogUploadChunkArgs {
                    key: key.clone(),
                    chunk_index: 0,
                    bytes: compressed[..1024 * 1024].to_vec(),
                })
                .expect("encode catalog chunk"),
            )
            .unwrap_or_else(|e| panic!("admin_upload_dict_catalog_chunk: {e:?}"));
        let status: Result<
            gleaph_provision::types::DictCatalogStatus,
            gleaph_provision::types::DictCatalogError,
        > = Decode!(
            &bytes,
            Result<
                gleaph_provision::types::DictCatalogStatus,
                gleaph_provision::types::DictCatalogError,
            >
        )
        .expect("decode catalog upload reply");
        let status = status.expect("first chunk accepted");
        assert_eq!(
            status.state,
            gleaph_provision::types::DictCatalogState::Uploading,
            "catalog entry with one chunk is Uploading, not Finalized"
        );
        let not_finalized_err = create_text_index(
            &env,
            env.admin,
            "unfinalized_catalog_text_idx",
            "Document",
            "title",
        )
        .expect_err("provisioning against a non-Finalized catalog must fail closed");
        assert!(
            matches!(not_finalized_err, RouterError::Internal(ref m) if m.contains("missing from created_resources")),
            "unexpected non-Finalized error: {not_finalized_err:?}"
        );
    }

    // --- (a) issue → canister created with Text kind + definition registered + born Backfilling ---
    let (_compressed, raw) = seed_dict_catalog(&env);
    // Gate-2 measurement (plan 0335): the provision canister executes the relay (catalog
    // streaming + the text canister's compressed finalize). Its cycle delta across
    // create_text_index isolates the relayed work end-to-end (inter-canister send costs
    // included; the raw-path finalize measured 275,016,142 cycles in 0334 for scale).
    let provision_cycles_before = env.pic.cycle_balance(env.provision);
    let info = create_text_index(&env, env.admin, INDEX_NAME, "Document", "title")
        .expect("issue must succeed");
    let canister = info.canister.expect("provisioned canister attached");
    let relay_cycles = provision_cycles_before.saturating_sub(env.pic.cycle_balance(env.provision));
    println!(
        "plan-0335 relay cost (provision-side: catalog streaming + relay calls incl. the text canister's compressed finalize) cycles: {relay_cycles}"
    );
    assert_ne!(canister, Principal::anonymous());
    // ADR 0059: a provisioned text definition is born Backfilling (planner-invisible) and flips
    // to Ready only after the migration ledger's convergence proof (scan-done AND flushed
    // watermark) via complete_text_backfill.
    assert_eq!(info.status, TextIndexStatusView::Backfilling);
    assert_eq!(
        info.analyzer_id,
        text_canister::ANALYZER_MULTILINGUAL,
        "the absent-clause admission pins the multilingual composite default (plan 0332)"
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
        text_canister::ANALYZER_MULTILINGUAL,
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

    // --- (a.1) plan 0335 todo 4: the RELAY-driven dictionary path --------------------
    // The catalog was seeded before issuance; the default CREATE TEXT INDEX path (id 0,
    // dictionary-required) provisioned its canister and the post-install relay streamed
    // the catalog's compressed container and finalized the dictionary — ZERO manual
    // dictionary operations (the former manual raw upload is superseded; raw correctness
    // is pinned by the text-canister unit tests).
    let raw_digest = xxhash_rust::xxh3::xxh3_128(&raw);
    env.pic.add_cycles(canister, 50_000_000_000_000);
    let status_bytes = env
        .pic
        .query_call(
            canister,
            env.router,
            "admin_get_dict_status",
            Encode!(&()).expect("encode get status"),
        )
        .unwrap_or_else(|e| panic!("admin_get_dict_status: {e:?}"));
    let relayed: text_canister::DictStatus =
        Decode!(&status_bytes, text_canister::DictStatus).expect("decode status");
    assert_eq!(
        relayed.state,
        text_canister::DictState::Finalized,
        "the relay must finalize the dictionary during provisioning"
    );
    assert_eq!(
        relayed.digest,
        Some(raw_digest),
        "relay pins the catalog's raw digest"
    );
    assert_eq!(
        relayed.len as usize,
        raw.len(),
        "raw length matches the full container (the 52,931,159 B correction)"
    );
    assert_eq!(
        relayed.compressed, None,
        "compressed staging is deactivated after the relayed finalize"
    );

    // Fail-closed (positive asserts): a THIRD principal is rejected by the relay guard
    // (plan 0335 §5-2), and a wrong digest cannot masquerade as the catalog identity.
    let outsider = Principal::from_slice(&[0x3E; 29]);
    let err = env
        .pic
        .update_call(
            canister,
            outsider,
            "admin_finalize_dict_upload",
            Encode!(
                &raw_digest,
                &None::<gleaph_graph_kernel::provisioning::dictionary::CompressedDictFinalize>
            )
            .expect("encode"),
        )
        .expect_err("third principal must not finalize the dictionary");
    assert!(
        err.reject_message
            .contains("is neither the text index controller"),
        "unexpected guard reject: {}",
        err.reject_message
    );

    // Post-finalize uploads reject in both modes (idempotence of the pinned identity).
    let rejected_upload: Result<u64, String> = {
        let bytes = env
            .pic
            .update_call(
                canister,
                env.router,
                "admin_upload_dict_chunk",
                Encode!(
                    &vec![1u8; 16],
                    &None::<gleaph_graph_kernel::provisioning::dictionary::CompressedDictUpload>
                )
                .expect("encode"),
            )
            .unwrap_or_else(|e| panic!("admin_upload_dict_chunk: {e:?}"));
        Decode!(&bytes, Result<u64, String>).expect("decode upload reply")
    };
    assert!(rejected_upload.is_err(), "post-finalize upload must reject");

    // --- (a.2) drive the migration lane to convergence: Backfilling → Ready ---
    // The definition is planner-invisible until the migration ledger reaches Applied (scan-done
    // AND flushed watermark). Drive `apply_schema_migration` one bounded step per call, mirroring
    // adr0059_text_backfill_migration; the empty corpus still converges (0 docs scanned).
    // Bare statement (absent clause): matches the definition's id-0 default pin.
    let statement = format!("CREATE TEXT INDEX {INDEX_NAME} FOR (v:Document) ON (v.title)");
    let args = migration_args(MIGRATION_ID, &statement);
    let mut applied_result = None;
    for _ in 0..12 {
        // Mid-drive the definition is either still Backfilling (planner-invisible) or already
        // flipped by the previous drive's convergence — both are legal; what must NEVER appear
        // is the pre-ADR-0059 Registered/Ready-at-registration shape.
        let status_now = get_text_index(&env, env.admin, INDEX_NAME)
            .expect("definition")
            .status;
        assert!(
            matches!(
                status_now,
                TextIndexStatusView::Backfilling | TextIndexStatusView::Ready
            ),
            "unexpected mid-drive status: {status_now:?}"
        );
        let result = apply_retrying_busy(&env, &args, 8);
        match &result.status {
            SchemaMigrationApplyStatus::Progress(progress) => match progress.phase {
                SchemaMigrationProgressPhase::Preparing
                | SchemaMigrationProgressPhase::Building
                | SchemaMigrationProgressPhase::Sealing => {}
                other_phase => panic!("unexpected migration phase: {other_phase:?}"),
            },
            SchemaMigrationApplyStatus::Applied => {
                applied_result = Some(result);
                break;
            }
            other => panic!("unexpected migration progress: {other:?}"),
        }
    }
    let applied = applied_result.expect("migration must reach Applied within budget");
    assert!(matches!(
        applied.record,
        SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
            state: SchemaMigrationRecordState::Applied { .. },
            ..
        })
    ));

    // Convergence flipped readiness exactly once: Backfilling → Ready via the catalog gate.
    let ready_info = get_text_index(&env, env.admin, INDEX_NAME).expect("definition");
    assert_eq!(ready_info.status, TextIndexStatusView::Ready);
    assert_eq!(ready_info.text_index_id, info.text_index_id);
    assert_eq!(ready_info.canister, Some(canister), "same canister id");

    // --- (b) identical re-issue → same canister id, no second creation ---
    let replay = create_text_index(&env, env.admin, INDEX_NAME, "Document", "title")
        .expect("identical re-issue is a no-op returning the existing resource");
    assert_eq!(replay.text_index_id, ready_info.text_index_id);
    assert_eq!(replay.canister, Some(canister), "same canister id");
    assert_eq!(replay.status, TextIndexStatusView::Ready);
    assert_eq!(
        get_text_index(&env, env.admin, INDEX_NAME).expect("single definition"),
        ready_info,
        "no second creation: the original row survives unchanged"
    );

    // (b.1) plan 0335 todo 4 idempotence: the re-provision replay must NOT re-append the
    // dictionary — the relay's short-circuit (Finalized + matching digest) leaves both the
    // catalog rows and the canister's staged bytes untouched.
    {
        use gleaph_provision::types::{DictCatalogKey, DictCatalogState};
        let status_bytes = env
            .pic
            .query_call(
                env.provision,
                env.admin,
                "admin_get_dict_catalog_status",
                Encode!(&DictCatalogKey {
                    kind: "ipadic".to_owned(),
                    version: "2.7.0".to_owned(),
                })
                .expect("encode catalog status"),
            )
            .unwrap_or_else(|e| panic!("admin_get_dict_catalog_status: {e:?}"));
        let catalog: Option<gleaph_provision::types::DictCatalogStatus> = Decode!(
            &status_bytes,
            Option<gleaph_provision::types::DictCatalogStatus>
        )
        .expect("decode catalog status");
        let catalog = catalog.expect("seeded catalog entry");
        assert_eq!(catalog.state, DictCatalogState::Finalized);
        let dict_status_bytes = env
            .pic
            .query_call(
                canister,
                env.router,
                "admin_get_dict_status",
                Encode!(&()).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("admin_get_dict_status: {e:?}"));
        let dict_status: text_canister::DictStatus =
            Decode!(&dict_status_bytes, text_canister::DictStatus).expect("decode status");
        assert_eq!(dict_status.state, text_canister::DictState::Finalized);
        assert_eq!(dict_status.digest, Some(raw_digest));
        assert_eq!(
            dict_status.compressed, None,
            "re-provision must not resurrect the compressed staging"
        );
    }

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
