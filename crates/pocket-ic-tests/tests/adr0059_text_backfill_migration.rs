//! PocketIC E2E for plan 0297 `backfill-pull` acceptance (ADR 0059 §Text build kind):
//! the migration-driven TEXT backfill lifecycle end to end.
//!
//! Flow: one single-shard federation whose Router is wired to a Provision canister; publish
//! and activate a release carrying the REAL text-canister wasm; declare a provisioned TEXT
//! definition (born `Backfilling`, planner-invisible); seed text-valued vertices through GQL;
//! then drive `apply_schema_migration` with a `CREATE TEXT INDEX` payload one bounded step at
//! a time. Each apply drives exactly one Register/Build/Seal step, so mid-backfill state is
//! observable and resumable: the test upgrades the Router canister mid-build (stable-memory
//! crash window) and converges without loss.
//!
//! Run: `cargo test -p gleaph-pocket-ic-tests --test adr0059_text_backfill_migration`.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::provisioning::LogicalResource;
use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
    SchemaMigrationProgressPhase, SchemaMigrationRecord, SchemaMigrationRecordState,
    SchemaMigrationRecordV1,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, gql_mutate_as_admin, install_single_shard_federation_with_provision,
    wasm_bytes,
};
use gleaph_provision::types::{
    ArtifactError, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload,
    ArtifactUploadChunkArgs, CanisterKind, DeploymentBinding, ReleaseActivateArgs, ReleaseError,
    ReleaseId, ReleaseManifest, ReleasePublishArgs, sha256,
};
use gleaph_router::RouterInitArgs;
use gleaph_router::types::{RegisterGraphArgs, RouterError, TextIndexInfo, TextIndexStatusView};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const INDEX_NAME: &str = "adr0059_doc_bio_text_idx";
const MIGRATION_ID: &str = "000102_adr0059_text_backfill";
const LABEL: &str = "Document";
const PROPERTY: &str = "bio";
const SEED_DOCS: usize = 3;
/// Matches Provision's 1 MiB install-chunk bound.
const PUBLISH_CHUNK_BYTES: usize = 1024 * 1024;

struct Env {
    fed: FederationEnv,
    provision: Principal,
}

// -- Wasm acquisition -------------------------------------------------------------------------

/// Reads a wasm artifact from `env_var` when set (build.rs fast path), otherwise builds the
/// named package in an isolated target dir (mirrors `text_index_provisioning`).
fn ensure_wasm(env_var: &str, package: &str, artifact: &str, cache_dir: &str) -> Vec<u8> {
    if let Ok(path) = std::env::var(env_var) {
        return std::fs::read(&path).unwrap_or_else(|e| panic!("read {env_var} {}: {e}", path));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("workspace root above crates/");
    let target_dir = workspace_root.join("target").join(cache_dir);
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .arg("--package")
        .arg(package)
        .status()
        .expect("spawn cargo build for PocketIC wasm");
    assert!(status.success(), "wasm build for {package} failed");
    let wasm_path = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(artifact);
    std::fs::read(&wasm_path).unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()))
}

fn provision_wasm() -> Vec<u8> {
    ensure_wasm(
        "PROVISION_WASM",
        "gleaph-provision",
        "gleaph_provision.wasm",
        "pocket-ic-text-provision-wasm",
    )
}

fn text_wasm() -> Vec<u8> {
    // Shared cache directory with `text_index_lifecycle`/`text_index_provisioning`.
    ensure_wasm(
        "TEXT_INDEX_WASM",
        "text-canister",
        "text_canister.wasm",
        "pocket-ic-text-wasm",
    )
}

fn router_wasm() -> Vec<u8> {
    // The build.rs-managed federation router wasm carries the pocket-ic-e2e surface.
    wasm_bytes("ROUTER_WASM")
}

// -- Provision calls --------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
fn call_artifact<R: candid::CandidType + serde::de::DeserializeOwned>(
    env: &Env,
    method: &str,
    args: &impl candid::CandidType,
) -> R {
    let bytes = env
        .fed
        .pic
        .update_call(
            env.provision,
            env.fed.admin,
            method,
            Encode!(args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("{method} on provision: {e:?}"));
    Decode!(&bytes, Result<R, ArtifactError>)
        .expect("decode artifact response")
        .unwrap_or_else(|e| panic!("{method} rejected: {e:?}"))
}

#[allow(clippy::result_large_err)]
fn call_release<R: candid::CandidType + serde::de::DeserializeOwned>(
    env: &Env,
    method: &str,
    args: &impl candid::CandidType,
) -> R {
    let bytes = env
        .fed
        .pic
        .update_call(
            env.provision,
            env.fed.admin,
            method,
            Encode!(args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("{method} on provision: {e:?}"));
    Decode!(&bytes, Result<R, ReleaseError>)
        .expect("decode release response")
        .unwrap_or_else(|e| panic!("{method} rejected: {e:?}"))
}

/// Publishes one verified artifact split into bounded chunks.
fn publish_verified_artifact(env: &Env, kind: CanisterKind, wasm: &[u8]) -> ArtifactId {
    let full_sha = sha256(wasm);
    let chunks: Vec<&[u8]> = if wasm.len() <= PUBLISH_CHUNK_BYTES {
        vec![wasm]
    } else {
        wasm.chunks(PUBLISH_CHUNK_BYTES).collect()
    };
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|c| sha256(c)).collect();
    let _: ArtifactMetadata = call_artifact(
        env,
        "artifact_publish_metadata",
        &ArtifactPublishMetadataArgs {
            canister_kind: kind.clone(),
            semantic_version: "0.1.0".to_owned(),
            sha256: full_sha,
            byte_length: wasm.len() as u64,
            chunk_hashes,
        },
    );
    let id = ArtifactId::new(kind, "0.1.0".to_owned(), full_sha);
    for (index, chunk) in chunks.iter().enumerate() {
        let _: ArtifactUpload = call_artifact(
            env,
            "artifact_upload_chunk",
            &ArtifactUploadChunkArgs {
                artifact_id: id.clone(),
                chunk_index: index as u32,
                bytes: chunk.to_vec(),
            },
        );
    }
    id
}

/// Publishes all five release kinds (Text carries the real text-canister wasm) and activates.
fn activate_release(env: &Env) {
    let dummy = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
    let ids = vec![
        publish_verified_artifact(env, CanisterKind::Router, &dummy),
        publish_verified_artifact(env, CanisterKind::Graph, &dummy),
        publish_verified_artifact(env, CanisterKind::PropertyIndex, &dummy),
        publish_verified_artifact(env, CanisterKind::VectorCanister, &dummy),
        publish_verified_artifact(env, CanisterKind::TextCanister, &text_wasm()),
    ];
    let _: ReleaseManifest = call_release(
        env,
        "release_publish",
        &ReleasePublishArgs {
            release_id: ReleaseId("release-text-backfill-0297".to_owned()),
            artifact_ids: ids,
        },
    );
    let _: gleaph_provision::types::ReleaseActivateResult = call_release(
        env,
        "release_activate",
        &ReleaseActivateArgs {
            release_id: ReleaseId("release-text-backfill-0297".to_owned()),
        },
    );
}

// -- Router helpers ---------------------------------------------------------------------------

fn register_graph(env: &Env) {
    let intent = RegisterGraphArgs {
        graph_name: GRAPH_NAME.to_owned(),
        owner: env.fed.admin,
        admins: BTreeSet::new(),
        is_home: false,
        shards: vec![],
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
        }],
    };
    let bytes = env
        .fed
        .pic
        .update_call(
            env.fed.router,
            env.fed.admin,
            "register_graph",
            Encode!(&intent).expect("encode register_graph"),
        )
        .unwrap_or_else(|e| panic!("register_graph on router: {e:?}"));
    let result: Result<(), RouterError> =
        Decode!(&bytes, Result<(), RouterError>).expect("decode register_graph");
    assert!(result.is_ok(), "register_graph must succeed: {result:?}");
}

/// Seeds one text-valued vertex through router-routed GQL. The single live shard IS the home
/// shard, so placement is deterministic and every seeded doc belongs to the base scan.
fn seed_text_vertex(env: &Env, key: &str, bio: &str) {
    let query = format!("INSERT (:{LABEL} {{ {PROPERTY}: \"{bio}\" }})");
    let _rows = gql_mutate_as_admin(&env.fed, &query, key);
}

fn create_text_index_definition(env: &Env) -> TextIndexInfo {
    let bytes = env
        .fed
        .pic
        .update_call(
            env.fed.router,
            env.fed.admin,
            "create_text_index",
            Encode!(
                &GRAPH_NAME.to_string(),
                &INDEX_NAME.to_string(),
                &LABEL.to_string(),
                &PROPERTY.to_string()
            )
            .expect("encode create_text_index"),
        )
        .unwrap_or_else(|e| panic!("create_text_index on router: {e:?}"));
    Decode!(&bytes, Result<TextIndexInfo, RouterError>)
        .expect("decode create_text_index")
        .expect("provisioned definition created")
}

fn get_text_index(env: &Env) -> TextIndexInfo {
    let bytes = env
        .fed
        .pic
        .query_call(
            env.fed.router,
            env.fed.admin,
            "get_text_index",
            Encode!(&GRAPH_NAME.to_string(), &INDEX_NAME.to_string()).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("get_text_index on router: {e:?}"));
    Decode!(&bytes, Result<TextIndexInfo, RouterError>)
        .expect("decode get_text_index")
        .expect("definition exists")
}

fn text_index_status(env: &Env) -> TextIndexStatusView {
    get_text_index(env).status
}

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

fn apply_once(env: &Env, args: &ApplySchemaMigrationArgs) -> ApplySchemaMigrationResultV1 {
    let bytes = env
        .fed
        .pic
        .update_call(
            env.fed.router,
            env.fed.admin,
            "apply_schema_migration",
            Encode!(args).expect("encode apply_schema_migration"),
        )
        .unwrap_or_else(|e| panic!("apply_schema_migration on router: {e:?}"));
    let decoded: Result<ApplySchemaMigrationResult, RouterError> =
        Decode!(&bytes, Result<ApplySchemaMigrationResult, RouterError>)
            .expect("decode apply_schema_migration");
    match decoded {
        Ok(ApplySchemaMigrationResult::V1(result)) => result,
        Err(err) => panic!("apply_schema_migration rejected: {err:?}"),
    }
}

fn text_stats(env: &Env) -> text_canister::TextIndexStats {
    let canister = get_text_index(env)
        .canister
        .expect("provisioned text canister attached");
    let bytes = env
        .fed
        .pic
        .query_call(
            canister,
            env.fed.admin,
            "get_stats",
            Encode!(&()).expect("encode"),
        )
        .expect("get_stats on text canister");
    Decode!(&bytes, text_canister::TextIndexStats).expect("decode get_stats")
}

// -- Scenario ---------------------------------------------------------------------------------

#[test]
fn text_backfill_migrates_converges_and_resumes_across_upgrade() {
    let env = bootstrap();
    activate_release(&env);
    register_graph(&env);

    // Declare + provision: issuance creates the text canister with the Router wired as its
    // control-plane caller; the definition is born Backfilling (planner-invisible).
    let info = create_text_index_definition(&env);
    assert_ne!(
        info.canister.expect("provisioned canister attached"),
        Principal::anonymous()
    );
    assert_eq!(info.status, TextIndexStatusView::Backfilling);
    assert_eq!(text_index_status(&env), TextIndexStatusView::Backfilling);

    // Seed the corpus BEFORE the migration registers its export scope so every doc is part
    // of the base scan.
    for index in 0..SEED_DOCS {
        seed_text_vertex(
            &env,
            &format!("seed_bio_{index}"),
            &format!("alpha beta {index}"),
        );
    }

    // Drive the migration one bounded step per apply.
    let statement = format!("CREATE TEXT INDEX {INDEX_NAME} ON (v:{LABEL}).{PROPERTY}");
    let args = migration_args(MIGRATION_ID, &statement);

    // Step 1 — prepare only: pending ledger row + durable build identity; no remote effects.
    let prepare = apply_once(&env, &args);
    assert!(matches!(
        prepare.status,
        SchemaMigrationApplyStatus::Progress(SchemaMigrationProgressPhase::Preparing)
    ));
    assert_eq!(text_index_status(&env), TextIndexStatusView::Backfilling);

    // Step 2 — Register: text-canister registration + Graph scope registration binding the
    // text canister as the ONLY authorized puller of its canonical export pages.
    let registered = apply_once(&env, &args);
    assert!(matches!(
        registered.status,
        SchemaMigrationApplyStatus::Progress(SchemaMigrationProgressPhase::Building)
    ));
    assert_eq!(text_index_status(&env), TextIndexStatusView::Backfilling);

    // CRASH WINDOW: replace the Router wasm mid-build. Durable state lives in stable memory,
    // so the pending migration must resume exactly where the last bounded step left it.
    env.fed
        .pic
        .upgrade_canister(env.fed.router, router_wasm(), Vec::new(), None)
        .expect("router upgrade mid-backfill");

    // Steps 3+ — Build until the scan is done, then Seal to convergence, then Applied.
    let mut saw_sealing_progress = false;
    let mut applied_result = None;
    for _ in 0..12 {
        // Until convergence lands, the definition must stay planner-invisible.
        assert_eq!(
            text_index_status(&env),
            TextIndexStatusView::Backfilling,
            "definition must stay Backfilling until the convergence flip"
        );
        let result = apply_once(&env, &args);
        match &result.status {
            SchemaMigrationApplyStatus::Progress(SchemaMigrationProgressPhase::Building) => {}
            SchemaMigrationApplyStatus::Progress(SchemaMigrationProgressPhase::Sealing) => {
                saw_sealing_progress = true;
            }
            SchemaMigrationApplyStatus::Applied => {
                applied_result = Some(result);
                break;
            }
            other => panic!("unexpected migration progress: {other:?}"),
        }
    }
    let applied = applied_result.expect("migration must reach Applied within budget");
    assert!(saw_sealing_progress, "the sealing fence must be observable");
    assert!(matches!(
        applied.record,
        SchemaMigrationRecord::V1(SchemaMigrationRecordV1 {
            state: SchemaMigrationRecordState::Applied { .. },
            ..
        })
    ));

    // Convergence flipped readiness EXACTLY once: Backfilling → Ready via the catalog gate.
    assert_eq!(text_index_status(&env), TextIndexStatusView::Ready);

    // The text canister ingested exactly the seeded corpus — no loss, no duplicates across
    // the crash-window upgrade.
    let stats = text_stats(&env);
    assert_eq!(
        stats.next_docid as usize, SEED_DOCS,
        "base scan must ingest exactly the seeded corpus"
    );

    // Exact replay after Applied: idempotent, no second lifecycle.
    let replay = apply_once(&env, &args);
    assert!(matches!(replay.status, SchemaMigrationApplyStatus::Replay));
}
