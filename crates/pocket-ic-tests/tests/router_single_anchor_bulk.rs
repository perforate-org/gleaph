//! PocketIC: ADR 0046 single-variable anchored mutation bulk admission.
//!
//! Verifies that selective single-variable seeds (e.g. `User.user_id`) are resolved
//! independently for each batch item and executed through the existing Router-to-Graph
//! bulk boundary, producing one group mutation-journal entry rather than one per item.

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_gql_ic::encode_gql_params_blob;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    admin_intern_property, create_vertex_property_index, gql_execute_idempotent_as_admin,
    gql_query_as_admin, graph_mutation_journal_len, install_single_shard_federation,
};
use gleaph_router::types::{GqlExecuteIdempotentBatchArgs, GqlExecuteIdempotentBatchItem};

const USER_ALICE: &str = "INSERT (:User {user_id: 'alice', demo_graph: 'social'})";
const USER_BOB: &str = "INSERT (:User {user_id: 'bob', demo_graph: 'social'})";

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
        Result<gleaph_router::types::GqlExecuteIdempotentBatchResult, RouterError>
    ) {
        Ok(Ok(result)) => result.results,
        Ok(Err(err)) => panic!("gql_execute_idempotent_batch rejected: {err:?}"),
        Err(err) => panic!("decode gql_execute_idempotent_batch: {err}"),
    }
}

#[test]
fn single_variable_anchor_bulk_resolves_per_item_seed_and_creates_one_journal_entry() {
    let env = install_single_shard_federation();

    // Ensure all properties used in the test are interned.
    admin_intern_property(&env, "demo_graph");
    admin_intern_property(&env, "demo_id");

    create_vertex_property_index(
        &env,
        "user_user_id",
        "User",
        "user_id",
        "single_anchor_create_user_id_index",
    );

    // Seed two distinct users.
    gql_execute_idempotent_as_admin(&env, USER_ALICE, "single_anchor_insert_alice");
    gql_execute_idempotent_as_admin(&env, USER_BOB, "single_anchor_insert_bob");

    let journal_before = graph_mutation_journal_len(&env, env.graph_source);

    // Two POST insertions anchored on different users through the same GQL plan.
    let mutations = vec![
        GqlExecuteIdempotentBatchItem {
            gql_query: POST_GQL.to_string(),
            params: params_blob(vec![
                ("$a_user_id", Value::Text("alice".to_string())),
                ("$b_demo_id", Value::Uint64(100)),
                ("$edge_id", Value::Text("p1".to_string())),
            ]),
            mutation_key: "single_anchor_post_alice".to_string(),
        },
        GqlExecuteIdempotentBatchItem {
            gql_query: POST_GQL.to_string(),
            params: params_blob(vec![
                ("$a_user_id", Value::Text("bob".to_string())),
                ("$b_demo_id", Value::Uint64(101)),
                ("$edge_id", Value::Text("p2".to_string())),
            ]),
            mutation_key: "single_anchor_post_bob".to_string(),
        },
    ];

    let results = execute_batch_as_admin(&env, mutations);
    assert_eq!(
        results.len(),
        2,
        "bulk batch must return one result per item"
    );

    let journal_after = graph_mutation_journal_len(&env, env.graph_source);
    assert_eq!(
        journal_after,
        journal_before + 1,
        "bulk group must create exactly one graph mutation-journal entry"
    );

    // Verify exact author/post relations and that item order is preserved.
    let posts = gql_query_as_admin(&env, POSTS_AND_AUTHORS);
    assert_eq!(posts.row_count, 2, "two POSTED edges should exist");
    let rows = decode_rows(&posts);
    assert_eq!(
        rows[0].get("post_id"),
        Some(&Value::Uint64(100)),
        "first post id"
    );
    assert_eq!(
        rows[0].get("author"),
        Some(&Value::Text("alice".to_string())),
        "alice authored first post"
    );
    assert_eq!(
        rows[1].get("post_id"),
        Some(&Value::Uint64(101)),
        "second post id"
    );
    assert_eq!(
        rows[1].get("author"),
        Some(&Value::Text("bob".to_string())),
        "bob authored second post"
    );
}
