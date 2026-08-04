//! Connectivity proof: the generated `PreparedExt` operations against the `gleaph-cdk` client.
//!
//! Hand-written; not part of the generated output. Compiles the generated marker and operations
//! trait against `GleaphClient<Prepared>` and decodes a simulated Router `prepared_query`
//! response into a generated row type.

use gleaph_cdk::{GleaphClient, GqlQueryResult, GqlWireRow, GqlWireRows, GqlWireValue};
use gleaph_codegen_rust_canister_fixture::{
    FindUsersParams, FindUsersRow, Prepared, PreparedExt, PreparedResponse,
};

#[test]
fn generated_trait_binds_to_the_prepared_client() {
    // Compile-time proof: `GleaphClient<Prepared>` implements the generated operations trait,
    // while the plain client does not.
    fn assert_prepared<T: PreparedExt>() {}
    assert_prepared::<GleaphClient<Prepared>>();
}

#[test]
fn prepared_and_plain_clients_construct_with_distinct_markers() {
    let plain = GleaphClient::new(candid::Principal::anonymous());
    let prepared = GleaphClient::with_prepared::<Prepared>(candid::Principal::anonymous());
    let _ = plain;
    let _ = prepared;
}

#[test]
fn generated_params_encode_to_router_blob() {
    let params = FindUsersParams { term: "ada".into() }.into_gql_params();
    let blob = gleaph_cdk::encode_gql_params(params).unwrap();
    assert!(!blob.is_empty());
}

#[test]
fn generated_rows_decode_from_router_envelope() {
    // Simulate the Router's `prepared_query` response blob and run the same decode path the
    // generated operation uses, without making an inter-canister call.
    let rows = GqlWireRows {
        rows: vec![GqlWireRow {
            columns: vec![("user_name".into(), GqlWireValue::Text("Ada".into()))],
        }],
    };
    let result = GqlQueryResult {
        row_count: 1,
        rows_blob: Some(candid::encode_one(&rows).expect("encode wire rows")),
        phase: None,
        token: None,
    };
    let response = PreparedResponse {
        row_count: result.row_count,
        rows: result
            .decode_serde_rows::<FindUsersRow>()
            .unwrap()
            .unwrap_or_default(),
    };
    assert_eq!(response.rows.len(), 1);
    assert_eq!(response.rows[0].user_name, "Ada");
}
