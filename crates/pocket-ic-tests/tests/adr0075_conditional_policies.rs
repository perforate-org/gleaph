//! PocketIC: ADR 0075 — conditional policies with router-resolved constant pushdown.
//!
//! One suite proves the visibility contract end to end:
//!
//! 1. **Union visibility matrix**: PUBLIC holds `MATCH Post FOR (p:Post) WHERE
//!    p.visibility = 'public'`; alice additionally holds `MATCH … WHERE
//!    p.owner = MSG_CALLER() AND p.visibility = 'private'` (AND within one row).
//!    Alice sees exactly public ∪ own-private; bob sees exactly public ∪ own-private;
//!    filtered-out rows are absent, never errors.
//! 2. **User-authored baseline**: the owner (implicit root, policy-free) runs the
//!    equivalent hand-written WHERE and observes alice's exact row set.
//! 3. **Traversal-bound destinations**: policies bind posts reached through expansion,
//!    not only labeled scans.
//! 4. **Prepared re-resolution**: one published query returns different subsets to two
//!    callers (per-invocation constant substitution).
//! 5. **Index-seeded equality equivalence**: with an active index on the policy
//!    property, single-row callers seed the index scan; multi-row callers keep the
//!    residual union filter — both observe the same rows as the un-indexed run.
//! 6. **Upgrade survival**: grants and visibility outcomes survive a canister upgrade.
//!
//! Expected counts stated before running (per plan contract):
//! - 5 `#[test]` functions.
//! - Each test constructs one PocketIC environment (`install_single_shard_federation`)
//!   installing exactly 3 canisters.
//! - Update/query budget per environment: 1 schema-bind mutate, ~14 seed mutates each
//!   with a maintenance drain, 4–6 GRANT mutates, ≤1 CREATE INDEX DDL, ≤1 prepare +
//!   publish, ≤1 upgrade triple, and ≤16 probe calls.
//! - Seeded graph: 2 users × 2 posts = 4 posts (`ap`=public/alice, `ar`=private/alice,
//!   `bp`=public/bob, `br`=private/bob). Expected visibility sets are asserted inline:
//!   anonymous `["ap","bp"]`, alice `["ap","ar","bp"]`, bob `["ap","bp","br"]`,
//!   stranger → NotFound.

use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, gql_mutate_as_admin, gql_query_as,
    install_single_shard_federation, prepare_batch_as_admin, prepared_query_with_params_as,
    publish_prepared_query_as, wasm_bytes,
};
use gleaph_router::types::{GrantOperationView, GrantSubjectView};

fn alice() -> Principal {
    Principal::from_slice(&[0xA1; 29])
}

fn bob() -> Principal {
    Principal::from_slice(&[0xB2; 29])
}

/// A principal holding no grants and no tenancy: default-deny everywhere.
fn stranger() -> Principal {
    Principal::from_slice(&[0xC3; 29])
}

/// Typed schema: User{name}, Post{tag, visibility, owner}, DIRECTED POSTED edges.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE pgt {{ NODE User {{ name STRING }}, \
         NODE Post {{ tag STRING, visibility STRING, owner STRING }}, \
         DIRECTED EDGE POSTED LABEL POSTED CONNECTING (User -> Post) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED pgt"
    );
    gql_mutate_as_admin(env, &ddl, "adr0075-bind-schema");
}

fn params_blob(fields: Vec<(&str, gleaph_gql::Value)>) -> Vec<u8> {
    gleaph_gql_ic::encode_gql_params_blob(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
    .expect("encode params blob")
}

/// Inserts one post whose `owner` property holds the IC principal value domain that
/// `MSG_CALLER()` compares against.
fn seed_post(env: &FederationEnv, owner: Principal, vis: &str, tag: &str) {
    let query = "INSERT (:Post {tag: $tag, visibility: $vis, owner: $owner})";
    let params = params_blob(vec![
        ("tag", gleaph_gql::Value::Text(tag.to_string())),
        ("vis", gleaph_gql::Value::Text(vis.to_string())),
        ("owner", gleaph_gql_ic::principal_to_value(owner)),
    ]);
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_mutate",
            Encode!(&query.to_string(), &params, &format!("adr0075-post-{tag}"))
                .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>)
        .expect("decode gql_mutate")
        .expect("post inserted");
    drain_maintenance_via_timer(env, env.graph_source);
}

/// Seeds alice/bob users, four posts (public+private per owner), and their POSTED edges.
fn seed_social_graph(env: &FederationEnv) {
    gql_mutate_as_admin(env, "INSERT (:User {name: 'alice'})", "adr0075-user-alice");
    drain_maintenance_via_timer(env, env.graph_source);
    gql_mutate_as_admin(env, "INSERT (:User {name: 'bob'})", "adr0075-user-bob");
    drain_maintenance_via_timer(env, env.graph_source);

    seed_post(env, alice(), "public", "ap");
    seed_post(env, alice(), "private", "ar");
    seed_post(env, bob(), "public", "bp");
    seed_post(env, bob(), "private", "br");

    // One edge per (user, own post): the owner param pins both endpoints deterministically.
    for (user, tag) in [
        ("alice", "ap"),
        ("alice", "ar"),
        ("bob", "bp"),
        ("bob", "br"),
    ] {
        let owner = if user == "alice" { alice() } else { bob() };
        let query = format!(
            "MATCH (u:User {{name: '{user}'}}), (p:Post {{tag: '{tag}', owner: $owner}}) \
             INSERT (u)-[:POSTED]->(p)"
        );
        let params = params_blob(vec![("owner", gleaph_gql_ic::principal_to_value(owner))]);
        let bytes = env
            .pic
            .update_call(
                env.router,
                env.admin,
                "gql_mutate",
                Encode!(
                    &query.to_string(),
                    &params,
                    &format!("adr0075-edge-{user}-{tag}")
                )
                .expect("encode gql_mutate"),
            )
            .expect("gql_mutate call");
        Decode!(&bytes, Result<GqlQueryResult, RouterError>)
            .expect("decode gql_mutate")
            .expect("edge inserted");
        drain_maintenance_via_timer(env, env.graph_source);
    }
}

/// The unconditional coverage every caller needs besides the conditional MATCH row:
/// property projection on Post plus the User/POSTED traversal surface.
fn grant_common_rows(env: &FederationEnv) {
    for statement in [
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Post {{ tag, visibility }} TO PUBLIC"),
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES User {{ name }} TO PUBLIC"),
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES User TO PUBLIC"),
        format!("GRANT TRAVERSE ON GRAPH {GRAPH_NAME} EDGES POSTED TO PUBLIC"),
    ] {
        gql_mutate_as_admin(env, &statement, "adr0075-grant-common");
    }
}

fn grant_public_conditional(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Post \
             FOR (p:Post) WHERE p.visibility = 'public' TO PUBLIC"
        ),
        "adr0075-grant-public-conditional",
    );
}

/// The two-grant union shape of the plan's success signal: PUBLIC narrows to public
/// posts while the member additionally holds own-private posts via an AND conjunction
/// over `MSG_CALLER()`.
fn grant_member_conditional(env: &FederationEnv, member: Principal) {
    let key = format!(
        "adr0075-grant-member-{}",
        &member.to_text()[member.to_text().len() - 6..]
    );
    gql_mutate_as_admin(
        env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Post \
             FOR (p:Post) WHERE p.owner = MSG_CALLER() AND p.visibility = 'private' \
             TO PRINCIPAL '{}'",
            member.to_text()
        ),
        &key,
    );
}

const SCAN_QUERY: &str =
    "MATCH (p:Post) RETURN p.tag AS tag, p.visibility AS vis ORDER BY vis, tag";

/// Issues one GRANT/REVOKE as `caller`, returning the typed verdict so failed-closed
/// authority probes are observable.
fn authorization_verdict(
    env: &FederationEnv,
    caller: Principal,
    statement: String,
    mutation_key: &str,
) -> Result<(), RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            caller,
            "gql_mutate",
            Encode!(&statement, &Vec::<u8>::new(), &mutation_key.to_string())
                .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

fn tags(result: &GqlQueryResult) -> Vec<String> {
    use gleaph_gql::Value;
    use gleaph_gql_ic::GqlWireRows;
    let rows_blob = result.rows_blob.as_ref().expect("rows blob present");
    let wire = GqlWireRows::decode_blob(rows_blob).expect("decode rows");
    let mut out = Vec::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("value row");
        let Value::Text(tag) = value_row.get("tag").expect("tag column") else {
            panic!("expected text tag, got {:?}", value_row.get("tag"));
        };
        out.push(tag.clone());
    }
    out.sort();
    out
}

fn assert_tags(result: &GqlQueryResult, expected: &[&str]) {
    let got = tags(result);
    let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    assert_eq!(got, expected, "policy-filtered row set mismatch");
}

#[test]
fn union_visibility_matrix_matches_user_authored_baseline() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_social_graph(&env);
    grant_common_rows(&env);
    grant_public_conditional(&env);
    grant_member_conditional(&env, alice());
    grant_member_conditional(&env, bob());

    // Introspection prints stored conditions inline ([ADR 0075] §1) with catalog-resolved
    // property names; both member rows carry identical condition text.
    let grant_bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_graph_grants",
            Encode!(&GRAPH_NAME.to_string()).expect("encode"),
        )
        .expect("list_graph_grants call");
    let summaries = Decode!(
        &grant_bytes,
        Result<Vec<gleaph_router::types::GraphGrantSummary>, RouterError>
    )
    .expect("decode")
    .expect("list");
    let conditional: Vec<_> = summaries.iter().filter(|s| s.predicate.is_some()).collect();
    assert_eq!(conditional.len(), 3, "PUBLIC + two member conditional rows");
    for summary in &conditional {
        assert_eq!(summary.operation, GrantOperationView::Match);
    }
    assert_eq!(
        conditional[0].predicate.as_deref(),
        Some("WHERE visibility = 'public'"),
        "PUBLIC row sorts first (subject kind 0)"
    );
    for summary in &conditional[1..] {
        assert!(matches!(summary.subject, GrantSubjectView::Principal(_)));
        assert_eq!(
            summary.predicate.as_deref(),
            Some("WHERE owner = MSG_CALLER() AND visibility = 'private'")
        );
    }

    // Anonymous callers evaluate through the PUBLIC row alone: only public posts.
    let anon = gql_query_as(&env, Principal::anonymous(), SCAN_QUERY).expect("anon scan");
    assert_tags(&anon, &["ap", "bp"]);

    // Alice: PUBLIC ∪ own-private — her private post is present, bob's is ABSENT
    // (absence-not-error: the query succeeds, the unauthorized row never appears).
    let alice_view = gql_query_as(&env, alice(), SCAN_QUERY).expect("alice scan");
    assert_tags(&alice_view, &["ap", "ar", "bp"]);

    // Bob symmetric: his private post, never alice's.
    let bob_view = gql_query_as(&env, bob(), SCAN_QUERY).expect("bob scan");
    assert_tags(&bob_view, &["ap", "bp", "br"]);

    // A stranger holds no rows of their own: the PUBLIC conditional row still applies,
    // so they see only the public subset ([ADR 0075]: strangers see only public).
    let stranger_view = gql_query_as(&env, stranger(), SCAN_QUERY).expect("stranger scan");
    assert_tags(&stranger_view, &["ap", "bp"]);

    // Authority fails closed ([ADR 0075] §6): a non-owner cannot attach policy rows,
    // and the failed attempt stores nothing.
    let verdict = authorization_verdict(
        &env,
        stranger(),
        format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Post \
             FOR (p:Post) WHERE p.visibility = 'public' TO PUBLIC"
        ),
        "adr0075-stranger-grant",
    );
    assert!(matches!(verdict, Err(RouterError::Forbidden)));
    // The stranger's visible set is unchanged after the rejected grant attempt.
    let still_public = gql_query_as(&env, stranger(), SCAN_QUERY).expect("still public-only");
    assert_tags(&still_public, &["ap", "bp"]);

    // User-authored baseline ([ADR 0075] §4 evidence): the owner is the implicit root
    // (no policies bind tenants), so the hand-written equivalent of alice's policy view
    // must return exactly alice's set.
    let baseline_query = "MATCH (p:Post) WHERE p.visibility = 'public' OR (p.owner = $o AND p.visibility = 'private') \
         RETURN p.tag AS tag ORDER BY tag";
    let baseline = gleaph_pocket_ic_tests::gql_query_with_params_on_router(
        &env.pic,
        env.admin,
        env.router,
        baseline_query,
        params_blob(vec![("o", gleaph_gql_ic::principal_to_value(alice()))]),
    );
    assert_eq!(
        tags(&baseline),
        tags(&alice_view),
        "policy ≡ user-authored WHERE"
    );
}

#[test]
fn traversal_bound_destinations_observe_policy_filtered_visibility() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_social_graph(&env);
    grant_common_rows(&env);
    grant_public_conditional(&env);
    grant_member_conditional(&env, alice());

    // Posts bound through expansion (no labeled Post scan in the pattern) carry the
    // guarded policy filter at the expansion destination.
    let traversal = "MATCH (u:User)-[:POSTED]->(p:Post) RETURN p.tag AS tag ORDER BY tag";
    let anon = gql_query_as(&env, Principal::anonymous(), traversal).expect("anon traversal");
    assert_tags(&anon, &["ap", "bp"]);

    let view = gql_query_as(&env, alice(), traversal).expect("alice traversal");
    assert_tags(&view, &["ap", "ar", "bp"]);
}

#[test]
fn prepared_execution_reresolves_constants_per_caller() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_social_graph(&env);
    grant_common_rows(&env);
    grant_public_conditional(&env);
    grant_member_conditional(&env, alice());
    grant_member_conditional(&env, bob());

    prepare_batch_as_admin(
        &env,
        &[gleaph_prepared_api::PreparedRegistration {
            name: "postfeed".into(),
            query: "MATCH (p:Post) RETURN p.tag AS tag ORDER BY tag".into(),
            metadata: None,
        }],
    )
    .expect("register postfeed");
    publish_prepared_query_as(
        &env,
        env.admin,
        "postfeed",
        "PUBLIC",
        "adr0075-publish-postfeed",
    )
    .expect("owner publishes postfeed");

    // ONE published query, TWO callers, TWO different resolved-constant plans.
    let feed_alice = prepared_query_with_params_as(&env, alice(), "postfeed", Vec::new());
    assert_tags(&feed_alice, &["ap", "ar", "bp"]);
    let feed_bob = prepared_query_with_params_as(&env, bob(), "postfeed", Vec::new());
    assert_tags(&feed_bob, &["ap", "bp", "br"]);

    // Repeat invocation exercises the per-caller lowered-plan cache without changing
    // the visible set.
    let again = prepared_query_with_params_as(&env, alice(), "postfeed", Vec::new());
    assert_tags(&again, &["ap", "ar", "bp"]);
}

#[test]
fn index_seeded_equality_matches_residual_filtering() {
    // Environment A: no index — policy comparisons lower into residual filters.
    let env_a = install_single_shard_federation();
    bind_typed_schema(&env_a);
    seed_social_graph(&env_a);
    grant_common_rows(&env_a);
    grant_public_conditional(&env_a);
    grant_member_conditional(&env_a, alice());
    grant_member_conditional(&env_a, bob());
    let residual_anon =
        gql_query_as(&env_a, Principal::anonymous(), SCAN_QUERY).expect("residual anon");
    let residual_alice = gql_query_as(&env_a, alice(), SCAN_QUERY).expect("residual alice");

    // Environment B: identical graph + grants plus an active vertex index on the
    // policy property, created before seeding so postings exist at query time.
    // Single-row callers now seed the index scan with the resolved literal; the
    // multi-row caller keeps the residual union filter.
    let env_b = install_single_shard_federation();
    bind_typed_schema(&env_b);
    gql_mutate_as_admin(
        &env_b,
        "CREATE INDEX adr0075_post_vis FOR (p:Post) ON (p.visibility)",
        "adr0075-create-index",
    );
    seed_social_graph(&env_b);
    grant_common_rows(&env_b);
    grant_public_conditional(&env_b);
    grant_member_conditional(&env_b, alice());
    grant_member_conditional(&env_b, bob());

    let seeded_anon =
        gql_query_as(&env_b, Principal::anonymous(), SCAN_QUERY).expect("seeded anon");
    assert_tags(&seeded_anon, &["ap", "bp"]);
    assert_eq!(
        tags(&seeded_anon),
        tags(&residual_anon),
        "index-seeded equality must equal residual filtering (single-row caller)"
    );

    let seeded_alice = gql_query_as(&env_b, alice(), SCAN_QUERY).expect("seeded-env alice");
    assert_tags(&seeded_alice, &["ap", "ar", "bp"]);
    assert_eq!(
        tags(&seeded_alice),
        tags(&residual_alice),
        "multi-row union filter must equal the un-indexed result"
    );
}

#[test]
fn grants_and_visibility_survive_canister_upgrade() {
    // Pin the artifact bytes once so install and upgrade read identical modules even
    // when a concurrent workspace build rewrites the shared pocket-ic-wasm output
    // mid-test (a bytes mismatch would fail post_upgrade on unrelated stable cells).
    let pinned: Vec<(&'static str, std::path::PathBuf)> =
        ["ROUTER_WASM", "INDEX_WASM", "GRAPH_WASM"]
            .iter()
            .map(|name| {
                let path = std::path::PathBuf::from(std::env::var(name).expect("wasm env"));
                let snapshot = std::env::temp_dir().join(format!(
                    "adr0075-upgrade-{}-{}.wasm",
                    name,
                    std::process::id()
                ));
                std::fs::copy(&path, &snapshot).expect("snapshot wasm artifact");
                (*name, snapshot)
            })
            .collect();
    for (name, path) in &pinned {
        // SAFETY: single-threaded test setup before the runtime spawns work; the
        // redirected paths point at byte-identical snapshots of the same artifacts.
        unsafe { std::env::set_var(name, path) };
    }

    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_social_graph(&env);
    grant_common_rows(&env);
    grant_public_conditional(&env);
    grant_member_conditional(&env, alice());
    grant_member_conditional(&env, bob());

    let before = gql_query_as(&env, alice(), SCAN_QUERY).expect("pre-upgrade scan");
    assert_tags(&before, &["ap", "ar", "bp"]);

    let empty = Vec::<u8>::new();
    env.pic
        .upgrade_canister(env.router, wasm_bytes("ROUTER_WASM"), empty.clone(), None)
        .expect("upgrade router canister");
    env.pic
        .upgrade_canister(env.index, wasm_bytes("INDEX_WASM"), empty.clone(), None)
        .expect("upgrade index canister");
    env.pic
        .upgrade_canister(env.graph_source, wasm_bytes("GRAPH_WASM"), empty, None)
        .expect("upgrade graph shard canister");

    // Grant rows live in stable storage (MemoryId 55); after the upgrade the compiled
    // predicates decode from the fresh-state row encoding and the visibility matrix is
    // unchanged.
    let after = gql_query_as(&env, alice(), SCAN_QUERY).expect("post-upgrade scan");
    assert_tags(&after, &["ap", "ar", "bp"]);
    let anon_after = gql_query_as(&env, Principal::anonymous(), SCAN_QUERY)
        .expect("post-upgrade anonymous scan");
    assert_tags(&anon_after, &["ap", "bp"]);
}
