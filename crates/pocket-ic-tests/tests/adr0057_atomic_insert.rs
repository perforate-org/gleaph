//! PocketIC: ADR 0049/0057 atomic-insert admission, receipts, and exact-key replay.

use gleaph_gql::Value;
use gleaph_graph_kernel::federation::{
    ENCODED_VERTEX_ID_BYTES, ElementIdEncodingKey, RouterError, encode_global_vertex_id,
};
use gleaph_graph_kernel::plan_exec::MutationLifecyclePhase;
use gleaph_pocket_ic_tests::{
    FederationEnv, arm_router_fault, atomic_insert_as_admin, atomic_insert_status_as_admin,
    e2e_insert_vertex, e2e_reverse_resolved_edge_property, ensure_edge_label, ensure_property,
    ensure_vertex_label, evict_graph_mutation_journal,
    federation_graph_element_id_encoding_key_bytes, gql_mutate_as_admin, gql_query_as_admin,
    install_single_shard_federation, run_router_recovery_timer,
};
use gleaph_router::types::{
    AtomicInsertEdgeV1, AtomicInsertEndpointV1, AtomicInsertOperationV1, AtomicInsertPropertyV1,
    AtomicInsertRequest, AtomicInsertRequestV1, AtomicInsertVertexV1,
};

fn ordered_vertex_request(key: &str) -> AtomicInsertRequest {
    AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: key.into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![
            AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
            AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
        ],
    })
}

fn ordered_request(env: &FederationEnv) -> AtomicInsertRequest {
    let source = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(env));
    let source = encode_global_vertex_id(&key, source);
    let target = encode_global_vertex_id(&key, target);
    AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: "adr0049-public-replay".into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
            source: AtomicInsertEndpointV1::Existing(source.0.to_vec()),
            target: AtomicInsertEndpointV1::Existing(target.0.to_vec()),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        })],
    })
}

fn ordered_mixed_request(env: &FederationEnv, key: &str) -> AtomicInsertRequest {
    let target = e2e_insert_vertex(env, env.graph_source).global_vertex_id;
    let encoding_key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(env));
    AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: key.into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![
            AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
            AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                target: AtomicInsertEndpointV1::Existing(
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

fn ordered_edge_request(key: &str, source: Vec<u8>, target: Vec<u8>) -> AtomicInsertRequest {
    AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: key.into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
            source: AtomicInsertEndpointV1::Existing(source),
            target: AtomicInsertEndpointV1::Existing(target),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        })],
    })
}

fn query_scores(env: &FederationEnv, query: &str) -> Vec<i128> {
    let result = gql_query_as_admin(env, query);
    let rows =
        gleaph_gql_ic::GqlWireRows::decode_blob(result.rows_blob.as_ref().expect("rows blob"))
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
fn atomic_insert_commits_and_replays_exactly() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    let first = atomic_insert_as_admin(&env, request.clone()).expect("atomic insert must commit");
    assert_eq!(first.status.phase, MutationLifecyclePhase::Completed);
    let mutation_id = first.status.mutation_id;
    let receipt = first
        .receipt
        .clone()
        .expect("completed atomic-insert receipt");
    assert_eq!(receipt.logical_edge_count, 1);

    let replay = atomic_insert_as_admin(&env, request)
        .expect("exact ordered public retry must be idempotent");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, mutation_id);
    assert_eq!(replay.receipt, Some(receipt));
}

#[test]
fn atomic_insert_vertex_status_and_retry_preserve_allocated_ids_after_response_loss() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_vertex_request("adr0049-public-vertex-response-loss");

    // The fault returns an application error immediately after Graph durably commits the vertex
    // receipt. This models a lost update response without allowing a second canonical allocation.
    arm_router_fault(&env, 4);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message) if message.contains("vertex canonical commit fault")
    ));
    arm_router_fault(&env, 0);

    let status = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-response-loss",
    )
    .expect("atomic_insert_status must recover the durable receipt");
    assert_eq!(
        status.status.phase,
        MutationLifecyclePhase::CanonicalCommitted
    );
    let receipt = status
        .receipt
        .clone()
        .expect("vertex receipt after response loss");
    assert_eq!(receipt.logical_vertex_count, 2);
    assert_eq!(receipt.logical_edge_count, 0);
    assert_eq!(receipt.allocated_vertex_ids.len(), 2);
    assert!(
        receipt
            .allocated_vertex_ids
            .iter()
            .all(|id| id.len() == ENCODED_VERTEX_ID_BYTES)
    );

    let retry = atomic_insert_as_admin(&env, request.clone())
        .expect("same-key retry must return the durable response");
    assert_eq!(retry.status.mutation_id, status.status.mutation_id);
    assert_eq!(retry.receipt, Some(receipt.clone()));

    run_router_recovery_timer(&env);
    let completed = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-response-loss",
    )
    .expect("recovered atomic_insert_status");
    assert_eq!(completed.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(completed.receipt, Some(receipt.clone()));

    let replay = atomic_insert_as_admin(&env, request).expect("completed retry must replay");
    assert_eq!(replay.receipt, Some(receipt));

    // The canonical write happened exactly once: recovery makes the two original vertices visible,
    // and an idempotent retry does not create a duplicate pair.
    let visible = gql_query_as_admin(&env, "MATCH (n) RETURN n");
    assert_eq!(visible.row_count, 2);
}

#[test]
fn atomic_insert_mixed_allocated_id_is_immediately_usable_by_edge_insert() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");

    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let encoding_key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let target_encoded = encode_global_vertex_id(&encoding_key, target).0.to_vec();
    let mixed = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: "adr0049-public-mixed-immediate-edge".into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![
            AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }),
            AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                target: AtomicInsertEndpointV1::Existing(target_encoded.clone()),
                directed: true,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: Vec::new(),
            }),
        ],
    });

    let mixed_response = atomic_insert_as_admin(&env, mixed).expect("mixed atomic insert");
    assert_eq!(
        mixed_response.status.phase,
        MutationLifecyclePhase::Completed
    );
    let mixed_receipt = mixed_response.receipt.expect("mixed receipt");
    assert_eq!(mixed_receipt.logical_vertex_count, 1);
    assert_eq!(mixed_receipt.logical_edge_count, 1);
    assert_eq!(mixed_receipt.allocated_vertex_ids.len(), 1);
    let allocated_id = mixed_receipt.allocated_vertex_ids[0].clone();
    assert_eq!(allocated_id.len(), ENCODED_VERTEX_ID_BYTES);

    // No MATCH, index wait, or label-stats polling occurs between the mixed response and this
    // edge-only request. Graph validates the returned canonical vertex directly.
    let edge_response = atomic_insert_as_admin(
        &env,
        ordered_edge_request(
            "adr0049-public-mixed-immediate-edge-followup",
            allocated_id,
            target_encoded,
        ),
    )
    .expect("edge-only atomic insert must accept the returned ID immediately");
    assert_eq!(
        edge_response.status.phase,
        MutationLifecyclePhase::Completed
    );
    let edge_receipt = edge_response.receipt.expect("edge-only receipt");
    assert_eq!(edge_receipt.logical_operation_count, 1);
    assert_eq!(edge_receipt.logical_vertex_count, 0);
    assert_eq!(edge_receipt.logical_edge_count, 1);
    assert!(edge_receipt.allocated_vertex_ids.is_empty());
}

#[test]
fn atomic_insert_rejects_missing_catalog_name_before_reservation() {
    let env = install_single_shard_federation();
    let source = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: "adr0049-missing-label".into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
            source: AtomicInsertEndpointV1::Existing(
                encode_global_vertex_id(&key, source).0.to_vec(),
            ),
            target: AtomicInsertEndpointV1::Existing(
                encode_global_vertex_id(&key, target).0.to_vec(),
            ),
            directed: true,
            edge_label_name: Some("MISSING".into()),
            inline_property: None,
            initial_edge_properties: Vec::new(),
        })],
    });
    let error = atomic_insert_as_admin(&env, request).unwrap_err();
    assert!(matches!(error, RouterError::NotFound(name) if name == "MISSING"));
}

#[test]
fn atomic_insert_persists_property_and_inline_values() {
    let env = install_single_shard_federation();
    gql_mutate_as_admin(
        &env,
        "CREATE GRAPH TYPE IF NOT EXISTS road_type { NODE City AS city, DIRECTED EDGE Road LABEL ROAD { distance UINT16 INLINE } CONNECTING (city -> city) } NEXT CREATE GRAPH IF NOT EXISTS gleaph.pocket_ic TYPED road_type",
        "adr0049-inline-schema",
    );
    let edge_label = ensure_edge_label(&env, "ROAD");
    let score = ensure_property(&env, "score");
    let source = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let target = e2e_insert_vertex(&env, env.graph_source).global_vertex_id;
    let key = ElementIdEncodingKey(federation_graph_element_id_encoding_key_bytes(&env));
    let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: "adr0049-property-inline".into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
            source: AtomicInsertEndpointV1::Existing(
                encode_global_vertex_id(&key, source).0.to_vec(),
            ),
            target: AtomicInsertEndpointV1::Existing(
                encode_global_vertex_id(&key, target).0.to_vec(),
            ),
            directed: true,
            edge_label_name: Some("ROAD".into()),
            inline_property: Some(3u16.to_le_bytes().to_vec()),
            initial_edge_properties: vec![AtomicInsertPropertyV1 {
                property_name: "score".into(),
                value: Value::Int64(42).to_binary_bytes().expect("encode score"),
            }],
        })],
    });

    let status = atomic_insert_as_admin(&env, request)
        .expect("property and inline atomic insert must commit");
    assert_eq!(status.status.phase, MutationLifecyclePhase::Completed);
    assert!(edge_label.raw() > 0);
    assert!(score.raw() > 0);

    let result = gql_query_as_admin(&env, "MATCH (a)-[e:ROAD]->(b) RETURN e.score AS score");
    assert_eq!(result.row_count, 1);
    let rows =
        gleaph_gql_ic::GqlWireRows::decode_blob(result.rows_blob.as_ref().expect("rows blob"))
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
fn atomic_insert_preserves_mixed_shapes_and_parallel_properties() {
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
    let property = |score| AtomicInsertPropertyV1 {
        property_name: "score".into(),
        value: Value::Int64(score)
            .to_binary_bytes()
            .expect("encode score property"),
    };
    let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: "adr0049-mixed-shapes".into(),
        graph_name: Some(gleaph_pocket_ic_tests::GRAPH_NAME.into()),
        operations: vec![
            AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(encoded(source)),
                target: AtomicInsertEndpointV1::Existing(encoded(target)),
                directed: true,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(10)],
            }),
            AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(encoded(source)),
                target: AtomicInsertEndpointV1::Existing(encoded(target)),
                directed: true,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(20)],
            }),
            AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(encoded(target)),
                target: AtomicInsertEndpointV1::Existing(encoded(other)),
                directed: false,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(30)],
            }),
            AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(encoded(loop_vertex)),
                target: AtomicInsertEndpointV1::Existing(encoded(loop_vertex)),
                directed: false,
                edge_label_name: Some("ROAD".into()),
                inline_property: None,
                initial_edge_properties: vec![property(40)],
            }),
        ],
    });

    let status =
        atomic_insert_as_admin(&env, request.clone()).expect("mixed atomic insert must commit");
    assert_eq!(status.status.phase, MutationLifecyclePhase::Completed);
    let replay = atomic_insert_as_admin(&env, request)
        .expect("mixed atomic-insert replay must be idempotent");
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
        "atomic-insert sidecar must resolve through the reverse alias path"
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
fn atomic_insert_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);
    arm_router_fault(&env, 4);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(
        matches!(error, RouterError::Internal(message) if message.contains("canonical commit fault"))
    );

    arm_router_fault(&env, 0);
    let before_recovery = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("canonical receipt status");
    assert_eq!(
        before_recovery.status.phase,
        MutationLifecyclePhase::CanonicalCommitted
    );
    run_router_recovery_timer(&env);
    let status = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("recovered ordered mutation status");
    assert_eq!(status.status.phase, MutationLifecyclePhase::Completed);
}

#[test]
fn atomic_insert_recovers_after_retirement_acknowledgement_loss() {
    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    // Graph has already retired the journal entry when Router loses the callback. The durable
    // Router record must remain RetirementPending, never be treated as a fresh canonical write.
    arm_router_fault(&env, 5);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message)
            if message.contains("retirement acknowledgement fault")
    ));

    arm_router_fault(&env, 0);
    let pending = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("retirement-pending ordered mutation status");
    // Router intentionally projects the internal RetirementPending state as the public
    // ProjectionPending phase; the retirement sub-state is durable but not a client enum variant.
    assert_eq!(
        pending.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );

    run_router_recovery_timer(&env);
    let completed = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("recovered ordered mutation status");
    assert_eq!(completed.status.phase, MutationLifecyclePhase::Completed);

    // An exact retry after recovery returns the same mutation rather than adding another edge.
    let replay =
        atomic_insert_as_admin(&env, request).expect("completed ordered retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.status.mutation_id);
}

#[test]
fn atomic_insert_stays_pending_when_retired_journal_is_evicted() {
    use std::time::Duration;

    let env = install_single_shard_federation();
    let request = ordered_request(&env);

    // Graph retires the mutation, but Router loses the acknowledgement before recording its
    // terminal state.
    arm_router_fault(&env, 5);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message)
            if message.contains("retirement acknowledgement fault")
    ));
    arm_router_fault(&env, 0);

    let pending = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("retirement-pending ordered mutation status");
    assert_eq!(
        pending.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );

    // After the real 9-day retention window, the retired Graph journal is absent. Router must
    // stay fail-closed instead of inferring completion or redispatching the canonical mutation.
    env.pic
        .advance_time(Duration::from_secs(9 * 24 * 60 * 60 + 60 * 60));
    assert_eq!(evict_graph_mutation_journal(&env, env.graph_source), 0);
    run_router_recovery_timer(&env);

    let after_absent = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-replay",
    )
    .expect("ordered mutation must remain retained for repair");
    assert_eq!(
        after_absent.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );
    assert_eq!(after_absent.status.mutation_id, pending.status.mutation_id);

    // The exact retry observes the same durable Router record; it does not create another edge.
    let retry = atomic_insert_as_admin(&env, request)
        .expect("exact retry must remain a repairable pending mutation");
    assert_eq!(
        retry.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );
    assert_eq!(retry.status.mutation_id, pending.status.mutation_id);
}

#[test]
fn atomic_insert_vertex_commits_replays_and_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_vertex_request("adr0049-public-vertex-replay");

    arm_router_fault(&env, 4);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        gleaph_graph_kernel::federation::RouterError::Internal(message)
            if message.contains("vertex canonical commit fault")
    ));

    arm_router_fault(&env, 0);
    let pending = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-replay",
    )
    .expect("vertex canonical receipt status");
    assert_eq!(
        pending.status.phase,
        MutationLifecyclePhase::CanonicalCommitted
    );

    run_router_recovery_timer(&env);
    let completed = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-replay",
    )
    .expect("recovered ordered vertex mutation status");
    assert_eq!(completed.status.phase, MutationLifecyclePhase::Completed);

    let replay =
        atomic_insert_as_admin(&env, request).expect("completed ordered vertex retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.status.mutation_id);
}

#[test]
fn atomic_insert_vertex_recovers_after_retirement_acknowledgement_loss() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_vertex_request("adr0049-public-vertex-retirement");

    arm_router_fault(&env, 5);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        gleaph_graph_kernel::federation::RouterError::Internal(message)
            if message.contains("vertex retirement acknowledgement fault")
    ));

    arm_router_fault(&env, 0);
    let pending = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-retirement",
    )
    .expect("vertex retirement-pending status");
    assert_eq!(
        pending.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );

    run_router_recovery_timer(&env);
    let completed = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-vertex-retirement",
    )
    .expect("recovered ordered vertex retirement status");
    assert_eq!(completed.status.phase, MutationLifecyclePhase::Completed);

    let replay =
        atomic_insert_as_admin(&env, request).expect("completed ordered vertex retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.status.mutation_id);
}

#[test]
fn atomic_insert_mixed_recovers_after_canonical_receipt_failure() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_mixed_request(&env, "adr0049-public-mixed-replay");

    arm_router_fault(&env, 4);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message) if message.contains("canonical commit fault")
    ));

    arm_router_fault(&env, 0);
    let pending = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-replay",
    )
    .expect("mixed canonical receipt status");
    assert_eq!(
        pending.status.phase,
        MutationLifecyclePhase::CanonicalCommitted
    );

    run_router_recovery_timer(&env);
    let completed = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-replay",
    )
    .expect("recovered mixed mutation status");
    assert_eq!(completed.status.phase, MutationLifecyclePhase::Completed);
}

#[test]
fn atomic_insert_mixed_recovers_after_retirement_acknowledgement_loss() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    let request = ordered_mixed_request(&env, "adr0049-public-mixed-retirement");

    arm_router_fault(&env, 5);
    let error = atomic_insert_as_admin(&env, request.clone()).unwrap_err();
    assert!(matches!(
        error,
        RouterError::Internal(message)
            if message.contains("retirement acknowledgement fault")
    ));

    arm_router_fault(&env, 0);
    let pending = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-retirement",
    )
    .expect("mixed retirement-pending status");
    assert_eq!(
        pending.status.phase,
        MutationLifecyclePhase::ProjectionPending
    );

    run_router_recovery_timer(&env);
    let completed = atomic_insert_status_as_admin(
        &env,
        gleaph_pocket_ic_tests::GRAPH_NAME,
        "adr0049-public-mixed-retirement",
    )
    .expect("recovered mixed retirement status");
    assert_eq!(completed.status.phase, MutationLifecyclePhase::Completed);

    let replay = atomic_insert_as_admin(&env, request).expect("completed mixed retry must replay");
    assert_eq!(replay.status.phase, MutationLifecyclePhase::Completed);
    assert_eq!(replay.status.mutation_id, completed.status.mutation_id);
}
