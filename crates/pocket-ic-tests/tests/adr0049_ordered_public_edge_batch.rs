//! PocketIC: ADR 0049 ordered public edge-batch admission and exact-key replay.

use gleaph_graph_kernel::federation::{ElementIdEncodingKey, RouterError, encode_global_vertex_id};
use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;
use gleaph_pocket_ic_tests::{
    FederationEnv, admin_intern_edge_label, e2e_insert_vertex, execute_ordered_edge_batch_as_admin,
    federation_graph_element_id_encoding_key_bytes, install_single_shard_federation,
};
use gleaph_router::types::{
    OrderedEdgeBatchPublicRequest, OrderedEdgeBatchPublicRequestV1, OrderedEdgeInsertPublicItemV1,
};

fn ordered_request(env: &FederationEnv) -> OrderedEdgeBatchPublicRequest {
    admin_intern_edge_label(env, "KNOWS");
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
            edge_label_name: Some("KNOWS".into()),
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
