//! PocketIC: ADR 0049 ordered public edge-batch admission and exact-key replay.

use gleaph_gql::Value;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, RouterError, encode_global_vertex_id};
use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;
use gleaph_pocket_ic_tests::{
    FederationEnv, ensure_edge_label, ensure_property, ensure_vertex_label,
    arm_router_fault, e2e_insert_vertex, e2e_reverse_resolved_edge_property,
    evict_graph_mutation_journal, batch_insert_as_admin,
    federation_graph_element_id_encoding_key_bytes, gql_execute_as_admin,
    gql_query_as_admin, install_single_shard_federation, get_mutation_status_as_admin,
    run_router_recovery_timer,
};
use gleaph_router::types::{
    BatchEdgeInsertV1, BatchEndpointV1, BatchOperationV1, BatchPropertyV1, BatchRequest,
    BatchRequestV1, BatchVertexInsertV1,
};

fn ordered_vertex_request(key: &str) -> BatchRequest {
    BatchRequest::V1(BatchRequestV1 {
        client_mutation_key: key.into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        operations: vec![
            BatchOperationV1::Vertex(BatchVertexInsertV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
            BatchOperationV1::Vertex(BatchVertexInsertV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
        ],
    })
}

fn ordered_request(env: &FederationEnv) -> BatchRequest {
    let source = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(env));
    let source = encode_global_vertex_id(&key, source);
    let target = encode_global_vertex_id(&key, target);
    BatchRequest::V1(BatchRequestV1 {
        client_mutation_key: "adr0049-public-replay".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        operations: vec![BatchOperationV1::Edge(BatchEdgeInsertV1 {
            source: BatchEndpointV1::Existing(source.0.to_vec()),
            target: BatchEndpointV1::Existing(target.0.to_vec()),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        })],
    })
}

fn ordered_mixed_request(env: &FederationEnv, key: &str) -> BatchRequest {
    let target = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let encoding_key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(env));
    BatchRequest::V1(BatchRequestV1 {
        client_mutation_key: key.into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        operations: vec![
            BatchOperationV1::Vertex(BatchVertexInsertV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
            BatchOperationV1::Edge(BatchEdgeInsertV1 {
                source: BatchEndpointV1::NewVertexOrdinal(0),
                target: BatchEndpointV1::Existing(
                    encode_global_vertex_id(&encoding_key, target).0.to_vec(),
                ),
                directed: true,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: Vec::new(),
            }),
        ],
    })
}

fn query_scores(env: &FederationEnv, query: &str) -> Vec<i128> {
    let result = gql_query_as_admin(env, query);
    let rows = gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(
        result.rows_blob.as_ref().expect("rows blob"),
    )
    .expect("decode rows")
    .try_into_value_rows()
    .expect("convert rows");
    let values = rows
        .iter()
        .map(|row| row.get("score").and_then(Value::as_i128))
        .collect::<Vec<_>>();
    assert!(
        values.iter().all(Option::is_some),
        "score value missing in {query}: rows={rows:?}"
    );
    values.into_iter().map(Option::unwrap).collect()
}

#[test]
fn ordered_public_edge_batch_commits_and_replays_exactly() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    let first =
        batch_insert_as_admin(&env, request.clone()).expect("ordered public batch must commit");
    assert_eq!(first.status.phase, MutationLifecyclePhase::Completed);
    let mutation_id = first.status.mutation_id;
    let receipt = first.receipt.clone().expect("completed batch receipt");
    assert_eq!(receipt.logical_edge_count, 1);

    let replay = batch_insert_as_admin(&env, request)
        .expect("exact ordered public retry must be idempotent");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, mutation_id);
    assert_eq!(replay.receipt, Some(receipt));
}

#[test]
fn ordered_public_edge_batch_rejects_missing_catalog_name_before_reservation() {
    let env = install_single_shard_federation();
    let source = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let request = BatchRequest::V1(BatchRequestV1 {
        client_mutation_key: "adr0049-missing-label".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        operations: vec![BatchOperationV1::Edge(BatchEdgeInsertV1 {
            source: BatchEndpointV1::Existing(encode_global_vertex_id(&key, source).0.to_vec()),
            target: BatchEndpointV1::Existing(encode_global_vertex_id(&key, target).0.to_vec()),
            directed: true,
            edge_label_name: Some("MISSING".into()),
            inline_property: None,
            initial_edge_properties: Vec::new(),
        })],
    });
    let error = batch_insert_as_admin(&env, request).unwrap_err();
    assert!(matches!(error, RouterError::NotFound(name) if name == "MISSING"));
}

#[test]
fn ordered_public_edge_batch_persists_property_and_inline_values() {
    let env = install_single_shard_federation();
    gql_execute_as_admin(
        &env,
        "CREATE GRAPH TYPE IF NOT EXISTS road_type { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { distance UINT16 INLINE } CONNECTING (city -> city) } NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED road_type",
        "adr0049-inline-schema",
    );
    let edge_label = ensure_edge_label(&env, "ROAD");
    let score = ensure_property(&env, "score");
    let source = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let request = BatchRequest::V1(BatchRequestV1 {
        client_mutation_key: "adr0049-property-inline".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        operations: vec![BatchOperationV1::Edge(BatchEdgeInsertV1 {
            source: BatchEndpointV1::Existing(encode_global_vertex_id(&key, source).0.to_vec()),
            target: BatchEndpointV1::Existing(encode_global_vertex_id(&key, target).0.to_vec()),
            directed: true,
            edge_label_name: Some("ROAD".into()),
            inline_property: Some(3u16.to_le_bytes().to_vec()),
            initial_edge_properties: vec![BatchPropertyV1 {
                property_name: "score".into(),
                value: Value::Int64(42).to_binary_bytes().expect("encode score"),
            }],
        })],
    });

    let status = batch_insert_as_admin(&env, request)
        .expect("property and inline ordered batch must commit");
    assert_eq!(status.status.phase, MutationLifecyclePhase::Completed);
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
fn ordered_public_edge_batch_preserves_mixed_shapes_and_parallel_properties() {
    let env = install_single_shard_federation();
    ensure_edge_label(&env, "ROAD");
    let score = ensure_property(&env, "score");

    let source_result = e2e_insert_vertex(&env, env.graph_source);
    let source = source_result.global_vertex_id;
    let target_result = e2e_insert_vertex(&env, env.graph_source);
    let target = target_result.global_vertex_id;
    let other = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let loop_vertex = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let encoded = |vertex| encode_global_vertex_id(&key, vertex).0.to_vec();
    let property = |score| BatchPropertyV1 {
        property_name: "score".into(),
        value: Value::Int64(score)
            .to_binary_bytes()
            .expect("encode score property"),
    };
    let request = BatchRequest::V1(BatchRequestV1 {
        client_mutation_key: "adr0049-mixed-shapes".into(),
        logical_graph_name: gleaph_pocket_ic_tests::GRAPH_NAME.into(),
        operations: vec![
            BatchOperationV1::Edge(BatchEdgeInsertV1 {
                source: BatchEndpointV1::Existing(encoded(source)),
                target: BatchEndpointV1::Existing(encoded(target)),
                directed: true,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(10)],
            }),
            BatchOperationV1::Edge(BatchEdgeInsertV1 {
                source: BatchEndpointV1::Existing(encoded(source)),
                target: BatchEndpointV1::Existing(encoded(target)),
                directed: true,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(20)],
            }),
            BatchOperationV1::Edge(BatchEdgeInsertV1 {
                source: BatchEndpointV1::Existing(encoded(target)),
                target: BatchEndpointV1::Existing(encoded(other)),
                directed: false,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(30)],
            }),
            BatchOperationV1::Edge(BatchEdgeInsertV1 {
                source: BatchEndpointV1::Existing(encoded(loop_vertex)),
                target: BatchEndpointV1::Existing(encoded(loop_vertex)),
                directed: false,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(40)],
            }),
        ],
    });

    let status =
        batch_insert_as_admin(&env, request.clone()).expect("mixed ordered batch must commit");
    assert_eq!(status.status.phase, MutationLifecyclePhase::Completed);
    let replay = batch_insert_as_admin(&env, request)
        .expect("mixed ordered batch replay must be idempotent");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, status.status.mutation_id);
    assert_eq!(
        e2e_reverse_resolved_edge_property(
            &env,
            env.graph_source,
            source_result.local_vertex_id,
            target_result.local_vertex_id,
            score.raw(),
        ),
        Some(10),
        "batch sidecar must resolve through the reverse alias path"
    );

    for (name, edge_pattern, expected_scores) in [
        ("left only", "<-[e:ROAD]-", vec![10, 20]),
        ("undirected only", "~[e:ROAD]~", vec![30, 30, 40]),
        ("right only", "-[e:ROAD]->", vec![10, 20]),
        (
            "left or undirected",
            "<~[e:ROAD]~",
            vec![10, 20, 30, 30, 40],
        ),
        (
            "undirected or right",
            "~[e:ROAD]~>",
            vec![10, 20, 30, 30, 40],
        ),
        ("directed only", "<-[e:ROAD]->", vec![10, 10, 20, 20]),
        (
            "all directions",
            "-[e:ROAD]-",
            vec![10, 10, 20, 20, 30, 30, 40],
        ),
        (
            "all directions alias",
            "<~[e:ROAD]~>",
            vec![10, 10, 20, 20, 30, 30, 40],
        ),
    ] {
        let query =
            format!("MATCH (a){edge_pattern}(b) RETURN e.score AS score ORDER BY score ASC");
        assert_eq!(
            query_scores(&env, &query),
            expected_scores,
            "edge pattern {name}"
        );
    }
}

#[test]
fn ordered_public_edge_batch_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);
    arm_router_fault(&env, 4);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(
        matches!(error, RouterError::Internal(message) if message.contains("canonical commit fault"))
    );

    arm_router_fault(&env, 0);
    let before_recovery = get_mutation_status_as_admin(
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
    let status = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("recovered ordered mutation status");
    assert_eq!(status.phase, MutationLifecyclePhase::Completed);
}

#[test]
fn ordered_public_edge_batch_recovers_after_retirement_acknowledgement_loss() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    // Graph has already retired the journal entry when Router loses the callback. The durable
    // Router record must remain RetirementPending, never be treated as a fresh canonical write.
    arm_router_fault(&env, 5);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message)
            if message.contains("retirement acknowledgement fault")
    ));

    arm_router_fault(&env, 0);
    let pending = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("retirement-pending ordered mutation status");
    // Router intentionally projects the internal RetirementPending state as the public
    // ProjectionPending phase; the retirement sub-state is durable but not a client enum variant.
    assert_eq!(pending.phase, MutationLifecyclePhase::ProjectionPending);

    run_router_recovery_timer(&env);
    let completed = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("recovered ordered mutation status");
    assert_eq!(completed.phase, MutationLifecyclePhase::Completed);

    // An exact retry after recovery returns the same mutation rather than adding another edge.
    let replay =
        batch_insert_as_admin(&env, request).expect("completed ordered retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.mutation_id);
}

#[test]
fn ordered_public_edge_batch_stays_pending_when_retired_journal_is_evicted() {
    use std::time::Duration;

    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    // Graph retires the mutation, but Router loses the acknowledgement before recording its
    // terminal state.
    arm_router_fault(&env, 5);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message)
            if message.contains("retirement acknowledgement fault")
    ));
    arm_router_fault(&env, 0);

    let pending = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("retirement-pending ordered mutation status");
    assert_eq!(pending.phase, MutationLifecyclePhase::ProjectionPending);

    // After the real 9-day retention window, the retired Graph journal is absent. Router must
    // stay fail-closed instead of inferring completion or redispatching the canonical mutation.
    env.pic
        .advance_time(Duration::from_secs(9 * 24 * 60 * 60 + 60 * 60));
    assert_eq!(evict_graph_mutation_journal(&env, env.graph_source), 0);
    run_router_recovery_timer(&env);

    let after_absent = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("ordered mutation must remain retained for repair");
    assert_eq!(
        after_absent.phase,
        MutationLifecyclePhase::ProjectionPending
    );
    assert_eq!(after_absent.mutation_id, pending.mutation_id);

    // The exact retry observes the same durable Router record; it does not create another edge.
    let retry = batch_insert_as_admin(&env, request)
        .expect("exact retry must remain a repairable pending mutation");
    assert_eq!(
        retry.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );
    assert_eq!(retry.status.mutation_id, pending.mutation_id);
}

#[test]
fn ordered_public_vertex_batch_commits_replays_and_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_vertex_request("adr0049-public-vertex-replay");

    arm_router_fault(&env, 4);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        gleaph_graph_kernel::federation::RouterError::Internal(message)
            if message.contains("vertex canonical commit fault")
    ));

    arm_router_fault(&env, 0);
    let pending = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-replay",
    )
    .expect("vertex canonical receipt status");
    assert_eq!(pending.phase, MutationLifecyclePhase::CanonicalCommitted);

    run_router_recovery_timer(&env);
    let completed = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-replay",
    )
    .expect("recovered ordered vertex mutation status");
    assert_eq!(completed.phase, MutationLifecyclePhase::Completed);

    let replay =
        batch_insert_as_admin(&env, request).expect("completed ordered vertex retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.mutation_id);
}

#[test]
fn ordered_public_vertex_batch_recovers_after_retirement_acknowledgement_loss() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_vertex_request("adr0049-public-vertex-retirement");

    arm_router_fault(&env, 5);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        gleaph_graph_kernel::federation::RouterError::Internal(message)
            if message.contains("vertex retirement acknowledgement fault")
    ));

    arm_router_fault(&env, 0);
    let pending = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-retirement",
    )
    .expect("vertex retirement-pending status");
    assert_eq!(pending.phase, MutationLifecyclePhase::ProjectionPending);

    run_router_recovery_timer(&env);
    let completed = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-retirement",
    )
    .expect("recovered ordered vertex retirement status");
    assert_eq!(completed.phase, MutationLifecyclePhase::Completed);

    let replay =
        batch_insert_as_admin(&env, request).expect("completed ordered vertex retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.mutation_id);
}

#[test]
fn ordered_public_mixed_batch_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_mixed_request(&env, "adr0049-public-mixed-replay");

    arm_router_fault(&env, 4);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message) if message.contains("canonical commit fault")
    ));

    arm_router_fault(&env, 0);
    let pending = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-replay",
    )
    .expect("mixed canonical receipt status");
    assert_eq!(pending.phase, MutationLifecyclePhase::CanonicalCommitted);

    run_router_recovery_timer(&env);
    let completed = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-replay",
    )
    .expect("recovered mixed mutation status");
    assert_eq!(completed.phase, MutationLifecyclePhase::Completed);
}

#[test]
fn ordered_public_mixed_batch_recovers_after_retirement_acknowledgement_loss() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_mixed_request(&env, "adr0049-public-mixed-retirement");

    arm_router_fault(&env, 5);
    let error = batch_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message)
            if message.contains("retirement acknowledgement fault")
    ));

    arm_router_fault(&env, 0);
    let pending = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-retirement",
    )
    .expect("mixed retirement-pending status");
    assert_eq!(pending.phase, MutationLifecyclePhase::ProjectionPending);

    run_router_recovery_timer(&env);
    let completed = get_mutation_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-retirement",
    )
    .expect("recovered mixed retirement status");
    assert_eq!(completed.phase, MutationLifecyclePhase::Completed);

    let replay = batch_insert_as_admin(&env, request).expect("completed mixed retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.mutation_id);
}
