//! PocketIC: Router typed seed batch activation (ADR 0047).
//!
//! Verifies the per-shard capability contract end-to-end on one Router and one
//! single-shard Graph/Index installation:
//!
//! - capability is false by default;
//! - admin refresh commits the Graph-advertised bit only while the registered
//!   principal identity is unchanged;
//! - capability-enabled eligible groups execute through the typed endpoint and
//!   produce ordered canonical results;
//! - a typed bulk group is durable: retry with the same client key does not
//!   duplicate canonical effects;
//! - admin clear disables only new typed admission; existing typed records
//!   continue to resolve through the typed path on retry.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql_ic::encode_gql_params_blob;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    admin_intern_edge_label, admin_intern_property, admin_intern_vertex_label, arm_router_fault,
    create_vertex_property_index, gql_execute_idempotent_as_admin, install_single_shard_federation,
};
use gleaph_router::types::{
    GqlExecuteIdempotentBatchArgs, GqlExecuteIdempotentBatchItem, GqlExecuteIdempotentBatchResult,
};

const GRAPH_NAME: &str = gleaph_pocket_ic_tests::GRAPH_NAME;
const SOURCE_SHARD: gleaph_graph_kernel::federation::ShardId =
    gleaph_graph_kernel::federation::ShardId::new(0);

const USER_ALICE: &str = "INSERT (:User {user_id: 'alice', demo_graph: 'social'})";
const USER_BOB: &str = "INSERT (:User {user_id: 'bob', demo_graph: 'social'})";
const USER_CAROL: &str = "INSERT (:User {user_id: 'carol', demo_graph: 'social'})";

const POST_GQL: &str = "\
MATCH (a:User {user_id: $a_user_id, demo_graph: 'social'}) RETURN a \
NEXT \
INSERT (a)-[:POSTED {demo_edge_id: $edge_id}]->(b:Post {demo_id: $b_demo_id, demo_graph: 'social'})";

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
    let result = gleaph_pocket_ic_tests::gql_query_as_admin(env, POSTS_AND_AUTHORS);
    decode_rows(&result).len()
}

fn post_mutations(user_ids: &[&str], start_demo_id: u32) -> Vec<GqlExecuteIdempotentBatchItem> {
    user_ids
        .iter()
        .enumerate()
        .map(|(i, user_id)| GqlExecuteIdempotentBatchItem {
            gql_query: POST_GQL.to_string(),
            mutation_key: format!("typed-activation-post-{}", start_demo_id + i as u32),
            params: params_blob(vec![
                ("$a_user_id", Value::Text(user_id.to_string())),
                (
                    "$edge_id",
                    Value::Text(format!("edge-{}", start_demo_id + i as u32)),
                ),
                (
                    "$b_demo_id",
                    Value::Text(format!("post-{}", start_demo_id + i as u32)),
                ),
            ]),
        })
        .collect()
}

fn refresh_capability(env: &gleaph_pocket_ic_tests::FederationEnv) -> bool {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_refresh_shard_execution_capabilities",
            Encode!(&GRAPH_NAME.to_string(), &SOURCE_SHARD.raw()).expect("encode refresh"),
        )
        .unwrap_or_else(|e| panic!("admin_refresh_shard_execution_capabilities: {e:?}"));
    let result: Result<bool, RouterError> =
        Decode!(&bytes, Result<bool, RouterError>).expect("decode refresh result");
    result.unwrap_or_else(|e| panic!("refresh rejected: {e:?}"))
}

fn clear_capability(env: &gleaph_pocket_ic_tests::FederationEnv) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_clear_shard_execution_capabilities",
            Encode!(&GRAPH_NAME.to_string(), &SOURCE_SHARD.raw()).expect("encode clear"),
        )
        .unwrap_or_else(|e| panic!("admin_clear_shard_execution_capabilities: {e:?}"));
    let result: Result<(), RouterError> =
        Decode!(&bytes, Result<(), RouterError>).expect("decode clear result");
    result.unwrap_or_else(|e| panic!("clear rejected: {e:?}"))
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

#[test]
fn typed_batch_activation_lifecycle_drives_capability_fallback_and_retry() {
    let env = install_single_shard_federation();

    // Prepare the graph schema.
    admin_intern_property(&env, "demo_graph");
    admin_intern_property(&env, "demo_id");
    admin_intern_property(&env, "user_id");
    admin_intern_property(&env, "demo_edge_id");
    admin_intern_vertex_label(&env, "User");
    admin_intern_vertex_label(&env, "Post");
    admin_intern_edge_label(&env, "POSTED");
    create_vertex_property_index(
        &env,
        "user_user_id",
        "User",
        "user_id",
        "typed_activation_user_id_index",
    );

    // Seed users.
    gql_execute_idempotent_as_admin(&env, USER_ALICE, "typed_activation_insert_alice");
    gql_execute_idempotent_as_admin(&env, USER_BOB, "typed_activation_insert_bob");
    gql_execute_idempotent_as_admin(&env, USER_CAROL, "typed_activation_insert_carol");

    // 1. Graph advertises typed V1 support only to the Router guard; an admin caller
    //    is rejected.
    let raw_bytes = env
        .pic
        .query_call(
            env.graph_source,
            env.admin,
            "execution_capabilities",
            Encode!(&()).expect("encode execution_capabilities args"),
        )
        .expect_err("admin must be rejected by router guard");
    assert!(
        raw_bytes
            .to_string()
            .contains("is not the configured router canister"),
        "unexpected rejection: {raw_bytes}"
    );

    // 2. With the registry capability in its default (disabled) state, the first batch
    //    falls back to the scalar bulk path and succeeds.
    let fallback_posts = post_mutations(&["alice", "bob"], 1);
    execute_batch_as_admin(&env, fallback_posts.clone());
    assert_eq!(count_posts(&env), 2, "fallback batch must create two posts");
    assert_eq!(typed_batch_trace(&env), "sequential-scalar-fallback");

    // 3. Refresh the capability. The Graph endpoint advertises support, so the bit commits.
    assert!(
        refresh_capability(&env),
        "refresh must commit the Graph-advertised capability"
    );

    // 4. With capability enabled, the next eligible batch executes through the typed endpoint.
    let typed_posts = post_mutations(&["carol", "alice"], 3);
    arm_router_fault(&env, 3);
    let trapped_args = GqlExecuteIdempotentBatchArgs {
        mutations: typed_posts.clone(),
        start_index: 0,
        instruction_budget: None,
    };
    let trapped = env.pic.update_call(
        env.router,
        env.admin,
        "gql_execute_idempotent_batch",
        Encode!(&trapped_args).expect("encode trapped typed batch"),
    );
    assert!(
        trapped.is_err(),
        "fault must trap after the Graph commit; typed trace: {}",
        typed_batch_trace(&env)
    );
    assert_eq!(
        typed_batch_trace(&env),
        "persisted",
        "fault must occur only after durable typed replay admission"
    );
    arm_router_fault(&env, 0);
    assert_eq!(
        count_posts(&env),
        4,
        "Graph canonical writes must survive the Router callback trap"
    );

    let typed_results = execute_batch_as_admin(&env, typed_posts.clone());
    assert_eq!(
        typed_results.len(),
        2,
        "typed batch must return one result per operation"
    );
    assert_eq!(
        typed_results
            .iter()
            .map(|result| result.row_count)
            .collect::<Vec<_>>(),
        vec![0, 0],
        "POSTED update transport reports no materialized result rows"
    );
    assert_eq!(
        count_posts(&env),
        4,
        "typed batch must add exactly two canonical posts"
    );

    // 5. Retry with the same client key is idempotent and does not duplicate canonical effects.
    let retry_results = execute_batch_as_admin(&env, typed_posts.clone());
    assert_eq!(
        retry_results.len(),
        2,
        "retry of a completed typed group must return the same number of results"
    );
    assert_eq!(
        retry_results
            .iter()
            .map(|result| result.row_count)
            .collect::<Vec<_>>(),
        typed_results
            .iter()
            .map(|result| result.row_count)
            .collect::<Vec<_>>(),
        "completed replay must preserve ordered row counts"
    );
    assert_eq!(
        count_posts(&env),
        4,
        "retry must not duplicate canonical posts"
    );

    // 6. Clear the capability. Existing typed records continue to resolve on retry.
    clear_capability(&env);
    let after_clear_retry = execute_batch_as_admin(&env, typed_posts.clone());
    assert_eq!(
        after_clear_retry.len(),
        2,
        "retry of durable typed record must work after capability clear"
    );
    assert_eq!(
        count_posts(&env),
        4,
        "retry after clear must not duplicate canonical posts"
    );

    // 7. New admission after clear falls back to the scalar path.
    let after_clear_posts = post_mutations(&["bob", "carol"], 5);
    execute_batch_as_admin(&env, after_clear_posts);
    assert_eq!(
        count_posts(&env),
        6,
        "post-clear batch must create two more posts via scalar fallback"
    );
    assert_eq!(typed_batch_trace(&env), "sequential-scalar-fallback");
}
