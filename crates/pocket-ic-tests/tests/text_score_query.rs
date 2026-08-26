//! PocketIC E2E for plan 0297 slice 5a: `text_score(prop, query)` through GQL (top-k phase).
//!
//! Flow: reuse the ADR 0059 bootstrap (single-shard provision-wired federation, real
//! text-canister wasm, migration-driven CREATE TEXT INDEX). Once the definition reaches
//! `Ready` and the pending log is flushed, a scored top-k GQL query must return ranked
//! vertices with Float64 scores; before readiness the same call resolves as function-unknown
//! fail-closed.
//!
//! Run: `cargo test -p gleaph-pocket-ic-tests --test text_score_query`.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::wire::encode_gql_params_blob;
use gleaph_gql_ic_wire::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_migration_api::{
    ApplySchemaMigrationArgs, ApplySchemaMigrationArgsV1, ApplySchemaMigrationResult,
    ApplySchemaMigrationResultV1, SchemaMigrationApplyStatus, SchemaMigrationGraphSelector,
    SchemaMigrationProgressPhase,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, ProvisionWiredRouterEnv,
    finish_provision_wired_single_shard_federation, gql_query_with_params_as_admin,
    install_provision_wired_router, wasm_bytes,
};
use gleaph_router::types::{TextIndexInfo, TextIndexStatusView};
use pocket_ic::PocketIc;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

const INDEX_NAME: &str = "text_score_query_idx";
const MIGRATION_ID: &str = "000103_text_score_query";
const LABEL: &str = "Document";
const PROPERTY: &str = "bio";
/// Matches Provision's 1 MiB install-chunk bound.
const PUBLISH_CHUNK_BYTES: usize = 1024 * 1024;

struct Env {
    fed: FederationEnv,
}

// -- Wasm acquisition -------------------------------------------------------------------------

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
    ensure_wasm(
        "TEXT_INDEX_WASM",
        "text-canister",
        "text_canister.wasm",
        "pocket-ic-text-wasm",
    )
}

fn graph_wasm() -> Vec<u8> {
    wasm_bytes("GRAPH_WASM")
}

fn router_wasm() -> Vec<u8> {
    wasm_bytes("ROUTER_WASM")
}

// -- Provision calls --------------------------------------------------------------------------

#[allow(clippy::result_large_err)]
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
    Decode!(
        &bytes,
        Result<R, gleaph_provision::types::ArtifactError>
    )
    .expect("decode artifact reply")
    .expect("artifact reply ok")
}

#[allow(clippy::result_large_err)]
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
    Decode!(&bytes, Result<R, gleaph_provision::types::ReleaseError>)
        .expect("decode release reply")
        .expect("release reply ok")
}

fn publish_verified_artifact(
    pic: &PocketIc,
    admin: Principal,
    provision: Principal,
    kind: gleaph_provision::types::CanisterKind,
    wasm: &[u8],
) -> gleaph_provision::types::ArtifactId {
    use gleaph_provision::types::{
        ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload, ArtifactUploadChunkArgs,
        sha256,
    };

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
    let id = gleaph_provision::types::ArtifactId::new(kind, "0.1.0".to_owned(), full_sha);
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
    use gleaph_provision::types::{
        ArtifactId, CanisterKind, ReleaseActivateArgs, ReleaseId, ReleaseManifest,
        ReleasePublishArgs,
    };

    let dummy = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];
    let ids: Vec<ArtifactId> = vec![
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
    ];
    let _: ReleaseManifest = call_release(
        pic,
        admin,
        provision,
        "release_publish",
        &ReleasePublishArgs {
            release_id: ReleaseId("release-text-score-query-0297".to_owned()),
            artifact_ids: ids,
        },
    );
    let _: gleaph_provision::types::ReleaseActivateResult = call_release(
        pic,
        admin,
        provision,
        "release_activate",
        &ReleaseActivateArgs {
            release_id: ReleaseId("release-text-score-query-0297".to_owned()),
        },
    );
}

fn bootstrap_with_active_release() -> ProvisionWiredRouterEnv {
    let wired = install_provision_wired_router();
    activate_release(&wired.pic, wired.admin, wired.provision);
    wired
}

// -- Router / migration helpers ---------------------------------------------------------------

fn seed_text_vertex(env: &Env, bio: &str) {
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
            Err(err) => panic!("apply_schema_migration rejected: {err:?}"),
        }
    }
    unreachable!("retry loop must return or panic")
}

/// Drives the migration to `Applied` within the bounded-step budget.
fn drive_to_ready(env: &Env, args: &ApplySchemaMigrationArgs) {
    let prepare =
        try_apply_once(env, args).unwrap_or_else(|err| panic!("prepare rejected: {err:?}"));
    assert!(matches!(
        prepare.status,
        SchemaMigrationApplyStatus::Progress(_)
    ));
    for step in 0..16 {
        let status_now = get_text_index(env).status;
        if status_now == TextIndexStatusView::Ready {
            return;
        }
        let result = apply_retrying_busy(env, args, 8);
        assert!(
            matches!(result.status, SchemaMigrationApplyStatus::Progress(_)),
            "step {step}: unexpected terminal status before Ready: {:?}",
            result.status
        );
    }
    panic!("migration did not reach Ready within the bounded-step budget");
}

/// Flushes the canister pending log until done so ingested docs become searchable.
fn flush_until_done(env: &Env) {
    let canister = get_text_index(env)
        .canister
        .expect("provisioned text canister attached");
    for _ in 0..16 {
        let bytes = env
            .fed
            .pic
            .update_call(
                canister,
                env.fed.router,
                "admin_flush",
                Encode!(&()).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("admin_flush: {e:?}"));
        let report: text_canister::FlushReport =
            Decode!(&bytes, text_canister::FlushReport).expect("decode flush report");
        if report.done {
            return;
        }
        for _ in 0..4 {
            env.fed.pic.tick();
        }
    }
    panic!("text pending log did not drain within the flush budget");
}

// -- text_score queries -----------------------------------------------------------------------

fn raw_gql_query(
    env: &Env,
    query: &str,
    params_blob: Vec<u8>,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .fed
        .pic
        .query_call(
            env.fed.router,
            env.fed.admin,
            "gql_query",
            Encode!(
                &query.to_string(),
                &params_blob,
                &gleaph_graph_kernel::plan_exec::ReadMode::Eventual
            )
            .expect("encode gql_query"),
        )
        .expect("gql_query call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query result")
}

/// Runs one scored top-k query and returns `(element_id?, score)` per returned row in order.
fn scored_rows(result: &GqlQueryResult) -> Vec<(Option<String>, f64)> {
    let rows_blob = result.rows_blob.as_ref().expect("rows blob present");
    let wire = GqlWireRows::decode_blob(rows_blob).expect("decode rows");
    wire.rows
        .iter()
        .map(|row| {
            let columns: BTreeMap<String, GqlWireValue> = row
                .columns
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let id = columns.get("d_id").and_then(|value| match value {
                GqlWireValue::Text(text) => Some(text.clone()),
                // ELEMENT_ID encodes the global element id as fixed-length bytes.
                GqlWireValue::Bytes(bytes) => {
                    Some(bytes.iter().map(|b| format!("{b:02X}")).collect())
                }
                other => panic!("ELEMENT_ID must decode as Text or Bytes, got {other:?}"),
            });
            let score = match columns.get("score").expect("score column present") {
                GqlWireValue::Float64(score) => *score,
                other => panic!("score must be Float64, got {other:?}"),
            };
            (id, score)
        })
        .collect()
}

const QUERY: &str = "MATCH (d:Document) \
     RETURN ELEMENT_ID(d) AS d_id, text_score(d.bio, $query) AS score \
     ORDER BY text_score(d.bio, $query) DESC LIMIT 10";

fn scored_query_params(query: &str, k: i64) -> Vec<u8> {
    encode_gql_params_blob(vec![
        ("query".to_string(), Value::Text(query.to_string())),
        ("k".to_string(), Value::Int64(k)),
    ])
    .expect("encode params")
}

// -- Scenario ---------------------------------------------------------------------------------

#[test]
fn text_score_ranks_through_gql_after_ready_and_fails_closed_before() {
    let wired = bootstrap_with_active_release();
    let env = Env {
        fed: finish_provision_wired_single_shard_federation(wired),
    };

    // Corpus with discriminating term frequencies: two wombats docs, one unrelated.
    // Insertion order fixes vertex ids ascending: heavy(0) < light(1) < unrelated(2).
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    seed_text_vertex(&env, "wombat wombat wombat");
    seed_text_vertex(&env, "wombat");
    seed_text_vertex(&env, "unrelated zebra");

    // Declare + provision: born Backfilling (planner-invisible).
    let info = create_text_index_definition(&env);
    env.fed.pic.add_cycles(
        info.canister.expect("provisioned canister attached"),
        20_000_000_000_000,
    );
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);
    assert_eq!(info.status, TextIndexStatusView::Backfilling);

    // NEGATIVE: while Backfilling the definition is planner-invisible, so the call does not
    // lower into a TextScan and the Router rejects the residual mention fail-closed.
    let early_err = raw_gql_query(&env, QUERY, scored_query_params("wombat", 10))
        .expect_err("pre-ready text_score must fail closed");
    let message = early_err.to_string();
    assert!(
        message.contains("did not lower into a TextScan"),
        "unexpected pre-ready error: {message}"
    );

    // Drive the migration to convergence, then flush so docs are searchable.
    let statement =
        format!("CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY})");
    let args = migration_args(MIGRATION_ID, &statement);
    drive_to_ready(&env, &args);
    assert_eq!(get_text_index(&env).status, TextIndexStatusView::Ready);
    flush_until_done(&env);

    // POSITIVE: ranked results through GQL. Both wombat docs come back, heavier doc first;
    // the unrelated doc is not part of the candidate set.
    let result = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("wombat", 10));
    assert_eq!(result.row_count, 2, "only matching docs rank");
    let rows = scored_rows(&result);
    assert_eq!(rows.len(), 2);
    let ids: Vec<String> = rows.iter().map(|(id, _)| id.clone().expect("id")).collect();
    assert_ne!(ids[0], ids[1], "two distinct documents");
    assert!(
        rows[0].1 >= rows[1].1,
        "scores arrive descending: [{}, {}]",
        rows[0].1,
        rows[1].1
    );
    assert!(
        rows[0].1 > 0.0,
        "matching docs carry positive engine scores"
    );

    // The heavier-frequency doc ranks strictly above the single-term doc under the
    // frequency-sensitive v0 scorer.
    assert_ne!(rows[0].1, rows[1].1, "frequency must discriminate");

    // Determinism: an identical re-run returns the identical order.
    let replay = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("wombat", 10));
    assert_eq!(scored_rows(&replay), rows, "merge must be deterministic");

    // TOP-K cap: a literal LIMIT 1 keeps only the best-ranked row.
    const CAPPED_QUERY: &str = "MATCH (d:Document) \
         RETURN ELEMENT_ID(d) AS d_id, text_score(d.bio, $query) AS score \
         ORDER BY text_score(d.bio, $query) DESC LIMIT 1";
    let capped =
        gql_query_with_params_as_admin(&env.fed, CAPPED_QUERY, scored_query_params("wombat", 1));
    assert_eq!(capped.row_count, 1, "LIMIT clamps the returned rows");
    let capped_rows = scored_rows(&capped);
    assert_eq!(capped_rows[0], rows[0], "the cap keeps the top-ranked row");

    // A different query term selects exactly the unrelated doc.
    let zebra = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("zebra", 5));
    assert_eq!(zebra.row_count, 1);
    let zebra_rows = scored_rows(&zebra);
    assert!(zebra_rows[0].1 > 0.0);
}
