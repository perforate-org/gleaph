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
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
    SchemaMigrationProgress, SchemaMigrationProgressPhase, SchemaMigrationRecord,
    SchemaMigrationRecordState, SchemaMigrationRecordV1,
};
use gleaph_pocket_ic_tests::CanisterWasm;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, ProvisionWiredRouterEnv,
    finish_provision_wired_single_shard_federation, install_provision_wired_router, wasm_bytes,
};
use gleaph_provision::types::{
    ArtifactError, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload,
    ArtifactUploadChunkArgs, CanisterKind, ReleaseActivateArgs, ReleaseError, ReleaseId,
    ReleaseManifest, ReleasePublishArgs, sha256,
};
use gleaph_router::types::{TextIndexInfo, TextIndexStatusView};
use pocket_ic::PocketIc;
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
}

/// Phase 1 + release activation: everything issuance needs BEFORE graph registration.
fn bootstrap_with_active_release() -> ProvisionWiredRouterEnv {
    let wired = install_provision_wired_router();
    activate_release(&wired.pic, wired.admin, wired.provision);
    wired
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

fn text_wasm() -> Vec<u8> {
    // Shared cache directory with `text_index_lifecycle`/`text_index_provisioning`.
    ensure_wasm(
        "TEXT_INDEX_WASM",
        "text-canister",
        "text_canister.wasm",
        "pocket-ic-text-wasm",
    )
}

fn graph_wasm() -> Vec<u8> {
    // Issuance installs this artifact onto the provision-issued GraphShard canister, so it
    // must be the REAL graph wasm (a dummy module would answer every call with IC0536).
    // build.rs-managed federation wasm carries the pocket-ic-e2e surface.
    wasm_bytes(CanisterWasm::Graph)
}

fn router_wasm() -> Vec<u8> {
    // The build.rs-managed federation router wasm carries the pocket-ic-e2e surface.
    wasm_bytes(CanisterWasm::Router)
}

// -- Provision calls --------------------------------------------------------------------------
fn call_artifact<R: candid::CandidType + serde::de::DeserializeOwned>(
    pic: &PocketIc,
    sender: Principal,
    provision: Principal,
    method: &str,
    args: &impl candid::CandidType,
) -> R {
    let bytes = pic
        .update_call(provision, sender, method, Encode!(args).expect("encode"))
        .unwrap_or_else(|e| panic!("{method} on provision: {e:?}"));
    Decode!(&bytes, Result<R, ArtifactError>)
        .expect("decode artifact response")
        .unwrap_or_else(|e| panic!("{method} rejected: {e:?}"))
}
fn call_release<R: candid::CandidType + serde::de::DeserializeOwned>(
    pic: &PocketIc,
    sender: Principal,
    provision: Principal,
    method: &str,
    args: &impl candid::CandidType,
) -> R {
    let bytes = pic
        .update_call(provision, sender, method, Encode!(args).expect("encode"))
        .unwrap_or_else(|e| panic!("{method} on provision: {e:?}"));
    Decode!(&bytes, Result<R, ReleaseError>)
        .expect("decode release response")
        .unwrap_or_else(|e| panic!("{method} rejected: {e:?}"))
}

/// Publishes one verified artifact split into bounded chunks.
fn publish_verified_artifact(
    pic: &PocketIc,
    admin: Principal,
    provision: Principal,
    kind: CanisterKind,
    wasm: &[u8],
) -> ArtifactId {
    let full_sha = sha256(wasm);
    let chunks: Vec<&[u8]> = if wasm.len() <= PUBLISH_CHUNK_BYTES {
        vec![wasm]
    } else {
        wasm.chunks(PUBLISH_CHUNK_BYTES).collect()
    };
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|c| sha256(c)).collect();
    let _: ArtifactMetadata = call_artifact(
        pic,
        admin,
        provision,
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
            pic,
            admin,
            provision,
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
fn activate_release(pic: &PocketIc, admin: Principal, provision: Principal) {
    let dummy = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
    let ids = vec![
        publish_verified_artifact(pic, admin, provision, CanisterKind::Router, &dummy),
        publish_verified_artifact(pic, admin, provision, CanisterKind::Graph, &graph_wasm()),
        publish_verified_artifact(pic, admin, provision, CanisterKind::PropertyIndex, &dummy),
        publish_verified_artifact(pic, admin, provision, CanisterKind::VectorCanister, &dummy),
        publish_verified_artifact(
            pic,
            admin,
            provision,
            CanisterKind::TextCanister,
            &text_wasm(),
        ),
        publish_verified_artifact(pic, admin, provision, CanisterKind::Inference, &dummy),
    ];
    let _: ReleaseManifest = call_release(
        pic,
        admin,
        provision,
        "release_publish",
        &ReleasePublishArgs {
            release_id: ReleaseId("release-text-backfill-0297".to_owned()),
            artifact_ids: ids,
        },
    );
    let _: gleaph_provision::types::ReleaseActivateResult = call_release(
        pic,
        admin,
        provision,
        "release_activate",
        &ReleaseActivateArgs {
            release_id: ReleaseId("release-text-backfill-0297".to_owned()),
        },
    );
}

// -- Router helpers ---------------------------------------------------------------------------

/// Seeds one text-valued vertex through router-routed GQL. The single live shard IS the home
/// shard, so placement is deterministic and every seeded doc belongs to the base scan.
fn seed_text_vertex(env: &Env, _key: &str, bio: &str) {
    // Direct graph-canister seed (router e2e surface): GQL INSERT string literals are not
    // supported by the wire mutation expression evaluator, and the text corpus only needs
    // canonical Value::Text storage for the base scan to export.
    let label_raw = gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL).raw();
    let property_raw = gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY).raw();
    gleaph_pocket_ic_tests::e2e_insert_vertex_with_label_and_text_property(
        &env.fed,
        env.fed.graph_source,
        label_raw,
        property_raw,
        bio.to_owned(),
    );
}

fn create_text_index_definition(env: &Env) -> TextIndexInfo {
    // Re-pinned (plan 0332): the absent `ANALYZER` clause now resolves to the
    // multilingual composite (id 0); this leg's backfill corpus + assertions are
    // bigram-era, so the definition pins `ANALYZER unicode_bigram` explicitly (the
    // id-1 pipeline is byte-unchanged by the promotion).
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "text-ddl");
    get_text_index(env)
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
    try_apply_once(env, args)
        .unwrap_or_else(|err| panic!("apply_schema_migration rejected: {err:?}"))
}

/// Applies once, tolerating the driver's explicit Retryable/Busy verdicts (post-upgrade the
/// first remote drive can land in a retryable window; ADR 0059 bounded-step contract).
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
                    env.fed.pic.tick();
                }
            }
            Err(err) => {
                eprintln!(
                    "PROBE info={:?} backfill={:?}",
                    get_text_index(env),
                    text_backfill_status_probe(env)
                );
                panic!("apply_schema_migration rejected: {err:?}");
            }
        }
    }
    unreachable!("retry loop must return or panic")
}

fn try_apply_once(
    env: &Env,
    args: &ApplySchemaMigrationArgs,
) -> Result<ApplySchemaMigrationResultV1, RouterError> {
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
    decoded.map(|applied| match applied {
        ApplySchemaMigrationResult::V1(result) => result,
    })
}

fn text_backfill_status_probe(env: &Env) -> Option<text_canister::TextBackfillStatus> {
    let canister = get_text_index(env)
        .canister
        .expect("provisioned text canister attached");
    let bytes = env
        .fed
        .pic
        .query_call(
            canister,
            env.fed.admin,
            "get_text_backfill_status",
            Encode!().expect("encode get_text_backfill_status"),
        )
        .expect("get_text_backfill_status call");
    Decode!(&bytes, Option<text_canister::TextBackfillStatus>).expect("decode text backfill status")
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
    // Phase 1: router wired to Provision + release published and activated. Issuance needs an
    // ACTIVE release before the router accepts graph registration.
    let wired = bootstrap_with_active_release();
    // Phase 2: single-shard federation (registers the graph, attaches the index shard,
    // installs the graph canister).
    let env = Env {
        fed: finish_provision_wired_single_shard_federation(wired),
    };

    // Schema anchors for the definition, then declare + provision: issuance creates the text
    // canister with the Router wired as its control-plane caller; the definition is born
    // Backfilling (planner-invisible).
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    let info = create_text_index_definition(&env);
    // Issued canisters start with minimal cycles; a full backfill (ingest + merge across
    // many bounded steps) drains them past the freezing threshold mid-scan.
    env.fed.pic.add_cycles(
        info.canister.expect("provisioned canister attached"),
        20_000_000_000_000,
    );
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);
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
    // Matches the definition (pinned ANALYZER unicode_bigram, plan 0332 re-pin).
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    let args = migration_args(MIGRATION_ID, &statement);

    // Step 1 — prepare only: pending ledger row + durable build identity; no remote effects.
    let prepare = apply_once(&env, &args);
    assert!(matches!(
        prepare.status,
        SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
            phase: SchemaMigrationProgressPhase::Preparing,
            ..
        })
    ));
    assert_eq!(text_index_status(&env), TextIndexStatusView::Backfilling);

    // Step 2 — Register: text-canister registration + Graph scope registration binding the
    // text canister as the ONLY authorized puller of its canonical export pages.
    let registered = apply_once(&env, &args);
    assert!(matches!(
        registered.status,
        SchemaMigrationApplyStatus::Progress(SchemaMigrationProgress {
            phase: SchemaMigrationProgressPhase::Building,
            ..
        })
    ));
    assert_eq!(text_index_status(&env), TextIndexStatusView::Backfilling);

    // CRASH WINDOW: replace the Router wasm mid-build. Durable state lives in stable memory,
    // so the pending migration must resume exactly where the last bounded step left it.
    env.fed
        .pic
        .upgrade_canister(
            env.fed.router,
            router_wasm(),
            candid::Encode!(&Option::<gleaph_router::RouterUpgradeArgs>::None)
                .expect("encode router upgrade args"),
            None,
        )
        .expect("router upgrade mid-backfill");

    // Steps 3+ — Build until the scan is done, then Seal to convergence, then Applied.
    let mut saw_sealing_progress = false;
    let mut applied_result = None;
    for _ in 0..12 {
        // The definition is either mid-backfill (planner-invisible) or already flipped by
        // the previous drive's convergence — both are legal inside this loop; what must
        // NEVER appear is the pre-ADR-0059 Registered/Ready-at-registration shape.
        let status_now = text_index_status(&env);
        assert!(matches!(
            status_now,
            TextIndexStatusView::Backfilling | TextIndexStatusView::Ready
        ));
        let result = apply_retrying_busy(&env, &args, 8);
        match &result.status {
            SchemaMigrationApplyStatus::Progress(progress) => match progress.phase {
                SchemaMigrationProgressPhase::Building => {}
                SchemaMigrationProgressPhase::Sealing => {
                    saw_sealing_progress = true;
                }
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

    // Drain the text canister's pending log: backfill ingest lands in the durable FIFO and
    // becomes searchable/stats-visible only after bounded admin_flush steps (plan 0297
    // dml-pending-flush will automate this; until then the operator/timer drives it).
    let canister = get_text_index(&env)
        .canister
        .expect("provisioned text canister attached");
    drain_text_flush(&env, canister);

    // The text canister ingested exactly the seeded corpus — no loss, no duplicates across
    // the crash-window upgrade.
    let stats = text_stats(&env);
    assert_eq!(
        stats.pending_ops, 0,
        "pending log must be fully drained by the flush drive"
    );
    assert_eq!(
        stats.next_docid as usize, SEED_DOCS,
        "base scan must ingest exactly the seeded corpus"
    );

    // Exact replay after Applied: idempotent, no second lifecycle.
    let replay = apply_once(&env, &args);
    // Idempotency contract: an exact re-apply of an Applied migration executes nothing.
    // The observable proof is the unchanged corpus below plus the stable Ready state;
    // the ledger may answer either the recorded terminal status or a fresh Applied echo.
    assert!(matches!(
        replay.status,
        SchemaMigrationApplyStatus::Replay | SchemaMigrationApplyStatus::Applied
    ));
    let stats_after_replay = text_stats(&env);
    assert_eq!(
        stats_after_replay.ndocs, stats.ndocs,
        "replay must not duplicate or drop any ingested doc"
    );

    // Shard-originated delivery after Ready. The Graph shard ships its own pending document log
    // to the text canister, so the text canister must accept that shard as a writer. The e2e
    // Router build leaves the attach legs to the harness; drive them in the production order:
    // writer authorization on the text canister first, then the shard's local target.
    let label_raw = gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL).raw();
    let property_raw = gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY).raw();
    let attached: Result<(), String> = router_update(
        &env,
        canister,
        "admin_attach_shard_canister",
        Encode!(&0_u32, &env.fed.graph_source).expect("encode text writer attach"),
    );
    attached.expect("text canister attaches the graph shard as a writer");
    let targeted: Result<(), String> = router_update(
        &env,
        env.fed.graph_source,
        "admin_set_text_canister",
        Encode!(&label_raw, &property_raw, &canister).expect("encode graph text target"),
    );
    targeted.expect("graph shard records the text target");

    gleaph_pocket_ic_tests::gql_mutate_as_admin(
        &env.fed,
        &format!("INSERT (:{LABEL} {{{PROPERTY}: 'delivered by the graph shard'}})"),
        "adr0059-shard-delivery",
    );
    // The shard's maintenance timer ships the queued document; give it a few relaxed-delay ticks.
    advance_seconds(&env, 20);
    drain_text_flush(&env, canister);
    let delivered = text_stats(&env);
    assert_eq!(
        delivered.ndocs,
        stats.ndocs + 1,
        "the shard's own delivery must reach the text canister"
    );
    assert_eq!(delivered.pending_ops, 0);

    // With the delivery confirmed, the shard's maintenance timer has no unconfirmed text work
    // and stops. A rejected delivery is requeued and retried every few seconds forever, which
    // burns hundreds of millions of cycles per idle minute; an idle shard burns storage only.
    let graph_before = env.fed.pic.cycle_balance(env.fed.graph_source);
    advance_seconds(&env, 60);
    let idle_burn = graph_before - env.fed.pic.cycle_balance(env.fed.graph_source);
    assert!(
        idle_burn < 50_000_000,
        "an idle graph shard must not keep retrying text delivery (burned {idle_burn} cycles in 60 s)"
    );

    // A permanent writer rejection parks the definition instead of retrying it. Detach the
    // shard's writer authorization (the leg the Router's unregister drives), then write: the
    // shard's delivery is refused, the document stays queued, and the shard goes idle.
    let detached: Result<(), String> = router_update(
        &env,
        canister,
        "admin_detach_shard_canister",
        Encode!(&0_u32).expect("encode text writer detach"),
    );
    detached.expect("text canister detaches the graph shard writer");
    gleaph_pocket_ic_tests::gql_mutate_as_admin(
        &env.fed,
        &format!("INSERT (:{LABEL} {{{PROPERTY}: 'parked while detached'}})"),
        "adr0059-shard-parked",
    );
    advance_seconds(&env, 20);
    drain_text_flush(&env, canister);
    assert_eq!(
        text_stats(&env).ndocs,
        delivered.ndocs,
        "a detached shard's delivery is rejected"
    );
    let graph_before = env.fed.pic.cycle_balance(env.fed.graph_source);
    advance_seconds(&env, 60);
    let parked_burn = graph_before - env.fed.pic.cycle_balance(env.fed.graph_source);
    assert!(
        parked_burn < 50_000_000,
        "a rejected text delivery must park, not retry (burned {parked_burn} cycles in 60 s)"
    );

    // Re-attachment (writer authorization, then the shard's target) releases the parked log.
    let reattached: Result<(), String> = router_update(
        &env,
        canister,
        "admin_attach_shard_canister",
        Encode!(&0_u32, &env.fed.graph_source).expect("encode text writer re-attach"),
    );
    reattached.expect("text canister re-attaches the graph shard as a writer");
    let retargeted: Result<(), String> = router_update(
        &env,
        env.fed.graph_source,
        "admin_set_text_canister",
        Encode!(&label_raw, &property_raw, &canister).expect("encode graph text target"),
    );
    retargeted.expect("graph shard re-records the text target");
    advance_seconds(&env, 20);
    drain_text_flush(&env, canister);
    assert_eq!(
        text_stats(&env).ndocs,
        delivered.ndocs + 1,
        "the parked document is delivered after re-attachment"
    );

    // Router unregistration detaches the shard from the text canister: the shard canister's
    // write probe (a delete of an unknown key, a no-op when admitted) is admitted before and
    // refused by the writer guard after.
    assert!(
        shard_delete_probe(&env, canister).is_ok(),
        "an attached shard is a text writer"
    );
    let bytes = env
        .fed
        .pic
        .update_call(
            env.fed.router,
            env.fed.admin,
            "unregister_shard",
            Encode!(&GRAPH_NAME.to_string(), &ShardId::new(0)).expect("encode unregister_shard"),
        )
        .expect("unregister_shard call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode unregister_shard")
        .expect("unregister_shard succeeds");
    assert!(
        shard_delete_probe(&env, canister).is_err(),
        "an unregistered shard is no longer a text writer"
    );
}

/// `delete_docs` of an unknown key sent as the graph shard canister: `Err` carries the writer
/// guard's rejection.
fn shard_delete_probe(env: &Env, canister: Principal) -> Result<(), String> {
    let bytes = env
        .fed
        .pic
        .update_call(
            canister,
            env.fed.graph_source,
            "delete_docs",
            Encode!(&vec![u64::MAX]).expect("encode delete probe"),
        )
        .map_err(|reject| format!("{reject:?}"))?;
    Decode!(&bytes, Result<(), String>).expect("decode delete probe")
}

/// Advances PocketIC time one second at a time so canister timers fire as they would live.
fn advance_seconds(env: &Env, seconds: u64) {
    for _ in 0..seconds {
        env.fed.pic.advance_time(std::time::Duration::from_secs(1));
        env.fed.pic.tick();
    }
}

/// Drives bounded `admin_flush` steps until the text canister reports its pending log drained.
fn drain_text_flush(env: &Env, canister: Principal) {
    for _ in 0..16 {
        let bytes = env
            .fed
            .pic
            .update_call(
                canister,
                env.fed.router,
                "admin_flush",
                Encode!(&()).expect("encode admin_flush"),
            )
            .unwrap_or_else(|e| panic!("admin_flush: {e:?}"));
        let report: text_canister::FlushReport =
            Decode!(&bytes, text_canister::FlushReport).expect("decode flush report");
        if report.done {
            return;
        }
    }
    panic!("text canister did not drain its pending log within 16 flush steps");
}

/// One Router-originated update call returning `Result<(), String>`.
fn router_update(
    env: &Env,
    canister: Principal,
    method: &str,
    args: Vec<u8>,
) -> Result<(), String> {
    let bytes = env
        .fed
        .pic
        .update_call(canister, env.fed.router, method, args)
        .unwrap_or_else(|e| panic!("{method}: {e:?}"));
    Decode!(&bytes, Result<(), String>).unwrap_or_else(|e| panic!("decode {method}: {e}"))
}
