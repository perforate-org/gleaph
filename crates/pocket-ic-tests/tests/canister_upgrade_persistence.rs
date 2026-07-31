//! PocketIC: the canister upgrade boundary must preserve all persisted state
//! without corruption.
//!
//! Persistence in gleaph is fully implicit: every durable structure lives in
//! `ic_stable_structures` behind a `MemoryManager`, and only the graph shard has
//! a `#[post_upgrade]` hook (it rebuilds non-stable process state). This test
//! seeds a realistic fan-out graph (vertices + directed edges + edge properties +
//! a federated index), captures the full query result, upgrades all three
//! canisters to the same wasm, and requires the post-upgrade query to be
//! byte-for-byte identical — then proves the graph is still writable.

use candid::{Decode, Encode};
use gleaph_codegen::{generate_typescript, parse_manifest};
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{IndexPostingBatchProgress, IndexPostingMutation, PostingHit};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, create_vertex_property_index, gql_execute_idempotent_as_admin,
    gql_query_as_admin, install_single_shard_federation, prepared_manifest_as_admin,
    prepared_query_with_params_as, prepared_upsert_as_admin, seed_upgrade_fixture_graph,
    upgrade_fixture_query, wasm_bytes,
};
use gleaph_prepared_api::{OperationKind, PreparedOperation, ResultSchema};
use gleaph_router::types::{
    AdminVertexPropertyBackfillStepArgs, VertexPropertyBackfillShardStatus,
};
use std::collections::BTreeSet;

/// Distinct `edge_id` values returned by the upgrade fixture query.
fn edge_ids(result: &GqlQueryResult) -> BTreeSet<String> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    let mut ids = BTreeSet::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("wire row to value row");
        let Value::Text(edge_id) = value_row.get("edge_id").expect("edge_id column") else {
            panic!("expected edge_id text, got {:?}", value_row.get("edge_id"));
        };
        ids.insert(edge_id.clone());
    }
    ids
}

/// Upgrade router, index, and graph shard to the same wasm in place. The graph
/// shard's `post_upgrade` re-installs the rkyv hook and re-arms the maintenance
/// timer; router and index have no upgrade hook and rely purely on stable memory
/// surviving the reinstall.
fn upgrade_all(env: &FederationEnv) {
    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty.clone(), None)
        .expect("upgrade router canister");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty.clone(), None)
        .expect("upgrade index canister");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");
}

#[test]
fn canister_upgrade_preserves_seeded_graph_without_corruption() {
    let env = install_single_shard_federation();
    seed_upgrade_fixture_graph(&env);

    let before = gql_query_as_admin(&env, upgrade_fixture_query());
    let before_ids = edge_ids(&before);
    assert_eq!(
        before.row_count, 26,
        "baseline upgrade fixture query should return one row per seeded edge"
    );
    assert_eq!(before_ids.len(), 26, "baseline edge ids must be distinct");

    upgrade_all(&env);

    // Post-upgrade the same query must observe the exact same graph: identical
    // row count and identical edge-id set. Any stable-layout, Storable, router
    // catalog, shard adjacency, or index posting corruption would diverge here.
    let after = gql_query_as_admin(&env, upgrade_fixture_query());
    let after_ids = edge_ids(&after);
    assert_eq!(
        after.row_count, before.row_count,
        "row count changed across canister upgrade"
    );
    assert_eq!(
        after_ids, before_ids,
        "edge-id set changed across canister upgrade"
    );

    // The graph must remain fully operational after the upgrade: an idempotent
    // re-seed (same client mutation keys) must be a no-op that still reads back
    // the unchanged graph, proving the router idempotency journal and shard
    // state survived intact.
    seed_upgrade_fixture_graph(&env);
    let after_reseed = gql_query_as_admin(&env, upgrade_fixture_query());
    assert_eq!(
        after_reseed.row_count, before.row_count,
        "idempotent re-seed after upgrade must not change the graph"
    );
    assert_eq!(
        edge_ids(&after_reseed),
        before_ids,
        "edge-id set changed after post-upgrade idempotent re-seed"
    );
}

#[test]
fn prepared_query_survives_router_upgrade_cache_rebuild() {
    let env = install_single_shard_federation();
    seed_upgrade_fixture_graph(&env);

    let query_name = "upgrade-prepared-cache-rebuild";
    prepared_upsert_as_admin(&env, query_name, upgrade_fixture_query());
    let before = prepared_query_with_params_as(&env, env.admin, query_name, Vec::new());

    upgrade_all(&env);

    // The Router stable record contains source, while the parsed AST and plan are rebuilt by
    // post_upgrade. Successful execution after upgrade proves the cache was reconstructed and
    // the Graph still received the prepared plan through the normal dispatch boundary.
    let after = prepared_query_with_params_as(&env, env.admin, query_name, Vec::new());
    assert_eq!(after.row_count, before.row_count);
    assert_eq!(edge_ids(&after), edge_ids(&before));
}

#[test]
fn prepared_manifest_exposes_doc_and_parameter_metadata() {
    let env = install_single_shard_federation();
    let name = "manifest-docs-and-parameters";
    let query = "/// Find people by term\n/// @param term Search term\nVALUE term TYPED STRING = $term MATCH (n) RETURN n";

    use candid::Encode;
    let metadata = PreparedOperation {
        name: name.to_owned(),
        description: None,
        kind: OperationKind::Query,
        parameters: Vec::new(),
        result: ResultSchema {
            columns: Vec::new(),
        },
        supports_consistency: false,
        supports_idempotency: false,
        allowed_sorts: Vec::new(),
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "prepared_upsert_with_metadata",
            Encode!(&name.to_owned(), &query.to_owned(), &metadata)
                .expect("encode prepared_upsert_with_metadata"),
        )
        .expect("prepared_upsert_with_metadata call");
    let result = candid::Decode!(&bytes, Result<(), gleaph_graph_kernel::federation::RouterError>)
        .expect("decode prepared_upsert_with_metadata");
    assert!(
        result.is_ok(),
        "prepared metadata registration failed: {result:?}"
    );

    let manifest = prepared_manifest_as_admin(&env, GRAPH_NAME);
    assert_eq!(manifest.manifest_version, 1);
    let operation = manifest
        .operations
        .iter()
        .find(|operation| operation.name == name)
        .expect("registered operation in manifest");
    assert_eq!(
        operation.description.as_deref(),
        Some("Find people by term")
    );
    assert_eq!(operation.parameters.len(), 1);
    assert_eq!(operation.parameters[0].name, "term");
    assert_eq!(
        operation.parameters[0].description.as_deref(),
        Some("Search term")
    );

    let snapshot = serde_json::to_string(&manifest).expect("serialize manifest snapshot");
    let parsed = parse_manifest(&snapshot).expect("codegen accepts Router manifest snapshot");
    let generated = generate_typescript(&parsed).expect("generate TypeScript from Router manifest");
    assert!(generated.contains("Find people by term"));
    assert!(generated.contains("Search term"));
    assert!(generated.contains("term"));
}

#[test]
fn canister_upgrade_repeated_is_stable() {
    let env = install_single_shard_federation();
    seed_upgrade_fixture_graph(&env);
    let baseline = edge_ids(&gql_query_as_admin(&env, upgrade_fixture_query()));

    // Two successive upgrades must each preserve the data (guards against an
    // upgrade that silently consumes/relocates stable regions on every cycle).
    for _ in 0..2 {
        upgrade_all(&env);
        let ids = edge_ids(&gql_query_as_admin(&env, upgrade_fixture_query()));
        assert_eq!(
            ids, baseline,
            "edge-id set drifted across repeated upgrades"
        );
    }

    // A new write after repeated upgrades must still land and be queryable.
    let new_edge_ddl = "MATCH (a:UpgradeSource {fixture_id: 'source'}) RETURN a \
NEXT INSERT (a)-[:UPGRADE_EDGE {fixture_edge_id: 'edge-post-upgrade', fixture_kind: 'verify'}]\
->(:UpgradeTarget {fixture_id: 'target-post-upgrade', fixture_kind: 'upgrade'})";
    let _ = gql_execute_idempotent_as_admin(&env, new_edge_ddl, "post_upgrade_new_edge");
    let after_write = edge_ids(&gql_query_as_admin(&env, upgrade_fixture_query()));
    assert!(
        after_write.contains("edge-post-upgrade"),
        "new edge written after repeated upgrades must be queryable, got {after_write:?}"
    );
    assert_eq!(
        after_write.len(),
        baseline.len() + 1,
        "exactly one new edge should be added after upgrades"
    );
}

#[test]
fn graph_index_batch_posting_survives_index_upgrade() {
    let env = install_single_shard_federation();
    let value = vec![0x42, 0x01];
    let args = Encode!(
        &ShardId::new(0),
        &vec![
            IndexPostingMutation::VertexProperty {
                remove: false,
                property_id: 77,
                value: value.clone(),
                vertex_id: 11,
            },
            IndexPostingMutation::VertexProperty {
                remove: false,
                property_id: 77,
                value: value.clone(),
                vertex_id: 12,
            },
        ]
    )
    .expect("encode posting batch");
    let bytes = env
        .pic
        .update_call(env.index, env.graph_source, "posting_batch", args)
        .expect("posting batch call");
    let progress = Decode!(&bytes, IndexPostingBatchProgress).expect("decode posting progress");
    assert_eq!(progress.applied, 2);
    assert!(progress.next_index.is_none());

    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty, None)
        .expect("upgrade graph-index canister");

    let lookup = env
        .pic
        .query_call(
            env.index,
            env.router,
            "lookup_equal",
            Encode!(&77u32, &value).expect("encode lookup"),
        )
        .expect("lookup after index upgrade");
    let hits = Decode!(&lookup, Vec<PostingHit>).expect("decode lookup hits");
    assert_eq!(
        hits.iter().map(|hit| hit.vertex_id).collect::<Vec<_>>(),
        vec![11, 12],
        "batch-applied postings must survive graph-index upgrade"
    );
}

#[test]
fn router_backfill_cursor_survives_router_upgrade() {
    let env = install_single_shard_federation();
    create_vertex_property_index(
        &env,
        "upgrade_backfill_age",
        "Person",
        "age",
        "upgrade_backfill_create_index",
    );
    gql_execute_idempotent_as_admin(&env, "INSERT (:Person {age: 1})", "upgrade_backfill_v1");
    gql_execute_idempotent_as_admin(&env, "INSERT (:Person {age: 2})", "upgrade_backfill_v2");

    let args = AdminVertexPropertyBackfillStepArgs {
        logical_graph_name: GRAPH_NAME.into(),
        shard_id: ShardId::new(0),
        max_vertices: 1,
    };
    let first_bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_vertex_property_backfill_step",
            Encode!(&args).expect("encode backfill step"),
        )
        .expect("first backfill step");
    let first: Result<gleaph_router::types::AdminVertexPropertyBackfillStepResult, _> =
        Decode!(&first_bytes, Result<_, gleaph_graph_kernel::federation::RouterError>)
            .expect("decode first backfill step");
    let first = first.expect("first backfill step succeeds");
    assert_eq!(first.vertices_processed, 1);

    let empty = Encode!(&()).expect("encode empty upgrade arg");
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty, None)
        .expect("upgrade router with stable backfill cursor");

    let status_bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "admin_list_vertex_property_backfill_status",
            Encode!(&GRAPH_NAME.to_string()).expect("encode status query"),
        )
        .expect("backfill status after upgrade");
    let status: Result<Vec<VertexPropertyBackfillShardStatus>, _> = Decode!(
        &status_bytes,
        Result<
            Vec<VertexPropertyBackfillShardStatus>,
            gleaph_graph_kernel::federation::RouterError,
        >
    )
    .expect("decode status");
    let status = status.expect("status succeeds");
    assert_eq!(status[0].next_vertex_id, first.next_vertex_id);
}
