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
    finish_provision_wired_single_shard_federation, gql_mutate_as_admin,
    gql_query_with_params_as_admin, gql_query_with_params_on_router,
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
    /// The Provision canister (catalog owner); captured from the wired bootstrap before
    /// the federation finish consumes it (plan 0335 todo-4 catalog seeding).
    provision: Principal,
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
    let provision = wired.provision;
    let env = Env {
        provision,
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
    let provision = wired.provision;
    let env = Env {
        provision,
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

// -- Plan 0331: ANALYZER japanese (ANALYZER_ID=2, plan 0343 rename mecab -> japanese) + stable-resident ipadic dictionary -----

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
/// skip. Size note (management-verified correction): the container is the FULL
/// `container::build` output — 52,931,159 B = image sum 52,930,923 + 236 B framing
/// (header + entry table); digests/lengths pin these full bytes, not the image sum.
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

/// Plan 0342 todo-4 fixture leg: seeds the Provision dictionary catalog with the FRAMED
/// REAL MPD container (N independent zstd frames, level 19, adaptive slice-to-cap so each
/// frame fits the kernel relay cap) so the post-install relay (`relay_dict_catalog`, todo 3)
/// auto-finalizes every dictionary-required text canister this test provisions. MUST run
/// before the first `CREATE TEXT INDEX`; the management's size correction is load-bearing
/// here: the catalog's `raw_len`/`raw_digest` pin the FULL `container::build` output
/// (52,931,159 B = image sum + 236 B framing), not the four-image sum. Frame i = catalog
/// row i = relay call i; each row's `frame_digest` is verified at upload time. Returns the
/// (frames, raw) pair for the manual framed-mode legs to reuse without re-reading.
/// The (raw, frames) pair MUST be computed BEFORE the PocketIC server boots (the zstd-19
/// framing takes ~85 s, longer than the server idle TTL) and passed in here.
fn seed_dictionary_catalog(env: &Env, raw: &[u8], frames: &[Vec<u8>]) {
    seed_dictionary_catalog_for(env, "ipadic", "2.7.0", raw, frames)
}

/// Plan 0341 todo 3: key-parameterized catalog seeding — the korean leg pins the
/// mecab-ko-dic 2.1.1-20180720 entry (the provision-side
/// `dict_catalog_key_for_analyzer(3)` mapping) while every older leg keeps the
/// ipadic entry through the wrapper above (zero behavior change there).
fn seed_dictionary_catalog_for(
    env: &Env,
    kind: &str,
    version: &str,
    raw: &[u8],
    frames: &[Vec<u8>],
) {
    let key = gleaph_provision::types::DictCatalogKey {
        kind: kind.to_owned(),
        version: version.to_owned(),
    };
    for (chunk_index, frame) in frames.iter().enumerate() {
        let bytes = env
            .fed
            .pic
            .update_call(
                env.provision,
                env.fed.admin,
                "admin_upload_dict_catalog_chunk",
                Encode!(&gleaph_provision::types::DictCatalogUploadChunkArgs {
                    key: key.clone(),
                    chunk_index: chunk_index as u32,
                    frame_digest: xxhash_rust::xxh3::xxh3_128(frame),
                    bytes: frame.clone(),
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
        status.expect("catalog frame accepted");
    }
    let bytes = env
        .fed
        .pic
        .update_call(
            env.provision,
            env.fed.admin,
            "admin_finalize_dict_catalog",
            Encode!(&gleaph_provision::types::DictCatalogFinalizeArgs {
                key: key.clone(),
                raw_digest: xxhash_rust::xxh3::xxh3_128(raw),
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
}

/// Returns the KO-DIC container bytes (the region-16 payload for analyzer 3) from the
/// gitignored `crates/pocket-ic-tests/resources/mecab-ko-dic/` build outputs (plan 0341
/// todo 1 artifact pipeline: sys.dic + unk.dic + matrix.bin + char.bin). FAIL-CLOSED:
/// absent images abort with build instructions — the korean leg never silently skips.
/// Size pins (plan 0341 gates): FULL `container::build` output 101,411,116 B; zstd-19
/// framed total ~20.3 MB (20.0%), within the 32 MiB provision cap.
fn fetch_ko_container() -> Vec<u8> {
    const IMAGES: [&str; 4] = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("mecab-ko-dic");
    for name in IMAGES {
        assert!(
            dir.join(name).exists(),
            "missing ko-dic image {name} under crates/pocket-ic-tests/resources/mecab-ko-dic/ — \
             build it per plan 0341 todo 1 (mecab-dict-index -f utf-8 -t utf-8 over mecab-ko-dic-2.1.1-20180720)"
        );
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

const MECAB_INDEX_NAME: &str = "text_score_mecab_idx";
const MECAB_MIGRATION_ID: &str = "000104_text_score_mecab";
const KOREAN_INDEX_NAME: &str = "text_score_korean_idx";
const KOREAN_PROBE_INDEX_NAME: &str = "text_score_korean_unseeded_probe_idx";
const KOREAN_MIGRATION_ID: &str = "000105_text_score_korean";
const KOREAN_CATALOG_KIND: &str = "korean";
const KOREAN_CATALOG_VERSION: &str = "2.1.1-20180720";

#[test]
fn mecab_analyzer_recalls_lemma_through_gql_and_fails_closed() {
    // Framed container FIRST: zstd-19 framing idles ~85 s, past the PocketIC server idle
    // TTL, so the bytes must exist before the server boots.
    let raw = fetch_mecab_container();
    let frames = gleaph_pocket_ic_tests::framed_container(&raw);
    let wired = bootstrap_with_active_release();
    let provision = wired.provision;
    let env = Env {
        provision,
        fed: finish_provision_wired_single_shard_federation(wired),
    };

    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    // Corpus: two inflected 走った docs and one unrelated (ids ascend with insertion).
    seed_text_vertex(&env, "毎日公園を走った。");
    seed_text_vertex(&env, "走った走った走った");
    seed_text_vertex(&env, "unrelated zebra");

    // LEG 0 — catalog seeding (plan 0342 todo 4): frame the REAL MPD container once and
    // pin it in the Provision catalog; the post-install relay then auto-finalizes the
    // dictionary on every dictionary-required text canister this test provisions.
    seed_dictionary_catalog(&env, &raw, &frames);

    // LEG 0b — gate-2 measurement (plan 0342): the framed path decodes EACH frame on its own
    // upload call (verify frame digest → ruzstd-decode THIS frame → append to region 16),
    // so the single-call decompression spike is gone. The relayed canister is born
    // Finalized, so the per-call cost is measured on a bare id-2 canister (installed
    // directly, no relay) driven through the framed path manually — the exact code path the
    // relay invokes. The MAX per-frame upload call's cycle delta is the gate-2 number, and
    // the finalize (one-pass region-16 hash under 案A) is recorded separately.
    let bare = env.fed.pic.create_canister();
    env.fed.pic.add_cycles(bare, 50_000_000_000_000);
    env.fed.pic.install_canister(
        bare,
        text_wasm(),
        Encode!(&text_canister::TextCanisterInitArgs {
            controller: Some(env.fed.router),
            analyzer_id: Some(text_canister::ANALYZER_MECAB),
            dict_relay_caller: None,
            // Plan 0343: the bare id-2 budget leg pins kinds=[Japanese] explicitly
            // (dict_required(2, [Japanese]) is the only true pair for id 2).
            kinds: Some(vec![
                gleaph_graph_kernel::provisioning::dictionary::DictKind::Japanese,
            ]),
        })
        .expect("encode bare init"),
        None,
    );
    use gleaph_graph_kernel::provisioning::dictionary::{
        CompressedDictFinalize, CompressedDictUpload,
    };
    let raw_len = raw.len() as u64;
    let stable_before = env.fed.pic.get_stable_memory(bare).len();
    let mut max_frame_cycles = 0u128;
    let mut first_frame_cycles = 0u128;
    for (i, frame) in frames.iter().enumerate() {
        let cycles_before = env.fed.pic.cycle_balance(bare);
        let bytes = env
            .fed
            .pic
            .update_call(
                bare,
                env.fed.router,
                "admin_upload_dict_chunk",
                Encode!(
                    &frame.clone(),
                    &Some(CompressedDictUpload {
                        frame_digest: xxhash_rust::xxh3::xxh3_128(frame),
                        raw_len,
                    })
                )
                .expect("encode framed chunk"),
            )
            .unwrap_or_else(|e| panic!("admin_upload_dict_chunk: {e:?}"));
        let total: u64 = Decode!(&bytes, Result<u64, String>)
            .expect("decode upload reply")
            .expect("framed chunk ok");
        assert!(total > 0, "framed decode appends raw bytes");
        let call_cycles = cycles_before.saturating_sub(env.fed.pic.cycle_balance(bare));
        if i == 0 {
            first_frame_cycles = call_cycles;
        }
        max_frame_cycles = max_frame_cycles.max(call_cycles);
    }
    let raw_digest = xxhash_rust::xxh3::xxh3_128(&raw);
    let cycles_before_finalize = env.fed.pic.cycle_balance(bare);
    let bytes = env
        .fed
        .pic
        .update_call(
            bare,
            env.fed.router,
            "admin_finalize_dict_upload",
            Encode!(
                &raw_digest,
                &Some(CompressedDictFinalize {
                    raw_digest,
                    raw_len,
                })
            )
            .expect("encode framed finalize"),
        )
        .unwrap_or_else(|e| panic!("admin_finalize_dict_upload: {e:?}"));
    let finalized: text_canister::DictStatus =
        Decode!(&bytes, Result<text_canister::DictStatus, String>)
            .expect("decode finalize reply")
            .expect("framed finalize ok");
    let finalize_cycles = cycles_before_finalize.saturating_sub(env.fed.pic.cycle_balance(bare));
    println!(
        "plan-0342 gate-2 (案A): first-frame upload call cycles {first_frame_cycles}; max per-frame upload call cycles {max_frame_cycles}; finalize (one-pass region-16 hash + validation + pin) cycles {finalize_cycles}"
    );
    assert_eq!(finalized.state, text_canister::DictState::Finalized);
    // Gate 2: each per-frame decode call must fit ONE update message (~10B instructions).
    assert!(
        max_frame_cycles < 10_000_000_000,
        "a per-frame decode call took {max_frame_cycles} cycles — exceeds the update-message budget"
    );
    // Stable-write volume (plan 0342 gate): the framed path writes ONLY the raw container to
    // region 16 (no compressed staging — region 17 is gone). Stable memory is bucketed
    // (MemoryManager grows in 128-page = 8 MiB buckets, one bucket minimum per touched
    // memory), so the absolute size (~176 MB) is bucket overhead, not data. The regression
    // signal is the DELTA across the upload: raw bytes + at most one trailing partial
    // bucket. A resurrected 10.9 MB compressed staging copy would cost ~2 extra buckets
    // and fail this bound.
    let stable_bytes = env.fed.pic.get_stable_memory(bare).len();
    println!(
        "plan-0342 stable-write volume: bare id-2 canister stable memory after framed dictionary upload = {stable_bytes} B ({:.1} MB; raw container {:.1} MB + non-dict regions)",
        stable_bytes as f64 / (1024.0 * 1024.0),
        raw.len() as f64 / (1024.0 * 1024.0),
    );
    assert!(
        stable_bytes.saturating_sub(stable_before) < raw.len() + 8 * 1024 * 1024,
        "stable-memory delta exceeds raw + one bucket — compressed staging may have been resurrected"
    );

    // LEG 1 — GQL-surface admission with the ANALYZER clause: the provisioned canister
    // pins analyzer 2 (install-arg flow through Provision).
    let statement = format!(
        "CREATE TEXT INDEX {MECAB_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER japanese WITH DICTIONARY japanese"
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
    assert_eq!(
        info.analyzer_id, 2,
        "the ANALYZER japanese clause pins id 2"
    );
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

    // LEG 3 — the post-install RELAY (plan 0342 todo 3): provisioning a
    // dictionary-required analyzer auto-relayed the catalog's frames (one frame per call,
    // each decoded immediately) and finalized it — no manual dictionary steps. Assert the
    // full observable end state.
    let status = get_dict_status(&env, canister);
    assert_eq!(
        status.state,
        text_canister::DictState::Finalized,
        "the relay must finalize the dictionary during provisioning"
    );
    let dict = fetch_mecab_container();
    let digest = xxhash_rust::xxh3::xxh3_128(&dict);
    assert_eq!(
        status.digest,
        Some(digest),
        "relay pins the raw container digest"
    );
    assert_eq!(
        status.len,
        dict.len() as u64,
        "raw length matches the container"
    );
    // The relay caller (Provision) is authorized on the relay endpoints; a THIRD
    // principal is not (plan 0335 §5-2 guard scoping — exercised through the real wasm
    // guard, whose Err surfaces as a PocketIC CanisterReject).
    let outsider = Principal::from_slice(&[0x3D; 29]);
    let err = env
        .fed
        .pic
        .update_call(
            canister,
            outsider,
            "admin_finalize_dict_upload",
            Encode!(
                &digest,
                &None::<gleaph_graph_kernel::provisioning::dictionary::CompressedDictFinalize>
            )
            .expect("encode"),
        )
        .expect_err("a third principal must not finalize the dictionary");
    assert!(
        err.reject_message
            .contains("is neither the text index controller"),
        "unexpected guard reject: {}",
        err.reject_message
    );

    // LEG 4 — post_upgrade rebind on the relayed canister (the plan 0342 invariant: the
    // framed path must NOT touch open/upgrade). The dictionary is already Finalized,
    // so this upgrade exercises the 0334 rebind: structural validation + resident memcpy,
    // NO decode. The install-overhead BASELINE is measured IN THIS RUN on a bare id-1
    // canister (same wasm, no dictionary → the open path skips the rebind), so the delta
    // isolates the rebind work under identical PocketIC conditions.
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    let bare_id1 = env.fed.pic.create_canister();
    env.fed.pic.add_cycles(bare_id1, 50_000_000_000_000);
    env.fed
        .pic
        .set_controllers(bare_id1, None, vec![env.fed.admin, Principal::anonymous()])
        .expect("set bare controllers");
    env.fed.pic.install_canister(
        bare_id1,
        text_wasm(),
        Encode!(&text_canister::TextCanisterInitArgs {
            controller: Some(env.fed.admin),
            analyzer_id: Some(text_canister::ANALYZER_UNICODE_BIGRAM),
            dict_relay_caller: None,
            // Plan 0343: dict-less bigram baseline (kinds omitted = no dict).
            kinds: None,
        })
        .expect("encode bare id-1 init"),
        None,
    );
    let cycles_before_baseline = env.fed.pic.cycle_balance(bare_id1);
    env.fed
        .pic
        .upgrade_canister(bare_id1, text_wasm(), empty.clone(), Some(env.fed.admin))
        .expect("dict-absent upgrade (install-overhead baseline leg)");
    let install_overhead =
        cycles_before_baseline.saturating_sub(env.fed.pic.cycle_balance(bare_id1));
    println!("plan-0335 upgrade install overhead (bare id-1, dict absent): {install_overhead}");
    let cycles_before_upgrade = env.fed.pic.cycle_balance(canister);
    env.fed
        .pic
        .upgrade_canister(canister, text_wasm(), empty, Some(env.fed.admin))
        .expect("in-place upgrade of the relayed text canister (rebind leg)");
    let rebind_cycles = cycles_before_upgrade.saturating_sub(env.fed.pic.cycle_balance(canister));
    let rebind_delta = rebind_cycles.saturating_sub(install_overhead);
    println!(
        "plan-0335 post_upgrade over relayed dictionary: {rebind_cycles} total; rebind DELTA vs same-run install overhead: {rebind_delta}"
    );
    // Same bound as the 0334 gate: page-charge dominated, orders below the decode era.
    assert!(
        rebind_delta < 500_000_000,
        "post_upgrade rebind delta {rebind_delta} cycles — expected ~10-100M (page-charge dominated), not the decode-era baseline (total {rebind_cycles})"
    );

    // LEG 6 — the SAME registration that held before finalize now replays idempotently:
    // drive the migration (statement carries the matching ANALYZER clause) to Ready.
    let args = migration_args(
        MECAB_MIGRATION_ID,
        &format!(
            "CREATE TEXT INDEX {MECAB_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER japanese WITH DICTIONARY japanese"
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

/// Plan 0341 todo 3 — ko-dic relay E2E leg. Flow: hoisted ko container + level-19
/// adaptive framing BEFORE bootstrap (idle-TTL rule) → provision-wired federation →
/// corpus → analyzer-3 DDL against an UNSEEDED korean catalog fails closed with no
/// definition row → seed the korean catalog entry via FRAMED ingress → redefined
/// budget gate on the 101.4 MB container (bare id-3 canister: MAX per-frame upload
/// cycles + finalize one-pass hash cycles) → `ANALYZER korean` DDL → relay
/// auto-supplies ko-dic (ZERO manual dict steps) → Ready → backfill → GQL recall
/// (학교 → both docs, 공부 → one doc, zebra discriminates) → re-provision
/// idempotence → relay/resident/stable-$ figures.
#[test]
fn korean_analyzer_recalls_through_gql_via_relay() {
    // Hoist FIRST: the ko container is 101.4 MB raw (~20.3 MB framed); zstd-19 framing
    // idles minutes, far past the PocketIC server idle TTL, so the bytes must exist
    // before the server boots.
    let raw = fetch_ko_container();
    let frames = gleaph_pocket_ic_tests::framed_container(&raw);
    let framed_total: usize = frames.iter().map(|frame| frame.len()).sum();
    println!(
        "plan-0341 ko-dic container: raw {} B ({:.1} MB) -> zstd-19 {} frames, {} B total ({:.1}%)",
        raw.len(),
        raw.len() as f64 / (1024.0 * 1024.0),
        frames.len(),
        framed_total,
        100.0 * framed_total as f64 / raw.len() as f64,
    );
    assert!(
        framed_total < 32 * 1024 * 1024,
        "framed ko-dic {framed_total} B must fit the 32 MiB provision cap"
    );
    let wired = bootstrap_with_active_release();
    let provision = wired.provision;
    let env = Env {
        provision,
        fed: finish_provision_wired_single_shard_federation(wired),
    };
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    // Corpus: two 학교 docs and one unrelated (ids ascend with insertion).
    seed_text_vertex(&env, "학교에서 공부했다");
    seed_text_vertex(&env, "친구와 학교에 갔다");
    seed_text_vertex(&env, "unrelated zebra");

    // LEG F — fail-closed: analyzer-3 DDL with the korean catalog UNSEEDED must fail
    // (the relay has no Finalized entry to stream) and leave NO definition row. The
    // probe name is distinct so the failed job can never collide with the real index.
    let probe_statement = format!(
        "CREATE TEXT INDEX {KOREAN_PROBE_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER korean WITH DICTIONARY korean"
    );
    let probe_err = gleaph_pocket_ic_tests::gql_mutate_as_admin_expect_err(
        &env.fed,
        &probe_statement,
        "korean-ddl-unseeded",
    );
    assert!(
        !probe_err.to_string().is_empty(),
        "unseeded korean provisioning must surface an error"
    );
    let probe_lookup = env
        .fed
        .pic
        .query_call(
            env.fed.router,
            env.fed.admin,
            "get_text_index",
            Encode!(
                &GRAPH_NAME.to_string(),
                &KOREAN_PROBE_INDEX_NAME.to_string()
            )
            .expect("encode"),
        )
        .unwrap_or_else(|e| panic!("get_text_index call: {e:?}"));
    let probe_info: Result<TextIndexInfo, RouterError> =
        Decode!(&probe_lookup, Result<TextIndexInfo, RouterError>).expect("decode get_text_index");
    assert!(
        probe_info.is_err(),
        "failed unseeded provisioning must leave no definition row"
    );

    // LEG 0 — seed the KOREAN catalog entry via FRAMED ingress (frame i = row i +
    // frame_digest, verified per upload; raw_digest + raw_len pinned at finalize).
    seed_dictionary_catalog_for(
        &env,
        KOREAN_CATALOG_KIND,
        KOREAN_CATALOG_VERSION,
        &raw,
        &frames,
    );

    // LEG 0b — REDEFINED budget gate (post-0342 amendment: NOT the old single-finalize
    // gate): MAX per-frame upload cycles + finalize one-pass hash cycles on the
    // 101.4 MB container, measured on a bare id-3 canister through the exact framed
    // path the relay invokes. Surface bounds: any frame >~10B or hash >~5B fails.
    let bare = env.fed.pic.create_canister();
    env.fed.pic.add_cycles(bare, 50_000_000_000_000);
    env.fed.pic.install_canister(
        bare,
        text_wasm(),
        Encode!(&text_canister::TextCanisterInitArgs {
            controller: Some(env.fed.router),
            analyzer_id: Some(text_canister::ANALYZER_KOREAN),
            dict_relay_caller: None,
            // Plan 0343: the bare id-3 budget leg pins kinds=[Korean] explicitly
            // (dict_required(3, [Korean]) is the only true pair for id 3).
            kinds: Some(vec![
                gleaph_graph_kernel::provisioning::dictionary::DictKind::Korean,
            ]),
        })
        .expect("encode bare id-3 init"),
        None,
    );
    use gleaph_graph_kernel::provisioning::dictionary::{
        CompressedDictFinalize, CompressedDictUpload,
    };
    let raw_len = raw.len() as u64;
    let stable_before = env.fed.pic.get_stable_memory(bare).len();
    let mut max_frame_cycles = 0u128;
    let mut first_frame_cycles = 0u128;
    for (i, frame) in frames.iter().enumerate() {
        let cycles_before = env.fed.pic.cycle_balance(bare);
        let bytes = env
            .fed
            .pic
            .update_call(
                bare,
                env.fed.router,
                "admin_upload_dict_chunk",
                Encode!(
                    &frame.clone(),
                    &Some(CompressedDictUpload {
                        frame_digest: xxhash_rust::xxh3::xxh3_128(frame),
                        raw_len,
                    })
                )
                .expect("encode framed chunk"),
            )
            .unwrap_or_else(|e| panic!("admin_upload_dict_chunk: {e:?}"));
        let total: u64 = Decode!(&bytes, Result<u64, String>)
            .expect("decode upload reply")
            .expect("framed chunk ok");
        assert!(total > 0, "framed decode appends raw bytes");
        let call_cycles = cycles_before.saturating_sub(env.fed.pic.cycle_balance(bare));
        if i == 0 {
            first_frame_cycles = call_cycles;
        }
        max_frame_cycles = max_frame_cycles.max(call_cycles);
    }
    let raw_digest = xxhash_rust::xxh3::xxh3_128(&raw);
    let cycles_before_finalize = env.fed.pic.cycle_balance(bare);
    let bytes = env
        .fed
        .pic
        .update_call(
            bare,
            env.fed.router,
            "admin_finalize_dict_upload",
            Encode!(
                &raw_digest,
                &Some(CompressedDictFinalize {
                    raw_digest,
                    raw_len,
                })
            )
            .expect("encode framed finalize"),
        )
        .unwrap_or_else(|e| panic!("admin_finalize_dict_upload: {e:?}"));
    let finalized: text_canister::DictStatus =
        Decode!(&bytes, Result<text_canister::DictStatus, String>)
            .expect("decode finalize reply")
            .expect("framed finalize ok");
    let finalize_cycles = cycles_before_finalize.saturating_sub(env.fed.pic.cycle_balance(bare));
    println!(
        "plan-0341 budget gate (101.4 MB ko-dic): first-frame upload call cycles {first_frame_cycles}; max per-frame upload call cycles {max_frame_cycles}; finalize (one-pass region-16 hash + validation + pin) cycles {finalize_cycles}"
    );
    assert_eq!(finalized.state, text_canister::DictState::Finalized);
    assert!(
        max_frame_cycles < 10_000_000_000,
        "a ko-dic per-frame decode call took {max_frame_cycles} cycles — exceeds the ~10B surface bound"
    );
    assert!(
        finalize_cycles < 5_000_000_000,
        "the ko-dic one-pass finalize hash took {finalize_cycles} cycles — exceeds the ~5B surface bound"
    );
    // Stable-write volume (0342 discipline): ONLY the raw container lands in region 16.
    let stable_bytes = env.fed.pic.get_stable_memory(bare).len();
    println!(
        "plan-0341 stable-write volume: bare id-3 stable memory after framed ko-dic upload = {stable_bytes} B ({:.1} MB; raw {:.1} MB)",
        stable_bytes as f64 / (1024.0 * 1024.0),
        raw.len() as f64 / (1024.0 * 1024.0),
    );
    assert!(
        stable_bytes.saturating_sub(stable_before) < raw.len() + 8 * 1024 * 1024,
        "stable-memory delta exceeds raw + one bucket — compressed staging may have been resurrected"
    );

    // LEG 1 — GQL-surface admission with `ANALYZER korean`: the provisioned canister
    // pins analyzer 3, and the post-install relay streams the KOREAN catalog entry's
    // frames and finalizes — ZERO manual dictionary steps on this canister (no
    // admin_upload_dict_chunk call below touches it before the assertions).
    let statement = format!(
        "CREATE TEXT INDEX {KOREAN_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER korean WITH DICTIONARY korean"
    );
    let provision_cycles_before = env.fed.pic.cycle_balance(env.provision);
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "korean-ddl");
    let relay_cycles =
        provision_cycles_before.saturating_sub(env.fed.pic.cycle_balance(env.provision));
    println!("plan-0341 relay cost (provision-side cycle delta across korean DDL): {relay_cycles}");
    let info = {
        let bytes = env
            .fed
            .pic
            .query_call(
                env.fed.router,
                env.fed.admin,
                "get_text_index",
                Encode!(&GRAPH_NAME.to_string(), &KOREAN_INDEX_NAME.to_string()).expect("encode"),
            )
            .unwrap_or_else(|e| panic!("get_text_index on router: {e:?}"));
        Decode!(&bytes, Result<TextIndexInfo, RouterError>)
            .expect("decode get_text_index")
            .expect("definition exists")
    };
    assert_eq!(info.analyzer_id, 3, "the ANALYZER korean clause pins id 3");
    let canister = info.canister.expect("provisioned canister attached");
    env.fed.pic.add_cycles(canister, 50_000_000_000_000);
    let status = get_dict_status(&env, canister);
    assert_eq!(
        status.state,
        text_canister::DictState::Finalized,
        "the relay must finalize the ko-dic dictionary during provisioning"
    );
    assert_eq!(
        status.digest,
        Some(raw_digest),
        "relay pins the ko-dic raw digest"
    );
    assert_eq!(
        status.len,
        raw.len() as u64,
        "raw length matches the ko-dic container"
    );

    // LEG 2 — drive the migration lane to Ready, flush, then RECALL through GQL:
    // query 학교 recalls both 학교 docs (조사 stripped at index AND query time),
    // query 공부 recalls only the 공부했다 doc, zebra stays discriminating, and the
    // replay is deterministic.
    let args = migration_args(
        KOREAN_MIGRATION_ID,
        &format!(
            "CREATE TEXT INDEX {KOREAN_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER korean WITH DICTIONARY korean"
        ),
    );
    drive_to_ready_for(&env, &args, KOREAN_INDEX_NAME);
    assert_eq!(
        get_text_index_named(&env, KOREAN_INDEX_NAME).status,
        TextIndexStatusView::Ready
    );
    flush_until_done_for(&env, KOREAN_INDEX_NAME);
    let hakgyo = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("학교", 10));
    assert_eq!(hakgyo.row_count, 2, "both 학교 docs recall under 학교");
    let hakgyo_rows = scored_rows(&hakgyo);
    assert!(
        hakgyo_rows[0].1 >= hakgyo_rows[1].1,
        "scores arrive descending"
    );
    assert!(hakgyo_rows.iter().all(|(_, score)| *score > 0.0));
    let replay = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("학교", 10));
    assert_eq!(
        scored_rows(&replay),
        hakgyo_rows,
        "recall must be deterministic"
    );
    let gongbu = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("공부", 10));
    assert_eq!(
        gongbu.row_count, 1,
        "only the 공부했다 doc recalls under 공부"
    );
    let zebra = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("zebra", 10));
    assert_eq!(zebra.row_count, 1, "zebra still matches its own doc");

    // LEG 3 — re-provision idempotence: the SAME registration replays without
    // re-appending the dictionary (relay short-circuit: Finalized + matching digest).
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "korean-ddl-replay");
    let replayed = get_text_index_named(&env, KOREAN_INDEX_NAME);
    assert_eq!(replayed.canister, Some(canister), "same canister id");
    assert_eq!(replayed.status, TextIndexStatusView::Ready);
    let status_after = get_dict_status(&env, canister);
    assert_eq!(status_after.state, text_canister::DictState::Finalized);
    assert_eq!(status_after.digest, Some(raw_digest));
    let catalog_bytes = env
        .fed
        .pic
        .query_call(
            env.provision,
            env.fed.admin,
            "admin_get_dict_catalog_status",
            Encode!(&gleaph_provision::types::DictCatalogKey {
                kind: KOREAN_CATALOG_KIND.to_owned(),
                version: KOREAN_CATALOG_VERSION.to_owned(),
            })
            .expect("encode"),
        )
        .unwrap_or_else(|e| panic!("admin_get_dict_catalog_status: {e:?}"));
    let catalog: Option<gleaph_provision::types::DictCatalogStatus> = Decode!(
        &catalog_bytes,
        Option<gleaph_provision::types::DictCatalogStatus>
    )
    .expect("decode catalog status");
    assert_eq!(
        catalog.expect("seeded korean entry").state,
        gleaph_provision::types::DictCatalogState::Finalized
    );

    // LEG 4 — figures: relay cycles (LEG 1), on-canister stable footprint, and the
    // stable-memory $/month at the design-doc rate ($0.058/month per 52,931,159 B).
    let memory_size = env
        .fed
        .pic
        .canister_status(canister, Some(env.fed.admin))
        .expect("relayed canister status")
        .memory_size;
    let monthly_usd = raw.len() as f64 * 0.058 / 52_931_159.0;
    println!(
        "plan-0341 figures: relay provision-side cycles {relay_cycles}; relayed analyzer-3 canister memory_size {memory_size} B; ko-dic stable $/month ~${monthly_usd:.3} (native resident set ~64.3 MB, feature region lazy)"
    );
}

#[test]
fn mecab_counter_leg_explicit_bigram_pin_does_not_recall_lemma() {
    let wired = bootstrap_with_active_release();
    let provision = wired.provision;
    let env = Env {
        provision,
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
    // Framed container FIRST (see mecab leg): zstd-19 framing outlasts the server idle TTL.
    let raw = fetch_mecab_container();
    let frames = gleaph_pocket_ic_tests::framed_container(&raw);
    let wired = bootstrap_with_active_release();
    let provision = wired.provision;
    let env = Env {
        provision,
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

    // LEG 0 — catalog seeding (plan 0335 todo 4): the relay (todo 3) then auto-finalizes
    // the id-0 canister's dictionary during provisioning.
    seed_dictionary_catalog(&env, &raw, &frames);

    // LEG 1 — plan 0343 single-kind composite: the GQL DDL surface with
    // `ANALYZER multilingual WITH DICTIONARY japanese` resolves to (0, [Japanese])
    // (id 0 takes any subset of {Japanese, Korean}; the multi-kind composite awaits
    // the undecided multi-container layout). The bare-endpoint dict-less default is
    // pinned by the text_index_provisioning E2E instead.
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER multilingual WITH DICTIONARY japanese"
    );
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "composite-ddl");
    let info = get_text_index_named(&env, INDEX_NAME);
    assert_eq!(
        info.analyzer_id, 0,
        "the ANALYZER multilingual clause pins id 0"
    );
    let canister = info.canister.expect("provisioned canister attached");
    env.fed.pic.add_cycles(canister, 50_000_000_000_000);
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);

    // LEG 2 — the post-install RELAY covers the selected kind: the provisioned
    // canister's dictionary is already relay-Finalized (ipadic), and the backfill
    // registration proceeds with no hold. Korean recall in LEG 5/6 runs the legacy
    // strip path (as in the 0332 era); ko-dic lemma quality is pinned by the
    // dedicated korean leg instead.
    let status = get_dict_status(&env, canister);
    assert_eq!(status.state, text_canister::DictState::Finalized);
    let dict = fetch_mecab_container();
    let digest = xxhash_rust::xxh3::xxh3_128(&dict);
    assert_eq!(status.digest, Some(digest));
    assert_eq!(status.len, dict.len() as u64);

    // LEG 4 — the migration statement carries the SAME (analyzer, kinds) selection
    // (the kinds-match gate compares statement kinds against the pinned row).
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER multilingual WITH DICTIONARY japanese"
    );
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

// -- Plan 0339: Japanese text folding (IVS strip + kana counter-variant unification) --------

const FOLDING_INDEX_NAME: &str = "text_score_folding_idx";
const FOLDING_MIGRATION_ID: &str = "000106_text_score_folding";

/// The IVS fixture: 葛󠄀 = 葛 (U+845B, UTF-8 E8 91 9B) + VARIATION SELECTOR-17
/// (U+E0100, UTF-8 F3 A0 84 80). The bare-base query 葛区 must recall this doc.
const IVS_DOC: &str = "葛\u{E0100}区";

/// One leg proving the plan 0339 folding contract end-to-end through provisioning +
/// backfill + GQL on the ANALYZER japanese (id 2, plan 0343 rename mecab -> japanese) pipeline (the dictionary is supplied
/// automatically by the plan 0335 catalog relay — ZERO manual dict steps):
///   (a) an IVS-bearing doc is recalled by the bare-base query (and by its own
///       IVS-literal query — the strip is side-symmetric);
///   (b) a 3ヶ月 doc is recalled by the ３ケ月 query (fullwidth NFKC + kana fold
///       chained) and vice versa;
///   (c) the HONEST negative: 3か月 does NOT fold-match — the か counter is not
///       unified to ケ, so the か月 doc is absent from the ケ月 candidate set.
#[test]
fn japanese_text_folding_recalls_ivs_and_kana_counter_variants() {
    // Framed container FIRST (see mecab leg): zstd-19 framing outlasts the server idle TTL.
    let raw = fetch_mecab_container();
    let frames = gleaph_pocket_ic_tests::framed_container(&raw);
    let wired = bootstrap_with_active_release();
    let provision = wired.provision;
    let env = Env {
        provision,
        fed: finish_provision_wired_single_shard_federation(wired),
    };
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    // Corpus: an IVS-bearing doc, a ヶ-counter doc, a か-counter doc (v1 boundary),
    // and an unrelated doc. Insertion order fixes vertex ids ascending.
    seed_text_vertex(&env, IVS_DOC);
    seed_text_vertex(&env, "3ヶ月");
    seed_text_vertex(&env, "3か月");
    seed_text_vertex(&env, "unrelated zebra");

    // Catalog seeding (plan 0335 todo 4): the relay auto-finalizes the dictionary on
    // the provisioned mecab canister — ZERO manual dict steps.
    seed_dictionary_catalog(&env, &raw, &frames);

    // Declare via the GQL DDL surface with ANALYZER japanese (id 2) + the strict
    // WITH DICTIONARY japanese selection (plan 0343: id 2 admits exactly [Japanese]).
    let statement = format!(
        "CREATE TEXT INDEX {FOLDING_INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER japanese WITH DICTIONARY japanese"
    );
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "folding-ddl");
    let info = get_text_index_named(&env, FOLDING_INDEX_NAME);
    assert_eq!(
        info.analyzer_id, 2,
        "the ANALYZER japanese clause pins id 2"
    );
    let canister = info.canister.expect("provisioned canister attached");
    env.fed.pic.add_cycles(canister, 50_000_000_000_000);
    env.fed
        .pic
        .add_cycles(env.fed.graph_source, 20_000_000_000_000);

    let args = migration_args(FOLDING_MIGRATION_ID, &statement);
    drive_to_ready_for(&env, &args, FOLDING_INDEX_NAME);
    assert_eq!(
        get_text_index_named(&env, FOLDING_INDEX_NAME).status,
        TextIndexStatusView::Ready
    );
    flush_until_done_for(&env, FOLDING_INDEX_NAME);

    // (a) IVS strip: the IVS-bearing doc is recalled by the bare-base query 葛区.
    let base = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("葛区", 10));
    assert_eq!(
        base.row_count, 1,
        "IVS doc recalls under the bare base form"
    );
    // ...and by the doc's own IVS-literal query (the strip is side-symmetric).
    let literal = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params(IVS_DOC, 10));
    assert_eq!(
        literal.row_count, 1,
        "IVS-literal query recalls the same doc"
    );
    assert_eq!(
        scored_rows(&literal)[0].0,
        scored_rows(&base)[0].0,
        "bare-base and IVS-literal queries hit the same doc"
    );

    // (b) kana counter fold: the 3ヶ月 doc is recalled by the ３ケ月 query (fullwidth
    // NFKC ３→3 + the ヶ→ケ emitted-unit fold chained).
    let ke = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("ケ月", 10));
    assert_eq!(
        ke.row_count, 1,
        "the ヶ doc recalls under the folded ケ月 unit"
    );
    let full = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("３ケ月", 10));
    assert_eq!(
        full.row_count, 2,
        "３ケ月 recalls the ヶ doc (2 units) and the か doc (shared 3)"
    );
    let full_rows = scored_rows(&full);
    assert_eq!(
        full_rows[0].0,
        scored_rows(&ke)[0].0,
        "the ヶ doc ranks first (matches both query units)"
    );
    assert!(
        full_rows[0].1 > full_rows[1].1,
        "strict score order: ヶ doc above the か doc"
    );

    // (c) the HONEST negative: 3か月 does NOT fold-match. The か counter is not
    // unified to ケ, so the か月 doc is absent from the ケ月 candidate set (ke above
    // returned exactly one row — the ヶ doc) and か月/ケ月 are distinct units.
    let ka = gql_query_with_params_as_admin(&env.fed, QUERY, scored_query_params("か月", 10));
    assert_eq!(
        ka.row_count, 1,
        "the か doc recalls under its own か月 unit"
    );
    assert_ne!(
        scored_rows(&ka)[0].0,
        scored_rows(&ke)[0].0,
        "か月 and ケ月 are distinct units (v1 boundary)"
    );
}

// -- Plan 0344: candidate-scoped non-leading text_score top-k ------------------------------

const CANDIDATE_USER_LABEL: &str = "User";
const CANDIDATE_PROJECT_LABEL: &str = "Project";
const CANDIDATE_UID_PROPERTY: &str = "uid";
const CANDIDATE_RANK_PROPERTY: &str = "rank";
const CANDIDATE_MEMBER_EDGE: &str = "MEMBER_OF";
const CANDIDATE_DOC_EDGE: &str = "HAS_DOCUMENT";
const CANDIDATE_USER_ID: i64 = 7;
const CANDIDATE_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN d.rank AS rank, ELEMENT_ID(d) AS d_id, text_score(d.bio,$q) AS score \
     ORDER BY score DESC LIMIT 20";

fn candidate_query_params(user_id: i64, query: &str) -> Vec<u8> {
    encode_gql_params_blob(vec![
        ("user_id".to_string(), Value::Int64(user_id)),
        ("q".to_string(), Value::Text(query.to_string())),
    ])
    .expect("encode params")
}

fn candidate_rows(result: &GqlQueryResult) -> Vec<(i64, Vec<u8>, f64)> {
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
            let rank = match columns.get("rank").expect("rank column present") {
                GqlWireValue::Int64(rank) => *rank,
                other => panic!("rank must be Int64, got {other:?}"),
            };
            let id = match columns.get("d_id").expect("d_id column present") {
                GqlWireValue::Bytes(bytes) => {
                    assert_eq!(bytes.len(), 8, "ELEMENT_ID is the 8-byte vertex encoding");
                    bytes.clone()
                }
                other => panic!("ELEMENT_ID must decode as Bytes, got {other:?}"),
            };
            let score = match columns.get("score").expect("score column present") {
                GqlWireValue::Float64(score) => *score,
                other => panic!("score must be Float64, got {other:?}"),
            };
            (rank, id, score)
        })
        .collect()
}

/// Shared 21-reachable + 101-unreachable candidate fixture: two users sharing
/// `uid = 7` fan every reachable document out to two prefix rows, while the 101
/// unreachable documents carry strictly heavier term frequencies so no global
/// top-100 window can hold a reachable row.
struct CandidateFixture {
    env: Env,
    text_canister: Principal,
}

fn seed_candidate_fixture() -> CandidateFixture {
    let wired = bootstrap_with_active_release();
    let provision = wired.provision;
    let env = Env {
        provision,
        fed: finish_provision_wired_single_shard_federation(wired),
    };
    let graph = env.fed.graph_source;

    // The provision-wired bootstrap handshakes the index canister index-side only;
    // label-anchor seed routing pages through the Router-side attach, so attach it
    // explicitly (the candidate prefix starts from a label-anchored `u:User` scan).
    gleaph_pocket_ic_tests::attach_index_canister_to_shard(
        &env.fed.pic,
        env.fed.admin,
        env.fed.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        gleaph_pocket_ic_tests::SOURCE_SHARD,
        env.fed.index,
    );

    // Labels / properties / edges for the two-hop candidate fixture.
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL);
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, CANDIDATE_USER_LABEL);
    gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, CANDIDATE_PROJECT_LABEL);
    gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY);
    let uid_property = gleaph_pocket_ic_tests::ensure_property(&env.fed, CANDIDATE_UID_PROPERTY);
    let rank_property = gleaph_pocket_ic_tests::ensure_property(&env.fed, CANDIDATE_RANK_PROPERTY);
    let member_edge = gleaph_pocket_ic_tests::ensure_edge_label(&env.fed, CANDIDATE_MEMBER_EDGE);
    let doc_edge = gleaph_pocket_ic_tests::ensure_edge_label(&env.fed, CANDIDATE_DOC_EDGE);
    let user_label =
        gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, CANDIDATE_USER_LABEL).raw();
    let project_label =
        gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, CANDIDATE_PROJECT_LABEL).raw();
    let document_label = gleaph_pocket_ic_tests::ensure_vertex_label(&env.fed, LABEL).raw();

    // Two users share the anchor uid: every reachable document fans out to TWO prefix
    // rows, so LIMIT counts rows (not distinct candidates).
    let mut users = Vec::new();
    for _ in 0..2 {
        users.push(
            gleaph_pocket_ic_tests::e2e_insert_vertex_with_label_and_property(
                &env.fed,
                graph,
                user_label,
                uid_property.raw(),
                CANDIDATE_USER_ID,
            )
            .local_vertex_id,
        );
    }
    let project =
        gleaph_pocket_ic_tests::e2e_insert_vertex_with_label(&env.fed, graph, project_label)
            .local_vertex_id;
    for user in &users {
        gleaph_pocket_ic_tests::e2e_insert_edge_with_label(
            &env.fed,
            graph,
            *user,
            project,
            member_edge.raw(),
        );
    }

    // 21 reachable documents with discriminating frequencies. Docs 0 and 1 share an
    // identical body (engineered score tie); rank carries the insertion index.
    let mut reachable = Vec::new();
    for i in 0..21 {
        let body = if i < 2 {
            "wombat wombat tiebreak".to_string()
        } else {
            format!("wombat DOC{i} {}", "wombat ".repeat(i % 6 + 1))
        };
        let doc = gleaph_pocket_ic_tests::e2e_insert_vertex_with_label_and_text_property(
            &env.fed,
            graph,
            document_label,
            gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY).raw(),
            body,
        )
        .local_vertex_id;
        gleaph_pocket_ic_tests::e2e_set_vertex_property(
            &env.fed,
            graph,
            doc,
            rank_property.raw(),
            i as i64,
        );
        gleaph_pocket_ic_tests::e2e_insert_edge_with_label(
            &env.fed,
            graph,
            project,
            doc,
            doc_edge.raw(),
        );
        reachable.push(doc);
    }

    // 101 unreachable documents with strictly heavier frequencies: the global
    // `search(100)` window holds ONLY unreachable docs, so any global-top-k-then-filter
    // execution could never return a reachable row.
    for i in 0..101 {
        gleaph_pocket_ic_tests::e2e_insert_vertex_with_label_and_text_property(
            &env.fed,
            graph,
            document_label,
            gleaph_pocket_ic_tests::ensure_property(&env.fed, PROPERTY).raw(),
            format!("wombat UNREACH{i} {}", "wombat ".repeat(20)),
        );
    }

    // Declare + provision, drive Ready, flush.
    let info = create_text_index_definition(&env);
    let text_canister = info.canister.expect("provisioned canister attached");
    env.fed.pic.add_cycles(text_canister, 20_000_000_000_000);
    env.fed.pic.add_cycles(graph, 20_000_000_000_000);
    let statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    let args = migration_args(MIGRATION_ID, &statement);
    drive_to_ready(&env, &args);
    assert_eq!(get_text_index(&env).status, TextIndexStatusView::Ready);
    flush_until_done(&env);
    CandidateFixture { env, text_canister }
}

#[test]
fn non_leading_text_candidate_topk_lifecycle() {
    let fixture = seed_candidate_fixture();
    let env = fixture.env;
    let text_canister = fixture.text_canister;

    // POSITIVE: top-20 rows over graph-qualified candidates only.
    let result = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_QUERY,
        candidate_query_params(CANDIDATE_USER_ID, "wombat"),
    );
    assert_eq!(result.row_count, 20, "LIMIT counts ranked rows");
    assert_eq!(
        result.truncated,
        Some(false),
        "exact top-k is never truncated"
    );
    let rows = candidate_rows(&result);
    assert_eq!(rows.len(), 20);
    for window in rows.windows(2) {
        assert!(
            window[0].2 >= window[1].2,
            "scores arrive descending: {} before {}",
            window[0].2,
            window[1].2
        );
        assert!(window[0].2 > 0.0, "matching rows carry positive scores");
    }
    // Every returned rank belongs to the reachable set: unreachable heavy docs never
    // leak through a global window.
    assert!(
        rows.iter().all(|(rank, _, _)| (0..21).contains(rank)),
        "only reachable documents rank"
    );
    // Row multiplicity: the top-ranked document fans out through both anchor users,
    // so its rank heads the frame twice in a row.
    assert_eq!(
        rows[0].0, rows[1].0,
        "duplicate prefix paths share the frame head"
    );
    assert_eq!(rows[0].1, rows[1].1, "duplicated rows share identity");
    assert_eq!(
        rows[0].2, rows[1].2,
        "duplicated rows carry identical score"
    );

    // Determinism: an identical re-run returns the identical frame.
    let replay = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_QUERY,
        candidate_query_params(CANDIDATE_USER_ID, "wombat"),
    );
    assert_eq!(
        candidate_rows(&replay),
        rows,
        "ranking must be deterministic"
    );

    // GUARD MATRIX (real wasm): the candidate endpoint enforces the stored
    // controller. A third principal and the anonymous caller both reject; the
    // Router path above proves the controller itself succeeds.
    for (sender, what) in [
        (Principal::from_slice(&[0x3D; 29]), "third principal"),
        (Principal::anonymous(), "anonymous caller"),
    ] {
        let err = env
            .fed
            .pic
            .query_call(
                text_canister,
                sender,
                "search_candidates",
                Encode!(&("wombat".to_string(), &vec![1u64])).expect("encode candidates call"),
            )
            .expect_err(format!("{what} must not reach search_candidates").leak());
        assert!(
            err.reject_message
                .contains("is not the text index controller"),
            "unexpected {what} guard reject: {}",
            err.reject_message
        );
    }

    // POLICY-DENIED HIGH SCORER (plan 0344 deferred gate): rank 5 carries a tf-7
    // body, so it heads the authorized frame — yet a conditional MATCH grant covers
    // every Document rank EXCEPT 5 (two AND-free range rows compose by union), while
    // the rest of the traversal surface stays granted. The denied candidate must
    // never enter the TEXT request: the remaining frame stays full at LIMIT 20 with
    // descending scores, headed by the tf-7 peers (ranks 11, 17).
    const DENIED_RANK: i64 = 5;
    let policy_caller = Principal::from_slice(&[0x41; 29]);
    for (key, statement) in [
        (
            "candidate-grant-match-user",
            format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES User TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-match-project",
            format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Project TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-traverse-member",
            format!(
                "GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES MEMBER_OF TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-traverse-doc",
            format!(
                "GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES HAS_DOCUMENT TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-read-user",
            format!(
                "GRANT READ ON GRAPH {GRAPH_NAME} NODES User {{ uid }} TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-read-doc",
            format!(
                "GRANT READ ON GRAPH {GRAPH_NAME} NODES Document {{ rank, bio }} TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-match-doc-below",
            format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document FOR (d:Document) \
                 WHERE d.rank < {DENIED_RANK} TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
        (
            "candidate-grant-match-doc-above",
            format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document FOR (d:Document) \
                 WHERE d.rank > {DENIED_RANK} TO PRINCIPAL '{}'",
                policy_caller.to_text()
            ),
        ),
    ] {
        gql_mutate_as_admin(&env.fed, &statement, key);
    }

    // Control: the implicit-root admin frame still ranks the denied doc at the head,
    // proving the exclusion below is policy — not scoring.
    let admin_frame = candidate_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_QUERY,
        candidate_query_params(CANDIDATE_USER_ID, "wombat"),
    ));
    assert!(
        admin_frame[..6]
            .iter()
            .all(|(rank, _, _)| [5, 11, 17].contains(rank)),
        "admin frame head is the tf-7 group: {:?}",
        &admin_frame[..6]
    );
    assert!(
        admin_frame[..6]
            .iter()
            .any(|(rank, _, _)| *rank == DENIED_RANK),
        "denied doc scores into the authorized head"
    );

    let denied = candidate_rows(&gql_query_with_params_on_router(
        &env.fed.pic,
        policy_caller,
        env.fed.router,
        CANDIDATE_QUERY,
        candidate_query_params(CANDIDATE_USER_ID, "wombat"),
    ));
    assert_eq!(denied.len(), 20, "remaining frame stays full at LIMIT");
    assert!(
        denied.iter().all(|(rank, _, _)| *rank != DENIED_RANK),
        "policy-denied high scorer never ranks"
    );
    assert!(
        denied.iter().all(|(rank, _, _)| (0..21).contains(rank)),
        "only reachable, authorized documents rank"
    );
    for window in denied.windows(2) {
        assert!(
            window[0].2 >= window[1].2,
            "authorized scores stay descending: {} before {}",
            window[0].2,
            window[1].2
        );
    }
    // The tf-7 peers inherit the head (each fanned out through both anchor users).
    assert!(
        denied[..4]
            .iter()
            .all(|(rank, _, _)| [11, 17].contains(rank)),
        "tf-7 peers head the denied frame: {:?}",
        &denied[..4]
    );
    assert_eq!(
        denied[0].0, denied[1].0,
        "head multiplicity survives denial"
    );
    let denied_replay = candidate_rows(&gql_query_with_params_on_router(
        &env.fed.pic,
        policy_caller,
        env.fed.router,
        CANDIDATE_QUERY,
        candidate_query_params(CANDIDATE_USER_ID, "wombat"),
    ));
    assert_eq!(denied_replay, denied, "denied frame is deterministic");
}

// ──── Candidate-scoped non-leading threshold ────

const CANDIDATE_THRESHOLD_SCORED_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > $t \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score";
const CANDIDATE_THRESHOLD_SCORED_GE_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) >= $t \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score";

fn threshold_query_params(user_id: i64, query: &str, bound: f64) -> Vec<u8> {
    encode_gql_params_blob(vec![
        ("user_id".to_string(), Value::Int64(user_id)),
        ("q".to_string(), Value::Text(query.to_string())),
        ("t".to_string(), Value::Float64(bound)),
    ])
    .expect("encode params")
}

fn threshold_scored_rows(result: &GqlQueryResult) -> Vec<(i64, f64)> {
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
            let rank = match columns.get("rank").expect("rank column present") {
                GqlWireValue::Int64(rank) => *rank,
                other => panic!("rank must be Int64, got {other:?}"),
            };
            let score = match columns.get("score").expect("score column present") {
                GqlWireValue::Float64(score) => *score,
                other => panic!("score must be Float64, got {other:?}"),
            };
            (rank, score)
        })
        .collect()
}

#[test]
fn non_leading_text_candidate_threshold_lifecycle() {
    let fixture = seed_candidate_fixture();
    let env = fixture.env;

    // ALL-PASS calibration: a bound below every reachable score returns the full
    // 42-row frame (21 documents × 2 anchor users). A global-window-then-filter
    // execution could never return a reachable row — the global top-100 window
    // holds only unreachable heavy docs — so a full reachable frame proves
    // candidate-first scoring.
    let all = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", 0.5),
    );
    assert_eq!(all.row_count, 42, "every candidate passes the low bound");
    assert_eq!(
        all.truncated,
        Some(false),
        "exact filtering never truncates"
    );
    let rows = threshold_scored_rows(&all);
    assert_eq!(rows.len(), 42);
    assert!(
        rows.iter().all(|(rank, _)| (0..21).contains(rank)),
        "only reachable documents pass"
    );
    for window in rows.windows(2) {
        assert!(
            window[0].1 >= window[1].1,
            "scores arrive descending: {} before {}",
            window[0].1,
            window[1].1
        );
    }
    // Row multiplicity: every rank fans out through both anchor users.
    let mut sorted: Vec<i64> = rows.iter().map(|(rank, _)| *rank).collect();
    sorted.sort_unstable();
    let mut expected = Vec::new();
    for rank in 0..21 {
        expected.push(rank);
        expected.push(rank);
    }
    assert_eq!(sorted, expected, "each reachable rank passes twice");

    // Boundary calibration from the observed frame (engine scores are exact
    // integer-valued floats): split at an interior distinct score.
    let mut distinct: Vec<f64> = rows.iter().map(|(_, score)| *score).collect();
    distinct.sort_by(|a, b| a.total_cmp(b));
    distinct.dedup();
    assert!(distinct.len() >= 2, "frame must span scores");
    let bound = distinct[(distinct.len() - 1) / 2];
    let above: Vec<(i64, f64)> = rows
        .iter()
        .filter(|(_, score)| *score > bound)
        .copied()
        .collect();
    let at: Vec<(i64, f64)> = rows
        .iter()
        .filter(|(_, score)| *score == bound)
        .copied()
        .collect();
    assert!(!above.is_empty(), "bound must not be the frame maximum");
    assert!(!at.is_empty(), "bound must hit the frame");

    // Strict `>` drops the boundary rows; inclusive `>=` keeps exactly those.
    let gt = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    ));
    assert!(
        gt.iter().all(|(_, score)| *score > bound),
        "strict bound drops equality"
    );
    assert_eq!(gt.len(), above.len(), "strict frame matches calibration");
    let ge = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_GE_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    ));
    assert_eq!(
        ge.len(),
        above.len() + at.len(),
        "inclusive frame adds exactly the boundary rows"
    );
    assert!(
        ge.iter().all(|(_, score)| *score >= bound),
        "inclusive bound keeps equality"
    );

    // Determinism on the inclusive frame.
    let replay = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_GE_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    ));
    assert_eq!(replay, ge, "threshold frame is deterministic");

    // LITERAL bound above every reachable score, threshold-only RETURN (no score
    // column): the exact empty set, not an error. Scores are integer-valued, so
    // `max + 0.5` sits strictly between the reachable maximum and the
    // unreachable minimum (tf 21 vs tf ≤ 7).
    let max = *distinct.last().expect("nonempty");
    let high_query = format!(
        "MATCH (u:User {{uid:$user_id}})-[:MEMBER_OF]->(p:Project) \
         MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > {} \
         RETURN d.rank AS rank",
        max + 0.5
    );
    let high = gql_query_with_params_as_admin(
        &env.fed,
        &high_query,
        candidate_query_params(CANDIDATE_USER_ID, "wombat"),
    );
    assert_eq!(high.row_count, 0, "no candidate clears the high bound");
    assert_eq!(
        high.truncated,
        Some(false),
        "empty filtering never truncates"
    );
}

const CANDIDATE_COMPOUND_SCORED_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > $t \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 5";
const CANDIDATE_COMPOUND_NOSCORE_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > $t \
     RETURN d.rank AS rank ORDER BY text_score(d.bio,$q) DESC LIMIT 5";

const CANDIDATE_TOPK_OFFSET_SCORED_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 10 OFFSET 5";
const CANDIDATE_TOPK_OFFSET_ZERO_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 10 OFFSET 0";
const CANDIDATE_TOPK_OFFSET_PAST_END_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 10 OFFSET 100";
const CANDIDATE_COMPOUND_OFFSET_SCORED_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > $t \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 5 OFFSET 3";

#[test]
fn non_leading_text_candidate_offset_lifecycle() {
    let fixture = seed_candidate_fixture();
    let env = fixture.env;

    // Calibration frame: the threshold-only barrier returns every candidate in
    // Router ranking order, so an offset query must equal the sliced frame.
    let frame = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", 0.5),
    ));
    assert_eq!(frame.len(), 42, "full candidate frame");
    let params = encode_gql_params_blob(vec![
        ("user_id".to_string(), Value::Int64(CANDIDATE_USER_ID)),
        ("q".to_string(), Value::Text("wombat".to_string())),
    ])
    .expect("encode params");

    // LIMIT 10 OFFSET 5: exactly the frame slice — an offset-ignoring
    // misimplementation would return the first 10 rows, and a limit-before-cut
    // misimplementation only 5.
    let offset = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_TOPK_OFFSET_SCORED_QUERY,
        params.clone(),
    );
    assert_eq!(offset.row_count, 10, "skip-then-take keeps 10 rows");
    assert_eq!(
        offset.truncated,
        Some(false),
        "exact offset never truncates"
    );
    let expected: Vec<(i64, f64)> = frame.iter().skip(5).take(10).copied().collect();
    assert_eq!(
        threshold_scored_rows(&offset),
        expected,
        "offset query equals the sliced frame"
    );

    // OFFSET 0 is the offset-free head.
    let zero = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_TOPK_OFFSET_ZERO_QUERY,
        params.clone(),
    ));
    assert_eq!(
        zero,
        frame.iter().take(10).copied().collect::<Vec<_>>(),
        "OFFSET 0 keeps the head"
    );

    // OFFSET past the end: the exact empty set, not an error.
    let past = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_TOPK_OFFSET_PAST_END_QUERY,
        params.clone(),
    );
    assert_eq!(past.row_count, 0, "no rows survive the past-end skip");
    assert_eq!(past.truncated, Some(false), "empty skip never truncates");

    // Compound with offset: threshold first, then skip-then-take on the filtered
    // frame. The interior bound from the threshold lifecycle leaves 20+ rows
    // above it, so rows 3..8 exist.
    let mut distinct: Vec<f64> = frame.iter().map(|(_, score)| *score).collect();
    distinct.sort_by(|a, b| a.total_cmp(b));
    distinct.dedup();
    let bound = distinct[(distinct.len() - 1) / 2];
    let filtered: Vec<(i64, f64)> = frame
        .iter()
        .filter(|(_, score)| *score > bound)
        .copied()
        .collect();
    assert!(filtered.len() >= 8, "calibration must clear skip + limit");
    let compound = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_COMPOUND_OFFSET_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    );
    assert_eq!(compound.row_count, 5);
    assert_eq!(
        threshold_scored_rows(&compound),
        filtered.iter().skip(3).take(5).copied().collect::<Vec<_>>(),
        "compound offset slices the filtered frame"
    );

    // Determinism on the offset query.
    let replay = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_TOPK_OFFSET_SCORED_QUERY,
        params,
    ));
    assert_eq!(replay, expected, "offset frame is deterministic");
}

const CANDIDATE_DISTINCT_SCORED_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN DISTINCT d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 42";
const CANDIDATE_DISTINCT_SCORE_ONLY_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN DISTINCT text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 42";
const CANDIDATE_DISTINCT_OFFSET_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN DISTINCT d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 10 OFFSET 5";
const CANDIDATE_DISTINCT_THRESHOLD_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > $t \
     RETURN DISTINCT d.rank AS rank";
const CANDIDATE_DISTINCT_COMPOUND_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.bio,$q) > $t \
     RETURN DISTINCT d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 42";

fn distinct_score_only_rows(result: &GqlQueryResult) -> Vec<f64> {
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
            match columns.get("score").expect("score column present") {
                GqlWireValue::Float64(score) => *score,
                other => panic!("score must be Float64, got {other:?}"),
            }
        })
        .collect()
}

#[test]
fn non_leading_text_candidate_distinct_lifecycle() {
    let fixture = seed_candidate_fixture();
    let env = fixture.env;

    // Calibration frame: every candidate in Router ranking order. The two-user
    // fan-out yields each document twice as byte-identical projected rows.
    let frame = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", 0.5),
    ));
    assert_eq!(frame.len(), 42, "full candidate frame");
    let params = encode_gql_params_blob(vec![
        ("user_id".to_string(), Value::Int64(CANDIDATE_USER_ID)),
        ("q".to_string(), Value::Text("wombat".to_string())),
    ])
    .expect("encode params");

    // kill-1 (count) + kill-2 (order): DISTINCT collapses the fan-out pairs
    // and keeps rank order. A dedup-skipping misimplementation returns 42
    // rows; a re-sorting one breaks the exact frame equality.
    let mut deduped: Vec<(i64, f64)> = Vec::with_capacity(frame.len());
    for row in &frame {
        if !deduped.contains(row) {
            deduped.push(*row);
        }
    }
    assert_eq!(deduped.len(), 21, "each document fanned out exactly twice");
    let distinct =
        gql_query_with_params_as_admin(&env.fed, CANDIDATE_DISTINCT_SCORED_QUERY, params.clone());
    assert_eq!(distinct.row_count, 21, "DISTINCT collapses fan-out pairs");
    assert_eq!(
        distinct.truncated,
        Some(false),
        "exact dedup never truncates"
    );
    assert_eq!(
        threshold_scored_rows(&distinct),
        deduped,
        "DISTINCT keeps first occurrences in rank order"
    );

    // kill-3 (key): the score-only projection dedups on the whole row. Docs 0
    // and 1 tie on score, so at least one pair collapses; a document-identity
    // dedup would wrongly keep all 42 rows.
    let mut distinct_scores: Vec<f64> = Vec::with_capacity(frame.len());
    for (_, score) in &frame {
        if !distinct_scores.contains(score) {
            distinct_scores.push(*score);
        }
    }
    assert!(
        distinct_scores.len() < frame.len(),
        "the engineered tie guarantees a collapse"
    );
    let score_only = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DISTINCT_SCORE_ONLY_QUERY,
        params.clone(),
    );
    assert_eq!(
        score_only.row_count as usize,
        distinct_scores.len(),
        "score-only DISTINCT collapses tied scores"
    );
    assert_eq!(
        score_only.truncated,
        Some(false),
        "exact dedup never truncates"
    );
    assert_eq!(
        distinct_score_only_rows(&score_only),
        distinct_scores,
        "score-only DISTINCT keeps rank order"
    );

    // kill-4 (DISTINCT + OFFSET): dedup runs before skip/take, so the result
    // is the deduped-frame slice — never a pre-dedup window cut.
    let offset =
        gql_query_with_params_as_admin(&env.fed, CANDIDATE_DISTINCT_OFFSET_QUERY, params.clone());
    assert_eq!(offset.row_count, 10);
    assert_eq!(offset.truncated, Some(false));
    assert_eq!(
        threshold_scored_rows(&offset),
        deduped.iter().skip(5).take(10).copied().collect::<Vec<_>>(),
        "DISTINCT offset slices the deduped frame"
    );

    // Threshold + DISTINCT opens together: the 0.5 calibration passes every
    // candidate, so 21 distinct ranks survive in frame order.
    let threshold = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DISTINCT_THRESHOLD_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", 0.5),
    );
    assert_eq!(threshold.row_count, 21);
    assert_eq!(threshold.truncated, Some(false));
    assert_eq!(
        compound_rank_rows(&threshold),
        deduped.iter().map(|(rank, _)| *rank).collect::<Vec<_>>(),
        "threshold DISTINCT keeps frame order"
    );

    // Compound + DISTINCT: retain-threshold first, then the same mode-agnostic
    // dedup → take. The interior bound splits the frame, so a compound path
    // that ignores the distinct flag returns the uncollapsed filtered count.
    let mut ordered_bounds: Vec<f64> = frame.iter().map(|(_, score)| *score).collect();
    ordered_bounds.sort_by(|a, b| a.total_cmp(b));
    ordered_bounds.dedup();
    let bound = ordered_bounds[(ordered_bounds.len() - 1) / 2];
    let mut filtered_deduped: Vec<(i64, f64)> = Vec::new();
    for row in frame.iter().filter(|(_, score)| *score > bound) {
        if !filtered_deduped.contains(row) {
            filtered_deduped.push(*row);
        }
    }
    assert!(
        filtered_deduped.len() < frame.iter().filter(|(_, score)| *score > bound).count(),
        "the interior bound must leave collapsible duplicates"
    );
    let compound = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DISTINCT_COMPOUND_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    );
    assert_eq!(
        compound.row_count as usize,
        filtered_deduped.len(),
        "compound DISTINCT collapses the filtered frame"
    );
    assert_eq!(
        compound.truncated,
        Some(false),
        "exact compound dedup never truncates"
    );
    assert_eq!(
        threshold_scored_rows(&compound),
        filtered_deduped,
        "compound DISTINCT keeps filtered rank order"
    );

    // Determinism on the DISTINCT query.
    let replay = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DISTINCT_SCORED_QUERY,
        params,
    ));
    assert_eq!(replay, deduped, "DISTINCT frame is deterministic");
}

fn compound_rank_rows(result: &GqlQueryResult) -> Vec<i64> {
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
            match columns.get("rank").expect("rank column present") {
                GqlWireValue::Int64(rank) => *rank,
                other => panic!("rank must be Int64, got {other:?}"),
            }
        })
        .collect()
}

#[test]
fn non_leading_text_candidate_compound_lifecycle() {
    let fixture = seed_candidate_fixture();
    let env = fixture.env;

    // Calibration frame: the threshold-only barrier returns every candidate in
    // Router ranking order (score DESC, key ASC, prefix stable), so the compound
    // expectation is exactly the filtered frame truncated to the row limit.
    let frame = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_THRESHOLD_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", 0.5),
    ));
    assert_eq!(frame.len(), 42, "full candidate frame");
    let mut distinct: Vec<f64> = frame.iter().map(|(_, score)| *score).collect();
    distinct.sort_by(|a, b| a.total_cmp(b));
    distinct.dedup();
    let bound = distinct[(distinct.len() - 1) / 2];
    let expected: Vec<(i64, f64)> = frame
        .iter()
        .filter(|(_, score)| *score > bound)
        .take(5)
        .copied()
        .collect();
    assert_eq!(expected.len(), 5, "calibration must clear the row limit");

    // Scored compound: threshold first, then top-k — a truncate-then-filter
    // misimplementation would return fewer than 5 rows here.
    let compound = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_COMPOUND_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    );
    assert_eq!(
        compound.row_count, 5,
        "threshold-then-truncate keeps 5 rows"
    );
    assert_eq!(
        compound.truncated,
        Some(false),
        "exact compound never truncates"
    );
    assert_eq!(
        threshold_scored_rows(&compound),
        expected,
        "compound is the filtered frame truncated to the limit"
    );

    // Score-less compound: the ORDER BY call drives ranking without a residual
    // score column; ranks match the scored frame.
    let noscore = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_COMPOUND_NOSCORE_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    );
    assert_eq!(noscore.row_count, 5);
    assert_eq!(
        compound_rank_rows(&noscore),
        expected.iter().map(|(rank, _)| *rank).collect::<Vec<_>>(),
        "score-less ranking matches"
    );

    // Determinism on the scored compound.
    let replay = threshold_scored_rows(&gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_COMPOUND_SCORED_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", bound),
    ));
    assert_eq!(replay, expected, "compound frame is deterministic");
}

// ──── Candidate-scoped dual-score (same variable, two properties) ────

const CANDIDATE_DUAL_BLURB_PROPERTY: &str = "blurb";
const CANDIDATE_DUAL_INDEX_NAME: &str = "text_score_query_blurb_idx";
const CANDIDATE_DUAL_MIGRATION_ID: &str = "000104_text_score_query_blurb";
/// Document rank whose blurb carries no query term: its rows must drop.
const CANDIDATE_DUAL_DROP_RANK: i64 = 7;

/// Blurb bodies cycle wombat counts out of phase with the bio bodies, so the
/// blurb ranking differs from the bio ranking (an s2-ordered execution cannot
/// pass as s1-ordered). The drop rank carries no query term at all.
fn dual_blurb_text(i: i64) -> String {
    if i == CANDIDATE_DUAL_DROP_RANK {
        "quiet harbor notes".to_string()
    } else {
        format!(
            "wombat BLURB{i} {}",
            "wombat ".repeat((i * 5 + 3) as usize % 7 + 1)
        )
    }
}

const CANDIDATE_DUAL_BIO_FRAME_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS score ORDER BY score DESC LIMIT 42";
const CANDIDATE_DUAL_BLURB_FRAME_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) WHERE text_score(d.blurb,$q) > $t \
     RETURN d.rank AS rank, text_score(d.blurb,$q) AS score";
const CANDIDATE_DUAL_SCORED_QUERY: &str = "MATCH (u:User {uid:$user_id})-[:MEMBER_OF]->(p:Project) \
     MATCH (p)-[:HAS_DOCUMENT]->(d:Document) \
     RETURN d.rank AS rank, text_score(d.bio,$q) AS s1, text_score(d.blurb,$q) AS s2 ORDER BY s1 DESC LIMIT 42";

fn dual_query_params(user_id: i64, query: &str) -> Vec<u8> {
    encode_gql_params_blob(vec![
        ("user_id".to_string(), Value::Int64(user_id)),
        ("q".to_string(), Value::Text(query.to_string())),
    ])
    .expect("encode params")
}

/// Reads `(rank, s1, s2)` rows in order from a dual-score result.
fn dual_scored_rows(result: &GqlQueryResult) -> Vec<(i64, f64, f64)> {
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
            let rank = match columns.get("rank").expect("rank column present") {
                GqlWireValue::Int64(rank) => *rank,
                other => panic!("rank must be Int64, got {other:?}"),
            };
            let score = |name: &str| match columns
                .get(name)
                .unwrap_or_else(|| panic!("{name} column present"))
            {
                GqlWireValue::Float64(score) => *score,
                other => panic!("{name} must be Float64, got {other:?}"),
            };
            (rank, score("s1"), score("s2"))
        })
        .collect()
}

/// Composes the shared candidate fixture with a second indexed text property
/// (`blurb`) on the same documents. Other candidate tests keep the unextended
/// seed; only the dual-score lifecycle pays for the extra migration.
fn seed_dual_score_fixture() -> CandidateFixture {
    let fixture = seed_candidate_fixture();
    let env = &fixture.env;
    gleaph_pocket_ic_tests::ensure_property(&env.fed, CANDIDATE_DUAL_BLURB_PROPERTY);
    for i in 0..21 {
        let statement = format!(
            "MATCH (d:Document) WHERE d.rank = {i} SET d.blurb = '{}'",
            dual_blurb_text(i)
        );
        gleaph_pocket_ic_tests::gql_mutate_as_admin(
            &env.fed,
            &statement,
            &format!("seed-blurb-{i}"),
        );
    }
    let statement = format!(
        "CREATE TEXT INDEX {CANDIDATE_DUAL_INDEX_NAME} FOR (v:{LABEL}) ON (v.{CANDIDATE_DUAL_BLURB_PROPERTY}) ANALYZER unicode_bigram"
    );
    gleaph_pocket_ic_tests::gql_mutate_as_admin(&env.fed, &statement, "text-ddl-blurb");
    let canister = get_text_index_named(env, CANDIDATE_DUAL_INDEX_NAME)
        .canister
        .expect("provisioned blurb canister attached");
    env.fed.pic.add_cycles(canister, 20_000_000_000_000);
    // Settle the bio migration head to a terminal replay before chaining. The
    // shared seed stops driving once the index reads Ready, and the blurb SETs
    // plus the second DDL re-arm the bio backfill build — so the settle runs
    // here, after all writes, immediately before the chained prepare.
    let bio_statement = format!(
        "CREATE TEXT INDEX {INDEX_NAME} FOR (v:{LABEL}) ON (v.{PROPERTY}) ANALYZER unicode_bigram"
    );
    let bio_args = migration_args(MIGRATION_ID, &bio_statement);
    let mut settled = false;
    for _ in 0..128 {
        match try_apply_once(env, &bio_args) {
            Ok(result)
                if matches!(
                    result.status,
                    SchemaMigrationApplyStatus::Applied | SchemaMigrationApplyStatus::Replay
                ) =>
            {
                settled = true;
                break;
            }
            Ok(result) => assert!(
                matches!(result.status, SchemaMigrationApplyStatus::Progress(_)),
                "unexpected bio settle status: {:?}",
                result.status
            ),
            Err(err) => panic!("bio settle rejected: {err:?}"),
        }
    }
    assert!(settled, "bio migration head did not settle to Replay");

    // The ledger already roots at the bio migration: the blurb migration chains
    // under it (checksum binds the same parent).
    let blurb_selector = SchemaMigrationGraphSelector::Default;
    let args = ApplySchemaMigrationArgs::V1(ApplySchemaMigrationArgsV1 {
        id: CANDIDATE_DUAL_MIGRATION_ID.to_owned(),
        parent: Some(MIGRATION_ID.to_owned()),
        graph_selector: blurb_selector.clone(),
        checksum: gleaph_migration_api::schema_migration_checksum(
            CANDIDATE_DUAL_MIGRATION_ID,
            Some(MIGRATION_ID),
            &blurb_selector,
            statement.as_bytes(),
        ),
        statement: statement.clone(),
    });
    drive_to_ready_for(env, &args, CANDIDATE_DUAL_INDEX_NAME);
    assert_eq!(
        get_text_index_named(env, CANDIDATE_DUAL_INDEX_NAME).status,
        TextIndexStatusView::Ready
    );
    flush_until_done_for(env, CANDIDATE_DUAL_INDEX_NAME);
    fixture
}

#[test]
fn non_leading_text_candidate_dual_score_lifecycle() {
    let fixture = seed_dual_score_fixture();
    let env = fixture.env;

    // Calibration frames, both self-calibrating with no hand-computed scores:
    // the bio top-k (ORDER BY s1 source) and the blurb threshold (s2 source).
    let bio = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DUAL_BIO_FRAME_QUERY,
        dual_query_params(CANDIDATE_USER_ID, "wombat"),
    );
    let bio_frame = threshold_scored_rows(&bio);
    assert_eq!(bio_frame.len(), 42, "bio frame holds every candidate row");
    let blurb = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DUAL_BLURB_FRAME_QUERY,
        threshold_query_params(CANDIDATE_USER_ID, "wombat", 0.5),
    );
    let blurb_frame = threshold_scored_rows(&blurb);
    assert_eq!(
        blurb_frame.len(),
        40,
        "the drop document has no blurb postings (21 docs minus one, times two users)"
    );
    let blurb_by_rank: BTreeMap<i64, f64> = blurb_frame.into_iter().collect();
    // Premise: on the surviving set the blurb ranking differs from the bio
    // ranking, so an s2-ordered execution cannot pass as s1-ordered.
    let bio_order: Vec<i64> = bio_frame
        .iter()
        .map(|(rank, _)| *rank)
        .filter(|rank| *rank != CANDIDATE_DUAL_DROP_RANK)
        .collect();
    let mut blurb_ranked: Vec<(f64, i64)> = blurb_by_rank
        .iter()
        .map(|(rank, score)| (*score, *rank))
        .collect();
    blurb_ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    let blurb_order: Vec<i64> = blurb_ranked.into_iter().map(|(_, rank)| rank).collect();
    assert_ne!(
        bio_order, blurb_order,
        "blurb ranking must differ from bio ranking for the order-confusion kill"
    );

    // Dual score: rank by s1, project both, drop the scoreless blurb rows.
    let dual = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DUAL_SCORED_QUERY,
        dual_query_params(CANDIDATE_USER_ID, "wombat"),
    );
    assert_eq!(
        dual.row_count, 40,
        "rows missing either score drop symmetrically"
    );
    assert_eq!(
        dual.truncated,
        Some(false),
        "exact dual join never truncates"
    );
    let expected: Vec<(i64, f64, f64)> = bio_frame
        .iter()
        .filter(|(rank, _)| *rank != CANDIDATE_DUAL_DROP_RANK)
        .map(|(rank, s1)| (*rank, *s1, blurb_by_rank[rank]))
        .collect();
    assert_eq!(
        dual_scored_rows(&dual),
        expected,
        "s1 order with per-row s2: a second-join-ignoring execution cannot match"
    );

    let replay = gql_query_with_params_as_admin(
        &env.fed,
        CANDIDATE_DUAL_SCORED_QUERY,
        dual_query_params(CANDIDATE_USER_ID, "wombat"),
    );
    assert_eq!(
        dual_scored_rows(&replay),
        expected,
        "dual-score replay is deterministic"
    );
}

/// Live dual-score contract: the second triple resolves BEFORE any TEXT I/O, so
/// a dual query whose second property has no Ready index fails closed with the
/// function-unknown NotFound — never a partial single-score frame, never a hang.
#[test]
fn non_leading_text_candidate_dual_score_unready_lifecycle() {
    let fixture = seed_candidate_fixture();
    let env = fixture.env;
    // The property exists (planner-plausible triple) but no TEXT definition
    // covers it: resolution fails closed before the first candidate call.
    gleaph_pocket_ic_tests::ensure_property(&env.fed, CANDIDATE_DUAL_BLURB_PROPERTY);
    let err = raw_gql_query(
        &env,
        CANDIDATE_DUAL_SCORED_QUERY,
        dual_query_params(CANDIDATE_USER_ID, "wombat"),
    )
    .expect_err("dual score without a second Ready index must fail closed");
    let message = err.to_string();
    assert!(
        message.contains("no ready TEXT index covers text_score"),
        "unexpected unready second-index error: {message}"
    );
}
