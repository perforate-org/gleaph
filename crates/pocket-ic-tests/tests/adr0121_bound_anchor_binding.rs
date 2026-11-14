//! PocketIC coverage for ADR 0121 / ADR 0046 amendments (2026-11-14): a MATCH-bound vector-search
//! anchor stays a binding after `SEARCH` on a single-shard graph.
//!
//! Before the amendments the Router dropped the anchor prefix from the search dispatch, so a
//! pattern that started from the anchor after `SEARCH` reached the Graph without the binding and
//! failed with `missing binding`. Keeping the prefix needs the Router to seed the index-anchored
//! anchor while the shard scans the searched label itself (ADR 0046 amendment).

use candid::{Decode, Encode, Principal};
use gleaph_gql::Value;
use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{RouterError, ShardId};
use gleaph_graph_kernel::plan_exec::GqlQueryResult;
use gleaph_graph_kernel::plan_exec::ReadMode;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
};
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, create_vertex_property_index, e2e_insert_edge_with_label,
    e2e_insert_vertex_with_label, e2e_insert_vertex_with_label_and_property, ensure_edge_label,
    ensure_property, ensure_vertex_label, gql_query_with_params_as_admin,
    install_single_shard_federation, install_vector_canister,
};
use gleaph_router::types::{AdminAttachVectorIndexShardArgs, RegisterVectorIndexArgs};
use std::collections::BTreeMap;

const EMBEDDING_NAME: &str = "adr0121_doc_vec_bound_anchor";
const INDEX_ID: u32 = 1;
const DIMS: u16 = 16;

fn vec_bytes(value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(DIMS as usize * 4);
    for _ in 0..DIMS {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn register_vector_index(env: &FederationEnv, metric: VectorMetric, target: Principal) {
    let args = RegisterVectorIndexArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        embedding_name: EMBEDDING_NAME.to_string(),
        index_id: INDEX_ID,
        dims: DIMS,
        labels: vec!["Document".to_string()],
        metric: Some(metric),
        encoding: None,
        target: Some(target),
        if_not_exists: false,
    };
    // The router resolves registration labels fail-closed against interned names, so intern the
    // fixture's labels before registering (ensure is idempotent; later test code re-ensures the
    // same names and observes the same VertexLabelId).
    for label in &args.labels {
        ensure_vertex_label(env, label);
    }
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

fn attach_shard_to_vector(
    env: &FederationEnv,
    vector: Principal,
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister: Principal,
) {
    let bytes = env
        .pic
        .update_call(
            vector,
            env.router,
            "admin_attach_shard_canister",
            Encode!(&graph_id, &shard_id, &shard_canister).expect("encode vector attach"),
        )
        .expect("vector admin_attach_shard_canister call");
    let _: () = Decode!(&bytes, Result<(), String>)
        .expect("decode vector attach")
        .expect("vector accepts shard");
}

fn attach_shard(env: &FederationEnv, shard_id: ShardId, vector: Principal) {
    let args = AdminAttachVectorIndexShardArgs {
        logical_graph_name: GRAPH_NAME.to_string(),
        shard_id,
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

fn router_graph_id(env: &FederationEnv) -> GraphId {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "get_graph_id",
            Encode!(&GRAPH_NAME.to_string()).expect("encode lookup"),
        )
        .expect("lookup_graph_id call");
    Decode!(&bytes, Result<GraphId, RouterError>)
        .expect("decode lookup_graph_id")
        .expect("graph id")
}

fn enable_vector_dispatch(env: &FederationEnv, vector: Principal) {
    let graph_id = router_graph_id(env);
    set_dispatch_activation(env, true);
    set_graph_vector_routing(env, env.graph_source, vector);
    attach_shard_to_vector(env, vector, graph_id, ShardId::new(0), env.graph_source);
    attach_shard(env, ShardId::new(0), vector);
}

fn seed_embedding(
    env: &FederationEnv,
    vector: Principal,
    shard_canister: Principal,
    vertex_id: u32,
    value: f32,
) {
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
            shard_canister,
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

fn extract_rows(result: GqlQueryResult) -> Vec<BTreeMap<String, gleaph_gql_ic::GqlWireValue>> {
    let rows_blob = result.rows_blob.expect("rows blob");
    let wire = GqlWireRows::decode_blob(&rows_blob).expect("decode rows");
    wire.rows
        .into_iter()
        .map(|row| row.columns.into_iter().collect())
        .collect()
}

#[allow(dead_code)]
fn gql_query_with_params_as_admin_result(
    env: &FederationEnv,
    query: &str,
    params: Vec<u8>,
) -> Result<GqlQueryResult, RouterError> {
    let bytes = env
        .pic
        .query_call(
            env.router,
            env.admin,
            "gql_query",
            Encode!(&query.to_string(), &params, &ReadMode::Eventual).expect("encode gql_query"),
        )
        .expect("gql_query call");
    Decode!(&bytes, Result<GqlQueryResult, RouterError>).expect("decode gql_query result")
}

#[test]
fn bound_anchor_stays_a_binding_after_search_on_a_single_shard() {
    let env = install_single_shard_federation();
    let vector = install_vector_canister(&env.pic, env.router);
    register_vector_index(&env, VectorMetric::L2Squared, vector);
    enable_vector_dispatch(&env, vector);

    let author_label_id = ensure_vertex_label(&env, "Author").raw();
    let doc_label_id = ensure_vertex_label(&env, "Document").raw();
    let wrote_label_id = ensure_edge_label(&env, "WROTE").raw();
    let cat_id = ensure_property(&env, "category").raw();
    create_vertex_property_index(
        &env,
        "document_category_idx",
        "Document",
        "category",
        "create_document_category_idx",
    );

    // The anchor is the only Document with category 7; its author wrote it.
    let author = e2e_insert_vertex_with_label(&env, env.graph_source, author_label_id);
    let anchor =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 7);
    let near =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);
    let far =
        e2e_insert_vertex_with_label_and_property(&env, env.graph_source, doc_label_id, cat_id, 1);
    e2e_insert_edge_with_label(
        &env,
        env.graph_source,
        author.local_vertex_id,
        anchor.local_vertex_id,
        wrote_label_id,
    );
    seed_embedding(&env, vector, env.graph_source, anchor.local_vertex_id, 0.0);
    seed_embedding(&env, vector, env.graph_source, near.local_vertex_id, 1.0);
    seed_embedding(&env, vector, env.graph_source, far.local_vertex_id, 4.0);

    // The pattern after SEARCH starts from the anchor: before the fix this reached the Graph
    // without `anchor` and failed with `missing binding`.
    let query = format!(
        "MATCH (anchor:Document {{category: 7}}) \
         MATCH (d:Document) \
         SEARCH d IN (VECTOR INDEX {EMBEDDING_NAME} FOR anchor.embedding LIMIT 3) \
           DISTANCE AS distance \
         MATCH (anchor)<-[:WROTE]-(a:Author) \
         RETURN a, d, anchor.category AS anchor_category, distance \
         ORDER BY distance ASC"
    );
    let params = gleaph_gql_ic::wire::encode_gql_params_blob(Vec::new()).expect("encode params");
    let rows = extract_rows(gql_query_with_params_as_admin(&env, &query, params));

    assert_eq!(
        rows.len(),
        3,
        "every searched document joins the anchor's one author"
    );
    for row in &rows {
        assert!(
            row.contains_key("a"),
            "the author bound from the anchor after SEARCH"
        );
        assert!(matches!(
            row.get("anchor_category"),
            Some(GqlWireValue::Int64(7))
        ));
    }
    let distances: Vec<f64> = rows
        .iter()
        .map(|row| match row.get("distance") {
            Some(GqlWireValue::Float64(value)) => *value,
            other => panic!("distance column: {other:?}"),
        })
        .collect();
    assert!(
        distances.windows(2).all(|pair| pair[0] <= pair[1]),
        "rows keep the search order: {distances:?}"
    );
}
