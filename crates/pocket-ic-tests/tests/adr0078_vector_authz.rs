//! PocketIC: ADR 0078 — authz-aware vector search with iterative deepening.
//!
//! One suite proves the three-layer contract end to end:
//!
//! 1. **k-preservation**: hidden (policy-invisible) vertices rank globally highest; an
//!    anonymous caller still receives exactly k authorized rows because deepening
//!    re-requests `ceil(k·2^r)` candidates until the visible subset fills k. The naive
//!    fetch-k-then-filter flow would have returned zero rows here.
//! 2. **Hidden-heavy exhaustion**: fewer than k authorized vertices exist; rounds stop at
//!    candidate exhaustion and the result carries `truncated = Some(true)` with the
//!    deterministic authorized prefix, identically across repeated invocations.
//! 3. **Policy parity**: conditional grants filter SEARCH candidates exactly as they
//!    filter an ordinary labeled scan — the PUBLIC/member union matrix of [ADR 0075]
//!    applied to vector results, with the ordinary-scan baseline equivalence.
//! 4. **Layer-1 admission**: a caller holding no `MATCH` row on the index-spanned label
//!    is rejected with the uniform `Forbidden` before any dispatch — observable as an
//!    error rather than a filtered empty success (post-dispatch filtering yields empty
//!    successes, never errors). The walker runs in
//!    `enforce_data_plane_authorization`, which precedes `try_execute_gql_search` in the
//!    Router query path, so rejection precedes ANN spend by construction.
//! 5. **Edge-subject visibility (ADR 0078 §6)**: edge-subject vectors ride traversal
//!    coverage including direction — the outgoing pattern runs, the reverse pattern is
//!    uniformly denied until the incoming row is granted. The fused `GLEAPH.VECTOR.*`
//!    predicate itself adds no demand beyond the traversal row (unit-pinned in
//!    `crates/router/src/authz.rs`
//!    `edge_inline_vector_rides_the_direction_aware_traversal_row`); there is no public
//!    inline-vector-profile fixture path, so the E2E proves the enforcement machinery
//!    the §6 translation rides on.
//!
//! Expected counts stated before running (per plan contract):
//! - 5 `#[test]` functions.
//! - Each test constructs one PocketIC environment (`install_single_shard_federation`)
//!   installing exactly 4 canisters: router, index, one graph shard, one vector canister.
//! - Per environment: 1 vector-index registration + ~4 activation/attach admin calls,
//!   ≤8 vertex inserts each with one embedding upsert and ≤1 property patch, ≤2 edge
//!   inserts for the edge-subject probes, ≤6 GRANT mutates, and ≤7 probe queries.

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::plan_exec::{GqlQueryResult, ReadMode};
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, e2e_insert_edge_with_label,
    e2e_insert_vertex_with_label, e2e_insert_vertex_with_label_and_two_properties,
    e2e_set_vertex_property, ensure_edge_label, ensure_property, ensure_vertex_label,
    install_single_shard_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};

const EMBEDDING_NAME: &str = "adr0078_doc_vec";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

fn alice() -> Principal {
    Principal::from_slice(&[0xA1; 29])
}

/// A principal holding no grants and no tenancy: default-deny everywhere.
fn stranger() -> Principal {
    Principal::from_slice(&[0xC3; 29])
}

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// The canonical all-zeros query vector; an embedding component `v` sits at squared
/// distance `DIMS · v²`, so distinct components give a strict deterministic ranking.
fn query_bytes() -> Vec<u8> {
    vec_bytes(0.0)
}

fn search_params(k: i64) -> Vec<u8> {
    gleaph_gql_ic::wire::encode_gql_params_blob(vec![
        ("query".to_string(), Value::Bytes(query_bytes())),
        ("k".to_string(), Value::Int64(k)),
    ])
    .expect("encode search params")
}

const SEARCH_QUERY: &str = "MATCH (d:Document) \
     SEARCH d IN ( \
       VECTOR INDEX adr0078_doc_vec FOR $query LIMIT $k \
     ) DISTANCE AS distance \
     RETURN d.tag AS tag ORDER BY distance ASC";

/// The ordinary labeled-scan baseline over the same predicate matrix ([ADR 0075] §4).
const SCAN_QUERY: &str = "MATCH (d:Document) RETURN d.tag AS tag ORDER BY d.tag ASC";

fn register_vector_index(env: &FederationEnv, target: Principal) {
    let args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        labels: vec!["Document".to_string()],
        metric: Some(VectorMetric::L2Squared),
        encoding: None,
        target: Some(target),
        if_not_exists: false,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "admin_register_vector_index",
            Encode!(&args).expect("encode register"),
        )
        .expect("admin_register_vector_index call");
    let _: bool = Decode!(&bytes, Result<bool, RouterError>)
        .expect("decode register result")
        .expect("register vector index");
}

fn set_dispatch_activation(env: &FederationEnv, enabled: bool) {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "set_vector_dispatch_enabled",
            Encode!(&enabled).expect("encode activation"),
        )
        .expect("admin_set_vector_dispatch_activation call");
    let _: () = Decode!(&bytes, Result<(), RouterError>)
        .expect("decode activation result")
        .expect("set activation");
}

fn set_graph_vector_routing(env: &FederationEnv, graph: Principal, vector: Principal) {
    let bytes = env
        .pic
        .update_call(
            graph,
            env.router,
            "admin_set_vector_canister",
            Encode!(&vector).expect("encode set vector routing"),
        )
        .expect("admin_set_vector_canister call");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode set vector routing")
        .expect("graph accepts vector routing");
}

fn attach_shard_to_vector(env: &FederationEnv, vector: Principal, graph_id: GraphId) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &ShardId::new(0), &env.graph_source).expect("encode attach"),
        )
        .expect("vector admin_attach_shard_canister call");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard");
}

fn attach_shard(env: &FederationEnv, vector: Principal) {
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id: ShardId::new(0),
        vector_canister: vector,
    };
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "attach_vector_shard",
            Encode!(&args).expect("encode attach"),
        )
        .expect("admin_attach_vector_index_shard call");
    let _: () = Decode!(&bytes, Result<(), RouterError>)
        .expect("decode attach result")
        .expect("attach shard");
}

fn enable_vector_dispatch(env: &FederationEnv, vector: Principal) {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode lookup"),
        )
        .expect("get_graph_id call");
    let graph_id = Decode!(&bytes, Result<GraphId, RouterError>)
        .expect("decode graph id")
        .expect("graph id");
    set_dispatch_activation(env, true);
    set_graph_vector_routing(env, env.graph_source, vector);
    attach_shard_to_vector(env, vector, graph_id);
    attach_shard(env, vector);
}

fn seed_embedding(env: &FederationEnv, vector: Principal, vertex_id: u32, value: f32) {
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id,
        },
        mutation_id: 1,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        bytes: vec_bytes(value),
        remove: false,
    };
    let bytes = env
        .pic
        .update_call(
            vector,
            env.graph_source,
            "vector_upsert",
            Encode!(&op).expect("encode upsert"),
        )
        .expect("vector_upsert call");
    let _: () = Decode!(
        &bytes,
        Result<(), gleaph_graph_kernel::vector_index::VectorCanisterError>
    )
    .expect("decode upsert result")
    .expect("seed embedding");
}

/// One Document vertex: integer `tag` plus `visibility` (0 public / 1 private); an
/// optional integer `owner`; plus its embedding at component `value`. When owned, the
/// `owner` property holds the document's own tag, so the member grant below can pin
/// exactly one private document through `d.owner = <tag> AND d.visibility = 1`.
fn seed_document(
    env: &FederationEnv,
    vector: Principal,
    doc_label_id: u16,
    visibility_id: u32,
    tag_id: u32,
    owner_id: Option<u32>,
    tag: i64,
    visibility: i64,
    value: f32,
) {
    let doc = e2e_insert_vertex_with_label_and_two_properties(
        env,
        env.graph_source,
        doc_label_id,
        visibility_id,
        visibility,
        tag_id,
        tag,
    );
    if let Some(owner_property_id) = owner_id {
        e2e_set_vertex_property(
            env,
            env.graph_source,
            doc.local_vertex_id,
            owner_property_id,
            tag,
        );
    }
    seed_embedding(env, vector, doc.local_vertex_id, value);
}

fn grant(env: &FederationEnv, statement: String, key: &str) {
    mutate_as(env, &statement, key).expect("grant accepted");
}

fn mutate_as(
    env: &FederationEnv,
    query: &str,
    client_mutation_key: &str,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .update_call(
            env.router,
            env.admin,
            "gql_mutate",
            Encode!(
                &query.to_string(),
                &Vec::<u8>::new(),
                &client_mutation_key.to_string()
            )
            .expect("encode gql_mutate"),
        )
        .expect("gql_mutate call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(outcome) => outcome,
        Err(err) => panic!("decode gql_mutate: {err}"),
    }
}

/// Issue one GQL query as `caller`, preserving the typed verdict so layer-1 rejections
/// are observable as errors rather than panics.
fn query_as(
    env: &FederationEnv,
    caller: Principal,
    query: &str,
    params_blob: Vec<u8>,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            caller,
            "gql_query",
            Encode!(&query.to_string(), &params_blob, &ReadMode::Eventual)
                .expect("encode gql_query"),
        )
        .expect("gql_query call");
    match Decode!(&bytes, Result<GqlQueryResult, RouterError>) {
        Ok(outcome) => outcome,
        Err(err) => panic!("decode gql_query: {err}"),
    }
}

fn search_as(env: &FederationEnv, caller: Principal, k: i64) -> Result<GqlQueryResult, RouterError> {
    query_as(env, caller, SEARCH_QUERY, search_params(k))
}

fn tags(result: &GqlQueryResult) -> Vec<i64> {
    use gleaph_gql_ic::GqlWireRows;
    let rows_blob = result.rows_blob.as_ref().expect("rows blob present");
    let wire = GqlWireRows::decode_blob(rows_blob).expect("decode rows");
    let mut tags = Vec::new();
    for row in wire.rows {
        let value_row = row.try_into_value_row().expect("value row");
        match value_row.get("tag").expect("tag column") {
            Value::Int64(tag) => tags.push(*tag),
            other => panic!("expected int tag, got {other:?}"),
        }
    }
    assert_eq!(tags.len() as u64, result.row_count, "row_count matches rows");
    tags
}

/// Vocabulary + dispatch wiring shared by the vertex-search tests: Document label, int
/// properties, and the derived vector canister attached to the single shard.
fn setup_env() -> (FederationEnv, Principal) {
    let env = install_single_shard_federation();
    // Vocabulary must resolve before registration: the creation-fixed label set is
    // validated against the graph-scoped catalogs.
    ensure_vertex_label(&env, "Document");
    ensure_property(&env, "visibility");
    ensure_property(&env, "tag");
    ensure_property(&env, "owner");
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, vector);
    enable_vector_dispatch(&env, vector);
    (env, vector)
}

fn doc_label_and_property_ids(env: &FederationEnv) -> (u16, u32, u32, u32) {
    (
        ensure_vertex_label(env, "Document").raw(),
        ensure_property(env, "visibility").raw(),
        ensure_property(env, "tag").raw(),
        ensure_property(env, "owner").raw(),
    )
}

#[test]
fn k_preserved_when_hidden_vertices_rank_higher() {
    let (env, vector) = setup_env();
    let (doc_label, visibility_id, tag_id, _) = doc_label_and_property_ids(&env);

    // Global ranking by ascending distance (query = zeros): both PRIVATE documents rank
    // above both public ones, so fetch-k-then-filter would return zero rows for k=2.
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 1, 1, 0.25);
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 2, 1, 0.5);
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 3, 0, 1.0);
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 4, 0, 1.5);

    // PUBLIC narrows to public documents; the tag projection rides the READ row.
    grant(
        &env,
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document FOR (d:Document) WHERE d.visibility = 0 TO PUBLIC"),
        "adr0078-grant-public-docs",
    );
    grant(
        &env,
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Document {{ tag }} TO PUBLIC"),
        "adr0078-grant-tag-projection",
    );

    // Deepening recovers the true top-2 of the authorized subset.
    let result = search_as(&env, Principal::anonymous(), 2).expect("k preserved");
    assert_eq!(tags(&result), vec![3, 4], "authorized top-2 in score order");
    assert_eq!(result.truncated, Some(false), "converged search is not truncated");

    // Deterministic ordering: identical state, identical authorized prefix.
    let again = search_as(&env, Principal::anonymous(), 2).expect("repeat");
    assert_eq!(tags(&again), tags(&result));
    assert_eq!(again.truncated, Some(false));

    // k=3 exceeds the authorized subset size: full prefix, explicit truncation.
    let over = search_as(&env, Principal::anonymous(), 3).expect("partial at k=3");
    assert_eq!(tags(&over), vec![3, 4]);
    assert_eq!(over.truncated, Some(true));
}

#[test]
fn hidden_heavy_graph_returns_partial_rows_with_truncated_marker() {
    let (env, vector) = setup_env();
    let (doc_label, visibility_id, tag_id, _) = doc_label_and_property_ids(&env);

    // Six private documents occupy ranks 1..=6; only two public documents exist below
    // them, so k=3 forces deepening until candidate exhaustion.
    for (tag, value) in [(10, 0.05), (11, 0.1), (12, 0.15), (13, 0.2), (14, 0.25), (15, 0.3)]
    {
        seed_document(&env, vector, doc_label, visibility_id, tag_id, None, tag, 1, value);
    }
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 20, 0, 1.0);
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 21, 0, 1.5);

    grant(
        &env,
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document FOR (d:Document) WHERE d.visibility = 0 TO PUBLIC"),
        "adr0078-grant-public-docs",
    );
    grant(
        &env,
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Document {{ tag }} TO PUBLIC"),
        "adr0078-grant-tag-projection",
    );

    let result = search_as(&env, Principal::anonymous(), 3).expect("partial result");
    assert_eq!(tags(&result), vec![20, 21], "authorized prefix in score order");
    assert_eq!(result.truncated, Some(true), "candidate exhaustion sets the marker");

    // Determinism across identical states: same prefix, same marker.
    let repeat = search_as(&env, Principal::anonymous(), 3).expect("repeat partial");
    assert_eq!(tags(&repeat), vec![20, 21]);
    assert_eq!(repeat.truncated, Some(true));
}

#[test]
fn policy_predicates_filter_candidates_like_ordinary_rows() {
    let (env, vector) = setup_env();
    let (doc_label, visibility_id, tag_id, owner_id) = doc_label_and_property_ids(&env);

    // Public pair plus two private documents; private document 31 belongs to alice
    // (owner pinned to its own tag, see [`seed_document`]).
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 30, 0, 0.5);
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 40, 0, 2.5);
    seed_document(
        &env, vector, doc_label, visibility_id, tag_id, Some(owner_id), 31, 1, 1.0,
    );
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 41, 1, 1.5);

    grant(
        &env,
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document FOR (d:Document) WHERE d.visibility = 0 TO PUBLIC"),
        "adr0078-grant-public-docs",
    );
    grant(
        &env,
        format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document \
             FOR (d:Document) WHERE d.owner = 31 AND d.visibility = 1 \
             TO PRINCIPAL '{}'",
            alice().to_text()
        ),
        "adr0078-grant-alice-private",
    );
    grant(
        &env,
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Document {{ tag }} TO PUBLIC"),
        "adr0078-grant-tag-projection",
    );

    // Anonymous callers: candidates filtered to public documents exactly like the scan.
    // Exactly two authorized rows exist, so k=2 converges...
    let anon = search_as(&env, Principal::anonymous(), 2).expect("anon search");
    assert_eq!(tags(&anon), vec![30, 40]);
    assert_eq!(anon.truncated, Some(false));
    // ...and k=4 exceeds the authorized subset size: full prefix plus the marker.
    let anon_over = search_as(&env, Principal::anonymous(), 4).expect("anon oversampled");
    assert_eq!(tags(&anon_over), vec![30, 40]);
    assert_eq!(anon_over.truncated, Some(true));

    // Alice observes the PUBLIC ∪ own-private union through vector candidates; her
    // three authorized rows satisfy k=3 exactly.
    let alice_view = search_as(&env, alice(), 3).expect("alice search");
    assert_eq!(tags(&alice_view), vec![30, 31, 40]);
    assert_eq!(alice_view.truncated, Some(false));

    // Parity evidence: the plain labeled scan under the identical predicate matrix
    // returns the same row set ([ADR 0075] §4 equivalence carried onto candidates).
    let baseline = query_as(&env, alice(), SCAN_QUERY, Vec::new()).expect("scan baseline");
    assert_eq!(tags(&baseline), tags(&alice_view), "policy ≡ ordinary-row filtering");

    // Stranger: only the PUBLIC row applies inside candidate filtering.
    let stranger_view = search_as(&env, stranger(), 2).expect("stranger search");
    assert_eq!(tags(&stranger_view), vec![30, 40]);
}

#[test]
fn layer1_rejects_unauthorized_callers_before_ann_dispatch() {
    let (env, vector) = setup_env();
    let (doc_label, visibility_id, tag_id, _) = doc_label_and_property_ids(&env);

    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 1, 0, 1.0);
    seed_document(&env, vector, doc_label, visibility_id, tag_id, None, 2, 0, 1.5);

    // Graph-context resolution requires every caller to hold at least one visible
    // grant row, so the denial probes hold an irrelevant User row: they can address
    // the graph while remaining uncovered for the Document rows the search demands.
    ensure_vertex_label(&env, "User");
    grant(
        &env,
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES User TO PUBLIC"),
        "adr0078-grant-context-user",
    );

    // Rows scoping the search itself are held by ONE principal only. PUBLIC must stay
    // rowless for Document: every caller evaluates `caller ∪ PUBLIC`, so a PUBLIC row
    // would admit the denial probes too.
    grant(
        &env,
        format!(
            "GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document TO PRINCIPAL '{}'",
            alice().to_text()
        ),
        "adr0078-grant-alice-match",
    );
    grant(
        &env,
        format!(
            "GRANT READ ON GRAPH {GRAPH_NAME} NODES Document {{ tag }} TO PRINCIPAL '{}'",
            alice().to_text()
        ),
        "adr0078-grant-alice-tag",
    );

    // The granted principal runs the full pipeline end to end.
    let ok = search_as(&env, alice(), 2).expect("granted caller");
    assert_eq!(tags(&ok), vec![1, 2]);
    assert_eq!(ok.truncated, Some(false));

    // A stranger holds no MATCH row on the spanned label (and PUBLIC holds none):
    // uniform Forbidden. Because post-dispatch policy filtering yields empty successes
    // (never errors), the error distinguishes pre-dispatch admission from late
    // filtering.
    let denied = search_as(&env, stranger(), 2).expect_err("uncovered demand denied");
    assert!(matches!(denied, RouterError::Forbidden), "got {denied:?}");

    // Conjunction proof: another grantless caller is denied identically — every
    // demanded row must be covered, no partial coverage suffices.
    let other_denied =
        search_as(&env, Principal::anonymous(), 2).expect_err("grantless caller denied");
    assert!(matches!(other_denied, RouterError::Forbidden));
}

#[test]
fn edge_subject_visibility_is_direction_aware() {
    let env = install_single_shard_federation();

    // Directional traversal rows are only grantable for labels whose directedness the
    // schema declares, so this probe binds a typed graph ([ADR 0075] fixture pattern).
    let ddl = format!(
        "CREATE GRAPH TYPE adr0078_pgt {{ NODE User, NODE Document, \
         DIRECTED EDGE wrote LABEL WROTE CONNECTING (User -> Document) }} \
         NEXT CREATE GRAPH {GRAPH_NAME} TYPED adr0078_pgt"
    );
    mutate_as(&env, &ddl, "adr0078-bind-typed-schema").expect("typed graph bound");

    let user_label = ensure_vertex_label(&env, "User").raw();
    let doc_label = ensure_vertex_label(&env, "Document").raw();
    let wrote_label = ensure_edge_label(&env, "WROTE").raw();

    // Two users writing edges into one shared document.
    let author_a = e2e_insert_vertex_with_label(&env, env.graph_source, user_label);
    let author_b = e2e_insert_vertex_with_label(&env, env.graph_source, user_label);
    let doc = e2e_insert_vertex_with_label(&env, env.graph_source, doc_label);
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        author_a.local_vertex_id,
        doc.local_vertex_id,
        wrote_label,
    );
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        author_b.local_vertex_id,
        doc.local_vertex_id,
        wrote_label,
    );

    grant(
        &env,
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES User TO PUBLIC"),
        "adr0078-grant-user-match",
    );
    grant(
        &env,
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES User TO PUBLIC"),
        "adr0078-grant-user-read",
    );
    grant(
        &env,
        format!("GRANT MATCH ON GRAPH {GRAPH_NAME} NODES Document TO PUBLIC"),
        "adr0078-grant-document-match",
    );
    grant(
        &env,
        format!("GRANT READ ON GRAPH {GRAPH_NAME} NODES Document TO PUBLIC"),
        "adr0078-grant-document-read",
    );
    grant(
        &env,
        format!("GRANT TRAVERSE OUTGOING ON GRAPH {GRAPH_NAME} EDGES WROTE TO PUBLIC"),
        "adr0078-grant-wrote-outgoing",
    );

    const FORWARD: &str = "MATCH (u:User)-[w:WROTE]->(d:Document) RETURN count(*) AS edges";
    const REVERSE: &str = "MATCH (d:Document)<-[w:WROTE]-(u:User) RETURN count(*) AS edges";

    // Outgoing coverage admits the forward pattern: both edges are visible candidates.
    // The bound source vertex hydrates its full property map, so the READ row is part
    // of the demanded set alongside MATCH and the directional TRAVERSE row.
    let forward = query_as(&env, Principal::anonymous(), FORWARD, Vec::new())
        .expect("outgoing pattern admitted");
    assert_eq!(forward.row_count, 1, "aggregate produces exactly one row");
    {
        use gleaph_gql_ic::GqlWireRows;
        let blob = forward.rows_blob.expect("rows blob");
        let wire = GqlWireRows::decode_blob(&blob).expect("decode rows");
        let row = wire.rows[0].clone().try_into_value_row().expect("value row");
        assert_eq!(row.get("edges"), Some(&Value::Int64(2)), "both edges visible");
    }

    // Without the INCOMING row the reverse pattern is uniformly denied: those edges are
    // invisible as candidates in that direction (admission-time denial, layer 1).
    let reverse = query_as(&env, Principal::anonymous(), REVERSE, Vec::new());
    assert!(
        matches!(reverse, Err(RouterError::Forbidden)),
        "reverse traversal without the directional row must be denied, got {reverse:?}"
    );

    // Granting the incoming row admits the same pattern: coverage is direction-aware.
    grant(
        &env,
        format!("GRANT TRAVERSE INCOMING ON GRAPH {GRAPH_NAME} EDGES WROTE TO PUBLIC"),
        "adr0078-grant-wrote-incoming",
    );
    let reverse_ok = query_as(&env, Principal::anonymous(), REVERSE, Vec::new())
        .expect("incoming row admits reverse pattern");
    assert_eq!(reverse_ok.row_count, 1);
}
