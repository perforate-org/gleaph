//! PocketIC: ADR 0049 ordered public edge-batch admission and exact-key replay.

use gleaph_gql::Value;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, RouterError, encode_global_vertex_id};
use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, admin_intern_property, arm_router_fault,
    e2e_insert_vertex, execute_ordered_edge_batch_as_admin,
    federation_graph_element_id_encoding_key_bytes, gql_execute_idempotent_as_admin,
    gql_query_as_admin, install_single_shard_federation, mutation_status_as_admin,
    run_router_recovery_timer,
};
use gleaph_router::types::{
    OrderedEdgeBatchPublicRequest, OrderedEdgeBatchPublicRequestV1, OrderedEdgeInsertPublicItemV1,
};

fn ordered_request(env: &FederationEnv) -> OrderedEdgeBatchPublicRequest {
    let source = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(env));
    let source = encode_global_vertex_id(&key, source);
    let target = encode_global_vertex_id(&key, target);
    OrderedEdgeBatchPublicRequest::V1(OrderedEdgeBatchPublicRequestV1 {
        client_mutation_key: "adr0049-public-replay".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        items: vec![OrderedEdgeInsertPublicItemV1 {
            source: source.0.to_vec(),
            target: target.0.to_vec(),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }],
    })
}

#[test]
fn ordered_public_edge_batch_commits_and_replays_exactly() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    let first = execute_ordered_edge_batch_as_admin(&env, request.clone())
        .expect("ordered public batch must commit");
    assert_eq!(first.phase, MutationLifecyclePhase::Completed);
    let mutation_id = first.mutation_id;

    let replay = execute_ordered_edge_batch_as_admin(&env, request)
        .expect("exact ordered public retry must be idempotent");
    assert_eq!(replay.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.mutation_id, mutation_id);
}

#[test]
fn ordered_public_edge_batch_rejects_missing_catalog_name_before_reservation() {
    let env = install_single_shard_federation();
    let source = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let request = OrderedEdgeBatchPublicRequest::V1(OrderedEdgeBatchPublicRequestV1 {
        client_mutation_key: "adr0049-missing-label".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        items: vec![OrderedEdgeInsertPublicItemV1 {
            source: encode_global_vertex_id(&key, source).0.to_vec(),
            target: encode_global_vertex_id(&key, target).0.to_vec(),
            directed: true,
            edge_label_name: Some("MISSING".into()),
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }],
    });
    let error = execute_ordered_edge_batch_as_admin(&env, request).unwrap_err();
    assert!(matches!(error, RouterError::NotFound(name) if name == "MISSING"));
}

#[test]
fn ordered_public_edge_batch_persists_property_and_inline_values() {
    let env = install_single_shard_federation();
    gql_execute_idempotent_as_admin(
        &env,
        "CREATE EDGE LABEL ROAD { distance UINT16 INLINE }",
        "adr0049-inline-schema",
    );
    let edge_label = admin_intern_edge_label(&env, "ROAD");
    let score = admin_intern_property(&env, "score");
    let source = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let request = OrderedEdgeBatchPublicRequest::V1(OrderedEdgeBatchPublicRequestV1 {
        client_mutation_key: "adr0049-property-inline".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        items: vec![OrderedEdgeInsertPublicItemV1 {
            source: encode_global_vertex_id(&key, source).0.to_vec(),
            target: encode_global_vertex_id(&key, target).0.to_vec(),
            directed: true,
            edge_label_name: Some("ROAD".into()),
            inline_property: Some(3u16.to_le_bytes().to_vec()),
            initial_edge_properties: vec![gleaph_router::types::OrderedEdgePropertyPublicV1 {
                property_name: "score".into(),
                value: Value::Int64(42).to_binary_bytes().expect("encode score"),
            }],
        }],
    });

    let status = execute_ordered_edge_batch_as_admin(&env, request)
        .expect("property and inline ordered batch must commit");
    assert_eq!(status.phase, MutationLifecyclePhase::Completed);
    assert!(edge_label.raw() > 0);
    assert!(score.raw() > 0);

    let result = gql_query_as_admin(&env, "MATCH (a)-[e:ROAD]->(b) RETURN e.score AS score");
    assert_eq!(result.row_count, 1);
    let rows = gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(
        result.rows_blob.as_ref().expect("rows blob"),
    )
    .expect("decode rows")
    .try_into_value_rows()
    .expect("convert rows");
    assert_eq!(rows[0].get("score"), Some(&Value::Int64(42)));
    let inline_predicate = gql_query_as_admin(
        &env,
        "MATCH (a)-[e:ROAD]->(b) WHERE e.distance = 3 RETURN b",
    );
    assert_eq!(inline_predicate.row_count, 1);
}

#[test]
fn ordered_public_edge_batch_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);
    arm_router_fault(&env, 4);
    let error = execute_ordered_edge_batch_as_admin(&env, request.clone()).unwrap_err();
    assert!(
        matches!(error, RouterError::Internal(message) if message.contains("canonical commit fault"))
    );

    arm_router_fault(&env, 0);
    let before_recovery = mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("canonical receipt status");
    assert_eq!(
        before_recovery.phase,
        MutationLifecyclePhase::CanonicalCommitted
    );
    run_router_recovery_timer(&env);
    let status = mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("recovered ordered mutation status");
    assert_eq!(status.phase, MutationLifecyclePhase::Completed);
}
