//! PocketIC: social demo public-read Gateway contract.
//!
//! Seeds the canonical social demo graph through Router `gql_execute_idempotent` and proves that
//! an anonymous browser caller can execute only the fixed social-demo scenarios through the
//! application-owned Gateway, while neither arbitrary ad-hoc `gql_query` nor arbitrary prepared
//! names or parameters are expressible through the Gateway.
//!
//! The deterministic Post embeddings enter Graph canonical state through Router
//! `admin_ingest_vertex_embedding`; the derived vector index is hydrated through that same Router
//! boundary, not by direct vector-canister seeding.
//!
//! The Gateway principal is registered as a graph administrator so Router can resolve the
//! prepared plan, but it remains a default Router Executor with no ad-hoc `Read` role. Router sees
//! the Gateway principal, not the original anonymous caller, because the Gateway makes a composite
//! inter-canister call.

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql_ic::IcWirePlanQueryResult;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_pocket_ic_tests::{
    admin_fully_activate_social_vector_index, admin_ingest_social_embeddings,
    admin_intern_edge_label, admin_intern_property, admin_intern_vertex_label,
    execute_social_demo_scenario_as, gql_query_as, install_single_shard_federation_with_gateway,
    install_vector_canister, prepared_register_as_admin,
    seed_social_graph_and_assert_feed_edge_order, social_feed_post_ids,
};
use gleaph_social_demo_gateway::SocialDemoScenario;

const PUBLIC_TIMELINE_QUERY: &str = "\
MATCH (feed:Feed {name: 'Public feed'})<-[e:IN_PUBLIC_FEED]-(p:Post)<-[:POSTED]-(author:User) \
OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) \
RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at \
ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 20";

const ALICE_HOME_FEED_QUERY: &str = "\
MATCH (u:User {user_id: 'alice'})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) \
WHERE p.is_public = TRUE \
OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) \
RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at \
ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 20";

const TOPIC_PATH_QUERY: &str = "\
MATCH (p:Post)-[has_topic:HAS_TOPIC]->(t:Topic) \
    WHERE t.name = 'Graph databases' \
MATCH (u:User)-[follows:FOLLOWS]->(author:User)-[posted:POSTED]->(p) \
WHERE u.user_id = 'alice' \
RETURN p.demo_id AS post_id, \
       author.name AS author_name, \
       t.demo_id AS topic_id, \
       p.body AS body, \
       p.created_at AS created_at";

const SEMANTIC_EMBEDDING_NAME: &str = "post_vec";
const SEMANTIC_INDEX_ID: u32 = 1;
const SEMANTIC_DIMS: u16 = 8;

const SEMANTIC_DISCOVERY_QUERY: &str = "MATCH (p:Post)<-[:POSTED]-(author:User) WHERE p.is_public = TRUE SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, distance ORDER BY distance ASC";
const ALICE_SEMANTIC_FEED_QUERY: &str = "MATCH (u:User)-[:FOLLOWS]->(author:User)-[:POSTED]->(p:Post) WHERE u.user_id = 'alice' AND p.is_public = TRUE SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, distance ORDER BY distance ASC";

const SOCIAL_SEEDS_JSON: &str =
    include_str!("../../../frontend/apps/knowledge-map/seeds/social-seeds.json");

#[test]
fn social_graph_demo_gateway_contract() {
    // A default-Executor principal that is NOT graph-visible must remain fail-closed for ad-hoc GQL.
    let default_executor = Principal::from_slice(&[0xCD; 29]);

    let (env, gateway) = install_single_shard_federation_with_gateway();

    intern_social_schema(&env);
    seed_social_graph_and_assert_feed_edge_order(&env);

    prepared_register_as_admin(&env, "public_timeline", PUBLIC_TIMELINE_QUERY);
    prepared_register_as_admin(&env, "alice_home_feed", ALICE_HOME_FEED_QUERY);
    prepared_register_as_admin(&env, "topic_path_explanation", TOPIC_PATH_QUERY);

    assert_public_timeline_through_gateway(&env, gateway);
    assert_alice_home_feed_through_gateway(&env, gateway);
    assert_topic_path_explanation_through_gateway(&env, gateway);

    ingest_social_embeddings_through_router(
        &env,
        SEMANTIC_INDEX_ID,
        SEMANTIC_EMBEDDING_NAME,
        SEMANTIC_DIMS,
    );
    prepared_register_as_admin(&env, "semantic_discovery", SEMANTIC_DISCOVERY_QUERY);
    prepared_register_as_admin(&env, "alice_semantic_feed", ALICE_SEMANTIC_FEED_QUERY);

    assert_semantic_discovery_through_gateway(&env, gateway);
    assert_alice_semantic_feed_through_gateway(&env, gateway);

    assert_ad_hoc_gql_fail_closed(&env, default_executor);
}

fn ingest_social_embeddings_through_router(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    index_id: u32,
    embedding_name: &str,
    dims: u16,
) {
    let manifest: serde_json::Value =
        serde_json::from_str(SOCIAL_SEEDS_JSON).expect("parse social seeds manifest");
    let embeddings = &manifest["embeddings"];
    assert!(
        embeddings.is_object(),
        "generated social seeds must carry deterministic Post embeddings"
    );

    let vector = install_vector_canister(&env.pic, env.router);
    admin_fully_activate_social_vector_index(env, vector, index_id, embedding_name, dims);

    admin_ingest_social_embeddings(env, embeddings);
}

fn assert_public_timeline_through_gateway(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    gateway: Principal,
) {
    let result = execute_social_demo_scenario_as(
        env,
        Principal::anonymous(),
        gateway,
        SocialDemoScenario::PublicTimeline,
    );
    let rows = decode_rows(&result);
    assert_eq!(
        rows.len(),
        20,
        "public timeline should return the newest twenty public posts"
    );
    let ids: Vec<String> = rows.iter().map(|r| demo_id_text(r, "post_id")).collect();
    let expected_ids = social_feed_post_ids("-in-public-feed")
        .into_iter()
        .take(20)
        .collect::<Vec<_>>();
    assert_eq!(
        ids, expected_ids,
        "public posts should be in exact reverse chronological order"
    );
    assert!(
        !ids.contains(&"42".to_string()),
        "private post (42) must be excluded from the public timeline"
    );
    for row in &rows {
        assert_body_is_text(row, "public timeline should surface body");
        assert_author_name_is_text(row, "public timeline should surface author name");
    }
    assert_author_names(
        &rows,
        &[
            ("63", "めい"),
            ("62", "めい"),
            ("61", "めい"),
            ("84", "匠"),
            ("83", "匠"),
            ("82", "匠"),
            ("81", "そら"),
            ("80", "そら"),
            ("79", "そら"),
            ("87", "ゆい"),
            ("86", "ゆい"),
            ("85", "ゆい"),
            ("78", "蓮"),
            ("77", "蓮"),
            ("76", "蓮"),
            ("29", "あかり"),
            ("28", "あかり"),
            ("27", "あかり"),
            ("75", "Quinn"),
            ("74", "Quinn"),
        ],
    );
}

fn assert_author_names(
    rows: &[std::collections::BTreeMap<String, Value>],
    expected: &[(&str, &str)],
) {
    assert_eq!(
        rows.len(),
        expected.len(),
        "author name assertion row count mismatch"
    );
    for (row, (post_id, expected_author)) in rows.iter().zip(expected.iter()) {
        assert_eq!(
            &demo_id_text(row, "post_id"),
            post_id,
            "author name order post_id mismatch"
        );
        let author = text(row, "author_name");
        assert_eq!(
            author, *expected_author,
            "author name for {post_id} expected {expected_author}, got {author}"
        );
    }
}

fn assert_alice_home_feed_through_gateway(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    gateway: Principal,
) {
    let result = execute_social_demo_scenario_as(
        env,
        Principal::anonymous(),
        gateway,
        SocialDemoScenario::AliceHomeFeed,
    );
    let rows = decode_rows(&result);
    assert_eq!(
        rows.len(),
        16,
        "Alice's home feed should return her own public posts plus posts by her six active followees"
    );
    let ids: Vec<String> = rows.iter().map(|r| demo_id_text(r, "post_id")).collect();
    let expected_ids = social_feed_post_ids("-in-home-alice");
    assert_eq!(
        ids, expected_ids,
        "home feed should be in exact reverse chronological order"
    );
    for row in &rows {
        assert_body_is_text(row, "home feed should surface body");
        assert_author_name_is_text(row, "home feed should surface author name");
    }
    assert_author_names(
        &rows,
        &[
            ("32", "Alice"),
            ("46", "George"),
            ("35", "Bob"),
            ("48", "Hana"),
            ("44", "Fiona"),
            ("52", "Jules"),
            ("47", "Hana"),
            ("31", "Alice"),
            ("37", "Carol"),
            ("51", "Jules"),
            ("45", "George"),
            ("30", "Alice"),
            ("43", "Fiona"),
            ("34", "Bob"),
            ("36", "Carol"),
            ("33", "Bob"),
        ],
    );
    assert_parent_post_ids(
        &rows,
        &[
            ("32", "47"),
            ("35", "30"),
            ("48", "47"),
            ("44", "47"),
            ("52", "36"),
            ("37", "33"),
            ("30", "33"),
            ("34", "33"),
        ],
    );
}

fn assert_parent_post_ids(
    rows: &[std::collections::BTreeMap<String, Value>],
    expected: &[(&str, &str)],
) {
    for (post_id, parent_post_id) in expected {
        let row = rows
            .iter()
            .find(|row| demo_id_text(row, "post_id") == *post_id)
            .unwrap_or_else(|| panic!("missing post {post_id}"));
        assert_eq!(
            demo_id_text(row, "parent_post_id"),
            *parent_post_id,
            "reply {post_id} should project its canonical parent id"
        );
    }
}

fn assert_topic_path_explanation_through_gateway(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    gateway: Principal,
) {
    let result = execute_social_demo_scenario_as(
        env,
        Principal::anonymous(),
        gateway,
        SocialDemoScenario::TopicPath,
    );
    let rows = decode_rows(&result);
    assert_eq!(
        rows.len(),
        2,
        "topic path explanation should return the two reachable graph-topic posts"
    );
    assert_eq!(
        rows.iter()
            .map(|row| demo_id_text(row, "post_id"))
            .collect::<Vec<_>>(),
        vec!["46", "33"],
        "topic path should return the reachable graph-topic posts in traversal order"
    );
    assert_author_names(&rows, &[("46", "George"), ("33", "Bob")]);
    for row in &rows {
        assert_eq!(
            demo_id_text(row, "topic_id"),
            "25",
            "path should reach the Graph databases topic"
        );
        assert_body_is_text(row, "topic path should surface the Post body");
    }

    for row in &rows {
        assert_ne!(
            demo_id_text(row, "topic_id"),
            "26",
            "topic path must not return the unrelated topic-ic topic"
        );
    }
}

fn assert_semantic_discovery_through_gateway(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    gateway: Principal,
) {
    let result = execute_social_demo_scenario_as(
        env,
        Principal::anonymous(),
        gateway,
        SocialDemoScenario::SemanticDiscovery,
    );
    let rows = decode_rows(&result);
    assert_eq!(
        rows.len(),
        10,
        "vector-only semantic discovery should return its bounded ten nearest public posts"
    );
    let ids: Vec<String> = rows.iter().map(|r| demo_id_text(r, "post_id")).collect();
    assert_eq!(
        ids,
        vec!["38", "34", "36", "33", "30", "40", "45", "47", "83", "29"],
        "vector-only results must be in exact L2-squared distance order"
    );
    assert!(
        !ids.contains(&"42".to_string()),
        "private post must be excluded from vector-only semantic discovery"
    );
    for row in &rows {
        assert_body_is_text(row, "semantic discovery should surface body");
        assert_author_name_is_text(row, "semantic discovery should surface author name");
    }
    assert_author_names(
        &rows,
        &[
            ("38", "Dave"),
            ("34", "Bob"),
            ("36", "Carol"),
            ("33", "Bob"),
            ("30", "Alice"),
            ("40", "Eve"),
            ("45", "George"),
            ("47", "Hana"),
            ("83", "匠"),
            ("29", "あかり"),
        ],
    );

    assert_non_decreasing_distances(&rows);
}

fn assert_alice_semantic_feed_through_gateway(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    gateway: Principal,
) {
    let result = execute_social_demo_scenario_as(
        env,
        Principal::anonymous(),
        gateway,
        SocialDemoScenario::AliceSemanticFeed,
    );
    let rows = decode_rows(&result);
    assert_eq!(
        rows.len(),
        5,
        "Alice's semantic feed should return the five nearest posts by followed authors"
    );
    let ids: Vec<String> = rows.iter().map(|r| demo_id_text(r, "post_id")).collect();
    assert_eq!(
        ids,
        vec!["34", "36", "33", "45", "47"],
        "graph-constrained semantic results must stay within Alice's followed graph"
    );

    assert_non_decreasing_distances(&rows);
    assert_author_names(
        &rows,
        &[
            ("34", "Bob"),
            ("36", "Carol"),
            ("33", "Bob"),
            ("45", "George"),
            ("47", "Hana"),
        ],
    );
    // Plan 0068 fixed AliceSemanticFeed's body column by extending the planner's
    // property_uses collection to include row-local operator expressions (Project, etc.).
    // The body assertion for this scenario lives in `alice_semantic_feed_body_regression`
    // below so the main contract test and the SEARCH-subplan regression are independently
    // observable.
}

fn assert_non_decreasing_distances(rows: &[std::collections::BTreeMap<String, Value>]) {
    let mut previous = f64::NEG_INFINITY;
    for row in rows {
        let distance = distance_f64(row, "distance");
        assert!(
            distance >= previous,
            "semantic results must be in non-decreasing distance order"
        );
        previous = distance;
    }
}

fn assert_ad_hoc_gql_fail_closed(
    env: &gleaph_pocket_ic_tests::FederationEnv,
    default_executor: Principal,
) {
    // A default-Executor principal that is not graph-visible cannot run ad-hoc GQL directly.
    let direct_ad_hoc = gql_query_as(
        env,
        default_executor,
        "MATCH (p:Post) RETURN p.demo_id AS post_id LIMIT 1",
    );
    assert!(
        matches!(direct_ad_hoc, Err(RouterError::Forbidden)),
        "default-Executor caller must not receive general ad-hoc GQL authority: {direct_ad_hoc:?}"
    );

    // Anonymous callers cannot run ad-hoc GQL directly on Router either.
    let anon_ad_hoc = gql_query_as(
        env,
        Principal::anonymous(),
        "MATCH (p:Post) RETURN p.demo_id AS post_id LIMIT 1",
    );
    assert!(
        matches!(anon_ad_hoc, Err(RouterError::Forbidden)),
        "anonymous caller must not receive general ad-hoc GQL authority: {anon_ad_hoc:?}"
    );

    // The Gateway exposes no ad-hoc GQL surface at all; its only public method is the fixed
    // scenario enum. The canister exports exactly that interface via `ic_cdk::export_candid!()`;
    // this compile-time call site and the recipe-generated `.did` are the verification surfaces.
}

fn intern_social_schema(env: &gleaph_pocket_ic_tests::FederationEnv) {
    for label in ["User", "Post", "Topic", "Community"] {
        admin_intern_vertex_label(env, label);
    }
    for label in ["FOLLOWS", "POSTED", "REPLY_TO", "HAS_TOPIC", "MEMBER_OF"] {
        admin_intern_edge_label(env, label);
    }
    for prop in [
        "demo_id",
        "demo_graph",
        "demo_edge_id",
        "demo_kind",
        "name",
        "body",
        "created_at",
        "is_public",
    ] {
        admin_intern_property(env, prop);
    }
}

fn assert_author_name_is_text(row: &std::collections::BTreeMap<String, Value>, context: &str) {
    match row
        .get("author_name")
        .unwrap_or_else(|| panic!("{context}: missing author_name column"))
    {
        Value::Text(_) => {}
        other => panic!("{context}: expected Text author_name, got {other:?}"),
    }
}

fn assert_body_is_text(row: &std::collections::BTreeMap<String, Value>, context: &str) {
    match row
        .get("body")
        .unwrap_or_else(|| panic!("{context}: missing body column"))
    {
        Value::Text(_) => {}
        other => panic!("{context}: expected Text body, got {other:?}"),
    }
}

fn decode_rows(
    result: &gleaph_graph_kernel::plan_exec::GqlQueryResult,
) -> Vec<std::collections::BTreeMap<String, Value>> {
    let rows_blob = result
        .rows_blob
        .as_ref()
        .expect("prepared query result should carry rows_blob");
    IcWirePlanQueryResult::decode_blob(rows_blob)
        .expect("decode rows_blob")
        .try_into_value_rows()
        .expect("convert wire rows to value rows")
}

fn text(row: &std::collections::BTreeMap<String, Value>, column: &str) -> String {
    match row
        .get(column)
        .unwrap_or_else(|| panic!("missing column {column}"))
    {
        Value::Text(value) => value.clone(),
        other => panic!("expected Text in column {column}, got {other:?}"),
    }
}

fn demo_id_text(row: &std::collections::BTreeMap<String, Value>, column: &str) -> String {
    match row
        .get(column)
        .unwrap_or_else(|| panic!("missing column {column}"))
    {
        Value::Uint64(value) => value.to_string(),
        Value::Int64(value) => value.to_string(),
        Value::Text(value) => value.clone(),
        other => panic!("expected numeric/text demo_id in column {column}, got {other:?}"),
    }
}

fn distance_f64(row: &std::collections::BTreeMap<String, Value>, column: &str) -> f64 {
    match row
        .get(column)
        .unwrap_or_else(|| panic!("missing column {column}"))
    {
        Value::Float64(value) => *value,
        other => panic!("expected Float64 in column {column}, got {other:?}"),
    }
}

/// Plan 0067 / Plan 0068 regression target.
///
/// Exercises the AliceSemanticFeed Gateway path and asserts that every row's `body`
/// column is a Text value (not Null). Enabled once Plan 0068 extended the planner's
/// property_uses collection to include row-local operator expressions such as Project.
#[test]
fn alice_semantic_feed_body_regression() {
    let (env, gateway) = install_single_shard_federation_with_gateway();

    intern_social_schema(&env);
    seed_social_graph_and_assert_feed_edge_order(&env);

    prepared_register_as_admin(&env, "public_timeline", PUBLIC_TIMELINE_QUERY);
    prepared_register_as_admin(&env, "alice_home_feed", ALICE_HOME_FEED_QUERY);
    prepared_register_as_admin(&env, "topic_path_explanation", TOPIC_PATH_QUERY);

    ingest_social_embeddings_through_router(
        &env,
        SEMANTIC_INDEX_ID,
        SEMANTIC_EMBEDDING_NAME,
        SEMANTIC_DIMS,
    );
    prepared_register_as_admin(&env, "semantic_discovery", SEMANTIC_DISCOVERY_QUERY);
    prepared_register_as_admin(&env, "alice_semantic_feed", ALICE_SEMANTIC_FEED_QUERY);

    let result = execute_social_demo_scenario_as(
        &env,
        Principal::anonymous(),
        gateway,
        SocialDemoScenario::AliceSemanticFeed,
    );
    let rows = decode_rows(&result);
    assert_eq!(
        rows.len(),
        7,
        "Alice semantic feed should return exactly 7 rows"
    );
    for row in &rows {
        assert_body_is_text(
            row,
            "Alice semantic feed should surface body (Plan 0067/0068 regression)",
        );
        assert_author_name_is_text(row, "Alice semantic feed should surface author name");
    }
}
