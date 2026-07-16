//! PocketIC: router `gql_query` composite path (parse → plan → graph dispatch).
//!
//! Covers single-shard NodeScan, index-seeded property equality, `ELEMENT_ID` rows, and
//! multi-shard router index lookup with per-shard `seed_bindings_blob` fan-out.

use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId};
use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
use gleaph_pocket_ic_tests::{
    DEST_SHARD, SOURCE_SHARD, admin_intern_edge_label, admin_intern_property,
    create_directed_edge_property_index, create_edge_property_index,
    create_undirected_edge_property_index, create_vertex_property_index,
    drain_maintenance_via_timer, drop_vertex_property_index,
    e2e_insert_directed_edge_with_property, e2e_insert_undirected_edge_with_property,
    e2e_insert_vertex, e2e_insert_vertex_with_property, e2e_insert_vertex_with_two_properties,
    gql_execute_idempotent_as_admin, gql_execute_idempotent_as_admin_expect_err,
    gql_execute_idempotent_result_as_admin, gql_query_as_admin, gql_query_as_admin_expect_err,
    gql_query_with_consistency_as_admin, graph_index_pending_min_mutation_id, install_federation,
    install_single_shard_federation, knowledge_map_live_query, mutation_status_as_admin,
    run_router_recovery_timer, seed_knowledge_map_graph, start_graph_shard, stop_graph_shard,
    test_inject_projection_pending_saga,
};

const INDEX_VERTEX_LABEL: &str = "Person";
const INDEX_AGE_NAME: &str = "pocket_ic_vertex_age";
const INDEX_SCORE_NAME: &str = "pocket_ic_vertex_score";
const INDEX_EDGE_LABEL: &str = "KNOWS";
// Federated vertex-index lifecycle fixtures (plan 0032).
const FEDERATED_VERTEX_LIFECYCLE_LABEL: &str = "Person";
const FEDERATED_VERTEX_LIFECYCLE_AGE: &str = "pocket_ic_federated_lifecycle_vertex_age";
const FEDERATED_VERTEX_LIFECYCLE_SCORE: &str = "pocket_ic_federated_lifecycle_vertex_score";
const FEDERATED_VERTEX_LIFECYCLE_HIT_AGE: i64 = 3000;
const FEDERATED_VERTEX_LIFECYCLE_MERGE_AGE: i64 = 500;
const FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_AGE: i64 = 600;
const FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_SCORE: i64 = 7000;

// Federated edge-index lifecycle fixtures (plan 0032).
const FEDERATED_EDGE_LIFECYCLE_LABEL_UNDIR: &str = "FederatedLifecycleKnowsUndir";
const FEDERATED_EDGE_LIFECYCLE_LABEL_RIGHT: &str = "FederatedLifecycleKnowsRight";
const FEDERATED_EDGE_LIFECYCLE_LABEL_DROP: &str = "FederatedLifecycleKnowsDrop";
const FEDERATED_EDGE_LIFECYCLE_WEIGHT_UNDIR: &str =
    "pocket_ic_federated_lifecycle_edge_weight_undir";
const FEDERATED_EDGE_LIFECYCLE_WEIGHT_RIGHT: &str =
    "pocket_ic_federated_lifecycle_edge_weight_right";
const FEDERATED_EDGE_LIFECYCLE_WEIGHT_DROP: &str = "pocket_ic_federated_lifecycle_edge_weight_drop";
const FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_UNDIR: i64 = 500;
const FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_RIGHT: i64 = 600;
const FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_DROP: i64 = 700;
const LIFECYCLE_EDGE_LABEL_GENERIC: &str = "LifecycleKnowsGeneric";
const LIFECYCLE_EDGE_LABEL_RIGHT: &str = "LifecycleKnowsRight";
const LIFECYCLE_EDGE_LABEL_UNDIR: &str = "LifecycleKnowsUndir";
const LIFECYCLE_EDGE_WEIGHT_NAME: &str = "pocket_ic_lifecycle_edge_weight";
const LIFECYCLE_EDGE_WEIGHT_RIGHT_NAME: &str = "pocket_ic_lifecycle_edge_weight_right";
const LIFECYCLE_EDGE_WEIGHT_UNDIR_NAME: &str = "pocket_ic_lifecycle_edge_weight_undir";

/// Consolidated lifecycle for the three former single-shard identity contracts:
///
/// 1. `router_gql_query_node_scan_on_single_shard`
/// 2. `standalone_e2e_insert_assigns_global_id`
/// 3. `standalone_gql_query_returns_element_id_bytes`
///
/// One fresh PocketIC federation, one inserted vertex, and three exact assertions:
/// NodeScan returns one row; `GlobalVertexId` matches `(SOURCE_SHARD, local_vertex_id)`;
/// `ELEMENT_ID(n)` bytes decode through the Router graph encoding key to that exact global id.
#[test]
fn single_shard_identity_lifecycle() {
    let env = install_single_shard_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);

    let scan = gql_query_as_admin(&env, "MATCH (n) RETURN n");
    assert_eq!(
        scan.row_count, 1,
        "NodeScan over the one-vertex fixture should return exactly one row"
    );

    assert_eq!(inserted.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(
        inserted.global_vertex_id.local_vertex_id,
        inserted.local_vertex_id
    );
    let expected_id = GlobalVertexId::new(SOURCE_SHARD, inserted.local_vertex_id);
    assert_eq!(inserted.global_vertex_id, expected_id);

    let element_id = gql_query_as_admin(&env, "MATCH (n) RETURN ELEMENT_ID(n) AS id");
    assert_eq!(
        element_id.row_count, 1,
        "ELEMENT_ID projection should return one row"
    );
    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );
    let id_bytes = gleaph_pocket_ic_tests::element_id_bytes_from_gql_result(&element_id, "id");
    let decoded = GraphPathVertexId::try_from_slice(id_bytes.as_ref())
        .expect("decode vertex ELEMENT_ID bytes")
        .decode_global(&encoding_key);
    assert_eq!(
        decoded, inserted.global_vertex_id,
        "ELEMENT_ID bytes should decode to the inserted vertex's global id"
    );
}

/// Consolidated lifecycle for the five former standalone vertex-index contracts:
///
/// 1. `standalone_gql_query_index_seeded_property_eq`
/// 2. `standalone_gql_query_index_intersection_two_properties`
/// 3. `standalone_drop_index_property_eq_still_queries_via_scan`
/// 4. `drop_index_if_exists_is_idempotent`
/// 5. `drop_index_without_if_exists_errors_when_missing`
///
/// State machine: absent -> created -> indexed equality -> two-index intersection ->
/// dropped -> scan fallback -> idempotent IF EXISTS drop -> missing DROP error.
#[test]
fn single_shard_vertex_index_lifecycle() {
    let env = install_single_shard_federation();
    let age = admin_intern_property(&env, "age");
    let score = admin_intern_property(&env, "score");

    // Unique seed values isolate each contract within the shared environment.
    const EQUALITY_AGE: i64 = 1000;
    const INTERSECTION_AGE: i64 = 500;
    const INTERSECTION_SCORE: i64 = 6000;

    // Create both indexes up front so every later insert is posted.
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "lifecycle_create_age",
    );
    create_vertex_property_index(
        &env,
        INDEX_SCORE_NAME,
        INDEX_VERTEX_LABEL,
        "score",
        "lifecycle_create_score",
    );

    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), EQUALITY_AGE);
    assert_indexed_equality_lookup(
        &env,
        "age",
        EQUALITY_AGE,
        "former: standalone_gql_query_index_seeded_property_eq",
    );

    let _ = e2e_insert_vertex_with_two_properties(
        &env,
        env.graph_source,
        age.raw(),
        INTERSECTION_AGE,
        score.raw(),
        INTERSECTION_SCORE,
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), INTERSECTION_AGE);

    assert_indexed_intersection(
        &env,
        "age",
        INTERSECTION_AGE,
        "score",
        INTERSECTION_SCORE,
        "former: standalone_gql_query_index_intersection_two_properties",
    );

    // Drop the age index; canonical data must survive and scan fallback must answer.
    drop_vertex_property_index(&env, INDEX_AGE_NAME, true, "lifecycle_drop_age");

    assert_scan_fallback_after_drop(
        &env,
        "age",
        EQUALITY_AGE,
        3,
        "former: standalone_drop_index_property_eq_still_queries_via_scan",
    );

    // Idempotent DROP ... IF EXISTS on an already absent index succeeds.
    drop_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        true,
        "lifecycle_drop_age_if_exists_again",
    );

    // Bare DROP on a missing index returns NotFound.
    assert_missing_index_drop_error(&env);
}

fn assert_indexed_equality_lookup(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    property_name: &str,
    value: i64,
    context: &str,
) {
    let result = gql_query_as_admin(
        env,
        &format!("MATCH (n {{{property_name}: {value}}}) RETURN n"),
    );
    assert_eq!(
        result.row_count, 1,
        "indexed equality lookup should return exactly one match ({context})"
    );
}
/// Two indexed equality predicates on one variable produce a `PlanOp::IndexIntersection`,
/// served by the streaming `lookup_equal_page` + `filter_hits_by_equal` path on graph-index.
fn assert_indexed_intersection(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    property_a_name: &str,
    value_a: i64,
    property_b_name: &str,
    value_b: i64,
    context: &str,
) {
    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (n {{{property_a_name}: {value_a}, {property_b_name}: {value_b}}}) RETURN n"
        ),
    );
    assert_eq!(
        result.row_count, 1,
        "two-property index intersection should return only the vertex matching both arms ({context})"
    );
}

fn assert_scan_fallback_after_drop(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    property_name: &str,
    value: i64,
    expected_total: u64,
    context: &str,
) {
    let by_scan = gql_query_as_admin(
        env,
        &format!("MATCH (n {{{property_name}: {value}}}) RETURN n"),
    );
    assert_eq!(
        by_scan.row_count, 1,
        "single-shard scan path should still match after DROP INDEX ({context})"
    );

    let all_nodes = gql_query_as_admin(env, "MATCH (n) RETURN n");
    assert_eq!(
        all_nodes.row_count, expected_total,
        "canonical vertices must remain after DROP INDEX ({context})"
    );
}

fn assert_missing_index_drop_error(env: &gleaph_pocket_ic_tests::FederationEnv) {
    let err = gql_execute_idempotent_as_admin_expect_err(
        env,
        &format!("DROP INDEX {INDEX_AGE_NAME}"),
        "lifecycle_drop_missing_age",
    );
    assert!(
        matches!(
            err,
            gleaph_graph_kernel::federation::RouterError::NotFound(_)
        ),
        "expected NotFound for missing index, got: {err:?}"
    );
}

/// Former `standalone_gql_query_returns_relationship_rows_for_knowledge_map_adapter`.
///
/// Helper-seeded `KNOWS {weight: 5}` edge: exact source, edge, target, and property columns.
/// Kept in a fresh fixture because its exact row membership (`row_count == 1`) is incompatible
/// with the GQL-insert path and with the broader demo fan-out graph.
#[test]
fn single_shard_knowledge_map_relationship_rows() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let edge_label = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        edge_label.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:KNOWS {weight: 5}]->(b) \
         RETURN ELEMENT_ID(a) AS source_id, ELEMENT_ID(e) AS edge_id, \
                ELEMENT_ID(b) AS target_id, e.weight AS edge_weight",
    );

    assert_eq!(result.row_count, 1);
    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );
    let row = decode_single_value_row(&result);
    assert_eq!(
        vertex_id_column(&row, "source_id", &encoding_key),
        source.global_vertex_id,
        "source ELEMENT_ID should identify the seeded source vertex"
    );
    assert_eq!(
        vertex_id_column(&row, "target_id", &encoding_key),
        target.global_vertex_id,
        "target ELEMENT_ID should identify the seeded target vertex"
    );
    let Value::Bytes(edge_id) = row.get("edge_id").expect("edge_id column") else {
        panic!("expected edge_id bytes, got {:?}", row.get("edge_id"));
    };
    GraphPathEdgeId::try_from_slice(edge_id.as_ref()).expect("edge ELEMENT_ID bytes");
    assert_eq!(
        row.get("edge_weight"),
        Some(&Value::Int64(5)),
        "edge property should be projected for adapter row metadata"
    );
}

/// Former `router_gql_insert_seeds_knowledge_map_fan_out_graph`.
///
/// Seeds the full knowledge-map demo graph (26 unique demo edges) through idempotent Router
/// inserts and asserts the live query returns 26 unique edge ids, including the required
/// representative ids `alice-storage` and `project-lara`.
#[test]
fn single_shard_knowledge_map_fan_out() {
    let env = install_single_shard_federation();
    seed_knowledge_map_graph(&env);

    let result = gql_query_as_admin(&env, knowledge_map_live_query());
    assert_eq!(
        result.row_count, 26,
        "knowledge-map live query should return one row per seeded demo edge"
    );

    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    assert_eq!(wire.rows.len(), 26);

    let mut edge_ids = std::collections::BTreeSet::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("wire row to value row");
        let Value::Text(edge_id) = value_row.get("edge_id").expect("edge_id column") else {
            panic!("expected edge_id text, got {:?}", value_row.get("edge_id"));
        };
        edge_ids.insert(edge_id.clone());
    }

    assert!(
        edge_ids.contains("alice-storage"),
        "expected alice-storage edge, got {edge_ids:?}"
    );
    assert!(
        edge_ids.contains("project-lara"),
        "expected project-lara edge, got {edge_ids:?}"
    );
}

#[test]
fn router_rejects_federated_match_based_multi_dml_bundle_before_dispatch() {
    // ADR 0029 Phase 5: a MATCH-based (non-pure-insert) bundle of more than one top-level DML
    // statement on a federated (multi-shard) graph has no defined cross-shard partial-application
    // contract, so the Router rejects it before any shard dispatch with `UnsupportedMultiDmlBundle`
    // — no canonical or projection state changes. Completely-new INSERT-only bundles are exempt
    // (contract 1, see `router_places_completely_new_insert_bundle_on_latest_shard`).
    use gleaph_graph_kernel::federation::RouterError;

    let env = install_federation();

    let err = gql_execute_idempotent_as_admin_expect_err(
        &env,
        "MATCH (n:Person) SET n.x = 1 NEXT MATCH (m:Project) SET m.y = 2",
        "router_rejects_federated_match_based_multi_dml_bundle_before_dispatch",
    );
    assert!(
        matches!(
            err,
            RouterError::UnsupportedMultiDmlBundle {
                dml_statements: 2,
                shard_count: 2,
            }
        ),
        "expected UnsupportedMultiDmlBundle for a 2-statement MATCH bundle on a 2-shard graph, got {err:?}"
    );
}

#[test]
fn router_allows_multi_dml_bundle_on_single_shard() {
    // ADR 0029 Phase 5: multi-DML stays shard-local atomic on a single-shard graph (Phase 1),
    // so the federated multi-DML gate must not reject it. Both inserts apply in one canonical
    // segment on the one shard.
    let env = install_single_shard_federation();

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person) NEXT INSERT (:Project)",
        "router_allows_multi_dml_bundle_on_single_shard",
    );
    assert!(
        result.token.is_some(),
        "single-shard multi-DML executes and issues a mutation token"
    );
}

/// Consolidated lifecycle for the two former latest-shard placement contracts:
///
/// 1. `router_places_completely_new_single_insert_on_latest_shard`
/// 2. `router_places_completely_new_insert_bundle_on_latest_shard`
///
/// One fresh multi-shard federation, distinct labels and client mutation keys, and exact
/// assertions that both the single INSERT and the pure INSERT bundle land on `DEST_SHARD`.
#[test]
fn federated_pure_insert_placement_lifecycle() {
    let env = install_federation();

    // Single completely-new INSERT: no index anchor, placed on the graph's latest shard.
    let single = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person)",
        "federated_pure_insert_placement_single",
    );
    let single_token = single
        .token
        .expect("a completely-new federated INSERT issues a mutation token");
    assert_eq!(
        single_token.shards.len(),
        1,
        "a pure single INSERT is placed on exactly one shard"
    );
    assert_eq!(
        single_token.shards[0].shard_id, DEST_SHARD,
        "the single INSERT lands on the graph's latest shard"
    );

    // Completely-new INSERT-only bundle: still co-placed on the latest shard atomically.
    let bundle = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Project) NEXT INSERT (:Thing)",
        "federated_pure_insert_placement_bundle",
    );
    let bundle_token = bundle
        .token
        .expect("a completely-new federated INSERT bundle issues a mutation token");
    assert_eq!(
        bundle_token.shards.len(),
        1,
        "the whole bundle is co-placed on one shard"
    );
    assert_eq!(
        bundle_token.shards[0].shard_id, DEST_SHARD,
        "the bundle lands on the graph's latest shard"
    );
}

#[test]
fn router_runs_anchored_multi_dml_bundle_when_anchor_resolves_to_one_shard() {
    // ADR 0029 Phase 5 (contract 1, anchored single-shard): a multi-DML bundle whose single
    // leading anchor resolves to exactly one shard performs no cross-shard reads, so the whole
    // bundle runs atomically on that shard. Here the `age = 5` anchor exists only on SOURCE_SHARD,
    // so the SET + threaded INSERT bundle is admitted and the token names exactly that one shard.
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "router_runs_anchored_multi_dml_bundle_when_anchor_resolves_to_one_shard",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "MATCH (n {age: 5}) SET n.age = 6 NEXT INSERT (n)-[:KNOWS]->(:Project)",
        "router_runs_anchored_multi_dml_bundle_when_anchor_resolves_to_one_shard",
    );
    let token = result
        .token
        .expect("an admitted anchored multi-DML bundle issues a mutation token");
    assert_eq!(
        token.shards.len(),
        1,
        "the anchor resolves to a single shard, so the bundle touches one shard"
    );
    assert_eq!(
        token.shards[0].shard_id, SOURCE_SHARD,
        "the bundle runs on the shard the anchor resolved to"
    );
}

#[test]
fn router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga() {
    // ADR 0029 Phase 5 (contract 2, roll-forward bundle): a single-anchor threaded multi-DML bundle
    // whose leading anchor fans out to more than one shard is dispatched per shard as a roll-forward
    // saga. It performs no cross-shard read, so each shard runs the whole bundle over its own anchor
    // rows atomically (shard-local), and cross-shard convergence is roll-forward. Here `age = 5`
    // exists on both shards, so the SET + threaded INSERT bundle applies on both; the happy path
    // (both shards reachable) converges to `Completed` and updates both shards' vertices.
    use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;

    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let dest = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);
    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest.global_vertex_id.shard_id, DEST_SHARD);

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "MATCH (n {age: 5}) SET n.age = 6 NEXT INSERT (n)-[:KNOWS]->(:Project)",
        "router_runs_anchored_multi_dml_bundle_across_shards_as_roll_forward_saga",
    );
    let token = result
        .token
        .expect("an admitted anchored multi-DML bundle issues a mutation token");
    assert_eq!(
        token.shards.len(),
        2,
        "the anchor fans out to both shards, so the bundle touches two shards"
    );
    assert_eq!(
        result.phase,
        Some(MutationLifecyclePhase::Completed),
        "with both shards reachable the roll-forward saga converges immediately"
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    drain_maintenance_via_timer(&env, env.graph_dest);

    // The age = 6 read anchors on the age index and fans out to both shards; it confirms the SET
    // applied on each. The threaded `INSERT (n)-[:KNOWS]->(:Project)` runs in the same shard-local
    // atomic segment as the SET, so a `Completed` phase plus this read is sufficient evidence that
    // the whole bundle committed on both shards.
    let updated = gql_query_as_admin(&env, "MATCH (n {age: 6}) RETURN n");
    assert_eq!(
        updated.row_count, 2,
        "both shards' anchor vertices were updated by the bundle"
    );
}

#[test]
fn router_recovers_anchored_multi_dml_roll_forward_saga_via_idempotent_retry() {
    // ADR 0029 Phase 5 (contract 2, roll-forward bundle): the multi-DML fan-out is a saga, so a
    // shard crashed mid-bundle leaves the mutation non-terminal (`CanonicalPending`) rather than
    // corrupting state. The autonomous timer never re-applies canonical DML, so the saga stays
    // pending while the shard is down; after it restarts, an idempotent retry on the same
    // `client_mutation_key` resumes only the outstanding shard (the committed shard is deduplicated
    // by `mutation_id`) and converges the bundle to `Completed`.
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;

    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "router_recovers_anchored_multi_dml_roll_forward_saga_via_idempotent_retry",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let dest = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);
    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest.global_vertex_id.shard_id, DEST_SHARD);

    let key = "router_recovers_anchored_multi_dml_roll_forward_saga";
    let bundle = "MATCH (n {age: 5}) SET n.age = 6 NEXT INSERT (n)-[:KNOWS]->(:Project)";

    stop_graph_shard(&env, env.graph_dest);
    let err = gql_execute_idempotent_as_admin_expect_err(&env, bundle, key);
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "a crashed shard fails the federated bundle, got {err:?}"
    );

    let pending = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status for the in-flight bundle saga");
    assert_eq!(
        pending.phase,
        MutationLifecyclePhase::CanonicalPending,
        "an outstanding canonical shard write leaves the bundle saga CanonicalPending"
    );

    run_router_recovery_timer(&env);
    let still_pending = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status after a recovery tick with the shard still down");
    assert_eq!(
        still_pending.phase,
        MutationLifecyclePhase::CanonicalPending,
        "the timer leaves an unavailable canonical shard pending (no double-apply)"
    );

    start_graph_shard(&env, env.graph_dest);
    let resumed = gql_execute_idempotent_result_as_admin(&env, bundle, key);
    assert_eq!(
        resumed.phase,
        Some(MutationLifecyclePhase::Completed),
        "the idempotent retry resumes the outstanding shard and converges the bundle"
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    drain_maintenance_via_timer(&env, env.graph_dest);

    let updated = gql_query_as_admin(&env, "MATCH (n {age: 6}) RETURN n");
    assert_eq!(
        updated.row_count, 2,
        "both shards' anchor vertices converged to the updated value"
    );
}

/// Consolidated lifecycle for the three former single-shard mutation consistency contracts:
///
/// 1. `router_idempotent_dml_issues_mutation_token_and_exposes_index_watermark`
/// 2. `router_atleast_read_barrier_serves_when_satisfied_and_lags_when_unmet`
/// 3. `router_mutation_status_reports_completed_and_recovery_timer_is_safe_noop`
///
/// One fresh single-shard federation, one idempotent INSERT that creates edge and property
/// posting work, and sequential assertions: the issued token carries a mutation id and shard
/// watermarks; the graph-index watermark is clear after inline flush; the same token satisfies an
/// `AtLeast` read-your-writes barrier for the `Person` label; an artificially lagging watermark
/// returns retryable `ProjectionLag`; `Canonical` is rejected; `Eventual` remains non-blocking;
/// `mutation_status` reports `Completed`; the recovery timer leaves the terminal saga untouched;
/// and an unknown key returns `InvalidArgument`.
#[test]
fn single_shard_mutation_token_barrier_status_lifecycle() {
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::{MutationLifecyclePhase, MutationTokenShard, ReadMode};

    let env = install_single_shard_federation();
    let key = "single_shard_mutation_token_barrier_status_lifecycle";

    // A non-vacuous mutation that creates both a vertex and an edge with a property. Edge/property
    // postings give the graph-index pending watermark something real to track, so the
    // `graph_index_pending_min_mutation_id == None` assertion below verifies inline flush rather
    // than a no-work path.
    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 5}]->(:Project)",
        key,
    );

    let token = result
        .token
        .expect("idempotent DML must issue a mutation token");
    assert_ne!(token.mutation_id, 0, "token carries a real mutation id");
    assert!(
        !token.shards.is_empty(),
        "token names the shards that participated in the mutation"
    );
    assert!(
        result.phase.is_some(),
        "idempotent DML reports a lifecycle phase"
    );
    drain_maintenance_via_timer(&env, env.graph_source);

    // Happy-path flush applies edge/property postings inline; no tracked mutation is left pending.
    assert_eq!(
        graph_index_pending_min_mutation_id(&env, env.graph_source),
        None,
        "a successfully flushed edge/property mutation leaves no pending index work"
    );

    // Watermarks satisfied -> the barrier serves the read-your-writes result.
    let served = gql_query_with_consistency_as_admin(
        &env,
        "MATCH (n:Person) RETURN n",
        ReadMode::AtLeast(token.clone()),
    )
    .expect("satisfied AtLeast(token) is served");
    assert_eq!(
        served.row_count, 1,
        "AtLeast(token) observes the just-written Person vertex"
    );

    // Force one shard's label-stats watermark past the projection cursor -> retryable lag.
    let mut lagging = token.clone();
    lagging.shards = lagging
        .shards
        .iter()
        .map(|shard| MutationTokenShard {
            shard_id: shard.shard_id,
            label_stats_seq: Some(u64::MAX),
        })
        .collect();
    let err = gql_query_with_consistency_as_admin(
        &env,
        "MATCH (n:Person) RETURN n",
        ReadMode::AtLeast(lagging),
    )
    .expect_err("unmet watermark is a retryable projection lag");
    assert!(
        matches!(err, RouterError::ProjectionLag { .. }),
        "unmet watermark returns ProjectionLag, got {err:?}"
    );

    // Canonical is deferred (Phase 3) and explicitly rejected.
    let err =
        gql_query_with_consistency_as_admin(&env, "MATCH (n:Person) RETURN n", ReadMode::Canonical)
            .expect_err("Canonical read mode is deferred");
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "Canonical is rejected, got {err:?}"
    );

    // Eventual remains non-blocking and serves the same data.
    let eventual =
        gql_query_with_consistency_as_admin(&env, "MATCH (n:Person) RETURN n", ReadMode::Eventual)
            .expect("Eventual never blocks");
    assert_eq!(eventual.row_count, 1);

    // Phase 4 status contract for the completed saga.
    let status = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status for a known client_mutation_key");
    assert_eq!(status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(status.target_shard, None);
    assert_eq!(status.next_action, "none");
    assert!(status.last_error.is_none());

    // The autonomous recovery timer must not disturb a completed saga.
    run_router_recovery_timer(&env);

    let after = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status after a recovery tick");
    assert_eq!(
        after.phase,
        MutationLifecyclePhase::Completed,
        "recovery timer must not disturb a completed saga"
    );

    // An unknown key is rejected.
    let missing = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, "no-such-key");
    assert!(
        matches!(missing, Err(RouterError::InvalidArgument(_))),
        "unknown client_mutation_key returns InvalidArgument, got {missing:?}"
    );
}

#[test]
fn router_recovers_non_terminal_federated_saga_via_idempotent_retry() {
    // ADR 0029 Phase 4: a federated DML whose fan-out spans two shards, with one shard crashed
    // mid-saga, leaves the mutation non-terminal (`CanonicalPending`) instead of corrupting state.
    // The autonomous recovery timer never re-applies canonical DML, so while the shard is down the
    // saga stays pending (no double-apply). After the shard restarts, an idempotent retry on the
    // same `client_mutation_key` resumes only the outstanding shard (the already-committed shard is
    // deduplicated by `mutation_id`) and converges the saga to `Completed`.
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;

    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "router_recovers_non_terminal_federated_saga_via_idempotent_retry",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let dest = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);
    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest.global_vertex_id.shard_id, DEST_SHARD);

    let key = "router_recovers_non_terminal_federated_saga";

    // Crash the dest shard, then run a federated DML whose `age = 5` anchor fans out to both
    // shards. The dest write cannot commit, so the router returns an error and persists a
    // non-terminal saga (the dispatch envelope, with both shards' seeds, is recorded before any
    // shard executes).
    stop_graph_shard(&env, env.graph_dest);
    let err =
        gql_execute_idempotent_as_admin_expect_err(&env, "MATCH (n {age: 5}) SET n.age = 6", key);
    assert!(
        matches!(err, RouterError::InvalidArgument(_)),
        "a crashed shard fails the federated DML, got {err:?}"
    );

    let pending = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status for the in-flight saga");
    assert_eq!(
        pending.phase,
        MutationLifecyclePhase::CanonicalPending,
        "an outstanding canonical shard write leaves the saga CanonicalPending"
    );
    assert!(
        pending.target_shard.is_some(),
        "status names the outstanding shard"
    );
    assert!(
        pending
            .next_action
            .contains("retry the idempotent mutation"),
        "CanonicalPending asks the caller to retry, got {:?}",
        pending.next_action
    );

    // The autonomous recovery timer must not converge or corrupt the saga while the shard is down:
    // it never re-dispatches canonical DML, and the unavailable shard cannot be projected.
    run_router_recovery_timer(&env);
    let still_pending = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status after a recovery tick with the shard still down");
    assert_eq!(
        still_pending.phase,
        MutationLifecyclePhase::CanonicalPending,
        "recovery timer leaves an unavailable canonical shard pending (no double-apply)"
    );

    // Restart the shard and retry the same idempotent mutation. The already-committed shard is a
    // no-op (deduplicated by mutation_id); the recovered shard applies and the saga finalizes.
    start_graph_shard(&env, env.graph_dest);
    let resumed =
        gql_execute_idempotent_result_as_admin(&env, "MATCH (n {age: 5}) SET n.age = 6", key);
    assert_eq!(
        resumed.phase,
        Some(MutationLifecyclePhase::Completed),
        "the idempotent retry converges the saga to Completed"
    );
    drain_maintenance_via_timer(&env, env.graph_source);
    drain_maintenance_via_timer(&env, env.graph_dest);

    let completed = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status after convergence");
    assert_eq!(completed.phase, MutationLifecyclePhase::Completed);
    assert_eq!(completed.target_shard, None);
    assert_eq!(completed.next_action, "none");

    // The mutation landed on both shards: both vertices now read back at the new value.
    let after = gql_query_as_admin(&env, "MATCH (n {age: 6}) RETURN n");
    assert_eq!(
        after.row_count, 2,
        "both shards' vertices converged to the updated value"
    );
}

#[test]
fn router_recovery_timer_converges_projection_pending_saga_autonomously() {
    // ADR 0029 Phase 4: the autonomous recovery driver converges a projection-lagging federated
    // saga to `Completed` with no client in the loop. `ProjectionPending` (canonical durable on all
    // shards, projection advanced on some) is unreachable through the black-box DML path, which
    // advances every shard's projection inline before returning, so a test-only seam injects that
    // exact persisted state referencing a real prior mutation. The timer then advances the lagging
    // shard's projection and finalizes the record.
    use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;

    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "router_recovery_timer_converges_projection_pending_saga_autonomously",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let dest = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);
    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest.global_vertex_id.shard_id, DEST_SHARD);

    // A real federated DML commits a mutation on both shards and advances both projections inline.
    let real = gql_execute_idempotent_result_as_admin(
        &env,
        "MATCH (n {age: 5}) SET n.age = 6",
        "projection_pending_seed_mutation",
    );
    let mutation_id = real
        .token
        .expect("federated DML returns a mutation token")
        .mutation_id;

    // Inject a record for that mutation with the dest shard's projection left lagging.
    let key = "router_recovery_timer_converges_projection_pending_saga";
    test_inject_projection_pending_saga(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        key,
        mutation_id,
        1,
    );

    let pending = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status for the injected saga");
    assert_eq!(
        pending.phase,
        MutationLifecyclePhase::ProjectionPending,
        "canonical-complete with a lagging shard projection is ProjectionPending"
    );
    assert!(
        pending.next_action.contains("automatic"),
        "ProjectionPending recovery is automatic, got {:?}",
        pending.next_action
    );

    // The autonomous timer converges the saga without any client retry.
    run_router_recovery_timer(&env);

    let completed = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status after autonomous recovery");
    assert_eq!(
        completed.phase,
        MutationLifecyclePhase::Completed,
        "the recovery timer advances the lagging projection and finalizes the saga"
    );
    assert_eq!(completed.target_shard, None);
    assert_eq!(completed.next_action, "none");
}

/// Former `router_gql_insert_seeds_relationship_rows_for_knowledge_map_adapter`.
///
/// A GQL `INSERT` creates a single `KNOWS {weight: 5}` relationship, then a second query
/// proves the GQL-created row is observable through the relationship-row adapter projection.
/// Kept separate from the helper-seeded relationship test to preserve independent creation-path
/// diagnosability: both paths must produce valid source/edge/target bytes and the projected weight.
#[test]
fn single_shard_knowledge_map_relationship_rows_from_insert() {
    let env = install_single_shard_federation();

    let row_count = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 5}]->(:Project)",
        "single_shard_knowledge_map_relationship_rows_from_insert",
    );
    assert_eq!(row_count, 0);

    let result = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:KNOWS {weight: 5}]->(b) \
         RETURN ELEMENT_ID(a) AS source_id, ELEMENT_ID(e) AS edge_id, \
                ELEMENT_ID(b) AS target_id, e.weight AS edge_weight",
    );

    assert_eq!(result.row_count, 1);
    let row = decode_single_value_row(&result);
    let Value::Bytes(source_id) = row.get("source_id").expect("source_id column") else {
        panic!("expected source_id bytes, got {:?}", row.get("source_id"));
    };
    GraphPathVertexId::try_from_slice(source_id.as_ref()).expect("decode source ELEMENT_ID bytes");
    let Value::Bytes(target_id) = row.get("target_id").expect("target_id column") else {
        panic!("expected target_id bytes, got {:?}", row.get("target_id"));
    };
    GraphPathVertexId::try_from_slice(target_id.as_ref()).expect("decode target ELEMENT_ID bytes");
    let Value::Bytes(edge_id) = row.get("edge_id").expect("edge_id column") else {
        panic!("expected edge_id bytes, got {:?}", row.get("edge_id"));
    };
    GraphPathEdgeId::try_from_slice(edge_id.as_ref()).expect("decode edge ELEMENT_ID bytes");
    assert_eq!(row.get("edge_weight"), Some(&Value::Int64(5)));
}

fn decode_single_value_row(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
) -> std::collections::BTreeMap<String, Value> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    assert_eq!(wire.rows.len(), 1);
    wire.rows
        .into_iter()
        .next()
        .expect("one row")
        .try_into_value_row()
        .expect("wire row to value row")
}

fn vertex_id_column(
    row: &std::collections::BTreeMap<String, Value>,
    column: &str,
    encoding_key: &ElementIdEncodingKey,
) -> GlobalVertexId {
    let Value::Bytes(id_bytes) = row.get(column).unwrap_or_else(|| panic!("{column} column"))
    else {
        panic!("expected {column} bytes, got {:?}", row.get(column));
    };
    GraphPathVertexId::try_from_slice(id_bytes.as_ref())
        .expect("decode vertex id")
        .decode_global(encoding_key)
}

/// Decode every row of a multi-row `ELEMENT_ID(n)` result into `GlobalVertexId`s.
fn vertex_ids_from_result(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
    column: &str,
    encoding_key: &ElementIdEncodingKey,
) -> Vec<GlobalVertexId> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    wire.rows
        .into_iter()
        .map(|row| {
            let value_row = row.try_into_value_row().expect("wire row to value row");
            vertex_id_column(&value_row, column, encoding_key)
        })
        .collect()
}

/// Decode the bound endpoint `ELEMENT_ID(b)` column from every row of an edge-index result.
fn edge_bound_endpoint_ids_from_result(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
    encoding_key: &ElementIdEncodingKey,
) -> Vec<GlobalVertexId> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("router gql_query should return rows_blob");
    let wire = IcWirePlanQueryResult::decode_blob(rows_blob).expect("decode rows_blob");
    wire.rows
        .into_iter()
        .map(|row| {
            let value_row = row.try_into_value_row().expect("wire row to value row");
            vertex_id_column(&value_row, "b_id", encoding_key)
        })
        .collect()
}

/// Assert an edge-index result spans both shards by inspecting the decoded bound endpoints.
fn assert_edge_result_spans_both_shards(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
    encoding_key: &ElementIdEncodingKey,
    context: &str,
) {
    let endpoint_ids = edge_bound_endpoint_ids_from_result(result, encoding_key);
    assert_eq!(
        endpoint_ids.len(),
        2,
        "{context}: expected exactly two rows, one per shard"
    );
    let shard_ids: std::collections::BTreeSet<_> =
        endpoint_ids.iter().map(|id| id.shard_id).collect();
    assert!(
        shard_ids.contains(&SOURCE_SHARD) && shard_ids.contains(&DEST_SHARD),
        "{context}: expected bound endpoints from both shards, got {shard_ids:?}"
    );
}

/// Consolidated federated vertex-index lifecycle for the four former contracts:
///
/// 1. `federated_gql_query_index_seeded_routes_to_hit_shard_only`
/// 2. `federated_gql_query_index_intersection_merges_matching_shards`
/// 3. `federated_gql_query_index_seeded_merges_across_shards`
/// 4. `federated_drop_index_property_eq_loses_federated_anchor`
///
/// One fresh multi-shard federation, unique lifecycle index names/values, and DROP last.
#[test]
fn federated_vertex_index_lifecycle() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    let score = admin_intern_property(&env, "score");

    create_vertex_property_index(
        &env,
        FEDERATED_VERTEX_LIFECYCLE_AGE,
        FEDERATED_VERTEX_LIFECYCLE_LABEL,
        "age",
        "federated_vertex_index_lifecycle_create_age",
    );
    create_vertex_property_index(
        &env,
        FEDERATED_VERTEX_LIFECYCLE_SCORE,
        FEDERATED_VERTEX_LIFECYCLE_LABEL,
        "score",
        "federated_vertex_index_lifecycle_create_score",
    );

    // Hit-shard-only routing: a value present on only one shard routes there.
    let hit_only = e2e_insert_vertex_with_property(
        &env,
        env.graph_source,
        age.raw(),
        FEDERATED_VERTEX_LIFECYCLE_HIT_AGE,
    );
    assert_eq!(hit_only.global_vertex_id.shard_id, SOURCE_SHARD);

    // Cross-shard merge: equal values on both shards fan out and merge both rows.
    let source_merge = e2e_insert_vertex_with_property(
        &env,
        env.graph_source,
        age.raw(),
        FEDERATED_VERTEX_LIFECYCLE_MERGE_AGE,
    );
    let dest_merge = e2e_insert_vertex_with_property(
        &env,
        env.graph_dest,
        age.raw(),
        FEDERATED_VERTEX_LIFECYCLE_MERGE_AGE,
    );
    assert_eq!(source_merge.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest_merge.global_vertex_id.shard_id, DEST_SHARD);

    // Intersection: full matches on both shards merge; partial match on dest is sieved out.
    let _ = e2e_insert_vertex_with_two_properties(
        &env,
        env.graph_source,
        age.raw(),
        FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_AGE,
        score.raw(),
        FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_SCORE,
    );
    let _ = e2e_insert_vertex_with_two_properties(
        &env,
        env.graph_dest,
        age.raw(),
        FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_AGE,
        score.raw(),
        FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_SCORE,
    );
    let _ = e2e_insert_vertex_with_property(
        &env,
        env.graph_dest,
        age.raw(),
        FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_AGE,
    );

    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );

    let hit_result = gql_query_as_admin(
        &env,
        &format!(
            "MATCH (n {{age: {}}}) RETURN ELEMENT_ID(n) AS id",
            FEDERATED_VERTEX_LIFECYCLE_HIT_AGE
        ),
    );
    assert_eq!(
        hit_result.row_count, 1,
        "unique age value routes to exactly one row"
    );
    let hit_ids = vertex_ids_from_result(&hit_result, "id", &encoding_key);
    assert_eq!(
        hit_ids[0].shard_id, SOURCE_SHARD,
        "hit-shard-only value landed on source shard"
    );

    let merge_result = gql_query_as_admin(
        &env,
        &format!(
            "MATCH (n {{age: {}}}) RETURN ELEMENT_ID(n) AS id",
            FEDERATED_VERTEX_LIFECYCLE_MERGE_AGE
        ),
    );
    assert_eq!(
        merge_result.row_count, 2,
        "equal age value merges rows from both shards"
    );
    let merge_shard_ids: std::collections::BTreeSet<_> =
        vertex_ids_from_result(&merge_result, "id", &encoding_key)
            .iter()
            .map(|id| id.shard_id)
            .collect();
    assert!(
        merge_shard_ids.contains(&SOURCE_SHARD) && merge_shard_ids.contains(&DEST_SHARD),
        "merge result must include both shards, got {merge_shard_ids:?}"
    );

    let intersection_result = gql_query_as_admin(
        &env,
        &format!(
            "MATCH (n {{age: {}, score: {}}}) RETURN ELEMENT_ID(n) AS id",
            FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_AGE,
            FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_SCORE
        ),
    );
    assert_eq!(
        intersection_result.row_count, 2,
        "intersection merges only full matches across both shards"
    );
    let intersection_shard_ids: std::collections::BTreeSet<_> =
        vertex_ids_from_result(&intersection_result, "id", &encoding_key)
            .iter()
            .map(|id| id.shard_id)
            .collect();
    assert!(
        intersection_shard_ids.contains(&SOURCE_SHARD)
            && intersection_shard_ids.contains(&DEST_SHARD),
        "intersection result must include both shards, got {intersection_shard_ids:?}"
    );

    // Sieve check: the partial dest match is returned by a single-property lookup but excluded
    // from the intersection.
    let sieve_result = gql_query_as_admin(
        &env,
        &format!(
            "MATCH (n {{age: {}}}) RETURN n",
            FEDERATED_VERTEX_LIFECYCLE_INTERSECTION_AGE
        ),
    );
    assert_eq!(
        sieve_result.row_count, 3,
        "age-only lookup must include the partial match on the destination shard"
    );

    // DROP occurs last: removing the age index removes the only federated anchor for age
    // equality, so the federated merge query fails closed with an explicit no-index-anchor error.
    drop_vertex_property_index(
        &env,
        FEDERATED_VERTEX_LIFECYCLE_AGE,
        true,
        "federated_vertex_index_lifecycle_drop_age",
    );

    let err = gql_query_as_admin_expect_err(
        &env,
        &format!(
            "MATCH (n {{age: {}}}) RETURN n",
            FEDERATED_VERTEX_LIFECYCLE_MERGE_AGE
        ),
    );
    assert!(
        err.to_string().contains("no index anchor"),
        "expected federated dispatch without index anchor to fail, got: {err:?}"
    );
}

/// Consolidated lifecycle for the two former generic directed edge-index contracts:
///
/// 1. `standalone_gql_query_edge_index_seeded_property_eq`
/// 2. `standalone_drop_edge_index_property_eq_still_queries_via_scan` (generic edge index half)
///
/// Uses a unique edge label so the fixture cannot be contaminated by other lifecycle tests.
#[test]
fn single_shard_generic_edge_index_lifecycle() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let label = admin_intern_edge_label(&env, LIFECYCLE_EDGE_LABEL_GENERIC);

    create_edge_property_index(
        &env,
        LIFECYCLE_EDGE_WEIGHT_NAME,
        LIFECYCLE_EDGE_LABEL_GENERIC,
        "weight",
        "lifecycle_generic_create",
    );

    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        label.raw(),
        weight.raw(),
        5,
    );

    let indexed = gql_query_as_admin(
        &env,
        "MATCH ()-[e:LifecycleKnowsGeneric {weight: 5}]->(b) RETURN e, b",
    );
    assert_eq!(
        indexed.row_count, 1,
        "generic edge index must seed directed equality lookup"
    );

    drop_vertex_property_index(
        &env,
        LIFECYCLE_EDGE_WEIGHT_NAME,
        true,
        "lifecycle_generic_drop",
    );

    let after_drop = gql_query_as_admin(
        &env,
        "MATCH ()-[e:LifecycleKnowsGeneric {weight: 5}]->(b) RETURN e, b",
    );
    assert_eq!(
        after_drop.row_count, 1,
        "scan fallback must still answer after DROP INDEX"
    );
}

/// Consolidated lifecycle for the former pointing-right directed edge-index contract
/// `standalone_gql_query_edge_index_pointing_right_ddl`, with new strengthened DROP coverage.
/// The post-DROP scan fallback assertion is not inherited from a former test; it was added here
/// to verify that dropping a pointing-right edge index leaves canonical edge data queryable.
///
/// Uses a unique edge label so the fixture cannot be contaminated by other lifecycle tests.
#[test]
fn single_shard_pointing_right_edge_index_lifecycle() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let label = admin_intern_edge_label(&env, LIFECYCLE_EDGE_LABEL_RIGHT);

    create_directed_edge_property_index(
        &env,
        LIFECYCLE_EDGE_WEIGHT_RIGHT_NAME,
        LIFECYCLE_EDGE_LABEL_RIGHT,
        "weight",
        "lifecycle_right_create",
    );

    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        label.raw(),
        weight.raw(),
        5,
    );

    let indexed = gql_query_as_admin(
        &env,
        "MATCH ()-[e:LifecycleKnowsRight {weight: 5}]->(b) RETURN e, b",
    );
    assert_eq!(
        indexed.row_count, 1,
        "pointing-right edge index must seed directed equality lookup"
    );

    drop_vertex_property_index(
        &env,
        LIFECYCLE_EDGE_WEIGHT_RIGHT_NAME,
        true,
        "lifecycle_right_drop",
    );

    let after_drop = gql_query_as_admin(
        &env,
        "MATCH ()-[e:LifecycleKnowsRight {weight: 5}]->(b) RETURN e, b",
    );
    assert_eq!(
        after_drop.row_count, 1,
        "scan fallback must still answer after DROP INDEX"
    );
}

/// Consolidated lifecycle for the three former undirected edge-index contracts:
///
/// 1. `standalone_gql_query_edge_index_undirected_ddl`
/// 2. `standalone_gql_query_undirected_index_does_not_seed_directed_edge`
/// 3. `standalone_gql_query_undirected_symmetric_anonymous_endpoints`
///
/// The post-DROP scan fallback assertion is new strengthened coverage; the former standalone
/// DROP test only covered a generic (directed) edge index. Lifecycle ordering isolates the
/// no-index symmetric expansion, the indexed undirected expansion, the directed-insert subset
/// exclusion, and the post-DROP scan fallback, all with a unique edge label.
#[test]
fn single_shard_undirected_edge_index_lifecycle() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let label = admin_intern_edge_label(&env, LIFECYCLE_EDGE_LABEL_UNDIR);

    let a = e2e_insert_vertex(&env, env.graph_source);
    let b = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        a.local_vertex_id,
        b.local_vertex_id,
        label.raw(),
        weight.raw(),
        5,
    );

    // Phase 1: anonymous endpoints on both sides expand the edge once per endpoint,
    // and the standalone scan path does not require a leading edge-index anchor.
    let anonymous = gql_query_as_admin(
        &env,
        "MATCH ()~[e:LifecycleKnowsUndir]~() WHERE e.weight = 5 RETURN e",
    );
    assert_eq!(
        anonymous.row_count, 2,
        "anonymous undirected expansion must return one row per endpoint without an index anchor"
    );

    // Phase 2: undirected-only index seeds undirected equality queries.
    create_undirected_edge_property_index(
        &env,
        LIFECYCLE_EDGE_WEIGHT_UNDIR_NAME,
        LIFECYCLE_EDGE_LABEL_UNDIR,
        "weight",
        "lifecycle_undir_create",
    );

    let seeded = gql_query_as_admin(
        &env,
        "MATCH ()~[e:LifecycleKnowsUndir {weight: 5}]~() RETURN e",
    );
    assert_eq!(
        seeded.row_count, 1,
        "undirected-only index must seed undirected equality lookup"
    );

    // Phase 3: directed inserts must not seed an undirected-only index (ADR 0012 subset rule).
    let c = e2e_insert_vertex(&env, env.graph_source);
    let d = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        c.local_vertex_id,
        d.local_vertex_id,
        label.raw(),
        weight.raw(),
        6,
    );

    let undirected_after_directed = gql_query_as_admin(
        &env,
        "MATCH ()~[e:LifecycleKnowsUndir {weight: 6}]~() RETURN e",
    );
    assert_eq!(
        undirected_after_directed.row_count, 0,
        "directed insert must not seed an undirected-only edge index"
    );

    // Phase 4: dropping the index removes only derived routing state; canonical data
    // remains queryable by the standalone scan path.
    drop_vertex_property_index(
        &env,
        LIFECYCLE_EDGE_WEIGHT_UNDIR_NAME,
        true,
        "lifecycle_undir_drop",
    );

    let after_drop = gql_query_as_admin(
        &env,
        "MATCH ()~[e:LifecycleKnowsUndir {weight: 5}]~() RETURN e",
    );
    assert_eq!(
        after_drop.row_count, 2,
        "scan fallback must still answer after DROP INDEX (one row per anonymous endpoint)"
    );
}

/// Consolidated federated edge-index lifecycle for the three former contracts:
///
/// 1. `federated_gql_query_edge_index_undirected_ddl`
/// 2. `federated_gql_query_edge_index_pointing_right_ddl`
/// 3. `federated_drop_edge_index_property_eq_loses_federated_anchor`
///
/// One fresh multi-shard federation, separate labels/directions/values per contract,
/// all indexed assertions before DROP, and DROP last.
#[test]
fn federated_edge_index_lifecycle() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows_undir = admin_intern_edge_label(&env, FEDERATED_EDGE_LIFECYCLE_LABEL_UNDIR);
    let knows_right = admin_intern_edge_label(&env, FEDERATED_EDGE_LIFECYCLE_LABEL_RIGHT);
    let knows_drop = admin_intern_edge_label(&env, FEDERATED_EDGE_LIFECYCLE_LABEL_DROP);

    create_undirected_edge_property_index(
        &env,
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_UNDIR,
        FEDERATED_EDGE_LIFECYCLE_LABEL_UNDIR,
        "weight",
        "federated_edge_index_lifecycle_undir_create",
    );
    create_directed_edge_property_index(
        &env,
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_RIGHT,
        FEDERATED_EDGE_LIFECYCLE_LABEL_RIGHT,
        "weight",
        "federated_edge_index_lifecycle_right_create",
    );
    create_edge_property_index(
        &env,
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_DROP,
        FEDERATED_EDGE_LIFECYCLE_LABEL_DROP,
        "weight",
        "federated_edge_index_lifecycle_drop_create",
    );

    // Undirected DDL: one undirected edge per shard, indexed undirected expansion.
    let undir_source_a = e2e_insert_vertex(&env, env.graph_source);
    let undir_target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        undir_source_a.local_vertex_id,
        undir_target_a.local_vertex_id,
        knows_undir.raw(),
        weight.raw(),
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_UNDIR,
    );
    let undir_source_b = e2e_insert_vertex(&env, env.graph_dest);
    let undir_target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_dest,
        undir_source_b.local_vertex_id,
        undir_target_b.local_vertex_id,
        knows_undir.raw(),
        weight.raw(),
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_UNDIR,
    );

    // Pointing-right DDL: one directed edge per shard, indexed directed expansion.
    let right_source_a = e2e_insert_vertex(&env, env.graph_source);
    let right_target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        right_source_a.local_vertex_id,
        right_target_a.local_vertex_id,
        knows_right.raw(),
        weight.raw(),
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_RIGHT,
    );
    let right_source_b = e2e_insert_vertex(&env, env.graph_dest);
    let right_target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_dest,
        right_source_b.local_vertex_id,
        right_target_b.local_vertex_id,
        knows_right.raw(),
        weight.raw(),
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_RIGHT,
    );

    // Drop-anchor DDL: one directed edge per shard, indexed directed expansion, then drop.
    let drop_source_a = e2e_insert_vertex(&env, env.graph_source);
    let drop_target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        drop_source_a.local_vertex_id,
        drop_target_a.local_vertex_id,
        knows_drop.raw(),
        weight.raw(),
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_DROP,
    );
    let drop_source_b = e2e_insert_vertex(&env, env.graph_dest);
    let drop_target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_dest,
        drop_source_b.local_vertex_id,
        drop_target_b.local_vertex_id,
        knows_drop.raw(),
        weight.raw(),
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_DROP,
    );

    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );

    // Indexed assertions before any DROP: each query returns one row per shard, and the
    // decoded bound endpoint `b` must originate from both shards.
    let undir_query = format!(
        "MATCH ()~[e:{FEDERATED_EDGE_LIFECYCLE_LABEL_UNDIR} {{weight: {FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_UNDIR}}}]~(b) RETURN e, ELEMENT_ID(b) AS b_id"
    );
    let undir_result = gql_query_as_admin(&env, &undir_query);
    assert_eq!(
        undir_result.row_count, 2,
        "undirected DDL must return one indexed row per edge across shards"
    );
    assert_edge_result_spans_both_shards(&undir_result, &encoding_key, "undirected DDL");

    let right_query = format!(
        "MATCH ()-[e:{FEDERATED_EDGE_LIFECYCLE_LABEL_RIGHT} {{weight: {FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_RIGHT}}}]->(b) RETURN e, ELEMENT_ID(b) AS b_id"
    );
    let right_result = gql_query_as_admin(&env, &right_query);
    assert_eq!(
        right_result.row_count, 2,
        "pointing-right DDL must return one indexed row per edge across shards"
    );
    assert_edge_result_spans_both_shards(&right_result, &encoding_key, "pointing-right DDL");

    let drop_query = format!(
        "MATCH ()-[e:{FEDERATED_EDGE_LIFECYCLE_LABEL_DROP} {{weight: {FEDERATED_EDGE_LIFECYCLE_WEIGHT_VALUE_DROP}}}]->(b) RETURN e, ELEMENT_ID(b) AS b_id"
    );
    let drop_indexed = gql_query_as_admin(&env, &drop_query);
    assert_eq!(
        drop_indexed.row_count, 2,
        "generic directed edge DDL must return one indexed row per edge across shards before DROP"
    );
    assert_edge_result_spans_both_shards(
        &drop_indexed,
        &encoding_key,
        "generic directed edge DDL before DROP",
    );

    // DROP occurs last: the generic directed edge index is the only anchor for the drop query.
    drop_vertex_property_index(
        &env,
        FEDERATED_EDGE_LIFECYCLE_WEIGHT_DROP,
        true,
        "federated_edge_index_lifecycle_drop",
    );

    let err = gql_query_as_admin_expect_err(&env, &drop_query);
    assert!(
        err.to_string().contains("no index anchor"),
        "expected federated edge dispatch without index anchor to fail, got: {err:?}"
    );
}
