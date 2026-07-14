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
    seed_social_graph_and_assert_feed_edge_order,
};
use gleaph_social_demo_gateway::SocialDemoScenario;

const PUBLIC_TIMELINE_QUERY: &str = "\
MATCH (feed:Feed {demo_id: 40})<-[e:IN_PUBLIC_FEED]-(p:Post)<-[:POSTED]-(author:User) \
OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) \
RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at \
ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 20";

const ALICE_HOME_FEED_QUERY: &str = "\
MATCH (u:User {demo_id: 1})<-[e:IN_HOME_FEED]-(p:Post)<-[:POSTED]-(author:User) \
WHERE p.is_public = TRUE \
OPTIONAL MATCH (p)-[:REPLY_TO]->(parent:Post) \
RETURN p.demo_id AS post_id, parent.demo_id AS parent_post_id, author.name AS author_name, p.body AS body, p.created_at AS created_at \
ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 20";

const TOPIC_PATH_QUERY: &str = "\
MATCH (p:Post)-[has_topic:HAS_TOPIC]->(t:Topic) \
WHERE t.demo_id = 7 \
MATCH (u:User)-[follows:FOLLOWS]->(author:User)-[posted:POSTED]->(p) \
WHERE u.demo_id = 1 \
RETURN p.demo_id AS post_id, \
       author.name AS author_name, \
       follows.demo_edge_id AS follows_edge_id, \
       posted.demo_edge_id AS posted_edge_id, \
       t.demo_id AS topic_id, \
       has_topic.demo_edge_id AS topic_edge_id, \
       p.body AS body, \
       p.created_at AS created_at";

const SEMANTIC_EMBEDDING_NAME: &str = "post_vec";
const SEMANTIC_INDEX_ID: u32 = 1;
const SEMANTIC_DIMS: u16 = 8;

const SEMANTIC_DISCOVERY_QUERY: &str = "MATCH (p:Post)<-[:POSTED]-(author:User) WHERE p.is_public = TRUE SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, distance ORDER BY distance ASC";

const ALICE_SEMANTIC_FEED_QUERY: &str = "MATCH (u:User)-[:FOLLOWS]->(author:User)-[:POSTED]->(p:Post) WHERE u.demo_id = 1 AND p.is_public = TRUE SEARCH p IN (VECTOR INDEX post_vec FOR $query LIMIT 10) DISTANCE AS distance RETURN p.demo_id AS post_id, author.name AS author_name, p.body AS body, distance ORDER BY distance ASC";

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
    assert_eq!(
        ids,
        vec![
            "17", "31", "20", "33", "24", "39", "29", "26", "37", "32", "35", "16", "22", "36",
            "30", "15", "28", "19", "34", "38",
        ],
        "public posts should be in exact reverse chronological order"
    );
    assert!(
        !ids.contains(&"27".to_string()),
        "private post (27) must be excluded from the public timeline"
    );
    for row in &rows {
        assert_body_is_text(row, "public timeline should surface body");
        assert_author_name_is_text(row, "public timeline should surface author name");
    }
    assert_author_names(
        &rows,
        &[
            ("17", "Alice"),
            ("31", "George"),
            ("20", "Bob"),
            ("33", "Hana"),
            ("24", "Dave"),
            ("39", "Kira"),
            ("29", "Fiona"),
            ("26", "Eve"),
            ("37", "Jules"),
            ("32", "Hana"),
            ("35", "Ian"),
            ("16", "Alice"),
            ("22", "Carol"),
            ("36", "Jules"),
            ("30", "George"),
            ("15", "Alice"),
            ("28", "Fiona"),
            ("19", "Bob"),
            ("34", "Ian"),
            ("38", "Kira"),
        ],
    );
    assert_parent_post_ids(
        &rows,
        &[
            ("17", "32"),
            ("20", "15"),
            ("33", "32"),
            ("29", "32"),
            ("37", "21"),
            ("22", "18"),
            ("15", "18"),
            ("19", "18"),
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
    assert_eq!(
        ids,
        vec![
            "17", "31", "20", "33", "29", "37", "32", "16", "22", "36", "30", "15", "28", "19",
            "21", "18",
        ],
        "home feed should be in exact reverse chronological order"
    );
    for adversary in ["23", "24", "25", "26", "27", "34", "35", "38", "39"] {
        assert!(
            !ids.contains(&adversary.to_string()),
            "home feed must exclude private or public posts by unfollowed, non-author users: {adversary}"
        );
    }
    for row in &rows {
        assert_body_is_text(row, "home feed should surface body");
        assert_author_name_is_text(row, "home feed should surface author name");
    }
    assert_author_names(
        &rows,
        &[
            ("17", "Alice"),
            ("31", "George"),
            ("20", "Bob"),
            ("33", "Hana"),
            ("29", "Fiona"),
            ("37", "Jules"),
            ("32", "Hana"),
            ("16", "Alice"),
            ("22", "Carol"),
            ("36", "Jules"),
            ("30", "George"),
            ("15", "Alice"),
            ("28", "Fiona"),
            ("19", "Bob"),
            ("21", "Carol"),
            ("18", "Bob"),
        ],
    );
    assert_parent_post_ids(
        &rows,
        &[
            ("17", "32"),
            ("20", "15"),
            ("33", "32"),
            ("29", "32"),
            ("37", "21"),
            ("22", "18"),
            ("15", "18"),
            ("19", "18"),
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
        1,
        "topic path explanation should return exactly the one followee post with a topic"
    );
    let row = &rows[0];
    assert_eq!(
        demo_id_text(row, "post_id"),
        "18",
        "path should go through Bob's graph-modeling post"
    );
    assert_eq!(
        demo_id_text(row, "topic_id"),
        "13",
        "path should reach the Graph databases topic"
    );
    assert_eq!(
        text(row, "follows_edge_id"),
        "alice-follows-bob",
        "follows edge identity should explain the path"
    );
    assert_eq!(
        text(row, "posted_edge_id"),
        "bob-posted-1",
        "posted edge identity should explain the path"
    );
    assert!(
        text(row, "topic_edge_id").ends_with("-topic-graph"),
        "HAS_TOPIC edge identity should retain the topic suffix"
    );
    assert_eq!(
        text(row, "body"),
        "I wrote up the little graph-modeling trick that saved us a migration. The diagram is in the replies.",
        "topic path should surface the Post body"
    );
    assert_eq!(
        text(row, "author_name"),
        "Bob",
        "topic path should surface the author name"
    );

    for row in &rows {
        assert_ne!(
            demo_id_text(row, "topic_id"),
            "14",
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
        vec!["23", "19", "21", "18", "15", "25", "30", "32", "20", "31"],
        "vector-only results must be in exact L2-squared distance order"
    );
    assert!(
        !ids.contains(&"27".to_string()),
        "private post must be excluded from vector-only semantic discovery"
    );
    for row in &rows {
        assert_body_is_text(row, "semantic discovery should surface body");
        assert_author_name_is_text(row, "semantic discovery should surface author name");
    }
    assert_author_names(
        &rows,
        &[
            ("23", "Dave"),
            ("19", "Bob"),
            ("21", "Carol"),
            ("18", "Bob"),
            ("15", "Alice"),
            ("25", "Eve"),
            ("30", "George"),
            ("32", "Hana"),
            ("20", "Bob"),
            ("31", "George"),
        ],
    );

    assert_exact_distances(
        &rows,
        &[
            ("23", 0.0),
            ("19", 1.0),
            ("21", 4.0),
            ("18", 9.0),
            ("15", 16.0),
            ("25", 25.0),
            ("30", 36.0),
            ("32", 49.0),
            ("20", 64.0),
            ("31", 81.0),
        ],
    );
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
        10,
        "Alice's semantic feed should return the ten nearest posts by followed authors"
    );
    let ids: Vec<String> = rows.iter().map(|r| demo_id_text(r, "post_id")).collect();
    assert_eq!(
        ids,
        vec!["19", "21", "18", "30", "32", "20", "31", "28", "22", "33"],
        "graph-constrained semantic results must exclude the nearer unfollowed post"
    );
    for adversary in ["15", "23", "24", "25", "26", "27", "34", "35", "38", "39"] {
        assert!(
            !ids.contains(&adversary.to_string()),
            "Alice's semantic feed must exclude {adversary}"
        );
    }

    assert_exact_distances(
        &rows,
        &[
            ("19", 1.0),
            ("21", 4.0),
            ("18", 9.0),
            ("30", 36.0),
            ("32", 49.0),
            ("20", 64.0),
            ("31", 81.0),
            ("28", 100.0),
            ("22", 121.0),
            ("33", 144.0),
        ],
    );
    assert_author_names(
        &rows,
        &[
            ("19", "Bob"),
            ("21", "Carol"),
            ("18", "Bob"),
            ("30", "George"),
            ("32", "Hana"),
            ("20", "Bob"),
            ("31", "George"),
            ("28", "Fiona"),
            ("22", "Carol"),
            ("33", "Hana"),
        ],
    );
    // Plan 0068 fixed AliceSemanticFeed's body column by extending the planner's
    // property_uses collection to include row-local operator expressions (Project, etc.).
    // The body assertion for this scenario lives in `alice_semantic_feed_body_regression`
    // below so the main contract test and the SEARCH-subplan regression are independently
    // observable.
}

fn assert_exact_distances(
    rows: &[std::collections::BTreeMap<String, Value>],
    expected: &[(&str, f64)],
) {
    assert_eq!(
        rows.len(),
        expected.len(),
        "distance assertion row count mismatch"
    );
    for (row, (post_id, expected_distance)) in rows.iter().zip(expected.iter()) {
        assert_eq!(
            &demo_id_text(row, "post_id"),
            post_id,
            "distance order post_id mismatch"
        );
        let distance = distance_f64(row, "distance");
        assert!(
            (distance - *expected_distance).abs() < 1e-6,
            "distance for {post_id} expected {expected_distance}, got {distance}"
        );
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
        10,
        "Alice semantic feed should return exactly 10 rows"
    );
    for row in &rows {
        assert_body_is_text(
            row,
            "Alice semantic feed should surface body (Plan 0067/0068 regression)",
        );
        assert_author_name_is_text(row, "Alice semantic feed should surface author name");
    }
}
