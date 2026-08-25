//! PocketIC: ADR 0082 — ReBAC conditional policies with bounded EXISTS traversal.
//!
//! One suite proves the relationship-based visibility contract end to end:
//!
//! 1. **One-hop direct-grant matrix**: a caller sees documents granted to an account
//!    they control (`EXISTS { (d)-[:GRANTED_TO]->(a:Acct) WHERE
//!    a.principal_id = MSG_CALLER() }`); uncovered rows are absent results, never
//!    errors; multi-match grants never duplicate rows; introspection prints the chain
//!    inline with resolved names.
//! 2. **Two-hop org-membership matrix**: visibility through
//!    `(d)-[:SHARED_TO]->(g:Group)<-[:MEMBER_OF]-(a:Acct)`; covered callers belonging
//!    to no group hold an empty success.
//! 3. **Prepared re-resolution**: one published query returns caller-shaped lowered
//!    plans' results to two distinct callers (per-invocation `MSG_CALLER()` substitution).
//! 4. **Vector tail composition** (ADR 0078 layer 2): search candidates filter through
//!    the lowered chain; deepening preserves k even when hidden docs rank nearest.
//! 5. **Vocabulary-drop cascade**: tearing down one graph sweeps its stale chains while
//!    a sibling graph whose labels reused identical numeric ids keeps working.
//! 6. **Deny-by-default + GRANT-time rejection**: callers without label coverage get
//!    the uniform non-disclosing `Forbidden`; invalid chains reject before any write.
//!
//! Expected counts stated before running (per plan contract):
//! - 6 `#[test]` functions.
//! - Tests 1–4 and 6 each construct one PocketIC environment
//!   (`install_single_shard_federation`, 3 canisters); test 5 uses
//!   `install_two_graph_federation` (5 canisters).
//! - Update budget per single-graph environment: 1 schema-bind mutate, ≤12 seed mutates
//!   each with a maintenance drain, ≤5 GRANT mutates, ≤6 probe queries.
//! - Test 4 adds one vector canister: 1 registration + 4 activation/attach calls,
//!   3 doc vertices with one embedding each.
//! - Test 5: ≤8 schema/seed/GRANT mutates per graph plus 1 shard unregister + 1 graph
//!   unregister; ≤4 probes.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, drain_maintenance_via_timer, ensure_property, ensure_vertex_label,
    gql_mutate_as_admin, gql_query_as, install_single_shard_federation, prepare_batch_as_admin,
    prepared_query_with_params_as, publish_prepared_query_as,
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

/// A member of no group: covered by the org-membership row, matches nothing.
fn charlie() -> Principal {
    Principal::from_slice(&[0xC4; 29])
}

/// Typed schema for the direct-grant and org-membership patterns plus an UNDIRECTED
/// edge used by the directedness-rejection probe.
fn bind_typed_schema(env: &FederationEnv) {
    let ddl = format!(
        "CREATE GRAPH TYPE rbt {{ \
         NODE Doc {{ tag STRING }}, \
         NODE Acct {{ principal_id STRING }}, \
         NODE Group {{ name STRING }}, \
         DIRECTED EDGE GrantedTo LABEL GRANTED_TO CONNECTING (Doc -> Acct), \
         DIRECTED EDGE SharedTo LABEL SHARED_TO CONNECTING (Doc -> Group), \
         DIRECTED EDGE MemberOf LABEL MEMBER_OF CONNECTING (Acct -> Group), \
         UNDIRECTED EDGE Link LABEL LINK CONNECTING (Doc ~ Doc) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED rbt"
    );
    gql_mutate_as_admin(env, &ddl, "adr0082-bind-schema");
}

fn params_blob(fields: Vec<(&str, Value)>) -> Vec<u8> {
    gleaph_gql_ic::encode_gql_params_blob(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
    .expect("encode params blob")
}

fn mutate_with_params(env: &FederationEnv, query: &str, fields: Vec<(&str, Value)>, key: &str) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_mutate",
            Encode!(&query.to_string(), &params_blob(fields), &key.to_string())
                .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>)
        .expect("decode gql_mutate")
        .expect("mutate ok");
    drain_maintenance_via_timer(env, env.graph_source);
}

/// Raw mutate verdict so failed-closed GRANT-time rejections are observable.
fn mutate_verdict(env: &FederationEnv, query: &str, key: &str) -> Result<(), RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_mutate",
            Encode!(&query.to_string(), &Vec::<u8>::new(), &key.to_string())
                .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

/// Raw composite query verdict so uniform denials are observable without panicking.
fn query_verdict(
    env: &FederationEnv,
    caller: Principal,
    query: &str,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &gleaph_graph_kernel::plan_exec::ReadMode::Eventual
            )
            .expect("encode gql_query"),
        )
        .expect("gql_query call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query")
}

fn insert_account(env: &FederationEnv, principal: Principal, key: &str) {
    mutate_with_params(
        env,
        "INSERT (:Acct {principal_id: $p})",
        vec![("p", gleaph_gql_ic::principal_to_value(principal))],
        key,
    );
}

/// Seeds the direct-grant fixture:
/// - `d1` granted to alice; `d2` ungranted; `d3` granted to BOTH alice and bob
///   (multi-match); `d4` granted to bob only.
fn seed_direct_grant_docs(env: &FederationEnv) {
    insert_account(env, alice(), "adr0082-acct-alice");
    insert_account(env, bob(), "adr0082-acct-bob");
    for tag in ["d1", "d2", "d3", "d4"] {
        mutate_with_params(
            env,
            &format!("INSERT (:Doc {{tag: '{tag}'}})"),
            vec![],
            &format!("adr0082-doc-{tag}"),
        );
    }
    for (doc, principal, key) in [
        ("d1", alice(), "a1"),
        ("d3", alice(), "a3a"),
        ("d3", bob(), "a3b"),
        ("d4", bob(), "a4"),
    ] {
        mutate_with_params(
            env,
            "MATCH (d:Doc {tag: $tag}), (a:Acct {principal_id: $p}) \
             INSERT (d)-[:GRANTED_TO]->(a)",
            vec![
                ("tag", Value::Text(doc.to_string())),
                ("p", gleaph_gql_ic::principal_to_value(principal)),
            ],
            &format!("adr0082-grant-edge-{key}"),
        );
    }
}

const DOC_SCAN: &str = "MATCH (d:Doc) RETURN d.tag AS tag ORDER BY d.tag";

fn tags(result: &GqlQueryResult) -> Vec<String> {
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
    assert_eq!(got, expected, "chain-filtered row set mismatch");
}

/// The one-hop relationship grant of [ADR 0082] §2.
fn grant_direct_chain(env: &FederationEnv, subject: Principal) {
    let statement = format!(
        "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Doc \
         FOR (d:Doc) WHERE EXISTS {{ (d)-[:GRANTED_TO]->(a:Acct) \
         WHERE a.principal_id = MSG_CALLER() }} TO PRINCIPAL '{}'",
        subject.to_text()
    );
    gql_mutate_as_admin(env, &statement, "adr0082-grant-direct-chain");
}

/// Property-projection coverage every caller needs besides the conditional rows
/// ([ADR 0082]: the chain itself is policy-internal and demands nothing).
fn grant_common_rows(env: &FederationEnv) {
    gql_mutate_as_admin(
        env,
        &format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Doc {{ tag }} TO PUBLIC"),
        "adr0082-common-read-doc",
    );
}

fn list_grants(env: &FederationEnv) -> Vec<gleaph_router::types::GraphGrantSummary> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "list_graph_grants",
            Encode!(&GRAPH_NAME.to_string()).expect("encode"),
        )
        .expect("list_graph_grants call");
    Decode!(
        &bytes,
        Result<Vec<gleaph_router::types::GraphGrantSummary>, RouterError>
    )
    .expect("decode")
    .expect("list")
}

#[test]
fn one_hop_direct_grant_visibility_matrix_and_inline_introspection() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_direct_grant_docs(&env);
    grant_common_rows(&env);
    grant_direct_chain(&env, alice());
    grant_direct_chain(&env, bob());

    // Introspection prints the stored chain inline with catalog-resolved names
    // ([ADR 0082] §3): both member rows carry identical condition text.
    let summaries = list_grants(&env);
    let conditional: Vec<_> = summaries.iter().filter(|s| s.predicate.is_some()).collect();
    assert_eq!(
        conditional.len(),
        2,
        "two chain rows behind the root marker"
    );
    for summary in &conditional {
        assert_eq!(summary.operation, GrantOperationView::Match);
        assert!(matches!(summary.subject, GrantSubjectView::Principal(_)));
        assert_eq!(
            summary.predicate.as_deref(),
            Some(
                "WHERE EXISTS { (:Doc)-[:GRANTED_TO]->(:Acct) WHERE principal_id = MSG_CALLER() }"
            )
        );
    }

    // Alice sees exactly her granted docs; d3 appears ONCE even though it carries two
    // grant edges (semi-join: never duplicated).
    let alice_view = gql_query_as(&env, alice(), DOC_SCAN).expect("alice scan");
    assert_tags(&alice_view, &["d1", "d3"]);

    // Bob symmetric: his own docs plus the multi-match doc, never alice-only d1
    // (absence-not-error: the scan succeeds with the uncovered row missing).
    let bob_view = gql_query_as(&env, bob(), DOC_SCAN).expect("bob scan");
    assert_tags(&bob_view, &["d3", "d4"]);

    // The ungranted d2 is visible to nobody; the owner sees everything (implicit root).
    let owner_view = gql_query_as(&env, env.admin, DOC_SCAN).expect("owner scan");
    assert_tags(&owner_view, &["d1", "d2", "d3", "d4"]);
}

#[test]
fn two_hop_org_membership_visibility_matrix() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    grant_common_rows(&env);

    mutate_with_params(
        &env,
        "INSERT (:Group {name: 'eng'}), (:Group {name: 'ghost'})",
        vec![],
        "adr0082-groups",
    );
    insert_account(&env, alice(), "adr0082-org-acct-alice");
    insert_account(&env, charlie(), "adr0082-org-acct-charlie");
    mutate_with_params(
        &env,
        "MATCH (a:Acct {principal_id: $pa}), (g:Group {name: 'eng'}) \
         INSERT (a)-[:MEMBER_OF]->(g)",
        vec![("pa", gleaph_gql_ic::principal_to_value(alice()))],
        "adr0082-member-alice",
    );

    // dg_eng is shared with a group alice belongs to; dg_ghost with a group nobody
    // belongs to; d_plain has no shares at all.
    for (tag, group, key) in [
        ("dg_eng", "eng", "e"),
        ("dg_ghost", "ghost", "g"),
        ("d_plain", "", "p"),
    ] {
        mutate_with_params(
            &env,
            &format!("INSERT (:Doc {{tag: '{tag}'}})"),
            vec![],
            &format!("adr0082-org-doc-{key}"),
        );
        if !group.is_empty() {
            mutate_with_params(
                &env,
                &format!(
                    "MATCH (d:Doc {{tag: '{tag}'}}), (g:Group {{name: '{group}'}}) \
                     INSERT (d)-[:SHARED_TO]->(g)"
                ),
                vec![],
                &format!("adr0082-share-{key}"),
            );
        }
    }

    // The two-hop org-membership grant for three callers.
    for principal in [alice(), bob(), charlie()] {
        let suffix = &principal.to_text()[principal.to_text().len() - 6..];
        gql_mutate_as_admin(
            &env,
            &format!(
                "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Doc \
                 FOR (d:Doc) WHERE EXISTS {{ (d)-[:SHARED_TO]->(g:Group)<-[:MEMBER_OF]-(a:Acct) \
                 WHERE a.principal_id = MSG_CALLER() }} TO PRINCIPAL '{}'",
                principal.to_text()
            ),
            &format!("adr0082-org-chain-{suffix}"),
        );
    }

    // Alice reaches dg_eng through her membership; the ghost share and the unshared
    // doc are absent results, never errors.
    let alice_view = gql_query_as(&env, alice(), DOC_SCAN).expect("alice org scan");
    assert_tags(&alice_view, &["dg_eng"]);

    // Bob holds the same grant shape but belongs to no group: empty success.
    let bob_view = gql_query_as(&env, bob(), DOC_SCAN).expect("bob org scan");
    assert_tags(&bob_view, &[]);

    // Charlie likewise: covered, matches nothing, no error.
    let charlie_view = gql_query_as(&env, charlie(), DOC_SCAN).expect("charlie org scan");
    assert_tags(&charlie_view, &[]);
}

#[test]
fn prepared_execution_reresolves_the_chain_per_caller() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_direct_grant_docs(&env);
    grant_common_rows(&env);
    grant_direct_chain(&env, alice());
    grant_direct_chain(&env, bob());

    prepare_batch_as_admin(
        &env,
        &[gleaph_prepared_api::PreparedRegistration {
            name: "docfeed".into(),
            query: DOC_SCAN.into(),
            metadata: None,
        }],
    )
    .expect("register docfeed");
    // Invariant 7 gate: the publisher's own coverage spans the requirement set.
    publish_prepared_query_as(
        &env,
        env.admin,
        "docfeed",
        "PUBLIC",
        "adr0082-publish-docfeed",
    )
    .expect("owner publishes docfeed");

    // ONE published query, TWO callers, TWO differently-resolved chains: the lowered
    // plan is rebuilt per invocation from each caller's constants ([ADR 0082] §8).
    let feed_alice = prepared_query_with_params_as(&env, alice(), "docfeed", Vec::new());
    assert_tags(&feed_alice, &["d1", "d3"]);
    let feed_bob = prepared_query_with_params_as(&env, bob(), "docfeed", Vec::new());
    assert_tags(&feed_bob, &["d3", "d4"]);
}

// ──── Vector tail composition (ADR 0078 layer 2) ────

const EMBEDDING_NAME: &str = "adr0082_doc_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

#[test]
fn vector_search_candidates_filter_through_the_lowered_chain() {
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::vector_index::{
        VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
    };
    use gleaph_pocket_ic_tests::install_vector_canister;
    use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};

    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);

    // Integer-tag schema so low-level seeding pins deterministic embeddings.
    let ddl = format!(
        "CREATE GRAPH TYPE vbt {{ \
         NODE Doc {{ tag INT }}, \
         NODE Acct {{ principal_id STRING }}, \
         DIRECTED EDGE GrantedTo LABEL GRANTED_TO CONNECTING (Doc -> Acct) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED vbt"
    );
    gql_mutate_as_admin(&env, &ddl, "adr0082-vec-schema");

    // Register + activate the ANN index over Doc.
    let register_args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        labels: vec!["Doc".to_string()],
        metric: Some(VectorMetric::L2Squared),
        encoding: None,
        target: Some(vector),
        if_not_exists: false,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(&register_args).expect("encode register"),
        )
        .expect("admin_register_vector_index call");
    let _: bool = Decode!(&bytes, Result<bool, RouterError>)
        .expect("decode register")
        .expect("vector index registered");

    let activation_bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "set_vector_dispatch_enabled",
            Encode!(&true).expect("encode activation"),
        )
        .expect("set_vector_dispatch_enabled call");
    let _: () = Decode!(&activation_bytes, Result<(), RouterError>)
        .expect("decode activation")
        .expect("dispatch enabled");

    let routing_bytes = env
        .pic
        .update_call(
            env.graph_source,
            env.router,
            "admin_set_vector_canister",
            Encode!(&vector).expect("encode set vector routing"),
        )
        .expect("admin_set_vector_canister call");
    let _: () = Decode!(&routing_bytes, Result<(), String>)
        .expect("decode routing")
        .expect("graph accepts vector routing");

    let lookup_bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode lookup"),
        )
        .expect("get_graph_id call");
    let graph_id: GraphId = Decode!(&lookup_bytes, Result<GraphId, RouterError>)
        .expect("decode graph id")
        .expect("graph id");

    let attach_vector_bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &ShardId::new(0), &env.graph_source).expect("encode attach"),
        )
        .expect("vector attach call");
    let _: () = Decode!(&attach_vector_bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard");

    let attach_shard_args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id: ShardId::new(0),
        vector_canister: vector,
    };
    let attach_shard_bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "attach_vector_shard",
            Encode!(&attach_shard_args).expect("encode attach shard"),
        )
        .expect("attach_vector_shard call");
    let _: () = Decode!(&attach_shard_bytes, Result<(), RouterError>)
        .expect("decode attach shard")
        .expect("shard attached");

    // Vocabulary ids for low-level doc seeding with deterministic embeddings.
    let doc_label = ensure_vertex_label(&env, "Doc");
    let tag_property = ensure_property(&env, "tag");

    // Docs v=1..3 sit at squared distance DIMS·v² from the zero query, so v=1 ranks
    // globally nearest. Grants wire v=1 to BOB (hidden from alice), v=2 to alice;
    // v=3 stays ungranted.
    let mut seeded = Vec::new();
    for value in 1..=3 {
        let inserted = gleaph_pocket_ic_tests::e2e_insert_vertex_with_label_and_two_properties(
            &env,
            env.graph_source,
            doc_label.raw(),
            tag_property.raw(),
            value,
            tag_property.raw(),
            value,
        );
        seeded.push((inserted.local_vertex_id, value));
    }
    drain_maintenance_via_timer(&env, env.graph_source);
    for (vertex_id, value) in &seeded {
        let op = VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: *vertex_id,
            },
            mutation_id: 1,
            encoding: VectorEncoding::F32,
            dims: DIMS,
            metric: VectorMetric::L2Squared,
            bytes: vec_bytes(*value as f32),
            remove: false,
        };
        let upsert_bytes = env
            .pic
            .update_call(
                vector,
                env.graph_source,
                "vector_upsert",
                Encode!(&op).expect("encode upsert"),
            )
            .expect("vector_upsert call");
        let _: () = Decode!(
            &upsert_bytes,
            Result<(), gleaph_graph_kernel::vector_index::VectorCanisterError>
        )
        .expect("decode upsert")
        .expect("seed embedding");
    }

    // Relationship data through the ordinary path.
    insert_account(&env, alice(), "adr0082-vec-acct-alice");
    insert_account(&env, bob(), "adr0082-vec-acct-bob");
    for (tag, principal, key) in [("1", bob(), "v1"), ("2", alice(), "v2")] {
        mutate_with_params(
            &env,
            "MATCH (d:Doc {tag: $tag}), (a:Acct {principal_id: $p}) \
             INSERT (d)-[:GRANTED_TO]->(a)",
            vec![
                ("tag", Value::Int64(tag.parse::<i64>().expect("int tag"))),
                ("p", gleaph_gql_ic::principal_to_value(principal)),
            ],
            &format!("adr0082-vec-edge-{key}"),
        );
    }
    grant_common_rows(&env);
    grant_direct_chain(&env, alice());

    const SEARCH_QUERY: &str = "MATCH (d:Doc) \
         SEARCH d IN (VECTOR INDEX adr0082_doc_vec FOR $query LIMIT $k) DISTANCE AS distance \
         RETURN d.tag AS tag ORDER BY distance ASC";

    // Alice's globally-nearest neighbor is bob's doc; deepening past it still fills
    // k=1 with her own doc — the chain filters ANN candidates exactly like a scan.
    let search = gleaph_pocket_ic_tests::gql_query_with_params_on_router(
        &env.pic,
        alice(),
        env.router,
        SEARCH_QUERY,
        params_blob(vec![
            ("query", Value::Bytes(vec_bytes(0.0))),
            ("k", Value::Int64(1)),
        ]),
    );
    assert_eq!(search.row_count, 1, "deepening preserves k");
    let got = int_tags(&search);
    assert_eq!(got, vec![2], "hidden nearest neighbor must not surface");
}

/// Integer-tag projection for the vector fixture.
fn int_tags(result: &GqlQueryResult) -> Vec<i64> {
    use gleaph_gql_ic::GqlWireRows;
    let rows_blob = result.rows_blob.as_ref().expect("rows blob present");
    let wire = GqlWireRows::decode_blob(rows_blob).expect("decode rows");
    let mut out = Vec::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("value row");
        let Value::Int64(tag) = value_row.get("tag").expect("tag column") else {
            panic!("expected int tag, got {:?}", value_row.get("tag"));
        };
        out.push(*tag);
    }
    out
}

#[test]
fn vocabulary_drop_sweeps_stale_chains_while_identical_sibling_ids_survive() {
    let env = gleaph_pocket_ic_tests::install_two_graph_federation();
    const DROPPED: &str = "tenant_a";
    const SURVIVOR: &str = "tenant_b";

    // Identical typed schemas on both graphs: identical interning order yields
    // identical numeric label/property ids — the exact-key hazard under test.
    for graph in [DROPPED, SURVIVOR] {
        let ddl = format!(
            "CREATE GRAPH TYPE rbt_{graph} {{ \
             NODE Doc {{ tag STRING }}, \
             NODE Acct {{ principal_id STRING }}, \
             DIRECTED EDGE GrantedTo LABEL GRANTED_TO CONNECTING (Doc -> Acct) }} \
             NEXT CREATE GRAPH {graph} TYPED rbt_{graph}"
        );
        gql_mutate_as_admin(&env, &ddl, &format!("adr0082-drop-schema-{graph}"));
        // GRANT statements are standalone and address the graph via ON GRAPH.
        gql_mutate_as_admin(
            &env,
            &format!("GRANT READ ON GRAPH {graph} NODES Doc {{ tag }} TO PUBLIC"),
            &format!("adr0082-drop-common-{graph}"),
        );
        gql_mutate_as_admin(
            &env,
            &format!(
                "GRANT MATCH ON GRAPH {graph} NODES Doc \
                 FOR (d:Doc) WHERE EXISTS {{ (d)-[:GRANTED_TO]->(a:Acct) \
                 WHERE a.principal_id = MSG_CALLER() }} TO PUBLIC"
            ),
            &format!("adr0082-drop-chain-{graph}"),
        );
    }

    // Seed one granted document on the home graph (DML dispatches to the caller's
    // home graph; the sibling stays data-free by design).
    insert_account(&env, alice(), "adr0082-drop-acct-home");
    mutate_with_params(
        &env,
        "INSERT (:Doc {tag: 'kept'})",
        vec![],
        "adr0082-drop-doc-home",
    );
    mutate_with_params(
        &env,
        "MATCH (d:Doc), (a:Acct {principal_id: $p}) INSERT (d)-[:GRANTED_TO]->(a)",
        vec![("p", gleaph_gql_ic::principal_to_value(alice()))],
        "adr0082-drop-edge-home",
    );

    // Pre-drop: the PUBLIC chain row serves alice at home, and the sibling's scan
    // succeeds over its (empty) data — both chains resolve against their catalogs.
    let home_pre = scoped_scan(&env, alice(), DROPPED).expect("home serves pre-drop");
    assert_eq!(home_pre.row_count, 1);
    let survivor_pre = scoped_scan(&env, alice(), SURVIVOR).expect("sibling serves pre-drop");
    assert_eq!(survivor_pre.row_count, 0);

    // Real teardown of tenant_a (the ADR 0074 §3 invariant-4 sweep boundary).
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "unregister_shard",
            Encode!(&DROPPED.to_string(), &ShardId::new(0)).expect("encode unregister_shard"),
        )
        .expect("unregister_shard call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode")
        .expect("shard removed");
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "unregister_graph",
            Encode!(&DROPPED.to_string()).expect("encode unregister_graph"),
        )
        .expect("unregister_graph call");
    Decode!(&bytes, Result<(), RouterError>)
        .expect("decode")
        .expect("graph removed");

    // Post-drop: tenant_b's identical numeric ids keep serving the same caller —
    // a Forbidden here would mean the sweep ate the sibling's rows — while the
    // dropped graph resolves nowhere at all.
    let survivor = scoped_scan(&env, alice(), SURVIVOR).expect("sibling keeps serving");
    assert_eq!(survivor.row_count, 0);

    let dropped = scoped_scan(&env, alice(), DROPPED);
    // A non-tenant probing a dead graph gets the path-uniform denial: ad-hoc USE
    // resolution fails closed as unattributable (`Forbidden`), while name resolution
    // alone reports `NotFound`.
    assert!(
        matches!(
            &dropped,
            Err(RouterError::Forbidden) | Err(RouterError::NotFound(_))
        ),
        "dropped graph resolves nowhere: {dropped:?}"
    );
}

fn scoped_scan(
    env: &FederationEnv,
    caller: Principal,
    graph: &str,
) -> Result<GqlQueryResult, RouterError> {
    let wrapped = format!("USE {graph} {{ MATCH (d:Doc) RETURN d.tag AS tag }}");
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(
                &wrapped.to_string(),
                &Vec::<u8>::new(),
                &gleaph_graph_kernel::plan_exec::ReadMode::Eventual
            )
            .expect("encode gql_query"),
        )
        .expect("gql_query call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query")
}

#[test]
fn deny_by_default_and_grant_time_rejections_fail_closed() {
    let env = install_single_shard_federation();
    bind_typed_schema(&env);
    seed_direct_grant_docs(&env);
    grant_common_rows(&env);
    grant_direct_chain(&env, alice());

    // Deny-by-default: a stranger holds no MATCH Doc row, so the labeled scan is a
    // structural denial — the uniform Forbidden that names nothing.
    let stranger_verdict = query_verdict(&env, stranger(), DOC_SCAN);
    let anon_verdict = query_verdict(&env, Principal::anonymous(), DOC_SCAN);
    let stranger_text = match &stranger_verdict {
        Err(err) => format!("{err}"),
        Ok(_) => panic!("uncovered stranger must not receive results"),
    };
    let anon_text = match &anon_verdict {
        Err(err) => format!("{err}"),
        Ok(_) => panic!("uncovered anonymous must not receive results"),
    };
    assert_eq!(stranger_text, anon_text, "uniform non-disclosure");
    assert!(
        !stranger_text.to_lowercase().contains("match"),
        "denial must not name the missing privilege: {stranger_text}"
    );

    // GRANT-time rejections fail closed before any write. Baseline listing first.
    let baseline_len = list_grants(&env).len();

    // Unknown edge label.
    let err = mutate_verdict(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Doc \
             FOR (d:Doc) WHERE EXISTS {{ (d)-[:NO_SUCH]->(a:Acct) \
             WHERE a.principal_id = MSG_CALLER() }} TO PUBLIC"
        ),
        "adr0082-reject-unknown-edge",
    )
    .expect_err("unknown edge label must reject");
    assert!(format!("{err}").contains("NO_SUCH") || matches!(err, RouterError::NotFound(_)));

    // Directional hop over an UNDIRECTED edge label fails closed on directedness.
    let err = mutate_verdict(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Doc \
             FOR (d:Doc) WHERE EXISTS {{ (d)-[:LINK]->(x:Doc)-[:GRANTED_TO]->(a:Acct) \
             WHERE a.principal_id = MSG_CALLER() }} TO PUBLIC"
        ),
        "adr0082-reject-undirected-direction",
    )
    .expect_err("directional spelling over UNDIRECTED label must reject");
    assert!(format!("{err}").contains("UNDIRECTED"), "got {err:?}");

    // Three hops exceed the bounded chain depth.
    let err = mutate_verdict(
        &env,
        &format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Doc \
             FOR (d:Doc) WHERE EXISTS {{ (d)-[:LINK]-(m:Group)-[:SHARED_TO]->(n:Group)<-[:MEMBER_OF]-(a:Acct) \
             WHERE a.principal_id = MSG_CALLER() }} TO PUBLIC"
        ),
        "adr0082-reject-three-hops",
    )
    .expect_err("3+ hop chains must reject");
    assert!(format!("{err}").contains("hops"), "got {err:?}");

    // Nothing above leaked into storage.
    assert_eq!(
        list_grants(&env).len(),
        baseline_len,
        "rejected grants store nothing"
    );
}
