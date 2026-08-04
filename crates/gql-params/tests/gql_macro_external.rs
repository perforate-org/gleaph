//! Cross-crate usability tests for `gleaph-gql-params`.
//!
//! This file is compiled as a separate crate, exercising the exported `gql!` macro,
//! `IntoGqlParam` trait, and query types exactly as a downstream dependency would.

use candid::Principal;
use gleaph_gql_params::{
    GqlParams, GqlQuery, GqlValue, IntoGqlParam, encode_gql_params, gql, gql_param_value,
};

#[test]
fn gql_macro_builds_explicit_named_params() {
    let query = gql!("MATCH (n:Person {id: $id}) RETURN n.name", id = 42u64);
    assert_eq!(query.query, "MATCH (n:Person {id: $id}) RETURN n.name");
    assert_eq!(query.params.len(), 1);
    assert_eq!(query.params[0].0, "id");
    assert!(matches!(query.params[0].1, GqlValue::Uint64(42)));
}

#[test]
fn gql_macro_infers_param_name_from_identifier() {
    let user_principal = Principal::from_text("aaaaa-aa").unwrap();
    let query = gql!(
        "MATCH (n:Person {owner: $user_principal}) RETURN n.name",
        user_principal,
    );
    assert_eq!(query.params[0].0, "user_principal");
    assert!(matches!(query.params[0].1, GqlValue::Extension(_)));
}

#[test]
fn gql_macro_accepts_no_params_and_trailing_comma() {
    assert!(gql!("RETURN 1").params.is_empty());
    assert!(gql!("RETURN 1",).params.is_empty());
    assert!(matches!(
        gql!("RETURN $flag", flag = true,).params[0].1,
        GqlValue::Bool(true)
    ));
}

#[test]
fn principal_params_encode_as_short_blob_extensions() {
    let principal = Principal::from_text("aaaaa-aa").unwrap();
    let query = gql!("MATCH (n {owner: $owner}) RETURN n", owner = principal);
    let bytes = encode_gql_params(query.params).unwrap();
    // The principal extension encodes as tag 34 (u8 length + principal bytes).
    assert!(bytes.contains(&34));
}

#[test]
fn trait_works_in_generic_bounds() {
    fn as_gql_param<T: IntoGqlParam>(value: T) -> GqlValue {
        value.into_gql_param()
    }
    assert!(matches!(as_gql_param(7i32), GqlValue::Int32(7)));
    assert!(matches!(
        as_gql_param("hello"),
        GqlValue::Text(value) if value == "hello"
    ));
    assert!(matches!(as_gql_param(None::<i64>), GqlValue::Null));
    assert!(matches!(
        as_gql_param(Principal::anonymous()),
        GqlValue::Extension(_)
    ));
}

#[test]
fn custom_types_implement_into_gql_param_directly() {
    struct UserId(u64);
    impl IntoGqlParam for UserId {
        fn into_gql_param(self) -> GqlValue {
            GqlValue::Uint64(self.0)
        }
    }
    let query = gql!("MATCH (n {id: $id}) RETURN n", id = UserId(7));
    assert!(matches!(query.params[0].1, GqlValue::Uint64(7)));
    assert!(matches!(gql_param_value(UserId(9)), GqlValue::Uint64(9)));
}

#[test]
fn plain_query_strings_convert_into_gql_query() {
    let query = GqlQuery::from("MATCH (n) RETURN n");
    assert_eq!(query.query, "MATCH (n) RETURN n");
    assert!(query.params.is_empty());
}

#[test]
fn gql_params_type_is_usable() {
    let params: GqlParams = vec![("x".to_string(), GqlValue::Int8(-1))];
    assert_eq!(params[0].0, "x");
}
