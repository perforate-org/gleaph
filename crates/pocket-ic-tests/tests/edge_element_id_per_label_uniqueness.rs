//! PocketIC: ADR 0090 — edge element-id label attribution. The router's wire-format
//! `ELEMENT_ID(e)` must be unique per edge under the per-graph key; cross-label
//! collisions at the same `(owner, slot)` are the bug fixed by this slice
//! (plan 0312 / GAP-2026-08-29-001).
//!
//! The fixture seeds a single owner vertex with three edges of three different labels,
//! each occupying the per-bucket slot index 0. Pre-fix, all three edges encoded to the
//! same `(shard, owner, 0)` and projected to the same element id; post-fix the labels
//! make them distinct. The host-level contract tests in
//! `crates/graph/src/plan/query/executor/path/element_id_per_label.rs` pin the encoder
//! in isolation; this test pins the end-to-end shape against a real router.
//!
//! Expected counts (per the focused-test brief):
//! - 2 `#[test]` functions; one PocketIC environment EACH
//!   (`install_single_shard_federation`: 3 funded creations + 3 canister installs per
//!   environment).
//! - Per environment: 1 schema bind mutate, 4 seeded INSERTs (one per node + three edges,
//!   each maintenance-drained), 1 gql_query probe per test (no prepared op, no grants:
//!   bootstrap admin runs the probe).

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, ensure_edge_label, ensure_vertex_label,
    install_single_shard_federation,
};

const PERSON: &str = "Person";
const KNOWS: &str = "KNOWS";
const OWNS: &str = "OWNS";
const ROUTES_VIA: &str = "ROUTES_VIA";

fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE edge_id_label {{ NODE Person {{ name STRING }}, \
         DIRECTED EDGE Knows LABEL {KNOWS} CONNECTING (Person -> Person), \
         DIRECTED EDGE Owns LABEL {OWNS} CONNECTING (Person -> Person), \
         DIRECTED EDGE RoutesVia LABEL {ROUTES_VIA} CONNECTING (Person -> Person) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED edge_id_label"
    );
    gql_mutate_as_admin(env, &ddl, "edge-id-bind-schema");
}

fn gql_mutate_as_admin(env: &FederationEnv, query: &str, key: &str) -> GqlQueryResult {
    use gleaph_graph_kernel::federation::RouterError;
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_mutate",
            Encode!(&query.to_string(), &Vec::<u8>::new(), &key.to_string())
                .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => panic!("gql_mutate rejected: {err:?}"),
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

fn gql_query_as_admin(env: &FederationEnv, query: &str) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &gleaph_graph_kernel::plan_exec::ReadMode::Eventual
            )
            .expect("encode gql_query"),
        )
        .expect("gql_query on router");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>)
        .unwrap_or_else(|err| panic!("decode gql_query: {err}"))
}

/// Set up a single-shard federation with one `Person` label and three edge labels.
/// Insert one source Person and three target Persons, then three edges from the same
/// source under three different labels — all three occupy per-bucket slot index 0
/// (the first edge in their respective `(owner, label)` buckets).
fn setup_three_label_owner() -> FederationEnv {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, PERSON);
    ensure_edge_label(&env, KNOWS);
    ensure_edge_label(&env, OWNS);
    ensure_edge_label(&env, ROUTES_VIA);
    bind_typed_schema(&env);
    gql_mutate_as_admin(&env, "INSERT (:Person {name: 'alice'})", "edge-id-v-alice");
    gql_mutate_as_admin(&env, "INSERT (:Person {name: 'bob'})", "edge-id-v-bob");
    gql_mutate_as_admin(&env, "INSERT (:Person {name: 'carol'})", "edge-id-v-carol");
    gql_mutate_as_admin(&env, "INSERT (:Person {name: 'dave'})", "edge-id-v-dave");
    gql_mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:KNOWS]->(b:Person {name: 'bob'})",
        "edge-id-edge-knows",
    );
    gql_mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:OWNS]->(c:Person {name: 'carol'})",
        "edge-id-edge-owns",
    );
    gql_mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:ROUTES_VIA]->(d:Person {name: 'dave'})",
        "edge-id-edge-routes",
    );
    env
}

/// Extract every element-id column value as 16-byte wire bytes, in row order. The
/// column may carry a single `GqlWireValue::Bytes` or — for group bindings — a list of
/// such values; both forms are reduced to the leaf set.
fn collect_edge_id_bytes(rows: &gleaph_gql_ic::GqlWireRows, column: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for row in &rows.rows {
        let value = row
            .columns
            .iter()
            .find(|(name, _)| name == column)
            .unwrap_or_else(|| panic!("missing {column} column"))
            .1
            .clone();
        match value {
            GqlWireValue::Bytes(bytes) => out.push(bytes),
            GqlWireValue::List(items) => {
                for item in items {
                    match item {
                        GqlWireValue::Bytes(bytes) => out.push(bytes),
                        other => panic!("{column} list entry must be Bytes, got {other:?}"),
                    }
                }
            }
            other => panic!("{column} must be Bytes or List of Bytes, got {other:?}"),
        }
    }
    out
}

/// Three edges from the same owner vertex under three different labels must produce
/// three distinct 16-byte `ELEMENT_ID(e)` values. Pre-fix, all three encoded to the
/// same `(shard, owner, 0)` and produced a single 12-byte id shared across labels.
#[test]
fn three_labels_same_owner_slot_zero_produce_distinct_element_ids() {
    let env = setup_three_label_owner();
    let result = gql_query_as_admin(
        &env,
        "MATCH (a:Person {name: 'alice'})-[e]->(b:Person) RETURN e, element_id(e) AS eid",
    )
    .expect("element id query");
    let blob = result.rows_blob.expect("rows blob");
    let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
    let bytes = collect_edge_id_bytes(&rows, "eid");
    assert_eq!(
        bytes.len(),
        3,
        "alice has exactly three outgoing edges (one per label)"
    );
    // Every edge id must be exactly 16 bytes (ADR 0090 layout).
    for (i, b) in bytes.iter().enumerate() {
        assert_eq!(b.len(), 16, "edge {i} id must be 16 bytes, got {}", b.len());
    }
    // The three ids must be pairwise distinct. Pre-fix they all collided to the same
    // 12-byte word; post-fix each label makes a different wire id.
    let mut sorted = bytes.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        3,
        "three labels must produce three distinct element ids; pre-fix this dedupes to 1"
    );
}

/// A single label with two distinct edges must still produce two distinct ids — this
/// is the per-bucket slot model preserved by ADR 0090. The label-bearing identity does
/// not collapse distinct edges within the same label.
#[test]
fn single_label_two_edges_produce_distinct_element_ids() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, PERSON);
    ensure_edge_label(&env, KNOWS);
    bind_typed_schema(&env);
    gql_mutate_as_admin(
        &env,
        "INSERT (:Person {name: 'alice'})",
        "edge-id-single-alice",
    );
    gql_mutate_as_admin(&env, "INSERT (:Person {name: 'bob'})", "edge-id-single-bob");
    gql_mutate_as_admin(
        &env,
        "INSERT (:Person {name: 'carol'})",
        "edge-id-single-carol",
    );
    gql_mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:KNOWS]->(b:Person {name: 'bob'})",
        "edge-id-single-k1",
    );
    gql_mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:KNOWS]->(c:Person {name: 'carol'})",
        "edge-id-single-k2",
    );
    let result = gql_query_as_admin(
        &env,
        "MATCH (a:Person {name: 'alice'})-[e:KNOWS]->(b:Person) RETURN e, element_id(e) AS eid",
    )
    .expect("element id query");
    let blob = result.rows_blob.expect("rows blob");
    let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
    let bytes = collect_edge_id_bytes(&rows, "eid");
    assert_eq!(bytes.len(), 2, "alice has two outgoing KNOWS edges");
    for (i, b) in bytes.iter().enumerate() {
        assert_eq!(b.len(), 16, "edge {i} id must be 16 bytes, got {}", b.len());
    }
    let mut sorted = bytes.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        2,
        "two distinct edges under the same label must produce two distinct ids"
    );
}

// Touch unused `Principal` import warnings if the helper set shrinks.
#[allow(dead_code)]
const _DEV: Principal = Principal::from_slice(&[0x0D; 29]);
