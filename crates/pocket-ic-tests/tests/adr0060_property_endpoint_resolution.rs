//! PocketIC contracts for ADR 0060 property-based bulk-load edge endpoints.
//!
//! `BulkLoadEdgeV1` endpoints may reference existing vertices by `{ label, property, value }`;
//! the Router resolves them through the converged graph property index during Append and rejects
//! the whole candidate chunk before any operation executes when an endpoint is missing or
//! non-unique.

use gleaph_gql::value::Value;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, advance_backfill_as_admin, bulk_load_as_admin,
    bulk_load_status_as_admin, ensure_property, ensure_vertex_label, gql_query_as_admin,
    index_vertex_property, install_single_shard_federation,
};
use gleaph_router::types::{
    AtomicInsertPropertyV1, AtomicInsertVertexV1, BackfillKind, BulkLoadChunkV1, BulkLoadCommand,
    BulkLoadEdgeV1, BulkLoadEndpointV1, BulkLoadPropertyEndpointV1, BulkLoadPublicStateV1,
    BulkLoadStatusPage,
};

fn graph(name: &str) -> Option<String> {
    Some(name.to_owned())
}

fn key(kind: &str) -> String {
    format!("adr0060-property-endpoint-{kind}")
}

fn text_property(name: &str, value: &str) -> AtomicInsertPropertyV1 {
    AtomicInsertPropertyV1 {
        property_name: name.to_owned(),
        value: Value::Text(value.to_owned())
            .to_binary_bytes()
            .expect("encode text property"),
    }
}

fn by_property(value: &str) -> BulkLoadEndpointV1 {
    let index_key = value_to_index_key_bytes(&Value::Text(value.to_owned()))
        .expect("encode endpoint value")
        .expect("text is indexable");
    BulkLoadEndpointV1::ByProperty(BulkLoadPropertyEndpointV1 {
        vertex_label: "Person".to_owned(),
        property_name: "email".to_owned(),
        value: index_key,
    })
}

fn start(kind: &str) -> BulkLoadCommand {
    BulkLoadCommand::Start {
        graph_name: graph(GRAPH_NAME),
        client_bulk_key: key(kind),
    }
}

fn append(kind: &str, chunk_index: u32, chunk: BulkLoadChunkV1) -> BulkLoadCommand {
    BulkLoadCommand::Append {
        graph_name: graph(GRAPH_NAME),
        client_bulk_key: key(kind),
        chunk_index,
        chunk,
    }
}

fn finalize(kind: &str) -> BulkLoadCommand {
    BulkLoadCommand::Finalize {
        graph_name: graph(GRAPH_NAME),
        client_bulk_key: key(kind),
    }
}

fn vertex(email: &str) -> AtomicInsertVertexV1 {
    AtomicInsertVertexV1 {
        vertex_labels: vec!["Person".to_owned()],
        initial_properties: vec![text_property("email", email)],
    }
}

fn drive_to_completed(env: &FederationEnv, kind: &str, chunks: Vec<BulkLoadChunkV1>) {
    bulk_load_as_admin(env, start(kind)).expect("start");
    for (chunk_index, chunk) in chunks.into_iter().enumerate() {
        let chunk_index = chunk_index as u32;
        bulk_load_as_admin(env, append(kind, chunk_index, chunk)).expect("append");
    }
    bulk_load_as_admin(env, finalize(kind)).expect("finalize");
    let status = bulk_load_status_as_admin(env, GRAPH_NAME, &key(kind), None, 64).expect("status");
    assert_eq!(status.state, BulkLoadPublicStateV1::Completed, "{status:?}");
}

fn setup_indexed_graph(env: &FederationEnv) {
    ensure_vertex_label(env, "Person");
    ensure_property(env, "email");
    index_vertex_property(env, "Person", "email");
}

/// Run bounded vertex-property backfill until every shard reports `done`. The admin-compat
/// index is published Active immediately, but postings are populated by the backfill pass.
fn converge_property_postings(env: &FederationEnv) {
    loop {
        let result = advance_backfill_as_admin(env, BackfillKind::VertexProperty, 64);
        if result.all_done {
            break;
        }
    }
}

#[test]
fn bulk_load_resolves_property_endpoints_through_the_property_index() {
    let env = install_single_shard_federation();
    setup_indexed_graph(&env);

    // Load two indexed vertices first; the property index is converged before the load so the
    // ordered vertex batches push postings, then the backfill pass syncs the postings.
    drive_to_completed(
        &env,
        "happy-vertices",
        vec![BulkLoadChunkV1::Vertices(vec![
            vertex("a@b.c"),
            vertex("c@d.e"),
        ])],
    );
    converge_property_postings(&env);

    // Edge chunk referencing both endpoints by property.
    drive_to_completed(
        &env,
        "happy-edges",
        vec![BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
            source: by_property("a@b.c"),
            target: by_property("c@d.e"),
            directed: true,
            edge_label_name: Some("KNOWS".to_owned()),
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }])],
    );

    let result = gql_query_as_admin(
        &env,
        "MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN a, e, b",
    );
    assert_eq!(
        result.row_count, 1,
        "property-resolved edge must be committed"
    );
}

#[test]
fn bulk_load_property_resolution_rejects_missing_and_non_unique_without_commit() {
    let env = install_single_shard_federation();
    setup_indexed_graph(&env);

    // Two vertices with a shared email make that value non-unique; one value is never loaded.
    drive_to_completed(
        &env,
        "reject-vertices",
        vec![BulkLoadChunkV1::Vertices(vec![
            vertex("dup@example.com"),
            vertex("dup@example.com"),
        ])],
    );
    converge_property_postings(&env);

    bulk_load_as_admin(&env, start("reject-missing")).expect("start reject-missing");
    let missing = bulk_load_as_admin(
        &env,
        append(
            "reject-missing",
            0,
            BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
                source: by_property("missing@example.com"),
                target: by_property("dup@example.com"),
                directed: true,
                edge_label_name: Some("KNOWS".to_owned()),
                inline_property: None,
                initial_edge_properties: Vec::new(),
            }]),
        ),
    )
    .expect_err("missing endpoint value must reject the whole chunk");
    let RouterError::InvalidArgument(missing_message) = &missing else {
        panic!("missing endpoint value must reject with InvalidArgument: {missing:?}");
    };
    assert!(
        missing_message.contains("does not resolve"),
        "{missing_message}"
    );

    bulk_load_as_admin(&env, start("reject-non-unique")).expect("start reject-non-unique");
    let non_unique = bulk_load_as_admin(
        &env,
        append(
            "reject-non-unique",
            0,
            BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
                source: by_property("dup@example.com"),
                target: by_property("dup@example.com"),
                directed: true,
                edge_label_name: Some("KNOWS".to_owned()),
                inline_property: None,
                initial_edge_properties: Vec::new(),
            }]),
        ),
    )
    .expect_err("non-unique endpoint value must reject the whole chunk");
    let RouterError::InvalidArgument(non_unique_message) = &non_unique else {
        panic!("non-unique endpoint value must reject with InvalidArgument: {non_unique:?}");
    };
    assert!(
        non_unique_message.contains("multiple vertices"),
        "{non_unique_message}"
    );

    // The rejected chunks were never admitted: each job is still Open with no committed receipt.
    for kind in ["reject-missing", "reject-non-unique"] {
        let status: BulkLoadStatusPage =
            bulk_load_status_as_admin(&env, GRAPH_NAME, &key(kind), None, 64).expect("status");
        assert_eq!(
            status.state,
            BulkLoadPublicStateV1::Open,
            "{kind}: {status:?}"
        );
        assert_eq!(status.next_chunk_index, 0, "{kind} must stay at chunk 0");
        assert!(status.receipts.is_empty(), "{kind} must have no receipts");
    }
}

#[test]
fn bulk_load_property_resolution_requires_a_converged_property_index() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_property(&env, "email");
    // Deliberately no `index_vertex_property` call: the label/property exist but the index does
    // not, so the resolution pre-pass must fail closed before any dispatch.

    bulk_load_as_admin(&env, start("reject-no-index")).expect("start reject-no-index");
    let rejected = bulk_load_as_admin(
        &env,
        append(
            "reject-no-index",
            0,
            BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
                source: by_property("a@b.c"),
                target: by_property("c@d.e"),
                directed: true,
                edge_label_name: Some("KNOWS".to_owned()),
                inline_property: None,
                initial_edge_properties: Vec::new(),
            }]),
        ),
    )
    .expect_err("missing property index must reject the chunk");
    let RouterError::InvalidArgument(rejected_message) = &rejected else {
        panic!("missing index must reject with InvalidArgument: {rejected:?}");
    };
    assert!(
        rejected_message.contains("property index"),
        "{rejected_message}"
    );
}
