//! Cross-crate usability of the `gql!` macro and `GleaphClient` through the `gleaph-cdk`
//! facade. Compiled as a separate crate, like a downstream application canister.

use candid::Principal;
use gleaph_cdk::gql;
use gleaph_cdk::{CallError, GleaphClient, GqlQueryResult, GqlValue, ReadMode};

#[test]
fn gql_macro_is_usable_through_the_cdk_facade() {
    let principal = Principal::from_text("aaaaa-aa").unwrap();
    let query = gql!(
        "MATCH (n:Person {owner: $owner}) RETURN n.name",
        owner = principal
    );
    assert_eq!(query.params[0].0, "owner");
    assert!(matches!(query.params[0].1, GqlValue::Extension(_)));
}

#[test]
fn client_methods_are_awaitable_from_an_external_crate() {
    fn assert_future<T>(_: &impl core::future::Future<Output = Result<T, CallError>>) {}

    let client = GleaphClient::new(Principal::anonymous());
    assert_future(&client.gql_query("RETURN 1"));
    assert_future(&client.gql_query(gql!("RETURN $x", x = 1i64)));
    assert_future(&client.gql_query_with_mode(gql!("RETURN $x", x = 1i64), ReadMode::Eventual));
    assert_future(&client.gql_mutate(gql!("CREATE (n:Person {id: $id})", id = 1i64), "request-1"));
    assert_future(&client.prepared_query("find-users", vec![1, 2, 3], None, ReadMode::Eventual));
    assert_future(&client.prepared_mutate("increment", vec![1, 2, 3], "request-1"));
    assert_future(&client.bulk_load(gleaph_cdk::BulkLoadCommand::Start {
        graph_name: None,
        client_bulk_key: "bulk-1".into(),
    }));
    assert_future(&client.bulk_load_status(None, "bulk-1", None, 16));
    assert_future(&client.prepare(vec![gleaph_cdk::PreparedRegistration {
        name: "find-users".into(),
        query: "MATCH (n) RETURN n".into(),
        metadata: None,
    }]));
    assert_future(&client.drop_prepared("find-users"));
    assert_future(&client.list_prepared(None));
}

#[test]
fn gql_query_result_envelope_decodes_without_rows() {
    let response = GqlQueryResult {
        row_count: 0,
        rows_blob: None,
        phase: None,
        token: None,
    };
    assert!(response.decode_rows().unwrap().is_none());
}
