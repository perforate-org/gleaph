//! PocketIC coverage for ADR 0034 Slice 25: ordinary read access to fixed-size inline edge STRUCTs.
//!
//! Router-resolved schema identifies the named inline STRUCT slot; Graph decodes the edge inline property bytes
//! into a GQL record so `e.stats.field` works in projection, filtering, aggregate input, and
//! ordering. The inline slot is the only read source for its `(label, property)` pair; a sidecar
//! value cannot override it.
//!
//! All scenarios run inside one fresh PocketIC fixture. Vertices carry the declared `User`
//! node label so every MATCH conforms to the typed schema's `CONNECTING (user -> user)`
//! constraint, and each scenario scopes its rows through a unique `updated_at` value instead
//! of ad-hoc source labels.

use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, e2e_insert_directed_edge_with_inline_property, e2e_insert_vertex_with_label,
    e2e_set_edge_property, ensure_edge_label, ensure_property, ensure_vertex_label,
    gql_mutate_as_admin, gql_mutate_as_admin_expect_err, gql_query_as_admin,
    install_single_shard_federation,
};
use std::collections::BTreeMap;

const EDGE_LABEL: &str = "AFFINITY";
const PROPERTY: &str = "stats";
const NODE_LABEL: &str = "User";

/// One distinct `updated_at` window per scenario keeps row sets independent inside the shared
/// typed store.
const UPDATED_AT_PROJECTION: u64 = 1_700_000_000;
const UPDATED_AT_FILTER: u64 = 1_700_000_001;
const UPDATED_AT_ORDER: u64 = 1_700_000_002;
const UPDATED_AT_AGGREGATE: u64 = 1_700_000_003;
const UPDATED_AT_PRECEDENCE: u64 = 1_700_000_004;

fn inline_struct_ddl() -> String {
    format!(
        "CREATE GRAPH TYPE IF NOT EXISTS affinity_type {{ NODE User AS user, DIRECTED EDGE Affinity LABEL {EDGE_LABEL} {{ {PROPERTY} {{ score FLOAT32, confidence FLOAT32, updated_at UINT64 }} INLINE }} CONNECTING (user -> user) }} NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED affinity_type"
    )
}

fn affinity_profile() -> gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
    gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
        byte_width: 16,
        encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawBytes,
    }
}

fn pack_stats_inline_property_bytes(score: f32, confidence: f32, updated_at: u64) -> Vec<u8> {
    let mut inline_property_bytes = Vec::with_capacity(16);
    inline_property_bytes.extend_from_slice(&score.to_le_bytes());
    inline_property_bytes.extend_from_slice(&confidence.to_le_bytes());
    inline_property_bytes.extend_from_slice(&updated_at.to_le_bytes());
    inline_property_bytes
}

fn setup() -> FederationEnv {
    let env = install_single_shard_federation();
    gql_mutate_as_admin(
        &env,
        &inline_struct_ddl(),
        "adr0034_inline_struct_read_access_schema",
    );
    env
}

fn extract_rows(result: GqlQueryResult) -> Vec<BTreeMap<String, GqlWireValue>> {
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode rows");
    wire.rows
        .into_iter()
        .map(|row| row.columns.into_iter().collect())
        .collect()
}

fn insert_affinity(
    env: &FederationEnv,
    source: u32,
    target: u32,
    label_id: u16,
    score: f32,
    confidence: f32,
    updated_at: u64,
) {
    e2e_insert_directed_edge_with_inline_property(
        env,
        env.graph_source,
        source,
        target,
        label_id,
        pack_stats_inline_property_bytes(score, confidence, updated_at),
        affinity_profile(),
    );
}

/// Two fresh `User` vertices joined by one AFFINITY edge; returns `(source, target)`.
fn insert_user_pair(env: &FederationEnv, user_label_id: u16) -> (u32, u32) {
    let source = e2e_insert_vertex_with_label(env, env.graph_source, user_label_id).local_vertex_id;
    let target = e2e_insert_vertex_with_label(env, env.graph_source, user_label_id).local_vertex_id;
    (source, target)
}

fn scenario_projection_returns_struct_fields(
    env: &FederationEnv,
    edge_label_id: u16,
    user_label_id: u16,
) {
    let (source, target) = insert_user_pair(env, user_label_id);
    insert_affinity(
        env,
        source,
        target,
        edge_label_id,
        3.5,
        0.75,
        UPDATED_AT_PROJECTION,
    );

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:User)-[e:AFFINITY]->(b) WHERE e.stats.updated_at = {UPDATED_AT_PROJECTION} RETURN e.stats.score AS s, e.stats.confidence AS c, e.stats.updated_at AS u"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "projection scenario: expected one AFFINITY edge"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&GqlWireValue::Float32(3.5)),
        "projection scenario: score field must decode"
    );
    assert_eq!(
        rows[0].get("c"),
        Some(&GqlWireValue::Float32(0.75)),
        "projection scenario: confidence field must decode"
    );
    assert_eq!(
        rows[0].get("u"),
        Some(&GqlWireValue::Uint64(UPDATED_AT_PROJECTION)),
        "projection scenario: updated_at field must decode"
    );
}

fn scenario_filter_matches_struct_field(
    env: &FederationEnv,
    edge_label_id: u16,
    user_label_id: u16,
) {
    let (source, match_target) = insert_user_pair(env, user_label_id);
    let (_, skip_target) = insert_user_pair(env, user_label_id);
    insert_affinity(
        env,
        source,
        match_target,
        edge_label_id,
        3.5,
        0.75,
        UPDATED_AT_FILTER,
    );
    insert_affinity(
        env,
        source,
        skip_target,
        edge_label_id,
        2.0,
        0.50,
        UPDATED_AT_FILTER,
    );

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:User)-[e:AFFINITY]->(b) WHERE e.stats.updated_at = {UPDATED_AT_FILTER} AND e.stats.score >= 3.0 RETURN e.stats.score AS s"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "filter scenario: expected one edge with score >= 3.0"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&GqlWireValue::Float32(3.5)),
        "filter scenario: wrong edge selected"
    );
}

fn scenario_order_by_sorts_by_struct_field(
    env: &FederationEnv,
    edge_label_id: u16,
    user_label_id: u16,
) {
    let (source, first) = insert_user_pair(env, user_label_id);
    let (_, second) = insert_user_pair(env, user_label_id);
    // Insert out of order to prove ORDER BY reads the inline property bytes, not insertion order.
    insert_affinity(
        env,
        source,
        second,
        edge_label_id,
        2.0,
        0.50,
        UPDATED_AT_ORDER,
    );
    insert_affinity(
        env,
        source,
        first,
        edge_label_id,
        3.5,
        0.75,
        UPDATED_AT_ORDER,
    );

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:User)-[e:AFFINITY]->(b) WHERE e.stats.updated_at = {UPDATED_AT_ORDER} RETURN e.stats.score AS s ORDER BY s ASC"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 2, "order scenario: expected two edges");
    assert_eq!(
        rows[0].get("s"),
        Some(&GqlWireValue::Float32(2.0)),
        "order scenario: first row must be the smaller score"
    );
    assert_eq!(
        rows[1].get("s"),
        Some(&GqlWireValue::Float32(3.5)),
        "order scenario: second row must be the larger score"
    );
}

fn scenario_aggregate_uses_struct_field(
    env: &FederationEnv,
    edge_label_id: u16,
    user_label_id: u16,
) {
    let (source, a) = insert_user_pair(env, user_label_id);
    let (_, b) = insert_user_pair(env, user_label_id);
    insert_affinity(
        env,
        source,
        a,
        edge_label_id,
        3.5,
        0.75,
        UPDATED_AT_AGGREGATE,
    );
    insert_affinity(
        env,
        source,
        b,
        edge_label_id,
        2.5,
        0.60,
        UPDATED_AT_AGGREGATE,
    );

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:User)-[e:AFFINITY]->(b) WHERE e.stats.updated_at = {UPDATED_AT_AGGREGATE} RETURN AVG(e.stats.score) AS avg_score"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "aggregate scenario: expected one aggregate row"
    );
    let avg = rows[0].get("avg_score").expect("avg_score");
    let expected = GqlWireValue::Float32(3.0);
    assert_eq!(
        avg, &expected,
        "aggregate scenario: AVG of 3.5 and 2.5 must be 3.0"
    );
}

fn scenario_inline_property_wins_over_sidecar(
    env: &FederationEnv,
    edge_label_id: u16,
    user_label_id: u16,
) {
    let (source, target) = insert_user_pair(env, user_label_id);
    insert_affinity(
        env,
        source,
        target,
        edge_label_id,
        3.5,
        0.75,
        UPDATED_AT_PRECEDENCE,
    );

    // Write a sidecar value with the same property id; the inline inline property bytes must still win.
    let property_id = ensure_property(env, PROPERTY).raw();
    e2e_set_edge_property(env, env.graph_source, source, target, property_id, 99);

    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:User)-[e:AFFINITY]->(b) WHERE e.stats.updated_at = {UPDATED_AT_PRECEDENCE} RETURN e.stats.score AS s"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(
        rows.len(),
        1,
        "precedence scenario: expected exactly one AFFINITY edge"
    );
    assert_eq!(
        rows[0].get("s"),
        Some(&GqlWireValue::Float32(3.5)),
        "precedence scenario: inline property bytes must win over sidecar value"
    );
}

fn scenario_unknown_struct_field_returns_null(env: &FederationEnv) {
    // Reuse the precedence scenario's scoped edge; an absent STRUCT field must decode to NULL.
    let result = gql_query_as_admin(
        env,
        &format!(
            "MATCH (a:User)-[e:AFFINITY]->(b) WHERE e.stats.updated_at = {UPDATED_AT_PRECEDENCE} RETURN e.stats.missing AS m"
        ),
    );
    let rows = extract_rows(result);
    assert_eq!(rows.len(), 1, "unknown-field scenario: expected one row");
    assert_eq!(
        rows[0].get("m"),
        Some(&GqlWireValue::Null),
        "unknown-field scenario: missing struct field must be NULL"
    );
}

fn scenario_struct_index_create_rejects_inline_property(env: &FederationEnv) {
    let err = gql_mutate_as_admin_expect_err(
        env,
        "CREATE INDEX stats_idx FOR ()-[e:AFFINITY]-() ON (e.stats)",
        "adr0034_inline_struct_read_index_conflict",
    );
    assert!(
        matches!(err, RouterError::Conflict(_)),
        "index-conflict scenario: expected Conflict, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// One ordered fixture family covering Slice 25 read semantics end-to-end.
// ---------------------------------------------------------------------------

#[test]
fn inline_struct_read_access_suite() {
    let env = setup();
    let affinity_label_id = ensure_edge_label(&env, EDGE_LABEL).raw();
    let user_label_id = ensure_vertex_label(&env, NODE_LABEL).raw();

    scenario_projection_returns_struct_fields(&env, affinity_label_id, user_label_id);
    scenario_filter_matches_struct_field(&env, affinity_label_id, user_label_id);
    scenario_order_by_sorts_by_struct_field(&env, affinity_label_id, user_label_id);
    scenario_aggregate_uses_struct_field(&env, affinity_label_id, user_label_id);
    scenario_inline_property_wins_over_sidecar(&env, affinity_label_id, user_label_id);
    scenario_unknown_struct_field_returns_null(&env);
    scenario_struct_index_create_rejects_inline_property(&env);
}
