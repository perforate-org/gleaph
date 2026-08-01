//! PocketIC: Router unified bulk execution (ADR 0047).
//!
//! The versionless `execute_plan_update_batch_bulk` envelope is always active — there is no
//! capability gate — so eligible groups dispatch through the Graph bulk path without any admin
//! activation step. This verifies that the social-demo POSTED shape (single-variable complete-row
//! seed) is admitted to the per-item-seed bulk variant and produces ordered canonical results.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql_ic::encode_gql_params_blob;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    admin_intern_edge_label, admin_intern_property, admin_intern_vertex_label,
    create_vertex_property_index, gql_execute_idempotent_as_admin, gql_query_as_admin,
    install_single_shard_federation,
};
use gleaph_router::types::{
    GqlExecuteIdempotentBatchArgs, GqlExecuteIdempotentBatchItem, GqlExecuteIdempotentBatchResult,
};

const USER_ALICE: &str = "INSERT (:User {user_id: 'alice', demo_graph: 'social'})";
const USER_BOB: &str = "INSERT (:User {user_id: 'bob', demo_graph: 'social'})";
const USER_CAROL: &str = "INSERT (:User {user_id: 'carol', demo_graph: 'social'})";

const SOCIAL_POSTED_GQL: &str = "\
MATCH (a:User {user_id: $a_user_id, demo_graph: 'social'}) RETURN a \
NEXT \
INSERT (a)-[:POSTED {demo_edge_id: $edge_id, demo_kind: $demo_kind}]->\
(b:Post {demo_id: $b_demo_id, demo_graph: 'social', body: $b_body, created_at: CURRENT_TIMESTAMP, is_public: $b_is_public})";

const POSTS_AND_AUTHORS: &str = "\
MATCH (u:User)-[:POSTED]->(p:Post) \
WHERE u.demo_graph = 'social' AND p.demo_graph = 'social' \
RETURN u.user_id AS author, p.demo_id AS post_id ORDER BY p.demo_id";

fn params_blob(items: Vec<(&str, Value)>) -> Vec<u8> {
    encode_gql_params_blob(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        .expect("encode params")
}

fn execute_batch_as_admin(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    mutations: Vec<GqlExecuteIdempotentBatchItem>,
) -> Vec<gleaph_graph_kernel::plan_exec::GqlQueryResult> {
    let args = GqlExecuteIdempotentBatchArgs {
        mutations,
        start_index: 0,
        instruction_budget: None,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_execute_idempotent_batch",
            Encode!(&args).expect("encode gql_execute_idempotent_batch"),
        )
        .unwrap_or_else(|e| panic!("gql_execute_idempotent_batch on router: {e:?}"));
    match Decode!(
        &bytes,
        Result<GqlExecuteIdempotentBatchResult, RouterError>
    ) {
        Ok(Ok(result)) => result.results,
        Ok(Err(err)) => panic!("gql_execute_idempotent_batch rejected: {err:?}"),
        Err(err) => panic!("decode gql_execute_idempotent_batch: {err}"),
    }
}

fn decode_rows(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
) -> Vec<std::collections::BTreeMap<String, Value>> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("prepared query result should carry rows_blob");
    gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(rows_blob)
        .expect("decode rows_blob")
        .try_into_value_rows()
        .expect("convert wire rows to value rows")
}

fn count_posts(env: &gleaph_pocket_ic_tests::FederationEnv) -> usize {
    let result = gql_query_as_admin(env, POSTS_AND_AUTHORS);
    decode_rows(&result).len()
}

fn social_posted_params(user_id: &str, edge_id: &str, demo_id: i64, body: &str) -> Vec<u8> {
    params_blob(vec![
        ("$a_user_id", Value::Text(user_id.to_string())),
        ("$edge_id", Value::Text(edge_id.to_string())),
        ("$demo_kind", Value::Text("posted".to_string())),
        ("$b_demo_id", Value::Int64(demo_id)),
        ("$b_body", Value::Text(body.to_string())),
        ("$b_is_public", Value::Bool(true)),
    ])
}

fn social_posted_mutations(
    user_ids: &[&str],
    start_demo_id: i32,
) -> Vec<GqlExecuteIdempotentBatchItem> {
    user_ids
        .iter()
        .enumerate()
        .map(|(i, user_id)| GqlExecuteIdempotentBatchItem {
            gql_query: SOCIAL_POSTED_GQL.to_string(),
            mutation_key: format!("social-typed-post-{}-{}", user_id, start_demo_id + i as i32),
            params: social_posted_params(
                user_id,
                &format!("edge-{}", start_demo_id + i as i32),
                (start_demo_id + i as i32) as i64,
                &format!("body {}", start_demo_id + i as i32),
            ),
        })
        .collect()
}

#[test]
fn social_demo_posted_shape_uses_unified_bulk_path() {
    let env = install_single_shard_federation();

    admin_intern_property(&env, "demo_graph");
    admin_intern_property(&env, "demo_id");
    admin_intern_property(&env, "user_id");
    admin_intern_property(&env, "demo_edge_id");
    admin_intern_property(&env, "demo_kind");
    admin_intern_property(&env, "body");
    admin_intern_property(&env, "created_at");
    admin_intern_property(&env, "is_public");
    admin_intern_vertex_label(&env, "User");
    admin_intern_vertex_label(&env, "Post");
    admin_intern_edge_label(&env, "POSTED");
    create_vertex_property_index(
        &env,
        "social_user_user_id",
        "User",
        "user_id",
        "social_typed_user_id_index",
    );
    create_vertex_property_index(
        &env,
        "social_post_demo_id",
        "Post",
        "demo_id",
        "social_typed_post_demo_id_index",
    );

    gql_execute_idempotent_as_admin(&env, USER_ALICE, "social_insert_alice");
    gql_execute_idempotent_as_admin(&env, USER_BOB, "social_insert_bob");
    gql_execute_idempotent_as_admin(&env, USER_CAROL, "social_insert_carol");

    // The unified bulk path is always active: no admin capability activation is needed.
    let posts = social_posted_mutations(&["alice", "bob"], 1);
    let results = execute_batch_as_admin(&env, posts);
    assert_eq!(
        results.len(),
        2,
        "social-demo POSTED batch must return two results"
    );
    assert_eq!(
        results.iter().map(|r| r.row_count).collect::<Vec<_>>(),
        vec![0, 0],
        "POSTED update transport reports no materialized result rows"
    );
    assert_eq!(
        count_posts(&env),
        2,
        "social-demo POSTED shape must create two canonical posts"
    );
    assert_eq!(
        typed_batch_trace(&env),
        "persisted",
        "social-demo POSTED shape must be admitted to the unified bulk path"
    );
    assert_eq!(
        typed_batch_prepare_count(&env),
        1,
        "unified bulk path must enter typed prepare exactly once"
    );
}

fn typed_batch_trace(env: &gleaph_pocket_ic_tests::FederationEnv) -> String {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "test_typed_batch_trace",
            Encode!(&()).expect("encode typed trace args"),
        )
        .expect("query typed trace");
    Decode!(&bytes, Result<String, RouterError>)
        .expect("decode typed trace")
        .expect("typed trace authorized")
}

fn typed_batch_prepare_count(env: &gleaph_pocket_ic_tests::FederationEnv) -> u64 {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "test_typed_batch_prepare_count",
            Encode!(&()).expect("encode typed prepare count args"),
        )
        .expect("query typed prepare count");
    Decode!(&bytes, Result<u64, RouterError>)
        .expect("decode typed prepare count")
        .expect("typed prepare count authorized")
}
