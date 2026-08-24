//! Candid-shaped router types.

use candid::{CandidType, Decode, Encode, Principal};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;

pub use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
pub use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId, PropertyId, VertexLabelId};
pub use gleaph_graph_kernel::federation::{
    GlobalVertexId, GraphShardKey, LocalVertexId, ShardId, ShardRegistryEntry,
};
use gleaph_graph_kernel::plan_exec::{MutationId, MutationLifecyclePhase};
pub use gleaph_graph_kernel::provisioning::wire::{
    CreatedResource, ProvisionAcceptResponse, ProvisionRequest, ProvisionResult,
    ProvisionResultOutcome, ProvisionableResource, RouterRegistrationAck,
    RouterRegistrationAckResponse,
};
pub use gleaph_graph_kernel::provisioning::{LogicalResource, ProvisioningIntentKey};
use gleaph_graph_kernel::vector_index::{
    VectorEncoding, VectorMaintenanceFailure, VectorMaintenancePolicy, VectorMaintenanceState,
    VectorMaintenanceStepResult, VectorMetric, VectorPartitionPageHealth, VectorRebuildStatus,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use sha2::{Digest, Sha256};

pub use crate::facade::stable::label_stats::{ClientMutationKey, RouterMutationRecord};
use crate::facade::stable::vector_maintenance_policy::VectorMaintenancePolicyRecord;

pub use gleaph_bulk_load_api::{
    AtomicInsertEdgeV1, AtomicInsertEndpointV1, AtomicInsertOperationV1, AtomicInsertPropertyV1,
    AtomicInsertReceiptV1, AtomicInsertVertexV1, BulkLoadChunkReceiptV1, BulkLoadChunkV1,
    BulkLoadCommand, BulkLoadEdgeV1, BulkLoadEndpointV1, BulkLoadPropertyEndpointV1,
    BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage, MAX_ATOMIC_INSERT_OPERATIONS,
    MAX_BULK_LOAD_RECEIPTS_PER_PAGE,
};

/// Registry-local summary row for one logical graph (ADR 0056 §7). Computed from Router stable
/// state only; no cross-canister calls, so `list_graphs` stays cheap for UI/CLI polling.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct GraphSummary {
    pub graph_id: GraphId,
    pub graph_name: String,
    pub status: GraphStatus,
    pub provisioning_state: ProvisioningState,
    pub shard_count: u32,
    pub updated_at_ns: u64,
}

/// Best-effort graph-level operational snapshot (ADR 0056 §7). Composite query; bounded and
/// partial results are reported in `notes` rather than failing the call.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct GraphHealthView {
    pub graph: GraphSummary,
    /// Shards that answered a liveness check.
    pub reachable_shard_count: u32,
    /// All shards' index-sync converged.
    pub index_sync_converged: bool,
    pub vector_index_count: u32,
    /// Unhealthy vector-index names only; detail lives at the L3 surface.
    pub unhealthy_vector_indexes: Vec<String>,
    /// Bounded, best-effort repair guidance.
    pub notes: Vec<String>,
}

/// Which graph-index repair driver `advance_backfill` advances (ADR 0056 §4). The Router
/// interprets `max_work` per kind (vertices / entries / deltas) and iterates shards internally.
/// The enum itself is shared with the in-flight backfill claim machinery; `LabelStats` covers
/// the label-stats projection driver.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct RegisterGraphArgs {
    pub graph_name: String,
    pub owner: Principal,
    pub admins: BTreeSet<Principal>,
    /// Dev mode: whether this graph is the HOME graph for callers without an explicit `USE GRAPH`
    /// (ADR 0011). The legacy home designation is a client-visible graph property, so it stays in
    /// the intent; federation topology (shards, canister ids) stays internal.
    pub is_home: bool,
    /// Dev mode: the caller-installed shard canisters (one graph + index canister per shard).
    pub shards: Vec<RegisterGraphShard>,
    /// Provisioned mode: resources requested from the configured Provision canister.
    pub requested_resources: Vec<ProvisionableResource>,
}

/// One dev-mode shard placement, mirroring `AdminRegisterShardArgs`. The Router validates
/// `shard_id` against its graph-local dense allocation (mismatch is `Conflict`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct RegisterGraphShard {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
}

/// Operator/SDK-facing status of a federated mutation (ADR 0029 Phase 4).
///
/// Pull-based observability for the autonomous recovery driver: a caller polls this to learn
/// whether a saga converged, which shard is outstanding, and what (if any) explicit action
/// is required. It deliberately carries no read-your-writes token — the token is issued with
/// the original DML result; this query reports lifecycle, not freshness watermarks.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MutationStatus {
    pub mutation_id: MutationId,
    pub phase: MutationLifecyclePhase,
    /// Most recent recovery diagnostic, if any.
    pub last_error: Option<String>,
    /// First shard still outstanding (incomplete canonical write, else lagging projection).
    pub target_shard: Option<ShardId>,
    /// Human-readable next step: `none` when terminal/auto-converging, or the explicit retry
    /// guidance when caller action is required.
    pub next_action: String,
}

/// Public result for one Router atomic insert.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertResponse {
    pub status: MutationStatus,
    /// Present after Graph commits the canonical vertex and/or edge operations.
    pub receipt: Option<AtomicInsertReceiptV1>,
}

impl AtomicInsertResponse {
    /// Project a durable ordered Graph receipt into the public graph-scoped receipt.
    ///
    /// Graph owns allocation and persists local IDs. Router owns the public encoding key and
    /// derives the opaque IDs at the response boundary, so the encoded list is never persisted
    /// as a second source of truth.
    pub(crate) fn from_record_with_encoding_key(
        record: &RouterMutationRecord,
        encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
    ) -> Self {
        use crate::facade::stable::label_stats::{
            OrderedEdgeBatchTargetProgressV1, OrderedMixedBatchTargetProgressV1,
            OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1,
        };

        let receipt = match record.payload() {
            RouterMutationPayloadV1::OrderedEdgeBatch(replay) => match &replay.target.progress {
                OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(receipt)
                | OrderedEdgeBatchTargetProgressV1::ProjectionPending(receipt)
                | OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(receipt)
                | OrderedEdgeBatchTargetProgressV1::RetirementPending(receipt) => {
                    Some(edge_receipt(receipt.logical_edge_count))
                }
                OrderedEdgeBatchTargetProgressV1::CanonicalPending => None,
            },
            RouterMutationPayloadV1::CompletedOrderedEdgeBatch { receipt, .. } => {
                Some(edge_receipt(receipt.logical_edge_count))
            }
            RouterMutationPayloadV1::OrderedVertexBatch(replay) => match &replay.target.progress {
                OrderedVertexBatchTargetProgressV1::CanonicalCommitted(receipt)
                | OrderedVertexBatchTargetProgressV1::ProjectionPending(receipt)
                | OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(receipt)
                | OrderedVertexBatchTargetProgressV1::RetirementPending(receipt) => {
                    Some(vertex_receipt(
                        receipt.logical_vertex_count,
                        replay.target.request.target_shard_id,
                        &receipt.allocated_vertex_ids,
                        encoding_key,
                    ))
                }
                OrderedVertexBatchTargetProgressV1::CanonicalPending => None,
            },
            RouterMutationPayloadV1::CompletedOrderedVertexBatch {
                receipt,
                projection_watermark,
                ..
            } => Some(vertex_receipt(
                receipt.logical_vertex_count,
                projection_watermark.shard_id,
                &receipt.allocated_vertex_ids,
                encoding_key,
            )),
            RouterMutationPayloadV1::OrderedMixedBatch(replay) => match &replay.target.progress {
                OrderedMixedBatchTargetProgressV1::CanonicalCommitted(receipt)
                | OrderedMixedBatchTargetProgressV1::ProjectionPending(receipt)
                | OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(receipt)
                | OrderedMixedBatchTargetProgressV1::RetirementPending(receipt) => {
                    Some(mixed_receipt(
                        receipt.logical_operation_count,
                        receipt.logical_vertex_count,
                        receipt.logical_edge_count,
                        replay.target.request.target_shard_id,
                        &receipt.allocated_vertex_ids,
                        encoding_key,
                    ))
                }
                OrderedMixedBatchTargetProgressV1::CanonicalPending => None,
            },
            RouterMutationPayloadV1::CompletedOrderedMixedBatch {
                receipt,
                projection_watermark,
                ..
            } => Some(mixed_receipt(
                receipt.logical_operation_count,
                receipt.logical_vertex_count,
                receipt.logical_edge_count,
                projection_watermark.shard_id,
                &receipt.allocated_vertex_ids,
                encoding_key,
            )),
            _ => None,
        };
        Self {
            status: MutationStatus::from_record(record),
            receipt,
        }
    }

    /// Host-only projection helper for unit tests that do not have a Router graph registry.
    /// Production ingress always calls [`Self::from_record_with_encoding_key`].
    #[cfg(test)]
    pub(crate) fn from_record(record: &RouterMutationRecord) -> Self {
        Self::from_record_with_encoding_key(
            record,
            &gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
        )
    }
}

/// Encode the graph-scoped local IDs allocated by Graph into the opaque Router-owned receipt IDs.
///
/// Graph owns allocation and persists local IDs; the Router derives the public IDs at the
/// response boundary, so the encoded list is never persisted as a second source of truth.
fn encoded_vertex_ids(
    shard_id: ShardId,
    local_ids: &[LocalVertexId],
    encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
) -> Vec<Vec<u8>> {
    local_ids
        .iter()
        .copied()
        .map(|local_id| {
            gleaph_graph_kernel::federation::encode_global_vertex_id(
                encoding_key,
                GlobalVertexId::new(shard_id, local_id),
            )
            .0
            .to_vec()
        })
        .collect()
}

/// Project an edge-only Graph receipt into the public receipt shape.
fn edge_receipt(count: u64) -> AtomicInsertReceiptV1 {
    AtomicInsertReceiptV1 {
        logical_operation_count: count,
        logical_vertex_count: 0,
        logical_edge_count: count,
        allocated_vertex_ids: Vec::new(),
    }
}

/// Project a vertex-only Graph receipt into the public receipt shape.
fn vertex_receipt(
    count: u64,
    shard_id: ShardId,
    local_ids: &[LocalVertexId],
    encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
) -> AtomicInsertReceiptV1 {
    AtomicInsertReceiptV1 {
        logical_operation_count: count,
        logical_vertex_count: count,
        logical_edge_count: 0,
        allocated_vertex_ids: encoded_vertex_ids(shard_id, local_ids, encoding_key),
    }
}

/// Project a mixed Graph receipt into the public receipt shape.
fn mixed_receipt(
    operation_count: u64,
    vertex_count: u64,
    edge_count: u64,
    shard_id: ShardId,
    local_ids: &[LocalVertexId],
    encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
) -> AtomicInsertReceiptV1 {
    AtomicInsertReceiptV1 {
        logical_operation_count: operation_count,
        logical_vertex_count: vertex_count,
        logical_edge_count: edge_count,
        allocated_vertex_ids: encoded_vertex_ids(shard_id, local_ids, encoding_key),
    }
}

/// Reject invalid page caps before stable iteration and prove the maximum public page fits the
/// IC response bound. The `64` value is owned by `gleaph-bulk-load-api`; this Router-side
/// admission additionally proves the worst-case page fits the canister response bound.
pub(crate) fn validate_max_receipts(max_receipts: u32) -> Result<(), String> {
    if max_receipts == 0 || max_receipts > MAX_BULK_LOAD_RECEIPTS_PER_PAGE {
        return Err(format!(
            "max_receipts must be in 1..={MAX_BULK_LOAD_RECEIPTS_PER_PAGE}"
        ));
    }
    if max_receipts != MAX_BULK_LOAD_RECEIPTS_PER_PAGE {
        return Ok(());
    }
    let receipt = AtomicInsertReceiptV1 {
        logical_operation_count: MAX_ATOMIC_INSERT_OPERATIONS as u64,
        logical_vertex_count: MAX_ATOMIC_INSERT_OPERATIONS as u64,
        logical_edge_count: 0,
        allocated_vertex_ids: vec![
            vec![0; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES];
            MAX_ATOMIC_INSERT_OPERATIONS
        ],
    };
    let page = BulkLoadStatusPage {
        state: BulkLoadPublicStateV1::Failed {
            reason: "x"
                .repeat(crate::facade::stable::label_stats::MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES),
        },
        next_chunk_index: u32::MAX,
        committed_chunk_count: u32::MAX,
        completed_chunk_count: u32::MAX,
        terminal_at_ns: Some(u64::MAX),
        expires_at_ns: Some(u64::MAX),
        receipts: (0..MAX_BULK_LOAD_RECEIPTS_PER_PAGE)
            .map(|chunk_index| BulkLoadChunkReceiptV1 {
                chunk_index,
                receipt: receipt.clone(),
            })
            .collect(),
        next_receipt_cursor: Some(u32::MAX),
    };
    let encoded = Encode!(&Result::<BulkLoadStatusPage, crate::state::RouterError>::Ok(page))
        .map_err(|error| format!("bulk-load status page proof encode: {error}"))?;
    if encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err("bulk-load status page exceeds the safe payload bound".into());
    }
    Ok(())
}

/// Versioned public request for Router-owned ordered batch mutation.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AtomicInsertRequest {
    V1(AtomicInsertRequestV1),
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertRequestV1 {
    /// Stable idempotency key supplied by the caller; retries must reuse it with identical data.
    pub client_mutation_key: String,
    /// Optional logical graph name; `None` resolves the caller's default (HOME) graph.
    pub graph_name: Option<String>,
    pub operations: Vec<AtomicInsertOperationV1>,
}

pub(crate) enum ClassifiedAtomicInsertRequest {
    Edge(OrderedEdgeBatchRequest),
    Vertex(OrderedVertexBatchRequest),
    Mixed(OrderedMixedBatchRequest),
}

impl AtomicInsertRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_with_operation_cap(Some(MAX_ATOMIC_INSERT_OPERATIONS))
    }

    /// Validate as a bulk-load candidate chunk: no fixed operation cap (ADR 0060 §3), but the
    /// chunk must be non-empty and fit the payload bound.
    pub(crate) fn validate_bulk_chunk(&self) -> Result<(), String> {
        self.validate_with_operation_cap(None)
    }

    fn validate_with_operation_cap(&self, cap: Option<usize>) -> Result<(), String> {
        let Self::V1(request) = self;
        if request.client_mutation_key.is_empty() || request.client_mutation_key.len() > 256 {
            return Err("atomic insert client mutation key must be 1..=256 bytes".into());
        }
        if let Some(name) = &request.graph_name
            && name.is_empty()
        {
            return Err("atomic insert graph name must not be empty when present".into());
        }
        if request.operations.is_empty() {
            return Err("atomic insert operations must contain at least 1 entry".into());
        }
        if cap.is_some_and(|cap| request.operations.len() > cap) {
            return Err(format!(
                "atomic insert operations must contain 1..={MAX_ATOMIC_INSERT_OPERATIONS} entries"
            ));
        }
        let vertex_count = request
            .operations
            .iter()
            .filter(|operation| matches!(operation, AtomicInsertOperationV1::Vertex(_)))
            .count() as u32;
        preflight_atomic_insert_response_size(request.operations.len(), vertex_count as usize)?;
        gleaph_bulk_load_api::validate_atomic_insert_operations(&request.operations, vertex_count)?;
        let encoded =
            Encode!(self).map_err(|error| format!("atomic insert request encode: {error}"))?;
        if encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("atomic insert request exceeds the safe payload bound".into());
        }
        Ok(())
    }

    /// Compute the Router-owned idempotency fingerprint without the client mutation key.
    pub(crate) fn public_fingerprint(&self) -> Result<[u8; 32], String> {
        self.validate()?;
        let Self::V1(request) = self;
        let mut operations = request.operations.clone();
        for operation in &mut operations {
            match operation {
                AtomicInsertOperationV1::Vertex(item) => {
                    item.vertex_labels
                        .sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                    item.initial_properties.sort_by(|left, right| {
                        left.property_name
                            .as_bytes()
                            .cmp(right.property_name.as_bytes())
                    });
                }
                AtomicInsertOperationV1::Edge(item) => {
                    item.initial_edge_properties.sort_by(|left, right| {
                        left.property_name
                            .as_bytes()
                            .cmp(right.property_name.as_bytes())
                    });
                }
            }
        }
        let encoded = Encode!(&(request.graph_name.clone(), operations))
            .map_err(|error| format!("batch fingerprint encode: {error}"))?;
        let mut hasher = Sha256::new();
        hasher.update(b"gleaph:atomic-insert-public:v1\0");
        hasher.update(encoded);
        Ok(hasher.finalize().into())
    }

    pub(crate) fn into_classified(
        self,
    ) -> Result<(ClassifiedAtomicInsertRequest, [u8; 32]), String> {
        let public_fingerprint = self.public_fingerprint()?;
        let classified = self.classify()?;
        Ok((classified, public_fingerprint))
    }

    /// Classify a bulk-load candidate chunk without the atomic-insert operation cap: `bulk_load`
    /// has no fixed operation ceiling (ADR 0060 §3), the payload bound governs the candidate
    /// size, and the runtime instruction budget decides the committed prefix. No atomic-insert
    /// public fingerprint is computed (the durable chunk fingerprint derives from the chunk
    /// envelope instead).
    pub(crate) fn into_classified_bulk(self) -> Result<ClassifiedAtomicInsertRequest, String> {
        self.validate_bulk_chunk()?;
        self.classify()
    }

    fn classify(self) -> Result<ClassifiedAtomicInsertRequest, String> {
        let Self::V1(request) = self;
        let AtomicInsertRequestV1 {
            client_mutation_key,
            graph_name,
            operations,
        } = request;
        let has_vertex = operations
            .iter()
            .any(|operation| matches!(operation, AtomicInsertOperationV1::Vertex(_)));
        let has_edge = operations
            .iter()
            .any(|operation| matches!(operation, AtomicInsertOperationV1::Edge(_)));
        let classified = match (has_vertex, has_edge) {
            (true, false) => {
                let items = operations
                    .into_iter()
                    .map(|operation| match operation {
                        AtomicInsertOperationV1::Vertex(item) => Ok(item),
                        AtomicInsertOperationV1::Edge(_) => {
                            Err("vertex-only batch contains an edge operation".to_string())
                        }
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                ClassifiedAtomicInsertRequest::Vertex(OrderedVertexBatchRequest::V1(
                    OrderedVertexBatchRequestV1 {
                        client_mutation_key,
                        graph_name,
                        items,
                    },
                ))
            }
            (false, true) => {
                let items = operations
                    .into_iter()
                    .map(|operation| {
                        let AtomicInsertOperationV1::Edge(item) = operation else {
                            return Err("edge-only batch contains a vertex operation".to_string());
                        };
                        let AtomicInsertEndpointV1::Existing(source) = item.source else {
                            return Err(
                                "edge-only batch source must reference an existing vertex".into()
                            );
                        };
                        let AtomicInsertEndpointV1::Existing(target) = item.target else {
                            return Err(
                                "edge-only batch target must reference an existing vertex".into()
                            );
                        };
                        Ok(OrderedEdgeInsertRequestItemV1 {
                            source,
                            target,
                            directed: item.directed,
                            edge_label_name: item.edge_label_name,
                            inline_property: item.inline_property,
                            initial_edge_properties: item.initial_edge_properties,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                ClassifiedAtomicInsertRequest::Edge(OrderedEdgeBatchRequest::V1(
                    OrderedEdgeBatchRequestV1 {
                        client_mutation_key,
                        graph_name,
                        items,
                    },
                ))
            }
            (true, true) => ClassifiedAtomicInsertRequest::Mixed(OrderedMixedBatchRequest::V1(
                OrderedMixedBatchRequestV1 {
                    client_mutation_key,
                    graph_name,
                    operations,
                },
            )),
            (false, false) => return Err("batch requires at least one operation".into()),
        };
        Ok(classified)
    }
}

/// Exact encoded size of the largest reachable successful public response for this request shape.
///
/// Every lifecycle phase is encoded with its real `next_action`, a maximum bounded recovery
/// diagnostic, a maximum-width target shard, and the full fixed-width vertex-ID receipt. Encoding
/// the outer `Result::Ok` keeps this proof aligned with the actual canister method response.
fn worst_case_atomic_insert_response_size(
    operation_count: usize,
    vertex_count: usize,
) -> Result<usize, String> {
    let edge_count = operation_count
        .checked_sub(vertex_count)
        .ok_or_else(|| "atomic insert vertex count exceeds operation count".to_string())?;
    let operation_count = u64::try_from(operation_count)
        .map_err(|_| "atomic insert operation count overflows u64")?;
    let vertex_count_u64 =
        u64::try_from(vertex_count).map_err(|_| "atomic insert vertex count overflows u64")?;
    let edge_count =
        u64::try_from(edge_count).map_err(|_| "atomic insert edge count overflows u64")?;
    let receipt = AtomicInsertReceiptV1 {
        logical_operation_count: operation_count,
        logical_vertex_count: vertex_count_u64,
        logical_edge_count: edge_count,
        allocated_vertex_ids: vec![
            vec![0; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES];
            vertex_count
        ],
    };
    let diagnostic =
        "x".repeat(crate::facade::stable::label_stats::MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES);
    MutationStatus::ALL_PHASES
        .into_iter()
        .map(|phase| {
            let response: Result<AtomicInsertResponse, crate::state::RouterError> =
                Ok(AtomicInsertResponse {
                    status: MutationStatus {
                        mutation_id: u64::MAX,
                        phase,
                        last_error: Some(diagnostic.clone()),
                        target_shard: Some(ShardId::new(u32::MAX)),
                        next_action: MutationStatus::next_action_for_phase(phase).to_owned(),
                    },
                    receipt: Some(receipt.clone()),
                });
            Encode!(&response)
                .map(|encoded| encoded.len())
                .map_err(|error| {
                    format!("atomic insert worst-case response encode failed: {error}")
                })
        })
        .try_fold(0usize, |maximum, encoded_len| {
            encoded_len.map(|encoded_len| maximum.max(encoded_len))
        })
}

fn preflight_atomic_insert_response_size(
    operation_count: usize,
    vertex_count: usize,
) -> Result<(), String> {
    preflight_atomic_insert_response_size_at_limit(
        operation_count,
        vertex_count,
        gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
    )
}

fn preflight_atomic_insert_response_size_at_limit(
    operation_count: usize,
    vertex_count: usize,
    response_limit: usize,
) -> Result<(), String> {
    if worst_case_atomic_insert_response_size(operation_count, vertex_count)? > response_limit {
        return Err("atomic insert worst-case response exceeds the safe payload bound".into());
    }
    Ok(())
}

/// Router-internal edge-only form selected from [`AtomicInsertRequest`].
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) enum OrderedEdgeBatchRequest {
    V1(OrderedEdgeBatchRequestV1),
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrderedEdgeBatchRequestV1 {
    pub client_mutation_key: String,
    pub graph_name: Option<String>,
    pub items: Vec<OrderedEdgeInsertRequestItemV1>,
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrderedEdgeInsertRequestItemV1 {
    /// Candid bytes for the encoded global vertex identifier.
    pub source: Vec<u8>,
    /// Candid bytes for the encoded global vertex identifier.
    pub target: Vec<u8>,
    pub directed: bool,
    pub edge_label_name: Option<String>,
    pub inline_property: Option<Vec<u8>>,
    pub initial_edge_properties: Vec<AtomicInsertPropertyV1>,
}

impl OrderedEdgeBatchRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let OrderedEdgeBatchRequest::V1(request) = self;
        if request.client_mutation_key.is_empty() {
            return Err("ordered edge batch client mutation key must not be empty".into());
        }
        if request.client_mutation_key.len() > 256 {
            return Err("ordered edge batch client mutation key exceeds 256 bytes".into());
        }
        if let Some(name) = &request.graph_name
            && name.is_empty()
        {
            return Err("ordered edge batch graph name must not be empty when present".into());
        }
        if request.items.is_empty() {
            return Err("ordered edge batch requires at least one item".into());
        }
        for (ordinal, item) in request.items.iter().enumerate() {
            for (endpoint_name, endpoint) in [("source", &item.source), ("target", &item.target)] {
                if endpoint.len() != gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES {
                    return Err(format!(
                        "ordered edge item {ordinal} {endpoint_name} must be exactly {} bytes",
                        gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
                    ));
                }
            }
            if let Some(bytes) = &item.inline_property
                && bytes.len() > gleaph_graph_kernel::entry::MAX_EDGE_INLINE_PROPERTY_BYTES
            {
                return Err(format!(
                    "ordered edge item {ordinal} inline property exceeds the byte bound"
                ));
            }
            for property in &item.initial_edge_properties {
                if property.property_name.is_empty() {
                    return Err(format!(
                        "ordered edge item {ordinal} contains an empty property name"
                    ));
                }
                if property.value.len()
                    > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
                {
                    return Err(format!(
                        "ordered edge item {ordinal} property value exceeds the payload bound"
                    ));
                }
            }
            let mut property_names = BTreeSet::new();
            for property in &item.initial_edge_properties {
                if !property_names.insert(&property.property_name) {
                    return Err(format!(
                        "ordered edge item {ordinal} repeats property name {}",
                        property.property_name
                    ));
                }
            }
        }
        let encoded =
            Encode!(self).map_err(|error| format!("ordered edge batch encode: {error}"))?;
        if encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered edge batch exceeds the safe payload bound".into());
        }
        Ok(())
    }

    /// Decode every classified endpoint and prove that the complete batch belongs to one shard.
    ///
    /// This is deliberately a read-only wire-boundary operation. Catalog resolution and the
    /// mutation reservation happen only after this proof succeeds.
    pub(crate) fn decode_same_shard_endpoints(
        &self,
        key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
    ) -> Result<Vec<(GlobalVertexId, GlobalVertexId)>, String> {
        self.validate()?;
        let Self::V1(request) = self;
        let mut decoded = Vec::with_capacity(request.items.len());
        let mut target_shard = None;
        for (ordinal, item) in request.items.iter().enumerate() {
            let source_bytes: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
                item.source.as_slice().try_into().map_err(|_| {
                    format!("ordered edge item {ordinal} source endpoint has invalid width")
                })?;
            let target_bytes: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
                item.target.as_slice().try_into().map_err(|_| {
                    format!("ordered edge item {ordinal} target endpoint has invalid width")
                })?;
            let source = gleaph_graph_kernel::federation::decode_global_vertex_id(
                key,
                gleaph_graph_kernel::federation::EncodedVertexId(source_bytes),
            );
            let target = gleaph_graph_kernel::federation::decode_global_vertex_id(
                key,
                gleaph_graph_kernel::federation::EncodedVertexId(target_bytes),
            );
            if source.shard_id != target.shard_id {
                return Err(format!(
                    "ordered edge item {ordinal} endpoints resolve to different shards"
                ));
            }
            if let Some(expected) = target_shard {
                if expected != source.shard_id {
                    return Err("ordered edge batch resolves to multiple shards".into());
                }
            } else {
                target_shard = Some(source.shard_id);
            }
            decoded.push((source, target));
        }
        Ok(decoded)
    }

    /// Convert a resolved classified request into the immutable Router → Graph request envelope.
    pub(crate) fn to_graph_request(
        &self,
        graph_id: GraphId,
        target_shard_id: ShardId,
        target_graph_canister: Principal,
        endpoints: &[(GlobalVertexId, GlobalVertexId)],
        resolved_labels: gleaph_graph_kernel::plan_exec::ResolvedLabelTable,
        resolved_properties: gleaph_graph_kernel::plan_exec::ResolvedPropertyTable,
    ) -> Result<gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest, String> {
        self.validate()?;
        let Self::V1(request) = self;
        if endpoints.len() != request.items.len() {
            return Err("ordered endpoint resolution count does not match classified items".into());
        }
        let items = request
            .items
            .iter()
            .zip(endpoints)
            .enumerate()
            .map(|(ordinal, (item, (source, target)))| {
                if source.shard_id != target_shard_id || target.shard_id != target_shard_id {
                    return Err(format!(
                        "ordered edge item {ordinal} does not target the selected shard"
                    ));
                }
                let catalog_edge_label_id = item
                    .edge_label_name
                    .as_ref()
                    .map(|name| {
                        resolved_labels
                            .edge
                            .iter()
                            .find(|entry| entry.name == *name)
                            .map(|entry| entry.id)
                            .ok_or_else(|| format!("ordered edge label {name} was not resolved"))
                    })
                    .transpose()?;
                let resolved_initial_edge_properties = item
                    .initial_edge_properties
                    .iter()
                    .map(|property| {
                        let property_id = resolved_properties
                            .properties
                            .iter()
                            .find(|entry| entry.name == property.property_name)
                            .map(|entry| entry.id)
                            .ok_or_else(|| {
                                format!(
                                    "ordered edge property {} was not resolved",
                                    property.property_name
                                )
                            })?;
                        Ok(
                            gleaph_graph_kernel::plan_exec::ResolvedOrderedEdgePropertyV1 {
                                property_id,
                                value: property.value.clone(),
                            },
                        )
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(
                    gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphItemV1 {
                        source_local_vertex_id: source.local_vertex_id,
                        target_local_vertex_id: target.local_vertex_id,
                        directed: item.directed,
                        catalog_edge_label_id,
                        inline_property_bytes: item.inline_property.clone().unwrap_or_default(),
                        resolved_initial_edge_properties,
                    },
                )
            })
            .collect::<Result<Vec<_>, String>>()?;
        let request = gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(
            gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequestV1 {
                graph_id,
                target_shard_id,
                target_graph_canister,
                resolved_labels,
                resolved_properties,
                items,
            },
        );
        request.validate()?;
        Ok(request)
    }
}

/// Router-internal vertex-only form selected from [`AtomicInsertRequest`].
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) enum OrderedVertexBatchRequest {
    V1(OrderedVertexBatchRequestV1),
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrderedVertexBatchRequestV1 {
    pub client_mutation_key: String,
    pub graph_name: Option<String>,
    pub items: Vec<AtomicInsertVertexV1>,
}

impl OrderedVertexBatchRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let Self::V1(request) = self;
        if request.client_mutation_key.is_empty() || request.client_mutation_key.len() > 256 {
            return Err("ordered vertex batch client mutation key must be 1..=256 bytes".into());
        }
        if let Some(name) = &request.graph_name
            && name.is_empty()
        {
            return Err("ordered vertex batch graph name must not be empty when present".into());
        }
        if request.items.is_empty() {
            return Err("ordered vertex batch requires at least one item".into());
        }
        for (ordinal, item) in request.items.iter().enumerate() {
            for label in &item.vertex_labels {
                if label.is_empty() {
                    return Err(format!(
                        "ordered vertex item {ordinal} contains an empty label"
                    ));
                }
            }
            let mut property_names = BTreeSet::new();
            for property in &item.initial_properties {
                if property.property_name.is_empty() {
                    return Err(format!(
                        "ordered vertex item {ordinal} contains an empty property name"
                    ));
                }
                if !property_names.insert(&property.property_name) {
                    return Err(format!(
                        "ordered vertex item {ordinal} repeats property name {}",
                        property.property_name
                    ));
                }
                if property.value.len()
                    > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
                {
                    return Err(format!(
                        "ordered vertex item {ordinal} property value exceeds the payload bound"
                    ));
                }
            }
        }
        let encoded =
            Encode!(self).map_err(|error| format!("ordered vertex batch encode: {error}"))?;
        if encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered vertex batch exceeds the safe payload bound".into());
        }
        Ok(())
    }

    pub(crate) fn to_graph_request(
        &self,
        graph_id: GraphId,
        target_shard_id: ShardId,
        target_graph_canister: Principal,
        resolved_labels: gleaph_graph_kernel::plan_exec::ResolvedLabelTable,
        resolved_properties: gleaph_graph_kernel::plan_exec::ResolvedPropertyTable,
    ) -> Result<gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest, String> {
        self.validate()?;
        let Self::V1(request) = self;
        let items = request
            .items
            .iter()
            .enumerate()
            .map(|(ordinal, item)| {
                let labels = item
                    .vertex_labels
                    .iter()
                    .map(|name| {
                        resolved_labels
                            .vertex
                            .iter()
                            .find(|entry| entry.name == *name)
                            .map(|entry| entry.id.raw())
                            .ok_or_else(|| format!("ordered vertex label {name} was not resolved"))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let properties = item
                    .initial_properties
                    .iter()
                    .map(|property| {
                        let property_id = resolved_properties
                            .properties
                            .iter()
                            .find(|entry| entry.name == property.property_name)
                            .map(|entry| entry.id)
                            .ok_or_else(|| {
                                format!(
                                    "ordered vertex property {} was not resolved",
                                    property.property_name
                                )
                            })?;
                        Ok(
                            gleaph_graph_kernel::plan_exec::ResolvedOrderedEdgePropertyV1 {
                                property_id,
                                value: property.value.clone(),
                            },
                        )
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let _ = ordinal;
                Ok(
                    gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphItemV1 {
                        resolved_vertex_labels: labels,
                        resolved_initial_properties: properties,
                    },
                )
            })
            .collect::<Result<Vec<_>, String>>()?;
        let request = gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(
            gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequestV1 {
                graph_id,
                target_shard_id,
                target_graph_canister,
                resolved_labels,
                resolved_properties,
                items,
            },
        );
        request.validate()?;
        Ok(request)
    }
}

/// Router-internal mixed form selected from [`AtomicInsertRequest`].
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) enum OrderedMixedBatchRequest {
    V1(OrderedMixedBatchRequestV1),
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrderedMixedBatchRequestV1 {
    pub client_mutation_key: String,
    pub graph_name: Option<String>,
    pub operations: Vec<AtomicInsertOperationV1>,
}

impl OrderedMixedBatchRequest {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let Self::V1(request) = self;
        if request.client_mutation_key.is_empty() || request.client_mutation_key.len() > 256 {
            return Err("ordered mixed batch client mutation key must be 1..=256 bytes".into());
        }
        if let Some(name) = &request.graph_name
            && name.is_empty()
        {
            return Err("ordered mixed batch graph name must not be empty when present".into());
        }
        if request.operations.is_empty() {
            return Err("ordered mixed batch requires at least one operation".into());
        }
        if !request
            .operations
            .iter()
            .any(|operation| matches!(operation, AtomicInsertOperationV1::Vertex(_)))
            || !request
                .operations
                .iter()
                .any(|operation| matches!(operation, AtomicInsertOperationV1::Edge(_)))
        {
            return Err("ordered mixed batch requires at least one vertex and one edge".into());
        }
        let vertex_count = request
            .operations
            .iter()
            .filter(|operation| matches!(operation, AtomicInsertOperationV1::Vertex(_)))
            .count() as u32;
        for (ordinal, operation) in request.operations.iter().enumerate() {
            match operation {
                AtomicInsertOperationV1::Vertex(item) => {
                    for label in &item.vertex_labels {
                        if label.is_empty() {
                            return Err(format!(
                                "mixed operation {ordinal} contains an empty vertex label"
                            ));
                        }
                    }
                    validate_classified_properties(ordinal, &item.initial_properties)?;
                }
                AtomicInsertOperationV1::Edge(item) => {
                    validate_mixed_endpoint(ordinal, "source", &item.source, vertex_count)?;
                    validate_mixed_endpoint(ordinal, "target", &item.target, vertex_count)?;
                    if let Some(label) = &item.edge_label_name
                        && label.is_empty()
                    {
                        return Err(format!(
                            "mixed operation {ordinal} contains an empty edge label"
                        ));
                    }
                    if let Some(inline) = &item.inline_property
                        && inline.len() > gleaph_graph_kernel::entry::MAX_EDGE_INLINE_PROPERTY_BYTES
                    {
                        return Err(format!(
                            "mixed operation {ordinal} inline property exceeds the byte bound"
                        ));
                    }
                    validate_classified_properties(ordinal, &item.initial_edge_properties)?;
                }
            }
        }
        let encoded =
            Encode!(self).map_err(|error| format!("ordered mixed batch encode: {error}"))?;
        if encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered mixed batch exceeds the safe payload bound".into());
        }
        Ok(())
    }

    /// Convert the classified request into the immutable Graph envelope after Router has resolved
    /// the graph catalogs. Existing endpoints are decoded here so the selected shard is checked
    /// at the Router → Graph boundary; new-vertex ordinals remain request-local references.
    pub(crate) fn to_graph_request(
        &self,
        graph_id: GraphId,
        target_shard_id: ShardId,
        target_graph_canister: Principal,
        encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
        resolved_labels: gleaph_graph_kernel::plan_exec::ResolvedLabelTable,
        resolved_properties: gleaph_graph_kernel::plan_exec::ResolvedPropertyTable,
    ) -> Result<gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest, String> {
        self.validate()?;
        let Self::V1(request) = self;
        let operations = request
            .operations
            .iter()
            .enumerate()
            .map(|(ordinal, operation)| match operation {
                AtomicInsertOperationV1::Vertex(item) => {
                    let labels = item
                        .vertex_labels
                        .iter()
                        .map(|name| {
                            resolved_labels
                                .vertex
                                .iter()
                                .find(|entry| entry.name == *name)
                                .map(|entry| entry.id.raw())
                                .ok_or_else(|| {
                                    format!("ordered mixed vertex label {name} was not resolved")
                                })
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    let properties = item
                        .initial_properties
                        .iter()
                        .map(|property| {
                            resolved_initial_property(
                                &resolved_properties,
                                &property.property_name,
                                property.value.clone(),
                            )
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Ok(
                        gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Vertex(
                            gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphItemV1 {
                                resolved_vertex_labels: labels,
                                resolved_initial_properties: properties,
                            },
                        ),
                    )
                }
                AtomicInsertOperationV1::Edge(item) => {
                    let source = decode_mixed_endpoint(
                        ordinal,
                        "source",
                        &item.source,
                        encoding_key,
                        target_shard_id,
                    )?;
                    let target = decode_mixed_endpoint(
                        ordinal,
                        "target",
                        &item.target,
                        encoding_key,
                        target_shard_id,
                    )?;
                    let catalog_edge_label_id = item
                        .edge_label_name
                        .as_ref()
                        .map(|name| {
                            resolved_labels
                                .edge
                                .iter()
                                .find(|entry| entry.name == *name)
                                .map(|entry| entry.id)
                                .ok_or_else(|| {
                                    format!("ordered mixed edge label {name} was not resolved")
                                })
                        })
                        .transpose()?;
                    let properties = item
                        .initial_edge_properties
                        .iter()
                        .map(|property| {
                            resolved_initial_property(
                                &resolved_properties,
                                &property.property_name,
                                property.value.clone(),
                            )
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Ok(
                        gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Edge(
                            gleaph_graph_kernel::plan_exec::OrderedMixedGraphEdgeItemV1 {
                                source,
                                target,
                                directed: item.directed,
                                catalog_edge_label_id,
                                inline_property_bytes: item
                                    .inline_property
                                    .clone()
                                    .unwrap_or_default(),
                                resolved_initial_edge_properties: properties,
                            },
                        ),
                    )
                }
            })
            .collect::<Result<Vec<_>, String>>()?;
        let graph_request = gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest::V1(
            gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequestV1 {
                graph_id,
                target_shard_id,
                target_graph_canister,
                resolved_labels,
                resolved_properties,
                operations,
            },
        );
        graph_request.validate()?;
        Ok(graph_request)
    }

    /// Return the shard carried by existing endpoints, proving that all such endpoints agree.
    /// New-vertex-only requests return `None`; Router then applies the graph's latest-shard
    /// placement policy.
    pub(crate) fn existing_endpoint_shard(
        &self,
        encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
    ) -> Result<Option<ShardId>, String> {
        self.validate()?;
        let Self::V1(request) = self;
        let mut target_shard = None;
        for (ordinal, operation) in request.operations.iter().enumerate() {
            let AtomicInsertOperationV1::Edge(item) = operation else {
                continue;
            };
            for (name, endpoint) in [("source", &item.source), ("target", &item.target)] {
                let AtomicInsertEndpointV1::Existing(bytes) = endpoint else {
                    continue;
                };
                let encoded: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
                    bytes.as_slice().try_into().map_err(|_| {
                        format!("mixed operation {ordinal} {name} endpoint has invalid width")
                    })?;
                let vertex = gleaph_graph_kernel::federation::decode_global_vertex_id(
                    encoding_key,
                    gleaph_graph_kernel::federation::EncodedVertexId(encoded),
                );
                if let Some(expected) = target_shard {
                    if expected != vertex.shard_id {
                        return Err("ordered mixed batch resolves to multiple shards".into());
                    }
                } else {
                    target_shard = Some(vertex.shard_id);
                }
            }
        }
        Ok(target_shard)
    }
}

fn resolved_initial_property(
    resolved_properties: &gleaph_graph_kernel::plan_exec::ResolvedPropertyTable,
    property_name: &str,
    value: Vec<u8>,
) -> Result<gleaph_graph_kernel::plan_exec::ResolvedOrderedEdgePropertyV1, String> {
    let property_id = resolved_properties
        .properties
        .iter()
        .find(|entry| entry.name == property_name)
        .map(|entry| entry.id)
        .ok_or_else(|| format!("ordered mixed property {property_name} was not resolved"))?;
    Ok(gleaph_graph_kernel::plan_exec::ResolvedOrderedEdgePropertyV1 { property_id, value })
}

fn decode_mixed_endpoint(
    ordinal: usize,
    name: &str,
    endpoint: &AtomicInsertEndpointV1,
    encoding_key: &gleaph_graph_kernel::federation::ElementIdEncodingKey,
    target_shard_id: ShardId,
) -> Result<gleaph_graph_kernel::plan_exec::OrderedMixedGraphEndpointV1, String> {
    match endpoint {
        AtomicInsertEndpointV1::NewVertexOrdinal(vertex_ordinal) => Ok(
            gleaph_graph_kernel::plan_exec::OrderedMixedGraphEndpointV1::NewVertexOrdinal(
                *vertex_ordinal,
            ),
        ),
        AtomicInsertEndpointV1::Existing(bytes) => {
            let encoded: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
                bytes.as_slice().try_into().map_err(|_| {
                    format!("mixed operation {ordinal} {name} endpoint has invalid width")
                })?;
            let vertex = gleaph_graph_kernel::federation::decode_global_vertex_id(
                encoding_key,
                gleaph_graph_kernel::federation::EncodedVertexId(encoded),
            );
            if vertex.shard_id != target_shard_id {
                return Err(format!(
                    "mixed operation {ordinal} {name} endpoint does not target the selected shard"
                ));
            }
            Ok(
                gleaph_graph_kernel::plan_exec::OrderedMixedGraphEndpointV1::Existing(
                    vertex.local_vertex_id,
                ),
            )
        }
    }
}

fn validate_mixed_endpoint(
    ordinal: usize,
    name: &str,
    endpoint: &AtomicInsertEndpointV1,
    vertex_count: u32,
) -> Result<(), String> {
    match endpoint {
        AtomicInsertEndpointV1::Existing(bytes)
            if bytes.len() != gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES =>
        {
            Err(format!(
                "mixed operation {ordinal} {name} must be exactly {} bytes",
                gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
            ))
        }
        AtomicInsertEndpointV1::NewVertexOrdinal(vertex_ordinal)
            if *vertex_ordinal >= vertex_count =>
        {
            Err(format!(
                "mixed operation {ordinal} {name} references unknown vertex ordinal {vertex_ordinal}"
            ))
        }
        _ => Ok(()),
    }
}

fn validate_classified_properties(
    ordinal: usize,
    properties: &[AtomicInsertPropertyV1],
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for property in properties {
        if property.property_name.is_empty() {
            return Err(format!(
                "mixed operation {ordinal} contains an empty property name"
            ));
        }
        if !names.insert(&property.property_name) {
            return Err(format!(
                "mixed operation {ordinal} repeats property name {}",
                property.property_name
            ));
        }
        if property.value.len()
            > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        {
            return Err(format!(
                "mixed operation {ordinal} property value exceeds the payload bound"
            ));
        }
    }
    Ok(())
}

impl MutationStatus {
    const ALL_PHASES: [MutationLifecyclePhase; 6] = [
        MutationLifecyclePhase::Routing,
        MutationLifecyclePhase::CanonicalPending,
        MutationLifecyclePhase::CanonicalCommitted,
        MutationLifecyclePhase::ProjectionPending,
        MutationLifecyclePhase::Completed,
        MutationLifecyclePhase::Failed,
    ];

    fn next_action_for_phase(phase: MutationLifecyclePhase) -> &'static str {
        match phase {
            MutationLifecyclePhase::Completed => "none",
            MutationLifecyclePhase::Failed => "resubmit with a new client_mutation_key",
            MutationLifecyclePhase::Routing => {
                "routing in progress; retry the idempotent mutation if it does not settle"
            }
            MutationLifecyclePhase::CanonicalPending => {
                "retry the idempotent mutation to resume the remaining canonical shard writes"
            }
            MutationLifecyclePhase::CanonicalCommitted
            | MutationLifecyclePhase::ProjectionPending => {
                "none; projection recovery is automatic (poll mutation_status or use AtLeast reads)"
            }
        }
    }

    pub fn from_record(record: &RouterMutationRecord) -> Self {
        let phase = record.lifecycle_phase();
        let target_shard = record
            .shards()
            .iter()
            .find(|shard| !shard.completed())
            .or_else(|| {
                record
                    .shards()
                    .iter()
                    .find(|shard| !shard.projection_advanced())
            })
            .map(|shard| shard.shard_id());
        let next_action = Self::next_action_for_phase(phase).to_string();
        Self {
            mutation_id: record.as_v1().mutation_id,
            phase,
            last_error: record.as_v1().last_error.clone(),
            target_shard,
            next_action,
        }
    }
}

#[derive(CandidType, Deserialize)]
pub struct GrantCapsArgs {
    pub target: Principal,
    /// Full replacement capability bitmask (`gleaph_auth::AdminCaps` bits).
    pub caps: u64,
}

// ──── Data-plane grant introspection (ADR 0074 §5, slice 2a) ────

/// Subject of a listed grant row: a concrete principal (text form) or the virtual
/// `PUBLIC` pseudo-subject.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GrantSubjectView {
    Principal(String),
    Public,
}

/// Logical traversal direction of a directional `TRAVERSE` grant
/// (`OUTGOING = source → target`). Absent on non-directional rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GrantDirectionView {
    Outgoing,
    Incoming,
}

/// Privilege operation of a listed grant row ([ADR 0074] §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GrantOperationView {
    Match,
    Traverse,
    Read,
    ReadProperty,
    Create,
    Update,
    Delete,
    /// Marker of the registry owner's implicit root authority over the whole graph
    /// (ADR 0074 §3 invariant 3). Synthesized by introspection only — ownership is
    /// never materialized as a stored grant row.
    ImplicitRoot,
}

/// Kind of the granted resource selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GrantResourceKindView {
    Vertex,
    Edge,
    /// Whole-graph coverage marker; used only by the synthesized
    /// [`GrantOperationView::ImplicitRoot`] entry.
    Graph,
}

/// Resource of a listed grant row: selector kind plus reverse-resolved names.
///
/// `property` is set only for `READ_PROPERTY` rows and names one property of the
/// vertex label. For `Graph`-kind entries (the implicit-root marker) `label` carries
/// the graph name and `property` is `None`.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GrantResourceView {
    pub kind: GrantResourceKindView,
    pub label: String,
    pub property: Option<String>,
}

/// One stored grant row of one graph, as listed by the owner-only introspection query.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphGrantSummary {
    pub subject: GrantSubjectView,
    pub operation: GrantOperationView,
    pub direction: Option<GrantDirectionView>,
    pub resource: GrantResourceView,
    /// Dormant expiry semantics ([ADR 0074] §1b): reads treat expired rows as absent.
    pub expires_at_ns: Option<u64>,
    /// Compiled conditional-policy condition printed inline ([ADR 0075] §1), e.g.
    /// `WHERE visibility = 'public' AND owner = MSG_CALLER()`. Property names resolve
    /// through the graph catalogs; unresolved ids print as `<property N>`.
    pub predicate: Option<String>,
}

/// Arguments for one expired client-mutation-key sweep step. The sweep is
/// operator-driven (like backfill / label-stats projection): call repeatedly,
/// feeding `next_cursor` back as `start_after`, until `done` is true.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminSweepMutationKeysStepArgs {
    /// Resume scanning strictly after this key; `None` starts from the beginning.
    pub start_after: Option<ClientMutationKey>,
    /// Maximum journal entries to scan in this step (must be > 0).
    pub max_scan: u32,
}

/// Progress from one expired client-mutation-key sweep step.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminSweepMutationKeysStepResult {
    /// Entries examined in this step.
    pub scanned: u32,
    /// Expired entries removed in this step.
    pub removed: u32,
    /// Feed back as `start_after` to continue; `None` when the scan reached the end.
    pub next_cursor: Option<ClientMutationKey>,
    /// True when the whole journal has been scanned in this step.
    pub done: bool,
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminRegisterShardArgs {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
    pub logical_graph_name: String,
}

/// Admin: wire (or retrofit) a derived vector-index target onto an already-registered shard and
/// drive the attach handshake (ADR 0031 Slice 4). The Router records the target in the shard
/// registry, calls the graph shard's router-guarded `admin_set_vector_canister` so its
/// **local** `FederationRouting` carries the target, attaches the shard to the vector canister, and
/// only then flips the durable `vector_index_attached` readiness bit. Idempotent; serves both fresh
/// and existing (upgraded) shards. Rejects an anonymous target.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminAttachVectorIndexShardArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    pub vector_canister: Principal,
}

/// One router-orchestrated batch of label posting backfill on a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminLabelBackfillStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum local vertices to scan on the shard in this step (must be > 0).
    pub max_vertices: u32,
}

/// Progress from one router backfill step.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminLabelBackfillStepResult {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}

/// Router-stable cursor for label posting backfill on one shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBackfillShardStatus {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub done: bool,
}

/// One router-orchestrated batch of vertex property posting backfill on a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminVertexPropertyBackfillStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum local vertices to scan on the shard in this step (must be > 0).
    pub max_vertices: u32,
}

/// Progress from one router vertex property backfill step.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminVertexPropertyBackfillStepResult {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}

/// Router-stable cursor for vertex property posting backfill on one shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexPropertyBackfillShardStatus {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub done: bool,
}

/// One graph shard whose graph-index convergence is queried through the Router.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminIndexSyncStatusArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
}

/// One router-orchestrated batch of edge property posting backfill on a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminEdgeBackfillStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum edge property entries to scan on the shard in this step (must be > 0).
    pub max_entries: u32,
}

/// Progress from one router edge backfill step.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminEdgeBackfillStepResult {
    pub shard_id: ShardId,
    pub next_after_key: Option<Vec<u8>>,
    pub entries_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}

/// Router-stable cursor for edge property posting backfill on one shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EdgeBackfillShardStatus {
    pub shard_id: ShardId,
    pub after_key: Option<Vec<u8>>,
    pub done: bool,
}

/// Which posting-backfill cursor a reset (and `advance_backfill`, ADR 0056 §4) targets.
/// `LabelStats` drives the label-stats projection drain.
#[derive(
    CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum BackfillKind {
    Label,
    VertexProperty,
    Edge,
    LabelStats,
}

/// One shard's advance outcome within `advance_backfill` (ADR 0056 §4).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct BackfillShardAdvance {
    pub shard_id: ShardId,
    pub done: bool,
}

/// Graph-level result of one `advance_backfill` call (one bounded unit per shard; the Router
/// iterates shards internally, so shard ids are not exposed at L2 as arguments).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct AdvanceBackfillResult {
    pub all_done: bool,
    pub shards: Vec<BackfillShardAdvance>,
}

/// One shard's backfill status in the kind-keyed `list_backfill_status` view (ADR 0056 §4).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Deserialize, Serialize)]
pub struct BackfillShardStatus {
    pub kind: BackfillKind,
    pub shard_id: ShardId,
    pub done: bool,
}

/// Operator recovery: clear a stuck `in_progress` claim on one shard's backfill
/// cursor (see ADR 0009). Only use after confirming no step is in flight for the
/// shard, since clearing a legitimately in-flight claim re-enables the cursor race.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminResetBackfillClaimArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    pub kind: BackfillKind,
}

/// One router-orchestrated batch advancing label stats projection for a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminLabelStatsProjectionStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum pending deltas to apply from the shard log in this step (must be > 0).
    pub max_deltas: u32,
}

/// Progress from one router label stats projection step.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminLabelStatsProjectionStepResult {
    pub shard_id: ShardId,
    pub deltas_drained: u32,
    pub deltas_applied: u32,
    pub done: bool,
}

/// Wire view of a derived vector-index activation state (ADR 0031 Slice 3). Mirrors the internal
/// `VectorIndexActivationState`; `DispatchEnabled` is unreachable in Slice 3 (fail-closed gate).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorIndexActivationStateView {
    Registered,
    DispatchBlocked,
    DispatchEnabled,
}

/// Admin: register a derived vector index for a logical graph (ADR 0031 Slice 3). The embedding is
/// identified **by name** (the Router interns it to a stable `EmbeddingNameId`), never by a raw id.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RegisterVectorIndexArgs {
    pub logical_graph_name: String,
    pub embedding_name: String,
    pub index_id: u32,
    pub dims: u16,
    /// Creation-fixed label set the index is scoped to (ADR 0064 §Router catalog); must be non-empty.
    pub labels: Vec<String>,
    /// Optional metric; defaults to `L2Squared` if omitted for wire stability.
    pub metric: Option<VectorMetric>,
    /// Optional stored encoding; defaults to `F32` if omitted for wire stability (I8 = scalar
    /// quantization). The wire embedding bytes are always canonical F32.
    pub encoding: Option<VectorEncoding>,
    /// Optional single dispatch target; rejected if anonymous. Slice 3 stores it as inspect-only
    /// metadata and never pushes it to graph shards or enables dispatch.
    pub target: Option<Principal>,
    pub if_not_exists: bool,
}

/// Admin: assign the single dispatch target of an existing vector index (ADR 0031 Slice 3).
/// Replaying the same target is idempotent; a different target is rejected.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SetVectorIndexTargetArgs {
    pub logical_graph_name: String,
    pub index_id: u32,
    pub target: Principal,
}

/// Wire view of a stored vector-index definition (ADR 0031 Slice 3). Algorithm-neutral: physical
/// search knobs (centroids, nlist, page geometry) are deliberately not exposed.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorIndexInfo {
    pub index_id: u32,
    pub embedding_name_id: u16,
    pub dims: u16,
    pub metric: VectorMetric,
    pub target: Option<Principal>,
    pub activation_state: VectorIndexActivationStateView,
}

/// Activation status + fail-closed explanation for one vector-index definition (ADR 0031 Slice 3).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorIndexActivationStatus {
    pub index_id: u32,
    pub activation_state: VectorIndexActivationStateView,
    /// `Some(reason)` while production dispatch/backfill is fail-closed; `None` otherwise.
    pub blocked_reason: Option<String>,
}

/// Physical stable-memory inventory for one registered graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GraphStableMemoryStats {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub memory: gleaph_graph_kernel::stable_memory::StableMemoryStats,
}

/// One page of the Graph shard's batch instruction log, forwarded by the Router
/// so callers do not need Router principal access.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GraphBatchInstrLogPage {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub lines: Vec<String>,
}

/// Public exact vector-search request (ADR 0031 Slice 5). The Router resolves the
/// `logical_graph_name` and `index_id` to the single activated target and forwards an exact
/// `ivf_flat` scan. The `F32` encoding and metric are supplied from the stored definition.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterVectorSearchRequest {
    pub logical_graph_name: String,
    pub index_id: u32,
    /// `dims * 4` bytes of little-endian `f32` query components.
    pub query: Vec<u8>,
    pub dims: u16,
    pub top_k: u32,
}

/// Admin: ingest one finite F32 vertex embedding through Router into the owning Graph shard
/// (plan 0048). The caller supplies only the logical graph name, the opaque encoded vertex id,
/// the registered embedding name, and the vector values; Router resolves ownership and the
/// definition, asks Graph to validate vertex metadata, and delivers the values to Vector.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AdminIngestVertexEmbeddingArgs {
    pub logical_graph_name: String,
    /// Opaque 8-byte graph-scoped vertex id (`ELEMENT_ID(v)`).
    pub encoded_vertex_id: Vec<u8>,
    pub embedding_name: String,
    pub values: Vec<f32>,
}

/// One item in a batch vertex-embedding ingestion request.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AdminIngestVertexEmbeddingBatchItem {
    pub encoded_vertex_id: Vec<u8>,
    pub values: Vec<f32>,
}

/// Admin: ingest many finite F32 vertex embeddings through Router into the owning Graph shard(s)
/// via the Router-initiated two-call flow (ADR 0064 §6). Router durably owns each allocated stamp
/// before Graph `stamp_embedding` validates vertex metadata; accepted intents then proceed to
/// Vector with bytes + stamp.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AdminIngestVertexEmbeddingBatchArgs {
    pub logical_graph_name: String,
    pub embedding_name: String,
    pub items: Vec<AdminIngestVertexEmbeddingBatchItem>,
}

/// Admin: create or replace a vector maintenance policy (ADR 0031 Slice 10). Router-owned SSOT for
/// maintenance thresholds + per-step budgets; validated and stored only when the vector-index
/// definition exists. Default state is absent (the push scheduler is a no-op until set + enabled).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SetVectorMaintenancePolicyArgs {
    pub logical_graph_name: String,
    pub index_id: u32,
    pub enabled: bool,
    pub policy: VectorMaintenancePolicy,
    pub target_nlist: Option<u32>,
    pub sample_limit: u32,
    pub scan_max_pages: u32,
    pub rebuild_max_subjects: u32,
    pub cleanup_max_work: u32,
}

/// Operator-facing view of a stored vector maintenance policy (ADR 0031 Slice 10).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorMaintenancePolicyView {
    pub graph_id: u32,
    pub index_id: u32,
    pub enabled: bool,
    pub policy: VectorMaintenancePolicy,
    pub target_nlist: Option<u32>,
    pub sample_limit: u32,
    pub scan_max_pages: u32,
    pub rebuild_max_subjects: u32,
    pub cleanup_max_work: u32,
}

impl From<VectorMaintenancePolicyRecord> for VectorMaintenancePolicyView {
    fn from(record: VectorMaintenancePolicyRecord) -> Self {
        Self {
            graph_id: record.graph_id.raw(),
            index_id: record.index_id,
            enabled: record.enabled,
            policy: record.policy,
            target_nlist: record.target_nlist,
            sample_limit: record.sample_limit,
            scan_max_pages: record.scan_max_pages,
            rebuild_max_subjects: record.rebuild_max_subjects,
            cleanup_max_work: record.cleanup_max_work,
        }
    }
}

/// Outcome of one Router-push maintenance step (ADR 0031 Slice 10). `Disabled` is a Router-level
/// no-op (absent or disabled policy); otherwise the vector canister's bounded step result is relayed.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum VectorMaintenanceStepOutcome {
    /// No policy exists or it is disabled; no work was forwarded.
    Disabled,
    /// The vector canister advanced one bounded maintenance unit.
    Stepped(VectorMaintenanceStepResult),
}

/// Cursor-redacted projection of the vector canister's [`VectorMaintenanceState`] for the Router
/// aggregate status (ADR 0031 Slice 10). The opaque resume cursor bytes are collapsed to a
/// `cursor_present` flag so the Router status surface honours the "present/absent, not decoded"
/// contract and never leaks internal stable `PageKey` bytes.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum VectorMaintenanceStateView {
    /// No maintenance in progress.
    Idle,
    /// A bounded page-health scan is accumulating counters; the resume cursor is reported as
    /// present/absent only.
    Scanning {
        /// Whether a resume cursor is set (`true`) or the scan would (re)start from the lower bound.
        cursor_present: bool,
        /// `true` once the scan has covered every page of the scoped version.
        exhausted: bool,
        /// Additive page-health accumulated so far, scoped by its `index_id`/`index_version`.
        merged: VectorPartitionPageHealth,
    },
    /// A prior step failed; recovery requires an explicit `admin_vector_maintenance_reset`.
    Failed(VectorMaintenanceFailure),
}

impl From<VectorMaintenanceState> for VectorMaintenanceStateView {
    fn from(state: VectorMaintenanceState) -> Self {
        match state {
            VectorMaintenanceState::Idle => Self::Idle,
            VectorMaintenanceState::Scanning {
                cursor,
                exhausted,
                merged,
            } => Self::Scanning {
                cursor_present: cursor.is_some(),
                exhausted,
                merged,
            },
            VectorMaintenanceState::Failed(failure) => Self::Failed(failure),
        }
    }
}

/// Aggregated maintenance status for one vector index (ADR 0031 Slice 10): Router-owned policy +
/// readiness, plus the forwarded vector-canister execution and rebuild state when reachable. Cursors
/// inside `maintenance_state` are reported present/absent, not decoded.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorMaintenanceStatusView {
    pub index_id: u32,
    /// Whether a Router policy exists and is enabled.
    pub policy_enabled: bool,
    /// Resolved single vector target, if set.
    pub target: Option<Principal>,
    /// Per-graph dispatch readiness (global flag on AND shards vector-attached).
    pub dispatch_ready: bool,
    /// `Some(reason)` while forwarding is fail-closed; `None` once ready.
    pub blocked_reason: Option<String>,
    /// Forwarded vector-canister maintenance execution state with the resume cursor redacted to a
    /// present/absent flag; `None` if unreachable.
    pub maintenance_state: Option<VectorMaintenanceStateView>,
    /// Forwarded vector-canister rebuild status; `None` if unreachable.
    pub rebuild_status: Option<VectorRebuildStatus>,
}

// === ADR 0035 provisioning types ==============================================

/// Stable-memory key for Map 45: RouterProvisioningRequest by (request_id, deployment_id).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProvisioningRequestKey {
    pub request_id: [u8; 32],
    pub deployment_id: String,
}

impl ProvisioningRequestKey {
    pub(crate) fn new(request_id: &[u8; 32], deployment_id: &str) -> Self {
        Self {
            request_id: *request_id,
            deployment_id: deployment_id.to_owned(),
        }
    }
}

impl Storable for ProvisioningRequestKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + 4 + self.deployment_id.len());
        out.extend_from_slice(&self.request_id);
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut request_id = [0u8; 32];
        request_id.copy_from_slice(&bytes[0..32]);
        let mut offset = 32usize;
        let deployment_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("deployment_id len"),
        ) as usize;
        offset += 4;
        let deployment_id = String::from_utf8(bytes[offset..offset + deployment_id_len].to_vec())
            .expect("deployment_id utf8");
        Self {
            request_id,
            deployment_id,
        }
    }
}

/// Secondary index key for Map 46: (deployment_id, graph_name, request_id) → ProvisioningRequestKey.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProvisioningByGraphKey {
    pub deployment_id: String,
    pub graph_name: String,
    pub request_id: [u8; 32],
}

impl ProvisioningByGraphKey {
    pub(crate) fn new(deployment_id: &str, graph_name: &str, request_id: &[u8; 32]) -> Self {
        Self {
            deployment_id: deployment_id.to_owned(),
            graph_name: graph_name.to_owned(),
            request_id: *request_id,
        }
    }
}

impl Storable for ProvisioningByGraphKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.deployment_id.len() + self.graph_name.len() + 32);
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out.extend_from_slice(&(self.graph_name.len() as u32).to_le_bytes());
        out.extend_from_slice(self.graph_name.as_bytes());
        out.extend_from_slice(&self.request_id);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut offset = 0usize;
        let deployment_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("deployment_id len"),
        ) as usize;
        offset += 4;
        let deployment_id = String::from_utf8(bytes[offset..offset + deployment_id_len].to_vec())
            .expect("deployment_id utf8");
        offset += deployment_id_len;
        let graph_name_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("graph_name len"),
        ) as usize;
        offset += 4;
        let graph_name = String::from_utf8(bytes[offset..offset + graph_name_len].to_vec())
            .expect("graph_name utf8");
        offset += graph_name_len;
        let mut request_id = [0u8; 32];
        request_id.copy_from_slice(&bytes[offset..offset + 32]);
        Self {
            deployment_id,
            graph_name,
            request_id,
        }
    }
}

/// Identity of the provisioning request that holds an intent lock in Map 47.
///
/// Each lock is owner-bound to a specific `(request_id, deployment_id)`, so a lock held by one
/// request cannot satisfy the preflight of another request, and a release can only remove locks
/// owned by the request being advanced or rolled back. `request_id` is the content hash, so it
/// already distinguishes different request content; no separate fingerprint is needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IntentLockOwner {
    pub request_key: ProvisioningRequestKey,
}

impl IntentLockOwner {
    pub(crate) fn new(request_key: ProvisioningRequestKey) -> Self {
        Self { request_key }
    }
}

impl Storable for IntentLockOwner {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.request_key.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.request_key.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self {
            request_key: ProvisioningRequestKey::from_bytes(bytes),
        }
    }
}

/// Router-side lifecycle state for a provisioning request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub(crate) enum RouterProvisioningRequestState {
    Pending,
    Submitted,
    AwaitingAck,
    Completed,
    Failed { reason: String },
}

/// Router canonical record for an issuance intent (ADR 0035 §Router orchestration state).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub(crate) struct RouterProvisioningRequest {
    pub request_id: [u8; 32],
    pub caller: Principal,
    pub owner: Principal,
    pub admins: BTreeSet<Principal>,
    pub provision_target: Principal,
    /// Exact Candid argument bytes dispatched to Provision. Retry sends these bytes verbatim.
    pub resolved_request_bytes: Vec<u8>,
    pub state: RouterProvisioningRequestState,
    pub created_at_ns: u64,
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum RouterProvisioningRequestStableRecord {
    V1(RouterProvisioningRequest),
}

impl Storable for RouterProvisioningRequest {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&RouterProvisioningRequestStableRecord::V1(self.clone()))
                .expect("encode RouterProvisioningRequest"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&RouterProvisioningRequestStableRecord::V1(self))
            .expect("encode RouterProvisioningRequest")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), RouterProvisioningRequestStableRecord)
            .expect("decode RouterProvisioningRequest")
        {
            RouterProvisioningRequestStableRecord::V1(v1) => v1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::label_stats::RouterMutationShardV1;

    fn shard(id: u32, completed: bool, projection_advanced: bool) -> RouterMutationShardV1 {
        let mut shard = RouterMutationShardV1::new(ShardId::new(id), Principal::anonymous(), None);
        shard.set_completed(completed);
        shard.set_projection_advanced(projection_advanced);
        shard
    }

    fn record_with(shards: Vec<RouterMutationShardV1>) -> RouterMutationRecord {
        let mut record = RouterMutationRecord::new(7, 0, Vec::new());
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().payload =
            crate::facade::stable::label_stats::RouterMutationPayloadV1::Scalar { shards };
        record
    }

    #[test]
    fn status_for_canonical_pending_points_at_outstanding_shard_and_asks_retry() {
        let record = record_with(vec![shard(0, true, true), shard(1, false, false)]);
        let status = MutationStatus::from_record(&record);
        assert_eq!(status.mutation_id, 7);
        assert_eq!(status.phase, MutationLifecyclePhase::CanonicalPending);
        assert_eq!(status.target_shard, Some(ShardId::new(1)));
        assert!(status.next_action.contains("retry"));
    }

    #[test]
    fn status_for_projection_pending_is_automatic_recovery() {
        let record = record_with(vec![shard(0, true, false)]);
        let status = MutationStatus::from_record(&record);
        assert_eq!(status.phase, MutationLifecyclePhase::CanonicalCommitted);
        assert_eq!(status.target_shard, Some(ShardId::new(0)));
        assert!(status.next_action.starts_with("none"));
    }

    #[test]
    fn status_for_completed_has_no_target_or_action() {
        let mut record = record_with(vec![shard(0, true, true)]);
        record.as_v1_mut().completed_row_count = Some(3);
        let status = MutationStatus::from_record(&record);
        assert_eq!(status.phase, MutationLifecyclePhase::Completed);
        assert_eq!(status.target_shard, None);
        assert_eq!(status.next_action, "none");
    }

    #[test]
    fn bulk_load_status_page_64_fits_safe_candid_bound() {
        validate_max_receipts(MAX_BULK_LOAD_RECEIPTS_PER_PAGE)
            .expect("maximum status page must fit the safe response bound");
    }

    #[test]
    fn bulk_load_status_page_65_rejects_before_iteration() {
        let error = validate_max_receipts(MAX_BULK_LOAD_RECEIPTS_PER_PAGE + 1)
            .expect_err("status page cap must reject before stable iteration");
        assert!(error.contains("1..=64"), "{error}");
    }

    #[test]
    fn bulk_load_chunk_fingerprint_normalizes_catalog_order_but_not_item_order() {
        let first = BulkLoadChunkV1::Vertices(vec![AtomicInsertVertexV1 {
            vertex_labels: vec!["z".into(), "a".into()],
            initial_properties: vec![
                AtomicInsertPropertyV1 {
                    property_name: "z".into(),
                    value: vec![2],
                },
                AtomicInsertPropertyV1 {
                    property_name: "a".into(),
                    value: vec![1],
                },
            ],
        }]);
        let reordered_catalogs = BulkLoadChunkV1::Vertices(vec![AtomicInsertVertexV1 {
            vertex_labels: vec!["a".into(), "z".into()],
            initial_properties: vec![
                AtomicInsertPropertyV1 {
                    property_name: "a".into(),
                    value: vec![1],
                },
                AtomicInsertPropertyV1 {
                    property_name: "z".into(),
                    value: vec![2],
                },
            ],
        }]);
        assert_eq!(
            first.fingerprint().unwrap(),
            reordered_catalogs.fingerprint().unwrap()
        );
        let changed = BulkLoadChunkV1::Vertices(vec![
            AtomicInsertVertexV1 {
                vertex_labels: vec!["a".into()],
                initial_properties: Vec::new(),
            },
            AtomicInsertVertexV1 {
                vertex_labels: vec!["z".into()],
                initial_properties: Vec::new(),
            },
        ]);
        assert_ne!(first.fingerprint().unwrap(), changed.fingerprint().unwrap());
    }

    #[test]
    fn bulk_load_chunk_exceeding_atomic_insert_cap_is_admitted() {
        // Resumable bulk-load chunks are not capped at MAX_ATOMIC_INSERT_OPERATIONS (ADR 0060):
        // the runtime instruction budget decides the committed prefix, and the payload bound
        // bounds the candidate size.
        let chunk = BulkLoadChunkV1::Vertices(
            (0..MAX_ATOMIC_INSERT_OPERATIONS + 1)
                .map(|_| AtomicInsertVertexV1 {
                    vertex_labels: vec!["Person".to_owned()],
                    initial_properties: Vec::new(),
                })
                .collect(),
        );
        chunk
            .validate()
            .expect("chunk above the atomic-insert cap must pass bulk-load validation");
    }

    #[test]
    fn bulk_load_chunk_above_atomic_insert_cap_classifies() {
        // ADR 0060 §3: `bulk_load` has no fixed operation cap; the classification path used by
        // `build_graph_request` must admit chunks above MAX_ATOMIC_INSERT_OPERATIONS even though
        // the shared atomic-insert request validate still caps the public `atomic_insert` path.
        let vertex = || AtomicInsertVertexV1 {
            vertex_labels: vec!["Person".to_owned()],
            initial_properties: Vec::new(),
        };
        let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "bulk-key".into(),
            graph_name: Some("tenant.main".into()),
            operations: (0..MAX_ATOMIC_INSERT_OPERATIONS + 1)
                .map(|_| AtomicInsertOperationV1::Vertex(vertex()))
                .collect(),
        });
        let classified = request
            .clone()
            .into_classified_bulk()
            .expect("bulk chunk above the atomic-insert cap must classify");
        assert!(matches!(
            classified,
            ClassifiedAtomicInsertRequest::Vertex(_)
        ));
        // The public atomic-insert path still enforces the cap.
        assert!(request.validate().is_err());
        // An empty bulk chunk is still rejected.
        let empty = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "bulk-key".into(),
            graph_name: Some("tenant.main".into()),
            operations: Vec::new(),
        });
        assert!(empty.into_classified_bulk().is_err());
    }

    #[test]
    fn batch_response_projects_graph_specific_receipts_to_common_counts() {
        use gleaph_graph_kernel::plan_exec::{
            GraphOrderedEdgeBatchReceiptV1, GraphOrderedMixedBatchReceiptV1,
            GraphOrderedVertexBatchReceiptV1, MutationTokenShard,
        };

        let watermark = MutationTokenShard {
            shard_id: ShardId::new(0),
            label_stats_seq: None,
        };
        let encoding_key =
            gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture();
        let encoded = |local_id| {
            gleaph_graph_kernel::federation::encode_global_vertex_id(
                &encoding_key,
                GlobalVertexId::new(ShardId::new(0), local_id),
            )
            .0
            .to_vec()
        };
        let mut record = record_with(Vec::new());
        record.as_v1_mut().payload =
            crate::facade::stable::label_stats::RouterMutationPayloadV1::CompletedOrderedEdgeBatch {
                graph_request_fingerprint: [0; 32],
                receipt: GraphOrderedEdgeBatchReceiptV1 {
                    logical_edge_count: 3,
                    emitted_delta_first_seq: None,
                    emitted_delta_last_seq: None,
                    hot_forward_vertices: Vec::new(),
                },
                projection_watermark: watermark.clone(),
            };
        assert_eq!(
            AtomicInsertResponse::from_record(&record).receipt,
            Some(AtomicInsertReceiptV1 {
                logical_operation_count: 3,
                logical_vertex_count: 0,
                logical_edge_count: 3,
                allocated_vertex_ids: Vec::new(),
            })
        );

        record.as_v1_mut().payload = crate::facade::stable::label_stats::RouterMutationPayloadV1::
            CompletedOrderedVertexBatch {
                graph_request_fingerprint: [0; 32],
                receipt: GraphOrderedVertexBatchReceiptV1 {
                    logical_vertex_count: 2,
                    emitted_delta_first_seq: None,
                    emitted_delta_last_seq: None,
                    hot_forward_vertices: Vec::new(),
                    allocated_vertex_ids: vec![2, 3],
                },
                projection_watermark: watermark.clone(),
            };
        assert_eq!(
            AtomicInsertResponse::from_record(&record).receipt,
            Some(AtomicInsertReceiptV1 {
                logical_operation_count: 2,
                logical_vertex_count: 2,
                logical_edge_count: 0,
                allocated_vertex_ids: vec![encoded(2), encoded(3)],
            })
        );

        record.as_v1_mut().payload = crate::facade::stable::label_stats::RouterMutationPayloadV1::
            CompletedOrderedMixedBatch {
                graph_request_fingerprint: [0; 32],
                receipt: GraphOrderedMixedBatchReceiptV1 {
                    logical_operation_count: 5,
                    logical_vertex_count: 2,
                    logical_edge_count: 3,
                    emitted_delta_first_seq: None,
                    emitted_delta_last_seq: None,
                    hot_forward_vertices: Vec::new(),
                    allocated_vertex_ids: vec![2, 3],
                },
                projection_watermark: watermark,
            };
        assert_eq!(
            AtomicInsertResponse::from_record(&record).receipt,
            Some(AtomicInsertReceiptV1 {
                logical_operation_count: 5,
                logical_vertex_count: 2,
                logical_edge_count: 3,
                allocated_vertex_ids: vec![encoded(2), encoded(3)],
            })
        );
    }

    #[test]
    fn atomic_insert_response_preflight_uses_full_public_status_envelope() {
        let operation_count = MAX_ATOMIC_INSERT_OPERATIONS;
        let vertex_count = MAX_ATOMIC_INSERT_OPERATIONS;
        let worst_case = worst_case_atomic_insert_response_size(operation_count, vertex_count)
            .expect("worst-case response size");

        let undercounted: Result<AtomicInsertResponse, crate::state::RouterError> =
            Ok(AtomicInsertResponse {
                status: MutationStatus {
                    mutation_id: u64::MAX,
                    phase: MutationLifecyclePhase::Completed,
                    last_error: None,
                    target_shard: None,
                    next_action: String::new(),
                },
                receipt: Some(AtomicInsertReceiptV1 {
                    logical_operation_count: operation_count as u64,
                    logical_vertex_count: vertex_count as u64,
                    logical_edge_count: 0,
                    allocated_vertex_ids: vec![
                        vec![0; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES];
                        vertex_count
                    ],
                }),
            });
        let undercounted_size = Encode!(&undercounted)
            .expect("encode undercounted response")
            .len();
        assert!(
            worst_case > undercounted_size,
            "omitting diagnostics, target shard, and real next_action must undercount"
        );
        assert!(
            worst_case <= gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
            "the configured atomic insert maximum must remain publicly returnable"
        );
    }

    #[test]
    fn atomic_insert_response_preflight_enforces_exact_encoded_boundary() {
        let exact =
            worst_case_atomic_insert_response_size(7, 5).expect("exact worst-case response size");
        preflight_atomic_insert_response_size_at_limit(7, 5, exact)
            .expect("an exact-boundary response fits");
        assert_eq!(
            preflight_atomic_insert_response_size_at_limit(7, 5, exact - 1),
            Err("atomic insert worst-case response exceeds the safe payload bound".into()),
            "a receipt-only or empty-status implementation would wrongly accept this limit"
        );
    }

    #[test]
    fn atomic_insert_request_round_trips_fingerprints_without_client_key_and_classifies_edge() {
        let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "ordered-1".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(vec![1; 8]),
                target: AtomicInsertEndpointV1::Existing(vec![2; 8]),
                directed: true,
                edge_label_name: Some("KNOWS".into()),
                inline_property: Some(vec![1, 2]),
                initial_edge_properties: vec![AtomicInsertPropertyV1 {
                    property_name: "weight".into(),
                    value: vec![3, 4],
                }],
            })],
        });
        request.validate().expect("valid atomic insert request");
        let fingerprint = request
            .public_fingerprint()
            .expect("atomic insert fingerprint");
        let bytes = Encode!(&request).expect("encode atomic insert request");
        let decoded: AtomicInsertRequest =
            Decode!(&bytes, AtomicInsertRequest).expect("decode atomic insert request");
        assert_eq!(decoded, request);
        let mut retry = request.clone();
        let AtomicInsertRequest::V1(ref mut retry_request) = retry;
        retry_request.client_mutation_key = "different-retry-key".into();
        assert_eq!(retry.public_fingerprint().unwrap(), fingerprint);
        let (classified, classified_fingerprint) = request.into_classified().unwrap();
        assert_eq!(classified_fingerprint, fingerprint);
        assert!(matches!(classified, ClassifiedAtomicInsertRequest::Edge(_)));
    }

    #[test]
    fn atomic_insert_request_classifies_vertex_only_operations() {
        let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "vertex-1".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                vertex_labels: vec!["Person".into(), "User".into()],
                initial_properties: vec![AtomicInsertPropertyV1 {
                    property_name: "name".into(),
                    value: vec![1, 2],
                }],
            })],
        });
        let (classified, _) = request.into_classified().unwrap();
        assert!(matches!(
            classified,
            ClassifiedAtomicInsertRequest::Vertex(_)
        ));
    }

    #[test]
    fn atomic_insert_request_classifies_mixed_operations_and_validates_vertex_ordinals() {
        let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "mixed-1".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![
                AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                    vertex_labels: vec!["Person".into()],
                    initial_properties: Vec::new(),
                }),
                AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                    source: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                    target: AtomicInsertEndpointV1::Existing(vec![2; 8]),
                    directed: true,
                    edge_label_name: None,
                    inline_property: None,
                    initial_edge_properties: Vec::new(),
                }),
            ],
        });
        let (classified, _) = request.into_classified().unwrap();
        assert!(matches!(
            classified,
            ClassifiedAtomicInsertRequest::Mixed(_)
        ));
    }

    #[test]
    fn ordered_mixed_target_shard_comes_from_existing_endpoints() {
        let key = gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture();
        let existing = gleaph_graph_kernel::federation::encode_global_vertex_id(
            &key,
            GlobalVertexId::new(ShardId::new(4), 11),
        );
        let request = OrderedMixedBatchRequest::V1(OrderedMixedBatchRequestV1 {
            client_mutation_key: "mixed-placement".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![
                AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                    vertex_labels: Vec::new(),
                    initial_properties: Vec::new(),
                }),
                AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                    source: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                    target: AtomicInsertEndpointV1::Existing(existing.0.to_vec()),
                    directed: true,
                    edge_label_name: None,
                    inline_property: None,
                    initial_edge_properties: Vec::new(),
                }),
            ],
        });
        assert_eq!(
            request.existing_endpoint_shard(&key).unwrap(),
            Some(ShardId::new(4))
        );
    }

    #[test]
    fn ordered_mixed_target_shard_rejects_existing_endpoints_from_multiple_shards() {
        let key = gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture();
        let first = gleaph_graph_kernel::federation::encode_global_vertex_id(
            &key,
            GlobalVertexId::new(ShardId::new(4), 11),
        );
        let second = gleaph_graph_kernel::federation::encode_global_vertex_id(
            &key,
            GlobalVertexId::new(ShardId::new(5), 12),
        );
        let request = OrderedMixedBatchRequest::V1(OrderedMixedBatchRequestV1 {
            client_mutation_key: "mixed-placement-conflict".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![
                AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                    vertex_labels: Vec::new(),
                    initial_properties: Vec::new(),
                }),
                AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                    source: AtomicInsertEndpointV1::Existing(first.0.to_vec()),
                    target: AtomicInsertEndpointV1::Existing(second.0.to_vec()),
                    directed: true,
                    edge_label_name: None,
                    inline_property: None,
                    initial_edge_properties: Vec::new(),
                }),
            ],
        });
        assert!(request.existing_endpoint_shard(&key).is_err());
    }

    #[test]
    fn ordered_mixed_request_converts_to_graph_operation_phases() {
        let request = OrderedMixedBatchRequest::V1(OrderedMixedBatchRequestV1 {
            client_mutation_key: "mixed-convert".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![
                AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                    vertex_labels: Vec::new(),
                    initial_properties: Vec::new(),
                }),
                AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                    source: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                    target: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                    directed: false,
                    edge_label_name: None,
                    inline_property: Some(vec![7, 8]),
                    initial_edge_properties: Vec::new(),
                }),
            ],
        });
        let graph_request = request
            .to_graph_request(
                GraphId::from_raw(11),
                ShardId::new(3),
                Principal::from_slice(&[1]),
                &gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
                Default::default(),
                Default::default(),
            )
            .expect("convert mixed request");
        let gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest::V1(graph_request) =
            graph_request;
        assert_eq!(graph_request.operations.len(), 2);
        assert!(matches!(
            graph_request.operations[0],
            gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Vertex(_)
        ));
        let gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Edge(edge) =
            &graph_request.operations[1]
        else {
            panic!("second operation must remain an edge");
        };
        assert_eq!(
            edge.source,
            gleaph_graph_kernel::plan_exec::OrderedMixedGraphEndpointV1::NewVertexOrdinal(0)
        );
        assert_eq!(edge.inline_property_bytes, vec![7, 8]);
    }

    #[test]
    fn atomic_insert_request_rejects_unknown_vertex_ordinal() {
        let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "mixed-2".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::NewVertexOrdinal(0),
                target: AtomicInsertEndpointV1::Existing(vec![2; 8]),
                directed: true,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: Vec::new(),
            })],
        });
        assert!(request.validate().is_err());
    }

    #[test]
    fn atomic_insert_fingerprint_canonicalizes_property_order_only() {
        let mut request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "ordered-property-order".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(vec![1; 8]),
                target: AtomicInsertEndpointV1::Existing(vec![2; 8]),
                directed: true,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: vec![
                    AtomicInsertPropertyV1 {
                        property_name: "zeta".into(),
                        value: vec![1],
                    },
                    AtomicInsertPropertyV1 {
                        property_name: "alpha".into(),
                        value: vec![2],
                    },
                ],
            })],
        });
        let fingerprint = request.public_fingerprint().expect("fingerprint");
        {
            let AtomicInsertRequest::V1(ref mut request_v1) = request;
            let AtomicInsertOperationV1::Edge(item) = &mut request_v1.operations[0] else {
                panic!("operation must remain an edge");
            };
            item.initial_edge_properties.swap(0, 1);
        }
        assert_eq!(request.public_fingerprint().unwrap(), fingerprint);

        {
            let AtomicInsertRequest::V1(ref mut request_v1) = request;
            let AtomicInsertOperationV1::Edge(item) = &mut request_v1.operations[0] else {
                panic!("operation must remain an edge");
            };
            item.target = AtomicInsertEndpointV1::Existing(vec![3; 8]);
        }
        assert_ne!(request.public_fingerprint().unwrap(), fingerprint);
    }

    #[test]
    fn atomic_insert_request_rejects_empty_property_name() {
        let request = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "ordered-2".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: AtomicInsertEndpointV1::Existing(vec![1; 8]),
                target: AtomicInsertEndpointV1::Existing(vec![2; 8]),
                directed: true,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: vec![AtomicInsertPropertyV1 {
                    property_name: String::new(),
                    value: Vec::new(),
                }],
            })],
        });
        assert!(request.validate().is_err());
    }

    #[test]
    fn atomic_insert_request_enforces_operation_and_property_name_bounds() {
        let operation = AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
            vertex_labels: Vec::new(),
            initial_properties: Vec::new(),
        });
        let empty = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "empty".into(),
            graph_name: Some("tenant.main".into()),
            operations: Vec::new(),
        });
        assert!(empty.validate().is_err());

        let oversized = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "oversized".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![operation; MAX_ATOMIC_INSERT_OPERATIONS + 1],
        });
        assert!(oversized.validate().is_err());

        let duplicate = AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: "duplicate-property".into(),
            graph_name: Some("tenant.main".into()),
            operations: vec![AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                vertex_labels: Vec::new(),
                initial_properties: vec![
                    AtomicInsertPropertyV1 {
                        property_name: "name".into(),
                        value: vec![1],
                    },
                    AtomicInsertPropertyV1 {
                        property_name: "name".into(),
                        value: vec![2],
                    },
                ],
            })],
        });
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn ordered_edge_request_decodes_and_requires_one_shard() {
        use gleaph_graph_kernel::federation::{
            ElementIdEncodingKey, GlobalVertexId, encode_global_vertex_id,
        };

        let key = ElementIdEncodingKey::host_test_fixture();
        let source = encode_global_vertex_id(&key, GlobalVertexId::new(ShardId::new(3), 11));
        let target = encode_global_vertex_id(&key, GlobalVertexId::new(ShardId::new(3), 12));
        let request = OrderedEdgeBatchRequest::V1(OrderedEdgeBatchRequestV1 {
            client_mutation_key: "ordered-3".into(),
            graph_name: Some("tenant.main".into()),
            items: vec![OrderedEdgeInsertRequestItemV1 {
                source: source.0.to_vec(),
                target: target.0.to_vec(),
                directed: false,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: vec![AtomicInsertPropertyV1 {
                    property_name: "weight".into(),
                    value: vec![7, 8],
                }],
            }],
        });
        assert_eq!(
            request.decode_same_shard_endpoints(&key).unwrap(),
            vec![(
                GlobalVertexId::new(ShardId::new(3), 11),
                GlobalVertexId::new(ShardId::new(3), 12),
            )]
        );

        let graph_request = request
            .to_graph_request(
                GraphId::from_raw(7),
                ShardId::new(3),
                Principal::self_authenticating([9; 32]),
                &[(
                    GlobalVertexId::new(ShardId::new(3), 11),
                    GlobalVertexId::new(ShardId::new(3), 12),
                )],
                gleaph_graph_kernel::plan_exec::ResolvedLabelTable::default(),
                gleaph_graph_kernel::plan_exec::ResolvedPropertyTable {
                    properties: vec![gleaph_graph_kernel::plan_exec::ResolvedProperty {
                        name: "weight".into(),
                        id: PropertyId::from_raw(4),
                    }],
                },
            )
            .unwrap();
        let gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(graph_request) =
            graph_request;
        assert_eq!(graph_request.graph_id, GraphId::from_raw(7));
        assert_eq!(graph_request.items[0].source_local_vertex_id, 11);
        assert_eq!(
            graph_request.items[0].resolved_initial_edge_properties[0].property_id,
            PropertyId::from_raw(4)
        );
    }
}

// === ADR 0035 Slice 5: Router outbound accept_envelope send ==================

/// Router-side ingress error enum for the outbound Provision call.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum RouterOutboundError {
    CallFailed(String),
    UnknownDeployment,
    ProvenPreEffectRejection(String),
    Conflict,
    IngressRejected(String),
    EncodingFailed(String),
}

/// Router ingress arguments for `provision_graph`.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ProvisionGraphArgs {
    pub deployment_id: String,
    pub graph_name: String,
    pub requested_resources: Vec<ProvisionableResource>,
    pub authorized_caller: Principal,
    pub release_id: String,
    /// Graph owner (receives `Admin` + owner role). Required so the Router can register the
    /// provisioned graph into its catalog from `created_resources`.
    pub owner: Principal,
    /// Additional graph admins seeded at registration.
    pub admins: BTreeSet<Principal>,
}

/// Router ingress response for `provision_graph`: a mirror of `ProvisionAcceptResponse`.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ProvisionGraphResponse {
    Accepted {
        job_view: gleaph_graph_kernel::provisioning::wire::ProvisionJobSummary,
        intent_lock_count: u32,
        created_resources: Vec<gleaph_graph_kernel::provisioning::wire::CreatedResource>,
    },
    Replay {
        job_view: gleaph_graph_kernel::provisioning::wire::ProvisionJobSummary,
        intent_lock_count: u32,
        created_resources: Vec<gleaph_graph_kernel::provisioning::wire::CreatedResource>,
    },
    /// The request already reached durable `Completed` state on a prior call.
    Completed,
}

#[cfg(test)]
mod outbound_tests {
    use super::*;

    // === ADR 0035 Slice 5 Candid roundtrip tests =================================

    use candid::{Decode, Encode};

    #[test]
    fn test_router_outbound_error_variants_are_candid_representable() {
        for variant in [
            RouterOutboundError::CallFailed("x".to_owned()),
            RouterOutboundError::UnknownDeployment,
            RouterOutboundError::Conflict,
            RouterOutboundError::IngressRejected("x".to_owned()),
            RouterOutboundError::EncodingFailed("x".to_owned()),
        ] {
            let bytes = Encode!(&variant)
                .unwrap_or_else(|e| panic!("encode RouterOutboundError variant {variant:?}: {e}"));
            let decoded: RouterOutboundError = Decode!(&bytes, RouterOutboundError)
                .unwrap_or_else(|e| panic!("decode RouterOutboundError variant {variant:?}: {e}"));
            assert_eq!(decoded, variant);
        }
    }

    #[test]
    fn test_provision_graph_args_roundtrip() {
        let args = ProvisionGraphArgs {
            deployment_id: "deploy-1".to_owned(),
            graph_name: "g".to_owned(),
            requested_resources: vec![],
            authorized_caller: Principal::from_slice(&[0xAB; 29]),
            release_id: "rel-1".to_owned(),
            owner: Principal::from_slice(&[0xAB; 29]),
            admins: std::collections::BTreeSet::new(),
        };
        let bytes = Encode!(&args).expect("encode ProvisionGraphArgs");
        let decoded: ProvisionGraphArgs =
            Decode!(&bytes, ProvisionGraphArgs).expect("decode ProvisionGraphArgs");
        assert_eq!(decoded, args);
    }

    #[test]
    fn test_provision_graph_response_roundtrip() {
        use gleaph_graph_kernel::provisioning::wire::ProvisionJobSummary;
        let summary = ProvisionJobSummary {
            request_id: [1u8; 32],
            deployment_id: "deploy-1".to_owned(),
            state: "Submitted".to_owned(),
            active_resource_index: 0,
            completed_effect_count: 0,
        };
        let response = ProvisionGraphResponse::Accepted {
            job_view: summary.clone(),
            intent_lock_count: 1,
            created_resources: vec![],
        };
        let bytes = Encode!(&response).expect("encode ProvisionGraphResponse");
        let decoded: ProvisionGraphResponse =
            Decode!(&bytes, ProvisionGraphResponse).expect("decode ProvisionGraphResponse");
        assert_eq!(decoded, response);

        let completed_response = ProvisionGraphResponse::Completed;
        let bytes = Encode!(&completed_response).expect("encode ProvisionGraphResponse Completed");
        let decoded: ProvisionGraphResponse = Decode!(&bytes, ProvisionGraphResponse)
            .expect("decode ProvisionGraphResponse Completed");
        assert_eq!(decoded, completed_response);
    }
}
