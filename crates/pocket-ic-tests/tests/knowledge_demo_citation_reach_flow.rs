//! PocketIC: runtime proof that the knowledge demo's `citation-reach` prepared op executes
//! END-TO-END for a NON-OWNER once the PUBLIC grant surface is applied (plan 0306 /
//! GAP-2026-08-24-008 residual follow-up (a); executor gap resolved by plan slice A,
//! group-variable ELEMENT_ID).
//!
//! The static half is pinned by `edge_element_id_projection_demands_stay_attributed`
//! (router lib test): the ELEMENT_ID(e) projection adds NO authz demand beyond the CITES
//! traversal row. These tests prove the dynamic half through the real Router → Graph
//! boundary with the EXACT demo source, registered and published exactly like the demo
//! (`apply-public-grants.sh` brace-form PUBLIC rows; quoted-identifier EXECUTE publication
//! per GAP-2026-08-24-005):
//!
//! 1. Default deny: before any publication a distinct non-owner principal is rejected with
//!    the uniform `Forbidden`.
//! 2. After publication the SAME non-owner call succeeds and returns exactly the reachable
//!    documents (`Chain B/C/D`) with populated `document_id`s. `cite_edge_id` arrives as a
//!    LIST of edge-id byte strings, one entry PER HOP in traversal order — for this linear
//!    fixture the hop count per row is determined by the destination's distance from the
//!    source (B=1, C=2, D=3).
//! 3. The traversal-only companion pins the identical authorization behavior when the edge
//!    identity projection is absent, isolating the projection as the only difference.
//!
//! Expected counts stated before running (per plan contract):
//! - 2 `#[test]` functions; one PocketIC environment EACH (`install_single_shard_federation`:
//!   3 funded creations + 3 canister installs per environment). No extra installs/upgrades.
//! - Per environment: 1 schema bind mutate, 7 seeded INSERTs (each maintenance-drained),
//!   1 prepare batch, 4 authorization statements (3 PUBLIC data rows + 1 quoted EXECUTE
//!   publication), 2 `prepared_query` probes (deny leg, then post-publication leg).

use candid::{Decode, Encode, Principal};
use gleaph_gql_ic::{GqlWireRow, GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, gql_mutate_as_admin,
    install_single_shard_federation, prepare_batch_as_admin, prepared_query_with_params_as,
};
use gleaph_prepared_api::PreparedRegistration;

const OP_NAME: &str = "citation-reach";

/// Byte-identical to demo/knowledge/prepared/citation-reach.gql modulo whitespace.
const CITATION_REACH_SOURCE: &str = "MATCH (src:Document {title: 'Introduction to graph databases'})\
                                     -[e:CITES]->{1,3}(dst:Document) \
                                     RETURN ELEMENT_ID(dst) AS document_id, dst.title AS title, \
                                     ELEMENT_ID(e) AS cite_edge_id";

/// A plain visitor: neither registry owner/admin nor grantee until publication.
fn dev() -> Principal {
    Principal::from_slice(&[0x0D; 29])
}

/// Typed schema with one directed CITES edge label, mirroring the demo's typed graph shape.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE kct {{ NODE Document {{ title STRING }}, \
         DIRECTED EDGE Cites LABEL CITES CONNECTING (Document -> Document) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED kct"
    );
    gql_mutate_as_admin(env, &ddl, "citation-reach-bind-schema");
}

fn insert_document(env: &FederationEnv, key: &str, title: &str) {
    gql_mutate_as_admin(
        env,
        &format!("INSERT (:Document {{title: '{title}'}})"),
        key,
    );
    drain_maintenance_via_timer(env, env.graph_source);
}

fn insert_cites_edge(env: &FederationEnv, key: &str, from_title: &str, to_title: &str) {
    gql_mutate_as_admin(
        env,
        &format!(
            "MATCH (a:Document {{title: '{from_title}'}}) RETURN a NEXT \
             MATCH (b:Document {{title: '{to_title}'}}) \
             INSERT (a)-[:CITES]->(b)"
        ),
        key,
    );
    drain_maintenance_via_timer(env, env.graph_source);
}

/// Seed A→B→C→D via CITES; A carries the op's literal-matched title.
fn seed_citation_chain(env: &FederationEnv) {
    insert_document(
        env,
        "citation-reach-seed-a",
        "Introduction to graph databases",
    );
    insert_document(env, "citation-reach-seed-b", "Chain B");
    insert_document(env, "citation-reach-seed-c", "Chain C");
    insert_document(env, "citation-reach-seed-d", "Chain D");
    insert_cites_edge(
        env,
        "citation-reach-seed-ab",
        "Introduction to graph databases",
        "Chain B",
    );
    insert_cites_edge(env, "citation-reach-seed-bc", "Chain B", "Chain C");
    insert_cites_edge(env, "citation-reach-seed-cd", "Chain C", "Chain D");
}

/// The PUBLIC data-plane rows citation-reach's static set demands, in the exact statement
/// forms of apply-public-grants.sh (brace-form property READ row included).
fn grant_public_surface(env: &FederationEnv) {
    for (key, statement) in [
        (
            "citation-reach-grant-match",
            format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document TO PUBLIC"),
        ),
        (
            "citation-reach-grant-read",
            format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Document {{ title }} TO PUBLIC"),
        ),
        (
            "citation-reach-grant-traverse",
            format!("GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES CITES TO PUBLIC"),
        ),
    ] {
        gql_mutate_as_admin(env, &statement, key);
    }
}

/// The EXECUTE publication row, issued through the same grammar path as the CLI/demo:
/// the kebab-case op name rides the double-quoted identifier form (GAP-2026-08-24-005).
fn publish_execute_row(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        &format!("GRANT EXECUTE ON PREPARED QUERY \"{OP_NAME}\" TO PUBLIC"),
        "citation-reach-publish",
    );
}

/// Bind the typed schema, seed the chain, and register `source` under the owner's graph
/// context.
fn register_and_seed(env: &FederationEnv, source: &str) {
    bind_typed_schema(env);
    seed_citation_chain(env);
    prepare_batch_as_admin(
        env,
        &[PreparedRegistration {
            name: OP_NAME.into(),
            query: source.into(),
            metadata: None,
        }],
    )
    .expect("register prepared op");
}

#[test]
fn non_owner_executes_citation_reach_after_publication_and_is_denied_before() {
    let env = install_single_shard_federation();
    register_and_seed(&env, CITATION_REACH_SOURCE);

    // Negative control — default deny: before ANY publication the non-owner is rejected.
    let denied = forbidden_prepared_query_err(&env, dev());
    assert!(matches!(denied, RouterError::Forbidden), "got {denied:?}");

    // Publish: PUBLIC data-plane rows + EXECUTE on the op.
    grant_public_surface(&env);
    publish_execute_row(&env);

    // The same non-owner now executes end-to-end and receives content.
    let result = prepared_query_with_params_as(&env, dev(), OP_NAME, Vec::new());
    assert_eq!(result.row_count, 3, "B/C/D are reachable within 1..3 hops");
    let blob = result.rows_blob.expect("rows blob");
    let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
    assert_eq!(rows.rows.len(), 3, "decoded rows match the reported count");

    let mut titles: Vec<String> = Vec::new();
    for row in &rows.rows {
        let title = column_text(row, "title");
        assert_non_empty_identity(column(row, "document_id"), "document_id");

        // Group-variable identity: a LIST of edge ids, one per hop in traversal order.
        // The linear fixture pins each destination's hop count to its distance.
        match column(row, "cite_edge_id") {
            GqlWireValue::List(hops) => {
                assert!(
                    !hops.is_empty(),
                    "cite_edge_id must carry at least one hop id"
                );
                assert_eq!(
                    hops.len(),
                    expected_hops(&title),
                    "{title} sits at a fixed distance from the source"
                );
                for hop in hops {
                    assert_non_empty_identity(hop, "cite_edge_id entry");
                }
            }
            other => panic!("cite_edge_id must be a list of hop ids, got {other:?}"),
        }
        titles.push(title);
    }
    titles.sort();
    assert_eq!(
        titles,
        vec![
            "Chain B".to_owned(),
            "Chain C".to_owned(),
            "Chain D".to_owned()
        ],
        "exactly the reachable documents within 1..3 hops come back"
    );
}

#[test]
fn non_owner_executes_citation_reach_traversal_and_gets_reachable_rows() {
    // Identical flow minus the edge-identity projection — isolates ELEMENT_ID(e) as the
    // only difference between the two flows while pinning the shared authorization path.
    let source = "MATCH (src:Document {title: 'Introduction to graph databases'})\
                  -[e:CITES]->{1,3}(dst:Document) \
                  RETURN ELEMENT_ID(dst) AS document_id, dst.title AS title";
    let env = install_single_shard_federation();
    register_and_seed(&env, source);

    let denied = forbidden_prepared_query_err(&env, dev());
    assert!(matches!(denied, RouterError::Forbidden), "got {denied:?}");

    grant_public_surface(&env);
    publish_execute_row(&env);

    let result = prepared_query_with_params_as(&env, dev(), OP_NAME, Vec::new());
    assert_eq!(result.row_count, 3, "B/C/D are reachable within 1..3 hops");
    let blob = result.rows_blob.expect("rows blob");
    let rows = GqlWireRows::decode_blob(&blob).expect("decode rows");
    assert_eq!(rows.rows.len(), 3, "decoded rows match the reported count");

    let mut titles: Vec<String> = Vec::new();
    for row in &rows.rows {
        titles.push(column_text(row, "title"));
        assert_non_empty_identity(column(row, "document_id"), "document_id");
    }
    titles.sort();
    assert_eq!(
        titles,
        vec![
            "Chain B".to_owned(),
            "Chain C".to_owned(),
            "Chain D".to_owned()
        ],
        "exactly the reachable documents within 1..3 hops come back"
    );
}

/// Hop count from the source to each seeded destination along the linear citation chain.
fn expected_hops(title: &str) -> usize {
    match title {
        "Chain B" => 1,
        "Chain C" => 2,
        "Chain D" => 3,
        other => panic!("unexpected destination title {other:?}"),
    }
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
