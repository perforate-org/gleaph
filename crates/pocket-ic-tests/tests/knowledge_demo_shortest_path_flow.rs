//! PocketIC: runtime proof that the knowledge demo's `shortest-path` prepared op executes
//! END-TO-END for a NON-OWNER after publication, closing the last broken demo scenario
//! (plan 0310 diagnosis / GAP-2026-08-26-004; standalone label-anchor seeding in plan 0311).
//!
//! The op reads incoming from the sink (`graph-databases` has zero outgoing RELATED_TO in
//! the demo seeds), so the matched chain is the authored two-hop
//! `Vector search -> GraphRAG -> Graph databases`. This flow replays exactly that fixture:
//!
//! 1. Default deny: before any publication a distinct non-owner principal is rejected with
//!    the uniform `Forbidden`.
//! 2. After publication (PUBLIC MATCH/TRAVERSE rows + brace-form `READ ... { name }` +
//!    quoted-identifier EXECUTE publication) the SAME non-owner call succeeds and returns
//!    exactly one row whose `concept` is `Vector search` — the pattern's far endpoint —
//!    with a populated `concept_id`.

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::{GqlWireRow, GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, gql_mutate_as_admin,
    install_single_shard_federation, prepare_batch_as_admin, prepared_query_with_params_as,
};
use gleaph_prepared_api::PreparedRegistration;

const OP_NAME: &str = "shortest-path";

/// Byte-identical to demo/knowledge/prepared/shortest-path.gql modulo whitespace.
const SHORTEST_PATH_SOURCE: &str = "MATCH ANY SHORTEST \
                                     (a:Concept {name: 'Graph databases'})<-[e:RELATED_TO]-{1,4}(b:Concept {name: 'Vector search'}) \
                                     RETURN ELEMENT_ID(b) AS concept_id, b.name AS concept";

/// A plain visitor: neither registry owner/admin nor grantee until publication.
fn dev() -> Principal {
    Principal::from_slice(&[0x0D; 29])
}

/// Typed schema with one directed RELATED_TO edge label between Concepts.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE kct {{ NODE Concept {{ name STRING }}, \
         DIRECTED EDGE Related LABEL RELATED_TO CONNECTING (Concept -> Concept) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED kct"
    );
    gql_mutate_as_admin(env, &ddl, "shortest-path-bind-schema");
}

fn insert_concept(env: &FederationEnv, key: &str, name: &str) {
    gql_mutate_as_admin(env, &format!("INSERT (:Concept {{name: '{name}'}})"), key);
    drain_maintenance_via_timer(env, env.graph_source);
}

fn insert_related_edge(env: &FederationEnv, key: &str, from_name: &str, to_name: &str) {
    gql_mutate_as_admin(
        env,
        &format!(
            "MATCH (a:Concept {{name: '{from_name}'}}) RETURN a NEXT \
             MATCH (b:Concept {{name: '{to_name}'}}) \
             INSERT (a)-[:RELATED_TO]->(b)"
        ),
        key,
    );
    drain_maintenance_via_timer(env, env.graph_source);
}

/// Seed the authored chain Vector search → GraphRAG → Graph databases; every RELATED_TO
/// edge points toward the sink `Graph databases`, mirroring the demo seeds' polarity.
fn seed_related_chain(env: &FederationEnv) {
    insert_concept(env, "shortest-path-seed-sink", "Graph databases");
    insert_concept(env, "shortest-path-seed-mid", "GraphRAG");
    insert_concept(env, "shortest-path-seed-src", "Vector search");
    insert_related_edge(env, "shortest-path-seed-vs-gr", "Vector search", "GraphRAG");
    insert_related_edge(
        env,
        "shortest-path-seed-gr-gd",
        "GraphRAG",
        "Graph databases",
    );
}

/// The PUBLIC data-plane rows shortest-path demands, in the exact statement forms of
/// apply-public-grants.sh (brace-form property READ row included).
fn grant_public_surface(env: &FederationEnv) {
    for (key, statement) in [
        (
            "shortest-path-grant-match",
            format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Concept TO PUBLIC"),
        ),
        (
            "shortest-path-grant-read",
            format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Concept {{ name }} TO PUBLIC"),
        ),
        (
            "shortest-path-grant-traverse",
            format!("GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES RELATED_TO TO PUBLIC"),
        ),
    ] {
        gql_mutate_as_admin(env, &statement, key);
    }
}

/// The EXECUTE publication row via the double-quoted identifier form
/// (GAP-2026-08-24-005), like the CLI/demo path.
fn publish_execute_row(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        &format!("GRANT EXECUTE ON PREPARED QUERY \"{OP_NAME}\" TO PUBLIC"),
        "shortest-path-publish",
    );
}

#[test]
fn non_owner_executes_shortest_path_after_publication_and_is_denied_before() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_related_chain(&env);
    prepare_batch_as_admin(
        &env,
        &[PreparedRegistration {
            name: OP_NAME.into(),
            query: SHORTEST_PATH_SOURCE.into(),
            metadata: None,
        }],
    )
    .expect("register prepared op");

    // Negative control — default deny: before ANY publication the non-owner is rejected.
    let denied = forbidden_prepared_query_err(&env, dev());
    assert!(matches!(denied, RouterError::Forbidden), "got {denied:?}");

    // Publish: PUBLIC data-plane rows + EXECUTE on the op.
    grant_public_surface(&env);
    publish_execute_row(&env);

    // The same non-owner now executes end-to-end and receives the far endpoint.
    let result = prepared_query_with_params_as(&env, dev(), OP_NAME, Vec::new());
    assert_eq!(
        result.row_count, 1,
        "ANY SHORTEST over the linear chain yields exactly the far endpoint"
    );
    let blob = result.rows_blob.expect("rows blob");
    let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
    assert_eq!(rows.rows.len(), 1, "decoded rows match the reported count");

    let row: &GqlWireRow = &rows.rows[0];
    assert_eq!(
        column_text(row, "concept"),
        "Vector search",
        "the returned concept is the pattern's far endpoint b"
    );
    assert_non_empty_identity(column(row, "concept_id"), "concept_id");
}

/// Raw denied `prepared_query` returning the typed error so denials are observable without
/// panicking inside the helper.
fn forbidden_prepared_query_err(env: &FederationEnv, caller: Principal) -> RouterError {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "prepared_query",
            Encode!(
                &OP_NAME.to_string(),
                &Vec::<u8>::new(),
                &Option::<Vec<gleaph_prepared_api::PreparedSortSpec>>::None,
                &ReadMode::Eventual
            )
            .expect("encode prepared_query"),
        )
        .expect("prepared_query call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Err(err)) => err,
        Ok(Ok(_)) => panic!("prepared_query unexpectedly succeeded for {caller}"),
        Err(err) => panic!("decode prepared_query: {err}"),
    }
}

fn column<'a>(row: &'a GqlWireRow, name: &'static str) -> &'a GqlWireValue {
    &row.columns
        .iter()
        .find(|(column, _)| column == name)
        .unwrap_or_else(|| panic!("missing {name} column"))
        .1
}

fn column_text(row: &GqlWireRow, name: &'static str) -> String {
    match column(row, name) {
        GqlWireValue::Text(text) => text.clone(),
        other => panic!("{name}: unexpected wire value {other:?}"),
    }
}

/// Identity metadata must arrive populated whatever wire shape element ids take.
fn assert_non_empty_identity(value: &GqlWireValue, what: &str) {
    match value {
        GqlWireValue::Text(text) => assert!(!text.is_empty(), "{what} must be non-empty"),
        GqlWireValue::Bytes(bytes) => assert!(!bytes.is_empty(), "{what} must be non-empty"),
        GqlWireValue::List(items) => assert!(!items.is_empty(), "{what} must be non-empty"),
        other => panic!("{what}: unexpected wire value {other:?}"),
    }
}
