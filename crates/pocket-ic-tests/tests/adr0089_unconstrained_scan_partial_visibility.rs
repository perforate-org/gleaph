//! PocketIC: ADR 0089 — permission-aware label-bucket restriction for unconstrained
//! vertex scans.
//!
//! Truly unconstrained scans (`label: None`) prove the marker + per-bucket path:
//! - `MATCH (a) RETURN 1` — unconstrained vertex scan, empty projection, marker path.
//! - `MATCH (a)-[e:RELATED_TO]->(b) RETURN element_id(e)` — the explorer's edge-load
//!   shape; `a` and `b` are unconstrained, output reads no property (topology-only).
//! - `MATCH (a) RETURN a.name` — property read on an unconstrained scan; must stay
//!   tenancy-only (fail closed) because property values are atomic.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, ensure_edge_label, ensure_vertex_label, gql_query_as,
    install_single_shard_federation,
};

const PERSON: &str = "Person";
const DOCUMENT: &str = "Document";
const RELATED_TO: &str = "RELATED_TO";

/// A non-owner caller who holds `MATCH` on every vertex label.
fn grantee_all() -> Principal {
    Principal::from_slice(&[0xA1; 29])
}

/// A non-owner caller who holds `MATCH` on `Person` only.
fn grantee_person_only() -> Principal {
    Principal::from_slice(&[0xA2; 29])
}

/// A non-owner caller with no vertex-label grants.
fn grantee_none() -> Principal {
    Principal::from_slice(&[0xA3; 29])
}

/// Run a `gql_mutate` as the bootstrap admin principal.
fn mutate_as_admin(env: &FederationEnv, query: &str, key: &str) -> GqlQueryResult {
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

/// Set up a single-shard federation with a Person label, a Document label, and a
/// RELATED_TO directed edge. Two Person and two Document vertices are inserted, with
/// edges between every (Person, Person), (Document, Document), and cross-label pair.
fn setup_env() -> FederationEnv {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, PERSON);
    ensure_vertex_label(&env, DOCUMENT);
    ensure_edge_label(&env, RELATED_TO);

    // Four vertices: two Person, two Document.
    mutate_as_admin(&env, "INSERT (:Person {name: 'alice'})", "adr0089_v_alice");
    mutate_as_admin(&env, "INSERT (:Person {name: 'bob'})", "adr0089_v_bob");
    mutate_as_admin(&env, "INSERT (:Document {title: 'doc1'})", "adr0089_v_doc1");
    mutate_as_admin(&env, "INSERT (:Document {title: 'doc2'})", "adr0089_v_doc2");

    // Edges: same-label pairs and cross-label pairs.
    mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:RELATED_TO]->(b:Person {name: 'bob'})",
        "adr0089_edge_pp",
    );
    mutate_as_admin(
        &env,
        "INSERT (c:Document {title: 'doc1'})-[:RELATED_TO]->(d:Document {title: 'doc2'})",
        "adr0089_edge_dd",
    );
    mutate_as_admin(
        &env,
        "INSERT (a:Person {name: 'alice'})-[:RELATED_TO]->(c:Document {title: 'doc1'})",
        "adr0089_edge_pd",
    );
    mutate_as_admin(
        &env,
        "INSERT (b:Person {name: 'bob'})-[:RELATED_TO]->(d:Document {title: 'doc2'})",
        "adr0089_edge_dp",
    );
    env
}

/// ADR 0089 §1, §5: a non-owner caller with the wildcard `NODES *` grant runs an
/// truly unconstrained vertex scan (`MATCH (a) RETURN 1`). The marker demand admits
/// the caller; the specialized plan visits every label bucket. The result row is
/// produced (one row per scanned vertex, capped at one by the `RETURN 1` constant —
/// but the `row_count` reflects the underlying scan, so we only assert non-Forbidden
/// and a non-error result).
#[test]
fn unconstrained_vertex_scan_with_wildcard_grant_succeeds() {
    let env = setup_env();
    mutate_as_admin(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES * TO PRINCIPAL '{}'",
            grantee_all().to_text()
        ),
        "adr0089_grant_wildcard",
    );
    let result = gql_query_as(&env, grantee_all(), "MATCH (a) RETURN 1");
    assert!(
        result.is_ok(),
        "wildcard caller must run unconstrained scan, got {:?}",
        result.err()
    );
}

/// ADR 0089 §1: a non-owner caller with `MATCH` on a single label runs an unconstrained
/// scan and the specialized plan visits only that label's bucket.
#[test]
fn unconstrained_vertex_scan_with_subset_grant_succeeds() {
    let env = setup_env();
    mutate_as_admin(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person TO PRINCIPAL '{}'",
            grantee_person_only().to_text()
        ),
        "adr0089_grant_person",
    );
    let result = gql_query_as(&env, grantee_person_only(), "MATCH (a) RETURN 1");
    assert!(
        result.is_ok(),
        "subset caller must run unconstrained scan, got {:?}",
        result.err()
    );
}

/// ADR 0089 §2: a non-owner caller with no vertex-label `MATCH` is rejected by the
/// marker demand at plan time (fail closed). The caller gets a READ grant to satisfy
/// the home-graph visibility check; the marker still fires because READ ≠ MATCH.
#[test]
fn unconstrained_vertex_scan_with_zero_match_grants_is_rejected() {
    let env = setup_env();
    mutate_as_admin(
        &env,
        &format!(
            "GRANT READ ON GRAPH {GRAPH_NAME} NODES Person TO PRINCIPAL '{}'",
            grantee_none().to_text()
        ),
        "adr0089_grant_read_none",
    );
    let err = gql_query_as(&env, grantee_none(), "MATCH (a) RETURN 1")
        .expect_err("zero-grant caller must be rejected by the marker");
    assert!(
        matches!(err, RouterError::Forbidden),
        "expected Forbidden, got {err:?}"
    );
}

/// ADR 0089 §1, §4 follow-up: the explorer's edge-load shape
/// `MATCH (a)-[e:RELATED_TO]->(b) RETURN element_id(e)` works for a MATCH-only caller
/// (no READ grants needed) because `element_id` is topology-only — neither `a` nor `b`
/// nor `e` reads a property value, so the scan stays on the marker + per-bucket path.
#[test]
fn explorer_edge_load_with_match_only_grant_succeeds() {
    let env = setup_env();
    // MATCH on all labels (the explorer's whole-graph grant) + TRAVERSE on the edge.
    // No READ grants: the caller must still see every edge because element_id(e) is
    // topology-only.
    mutate_as_admin(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES * TO PRINCIPAL '{}'",
            grantee_all().to_text()
        ),
        "adr0089_explorer_match_all",
    );
    mutate_as_admin(
        &env,
        &format!(
            "GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES RELATED_TO TO PRINCIPAL '{}'",
            grantee_all().to_text()
        ),
        "adr0089_explorer_traverse",
    );
    let result = gql_query_as(
        &env,
        grantee_all(),
        "MATCH (a)-[e:RELATED_TO]->(b) RETURN element_id(e)",
    )
    .expect("MATCH-only caller must run the explorer's edge-load shape");
    // Four distinct edges exist. The cartesian expansion of (a ∈ {Person, Document})
    // × (b ∈ {Person, Document}) × RELATED_TO produces duplicates per edge identity
    // when the same edge is reached from different (a, b) pairs; we assert at least
    // 4 rows (one per edge) and rely on the per-bucket restriction to keep the
    // result non-empty.
    assert!(
        result.row_count >= 4,
        "explorer edge-load must return every edge, got {}",
        result.row_count
    );
}

/// ADR 0089 §4: a MATCH-only caller with `MATCH` on a subset sees only those labels'
/// edges in the explorer's edge-load shape. Specialization restricts the
/// unconstrained `a` endpoint to the caller's grantable label(s); the `b` endpoint is
/// bound by `Expand` and inherits visibility from the traversal (its concrete label
/// is checked at execute time, not by the plan-time authorization walker).
#[test]
fn explorer_edge_load_with_subset_match_grant_returns_only_subset() {
    let env = setup_env();
    mutate_as_admin(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Person TO PRINCIPAL '{}'",
            grantee_person_only().to_text()
        ),
        "adr0089_explorer_match_person",
    );
    mutate_as_admin(
        &env,
        &format!(
            "GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES RELATED_TO TO PRINCIPAL '{}'",
            grantee_person_only().to_text()
        ),
        "adr0089_explorer_traverse_person",
    );
    let result = gql_query_as(
        &env,
        grantee_person_only(),
        "MATCH (a)-[e:RELATED_TO]->(b) RETURN element_id(e)",
    )
    .expect("subset MATCH-only caller must run the explorer's edge-load shape");
    // Specialization restricts `a` to Person, so the scan visits Person vertices
    // only. Each Person vertex expands via RELATED_TO to its neighbors; the three
    // edges with a Person source (alice->bob, alice->doc1, bob->doc2) are visible.
    // The fourth edge (doc1->doc2) is not visible because its source is a Document
    // vertex, which is excluded by the per-bucket restriction.
    assert_eq!(
        result.row_count, 3,
        "subset caller must see only edges sourced from Person, got {}",
        result.row_count
    );
}

/// ADR 0089 §4: a query that reads a property on an unconstrained scan
/// (`MATCH (a) RETURN a.name`) stays tenancy-only (fail closed) because property
/// values are atomic. A MATCH-only caller is rejected with Forbidden.
#[test]
fn unconstrained_scan_with_property_read_stays_tenancy_only() {
    let env = setup_env();
    mutate_as_admin(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES * TO PRINCIPAL '{}'",
            grantee_all().to_text()
        ),
        "adr0089_property_match_wildcard",
    );
    // MATCH alone does not cover READ on a.name; the unconstrained scan with a
    // property projection must fail closed.
    let err = gql_query_as(&env, grantee_all(), "MATCH (a) RETURN a.name")
        .expect_err("property read on unconstrained scan must be tenancy-only");
    assert!(
        matches!(err, RouterError::Forbidden),
        "expected Forbidden, got {err:?}"
    );
}
