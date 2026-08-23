//! PocketIC: ADR 0074 §3 invariant 4 — vocabulary-drop grant cascade end to end.
//!
//! One suite drives the real teardown path (`unregister_shard`, then
//! `unregister_graph`) and proves the authorization hygiene contract:
//!
//! 1. Before teardown, the dropped graph's owner-only introspection lists exactly its
//!    stored rows behind the synthesized implicit-root marker, and a formerly-granted
//!    caller executes a labeled pattern through the prepared surface.
//! 2. Teardown sweeps exactly the dropped graph's data-plane grant rows: the sibling
//!    graph's listing is byte-for-byte unchanged even though its labels reused the same
//!    numeric catalog ids (exact-key evidence), while name-keyed `EXECUTE PreparedQuery`
//!    rows are intentionally not part of any graph sweep.
//! 3. Afterwards the formerly-granted caller loses access uniformly: the dropped graph no
//!    longer resolves (ADR 0028 `NotFound`) and the prepared query referencing the removed
//!    label stays denied (`Forbidden`) — stored requirement sets fail closed on dead ids
//!    and are deliberately not recomputed.
//!
//! Expected counts stated before running (per plan contract):
//! - 1 `#[test]` function.
//! - One environment via `install_two_graph_federation`: 5 funded canister creations
//!   (Router, Index ×2, Graph shard ×2) and 5 installs, plus the constructor's internal
//!   registry updates.
//! - Update-call budget per environment: ≤4 label/property interning calls, ≤6
//!   GRANT/publication mutates, ≤1 prepare batch, ≤1 shard unregister, ≤1 graph
//!   unregister.
//! - Query-call budget: ≤10 (grant listings, prepared execution, ad-hoc reads).

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, gql_mutate_as_admin, install_two_graph_federation, prepare_as_admin,
};
use gleaph_router::types::{
    GrantOperationView, GrantResourceKindView, GrantSubjectView, GraphGrantSummary,
};

const DROPPED_GRAPH: &str = "tenant_a";
const SURVIVOR_GRAPH: &str = "tenant_b";

/// A principal holding rows only on [`DROPPED_GRAPH`] (plus the prepared EXECUTE row):
/// the formerly-granted caller whose access must vanish with the vocabulary.
fn grantee() -> Principal {
    Principal::from_slice(&[0xC7; 29])
}

/// A second, unrelated subject carrying survivor rows on [`SURVIVOR_GRAPH`].
fn survivor_subject() -> Principal {
    Principal::from_slice(&[0xC9; 29])
}

/// Intern one vertex label on an arbitrary named graph (the shared helper is pinned to
/// the single-shard fixture graph).
fn ensure_vertex_label_on(env: &FederationEnv, graph_name: &str, label: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_vertex_label",
            Encode!(&graph_name.to_string(), &label.to_string())
                .expect("encode ensure_vertex_label"),
        )
        .expect("ensure_vertex_label call");
    match Decode!(
        &bytes,
        Result<gleaph_graph_kernel::entry::VertexLabelId, RouterError>
    ) {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => panic!("ensure_vertex_label rejected: {err:?}"),
        Err(err) => panic!("decode ensure_vertex_label: {err}"),
    }
}

/// Intern one edge label on an arbitrary named graph.
fn ensure_edge_label_on(env: &FederationEnv, graph_name: &str, label: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "ensure_edge_label",
            Encode!(&graph_name.to_string(), &label.to_string()).expect("encode ensure_edge_label"),
        )
        .expect("ensure_edge_label call");
    match Decode!(
        &bytes,
        Result<gleaph_graph_kernel::entry::EdgeLabelId, RouterError>
    ) {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => panic!("ensure_edge_label rejected: {err:?}"),
        Err(err) => panic!("decode ensure_edge_label: {err}"),
    }
}

/// Owner-only grant introspection with the typed error surfaced.
fn list_grants(
    env: &FederationEnv,
    graph_name: &str,
) -> Result<Vec<GraphGrantSummary>, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_graph_grants",
            Encode!(&graph_name.to_string()).expect("encode list_graph_grants"),
        )
        .expect("list_graph_grants call");
    Decode!(&bytes, Result<Vec<GraphGrantSummary>, RouterError>).expect("decode list_graph_grants")
}

/// Raw `prepared_query` verdict so denials are observable without panicking.
fn prepared_query_result(
    env: &FederationEnv,
    caller: Principal,
    name: &str,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "prepared_query",
            Encode!(
                &name.to_string(),
                &Vec::<u8>::new(),
                &Option::<Vec<gleaph_prepared_api::PreparedSortSpec>>::None,
                &gleaph_graph_kernel::plan_exec::ReadMode::Eventual
            )
            .expect("encode prepared_query"),
        )
        .expect("prepared_query call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode prepared_query")
}

/// Raw composite `gql_query` verdict so resolution-level denial is observable.
fn gql_query_result(
    env: &FederationEnv,
    caller: Principal,
    query: &str,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &gleaph_graph_kernel::plan_exec::ReadMode::Eventual
            )
            .expect("encode gql_query"),
        )
        .expect("gql_query call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query")
}

/// Real ingress teardown, step one: remove the dropped graph's only shard.
fn unregister_shard(env: &FederationEnv, graph_name: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "unregister_shard",
            Encode!(&graph_name.to_string(), &ShardId::new(0)).expect("encode unregister_shard"),
        )
        .expect("unregister_shard call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("unregister_shard rejected: {err:?}"),
        Err(err) => panic!("decode unregister_shard: {err}"),
    }
}

/// Real ingress teardown, step two: unregister the graph itself (the vocabulary-drop
/// boundary that owns the ADR 0074 invariant-4 grant sweep).
fn unregister_graph(env: &FederationEnv, graph_name: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "unregister_graph",
            Encode!(&graph_name.to_string()).expect("encode unregister_graph"),
        )
        .expect("unregister_graph call");
    match Decode!(&bytes, Result<(), RouterError>) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("unregister_graph rejected: {err:?}"),
        Err(err) => panic!("decode unregister_graph: {err}"),
    }
}

/// The implicit-root marker every owner listing leads with (ADR 0074 §3 invariant 3).
fn expect_implicit_root_marker(row: &GraphGrantSummary, graph_name: &str, env: &FederationEnv) {
    assert_eq!(
        row.subject,
        GrantSubjectView::Principal(env.admin.to_text())
    );
    assert_eq!(row.operation, GrantOperationView::ImplicitRoot);
    assert_eq!(row.direction, None);
    assert_eq!(row.resource.kind, GrantResourceKindView::Graph);
    assert_eq!(row.resource.label, graph_name);
    assert_eq!(row.resource.property, None);
    assert_eq!(row.expires_at_ns, None);
}

#[test]
fn unregister_graph_sweeps_dropped_vocabulary_grants_and_fails_closed() {
    let env = install_two_graph_federation();

    // Both graphs intern identically-named vocabulary, so their numeric catalog ids
    // collide across graphs — exactly the collision an inexact sweep would corrupt.
    ensure_vertex_label_on(&env, DROPPED_GRAPH, "Person");
    ensure_edge_label_on(&env, DROPPED_GRAPH, "KNOWS");
    ensure_vertex_label_on(&env, SURVIVOR_GRAPH, "Person");
    ensure_edge_label_on(&env, SURVIVOR_GRAPH, "KNOWS");

    // Dropped-graph rows: MATCH + READ for the grantee (the prepared scan's static
    // demand) and a PUBLIC traversal row.
    let self_grantee = format!("PRINCIPAL '{}'", grantee().to_text());
    for (statement, key) in [
        (
            format!("GRANT MATCH ON GRAPH {DROPPED_GRAPH} NODES Person TO {self_grantee}"),
            "cascade-grant-match",
        ),
        (
            format!("GRANT READ ON GRAPH {DROPPED_GRAPH} NODES Person TO {self_grantee}"),
            "cascade-grant-read",
        ),
        (
            format!("GRANT TRAVERSE ON GRAPH {DROPPED_GRAPH} EDGES KNOWS TO PUBLIC"),
            "cascade-grant-traverse",
        ),
    ] {
        gql_mutate_as_admin(&env, &statement, key);
    }

    // Survivor-graph rows under DIFFERENT subjects: identical label names and therefore
    // identical numeric ids, different graph discriminator and subject.
    let self_survivor = format!("PRINCIPAL '{}'", survivor_subject().to_text());
    for (statement, key) in [
        (
            format!("GRANT MATCH ON GRAPH {SURVIVOR_GRAPH} NODES Person TO {self_survivor}"),
            "cascade-survivor-match",
        ),
        (
            format!("GRANT TRAVERSE ON GRAPH {SURVIVOR_GRAPH} EDGES KNOWS TO {self_survivor}"),
            "cascade-survivor-traverse",
        ),
    ] {
        gql_mutate_as_admin(&env, &statement, key);
    }

    // Prepared query bound to the home graph (the dropped one), published to the grantee:
    // invariant 7 passes because the publishing admin is the registry owner (implicit root).
    prepare_as_admin(&env, "cascade_q", "MATCH (n:Person) RETURN n");
    gleaph_pocket_ic_tests::publish_prepared_query_as(
        &env,
        env.admin,
        "cascade_q",
        &self_grantee,
        "cascade-publish-execute",
    )
    .expect("owner publishes the prepared query to the grantee");

    // Baseline introspection: each listing leads with the marker and carries exactly its
    // own graph's rows.
    let dropped_before =
        list_grants(&env, DROPPED_GRAPH).expect("owner lists dropped-graph grants");
    assert_eq!(
        dropped_before.len(),
        4,
        "marker plus MATCH/READ/TRAVERSE rows, got {dropped_before:?}"
    );
    expect_implicit_root_marker(&dropped_before[0], DROPPED_GRAPH, &env);
    let survivor_before =
        list_grants(&env, SURVIVOR_GRAPH).expect("owner lists survivor-graph grants");
    assert_eq!(
        survivor_before.len(),
        3,
        "marker plus MATCH/TRAVERSE rows, got {survivor_before:?}"
    );
    expect_implicit_root_marker(&survivor_before[0], SURVIVOR_GRAPH, &env);
    assert_eq!(
        survivor_before[1].operation,
        GrantOperationView::Match,
        "survivor MATCH row"
    );
    assert_eq!(
        survivor_before[1].subject,
        GrantSubjectView::Principal(survivor_subject().to_text())
    );
    assert_eq!(
        survivor_before[1].resource.kind,
        GrantResourceKindView::Vertex
    );
    assert_eq!(survivor_before[1].resource.label, "Person");
    assert_eq!(survivor_before[2].operation, GrantOperationView::Traverse);
    // Open-schema KNOWS has undeclared directedness: only the unoriented form is grantable.
    assert_eq!(survivor_before[2].direction, None);
    assert_eq!(
        survivor_before[2].resource.kind,
        GrantResourceKindView::Edge
    );
    assert_eq!(survivor_before[2].resource.label, "KNOWS");

    // The formerly-granted caller can execute the labeled pattern today.
    prepared_query_result(&env, grantee(), "cascade_q")
        .unwrap_or_else(|err| panic!("pre-teardown prepared execution must be admitted: {err:?}"));

    // ── Teardown through the real path ──
    unregister_shard(&env, DROPPED_GRAPH);
    unregister_graph(&env, DROPPED_GRAPH);

    // The dropped graph's listing is gone with its registry entry (ADR 0028 NotFound).
    let dropped_after = list_grants(&env, DROPPED_GRAPH)
        .expect_err("unregistered graph must not answer grant listings");
    assert!(
        matches!(dropped_after, RouterError::NotFound(_)),
        "expected NotFound after teardown, got {dropped_after:?}"
    );

    // The sibling graph's listing is byte-for-byte unchanged: its rows survived the sweep
    // even though they reuse the dropped graph's numeric label ids and differ only in the
    // graph discriminator and subject — exact-key cascade evidence.
    let survivor_after = list_grants(&env, SURVIVOR_GRAPH).expect("survivor listing");
    assert_eq!(
        survivor_after, survivor_before,
        "unrelated graphs' rows must survive untouched"
    );

    // The formerly-granted caller lost access: the prepared query stays uniformly denied.
    // The EXECUTE row itself is name-keyed and survives the sweep, so admission passes and
    // the denial comes from requirement coverage: swept data-plane rows cannot cover the
    // stored set, and dead ids can never be re-covered (monotonic catalogs).
    let prepared_after = prepared_query_result(&env, grantee(), "cascade_q")
        .expect_err("prepared query over dropped vocabulary must stay denied");
    assert!(
        matches!(prepared_after, RouterError::Forbidden),
        "expected uniform Forbidden, got {prepared_after:?}"
    );

    // Ad-hoc access is gone with the graph itself (ADR 0028 indistinguishable NotFound).
    let adhoc_after = gql_query_result(&env, grantee(), "MATCH (n:Person) RETURN n")
        .expect_err("formerly-granted caller must lose access to the dropped pattern");
    assert!(
        matches!(adhoc_after, RouterError::NotFound(_)),
        "expected NotFound after teardown, got {adhoc_after:?}"
    );
}
