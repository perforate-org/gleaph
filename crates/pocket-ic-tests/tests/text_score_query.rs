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
    // Re-pinned (plan 0332): the absent `ANALYZER` clause now resolves to the
    // multilingual composite (id 0); this leg's assertions are bigram-specific, so
    // the definition pins `ANALYZER unicode_bigram` explicitly through the GQL DDL
    // surface — the id-1 pipeline is byte-unchanged by the promotion.
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "text-ddl");
    get_text_index(env)
}

fn get_text_index(env: &Env) -> TextIndexInfo {
    get_text_index_named(env, INDEX_NAME)
}

fn get_text_index_named(env: &Env, index_name: &str) -> TextIndexInfo {
    let bytes = env
        .fed
        .pic
        .query_call(
            env.fed.router,
            env.fed.admin,
            "get_text_index",
            Encode!(&GRAPH_NAME.to_string(), &index_name.to_string()).expect("encode"),
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
    drive_to_ready_for(env, args, INDEX_NAME)
}

fn drive_to_ready_for(env: &Env, args: &ApplySchemaMigrationArgs, index_name: &str) {
    let prepare =
        try_apply_once(env, args).unwrap_or_else(|err| panic!("prepare rejected: {err:?}"));
    assert!(matches!(
        prepare.status,
        SchemaMigrationApplyStatus::Progress(_)
    ));
    for step in 0..16 {
        let status_now = get_text_index_named(env, index_name).status;
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
    flush_until_done_for(env, INDEX_NAME)
}

fn flush_until_done_for(env: &Env, index_name: &str) {
    let canister = get_text_index_named(env, index_name)
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
    // Matches the definition (pinned ANALYZER unicode_bigram, plan 0332 re-pin).
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
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

// -- Plan 0329: compound WHERE-threshold + ORDER BY top-k lowering ----------------------------

const COMBINED_QUERY: &str = "MATCH (d:Document) \
     WHERE text_score(d.bio, $query) > $min \
     RETURN ELEMENT_ID(d) AS d_id, text_score(d.bio, $query) AS score \
     ORDER BY text_score(d.bio, $query) DESC LIMIT 10";

const COMBINED_LIMIT_1_QUERY: &str = "MATCH (d:Document) \
     WHERE text_score(d.bio, $query) > $min \
     RETURN ELEMENT_ID(d) AS d_id, text_score(d.bio, $query) AS score \
     ORDER BY text_score(d.bio, $query) DESC LIMIT 1";

fn combined_query_params(query: &str, min: f64) -> Vec<u8> {
    encode_gql_params_blob(vec![
        ("query".to_string(), Value::Text(query.to_string())),
        ("min".to_string(), Value::Float64(min)),
    ])
    .expect("encode params")
}

#[test]
fn text_score_compound_threshold_topk_lowers_and_ranks() {
    let wired = bootstrap_with_active_release();
    let env = Env {
        fed: finish_provision_wired_single_shard_federation(wired),
    };

    // Same frequency-discriminated corpus as the plan 0297 leg: heavy(0) < light(1).
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    seed_text_vertex(&env, "wombat wombat wombat");
    seed_text_vertex(&env, "wombat");
    seed_text_vertex(&env, "unrelated zebra");

    let info = create_text_index_definition(&env);
    env.fed.pic.add_cycles(
        info.canister.expect("provisioned canister attached"),
        20_000_000_000_000,
    );
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);

    // Matches the definition (pinned ANALYZER unicode_bigram, plan 0332 re-pin).
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    let args = migration_args(MIGRATION_ID, &statement);
    drive_to_ready(&env, &args);
    assert_eq!(get_text_index(&env).status, TextIndexStatusView::Ready);
    flush_until_done(&env);

    // Plain top-k reference: both wombat docs rank, heavier first.
    let plain = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("wombat", 10));
    assert_eq!(plain.row_count, 2);
    let plain_rows = scored_rows(&plain);
    assert!(plain_rows[0].1 > plain_rows[1].1, "frequency discriminates");
    let weakest = plain_rows[1].1;

    // LEG 1 — COMBINED positive: the fused compound scan returns ranked rows in
    // frequency-discriminated order, every returned score > $min, alias bound.
    let combined = gql_query_with_params_as_admin(
        &env.fed,
        COMBINED_QUERY,
        combined_query_params("wombat", 0.0),
    );
    assert_eq!(combined.row_count, 2, "both docs above the floor rank");
    let combined_rows = scored_rows(&combined);
    assert_eq!(
        combined_rows, plain_rows,
        "compound order matches plain top-k"
    );
    assert!(combined_rows.iter().all(|(_, score)| *score > 0.0));

    // LEG 2 — threshold enforcement: a $min above the weakest plain top-k score returns
    // strictly fewer rows than the plain top-k, and every remaining score > $min.
    let strict_min = weakest + 0.5;
    let strict = gql_query_with_params_as_admin(
        &env.fed,
        COMBINED_QUERY,
        combined_query_params("wombat", strict_min),
    );
    assert_eq!(strict.row_count, 1, "only the strongest doc survives $min");
    let strict_rows = scored_rows(&strict);
    assert_eq!(strict_rows[0].0, plain_rows[0].0, "the heavy doc survives");
    assert!(
        strict_rows[0].1 > strict_min,
        "every returned score must exceed the threshold"
    );

    // LEG 3 — empty: $min above every score returns 0 rows, no error.
    let empty_min = plain_rows[0].1 + 0.5;
    let empty = gql_query_with_params_as_admin(
        &env.fed,
        COMBINED_QUERY,
        combined_query_params("wombat", empty_min),
    );
    assert_eq!(empty.row_count, 0, "nothing survives the impossible floor");

    // LEG 4 — fail-closed: WHERE and ORDER BY disagreeing on the query literal keeps the
    // shape unfused and the Router rejects the residual mention.
    const MISMATCHED_QUERY: &str = "MATCH (d:Document) \
         WHERE text_score(d.bio, 'other') > 0.0 \
         RETURN ELEMENT_ID(d) AS d_id, text_score(d.bio, $query) AS score \
         ORDER BY text_score(d.bio, $query) DESC LIMIT 10";
    let mismatch_err = raw_gql_query(&env, MISMATCHED_QUERY, combined_query_params("wombat", 0.0))
        .expect_err("disagreeing query literals must fail closed");
    let mismatch_message = mismatch_err.to_string();
    assert!(
        mismatch_message
            .contains("residual text_score references are only supported as projected expressions")
            || mismatch_message.contains("did not lower into a TextScan"),
        "unexpected mismatch error: {mismatch_message}"
    );

    // LEG 5 — compound + LIMIT clamp sanity: LIMIT 1 returns the single best row above
    // $min (the frequency-heaviest wombat doc).
    let capped = gql_query_with_params_as_admin(
        &env.fed,
        COMBINED_LIMIT_1_QUERY,
        combined_query_params("wombat", 0.0),
    );
    assert_eq!(capped.row_count, 1, "LIMIT clamps the compound scan");
    let capped_rows = scored_rows(&capped);
    assert_eq!(
        capped_rows[0], combined_rows[0],
        "the cap keeps the best row"
    );

    // Determinism: an identical compound re-run returns the identical order.
    let replay = gql_query_with_params_as_admin(
        &env.fed,
        COMBINED_QUERY,
        combined_query_params("wombat", 0.0),
    );
    assert_eq!(
        scored_rows(&replay),
        combined_rows,
        "merge must be deterministic"
    );
}

// -- Plan 0331: ANALYZER mecab (ANALYZER_ID=2) + stable-resident ipadic dictionary -----

/// The pinned ipadic artifact source (plan 0334): the immutable PyPI `ipadic 1.0.0`
/// sdist — the compiled MeCab-format 2.7.0 utf8 four-image set. (The Debian snapshot
/// `mecab-ipadic-utf8` .deb was evaluated first per the plan and REJECTED: it is an
/// install-time stub — the binary images are built by the package postinst on the
/// target machine, so the .deb carries no sys.dic/unk.dic/matrix.bin/char.bin.)
/// Tarball SHA-256: f5923d31eca6131acaaf18ed28d8998665b1347b640d3a6476f64650e9a71c07.
/// Per-image SHA-256 (recorded in the plan audit):
///   sys.dic    223af63996d5d9a104d8c8baaf32029fbbf6a370d7dff33fe19fda2e92ac0ac1
///   unk.dic    f0bf15e3e28259f4470b9c1e775d98bef53207d4b09afcd6d8545209b7b53f88
///   matrix.bin ee44d7350cdcb680ebd699f83e121be1dc63310f8832d55bcb537a068177611a
///   char.bin   81bba502ae48fa005a374819f15e44452eb68086a8b323ec20a044d514400832
const MECAB_DICT_URL: &str = "https://files.pythonhosted.org/packages/e7/4e/c459f94d62a0bef89f866857bc51b9105aff236b83928618315b41a26b7b/ipadic-1.0.0.tar.gz";

/// Returns the MORPHDICT1 container bytes (the region-16 payload; the digest is over
/// THESE bytes) from the gitignored `crates/pocket-ic-tests/resources/mecrab/` cache,
/// fetching the pinned source (curl + tar) when absent. FAIL-CLOSED: an unreachable
/// artifact aborts the test with fetch instructions — the mecab legs never silently
/// skip.
fn fetch_mecab_container() -> Vec<u8> {
    const IMAGES: [&str; 4] = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
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
        let _ = std::fs::remove_file(&tarball);
        let _ = std::fs::remove_dir_all(dir.join("ipadic-1.0.0"));
    }
    let images: Vec<(String, Vec<u8>)> = IMAGES
        .iter()
        .map(|n| {
            (
                n.to_string(),
                std::fs::read(dir.join(n)).unwrap_or_else(|e| panic!("read {n}: {e}")),
            )
        })
        .collect();
    morph_dict::container::build(images)
}

/// Read-only dictionary status query on the text canister.
fn get_dict_status(env: &Env, canister: candid::Principal) -> text_canister::DictStatus {
    let bytes = env
        .fed
        .pic
        .query_call(
            canister,
            env.fed.router,
            "admin_get_dict_status",
            Encode!(&()).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("admin_get_dict_status: {e:?}"));
    Decode!(&bytes, text_canister::DictStatus).expect("decode status")
}

/// Calls a text-canister admin endpoint as the Router (its controller).
fn call_text_canister<R: candid::CandidType + serde::de::DeserializeOwned>(
    env: &Env,
    canister: candid::Principal,
    method: &str,
    args: &impl candid::CandidType,
) -> R {
    let bytes = env
        .fed
        .pic
        .update_call(
            canister,
            env.fed.router,
            method,
            Encode!(args).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("{method} on text canister: {e:?}"));
    Decode!(&bytes, Result<R, String>)
        .expect("decode reply")
        .expect("reply ok")
}

const MECAB_INDEX_NAME: &str = "text_score_mecab_idx";
const MECAB_MIGRATION_ID: &str = "000104_text_score_mecab";

#[test]
fn mecab_analyzer_recalls_lemma_through_gql_and_fails_closed() {
    let wired = bootstrap_with_active_release();
    let env = Env {
        fed: finish_provision_wired_single_shard_federation(wired),
    };

    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    // Corpus: two inflected 走った docs and one unrelated (ids ascend with insertion).
    seed_text_vertex(&env, "毎日公園を走った。");
    seed_text_vertex(&env, "走った走った走った");
    seed_text_vertex(&env, "unrelated zebra");

    // LEG 1 — GQL-surface admission with the ANALYZER clause: the provisioned canister
    // pins analyzer 2 (install-arg flow through Provision).
    let statement = format!(
        "CREATE TEXT INDEX {MECAB_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER mecab"
    );
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "mecab-ddl");
    let info = {
        let bytes = env
            .fed
            .pic
            .query_call(
                env.fed.router,
                env.fed.admin,
                "get_text_index",
                Encode!(&GRAPH_NAME.to_string(), &MECAB_INDEX_NAME).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("get_text_index on router: {e:?}"));
        Decode!(&bytes, Result<TextIndexInfo, RouterError>)
            .expect("decode get_text_index")
            .expect("definition exists")
    };
    assert_eq!(info.analyzer_id, 2, "the ANALYZER mecab clause pins id 2");
    let canister = info.canister.expect("provisioned canister attached");
    env.fed.pic.add_cycles(canister, 50_000_000_000_000);
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);

    // LEG 2 — fail-closed admission: an unknown analyzer name never reaches a durable
    // or remote effect.
    let nonsense = format!(
        "CREATE TEXT INDEX nonsense_analyzer_idx FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER nonsense"
    );
    let err = gleaph_pocket_ic_tests::gql_mutate_as_admin_expect_err(
        &env.fed,
        &nonsense,
        "mecab-ddl-nonsense",
    );
    assert!(
        err.to_string().contains("unknown ANALYZER name `nonsense`"),
        "unexpected admission error: {err}"
    );

    // LEG 3 — canister-side dictionary gates before finalize.
    // 3a: oversized chunk rejected.
    let oversized = vec![0u8; 1024 * 1024 + 1];
    let err: String = {
        let bytes = env
            .fed
            .pic
            .update_call(
                canister,
                env.fed.router,
                "admin_upload_dict_chunk",
                Encode!(&oversized).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("admin_upload_dict_chunk: {e:?}"));
        Decode!(&bytes, Result<u64, String>)
            .expect("decode reply")
            .expect_err("oversized chunk must reject")
    };
    assert!(
        err.contains("MAX_DICT_CHUNK_BYTES"),
        "unexpected chunk error: {err}"
    );
    // 3b: backfill registration HOLDS until finalize (recorded wording, no state change).
    let request = text_canister::RegisterTextBackfillRequest {
        text_index_id: gleaph_graph_kernel::federation::TextIndexId::new(1),
        graph_canister: env.fed.graph_source,
        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(3),
        index_name_id: gleaph_graph_kernel::entry::IndexNameId::from_raw(5),
        physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId::new(900_100).unwrap(),
        catalog_epoch: 1,
        scope: text_canister::TextBackfillScope {
            label_id: 1,
            property_id: gleaph_graph_kernel::entry::PropertyId::from_raw(1),
            analyzer_id: 2,
        },
    };
    let bytes = env
        .fed
        .pic
        .update_call(
            canister,
            env.fed.router,
            "admin_register_text_backfill",
            Encode!(&request).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("admin_register_text_backfill: {e:?}"));
    let hold: Result<text_canister::TextBackfillStatus, String> =
        Decode!(&bytes, Result<text_canister::TextBackfillStatus, String>).expect("decode reply");
    let hold = hold.expect_err("pre-finalize registration must hold");
    assert!(
        hold.contains("until the ipadic dictionary is finalized"),
        "unexpected hold wording: {hold}"
    );

    // Baseline upgrade on the SAME canister with the dictionary still absent (the open
    // path skips the rebind) — isolates pocket-ic's install overhead from the rebind.
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    let cycles_before = env.fed.pic.cycle_balance(canister);
    env.fed
        .pic
        .upgrade_canister(canister, text_wasm(), empty.clone(), Some(env.fed.admin))
        .expect("pre-finalize upgrade (no dictionary: no rebind work)");
    let install_overhead = cycles_before.saturating_sub(env.fed.pic.cycle_balance(canister));
    println!("plan-0334 upgrade install overhead (dict absent): {install_overhead}");

    // Upload the pinned container in 1 MiB chunks (52,930,923 bytes = 51 chunks).
    let dict = fetch_mecab_container();
    for (index, chunk) in dict.chunks(1024 * 1024).enumerate() {
        let total: u64 =
            call_text_canister(&env, canister, "admin_upload_dict_chunk", &chunk.to_vec());
        let expected = ((index + 1) * 1024 * 1024).min(dict.len());
        assert_eq!(total as usize, expected, "append accounting");
    }
    let status = get_dict_status(&env, canister);
    assert_eq!(status.state, text_canister::DictState::Uploading);
    assert_eq!(status.len, dict.len() as u64);
    assert_eq!(status.digest, None, "digest pins at finalize only");

    // LEG 4 — finalize with a wrong digest rejects WITHOUT touching state.
    let wrong_digest = xxhash_rust::xxh3::xxh3_128(&dict) ^ 1;
    let bytes = env
        .fed
        .pic
        .update_call(
            canister,
            env.fed.router,
            "admin_finalize_dict_upload",
            Encode!(&wrong_digest).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("admin_finalize_dict_upload: {e:?}"));
    let mismatch: Result<text_canister::DictStatus, String> =
        Decode!(&bytes, Result<text_canister::DictStatus, String>).expect("decode reply");
    let mismatch = mismatch.expect_err("wrong digest must reject");
    assert!(
        mismatch.contains("digest mismatch"),
        "unexpected: {mismatch}"
    );
    let status = get_dict_status(&env, canister);
    assert_eq!(status.state, text_canister::DictState::Uploading);

    // LEG 5 — correct digest finalizes; an exact replay is an idempotent no-op.
    // Instruction accounting (plan 0331 validation): cycle delta across the finalize
    // call covers the zstd decode + tokenizer build of the eager load (recorded in the
    // plan audit).
    let digest = xxhash_rust::xxh3::xxh3_128(&dict);
    let cycles_before = env.fed.pic.cycle_balance(canister);
    let finalized: text_canister::DictStatus =
        call_text_canister(&env, canister, "admin_finalize_dict_upload", &digest);
    let cycles_after = env.fed.pic.cycle_balance(canister);
    println!(
        "plan-0334 finalize (digest + container validation + resident-set memcpy) cycles: {}",
        cycles_before.saturating_sub(cycles_after)
    );
    assert_eq!(finalized.state, text_canister::DictState::Finalized);
    assert_eq!(finalized.digest, Some(digest));
    assert_eq!(finalized.len, dict.len() as u64);
    let replay: text_canister::DictStatus =
        call_text_canister(&env, canister, "admin_finalize_dict_upload", &digest);
    assert_eq!(replay, finalized, "exact re-finalize is a no-op");

    // LEG 5b — post_upgrade rebind measurement (the plan 0334 headline): the DELTA
    // against the pre-finalize upgrade of the SAME canister isolates the rebind work
    // (structural container validation + resident-set memcpy over batched stable
    // reads; NO decode, NO full-container copy; the feature region stays lazy over
    // stable memory). Recorded vs the 0331 4.75B-cycle eager-decode baseline.
    let cycles_before_upgrade = env.fed.pic.cycle_balance(canister);
    env.fed
        .pic
        .upgrade_canister(canister, text_wasm(), empty, Some(env.fed.admin))
        .expect("in-place upgrade of the text canister (rebind leg)");
    let rebind_cycles = cycles_before_upgrade.saturating_sub(env.fed.pic.cycle_balance(canister));
    let rebind_delta = rebind_cycles.saturating_sub(install_overhead);
    println!(
        "plan-0334 post_upgrade total: {rebind_cycles}; rebind DELTA vs dict-absent upgrade: {rebind_delta}"
    );
    // Per the IC cost model the rebind is dominated by the unavoidable 4 KiB page
    // charges of the resident memcpy (~5.2K pages x 5,000 = ~26M) + structural
    // validation reads — MUST be orders below the 4.75B eager-decode baseline.
    assert!(
        rebind_delta < 500_000_000,
        "post_upgrade rebind delta {rebind_delta} cycles — expected ~10-100M (page-charge dominated), not the 4.75B baseline (total {rebind_cycles})"
    );

    // LEG 6 — the SAME registration that held before finalize now replays idempotently:
    // drive the migration (statement carries the matching ANALYZER clause) to Ready.
    let args = migration_args(
        MECAB_MIGRATION_ID,
        &format!(
            "CREATE TEXT INDEX {MECAB_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER mecab"
        ),
    );
    drive_to_ready_for(&env, &args, MECAB_INDEX_NAME);
    assert_eq!(
        get_text_index_named(&env, MECAB_INDEX_NAME).status,
        TextIndexStatusView::Ready
    );
    flush_until_done_for(&env, MECAB_INDEX_NAME);

    // LEG 7 — RECALL through GQL: doc 走った ranks under query 走る (lemma shares the
    // unit), the unrelated doc is absent, deterministic order, alias ride-along intact.
    let result = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("走る", 10));
    assert_eq!(result.row_count, 2, "both 走った docs recall under 走る");
    let rows = scored_rows(&result);
    assert!(rows[0].1 >= rows[1].1, "scores arrive descending");
    assert!(rows.iter().all(|(_, score)| *score > 0.0));
    let replay = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("走る", 10));
    assert_eq!(scored_rows(&replay), rows, "recall must be deterministic");

    // The unrelated doc stays out of the candidate set under a discriminating term.
    let zebra = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("zebra", 10));
    assert_eq!(zebra.row_count, 1, "zebra still matches its own doc");
}

#[test]
fn mecab_counter_leg_explicit_bigram_pin_does_not_recall_lemma() {
    let wired = bootstrap_with_active_release();
    let env = Env {
        fed: finish_provision_wired_single_shard_federation(wired),
    };
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    seed_text_vertex(&env, "毎日公園を走った。");

    // Declare via the GQL DDL surface with the explicit bigram pin (plan 0332 re-pin:
    // the ABSENT clause now defaults to the multilingual composite, so this counter-leg
    // pins `ANALYZER unicode_bigram` to keep testing the id-1 pipeline's no-lemma
    // behavior byte-unchanged).
    let info = create_text_index_definition(&env);
    env.fed.pic.add_cycles(
        info.canister.expect("provisioned canister attached"),
        20_000_000_000_000,
    );
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);

    // Explicit bigram pin (matches the definition; the id-1 pipeline is unchanged).
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    let args = migration_args(MIGRATION_ID, &statement);
    drive_to_ready(&env, &args);
    assert_eq!(get_text_index(&env).status, TextIndexStatusView::Ready);
    flush_until_done(&env);

    // Query 走る does NOT match the 走った doc under bigrams ([走っ, った] vs [走る]).
    let result = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("走る", 10));
    assert_eq!(result.row_count, 0, "bigram counter-leg: no lemma recall");
    // …while its own surface bigrams match (sanity that the corpus IS indexed).
    let own = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("走っ", 10));
    assert_eq!(own.row_count, 1, "bigram index matches its own unit");
}

// -- Plan 0332: the multilingual composite (ANALYZER_ID=0) is the DEFAULT -------------------

const COMPOSITE_MIGRATION_ID: &str = "000105_text_score_composite";

/// One composite index, four languages: the plan 0332 default-promotion leg. The
/// definition is declared with the ABSENT `ANALYZER` clause (the bare admin
/// endpoint) — proving end-to-end that the default resolves to the multilingual
/// composite (id 0) — then the same MPD container as id 2 finalizes into region 16
/// and the multilingual recall matrix recalls through GQL:
/// - Japanese 走った doc ⇄ 走る query (the mecab layer over {kanji∪kana} runs),
/// - Korean 학교에서 doc ⇄ 학교 query (the 조사 strip layer),
/// - English running doc ⇄ run query (the Porter layer),
/// - Chinese 数据库 doc ⇄ 数据 query (the pure-Han bigram fallback).
///
/// The dict-not-finalized hold applies to the id-0 index exactly like id 2 (the
/// DICT_REQUIRED gate is shared).
#[test]
fn multilingual_composite_default_recalls_across_languages() {
    let wired = bootstrap_with_active_release();
    let env = Env {
        fed: finish_provision_wired_single_shard_federation(wired),
    };
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    // One doc per language layer (insertion order ascends the docids).
    seed_text_vertex(&env, "毎日公園を走った。");
    seed_text_vertex(&env, "학교에서 공부했다");
    seed_text_vertex(&env, "I was running fast yesterday");
    seed_text_vertex(&env, "知识图谱数据库应用");
    seed_text_vertex(&env, "unrelated zebra");
    // Mixed-language doc for LEG 6: all four layers in ONE document, with terms
    // chosen to NOT overlap the single-language docs' query units above
    // (ねこ/walking/책에서/图书 vs 走る/학교/run/数据).
    seed_text_vertex(&env, "ねこが walking 중 책과 图书");

    // LEG 1 — DEFAULT admission: the bare admin endpoint carries NO analyzer
    // argument; the absent clause must resolve to the composite (id 0).
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
    let info: TextIndexInfo = Decode!(&bytes, Result<TextIndexInfo, RouterError>)
        .expect("decode create_text_index")
        .expect("provisioned definition created");
    assert_eq!(
        info.analyzer_id, 0,
        "the ABSENT clause must pin the multilingual composite (id 0)"
    );
    let canister = info.canister.expect("provisioned canister attached");
    env.fed.pic.add_cycles(canister, 50_000_000_000_000);
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);

    // LEG 2 — the DICT_REQUIRED gate covers id 0: backfill registration HOLDS
    // until the dictionary is finalized (same recorded wording as id 2).
    let request = text_canister::RegisterTextBackfillRequest {
        text_index_id: gleaph_graph_kernel::federation::TextIndexId::new(1),
        graph_canister: env.fed.graph_source,
        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(3),
        index_name_id: gleaph_graph_kernel::entry::IndexNameId::from_raw(5),
        physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId::new(900_100).unwrap(),
        catalog_epoch: 1,
        scope: text_canister::TextBackfillScope {
            label_id: 1,
            property_id: gleaph_graph_kernel::entry::PropertyId::from_raw(1),
            analyzer_id: 0,
        },
    };
    let bytes = env
        .fed
        .pic
        .update_call(
            canister,
            env.fed.router,
            "admin_register_text_backfill",
            Encode!(&request).expect("encode"),
        )
        .unwrap_or_else(|e| panic!("admin_register_text_backfill: {e:?}"));
    let hold: Result<text_canister::TextBackfillStatus, String> =
        Decode!(&bytes, Result<text_canister::TextBackfillStatus, String>).expect("decode reply");
    let hold = hold.expect_err("pre-finalize registration must hold for id 0 too");
    assert!(
        hold.contains("until the ipadic dictionary is finalized"),
        "unexpected hold wording: {hold}"
    );

    // LEG 3 — upload + finalize the SAME MPD container as id 2 (no new machinery).
    let dict = fetch_mecab_container();
    for chunk in dict.chunks(1024 * 1024) {
        let _total: u64 =
            call_text_canister(&env, canister, "admin_upload_dict_chunk", &chunk.to_vec());
    }
    let digest = xxhash_rust::xxh3::xxh3_128(&dict);
    let finalized: text_canister::DictStatus =
        call_text_canister(&env, canister, "admin_finalize_dict_upload", &digest);
    assert_eq!(finalized.state, text_canister::DictState::Finalized);

    // LEG 4 — the SAME registration that held now replays idempotently; the
    // migration statement carries the ABSENT clause (the default IS id 0).
    let statement = format!("CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY})");
    let args = migration_args(COMPOSITE_MIGRATION_ID, &statement);
    drive_to_ready_for(&env, &args, INDEX_NAME);
    assert_eq!(
        get_text_index_named(&env, INDEX_NAME).status,
        TextIndexStatusView::Ready
    );
    flush_until_done_for(&env, INDEX_NAME);

    // LEG 5 — the multilingual recall matrix through GQL, all on ONE index.
    // Japanese: 走った doc recalls under the lemma 走る.
    let jp = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("走る", 10));
    let jp_rows = scored_rows(&jp);
    assert_eq!(
        jp.row_count, 1,
        "mecab layer: 走った doc recalls under 走る"
    );
    assert!(jp_rows[0].1 > 0.0, "the hit carries a positive score");
    // Korean: 학교에서 doc recalls under the stem 학교 (조사 strip).
    let kr = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("학교", 10));
    assert_eq!(
        kr.row_count, 1,
        "조사 layer: 학교에서 doc recalls under 학교"
    );
    // English: running doc recalls under the Porter stem run.
    let en = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("run", 10));
    assert_eq!(
        en.row_count, 1,
        "Porter layer: running doc recalls under run"
    );
    // Chinese: 数据库 doc recalls under the bigram 数据.
    let zh = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("数据", 10));
    assert_eq!(
        zh.row_count, 1,
        "bigram fallback: 数据库 doc recalls under 数据"
    );
    // The unrelated doc stays out of every candidate set.
    let zebra = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("zebra", 10));
    assert_eq!(zebra.row_count, 1, "zebra still matches its own doc");
    // Cross-language precision: the English query does not surface the Japanese doc.
    let cross = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("run", 10));
    assert_ne!(
        cross.row_count, 2,
        "Porter stem must not leak into the Japanese doc's candidate set"
    );

    // LEG 6 — a MIXED-language query against the MIXED-language doc: every layer
    // dispatches inside ONE query string and all four units hit the same doc.
    // ねこ → mecab layer, walking → Porter walk, 책 (from 책과) → 조사 strip, 图书 → Han bigram.
    let mixed = gql_query_with_params_as_admin(
        &env.fed,
        QUERY,
        scored_query_params("ねこ walking 책 图书", 10),
    );
    assert_eq!(
        mixed.row_count, 1,
        "the mixed query must recall exactly the mixed-language doc"
    );
    assert!(
        scored_rows(&mixed)[0].1 > 0.0,
        "the mixed-language hit carries a positive score"
    );

    // Determinism: an identical re-run returns the identical order.
    let replay = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("走る", 10));
    assert_eq!(
        scored_rows(&replay),
        jp_rows,
        "recall must be deterministic"
    );
}
