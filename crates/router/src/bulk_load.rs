//! Router-owned public durable bulk-load workflow (ADR 0057).
//!
//! The parent and receipt-map transitions live in the Router store facade. This module owns only
//! public command validation, graph-request construction, pinned-shard dispatch, and public status
//! canonical work is dispatched only from an explicitly submitted client command.

use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId, ShardId};
use gleaph_graph_kernel::plan_exec::{
    GraphOrderedEdgeBatchResult, GraphOrderedEdgeBatchResultV1, GraphOrderedVertexBatchResult,
    GraphOrderedVertexBatchResultV1, MutationId, OrderedBatchExecutionModeV1,
    OrderedEdgeBatchGraphArgs, OrderedEdgeBatchGraphArgsV1, OrderedMutationRetirementAck,
    OrderedMutationRetirementAckV1, OrderedMutationRetirementArgs, OrderedMutationRetirementArgsV1,
    OrderedVertexBatchGraphArgs, OrderedVertexBatchGraphArgsV1, OrderedVertexMutationRetirementAck,
    OrderedVertexMutationRetirementAckV1, OrderedVertexMutationRetirementArgs,
    OrderedVertexMutationRetirementArgsV1, ShardEventSeq,
};
use ic_cdk::api::{msg_caller, time};

use crate::facade::stable::bulk_load::{
    BulkLoadChunkEnvelopeV1, BulkLoadChunkProgressV1, BulkLoadChunkReceiptRecordV1,
    BulkLoadGraphReceiptV1, BulkLoadGraphRequestV1,
};
use crate::facade::stable::label_stats::{
    BulkLoadCoordinatorV1, BulkLoadLifecycleV1, BulkLoadTargetV1, RouterMutationPayloadV1,
    RouterMutationRecord, RouterMutationRequestIdentityV1,
};
use crate::facade::store::RouterStore;
use crate::facade::store::bulk_load::BulkLoadStartAdmission;
use crate::graph_client::{
    execute_ordered_edge_batch_on_graph, execute_ordered_vertex_batch_on_graph,
    retire_ordered_mutation_on_graph, retire_ordered_vertex_mutation_on_graph,
};
use crate::index_lookup::RouterIndexLookup;
use crate::state::RouterError;
use crate::types::{
    AtomicInsertEndpointV1, AtomicInsertOperationV1, AtomicInsertRequest, AtomicInsertRequestV1,
    BulkLoadChunkReceiptV1, BulkLoadChunkV1, BulkLoadCommand, BulkLoadEndpointV1,
    BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
};

fn invalid(message: impl Into<String>) -> RouterError {
    RouterError::InvalidArgument(message.into())
}

fn bulk_parent(record: &RouterMutationRecord) -> Result<&BulkLoadCoordinatorV1, RouterError> {
    if !matches!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::BulkLoadJob
    ) {
        return Err(RouterError::Conflict(
            "client_bulk_key belongs to a different mutation family".into(),
        ));
    }
    match record.payload() {
        RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) => {
            coordinator
                .validate()
                .unwrap_or_else(|error| panic!("invalid durable bulk-load coordinator: {error}"));
            Ok(coordinator)
        }
        _ => panic!("bulk-load identity/payload family mismatch in durable Router record"),
    }
}

fn bulk_record(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_key: &str,
) -> Result<RouterMutationRecord, RouterError> {
    let record = store
        .router_mutation_record(caller, graph_id, client_key)
        .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
    bulk_parent(&record)?;
    Ok(record)
}

fn public_state(coordinator: &BulkLoadCoordinatorV1) -> BulkLoadPublicStateV1 {
    match &coordinator.lifecycle {
        BulkLoadLifecycleV1::Open => BulkLoadPublicStateV1::Open,
        BulkLoadLifecycleV1::AppendPending { .. } => BulkLoadPublicStateV1::AppendPending,
        BulkLoadLifecycleV1::FinalizePending { .. } => BulkLoadPublicStateV1::FinalizePending,
        BulkLoadLifecycleV1::AbortPending { .. } => BulkLoadPublicStateV1::AbortPending,
        BulkLoadLifecycleV1::Completed => BulkLoadPublicStateV1::Completed,
        BulkLoadLifecycleV1::Aborted => BulkLoadPublicStateV1::Aborted,
        BulkLoadLifecycleV1::Failed { reason } => BulkLoadPublicStateV1::Failed {
            reason: reason.clone(),
        },
    }
}

fn terminal_expiry(terminal_at_ns: Option<u64>) -> Option<u64> {
    terminal_at_ns.map(|at| at.saturating_add(crate::facade::store::CLIENT_MUTATION_KEY_TTL_NS))
}

fn target_from_latest_shard(
    store: &RouterStore,
    graph_id: GraphId,
) -> Result<BulkLoadTargetV1, RouterError> {
    let routing =
        crate::federation::latest_shard_routing(&store.list_live_shards_for_graph_id(graph_id)?)?;
    let shard = routing
        .into_iter()
        .next()
        .ok_or(RouterError::ShardNotRegistered)?;
    Ok(BulkLoadTargetV1 {
        shard_id: shard.shard_id,
        graph_canister: shard.graph_canister,
    })
}

fn start_target_for_key(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_bulk_key: &str,
) -> Result<BulkLoadTargetV1, RouterError> {
    match store.router_mutation_record(caller, graph_id, client_bulk_key) {
        Some(record) => Ok(bulk_parent(&record)?.target.clone()),
        None => target_from_latest_shard(store, graph_id),
    }
}

fn atomic_request_from_chunk(
    graph_name: Option<String>,
    client_bulk_key: &str,
    chunk: &BulkLoadChunkV1,
) -> Result<AtomicInsertRequest, RouterError> {
    let operations = match chunk {
        BulkLoadChunkV1::Vertices(items) => items
            .iter()
            .cloned()
            .map(AtomicInsertOperationV1::Vertex)
            .collect(),
        BulkLoadChunkV1::Edges(items) => items
            .iter()
            .cloned()
            .map(|item| {
                let source = existing_endpoint_bytes(item.source)?;
                let target = existing_endpoint_bytes(item.target)?;
                Ok(AtomicInsertOperationV1::Edge(
                    crate::types::AtomicInsertEdgeV1 {
                        source: AtomicInsertEndpointV1::Existing(source),
                        target: AtomicInsertEndpointV1::Existing(target),
                        directed: item.directed,
                        edge_label_name: item.edge_label_name,
                        inline_property: item.inline_property,
                        initial_edge_properties: item.initial_edge_properties,
                    },
                ))
            })
            .collect::<Result<Vec<_>, RouterError>>()?,
    };
    Ok(AtomicInsertRequest::V1(AtomicInsertRequestV1 {
        client_mutation_key: client_bulk_key.to_owned(),
        graph_name,
        operations,
    }))
}

/// Extract the encoded existing vertex ID from a bulk-load endpoint. Property-based endpoints
/// must have been resolved by [`resolve_by_property_endpoints`] before the graph request is
/// built; reaching this point with one is a Router invariant violation and fails closed.
fn existing_endpoint_bytes(endpoint: BulkLoadEndpointV1) -> Result<Vec<u8>, RouterError> {
    match endpoint {
        BulkLoadEndpointV1::Existing(bytes) => Ok(bytes),
        BulkLoadEndpointV1::ByProperty(_) => Err(invalid(
            "bulk-load chunk reached graph-request construction with an unresolved property endpoint",
        )),
    }
}

/// Distinct `(vertex_label, property_name, value)` reference within one edge chunk.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ByPropertyRef {
    vertex_label: String,
    property_name: String,
    value: Vec<u8>,
}

/// Resolve every `ByProperty` edge endpoint in the chunk through the graph property index.
/// The whole candidate chunk is rejected before any operation
/// executes when any endpoint is missing or non-unique, or when the required converged property
/// index on `(vertex_label, property_name)` does not exist. Endpoints are grouped by
/// `(label, property)` and resolved with one batched equality request per group (values
/// deduplicated), following the index resume cursor until every bucket is answered. The durable
/// child row later stores the resolved graph request, so replay never re-resolves. Chunks
/// without `ByProperty` endpoints are returned unchanged and never touch the index.
async fn resolve_by_property_endpoints(
    store: &RouterStore,
    graph_id: GraphId,
    encoding_key: &ElementIdEncodingKey,
    chunk: &BulkLoadChunkV1,
) -> Result<BulkLoadChunkV1, RouterError> {
    let BulkLoadChunkV1::Edges(items) = chunk else {
        return Ok(chunk.clone());
    };
    let has_by_property = items.iter().any(|item| {
        matches!(item.source, BulkLoadEndpointV1::ByProperty(_))
            || matches!(item.target, BulkLoadEndpointV1::ByProperty(_))
    });
    if !has_by_property {
        return Ok(chunk.clone());
    }

    let mut distinct = BTreeSet::new();
    for item in items {
        for endpoint in [&item.source, &item.target] {
            if let BulkLoadEndpointV1::ByProperty(property) = endpoint {
                distinct.insert(ByPropertyRef {
                    vertex_label: property.vertex_label.clone(),
                    property_name: property.property_name.clone(),
                    value: property.value.clone(),
                });
            }
        }
    }
    let mut groups: BTreeMap<(String, String), Vec<Vec<u8>>> = BTreeMap::new();
    for reference in &distinct {
        groups
            .entry((
                reference.vertex_label.clone(),
                reference.property_name.clone(),
            ))
            .or_default()
            .push(reference.value.clone());
    }

    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let lookup = RouterIndexLookup::from_shards(graph_id, &shards).map_err(invalid)?;
    let mut resolved: BTreeMap<ByPropertyRef, Vec<u8>> = BTreeMap::new();
    for ((vertex_label, property_name), values) in groups {
        let label_id = store.lookup_vertex_label_id(graph_id, &vertex_label)?;
        let property_id = store.lookup_property_id(graph_id, &property_name)?;
        let physical_index_id =
            crate::facade::stable::indexed_catalog::active_vertex_physical_index(
                graph_id,
                Some(label_id),
                property_id,
            )?;
        let per_value = lookup
            .batch_equal_per_value(
                physical_index_id,
                property_id.raw(),
                values,
                // Uniqueness detection needs at most two postings per bucket.
                2,
            )
            .await
            .map_err(invalid)?;
        let group_resolved = classify_resolved_values(&vertex_label, &property_name, per_value)?;
        for (value, hit) in group_resolved {
            let encoded = gleaph_graph_kernel::federation::encode_global_vertex_id(
                encoding_key,
                GlobalVertexId::new(hit.shard_id, hit.vertex_id),
            )
            .0
            .to_vec();
            resolved.insert(
                ByPropertyRef {
                    vertex_label: vertex_label.clone(),
                    property_name: property_name.clone(),
                    value,
                },
                encoded,
            );
        }
    }

    let items = items
        .iter()
        .cloned()
        .map(|mut item| {
            item.source = resolve_endpoint(item.source, &resolved)?;
            item.target = resolve_endpoint(item.target, &resolved)?;
            Ok(item)
        })
        .collect::<Result<Vec<_>, RouterError>>()?;
    Ok(BulkLoadChunkV1::Edges(items))
}

/// Replace a resolved `ByProperty` endpoint with its encoded `Existing` vertex ID.
fn resolve_endpoint(
    endpoint: BulkLoadEndpointV1,
    resolved: &BTreeMap<ByPropertyRef, Vec<u8>>,
) -> Result<BulkLoadEndpointV1, RouterError> {
    match endpoint {
        BulkLoadEndpointV1::Existing(bytes) => Ok(BulkLoadEndpointV1::Existing(bytes)),
        BulkLoadEndpointV1::ByProperty(property) => {
            let reference = ByPropertyRef {
                vertex_label: property.vertex_label,
                property_name: property.property_name,
                value: property.value,
            };
            let encoded = resolved.get(&reference).ok_or_else(|| {
                RouterError::Internal(
                    "resolved property endpoint map is missing a reference".into(),
                )
            })?;
            Ok(BulkLoadEndpointV1::Existing(encoded.clone()))
        }
    }
}

/// Classify one group's per-value postings: exactly one live posting resolves the value, zero is
/// missing, and more than one (or a truncated bucket) is non-unique. The whole candidate chunk is
/// rejected before any operation executes, so failures never surface as a partial commit.
fn classify_resolved_values(
    vertex_label: &str,
    property_name: &str,
    per_value: Vec<crate::index_lookup::ResolvedEqualValue>,
) -> Result<BTreeMap<Vec<u8>, gleaph_graph_kernel::index::PostingHit>, RouterError> {
    let mut resolved = BTreeMap::new();
    for result in per_value {
        let unique = result.hits.len() == 1 && result.complete;
        if result.hits.is_empty() {
            return Err(invalid(format!(
                "bulk-load edge endpoint by property ({vertex_label}, {property_name}) value does not resolve to a vertex"
            )));
        }
        if !unique {
            return Err(invalid(format!(
                "bulk-load edge endpoint by property ({vertex_label}, {property_name}) value resolves to multiple vertices"
            )));
        }
        resolved.insert(result.value, result.hits[0]);
    }
    Ok(resolved)
}

fn build_graph_request(
    store: &RouterStore,
    graph_id: GraphId,
    graph_name: Option<String>,
    client_bulk_key: &str,
    target: &BulkLoadTargetV1,
    chunk: &BulkLoadChunkV1,
    encoding_key: &ElementIdEncodingKey,
) -> Result<(BulkLoadGraphRequestV1, [u8; 32]), RouterError> {
    let request = atomic_request_from_chunk(graph_name, client_bulk_key, chunk)?;
    let classified = request.into_classified_bulk().map_err(invalid)?;
    match classified {
        crate::types::ClassifiedAtomicInsertRequest::Vertex(request) => {
            let crate::types::OrderedVertexBatchRequest::V1(request_v1) = &request;
            let (labels, properties) = store.resolve_ordered_vertex_catalogs(
                graph_id,
                request_v1
                    .items
                    .iter()
                    .flat_map(|item| item.vertex_labels.iter().cloned()),
                request_v1.items.iter().flat_map(|item| {
                    item.initial_properties
                        .iter()
                        .map(|property| property.property_name.clone())
                }),
            )?;
            let graph_request = request
                .to_graph_request(
                    graph_id,
                    target.shard_id,
                    target.graph_canister,
                    labels,
                    properties,
                )
                .map_err(invalid)?;
            let fingerprint =
                gleaph_graph_kernel::plan_exec::ordered_vertex_batch_graph_request_fingerprint(
                    &graph_request,
                )
                .map_err(invalid)?;
            let graph_request = match graph_request {
                gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(request) => {
                    BulkLoadGraphRequestV1::Vertex(request)
                }
            };
            let (request_graph_id, shard_id, graph_canister) = graph_request.target();
            if request_graph_id != graph_id
                || shard_id != target.shard_id
                || graph_canister != target.graph_canister
            {
                return Err(RouterError::Conflict(
                    "bulk-load Graph request does not match pinned target".into(),
                ));
            }
            let _ = encoding_key;
            Ok((graph_request, fingerprint))
        }
        crate::types::ClassifiedAtomicInsertRequest::Edge(request) => {
            let endpoints = request
                .decode_same_shard_endpoints(encoding_key)
                .map_err(invalid)?;
            if endpoints
                .iter()
                .any(|(source, _target_id)| source.shard_id != target.shard_id)
            {
                return Err(invalid(
                    "bulk-load edge endpoints resolve to a different shard",
                ));
            }
            let crate::types::OrderedEdgeBatchRequest::V1(request_v1) = &request;
            let (labels, properties) = store.resolve_ordered_edge_catalogs(
                graph_id,
                request_v1
                    .items
                    .iter()
                    .map(|item| item.edge_label_name.clone()),
                request_v1.items.iter().flat_map(|item| {
                    item.initial_edge_properties
                        .iter()
                        .map(|property| property.property_name.clone())
                }),
            )?;
            let graph_request = request
                .to_graph_request(
                    graph_id,
                    target.shard_id,
                    target.graph_canister,
                    &endpoints,
                    labels,
                    properties,
                )
                .map_err(invalid)?;
            let fingerprint =
                gleaph_graph_kernel::plan_exec::ordered_edge_batch_graph_request_fingerprint(
                    &graph_request,
                )
                .map_err(invalid)?;
            let graph_request = match graph_request {
                gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(request) => {
                    BulkLoadGraphRequestV1::Edge(request)
                }
            };
            let (request_graph_id, shard_id, graph_canister) = graph_request.target();
            if request_graph_id != graph_id
                || shard_id != target.shard_id
                || graph_canister != target.graph_canister
            {
                return Err(RouterError::Conflict(
                    "bulk-load Graph request does not match pinned target".into(),
                ));
            }
            Ok((graph_request, fingerprint))
        }
        crate::types::ClassifiedAtomicInsertRequest::Mixed(_) => Err(invalid(
            "bulk-load chunks must contain only vertices or existing-ID edges",
        )),
    }
}

fn public_receipt(
    target_shard: ShardId,
    graph_receipt: &BulkLoadGraphReceiptV1,
    encoding_key: &ElementIdEncodingKey,
) -> Result<crate::types::AtomicInsertReceiptV1, RouterError> {
    let receipt = match graph_receipt {
        BulkLoadGraphReceiptV1::Edge(receipt) => crate::types::AtomicInsertReceiptV1 {
            logical_operation_count: receipt.logical_edge_count,
            logical_vertex_count: 0,
            logical_edge_count: receipt.logical_edge_count,
            allocated_vertex_ids: Vec::new(),
        },
        BulkLoadGraphReceiptV1::Vertex(receipt) => crate::types::AtomicInsertReceiptV1 {
            logical_operation_count: receipt.logical_vertex_count,
            logical_vertex_count: receipt.logical_vertex_count,
            logical_edge_count: 0,
            allocated_vertex_ids: receipt
                .allocated_vertex_ids
                .iter()
                .copied()
                .map(|local_id| {
                    gleaph_graph_kernel::federation::encode_global_vertex_id(
                        encoding_key,
                        gleaph_graph_kernel::federation::GlobalVertexId::new(
                            target_shard,
                            local_id,
                        ),
                    )
                    .0
                    .to_vec()
                })
                .collect(),
        },
    };
    receipt.validate().map_err(invalid)?;
    Ok(receipt)
}

fn projection_target(receipt: &BulkLoadGraphReceiptV1) -> Option<ShardEventSeq> {
    match receipt {
        BulkLoadGraphReceiptV1::Edge(receipt) => receipt.emitted_delta_last_seq,
        BulkLoadGraphReceiptV1::Vertex(receipt) => receipt.emitted_delta_last_seq,
    }
}

async fn dispatch_graph_child(
    graph_request: &BulkLoadGraphRequestV1,
    child_mutation_id: MutationId,
    graph_request_fingerprint: [u8; 32],
) -> Result<BulkLoadGraphReceiptV1, RouterError> {
    match graph_request {
        BulkLoadGraphRequestV1::Edge(request) => {
            let indexed_property_catalog =
                crate::facade::stable::indexed_catalog::ordered_edge_batch_catalog(request);
            let result = execute_ordered_edge_batch_on_graph(
                request.target_graph_canister,
                OrderedEdgeBatchGraphArgs::V1(OrderedEdgeBatchGraphArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                    execution_mode: OrderedBatchExecutionModeV1::Resumable,
                    indexed_property_catalog,
                    request: gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(
                        request.clone(),
                    ),
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            match result {
                GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(
                    receipt,
                )) => Ok(BulkLoadGraphReceiptV1::Edge(receipt)),
                GraphOrderedEdgeBatchResult::V1(
                    GraphOrderedEdgeBatchResultV1::MutationRetired { .. },
                ) => Err(invalid(
                    "bulk-load Graph returned a retired child without a receipt",
                )),
            }
        }
        BulkLoadGraphRequestV1::Vertex(request) => {
            let indexed_property_catalog =
                crate::facade::stable::indexed_catalog::ordered_vertex_batch_catalog(request);
            let result = execute_ordered_vertex_batch_on_graph(
                request.target_graph_canister,
                OrderedVertexBatchGraphArgs::V1(OrderedVertexBatchGraphArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                    execution_mode: OrderedBatchExecutionModeV1::Resumable,
                    indexed_property_catalog,
                    request: gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(
                        request.clone(),
                    ),
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            match result {
                GraphOrderedVertexBatchResult::V1(GraphOrderedVertexBatchResultV1::Completed(
                    receipt,
                )) => Ok(BulkLoadGraphReceiptV1::Vertex(receipt)),
                GraphOrderedVertexBatchResult::V1(
                    GraphOrderedVertexBatchResultV1::MutationRetired { .. },
                ) => Err(invalid(
                    "bulk-load Graph returned a retired child without a receipt",
                )),
            }
        }
    }
}

async fn retire_graph_child(
    graph_request: &BulkLoadGraphRequestV1,
    child_mutation_id: MutationId,
    graph_request_fingerprint: [u8; 32],
    expected_receipt: &BulkLoadGraphReceiptV1,
) -> Result<(), RouterError> {
    match graph_request {
        BulkLoadGraphRequestV1::Edge(request) => {
            let ack = retire_ordered_mutation_on_graph(
                request.target_graph_canister,
                OrderedMutationRetirementArgs::V1(OrderedMutationRetirementArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            let OrderedMutationRetirementAck::V1(OrderedMutationRetirementAckV1 {
                mutation_id,
                graph_request_fingerprint: returned_fingerprint,
                receipt,
            }) = ack;
            if mutation_id != child_mutation_id
                || returned_fingerprint != graph_request_fingerprint
                || !matches!(expected_receipt, BulkLoadGraphReceiptV1::Edge(expected) if &receipt == expected)
            {
                return Err(invalid(
                    "bulk-load edge retirement acknowledgement does not match the child receipt",
                ));
            }
        }
        BulkLoadGraphRequestV1::Vertex(request) => {
            let ack = retire_ordered_vertex_mutation_on_graph(
                request.target_graph_canister,
                OrderedVertexMutationRetirementArgs::V1(OrderedVertexMutationRetirementArgsV1 {
                    mutation_id: child_mutation_id,
                    graph_request_fingerprint,
                }),
            )
            .await
            .map_err(RouterError::Internal)?;
            let OrderedVertexMutationRetirementAck::V1(OrderedVertexMutationRetirementAckV1 {
                mutation_id,
                graph_request_fingerprint: returned_fingerprint,
                receipt,
            }) = ack;
            if mutation_id != child_mutation_id
                || returned_fingerprint != graph_request_fingerprint
                || !matches!(expected_receipt, BulkLoadGraphReceiptV1::Vertex(expected) if &receipt == expected)
            {
                return Err(invalid(
                    "bulk-load vertex retirement acknowledgement does not match the child receipt",
                ));
            }
        }
    }
    Ok(())
}

async fn drive_bulk_child(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_bulk_key: &str,
    parent_mutation_id: MutationId,
    chunk_index: u32,
    chunk_fingerprint: [u8; 32],
    target: &BulkLoadTargetV1,
    encoding_key: &ElementIdEncodingKey,
) -> Result<crate::types::AtomicInsertReceiptV1, RouterError> {
    loop {
        let child = store
            .bulk_load_chunk_receipt(parent_mutation_id, chunk_index)
            .ok_or_else(|| {
                RouterError::Internal("bulk-load child receipt row is missing".into())
            })?;
        if child.chunk_fingerprint != chunk_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load child fingerprint conflicts with the durable row".into(),
            ));
        }
        match child.progress {
            BulkLoadChunkProgressV1::CanonicalPending => {
                let graph_request = child.graph_request.as_ref().ok_or_else(|| {
                    RouterError::Internal("bulk-load child lacks its Graph request".into())
                })?;
                let graph_receipt = dispatch_graph_child(
                    graph_request,
                    child.child_mutation_id,
                    child.graph_request_fingerprint.ok_or_else(|| {
                        RouterError::Internal(
                            "bulk-load child lacks its Graph request fingerprint".into(),
                        )
                    })?,
                )
                .await?;
                let (request_graph_id, shard_id, graph_canister) = graph_request.target();
                if request_graph_id != graph_id
                    || shard_id != target.shard_id
                    || graph_canister != target.graph_canister
                {
                    return Err(RouterError::Internal(
                        "bulk-load child target changed after admission".into(),
                    ));
                }
                let public_receipt = public_receipt(target.shard_id, &graph_receipt, encoding_key)?;
                store.record_bulk_load_canonical_committed(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                    graph_receipt,
                    public_receipt,
                )?;
            }
            BulkLoadChunkProgressV1::CanonicalCommitted
            | BulkLoadChunkProgressV1::ProjectionPending => {
                let graph_receipt = child.graph_receipt.clone().ok_or_else(|| {
                    RouterError::Internal("bulk-load child lacks its Graph receipt".into())
                })?;
                store.record_bulk_load_projection_pending(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                )?;
                crate::gql::advance_label_stats_projection_through(
                    store,
                    graph_id,
                    target.graph_canister,
                    target.shard_id,
                    projection_target(&graph_receipt),
                )
                .await?;
                store.record_bulk_load_retirement_pending(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                )?;
            }
            BulkLoadChunkProgressV1::RetirementPending => {
                let graph_receipt = child.graph_receipt.clone().ok_or_else(|| {
                    RouterError::Internal("bulk-load child lacks its Graph receipt".into())
                })?;
                retire_graph_child(
                    child.graph_request.as_ref().ok_or_else(|| {
                        RouterError::Internal("bulk-load child lacks its Graph request".into())
                    })?,
                    child.child_mutation_id,
                    child.graph_request_fingerprint.ok_or_else(|| {
                        RouterError::Internal(
                            "bulk-load child lacks its Graph request fingerprint".into(),
                        )
                    })?,
                    &graph_receipt,
                )
                .await?;
                store.complete_bulk_load_child(
                    caller,
                    graph_id,
                    client_bulk_key,
                    parent_mutation_id,
                    chunk_index,
                    chunk_fingerprint,
                    time(),
                )?;
            }
            BulkLoadChunkProgressV1::Completed => {
                return child.public_receipt.ok_or_else(|| {
                    RouterError::Internal("completed bulk-load child lacks public receipt".into())
                });
            }
        }
    }
}

fn start_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let target = start_target_for_key(&store, caller, graph_id, &client_bulk_key)?;
    match store.start_bulk_load_job(caller, graph_id, &client_bulk_key, target, time())? {
        BulkLoadStartAdmission::Created { .. } => Ok(BulkLoadResponse::Started {
            next_chunk_index: 0,
        }),
        BulkLoadStartAdmission::Replay { record } => {
            let coordinator = bulk_parent(&record)?;
            Ok(BulkLoadResponse::Started {
                next_chunk_index: coordinator.next_chunk_index,
            })
        }
    }
}

async fn append_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
    chunk_index: u32,
    chunk: BulkLoadChunkV1,
) -> Result<BulkLoadResponse, RouterError> {
    let chunk_envelope = BulkLoadChunkEnvelopeV1::from_chunk(&chunk);
    let chunk_fingerprint = chunk_envelope.fingerprint().map_err(invalid)?;
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let parent_mutation_id = record.as_v1().mutation_id;
    let coordinator = bulk_parent(&record)?.clone();
    if coordinator.receipt_gc_cursor.is_some() {
        return Err(RouterError::Conflict(
            "client_bulk_key expired while bulk-load receipt GC is active".into(),
        ));
    }
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;

    if let Some(child) = store.bulk_load_chunk_receipt(parent_mutation_id, chunk_index) {
        if child.chunk_fingerprint != chunk_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load chunk fingerprint conflicts with the durable row".into(),
            ));
        }
        let receipt = drive_bulk_child(
            &store,
            caller,
            graph_id,
            &client_bulk_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            &coordinator.target,
            &encoding_key,
        )
        .await?;
        return Ok(BulkLoadResponse::Appended {
            chunk_index,
            next_offset: committed_offset(&receipt)?,
            receipt,
        });
    }

    if !matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Open)
        || chunk_index != coordinator.next_chunk_index
    {
        return Err(RouterError::Conflict(
            "bulk-load append is not the next admissible chunk".into(),
        ));
    }
    // Resolve property-based endpoints before any admission or dispatch: the whole candidate
    // chunk is rejected when any endpoint is missing or non-unique, so failures never surface as
    // a partial commit mid-chunk. The durable child row stores the resolved graph request, so
    // replay never re-resolves.
    let resolved_chunk =
        resolve_by_property_endpoints(&store, graph_id, &encoding_key, &chunk).await?;
    let (graph_request, graph_request_fingerprint) = build_graph_request(
        &store,
        graph_id,
        graph_name,
        &client_bulk_key,
        &coordinator.target,
        &resolved_chunk,
        &encoding_key,
    )?;
    let child = BulkLoadChunkReceiptRecordV1 {
        chunk_fingerprint,
        graph_request: Some(graph_request),
        graph_request_fingerprint: Some(graph_request_fingerprint),
        child_mutation_id: 1,
        progress: BulkLoadChunkProgressV1::CanonicalPending,
        public_receipt: None,
        graph_receipt: None,
        completed_at_ns: None,
    };
    store.admit_bulk_load_child(
        caller,
        graph_id,
        &client_bulk_key,
        parent_mutation_id,
        chunk_index,
        chunk_fingerprint,
        child,
    )?;
    let receipt = drive_bulk_child(
        &store,
        caller,
        graph_id,
        &client_bulk_key,
        parent_mutation_id,
        chunk_index,
        chunk_fingerprint,
        &coordinator.target,
        &encoding_key,
    )
    .await?;
    Ok(BulkLoadResponse::Appended {
        chunk_index,
        next_offset: committed_offset(&receipt)?,
        receipt,
    })
}

/// Operations of the candidate batch committed as this chunk. The public receipt counts are the
/// committed prefix (one of vertex/edge is zero), so the operation count is the offset the client
/// resumes from at `chunk_index + 1` (ADR 0060).
fn committed_offset(receipt: &crate::types::AtomicInsertReceiptV1) -> Result<u32, RouterError> {
    u32::try_from(receipt.logical_operation_count)
        .map_err(|_| RouterError::Internal("bulk-load chunk committed count exceeds u32".into()))
}

fn finalize_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let coordinator = store.begin_bulk_load_finalize(caller, graph_id, &client_bulk_key)?;
    let coordinator = if matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Completed) {
        coordinator
    } else {
        store.finalize_bulk_load_step(caller, graph_id, &client_bulk_key, time())?
    };
    Ok(BulkLoadResponse::FinalizeAccepted {
        state: public_state(&coordinator),
    })
}

async fn abort_bulk_load(
    graph_name: Option<String>,
    client_bulk_key: String,
) -> Result<BulkLoadResponse, RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let coordinator = store.begin_bulk_load_abort(caller, graph_id, &client_bulk_key, time())?;
    if let BulkLoadLifecycleV1::AbortPending { active_chunk } = coordinator.lifecycle {
        let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
        let parent_mutation_id = record.as_v1().mutation_id;
        let child = store
            .bulk_load_chunk_receipt(parent_mutation_id, active_chunk)
            .ok_or_else(|| RouterError::Internal("bulk-load abort child row is missing".into()))?;
        let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
        drive_bulk_child(
            &store,
            caller,
            graph_id,
            &client_bulk_key,
            parent_mutation_id,
            active_chunk,
            child.chunk_fingerprint,
            &bulk_parent(&record)?.target,
            &encoding_key,
        )
        .await?;
    }
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let coordinator = bulk_parent(&record)?;
    Ok(BulkLoadResponse::AbortAccepted {
        state: public_state(coordinator),
    })
}

/// Public update entrypoint used by `api::client::bulk_load`.
pub(crate) async fn bulk_load_public(
    command: BulkLoadCommand,
) -> Result<BulkLoadResponse, RouterError> {
    command.validate().map_err(invalid)?;
    match command {
        BulkLoadCommand::Start {
            graph_name,
            client_bulk_key,
        } => start_bulk_load(graph_name, client_bulk_key),
        BulkLoadCommand::Append {
            graph_name,
            client_bulk_key,
            chunk_index,
            chunk,
        } => append_bulk_load(graph_name, client_bulk_key, chunk_index, chunk).await,
        BulkLoadCommand::Finalize {
            graph_name,
            client_bulk_key,
        } => finalize_bulk_load(graph_name, client_bulk_key),
        BulkLoadCommand::Abort {
            graph_name,
            client_bulk_key,
        } => abort_bulk_load(graph_name, client_bulk_key).await,
    }
}

/// Public status query used by `api::client::bulk_load_status`.
pub(crate) fn bulk_load_status_public(
    graph_name: Option<String>,
    client_bulk_key: String,
    receipt_cursor: Option<u32>,
    max_receipts: u32,
) -> Result<BulkLoadStatusPage, RouterError> {
    crate::types::validate_max_receipts(max_receipts).map_err(invalid)?;
    BulkLoadCommand::Start {
        graph_name: graph_name.clone(),
        client_bulk_key: client_bulk_key.clone(),
    }
    .validate()
    .map_err(invalid)?;
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let record = bulk_record(&store, caller, graph_id, &client_bulk_key)?;
    let coordinator = bulk_parent(&record)?;
    let cursor = receipt_cursor.unwrap_or(0);
    // Completed rows are compacted by `complete_bulk_load_child`, so status pagination decodes
    // only receipt-sized rows; a job has at most one transient in-flight child at a time.
    let rows =
        store.list_bulk_load_chunk_receipts(record.as_v1().mutation_id, cursor, max_receipts)?;
    let receipts = rows
        .iter()
        .filter_map(|(chunk_index, row)| {
            row.public_receipt
                .clone()
                .map(|receipt| BulkLoadChunkReceiptV1 {
                    chunk_index: *chunk_index,
                    receipt,
                })
        })
        .collect::<Vec<_>>();
    let next_receipt_cursor = rows.last().and_then(|(chunk_index, _)| {
        let next = chunk_index.checked_add(1)?;
        store
            .bulk_load_has_chunk_receipt_at_or_after(record.as_v1().mutation_id, next)
            .then_some(next)
    });
    Ok(BulkLoadStatusPage {
        state: public_state(coordinator),
        next_chunk_index: coordinator.next_chunk_index,
        committed_chunk_count: coordinator.committed_chunk_count,
        completed_chunk_count: coordinator.completed_chunk_count,
        terminal_at_ns: record.as_v1().terminal_at_ns,
        expires_at_ns: terminal_expiry(record.as_v1().terminal_at_ns),
        receipts,
        next_receipt_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::store::tests::test_init_args;
    use crate::index_lookup::ResolvedEqualValue;

    #[test]
    fn classify_resolved_values_resolves_unique_and_rejects_missing_or_non_unique() {
        let hit = |shard: u32, vertex: u32| gleaph_graph_kernel::index::PostingHit {
            shard_id: ShardId::new(shard),
            vertex_id: vertex,
        };
        let unique = ResolvedEqualValue {
            value: b"a".to_vec(),
            hits: vec![hit(0, 7)],
            complete: true,
        };
        let resolved = classify_resolved_values("Person", "email", vec![unique])
            .expect("unique value must resolve");
        assert_eq!(resolved[&b"a".to_vec()], hit(0, 7));

        let missing = ResolvedEqualValue {
            value: b"b".to_vec(),
            hits: Vec::new(),
            complete: true,
        };
        let error = classify_resolved_values("Person", "email", vec![missing])
            .expect_err("missing value must reject the whole chunk");
        assert!(error.to_string().contains("does not resolve"), "{error}");

        let non_unique = ResolvedEqualValue {
            value: b"c".to_vec(),
            hits: vec![hit(0, 1), hit(1, 2)],
            complete: true,
        };
        let error = classify_resolved_values("Person", "email", vec![non_unique])
            .expect_err("non-unique value must reject the whole chunk");
        assert!(error.to_string().contains("multiple vertices"), "{error}");

        // A truncated bucket (more postings than the limit) is non-unique even when only one
        // posting was materialized.
        let truncated = ResolvedEqualValue {
            value: b"d".to_vec(),
            hits: vec![hit(0, 3)],
            complete: false,
        };
        let error = classify_resolved_values("Person", "email", vec![truncated])
            .expect_err("truncated bucket must reject as non-unique");
        assert!(error.to_string().contains("multiple vertices"), "{error}");
    }

    #[test]
    fn start_rejects_wrong_mutation_family_before_shard_routing() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([41; 32]);
        let graph_id = GraphId::from_raw(1);
        store
            .reserve_mutation_id_for_client_key_at(
                caller,
                graph_id,
                "wrong-family",
                b"scalar-request".to_vec(),
                1,
            )
            .expect("seed scalar mutation record");

        assert_eq!(
            start_target_for_key(&store, caller, graph_id, "wrong-family"),
            Err(RouterError::Conflict(
                "client_bulk_key belongs to a different mutation family".into()
            )),
            "wrong-family lookup must win even when the graph has no shard to route to"
        );
    }
}
