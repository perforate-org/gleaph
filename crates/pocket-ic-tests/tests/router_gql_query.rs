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
    drop_vertex_property_index, e2e_insert_directed_edge_with_property,
    e2e_insert_undirected_edge_with_property, e2e_insert_vertex, e2e_insert_vertex_with_property,
    e2e_insert_vertex_with_two_properties, gql_execute_idempotent_as_admin,
    gql_execute_idempotent_as_admin_expect_err, gql_execute_idempotent_result_as_admin,
    gql_query_as_admin, gql_query_as_admin_expect_err, gql_query_with_consistency_as_admin,
    graph_index_pending_min_mutation_id, install_federation, install_single_shard_federation,
    knowledge_map_live_query, mutation_status_as_admin, run_router_recovery_timer,
    seed_knowledge_map_graph, start_graph_shard, stop_graph_shard,
    test_inject_projection_pending_saga,
};

const INDEX_VERTEX_LABEL: &str = "Person";
const INDEX_AGE_NAME: &str = "pocket_ic_vertex_age";
const INDEX_SCORE_NAME: &str = "pocket_ic_vertex_score";
const INDEX_EDGE_LABEL: &str = "KNOWS";
const INDEX_WEIGHT_NAME: &str = "pocket_ic_edge_weight";
const INDEX_WEIGHT_RIGHT_NAME: &str = "pocket_ic_edge_weight_right";
const INDEX_WEIGHT_UNDIR_NAME: &str = "pocket_ic_edge_weight_undir";
const EDGE_WEIGHT_QUERY: &str = "MATCH ()-[e:KNOWS {weight: 5}]->(b) RETURN e, b";
const EDGE_WEIGHT_UNDIR_QUERY: &str = "MATCH ()~[e:KNOWS {weight: 5}]~() RETURN e";
const EDGE_WEIGHT_UNDIR_BOUND_QUERY: &str = "MATCH ()~[e:KNOWS {weight: 5}]~(b) RETURN e, b";

#[test]
fn router_gql_query_node_scan_on_single_shard() {
    let env = install_single_shard_federation();
    let _ = e2e_insert_vertex(&env, env.graph_source);

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN n");

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_e2e_insert_assigns_global_id() {
    let env = install_single_shard_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);

    assert_eq!(inserted.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(
        inserted.global_vertex_id.local_vertex_id,
        inserted.local_vertex_id
    );

    let same_id = GlobalVertexId::new(SOURCE_SHARD, inserted.local_vertex_id);
    assert_eq!(inserted.global_vertex_id, same_id);
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

#[test]
fn standalone_gql_query_returns_element_id_bytes() {
    let env = install_single_shard_federation();
    let inserted = e2e_insert_vertex(&env, env.graph_source);
    let encoding_key = gleaph_pocket_ic_tests::graph_element_id_encoding_key(
        &env.pic,
        env.admin,
        env.router,
        gleaph_pocket_ic_tests::GRAPH_NAME,
    );

    let result = gql_query_as_admin(&env, "MATCH (n) RETURN ELEMENT_ID(n) AS id");

    assert_eq!(result.row_count, 1);
    let id_bytes = gleaph_pocket_ic_tests::element_id_bytes_from_gql_result(&result, "id");
    assert_eq!(
        GraphPathVertexId::try_from_slice(id_bytes.as_ref())
            .expect("decode vertex id")
            .decode_global(&encoding_key),
        inserted.global_vertex_id
    );
}

#[test]
fn standalone_gql_query_returns_relationship_rows_for_knowledge_map_adapter() {
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

#[test]
fn router_gql_insert_seeds_knowledge_map_fan_out_graph() {
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

#[test]
fn router_places_completely_new_single_insert_on_latest_shard() {
    // ADR 0029 §6 (Phase 5 contract 1): a completely-new (unanchored) INSERT on a federated graph
    // is placed on the graph's latest shard (greatest graph-local shard id = DEST_SHARD here),
    // instead of being rejected with `no index anchor`. The mutation token names exactly that
    // single shard.
    let env = install_federation();

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person)",
        "router_places_completely_new_single_insert_on_latest_shard",
    );
    let token = result
        .token
        .expect("a completely-new federated INSERT issues a mutation token");
    assert_eq!(
        token.shards.len(),
        1,
        "a pure insert is placed on exactly one shard"
    );
    assert_eq!(
        token.shards[0].shard_id, DEST_SHARD,
        "the pure insert lands on the graph's latest shard"
    );
}

#[test]
fn router_places_completely_new_insert_bundle_on_latest_shard() {
    // ADR 0029 §6 (Phase 5 contract 1): a completely-new INSERT-only *bundle* (multiple top-level
    // DML statements) on a federated graph is co-placed on the latest shard and executed there
    // atomically in one canonical segment, so the federated multi-DML gate does not reject it. The
    // token names exactly the one (latest) shard.
    let env = install_federation();

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person) NEXT INSERT (:Project)",
        "router_places_completely_new_insert_bundle_on_latest_shard",
    );
    let token = result
        .token
        .expect("a completely-new federated INSERT bundle issues a mutation token");
    assert_eq!(
        token.shards.len(),
        1,
        "the whole bundle is co-placed on one shard"
    );
    assert_eq!(
        token.shards[0].shard_id, DEST_SHARD,
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

    let updated = gql_query_as_admin(&env, "MATCH (n {age: 6}) RETURN n");
    assert_eq!(
        updated.row_count, 2,
        "both shards' anchor vertices converged to the updated value"
    );
}

#[test]
fn router_idempotent_dml_issues_mutation_token_and_exposes_index_watermark() {
    // ADR 0029 Phase 2: an idempotent DML returns a read-your-writes token (mutation id +
    // per-shard label-stats watermarks) and a lifecycle phase. After a successful insert the
    // index postings are applied inline, so the shard's index watermark is clear (`None`).
    let env = install_single_shard_federation();

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 5}]->(:Project)",
        "router_idempotent_dml_issues_mutation_token_and_exposes_index_watermark",
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

    // Happy-path flush applies postings inline; no tracked mutation is left pending.
    assert_eq!(
        graph_index_pending_min_mutation_id(&env, env.graph_source),
        None,
        "a successfully flushed mutation leaves no pending index work"
    );
}

#[test]
fn router_atleast_read_barrier_serves_when_satisfied_and_lags_when_unmet() {
    // ADR 0029 Phase 3: `gql_query_with_consistency(AtLeast(token))` enforces the
    // read-your-writes barrier. After a successful idempotent DML the projections are
    // caught up, so the real token is *served* (read-your-writes); a token whose label-stats
    // watermark is forced beyond the projection cursor returns a retryable `ProjectionLag`
    // without serving stale state; `Canonical` is deferred and rejected.
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::{MutationTokenShard, ReadMode};

    let env = install_single_shard_federation();

    let result = gql_execute_idempotent_result_as_admin(
        &env,
        "INSERT (:Person)",
        "router_atleast_read_barrier_serves_when_satisfied_and_lags_when_unmet",
    );
    let token = result.token.expect("idempotent DML issues a token");

    // Watermarks satisfied → the barrier serves the read-your-writes result.
    let served = gql_query_with_consistency_as_admin(
        &env,
        "MATCH (n:Person) RETURN n",
        ReadMode::AtLeast(token.clone()),
    )
    .expect("satisfied AtLeast(token) is served");
    assert_eq!(
        served.row_count, 1,
        "AtLeast(token) observes the just-written vertex"
    );

    // Force one shard's label-stats watermark past the projection cursor → retryable lag.
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
}

#[test]
fn router_mutation_status_reports_completed_and_recovery_timer_is_safe_noop() {
    // ADR 0029 Phase 4: a successful idempotent DML finalizes its saga inline, so
    // `mutation_status` reports `Completed` with no outstanding shard or required action. The
    // autonomous recovery timer (armed after every idempotent DML) must find no recoverable
    // work and leave the terminal saga untouched.
    use gleaph_graph_kernel::federation::RouterError;
    use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;

    let env = install_single_shard_federation();
    let key = "router_mutation_status_completed";

    let result = gql_execute_idempotent_result_as_admin(&env, "INSERT (:Person)", key);
    assert!(result.token.is_some(), "idempotent DML issues a token");

    let status = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status for a known client_mutation_key");
    assert_eq!(status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(status.target_shard, None);
    assert_eq!(status.next_action, "none");
    assert!(status.last_error.is_none());

    run_router_recovery_timer(&env);

    let after = mutation_status_as_admin(&env, gleaph_pocket_ic_tests::GRAPH_NAME, key)
        .expect("status after a recovery tick");
    assert_eq!(
        after.phase,
        MutationLifecyclePhase::Completed,
        "recovery timer must not disturb a completed saga"
    );

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

#[test]
fn router_gql_insert_seeds_relationship_rows_for_knowledge_map_adapter() {
    let env = install_single_shard_federation();

    let row_count = gql_execute_idempotent_as_admin(
        &env,
        "INSERT (:Person)-[:KNOWS {weight: 5}]->(:Project)",
        "router_gql_insert_seeds_relationship_rows_for_knowledge_map_adapter",
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

#[test]
fn federated_gql_query_index_seeded_routes_to_hit_shard_only() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_gql_query_index_seeded_routes_to_hit_shard_only",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let _ = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 9);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");

    assert_eq!(result.row_count, 1);
}

#[test]
fn federated_gql_query_index_intersection_merges_matching_shards() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    let score = admin_intern_property(&env, "score");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_intersection_age",
    );
    create_vertex_property_index(
        &env,
        INDEX_SCORE_NAME,
        INDEX_VERTEX_LABEL,
        "score",
        "federated_intersection_score",
    );
    // Full match on each shard.
    let _ =
        e2e_insert_vertex_with_two_properties(&env, env.graph_source, age.raw(), 5, score.raw(), 9);
    let _ =
        e2e_insert_vertex_with_two_properties(&env, env.graph_dest, age.raw(), 5, score.raw(), 9);
    // Partial match (age only) on dest — must be excluded.
    let _ = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5, score: 9}) RETURN n");

    assert_eq!(
        result.row_count, 2,
        "streamed intersection should merge full matches across both shards"
    );
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

#[test]
fn federated_gql_query_index_seeded_merges_across_shards() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_gql_query_index_seeded_merges_across_shards",
    );
    let source = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let dest = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);

    assert_eq!(source.global_vertex_id.shard_id, SOURCE_SHARD);
    assert_eq!(dest.global_vertex_id.shard_id, DEST_SHARD);

    let result = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");

    assert_eq!(result.row_count, 2);
}

#[test]
fn federated_drop_index_property_eq_loses_federated_anchor() {
    let env = install_federation();
    let age = admin_intern_property(&env, "age");
    create_vertex_property_index(
        &env,
        INDEX_AGE_NAME,
        INDEX_VERTEX_LABEL,
        "age",
        "federated_drop_index_create",
    );
    let _ = e2e_insert_vertex_with_property(&env, env.graph_source, age.raw(), 5);
    let _ = e2e_insert_vertex_with_property(&env, env.graph_dest, age.raw(), 5);

    let indexed = gql_query_as_admin(&env, "MATCH (n {age: 5}) RETURN n");
    assert_eq!(indexed.row_count, 2);

    drop_vertex_property_index(&env, INDEX_AGE_NAME, true, "federated_drop_index_drop");

    let err = gql_query_as_admin_expect_err(&env, "MATCH (n {age: 5}) RETURN n");
    assert!(
        err.to_string().contains("no index anchor"),
        "expected federated dispatch without index anchor to fail, got: {err:?}"
    );
}

#[test]
fn standalone_gql_query_edge_index_seeded_property_eq() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_edge_index_seeded_property_eq",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_gql_query_edge_index_pointing_right_ddl() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_directed_edge_property_index(
        &env,
        INDEX_WEIGHT_RIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_edge_index_pointing_right_ddl",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);

    assert_eq!(result.row_count, 1);
}

#[test]
fn standalone_gql_query_edge_index_undirected_ddl() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_undirected_edge_property_index(
        &env,
        INDEX_WEIGHT_UNDIR_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_edge_index_undirected_ddl",
    );
    let v = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        v.local_vertex_id,
        v.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_UNDIR_QUERY);

    assert_eq!(result.row_count, 1);
}

/// Undirected-only index maintains undirected wire postings; directed inserts must not seed
/// an undirected leading `EdgeIndexScan` (ADR 0012 subset rule).
#[test]
fn standalone_gql_query_undirected_index_does_not_seed_directed_edge() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_undirected_edge_property_index(
        &env,
        INDEX_WEIGHT_UNDIR_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_gql_query_undirected_index_does_not_seed_directed_edge",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_UNDIR_BOUND_QUERY);

    assert_eq!(result.row_count, 0);
}

/// Anonymous endpoints on both sides of an undirected edge match once per endpoint
/// when the planner expands from each vertex (no leading edge index anchor).
#[test]
fn standalone_gql_query_undirected_symmetric_anonymous_endpoints() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, "MATCH ()~[e:KNOWS]~() WHERE e.weight = 5 RETURN e");

    assert_eq!(result.row_count, 2);
}

#[test]
fn federated_gql_query_edge_index_undirected_ddl() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_undirected_edge_property_index(
        &env,
        INDEX_WEIGHT_UNDIR_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "federated_gql_query_edge_index_undirected_ddl",
    );
    let source_a = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_source,
        source_a.local_vertex_id,
        target_a.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );
    let source_b = e2e_insert_vertex(&env, env.graph_dest);
    let target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_undirected_edge_with_property(
        &env,
        env.graph_dest,
        source_b.local_vertex_id,
        target_b.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_UNDIR_BOUND_QUERY);

    assert_eq!(result.row_count, 2);
}

#[test]
fn federated_gql_query_edge_index_pointing_right_ddl() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_directed_edge_property_index(
        &env,
        INDEX_WEIGHT_RIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "federated_gql_query_edge_index_pointing_right_ddl",
    );
    let source_a = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source_a.local_vertex_id,
        target_a.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );
    let source_b = e2e_insert_vertex(&env, env.graph_dest);
    let target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_dest,
        source_b.local_vertex_id,
        target_b.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let result = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);

    assert_eq!(result.row_count, 2);
}

#[test]
fn standalone_drop_edge_index_property_eq_still_queries_via_scan() {
    let env = install_single_shard_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "standalone_drop_edge_index_create",
    );
    let source = e2e_insert_vertex(&env, env.graph_source);
    let target = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source.local_vertex_id,
        target.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let indexed = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);
    assert_eq!(indexed.row_count, 1);

    drop_vertex_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        true,
        "standalone_drop_edge_index_drop",
    );

    let all_edges = gql_query_as_admin(&env, "MATCH ()-[e:KNOWS]->(b) RETURN e, b");
    assert_eq!(
        all_edges.row_count, 1,
        "edge should still exist after DROP INDEX"
    );

    let after_drop = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);
    assert_eq!(
        after_drop.row_count, 1,
        "single-shard scan path should still match after DROP INDEX"
    );
}

#[test]
fn federated_drop_edge_index_property_eq_loses_federated_anchor() {
    let env = install_federation();
    let weight = admin_intern_property(&env, "weight");
    let knows = admin_intern_edge_label(&env, INDEX_EDGE_LABEL);
    create_edge_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        INDEX_EDGE_LABEL,
        "weight",
        "federated_drop_edge_index_create",
    );
    let source_a = e2e_insert_vertex(&env, env.graph_source);
    let target_a = e2e_insert_vertex(&env, env.graph_source);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_source,
        source_a.local_vertex_id,
        target_a.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );
    let source_b = e2e_insert_vertex(&env, env.graph_dest);
    let target_b = e2e_insert_vertex(&env, env.graph_dest);
    e2e_insert_directed_edge_with_property(
        &env,
        env.graph_dest,
        source_b.local_vertex_id,
        target_b.local_vertex_id,
        knows.raw(),
        weight.raw(),
        5,
    );

    let indexed = gql_query_as_admin(&env, EDGE_WEIGHT_QUERY);
    assert_eq!(indexed.row_count, 2);

    drop_vertex_property_index(
        &env,
        INDEX_WEIGHT_NAME,
        true,
        "federated_drop_edge_index_drop",
    );

    let err = gql_query_as_admin_expect_err(&env, EDGE_WEIGHT_QUERY);
    assert!(
        err.to_string().contains("no index anchor"),
        "expected federated dispatch without index anchor to fail, got: {err:?}"
    );
}
