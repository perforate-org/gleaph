//! Cross-canister GQL execution wire types (router → graph).
//!
//! IC surface rules (enforced by canister `#[query]` / `#[update]` attributes):
//! - **Query** programs use composite query on the router and `execute_*_query` on graph
//!   (read path; may call index / other canisters).
//! - **Update** programs use update on the router and `execute_*_update` on graph (DML and
//!   posting maintenance). A composite query must not invoke an update method.

use candid::{CandidType, Encode, Principal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;

use crate::entry::{
    ConstraintNameId, EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId, GraphId,
    MAX_EDGE_INLINE_PROPERTY_BYTES, PropertyId, VertexLabelId,
};
use crate::federation::{LocalVertexId, ShardId};

/// Router-issued mutation id. `0` is reserved; ids are never reused.
pub type MutationId = u64;

/// Shard-local label stats delta sequence. `0` is reserved; ids are never reused.
pub type ShardEventSeq = u64;

/// Selects the IC call kind for a wired program/plan (must match the canister entrypoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GqlExecutionMode {
    /// Read-only execution (`gql_query` / `execute_plan_query` / composite where needed).
    Query,
    /// Write path (`gql_execute` / `execute_plan_update`).
    Update,
}

/// Router → graph: execute a pre-built physical plan on a target shard.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanArgs {
    pub target_shard_id: ShardId,
    /// Per-graph key for ELEMENT_ID/path id encoding.
    pub element_id_encoding_key: [u8; 16],
    /// Router-issued idempotency key for update/DML execution.
    pub mutation_id: Option<MutationId>,
    pub plan_blob: Vec<u8>,
    pub params_blob: Vec<u8>,
    pub mode: GqlExecutionMode,
    /// When set, graph skips the first anchor `IndexScan` and binds these local vertex ids.
    pub seed_bindings_blob: Option<Vec<u8>>,
    /// Router-resolved label names referenced by the physical plan.
    pub resolved_labels: Option<ResolvedLabelTable>,
    /// Router-resolved property names referenced by the physical plan.
    pub resolved_properties: Option<ResolvedPropertyTable>,
    /// Router-sourced indexed-property catalog for this operation (ADR 0023 D1/D3).
    /// Consulted ephemerally by shard DML to decide which postings to maintain.
    pub indexed_properties: Option<crate::index::IndexedPropertyCatalog>,
    /// Cross-shard uniqueness claims the shard must `Acquire` for the element it creates in this
    /// segment (ADR 0030 slice 5). The Router has already reserved each `(constraint_id,
    /// encoded_value)` via the no-`await` Try; the shard mints `ClaimId(mutation_id, claim_ordinal)`
    /// and pins one `Acquire` receipt per claim so the Router can Confirm it. `None`/empty when the
    /// operation touches no constrained property.
    pub unique_claims: Option<Vec<UniqueClaimDispatch>>,
    /// Constrained `(vertex_label, property)` set the shard consults when this segment can delete or
    /// remove a constrained element, so it can pin one `Release` receipt per freed value (ADR 0030
    /// slice 5b). Like `indexed_properties` this is an ephemeral per-operation slice of the Router's
    /// constraint catalog (no persistent shard-side catalog; ADR 0023). `None`/empty when the
    /// operation cannot release a constrained value.
    pub constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
    /// `ShardLocalGlobal` fast-path claims (ADR 0030 slice 10). Unlike `unique_claims`, these were
    /// **not** reserved through the Router (no Try/Acquire/Confirm). The single owning shard enforces
    /// graph-wide uniqueness entirely in its local unique table: it preflights every claim against
    /// the table and, only if all are clean, inserts them inside the same canonical write segment.
    /// `None`/empty when no constrained property uses the `ShardLocalGlobal` strategy.
    pub local_unique_claims: Option<Vec<UniqueClaimDispatch>>,
    /// Constrained `(vertex_label, property)` set for `ShardLocalGlobal` constraints (ADR 0030 slice
    /// 10). A delete/remove of such an element frees its value directly in the local unique table
    /// (owner-matched), rather than pinning an outbox `Release`. `None`/empty when no constrained
    /// property uses the `ShardLocalGlobal` strategy.
    pub local_constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
    /// Router-sourced indexed-embedding catalog for this operation (ADR 0031 Slice 3). Mirrors
    /// `indexed_properties`: an ephemeral per-operation slice the shard consults to decide which
    /// derived vector-embedding mutations to dispatch. In Slice 3 the Router builder is fail-closed
    /// (always `None`/empty) until delete-spanning incarnation fencing activates dispatch, so
    /// production shards never receive a non-empty catalog and vector sync stays inert.
    pub indexed_embeddings: Option<crate::vector_index::IndexedEmbeddingCatalog>,
    /// Router-resolved non-leading vector search hits for `PlanOp::Search` (ADR 0034 Slice 5).
    /// Per-shard shard-local relation containing the bound vertex id and the user-visible scalar.
    /// `None` for plans without a supported non-leading `SEARCH`.
    pub resolved_search_blob: Option<Vec<u8>>,
}

/// A bounded group of independent plan executions sent in one Router → Graph call.
///
/// Each item retains its own mutation identity and execution payload. The Graph executes items
/// independently; this type is an internal transport aggregation and does not make the group atomic.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanBatchArgs {
    pub operations: Vec<ExecutePlanArgs>,
    pub mode: ExecutePlanBatchMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ExecutePlanBatchMode {
    Fixed,
    Dynamic,
}

/// Per-item outcomes for [`ExecutePlanBatchArgs`].
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanBatchResult {
    pub results: Vec<Result<ExecutePlanResult, String>>,
    /// Index of the first operation not attempted when Dynamic mode hit the Graph budget.
    pub next_index: Option<u32>,
}

/// Canonical compact GQL value bytes carried across an independently encoded boundary.
pub type CanonicalGqlValueBytesV1 = Vec<u8>;

/// Router-resolved initial property assignment for one logical ordered edge.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedOrderedEdgePropertyV1 {
    pub property_id: PropertyId,
    pub value: CanonicalGqlValueBytesV1,
}

/// One logical edge in the immutable Router → Graph ordered request.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedEdgeBatchGraphItemV1 {
    pub source_local_vertex_id: LocalVertexId,
    pub target_local_vertex_id: LocalVertexId,
    pub directed: bool,
    pub catalog_edge_label_id: Option<EdgeLabelId>,
    pub inline_property_bytes: Vec<u8>,
    pub resolved_initial_edge_properties: Vec<ResolvedOrderedEdgePropertyV1>,
}

/// Versioned immutable Router → Graph canonical request for ADR 0049.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedEdgeBatchGraphRequest {
    V1(OrderedEdgeBatchGraphRequestV1),
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedEdgeBatchGraphRequestV1 {
    pub graph_id: GraphId,
    pub target_shard_id: ShardId,
    pub target_graph_canister: Principal,
    pub resolved_labels: ResolvedLabelTable,
    pub resolved_properties: ResolvedPropertyTable,
    pub items: Vec<OrderedEdgeBatchGraphItemV1>,
}

/// One logical vertex in the versioned Router → Graph vertex request.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedVertexBatchGraphItemV1 {
    pub resolved_vertex_labels: Vec<u16>,
    pub resolved_initial_properties: Vec<ResolvedOrderedEdgePropertyV1>,
}

/// Versioned immutable Router → Graph canonical request for vertex bulk placement.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedVertexBatchGraphRequest {
    V1(OrderedVertexBatchGraphRequestV1),
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedVertexBatchGraphRequestV1 {
    pub graph_id: GraphId,
    pub target_shard_id: ShardId,
    pub target_graph_canister: Principal,
    pub resolved_labels: ResolvedLabelTable,
    pub resolved_properties: ResolvedPropertyTable,
    pub items: Vec<OrderedVertexBatchGraphItemV1>,
}

impl OrderedVertexBatchGraphRequest {
    pub fn validate(&self) -> Result<(), String> {
        let OrderedVertexBatchGraphRequest::V1(request) = self;
        if request.items.is_empty() {
            return Err("ordered vertex batch requires at least one item".into());
        }
        if request.target_graph_canister == Principal::anonymous() {
            return Err("ordered vertex batch target graph canister must not be anonymous".into());
        }
        for (ordinal, item) in request.items.iter().enumerate() {
            if item.resolved_vertex_labels.contains(&0) {
                return Err(format!(
                    "ordered vertex item {ordinal} contains a reserved label"
                ));
            }
            let mut property_ids = BTreeSet::new();
            for property in &item.resolved_initial_properties {
                if property.property_id.raw() == 0 {
                    return Err(format!(
                        "ordered vertex item {ordinal} contains reserved property id"
                    ));
                }
                if !property_ids.insert(property.property_id) {
                    return Err(format!(
                        "ordered vertex item {ordinal} repeats property id {}",
                        property.property_id.raw()
                    ));
                }
                if property.value.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
                    return Err(format!(
                        "ordered vertex item {ordinal} property {} exceeds value bound",
                        property.property_id.raw()
                    ));
                }
            }
        }
        let encoded = Encode!(self)
            .map_err(|error| format!("ordered vertex batch request encode failed: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered vertex batch request exceeds the safe payload limit".into());
        }
        Ok(())
    }
}

/// Endpoint of an edge in the request-local mixed vertex/edge operation table.
///
/// `NewVertexOrdinal` is an ordinal among vertex operations, not an ordinal in the
/// mixed operation array. Graph allocates the vertex phase first and resolves this
/// reference before admitting the edge phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedMixedGraphEndpointV1 {
    Existing(LocalVertexId),
    NewVertexOrdinal(u32),
}

/// One logical edge in the Graph-owned mixed request.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedMixedGraphEdgeItemV1 {
    pub source: OrderedMixedGraphEndpointV1,
    pub target: OrderedMixedGraphEndpointV1,
    pub directed: bool,
    pub catalog_edge_label_id: Option<EdgeLabelId>,
    pub inline_property_bytes: Vec<u8>,
    pub resolved_initial_edge_properties: Vec<ResolvedOrderedEdgePropertyV1>,
}

/// Input-order-preserving mixed operation. The array order remains the public
/// logical order; Graph uses the variant to derive the two execution phases.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedMixedGraphOperationV1 {
    Vertex(OrderedVertexBatchGraphItemV1),
    Edge(OrderedMixedGraphEdgeItemV1),
}

/// Versioned Graph envelope for a mixed vertex/edge batch.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedMixedBatchGraphRequest {
    V1(OrderedMixedBatchGraphRequestV1),
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedMixedBatchGraphRequestV1 {
    pub graph_id: GraphId,
    pub target_shard_id: ShardId,
    pub target_graph_canister: Principal,
    pub resolved_labels: ResolvedLabelTable,
    pub resolved_properties: ResolvedPropertyTable,
    pub operations: Vec<OrderedMixedGraphOperationV1>,
}

impl OrderedMixedBatchGraphRequest {
    pub fn validate(&self) -> Result<(), String> {
        let OrderedMixedBatchGraphRequest::V1(request) = self;
        if request.operations.is_empty() {
            return Err("ordered mixed batch requires at least one operation".into());
        }
        if request.target_graph_canister == Principal::anonymous() {
            return Err("ordered mixed batch target graph canister must not be anonymous".into());
        }
        let vertex_count = request
            .operations
            .iter()
            .filter(|operation| matches!(operation, OrderedMixedGraphOperationV1::Vertex(_)))
            .count() as u32;
        if vertex_count == 0 {
            return Err("ordered mixed batch requires at least one vertex operation".into());
        }
        if !request
            .operations
            .iter()
            .any(|operation| matches!(operation, OrderedMixedGraphOperationV1::Edge(_)))
        {
            return Err("ordered mixed batch requires at least one edge operation".into());
        }

        for (ordinal, operation) in request.operations.iter().enumerate() {
            match operation {
                OrderedMixedGraphOperationV1::Vertex(item) => {
                    if item.resolved_vertex_labels.contains(&0) {
                        return Err(format!(
                            "ordered mixed vertex operation {ordinal} contains a reserved label"
                        ));
                    }
                    validate_ordered_initial_properties(
                        ordinal,
                        &item.resolved_initial_properties,
                        "vertex",
                    )?;
                }
                OrderedMixedGraphOperationV1::Edge(item) => {
                    validate_ordered_mixed_endpoint(ordinal, item.source, vertex_count)?;
                    validate_ordered_mixed_endpoint(ordinal, item.target, vertex_count)?;
                    if item.inline_property_bytes.len() > MAX_EDGE_INLINE_PROPERTY_BYTES {
                        return Err(format!(
                            "ordered mixed edge operation {ordinal} inline property bytes exceed bound"
                        ));
                    }
                    validate_ordered_initial_properties(
                        ordinal,
                        &item.resolved_initial_edge_properties,
                        "edge",
                    )?;
                }
            }
        }
        let encoded = Encode!(self)
            .map_err(|error| format!("ordered mixed batch request encode failed: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered mixed batch request exceeds the safe payload limit".into());
        }
        Ok(())
    }

    /// Build the two phase-specific Graph requests after vertex allocation.
    ///
    /// The caller supplies the allocated local ids in request-local vertex ordinal
    /// order. This helper performs no writes and does not expose physical placement;
    /// it only converts new-vertex references into the existing ordered edge request
    /// shape owned by the Graph edge path.
    pub fn plan_phases(
        &self,
        allocated_vertex_ids: &[LocalVertexId],
    ) -> Result<OrderedMixedBatchPhasePlanV1, String> {
        self.validate()?;
        let OrderedMixedBatchGraphRequest::V1(request) = self;
        let expected_vertex_count = request
            .operations
            .iter()
            .filter(|operation| matches!(operation, OrderedMixedGraphOperationV1::Vertex(_)))
            .count();
        if allocated_vertex_ids.len() != expected_vertex_count {
            return Err(format!(
                "ordered mixed phase allocation returned {} vertex ids, expected {}",
                allocated_vertex_ids.len(),
                expected_vertex_count
            ));
        }

        let mut vertices = Vec::with_capacity(expected_vertex_count);
        let mut edges = Vec::new();
        let mut vertex_ordinal = 0usize;
        for operation in &request.operations {
            match operation {
                OrderedMixedGraphOperationV1::Vertex(item) => {
                    vertices.push(item.clone());
                    vertex_ordinal += 1;
                }
                OrderedMixedGraphOperationV1::Edge(item) => {
                    let source = resolve_ordered_mixed_endpoint(item.source, allocated_vertex_ids)?;
                    let target = resolve_ordered_mixed_endpoint(item.target, allocated_vertex_ids)?;
                    edges.push(OrderedEdgeBatchGraphItemV1 {
                        source_local_vertex_id: source,
                        target_local_vertex_id: target,
                        directed: item.directed,
                        catalog_edge_label_id: item.catalog_edge_label_id,
                        inline_property_bytes: item.inline_property_bytes.clone(),
                        resolved_initial_edge_properties: item
                            .resolved_initial_edge_properties
                            .clone(),
                    });
                }
            }
        }
        debug_assert_eq!(vertex_ordinal, allocated_vertex_ids.len());
        Ok(OrderedMixedBatchPhasePlanV1 {
            vertex_request: OrderedVertexBatchGraphRequest::V1(OrderedVertexBatchGraphRequestV1 {
                graph_id: request.graph_id,
                target_shard_id: request.target_shard_id,
                target_graph_canister: request.target_graph_canister,
                resolved_labels: request.resolved_labels.clone(),
                resolved_properties: request.resolved_properties.clone(),
                items: vertices,
            }),
            edge_request: OrderedEdgeBatchGraphRequest::V1(OrderedEdgeBatchGraphRequestV1 {
                graph_id: request.graph_id,
                target_shard_id: request.target_shard_id,
                target_graph_canister: request.target_graph_canister,
                resolved_labels: request.resolved_labels.clone(),
                resolved_properties: request.resolved_properties.clone(),
                items: edges,
            }),
        })
    }
}

/// The immutable phase requests derived from a mixed Graph envelope.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedMixedBatchPhasePlanV1 {
    pub vertex_request: OrderedVertexBatchGraphRequest,
    pub edge_request: OrderedEdgeBatchGraphRequest,
}

fn resolve_ordered_mixed_endpoint(
    endpoint: OrderedMixedGraphEndpointV1,
    allocated_vertex_ids: &[LocalVertexId],
) -> Result<LocalVertexId, String> {
    match endpoint {
        OrderedMixedGraphEndpointV1::Existing(vertex_id) => Ok(vertex_id),
        OrderedMixedGraphEndpointV1::NewVertexOrdinal(ordinal) => allocated_vertex_ids
            .get(ordinal as usize)
            .copied()
            .ok_or_else(|| format!("ordered mixed vertex ordinal {ordinal} has no allocation")),
    }
}

fn validate_ordered_mixed_endpoint(
    operation_ordinal: usize,
    endpoint: OrderedMixedGraphEndpointV1,
    vertex_count: u32,
) -> Result<(), String> {
    if let OrderedMixedGraphEndpointV1::NewVertexOrdinal(vertex_ordinal) = endpoint
        && vertex_ordinal >= vertex_count
    {
        return Err(format!(
            "ordered mixed edge operation {operation_ordinal} references vertex ordinal {vertex_ordinal}, but vertex count is {vertex_count}"
        ));
    }
    Ok(())
}

fn validate_ordered_initial_properties(
    operation_ordinal: usize,
    properties: &[ResolvedOrderedEdgePropertyV1],
    operation_kind: &str,
) -> Result<(), String> {
    let mut property_ids = BTreeSet::new();
    for property in properties {
        if property.property_id.raw() == 0 {
            return Err(format!(
                "ordered mixed {operation_kind} operation {operation_ordinal} contains reserved property id"
            ));
        }
        if !property_ids.insert(property.property_id) {
            return Err(format!(
                "ordered mixed {operation_kind} operation {operation_ordinal} repeats property id {}",
                property.property_id.raw()
            ));
        }
        if property.value.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err(format!(
                "ordered mixed {operation_kind} operation {operation_ordinal} property {} exceeds value bound",
                property.property_id.raw()
            ));
        }
    }
    Ok(())
}

/// Execution mode for an ordered batch Graph request (ADR 0060).
///
/// The mode is a transport property carried on the args envelope, never inside the fingerprinted
/// request, so the same chunk content has the same fingerprint regardless of mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedBatchExecutionModeV1 {
    /// Commit the entire request atomically or fail (atomic-insert contract).
    Atomic,
    /// Commit the largest budget-fitting prefix as one atomic journal entry and return the
    /// committed count (bulk-load chunk execution; the committed prefix is the chunk).
    Resumable,
}

/// Internal execution envelope for the mixed two-phase path. Execution is not
/// active yet; this type exists so Router and Graph can agree on the immutable
/// request before the durable phase/journal implementation lands.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedMixedBatchGraphArgs {
    V1(OrderedMixedBatchGraphArgsV1),
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedMixedBatchGraphArgsV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
    pub execution_mode: OrderedBatchExecutionModeV1,
    /// Router-sourced indexed-property catalog scoped to the vertex properties and edge
    /// `(label_id, property_id)` scopes this batch writes (ADR 0023 D1/D3). Transport metadata
    /// like [`Self::execution_mode`]: carried outside the fingerprinted request, so catalog
    /// changes do not perturb replay identity.
    pub indexed_property_catalog: crate::index::IndexedPropertyCatalog,
    pub request: OrderedMixedBatchGraphRequest,
}

impl OrderedMixedBatchGraphArgs {
    pub fn recompute_and_validate_fingerprint(&self) -> Result<[u8; 32], String> {
        let OrderedMixedBatchGraphArgs::V1(args) = self;
        let fingerprint = ordered_mixed_batch_graph_request_fingerprint(&args.request)?;
        if fingerprint != args.graph_request_fingerprint {
            return Err("ordered mixed Graph request fingerprint mismatch".into());
        }
        Ok(fingerprint)
    }
}

/// Internal Graph execution envelope for the vertex-only bulk-placement slice.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedVertexBatchGraphArgs {
    V1(OrderedVertexBatchGraphArgsV1),
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedVertexBatchGraphArgsV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
    pub execution_mode: OrderedBatchExecutionModeV1,
    /// Router-sourced indexed-property catalog scoped to the properties this batch writes (ADR
    /// 0023 D1/D3). Transport metadata like [`Self::execution_mode`]: carried outside the
    /// fingerprinted request, so catalog changes do not perturb replay identity.
    pub indexed_property_catalog: crate::index::IndexedPropertyCatalog,
    pub request: OrderedVertexBatchGraphRequest,
}

impl OrderedVertexBatchGraphArgs {
    pub fn recompute_and_validate_fingerprint(&self) -> Result<[u8; 32], String> {
        let OrderedVertexBatchGraphArgs::V1(args) = self;
        let fingerprint = ordered_vertex_batch_graph_request_fingerprint(&args.request)?;
        if fingerprint != args.graph_request_fingerprint {
            return Err("ordered vertex Graph request fingerprint mismatch".into());
        }
        Ok(fingerprint)
    }
}

/// Immutable Graph execution envelope. The transmitted fingerprint is integrity metadata; the
/// Graph recomputes it from `request` before journal lookup.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum OrderedEdgeBatchGraphArgs {
    V1(OrderedEdgeBatchGraphArgsV1),
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct OrderedEdgeBatchGraphArgsV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
    pub execution_mode: OrderedBatchExecutionModeV1,
    /// Router-sourced indexed-property catalog scoped to the edge label/property pairs this batch
    /// writes (ADR 0023 D1/D3). Transport metadata like [`Self::execution_mode`]: carried outside
    /// the fingerprinted request, so catalog changes do not perturb replay identity.
    pub indexed_property_catalog: crate::index::IndexedPropertyCatalog,
    pub request: OrderedEdgeBatchGraphRequest,
}

impl OrderedEdgeBatchGraphArgs {
    pub fn recompute_and_validate_fingerprint(&self) -> Result<[u8; 32], String> {
        let OrderedEdgeBatchGraphArgs::V1(args) = self;
        let fingerprint = ordered_edge_batch_graph_request_fingerprint(&args.request)?;
        if fingerprint != args.graph_request_fingerprint {
            return Err("ordered Graph request fingerprint mismatch".into());
        }
        Ok(fingerprint)
    }
}

impl OrderedEdgeBatchGraphRequest {
    pub fn validate(&self) -> Result<(), String> {
        let OrderedEdgeBatchGraphRequest::V1(request) = self;
        if request.items.is_empty() {
            return Err("ordered edge batch requires at least one item".into());
        }
        if request.target_graph_canister == Principal::anonymous() {
            return Err("ordered edge batch target graph canister must not be anonymous".into());
        }
        for (ordinal, item) in request.items.iter().enumerate() {
            if item.inline_property_bytes.len() > MAX_EDGE_INLINE_PROPERTY_BYTES {
                return Err(format!(
                    "ordered edge item {ordinal} inline property bytes exceed bound"
                ));
            }
            let mut property_ids = BTreeSet::new();
            for property in &item.resolved_initial_edge_properties {
                if !property_ids.insert(property.property_id) {
                    return Err(format!(
                        "ordered edge item {ordinal} repeats property id {}",
                        property.property_id.raw()
                    ));
                }
            }
        }
        let encoded = Encode!(self)
            .map_err(|error| format!("ordered edge batch request encode failed: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered edge batch request exceeds the safe payload limit".into());
        }
        Ok(())
    }
}

pub const ORDERED_EDGE_GRAPH_FINGERPRINT_DOMAIN: &[u8] = b"gleaph:ordered-edge-graph:v1\0";
pub const ORDERED_VERTEX_GRAPH_FINGERPRINT_DOMAIN: &[u8] = b"gleaph:ordered-vertex-graph:v1\0";
pub const ORDERED_MIXED_GRAPH_FINGERPRINT_DOMAIN: &[u8] = b"gleaph:ordered-mixed-graph:v1\0";

fn encode_len_prefixed_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn encode_string(out: &mut Vec<u8>, value: &str) {
    encode_len_prefixed_bytes(out, value.as_bytes());
}

fn encode_inline_encoding(out: &mut Vec<u8>, encoding: &EdgeInlinePropertyEncoding) {
    let tag = match encoding {
        EdgeInlinePropertyEncoding::RawU8 => 0,
        EdgeInlinePropertyEncoding::RawU16 => 1,
        EdgeInlinePropertyEncoding::RawU32 => 2,
        EdgeInlinePropertyEncoding::RawU64 => 3,
        EdgeInlinePropertyEncoding::RawI8 => 4,
        EdgeInlinePropertyEncoding::RawI16 => 5,
        EdgeInlinePropertyEncoding::RawI32 => 6,
        EdgeInlinePropertyEncoding::RawI64 => 7,
        EdgeInlinePropertyEncoding::F16 => 8,
        EdgeInlinePropertyEncoding::F32 => 9,
        EdgeInlinePropertyEncoding::F64 => 10,
        EdgeInlinePropertyEncoding::RawU128 => 11,
        EdgeInlinePropertyEncoding::RawI128 => 12,
        EdgeInlinePropertyEncoding::RawFixed32 => 13,
        EdgeInlinePropertyEncoding::RawFixed64 => 14,
        EdgeInlinePropertyEncoding::VectorF32 { .. } => 15,
        EdgeInlinePropertyEncoding::RawBytes => 16,
    };
    out.push(tag);
    if let EdgeInlinePropertyEncoding::VectorF32 { dims } = encoding {
        out.extend_from_slice(&dims.to_le_bytes());
    }
}

fn encode_inline_profile(out: &mut Vec<u8>, profile: &EdgeInlinePropertyProfile) {
    out.extend_from_slice(&profile.byte_width.to_le_bytes());
    encode_inline_encoding(out, &profile.encoding);
}

fn encode_resolved_labels(out: &mut Vec<u8>, labels: &ResolvedLabelTable) {
    out.extend_from_slice(&(labels.vertex.len() as u32).to_le_bytes());
    for label in &labels.vertex {
        encode_string(out, &label.name);
        out.extend_from_slice(&label.id.raw().to_le_bytes());
    }
    out.extend_from_slice(&(labels.edge.len() as u32).to_le_bytes());
    for label in &labels.edge {
        encode_string(out, &label.name);
        out.extend_from_slice(&label.id.raw().to_le_bytes());
        encode_inline_profile(out, &label.inline_property_profile);
        match &label.inline_schema {
            None => out.push(0),
            Some(ResolvedInlineSchema::Scalar { property_id }) => {
                out.push(1);
                out.extend_from_slice(&property_id.raw().to_le_bytes());
            }
            Some(ResolvedInlineSchema::Struct {
                property_id,
                fields,
            }) => {
                out.push(2);
                out.extend_from_slice(&property_id.raw().to_le_bytes());
                out.extend_from_slice(&(fields.len() as u32).to_le_bytes());
                for field in fields {
                    encode_string(out, &field.name);
                    out.extend_from_slice(&field.byte_offset.to_le_bytes());
                    encode_inline_profile(out, &field.profile);
                }
            }
        }
    }
}

fn encode_resolved_properties(out: &mut Vec<u8>, properties: &ResolvedPropertyTable) {
    out.extend_from_slice(&(properties.properties.len() as u32).to_le_bytes());
    for property in &properties.properties {
        encode_string(out, &property.name);
        out.extend_from_slice(&property.id.raw().to_le_bytes());
    }
}

/// Encode the exact Graph request without its derived fingerprint or mutation id.
pub fn encode_ordered_edge_batch_graph_request(
    request: &OrderedEdgeBatchGraphRequest,
) -> Result<Vec<u8>, String> {
    request.validate()?;
    let OrderedEdgeBatchGraphRequest::V1(request) = request;
    let mut out = Vec::new();
    out.push(1); // outer Graph request V1 envelope
    out.extend_from_slice(&request.graph_id.raw().to_le_bytes());
    out.extend_from_slice(&request.target_shard_id.raw().to_le_bytes());
    encode_len_prefixed_bytes(&mut out, request.target_graph_canister.as_slice());
    encode_resolved_labels(&mut out, &request.resolved_labels);
    encode_resolved_properties(&mut out, &request.resolved_properties);
    out.extend_from_slice(&(request.items.len() as u32).to_le_bytes());
    for item in &request.items {
        out.extend_from_slice(&item.source_local_vertex_id.to_le_bytes());
        out.extend_from_slice(&item.target_local_vertex_id.to_le_bytes());
        out.push(u8::from(item.directed));
        match item.catalog_edge_label_id {
            None => out.push(0),
            Some(label_id) => {
                out.push(1);
                out.extend_from_slice(&label_id.raw().to_le_bytes());
            }
        }
        encode_len_prefixed_bytes(&mut out, &item.inline_property_bytes);
        out.extend_from_slice(&(item.resolved_initial_edge_properties.len() as u32).to_le_bytes());
        for property in &item.resolved_initial_edge_properties {
            out.extend_from_slice(&property.property_id.raw().to_le_bytes());
            encode_len_prefixed_bytes(&mut out, &property.value);
        }
    }
    Ok(out)
}

/// Compute the order-sensitive Graph request fingerprint from the immutable envelope.
pub fn ordered_edge_batch_graph_request_fingerprint(
    request: &OrderedEdgeBatchGraphRequest,
) -> Result<[u8; 32], String> {
    let bytes = encode_ordered_edge_batch_graph_request(request)?;
    let mut hasher = Sha256::new();
    hasher.update(ORDERED_EDGE_GRAPH_FINGERPRINT_DOMAIN);
    hasher.update(bytes);
    Ok(hasher.finalize().into())
}

/// Encode the exact vertex Graph request without its derived fingerprint or mutation id.
pub fn encode_ordered_vertex_batch_graph_request(
    request: &OrderedVertexBatchGraphRequest,
) -> Result<Vec<u8>, String> {
    request.validate()?;
    let OrderedVertexBatchGraphRequest::V1(request) = request;
    let mut out = Vec::new();
    out.push(1);
    out.extend_from_slice(&request.graph_id.raw().to_le_bytes());
    out.extend_from_slice(&request.target_shard_id.raw().to_le_bytes());
    encode_len_prefixed_bytes(&mut out, request.target_graph_canister.as_slice());
    encode_resolved_labels(&mut out, &request.resolved_labels);
    encode_resolved_properties(&mut out, &request.resolved_properties);
    out.extend_from_slice(&(request.items.len() as u32).to_le_bytes());
    for item in &request.items {
        out.extend_from_slice(&(item.resolved_vertex_labels.len() as u32).to_le_bytes());
        for label in &item.resolved_vertex_labels {
            out.extend_from_slice(&label.to_le_bytes());
        }
        out.extend_from_slice(&(item.resolved_initial_properties.len() as u32).to_le_bytes());
        for property in &item.resolved_initial_properties {
            out.extend_from_slice(&property.property_id.raw().to_le_bytes());
            encode_len_prefixed_bytes(&mut out, &property.value);
        }
    }
    Ok(out)
}

pub fn ordered_vertex_batch_graph_request_fingerprint(
    request: &OrderedVertexBatchGraphRequest,
) -> Result<[u8; 32], String> {
    let bytes = encode_ordered_vertex_batch_graph_request(request)?;
    let mut hasher = Sha256::new();
    hasher.update(ORDERED_VERTEX_GRAPH_FINGERPRINT_DOMAIN);
    hasher.update(bytes);
    Ok(hasher.finalize().into())
}

/// Encode the exact mixed Graph request without its derived fingerprint or mutation id.
pub fn encode_ordered_mixed_batch_graph_request(
    request: &OrderedMixedBatchGraphRequest,
) -> Result<Vec<u8>, String> {
    request.validate()?;
    let OrderedMixedBatchGraphRequest::V1(request) = request;
    let mut out = Vec::new();
    out.push(1);
    out.extend_from_slice(&request.graph_id.raw().to_le_bytes());
    out.extend_from_slice(&request.target_shard_id.raw().to_le_bytes());
    encode_len_prefixed_bytes(&mut out, request.target_graph_canister.as_slice());
    encode_resolved_labels(&mut out, &request.resolved_labels);
    encode_resolved_properties(&mut out, &request.resolved_properties);
    out.extend_from_slice(&(request.operations.len() as u32).to_le_bytes());
    for operation in &request.operations {
        match operation {
            OrderedMixedGraphOperationV1::Vertex(item) => {
                out.push(0);
                out.extend_from_slice(&(item.resolved_vertex_labels.len() as u32).to_le_bytes());
                for label in &item.resolved_vertex_labels {
                    out.extend_from_slice(&label.to_le_bytes());
                }
                encode_ordered_initial_properties(&mut out, &item.resolved_initial_properties);
            }
            OrderedMixedGraphOperationV1::Edge(item) => {
                out.push(1);
                encode_ordered_mixed_endpoint(&mut out, item.source);
                encode_ordered_mixed_endpoint(&mut out, item.target);
                out.push(u8::from(item.directed));
                match item.catalog_edge_label_id {
                    None => out.push(0),
                    Some(label_id) => {
                        out.push(1);
                        out.extend_from_slice(&label_id.raw().to_le_bytes());
                    }
                }
                encode_len_prefixed_bytes(&mut out, &item.inline_property_bytes);
                encode_ordered_initial_properties(&mut out, &item.resolved_initial_edge_properties);
            }
        }
    }
    Ok(out)
}

fn encode_ordered_initial_properties(
    out: &mut Vec<u8>,
    properties: &[ResolvedOrderedEdgePropertyV1],
) {
    out.extend_from_slice(&(properties.len() as u32).to_le_bytes());
    for property in properties {
        out.extend_from_slice(&property.property_id.raw().to_le_bytes());
        encode_len_prefixed_bytes(out, &property.value);
    }
}

fn encode_ordered_mixed_endpoint(out: &mut Vec<u8>, endpoint: OrderedMixedGraphEndpointV1) {
    match endpoint {
        OrderedMixedGraphEndpointV1::Existing(vertex_id) => {
            out.push(0);
            out.extend_from_slice(&vertex_id.to_le_bytes());
        }
        OrderedMixedGraphEndpointV1::NewVertexOrdinal(ordinal) => {
            out.push(1);
            out.extend_from_slice(&ordinal.to_le_bytes());
        }
    }
}

pub fn ordered_mixed_batch_graph_request_fingerprint(
    request: &OrderedMixedBatchGraphRequest,
) -> Result<[u8; 32], String> {
    let bytes = encode_ordered_mixed_batch_graph_request(request)?;
    let mut hasher = Sha256::new();
    hasher.update(ORDERED_MIXED_GRAPH_FINGERPRINT_DOMAIN);
    hasher.update(bytes);
    Ok(hasher.finalize().into())
}

/// One cross-shard uniqueness claim dispatched to the shard for `Acquire` (ADR 0030 slice 5).
///
/// `claim_ordinal` is the claim's deterministic position within the mutation; combined with the
/// envelope's `mutation_id` it yields the immutable `ClaimId` the Router reserved. `encoded_value`
/// is the canonical key the Router already validated and reserved, carried verbatim so the shard's
/// pinned receipt and the Router's reservation reference identical bytes.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct UniqueClaimDispatch {
    pub claim_ordinal: u32,
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
}

/// One constrained `(vertex_label, property)` dispatched to the shard so a delete/remove can pin a
/// `Release` for the freed value (ADR 0030 slice 5b).
///
/// The ids are Router-interned and match the shard's stored vertex labels/property ids verbatim
/// (the Router is the sole interner; it ships `ResolvedLabelTable`/`ResolvedPropertyTable` and the
/// shard persists those same ids), so the shard matches a deleted vertex's labels/properties with
/// no translation. `constraint_id` is the reservation-key constraint the freed value belongs to.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ConstrainedPropertyDispatch {
    pub vertex_label_id: VertexLabelId,
    pub property_id: PropertyId,
    pub constraint_id: ConstraintNameId,
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanResult {
    pub row_count: u64,
    /// Candid-encoded [`gleaph_gql_ic::IcWirePlanQueryResult`]; set on query shard execution.
    pub rows_blob: Option<Vec<u8>>,
    /// Forward out-adjacency hubs from a DML batch (router P3 auto-finalize hint).
    pub hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
}

/// Federated mutation lifecycle phase (ADR 0029).
///
/// Router owns the transitions; this is the wire projection a client receives for an
/// idempotent mutation. It is deliberately distinct from [`MutationJournalState`], which
/// only attests a *shard-local* replayable outcome and never describes cross-canister
/// projection convergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MutationLifecyclePhase {
    /// Router is resolving and durably recording the immutable dispatch envelope.
    Routing,
    /// At least one required canonical shard outcome is not yet known.
    CanonicalPending,
    /// All required canonical shard writes are durable; no projection has advanced yet.
    CanonicalCommitted,
    /// Canonical writes are durable; one or more required derived projections still lag.
    ProjectionPending,
    /// Canonical writes and every projection required by the mutation contract converged.
    Completed,
    /// Validation or execution failed before any canonical write committed.
    Failed,
}

/// Read-your-writes token for a federated mutation (ADR 0029 §5, Phase 2).
///
/// Issued with an idempotent DML result. It names the mutation and the per-shard
/// projection watermarks a later read must reach to observe this mutation's effects.
/// It is deliberately **not** a global snapshot timestamp: graph-index freshness is
/// keyed by the monotonic `mutation_id` (a shard's index work for `mutation_id` is
/// applied once its repair watermark passes it), and label-stats freshness by each
/// shard's delta [`ShardEventSeq`]. Phase 2 *issues* the token; Phase 3 enforces it via
/// [`ReadMode::AtLeast`].
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MutationToken {
    pub mutation_id: MutationId,
    pub shards: Vec<MutationTokenShard>,
}

/// Read freshness contract a caller selects per read (ADR 0029 §5, Phase 3).
///
/// This lives at the Gleaph integration boundary, not in the generic GQL crates:
/// it is keyed by Gleaph-specific projection watermarks (`MutationToken`).
///
/// - [`ReadMode::Eventual`] is non-blocking and may observe documented projection lag
///   (count-only under-count, posting lag). It is the default and matches the
///   historical `gql_query` behavior.
/// - [`ReadMode::AtLeast`] enforces a read barrier: the read is served only once every
///   shard in the token has caught its label-stats and graph-index watermarks; otherwise
///   the router returns a retryable projection-lag error without serving stale state.
#[derive(Clone, Debug, PartialEq, Eq, Default, CandidType, Serialize, Deserialize)]
pub enum ReadMode {
    /// Non-blocking; may observe documented projection lag.
    #[default]
    Eventual,
    /// Block (retryable) until every shard reaches the token's watermarks.
    AtLeast(MutationToken),
}

/// Per-shard watermarks a read must reach for read-your-writes against one mutation.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MutationTokenShard {
    pub shard_id: ShardId,
    /// Highest label-stats delta seq this mutation emitted on the shard. The Router
    /// label-stats projection must reach this seq to satisfy a count-only
    /// read-your-writes. `None` when the mutation emitted no label-stats delta here.
    pub label_stats_seq: Option<ShardEventSeq>,
}

/// Router read-path result: merged row count and optional materialized rows.
///
/// `phase` is populated only for idempotent mutations, where Router tracks a federated
/// saga; it is `None` for read queries and for non-idempotent escape-hatch writes that
/// carry no tracked mutation record (ADR 0029).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GqlQueryResult {
    pub row_count: u64,
    /// Candid-encoded [`gleaph_gql_ic::IcWirePlanQueryResult`] after federated merge.
    pub rows_blob: Option<Vec<u8>>,
    /// Federated mutation lifecycle phase for idempotent mutations (ADR 0029).
    pub phase: Option<MutationLifecyclePhase>,
    /// Read-your-writes token for idempotent mutations (ADR 0029 §5, Phase 2). `None`
    /// for reads and untracked escape-hatch writes.
    pub token: Option<MutationToken>,
}

impl GqlQueryResult {
    pub fn from_merged(merged: &ExecutePlanResult) -> Self {
        Self {
            row_count: merged.row_count,
            rows_blob: merged.rows_blob.clone(),
            phase: None,
            token: None,
        }
    }

    pub fn row_count_only(row_count: u64) -> Self {
        Self {
            row_count,
            rows_blob: None,
            phase: None,
            token: None,
        }
    }

    /// Attach a federated mutation lifecycle phase (ADR 0029).
    #[must_use]
    pub fn with_phase(mut self, phase: MutationLifecyclePhase) -> Self {
        self.phase = Some(phase);
        self
    }

    /// Attach a read-your-writes mutation token (ADR 0029 §5, Phase 2).
    #[must_use]
    pub fn with_token(mut self, token: MutationToken) -> Self {
        self.token = Some(token);
        self
    }
}

/// Ordered label stats delta appended by graph shard DML (ADR 0015).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LabelStatsDeltaEventWire {
    pub mutation_id: MutationId,
    pub shard_event_seq: ShardEventSeq,
    pub label_stats_delta: LabelStatsDelta,
}

/// Per-label live count changes emitted by graph shard DML (ADR 0015).
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LabelStatsDelta {
    pub vertex: Vec<(VertexLabelId, i64)>,
    pub edge: Vec<(EdgeLabelId, i64)>,
}

/// Graph-local mutation journal state (ADR 0015).
///
/// This is a *shard-local* idempotency outcome, not a cross-canister status. `Completed`
/// here means the shard-local canonical mutation outcome is durable and replayable; it
/// does **not** imply that derived projections (graph-index postings, Router label stats)
/// have converged. Cross-canister convergence is tracked separately by Router's
/// [`MutationLifecyclePhase`] (ADR 0029).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MutationJournalState {
    Incomplete,
    Completed,
}

/// Graph-owned request identity for journal-first replay.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphMutationRequestIdentityV1 {
    /// Existing scalar and legacy/bulk plan execution identity.
    PlanExecution,
    /// Order-sensitive identity for the ADR 0049 edge batch envelope.
    OrderedEdgeBatch {
        canonical_encoding_version: u16,
        graph_request_fingerprint: [u8; 32],
        logical_item_count: u32,
    },
    /// Order-sensitive identity for the ADR 0049 vertex bulk-placement envelope.
    OrderedVertexBatch {
        canonical_encoding_version: u16,
        graph_request_fingerprint: [u8; 32],
        logical_item_count: u32,
    },
    /// Order-sensitive identity for the mixed vertex/edge two-phase envelope.
    OrderedMixedBatch {
        canonical_encoding_version: u16,
        graph_request_fingerprint: [u8; 32],
        logical_operation_count: u32,
        logical_vertex_count: u32,
        logical_edge_count: u32,
    },
}

/// Stable Graph-owned retirement state for an ordered batch journal entry.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphMutationRetirementV1 {
    NotApplicable,
    Active,
    Retired { at_ns: u64 },
}

/// Wire projection of [`GraphMutationRetirementV1`] without the stable timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphMutationRetirementWireV1 {
    NotApplicable,
    Active,
    Retired,
}

/// Validate the request-kind-specific combinations allowed in the Graph journal.
///
/// PlanExecution retains the scalar state machine. Ordered mutation families are completed
/// requests and therefore cannot carry partial-progress state.
pub fn validate_graph_mutation_journal_fields(
    identity: &GraphMutationRequestIdentityV1,
    retirement: GraphMutationRetirementWireV1,
    state: MutationJournalState,
    row_count: u64,
    next_index: Option<u32>,
    bulk_progress: &Option<GraphBulkMutationProgress>,
) -> Result<(), &'static str> {
    match identity {
        GraphMutationRequestIdentityV1::PlanExecution => {
            if retirement != GraphMutationRetirementWireV1::NotApplicable {
                return Err("PlanExecution journal entry must not have retirement state");
            }
        }
        GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            logical_item_count, ..
        } => {
            if retirement == GraphMutationRetirementWireV1::NotApplicable {
                return Err("ordered journal entry must have retirement state");
            }
            if state != MutationJournalState::Completed {
                return Err("ordered journal entry must be completed");
            }
            if next_index.is_some() {
                return Err("ordered journal entry must not have next_index");
            }
            if bulk_progress.is_some() {
                return Err("ordered journal entry must not have bulk progress");
            }
            if row_count != u64::from(*logical_item_count) {
                return Err("ordered journal row_count must equal logical_item_count");
            }
        }
        GraphMutationRequestIdentityV1::OrderedVertexBatch {
            logical_item_count, ..
        } => {
            if retirement == GraphMutationRetirementWireV1::NotApplicable {
                return Err("ordered vertex journal entry must have retirement state");
            }
            if state != MutationJournalState::Completed {
                return Err("ordered vertex journal entry must be completed");
            }
            if next_index.is_some() {
                return Err("ordered vertex journal entry must not have next_index");
            }
            if bulk_progress.is_some() {
                return Err("ordered vertex journal entry must not have bulk progress");
            }
            if row_count != u64::from(*logical_item_count) {
                return Err("ordered vertex journal row_count must equal logical_item_count");
            }
        }
        GraphMutationRequestIdentityV1::OrderedMixedBatch {
            logical_operation_count,
            logical_vertex_count,
            logical_edge_count,
            ..
        } => {
            if retirement == GraphMutationRetirementWireV1::NotApplicable {
                return Err("ordered mixed journal entry must have retirement state");
            }
            if state != MutationJournalState::Completed {
                return Err("ordered mixed journal entry must be completed");
            }
            if next_index.is_some() {
                return Err("ordered mixed journal entry must not have next_index");
            }
            if bulk_progress.is_some() {
                return Err("ordered mixed journal entry must not have bulk progress");
            }
            let expected_count = u64::from(*logical_vertex_count)
                .checked_add(u64::from(*logical_edge_count))
                .ok_or("ordered mixed journal logical count overflow")?;
            if expected_count != u64::from(*logical_operation_count) {
                return Err("ordered mixed journal counts must sum to logical_operation_count");
            }
            if row_count != u64::from(*logical_operation_count) {
                return Err("ordered mixed journal row_count must equal logical_operation_count");
            }
        }
    }
    Ok(())
}

/// Validate the ordered-insert allocated-ID appendix against the journal request family.
///
/// Vertex and mixed ordered mutations must persist the exact request-local vertex allocation
/// sequence. Edge-only and legacy plan mutations must not carry this appendix; treating an
/// absent vertex receipt as an empty successful result would make old v1 bytes replayable with a
/// different contract.
pub fn validate_allocated_vertex_ids_appendix(
    identity: &GraphMutationRequestIdentityV1,
    allocated_vertex_ids: Option<&[LocalVertexId]>,
) -> Result<(), &'static str> {
    validate_allocated_vertex_id_count(identity, allocated_vertex_ids.map(<[LocalVertexId]>::len))
}

/// Validate the allocated-ID appendix cardinality without requiring the encoded IDs themselves.
///
/// Graph uses this form during journal-record admission, before canonical vertex allocation can
/// produce the actual local IDs. The cardinality rule remains owned by the Graph journal identity
/// contract so admission and decode cannot drift.
pub fn validate_allocated_vertex_id_count(
    identity: &GraphMutationRequestIdentityV1,
    allocated_vertex_count: Option<usize>,
) -> Result<(), &'static str> {
    match identity {
        GraphMutationRequestIdentityV1::PlanExecution => {
            if allocated_vertex_count.is_some() {
                return Err("PlanExecution journal entry must not have allocated vertex ids");
            }
        }
        GraphMutationRequestIdentityV1::OrderedEdgeBatch { .. } => {
            if allocated_vertex_count.is_some() {
                return Err("ordered edge journal entry must not have allocated vertex ids");
            }
        }
        GraphMutationRequestIdentityV1::OrderedVertexBatch {
            logical_item_count, ..
        } => {
            let Some(count) = allocated_vertex_count else {
                return Err("ordered vertex journal entry requires allocated vertex ids");
            };
            if count != *logical_item_count as usize {
                return Err(
                    "ordered vertex allocated vertex id count must equal logical item count",
                );
            }
        }
        GraphMutationRequestIdentityV1::OrderedMixedBatch {
            logical_vertex_count,
            ..
        } => {
            let Some(count) = allocated_vertex_count else {
                return Err("ordered mixed journal entry requires allocated vertex ids");
            };
            if count != *logical_vertex_count as usize {
                return Err(
                    "ordered mixed allocated vertex id count must equal logical vertex count",
                );
            }
        }
    }
    Ok(())
}

/// Durable aggregate receipt returned by a completed ordered Graph edge batch.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphOrderedEdgeBatchReceiptV1 {
    pub logical_edge_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    pub hot_forward_vertices: Vec<LocalVertexId>,
}

/// Durable aggregate receipt returned by a completed ordered Graph vertex batch.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphOrderedVertexBatchReceiptV1 {
    pub logical_vertex_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    pub hot_forward_vertices: Vec<LocalVertexId>,
    /// Vertex IDs in request-local vertex-operation ordinal order.
    pub allocated_vertex_ids: Vec<LocalVertexId>,
}

/// Durable aggregate receipt returned by a completed mixed Graph batch.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphOrderedMixedBatchReceiptV1 {
    pub logical_operation_count: u64,
    pub logical_vertex_count: u64,
    pub logical_edge_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    pub hot_forward_vertices: Vec<LocalVertexId>,
    /// Vertex IDs in request-local vertex-operation ordinal order.
    pub allocated_vertex_ids: Vec<LocalVertexId>,
}

impl GraphOrderedMixedBatchReceiptV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        let Some(operation_count) = self
            .logical_vertex_count
            .checked_add(self.logical_edge_count)
        else {
            return Err("ordered mixed receipt logical count overflow");
        };
        if operation_count != self.logical_operation_count {
            return Err("ordered mixed receipt counts must sum to logical_operation_count");
        }
        if self
            .hot_forward_vertices
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err("ordered mixed receipt hot-forward vertices must be sorted and unique");
        }
        let expected_vertex_count = usize::try_from(self.logical_vertex_count)
            .map_err(|_| "ordered mixed receipt logical vertex count overflows usize")?;
        if self.allocated_vertex_ids.len() != expected_vertex_count {
            return Err(
                "ordered mixed receipt allocated vertex id count must equal logical vertex count",
            );
        }
        let encoded = Encode!(self).map_err(|_| "ordered mixed receipt encode failed")?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered mixed receipt exceeds the safe payload limit");
        }
        Ok(())
    }
}

impl GraphOrderedVertexBatchReceiptV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .hot_forward_vertices
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err("ordered vertex receipt hot-forward vertices must be sorted and unique");
        }
        let expected_vertex_count = usize::try_from(self.logical_vertex_count)
            .map_err(|_| "ordered vertex receipt logical vertex count overflows usize")?;
        if self.allocated_vertex_ids.len() != expected_vertex_count {
            return Err(
                "ordered vertex receipt allocated vertex id count must equal logical vertex count",
            );
        }
        let encoded = Encode!(self).map_err(|_| "ordered vertex receipt encode failed")?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered vertex receipt exceeds the safe payload limit");
        }
        Ok(())
    }
}

impl GraphOrderedEdgeBatchReceiptV1 {
    /// Validate the canonical hot-vertex projection retained by the receipt.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .hot_forward_vertices
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err("ordered receipt hot-forward vertices must be sorted and unique");
        }
        let encoded = Encode!(self).map_err(|_| "ordered edge receipt encode failed")?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("ordered edge receipt exceeds the safe payload limit");
        }
        Ok(())
    }
}

/// Graph-owned canonical response for the ordered edge-batch endpoint.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphOrderedEdgeBatchResult {
    V1(GraphOrderedEdgeBatchResultV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphOrderedEdgeBatchResultV1 {
    Completed(GraphOrderedEdgeBatchReceiptV1),
    MutationRetired {
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphOrderedVertexBatchResult {
    V1(GraphOrderedVertexBatchResultV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphOrderedVertexBatchResultV1 {
    Completed(GraphOrderedVertexBatchReceiptV1),
    MutationRetired {
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphOrderedMixedBatchResult {
    V1(GraphOrderedMixedBatchResultV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphOrderedMixedBatchResultV1 {
    Completed(GraphOrderedMixedBatchReceiptV1),
    MutationRetired {
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    },
}

impl GraphOrderedVertexBatchResult {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(GraphOrderedVertexBatchResultV1::Completed(receipt)) => receipt.validate(),
            Self::V1(GraphOrderedVertexBatchResultV1::MutationRetired { .. }) => Ok(()),
        }
    }
}

impl GraphOrderedEdgeBatchResult {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(GraphOrderedEdgeBatchResultV1::Completed(receipt)) => receipt.validate(),
            Self::V1(GraphOrderedEdgeBatchResultV1::MutationRetired { .. }) => Ok(()),
        }
    }
}

impl GraphOrderedMixedBatchResult {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(GraphOrderedMixedBatchResultV1::Completed(receipt)) => receipt.validate(),
            Self::V1(GraphOrderedMixedBatchResultV1::MutationRetired { .. }) => Ok(()),
        }
    }
}

/// Graph-owned acknowledgement for the internal Router-to-Graph retirement call.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedMutationRetirementAck {
    V1(OrderedMutationRetirementAckV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct OrderedMutationRetirementAckV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
    pub receipt: GraphOrderedEdgeBatchReceiptV1,
}

impl OrderedMutationRetirementAck {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(ack) => ack.receipt.validate(),
        }
    }
}

/// Graph-owned acknowledgement for vertex-only ordered mutation retirement.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedVertexMutationRetirementAck {
    V1(OrderedVertexMutationRetirementAckV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct OrderedVertexMutationRetirementAckV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
    pub receipt: GraphOrderedVertexBatchReceiptV1,
}

/// Graph-owned acknowledgement for mixed ordered mutation retirement.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedMixedMutationRetirementAck {
    V1(OrderedMixedMutationRetirementAckV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct OrderedMixedMutationRetirementAckV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
    pub receipt: GraphOrderedMixedBatchReceiptV1,
}

impl OrderedMixedMutationRetirementAck {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(ack) => ack.receipt.validate(),
        }
    }
}

impl OrderedVertexMutationRetirementAck {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(ack) => ack.receipt.validate(),
        }
    }
}

/// Arguments for the internal Router-to-Graph ordered retirement call.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedMutationRetirementArgs {
    V1(OrderedMutationRetirementArgsV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct OrderedMutationRetirementArgsV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
}

/// Arguments for vertex-only ordered mutation retirement.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum OrderedVertexMutationRetirementArgs {
    V1(OrderedVertexMutationRetirementArgsV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct OrderedVertexMutationRetirementArgsV1 {
    pub mutation_id: MutationId,
    pub graph_request_fingerprint: [u8; 32],
}

/// Versioned graph shard mutation idempotency journal entry (ADR 0015, ADR 0027, ADR 0057).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphMutationJournalEntryWire {
    V1(GraphMutationJournalEntryWireV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphMutationJournalEntryWireV1 {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub row_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    /// Forward hubs observed during DML, persisted so router recovery can still finalize.
    pub hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
    /// Ordered local vertex allocations. `Some` is mandatory for ordered vertex/mixed entries;
    /// edge-only and plan-execution entries must keep this `None` so the appendix remains absent.
    pub allocated_vertex_ids: Option<Vec<crate::federation::LocalVertexId>>,
    /// Internal plan-batch cursor: the first unexecuted operation.
    #[serde(default)]
    pub next_index: Option<u32>,
    /// Internal plan-batch progress metadata.
    #[serde(default)]
    pub bulk_progress: Option<GraphBulkMutationProgress>,
    #[serde(default = "default_graph_mutation_request_identity")]
    pub request_identity: GraphMutationRequestIdentityV1,
    #[serde(default = "default_graph_mutation_retirement_wire")]
    pub retirement: GraphMutationRetirementWireV1,
}

fn default_graph_mutation_request_identity() -> GraphMutationRequestIdentityV1 {
    GraphMutationRequestIdentityV1::PlanExecution
}

fn default_graph_mutation_retirement_wire() -> GraphMutationRetirementWireV1 {
    GraphMutationRetirementWireV1::NotApplicable
}

/// Versioned internal plan-batch progress stored in a Graph journal entry.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphBulkMutationProgress {
    V1(GraphBulkMutationProgressV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphBulkMutationProgressV1 {
    pub operation_count: u32,
    pub completed_count: u32,
    #[serde(default)]
    pub operation_row_counts: Vec<u64>,
}

impl GraphMutationJournalEntryWire {
    pub fn new(
        mutation_id: MutationId,
        state: MutationJournalState,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
    ) -> Self {
        Self::V1(GraphMutationJournalEntryWireV1 {
            mutation_id,
            state,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            allocated_vertex_ids: None,
            next_index: None,
            bulk_progress: None,
            request_identity: GraphMutationRequestIdentityV1::PlanExecution,
            retirement: GraphMutationRetirementWireV1::NotApplicable,
        })
    }

    fn as_v1(&self) -> &GraphMutationJournalEntryWireV1 {
        match self {
            GraphMutationJournalEntryWire::V1(v1) => v1,
        }
    }

    fn as_v1_mut(&mut self) -> &mut GraphMutationJournalEntryWireV1 {
        match self {
            GraphMutationJournalEntryWire::V1(v1) => v1,
        }
    }

    pub fn mutation_id(&self) -> MutationId {
        self.as_v1().mutation_id
    }
    pub fn state(&self) -> MutationJournalState {
        self.as_v1().state
    }
    pub fn row_count(&self) -> u64 {
        self.as_v1().row_count
    }
    pub fn emitted_delta_first_seq(&self) -> Option<ShardEventSeq> {
        self.as_v1().emitted_delta_first_seq
    }
    pub fn emitted_delta_last_seq(&self) -> Option<ShardEventSeq> {
        self.as_v1().emitted_delta_last_seq
    }
    pub fn hot_forward_vertices(&self) -> &Vec<crate::federation::LocalVertexId> {
        &self.as_v1().hot_forward_vertices
    }
    pub fn allocated_vertex_ids(&self) -> Option<&[crate::federation::LocalVertexId]> {
        self.as_v1().allocated_vertex_ids.as_deref()
    }
    pub fn next_index(&self) -> Option<u32> {
        self.as_v1().next_index
    }
    pub fn bulk_progress(&self) -> &Option<GraphBulkMutationProgress> {
        &self.as_v1().bulk_progress
    }
    pub fn request_identity(&self) -> &GraphMutationRequestIdentityV1 {
        self.validate()
            .expect("invalid graph mutation journal wire entry");
        &self.as_v1().request_identity
    }
    pub fn retirement(&self) -> GraphMutationRetirementWireV1 {
        self.validate()
            .expect("invalid graph mutation journal wire entry");
        self.as_v1().retirement
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let entry = self.as_v1();
        validate_graph_mutation_journal_fields(
            &entry.request_identity,
            entry.retirement,
            entry.state,
            entry.row_count,
            entry.next_index,
            &entry.bulk_progress,
        )?;
        validate_allocated_vertex_ids_appendix(
            &entry.request_identity,
            entry.allocated_vertex_ids.as_deref(),
        )
    }

    pub fn set_state(&mut self, state: MutationJournalState) {
        self.as_v1_mut().state = state;
    }
    pub fn set_row_count(&mut self, row_count: u64) {
        self.as_v1_mut().row_count = row_count;
    }
    pub fn set_emitted_delta_first_seq(&mut self, seq: Option<ShardEventSeq>) {
        self.as_v1_mut().emitted_delta_first_seq = seq;
    }
    pub fn set_emitted_delta_last_seq(&mut self, seq: Option<ShardEventSeq>) {
        self.as_v1_mut().emitted_delta_last_seq = seq;
    }
    pub fn set_hot_forward_vertices(&mut self, vertices: Vec<crate::federation::LocalVertexId>) {
        self.as_v1_mut().hot_forward_vertices = vertices;
    }
    pub fn set_allocated_vertex_ids(
        &mut self,
        allocated_vertex_ids: Option<Vec<crate::federation::LocalVertexId>>,
    ) {
        self.as_v1_mut().allocated_vertex_ids = allocated_vertex_ids;
    }
    pub fn set_next_index(&mut self, next_index: Option<u32>) {
        self.as_v1_mut().next_index = next_index;
    }
    pub fn set_bulk_progress(&mut self, bulk_progress: Option<GraphBulkMutationProgress>) {
        self.as_v1_mut().bulk_progress = bulk_progress;
    }
    pub fn set_request_identity(&mut self, identity: GraphMutationRequestIdentityV1) {
        self.as_v1_mut().request_identity = identity;
    }
    pub fn set_retirement(&mut self, retirement: GraphMutationRetirementWireV1) {
        self.as_v1_mut().retirement = retirement;
    }
}

impl GraphBulkMutationProgress {
    pub fn new(operation_count: u32, completed_count: u32, operation_row_counts: Vec<u64>) -> Self {
        Self::V1(GraphBulkMutationProgressV1 {
            operation_count,
            completed_count,
            operation_row_counts,
        })
    }

    pub fn operation_count(&self) -> u32 {
        match self {
            Self::V1(v1) => v1.operation_count,
        }
    }

    pub fn completed_count(&self) -> u32 {
        match self {
            Self::V1(v1) => v1.completed_count,
        }
    }

    pub fn operation_row_counts(&self) -> &[u64] {
        match self {
            Self::V1(v1) => &v1.operation_row_counts,
        }
    }
}

/// Router → graph: read a batch of mutation journal entries in one call.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GetMutationJournalEntriesArgs {
    pub mutation_ids: Vec<MutationId>,
}

/// Graph → router: ordered optional journal entries for [`GetMutationJournalEntriesArgs`].
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GetMutationJournalEntriesResult {
    pub entries: Vec<Option<GraphMutationJournalEntryWire>>,
    /// Smallest mutation id not included because the Graph canister neared its instruction budget.
    /// When present, the Router must issue a follow-up batch read for this and larger ids.
    pub next: Option<MutationId>,
}

#[derive(Clone, Debug, Default, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedLabelTable {
    pub vertex: Vec<ResolvedVertexLabel>,
    pub edge: Vec<ResolvedEdgeLabel>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedVertexLabel {
    pub name: String,
    pub id: VertexLabelId,
}

/// Physical field descriptor for one fixed-size inline edge STRUCT slot.
///
/// Router derives this from the canonical declaration order; Graph receives it as a plan-scoped
/// projection and must not persist or infer it. Each descriptor carries only the data Graph needs
/// to validate and decode the inline property bytes slice: field name, byte offset, and exact scalar profile.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedInlineStructField {
    pub name: String,
    pub byte_offset: u16,
    pub profile: EdgeInlinePropertyProfile,
}

/// Router-derived resolved schema for the named inline edge property of one concrete label.
///
/// Replaces the ambiguous `Option<PropertyId>` parallel wire state with one explicit enum:
/// - `None`: this label has no named inline property.
/// - `Scalar { property_id }`: one fixed-width scalar inline property.
/// - `Struct { property_id, fields }`: one fixed-size inline STRUCT, declaration-ordered.
///
/// Graph receives this as a plan-scoped projection and must not persist or infer it.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum ResolvedInlineSchema {
    #[serde(rename = "scalar")]
    Scalar { property_id: PropertyId },
    #[serde(rename = "struct")]
    Struct {
        property_id: PropertyId,
        fields: Vec<ResolvedInlineStructField>,
    },
}

impl ResolvedInlineSchema {
    /// The inline property identity for this schema, regardless of scalar or struct shape.
    #[inline]
    pub fn property_id(&self) -> PropertyId {
        match self {
            Self::Scalar { property_id } | Self::Struct { property_id, .. } => *property_id,
        }
    }

    /// True when this resolved schema is a struct projection.
    #[inline]
    pub fn is_struct(&self) -> bool {
        matches!(self, Self::Struct { .. })
    }

    /// True when this resolved schema is a scalar projection.
    #[inline]
    pub fn is_scalar(&self) -> bool {
        matches!(self, Self::Scalar { .. })
    }
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedEdgeLabel {
    pub name: String,
    pub id: EdgeLabelId,
    /// Router-owned logical schema (ADR 0008). Default `no_inline_property` when omitted on legacy wire.
    pub inline_property_profile: EdgeInlinePropertyProfile,
    /// Router-derived named inline property schema for this concrete edge label (ADR 0034 Slices 21/24/25).
    /// `None` for labels with no named inline slot; otherwise a scalar or struct projection.
    /// Graph receives this as a plan-scoped projection and must not persist or infer it.
    pub inline_schema: Option<ResolvedInlineSchema>,
    /// Per-label ordering policy resolved by the Router from the Graph Type declaration
    /// (ADR 0052 §1/§4). `Unordered` is the default; `Insertion` declares that the
    /// bucket-local live order is the semantic insertion order.
    pub ordering: EdgeOrderingPolicy,
}

/// Per-label edge ordering policy resolved by the Router from the Graph Type
/// declaration (ADR 0052 §1). The default is `Unordered`; `Insertion` labels
/// preserve bucket-local insertion order. This is a resolved schema projection:
/// Graph must not persist or re-derive it from physical state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, CandidType, Serialize, Deserialize)]
pub enum EdgeOrderingPolicy {
    Unordered,
    Insertion,
}

impl EdgeOrderingPolicy {
    pub fn is_insertion(self) -> bool {
        matches!(self, Self::Insertion)
    }
}

impl ResolvedEdgeLabel {
    pub fn new(
        name: impl Into<String>,
        id: EdgeLabelId,
        inline_property_profile: EdgeInlinePropertyProfile,
    ) -> Self {
        Self::with_inline_schema(name, id, inline_property_profile, None)
    }

    pub fn with_inline_schema(
        name: impl Into<String>,
        id: EdgeLabelId,
        inline_property_profile: EdgeInlinePropertyProfile,
        inline_schema: Option<ResolvedInlineSchema>,
    ) -> Self {
        Self {
            name: name.into(),
            id,
            inline_property_profile,
            inline_schema,
            ordering: EdgeOrderingPolicy::Unordered,
        }
    }

    /// Sets the resolved ordering policy (ADR 0052 §4).
    pub fn with_ordering(mut self, ordering: EdgeOrderingPolicy) -> Self {
        self.ordering = ordering;
        self
    }

    /// Scalar convenience constructor: builds a `Scalar { property_id }` resolved inline schema.
    pub fn with_inline_property(
        name: impl Into<String>,
        id: EdgeLabelId,
        inline_property_profile: EdgeInlinePropertyProfile,
        inline_property_id: Option<PropertyId>,
    ) -> Self {
        let inline_schema =
            inline_property_id.map(|property_id| ResolvedInlineSchema::Scalar { property_id });
        Self::with_inline_schema(name, id, inline_property_profile, inline_schema)
    }

    /// The inline property identity projected from Router schema, if any.
    #[inline]
    pub fn inline_property_id(&self) -> Option<PropertyId> {
        self.inline_schema
            .as_ref()
            .map(ResolvedInlineSchema::property_id)
    }

    /// The resolved inline schema projection, if any.
    #[inline]
    pub fn inline_schema(&self) -> Option<&ResolvedInlineSchema> {
        self.inline_schema.as_ref()
    }
}

impl ResolvedLabelTable {
    pub fn edge_inline_property_profile(
        &self,
        id: EdgeLabelId,
    ) -> Option<&EdgeInlinePropertyProfile> {
        self.edge
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.inline_property_profile)
    }

    pub fn resolved_edge_label(&self, id: EdgeLabelId) -> Option<&ResolvedEdgeLabel> {
        self.edge.iter().find(|entry| entry.id == id)
    }

    pub fn edge_label_ids_with_nonzero_inline_property_bytes(&self) -> Vec<EdgeLabelId> {
        self.edge
            .iter()
            .filter(|entry| entry.inline_property_profile.required_byte_width() > 0)
            .map(|entry| entry.id)
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedPropertyTable {
    pub properties: Vec<ResolvedProperty>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedProperty {
    pub name: String,
    pub id: PropertyId,
}

/// Shard-local edge identity for router seed bindings (ADR 0009 phase D).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LocalEdgePosting {
    pub owner_vertex_id: u32,
    pub label_id: u16,
    pub slot_index: u32,
}

/// Router → graph seed bindings for a single variable on the target shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingEntry {
    pub variable: String,
    pub local_vertex_ids: Vec<u32>,
    pub local_edge_postings: Vec<LocalEdgePosting>,
}

/// One vertex binding inside a row-shaped seed, with optional label constraints enforced during
/// hydration. Carrying the label ids on the seed row lets the Router express a leading
/// `NodeScan(variable, label = Some(...))` without leaking label-name resolution into the graph
/// canister. Label ids are stored as raw `u16` because Candid does not subtype through the
/// `VertexLabelId` newtype inside a vector.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedVertexBinding {
    pub variable: String,
    pub local_vertex_id: u32,
    pub required_vertex_label_ids: Vec<u16>,
}

/// One scalar binding inside a row-shaped seed. Used to carry a `SEARCH ... SCORE/DISTANCE AS alias`
/// value alongside its matched vertex binding without requiring a second grouped seed entry.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct SeedFloat64Binding {
    pub variable: String,
    pub value: f64,
}

/// One complete seed row produced by Router-side vector-search lowering. Each hit becomes one row
/// carrying the matched vertex and the score/distance alias. Row-shaped seeds are processed
/// independently; a row is skipped if any of its required vertex bindings is missing, tombstoned, or
/// fails the required label check.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct SeedRowWire {
    pub vertex_bindings: Vec<SeedVertexBinding>,
    pub float64_bindings: Vec<SeedFloat64Binding>,
}

/// Router → graph seed bindings. `entries` is the legacy grouped-anchor path; `rows` is the
/// row-shaped path introduced for GQL `SEARCH` lowering. Both may be present; a plan that uses row
/// seeds has already had its leading anchor stripped, so the graph executor consumes only `rows`.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingsWire {
    pub entries: Vec<SeedBindingEntry>,
    pub rows: Vec<SeedRowWire>,
    /// When true, `rows` are complete for the entire read prefix and the Graph executor may skip
    /// the whole prefix rather than only the leading index-anchor ops. Introduced for ADR 0046
    /// Phase 1 multi-variable seed relations; `false` preserves the legacy `SEARCH`/single-variable
    /// semantics. Missing field decodes as `false` for stable blobs encoded before this addition.
    #[serde(default)]
    pub complete_prefix_rows: bool,
}

/// One vertex hit inside a Router-resolved non-leading `SEARCH` relation (ADR 0034 Slice 5).
/// Carries only the provider-neutral shard-local vertex id and the user-visible scalar value.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedSearchVertexHitWire {
    pub local_vertex_id: u32,
    pub value: f64,
}

/// Router-resolved relation for one non-leading `PlanOp::Search` (ADR 0034 Slice 5).
///
/// `binding` names the vertex variable that must already be bound when the operator executes.
/// `output_alias` names the scalar binding to add to each surviving row. The Graph executor joins
/// `input_rows[d]` against `vertex_hits.local_vertex_id` and binds `output_alias` to the matching
/// `value`.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedSearchWire {
    pub binding: String,
    pub output_alias: String,
    pub vertex_hits: Vec<ResolvedSearchVertexHitWire>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EdgeInlinePropertyEncoding;
    use crate::federation::ElementIdEncodingKey;
    use candid::{Decode, Encode};

    #[test]
    fn execute_plan_result_roundtrip_with_hot_forward_vertices() {
        let result = ExecutePlanResult {
            row_count: 1,
            rows_blob: None,
            hot_forward_vertices: vec![7, 42],
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: ExecutePlanResult = Decode!(&bytes, ExecutePlanResult).expect("decode");
        assert_eq!(result, decoded);
    }

    #[test]
    fn ordered_batch_identity_and_receipt_roundtrip() {
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [7u8; 32],
            logical_item_count: 2,
        };
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: 2,
            emitted_delta_first_seq: Some(11),
            emitted_delta_last_seq: Some(12),
            hot_forward_vertices: vec![3, 9],
        };
        receipt.validate().expect("canonical receipt");

        let bytes = Encode!(&identity, &receipt).expect("encode ordered batch contract");
        let (decoded_identity, decoded_receipt) = Decode!(
            &bytes,
            GraphMutationRequestIdentityV1,
            GraphOrderedEdgeBatchReceiptV1
        )
        .expect("decode ordered batch contract");
        assert_eq!(identity, decoded_identity);
        assert_eq!(receipt, decoded_receipt);
    }

    #[test]
    fn ordered_batch_receipt_rejects_noncanonical_hot_vertices() {
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: vec![9, 3],
        };
        assert!(receipt.validate().is_err());
    }

    #[test]
    fn ordered_vertex_receipt_preserves_allocated_vertex_operation_order() {
        let receipt = GraphOrderedVertexBatchReceiptV1 {
            logical_vertex_count: 2,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: vec![3, 9],
            allocated_vertex_ids: vec![9, 3],
        };
        receipt
            .validate()
            .expect("allocated IDs are ordinal, not sorted");
        let bytes = Encode!(&receipt).expect("encode receipt");
        let decoded: GraphOrderedVertexBatchReceiptV1 =
            Decode!(&bytes, GraphOrderedVertexBatchReceiptV1).expect("decode receipt");
        assert_eq!(decoded.allocated_vertex_ids, vec![9, 3]);
    }

    #[test]
    fn allocated_vertex_id_appendix_is_required_only_for_vertex_families() {
        let edge_identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [0; 32],
            logical_item_count: 1,
        };
        assert_eq!(
            validate_allocated_vertex_ids_appendix(&edge_identity, Some(&[7])),
            Err("ordered edge journal entry must not have allocated vertex ids")
        );
        let vertex_identity = GraphMutationRequestIdentityV1::OrderedVertexBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [0; 32],
            logical_item_count: 1,
        };
        assert_eq!(
            validate_allocated_vertex_ids_appendix(&vertex_identity, None),
            Err("ordered vertex journal entry requires allocated vertex ids")
        );
        assert!(validate_allocated_vertex_ids_appendix(&vertex_identity, Some(&[7])).is_ok());
    }

    #[test]
    fn ordered_result_and_retirement_ack_roundtrip() {
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: 2,
            emitted_delta_first_seq: Some(4),
            emitted_delta_last_seq: Some(5),
            hot_forward_vertices: vec![3, 9],
        };
        let result = GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(
            receipt.clone(),
        ));
        let ack = OrderedMutationRetirementAck::V1(OrderedMutationRetirementAckV1 {
            mutation_id: 7,
            graph_request_fingerprint: [6; 32],
            receipt,
        });
        result.validate().expect("valid ordered result");
        ack.validate().expect("valid retirement ack");
        let bytes = Encode!(&result, &ack).expect("encode ordered response envelopes");
        let (decoded_result, decoded_ack) = Decode!(
            &bytes,
            GraphOrderedEdgeBatchResult,
            OrderedMutationRetirementAck
        )
        .expect("decode ordered response envelopes");
        assert_eq!(result, decoded_result);
        assert_eq!(ack, decoded_ack);
    }

    #[test]
    fn ordered_graph_request_roundtrip_and_shape_validation() {
        let request = OrderedEdgeBatchGraphRequest::V1(OrderedEdgeBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedEdgeBatchGraphItemV1 {
                source_local_vertex_id: 3,
                target_local_vertex_id: 9,
                directed: true,
                catalog_edge_label_id: None,
                inline_property_bytes: Vec::new(),
                resolved_initial_edge_properties: vec![ResolvedOrderedEdgePropertyV1 {
                    property_id: PropertyId::from_raw(4),
                    value: vec![1, 2],
                }],
            }],
        });
        request.validate().expect("valid ordered graph request");
        let bytes = Encode!(&request).expect("encode ordered graph request");
        let decoded: OrderedEdgeBatchGraphRequest =
            Decode!(&bytes, OrderedEdgeBatchGraphRequest).expect("decode ordered graph request");
        assert_eq!(request, decoded);

        let OrderedEdgeBatchGraphRequest::V1(mut invalid) = request;
        invalid.items[0]
            .resolved_initial_edge_properties
            .push(ResolvedOrderedEdgePropertyV1 {
                property_id: PropertyId::from_raw(4),
                value: vec![3],
            });
        assert!(
            OrderedEdgeBatchGraphRequest::V1(invalid)
                .validate()
                .unwrap_err()
                .contains("repeats property id")
        );
    }

    #[test]
    fn ordered_edge_request_uses_payload_instead_of_fixed_item_count() {
        let item = OrderedEdgeBatchGraphItemV1 {
            source_local_vertex_id: 1,
            target_local_vertex_id: 2,
            directed: true,
            catalog_edge_label_id: None,
            inline_property_bytes: Vec::new(),
            resolved_initial_edge_properties: Vec::new(),
        };
        let request = OrderedEdgeBatchGraphRequest::V1(OrderedEdgeBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![item; 1_025],
        });

        request
            .validate()
            .expect("payload-sized edge request should not hit a fixed item cap");
    }

    #[test]
    fn ordered_vertex_graph_request_roundtrip_and_fingerprint_is_order_sensitive() {
        let request = OrderedVertexBatchGraphRequest::V1(OrderedVertexBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: vec![3],
                resolved_initial_properties: vec![ResolvedOrderedEdgePropertyV1 {
                    property_id: PropertyId::from_raw(4),
                    value: vec![1, 2],
                }],
            }],
        });
        request
            .validate()
            .expect("valid ordered vertex graph request");
        let bytes = Encode!(&request).expect("encode ordered vertex graph request");
        let decoded: OrderedVertexBatchGraphRequest =
            Decode!(&bytes, OrderedVertexBatchGraphRequest).expect("decode vertex request");
        assert_eq!(request, decoded);

        let fingerprint = ordered_vertex_batch_graph_request_fingerprint(&request)
            .expect("vertex request fingerprint");
        let mut reordered = request.clone();
        let OrderedVertexBatchGraphRequest::V1(request) = &mut reordered;
        request.items[0].resolved_vertex_labels.push(5);
        assert_ne!(
            fingerprint,
            ordered_vertex_batch_graph_request_fingerprint(&reordered)
                .expect("reordered vertex fingerprint")
        );
    }

    #[test]
    fn ordered_vertex_graph_request_rejects_reserved_property_id() {
        let request = OrderedVertexBatchGraphRequest::V1(OrderedVertexBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: Vec::new(),
                resolved_initial_properties: vec![ResolvedOrderedEdgePropertyV1 {
                    property_id: PropertyId::from_raw(0),
                    value: Vec::new(),
                }],
            }],
        });
        assert!(request.validate().is_err());
    }

    #[test]
    fn ordered_mixed_graph_request_roundtrips_and_fingerprints_operation_order() {
        let request = OrderedMixedBatchGraphRequest::V1(OrderedMixedBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            operations: vec![
                OrderedMixedGraphOperationV1::Vertex(OrderedVertexBatchGraphItemV1 {
                    resolved_vertex_labels: vec![3],
                    resolved_initial_properties: Vec::new(),
                }),
                OrderedMixedGraphOperationV1::Edge(OrderedMixedGraphEdgeItemV1 {
                    source: OrderedMixedGraphEndpointV1::NewVertexOrdinal(0),
                    target: OrderedMixedGraphEndpointV1::Existing(9),
                    directed: true,
                    catalog_edge_label_id: None,
                    inline_property_bytes: Vec::new(),
                    resolved_initial_edge_properties: Vec::new(),
                }),
            ],
        });
        request.validate().expect("valid mixed Graph request");
        let bytes = Encode!(&request).expect("encode mixed Graph request");
        let decoded: OrderedMixedBatchGraphRequest =
            Decode!(&bytes, OrderedMixedBatchGraphRequest).expect("decode mixed Graph request");
        assert_eq!(request, decoded);

        let fingerprint = ordered_mixed_batch_graph_request_fingerprint(&request)
            .expect("mixed Graph request fingerprint");
        let mut reordered = request.clone();
        let OrderedMixedBatchGraphRequest::V1(request) = &mut reordered;
        request.operations.swap(0, 1);
        assert_ne!(
            fingerprint,
            ordered_mixed_batch_graph_request_fingerprint(&reordered)
                .expect("reordered mixed fingerprint")
        );
    }

    #[test]
    fn ordered_mixed_graph_request_rejects_forward_or_out_of_range_vertex_ordinals() {
        let edge = OrderedMixedGraphOperationV1::Edge(OrderedMixedGraphEdgeItemV1 {
            source: OrderedMixedGraphEndpointV1::NewVertexOrdinal(1),
            target: OrderedMixedGraphEndpointV1::Existing(9),
            directed: true,
            catalog_edge_label_id: None,
            inline_property_bytes: Vec::new(),
            resolved_initial_edge_properties: Vec::new(),
        });
        let request = OrderedMixedBatchGraphRequest::V1(OrderedMixedBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            operations: vec![
                edge,
                OrderedMixedGraphOperationV1::Vertex(OrderedVertexBatchGraphItemV1 {
                    resolved_vertex_labels: Vec::new(),
                    resolved_initial_properties: Vec::new(),
                }),
            ],
        });
        let error = request.validate().expect_err("forward ordinal must reject");
        assert!(error.contains("vertex ordinal 1"));
    }

    #[test]
    fn ordered_mixed_graph_request_plans_vertex_then_edge_phases() {
        let request = OrderedMixedBatchGraphRequest::V1(OrderedMixedBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(7),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::management_canister(),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            operations: vec![
                OrderedMixedGraphOperationV1::Vertex(OrderedVertexBatchGraphItemV1 {
                    resolved_vertex_labels: vec![3],
                    resolved_initial_properties: Vec::new(),
                }),
                OrderedMixedGraphOperationV1::Edge(OrderedMixedGraphEdgeItemV1 {
                    source: OrderedMixedGraphEndpointV1::NewVertexOrdinal(0),
                    target: OrderedMixedGraphEndpointV1::Existing(9),
                    directed: true,
                    catalog_edge_label_id: None,
                    inline_property_bytes: Vec::new(),
                    resolved_initial_edge_properties: Vec::new(),
                }),
            ],
        });
        let phases = request.plan_phases(&[41]).expect("mixed phase planning");
        let OrderedVertexBatchGraphRequest::V1(vertices) = phases.vertex_request;
        assert_eq!(vertices.items.len(), 1);
        let OrderedEdgeBatchGraphRequest::V1(edges) = phases.edge_request;
        assert_eq!(edges.items.len(), 1);
        assert_eq!(edges.items[0].source_local_vertex_id, 41);
        assert_eq!(edges.items[0].target_local_vertex_id, 9);

        let error = request
            .plan_phases(&[])
            .expect_err("allocation cardinality must be exact");
        assert!(error.contains("expected 1"));
    }

    #[test]
    fn ordered_graph_fingerprint_changes_with_item_order_and_payload() {
        let item = |source, target| OrderedEdgeBatchGraphItemV1 {
            source_local_vertex_id: source,
            target_local_vertex_id: target,
            directed: true,
            catalog_edge_label_id: None,
            inline_property_bytes: Vec::new(),
            resolved_initial_edge_properties: Vec::new(),
        };
        let request = |items| {
            OrderedEdgeBatchGraphRequest::V1(OrderedEdgeBatchGraphRequestV1 {
                graph_id: GraphId::from_raw(7),
                target_shard_id: ShardId::new(2),
                target_graph_canister: Principal::management_canister(),
                resolved_labels: ResolvedLabelTable::default(),
                resolved_properties: ResolvedPropertyTable::default(),
                items,
            })
        };
        let ordered = request(vec![item(1, 2), item(3, 4)]);
        let reordered = request(vec![item(3, 4), item(1, 2)]);
        let changed = request(vec![item(1, 2), item(3, 5)]);
        let ordered_fingerprint =
            ordered_edge_batch_graph_request_fingerprint(&ordered).expect("fingerprint");
        assert_ne!(
            ordered_fingerprint,
            ordered_edge_batch_graph_request_fingerprint(&reordered)
                .expect("reordered fingerprint")
        );
        assert_ne!(
            ordered_fingerprint,
            ordered_edge_batch_graph_request_fingerprint(&changed).expect("changed fingerprint")
        );
        assert_eq!(
            ordered_fingerprint,
            ordered_edge_batch_graph_request_fingerprint(&ordered).expect("repeat fingerprint")
        );
    }

    #[test]
    fn mutation_journal_wire_roundtrip_preserves_replay_contract() {
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [4; 32],
            logical_item_count: 3,
        };
        let mut entry = GraphMutationJournalEntryWire::new(
            7,
            MutationJournalState::Completed,
            3,
            None,
            None,
            vec![],
        );
        entry.set_request_identity(identity.clone());
        entry.set_retirement(GraphMutationRetirementWireV1::Retired);

        let bytes = Encode!(&entry).expect("encode journal wire");
        let decoded: GraphMutationJournalEntryWire =
            Decode!(&bytes, GraphMutationJournalEntryWire).expect("decode journal wire");
        assert_eq!(decoded.request_identity(), &identity);
        assert_eq!(decoded.retirement(), GraphMutationRetirementWireV1::Retired);
    }

    #[test]
    fn ordered_journal_validation_rejects_wrong_row_count() {
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [0; 32],
            logical_item_count: 2,
        };
        assert_eq!(
            validate_graph_mutation_journal_fields(
                &identity,
                GraphMutationRetirementWireV1::Active,
                MutationJournalState::Completed,
                1,
                None,
                &None,
            ),
            Err("ordered journal row_count must equal logical_item_count")
        );
    }

    #[test]
    fn plan_execution_journal_validation_rejects_retirement() {
        assert_eq!(
            validate_graph_mutation_journal_fields(
                &GraphMutationRequestIdentityV1::PlanExecution,
                GraphMutationRetirementWireV1::Active,
                MutationJournalState::Completed,
                0,
                None,
                &None,
            ),
            Err("PlanExecution journal entry must not have retirement state")
        );
    }

    #[test]
    fn execute_plan_result_roundtrip_with_rows_blob() {
        let result = ExecutePlanResult {
            row_count: 2,
            rows_blob: Some(vec![1, 2, 3]),
            hot_forward_vertices: Vec::new(),
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: ExecutePlanResult = Decode!(&bytes, ExecutePlanResult).expect("decode");
        assert_eq!(result, decoded);
    }

    #[test]
    fn execute_plan_batch_result_roundtrip_preserves_ordered_partial_outcomes() {
        let result = ExecutePlanBatchResult {
            results: vec![
                Ok(ExecutePlanResult {
                    row_count: 3,
                    rows_blob: None,
                    hot_forward_vertices: vec![9],
                }),
                Err("item failed".to_string()),
            ],
            next_index: Some(1),
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: ExecutePlanBatchResult =
            Decode!(&bytes, ExecutePlanBatchResult).expect("decode");
        assert_eq!(result, decoded);
    }

    #[test]
    fn mutation_token_candid_roundtrip() {
        let token = MutationToken {
            mutation_id: 42,
            shards: vec![
                MutationTokenShard {
                    shard_id: ShardId::new(0),
                    label_stats_seq: Some(7),
                },
                MutationTokenShard {
                    shard_id: ShardId::new(1),
                    label_stats_seq: None,
                },
            ],
        };
        let bytes = Encode!(&token).expect("encode");
        let decoded: MutationToken = Decode!(&bytes, MutationToken).expect("decode");
        assert_eq!(token, decoded);
    }

    #[test]
    fn read_mode_candid_roundtrip_all_variants() {
        for mode in [
            ReadMode::Eventual,
            ReadMode::AtLeast(MutationToken {
                mutation_id: 11,
                shards: vec![MutationTokenShard {
                    shard_id: ShardId::new(3),
                    label_stats_seq: Some(4),
                }],
            }),
        ] {
            let bytes = Encode!(&mode).expect("encode");
            let decoded: ReadMode = Decode!(&bytes, ReadMode).expect("decode");
            assert_eq!(mode, decoded);
        }
        assert_eq!(ReadMode::default(), ReadMode::Eventual);
    }

    #[test]
    fn gql_query_result_carries_phase_and_token() {
        let result = GqlQueryResult::row_count_only(3)
            .with_phase(MutationLifecyclePhase::ProjectionPending)
            .with_token(MutationToken {
                mutation_id: 9,
                shards: vec![MutationTokenShard {
                    shard_id: ShardId::new(2),
                    label_stats_seq: Some(5),
                }],
            });
        let bytes = Encode!(&result).expect("encode");
        let decoded: GqlQueryResult = Decode!(&bytes, GqlQueryResult).expect("decode");
        assert_eq!(result, decoded);
        assert_eq!(
            decoded.phase,
            Some(MutationLifecyclePhase::ProjectionPending)
        );
        assert_eq!(decoded.token.expect("token").mutation_id, 9);
    }

    #[test]
    fn gql_execution_mode_candid_roundtrip() {
        for mode in [GqlExecutionMode::Query, GqlExecutionMode::Update] {
            let bytes = Encode!(&mode).expect("encode");
            let decoded: GqlExecutionMode = Decode!(&bytes, GqlExecutionMode).expect("decode");
            assert_eq!(mode, decoded);
        }
    }

    #[test]
    fn execute_plan_args_with_seed_bindings_roundtrip() {
        let seed = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "u".into(),
                local_vertex_ids: vec![1, 2],
                local_edge_postings: Vec::new(),
            }],
            rows: vec![SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "d".into(),
                    local_vertex_id: 7,
                    required_vertex_label_ids: vec![3],
                }],
                float64_bindings: vec![SeedFloat64Binding {
                    variable: "distance".into(),
                    value: 1.5,
                }],
            }],
            complete_prefix_rows: false,
        };
        let seed_blob = Encode!(&seed).expect("seed encode");
        let args = ExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
            mutation_id: Some(1),
            plan_blob: vec![1, 2, 3],
            params_blob: vec![4],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: Some(ResolvedLabelTable {
                vertex: vec![ResolvedVertexLabel {
                    name: "User".into(),
                    id: VertexLabelId::from_raw(1),
                }],
                edge: vec![ResolvedEdgeLabel::with_inline_property(
                    "KNOWS",
                    EdgeLabelId::from_raw(1),
                    EdgeInlinePropertyProfile::no_inline_property(),
                    None,
                )],
            }),
            resolved_properties: Some(ResolvedPropertyTable {
                properties: vec![ResolvedProperty {
                    name: "name".into(),
                    id: PropertyId::from_raw(1),
                }],
            }),
            indexed_properties: None,
            unique_claims: Some(vec![UniqueClaimDispatch {
                claim_ordinal: 0,
                constraint_id: ConstraintNameId::from_raw(3),
                encoded_value: vec![9, 8, 7],
            }]),
            constrained_properties: Some(vec![ConstrainedPropertyDispatch {
                vertex_label_id: VertexLabelId::from_raw(2),
                property_id: PropertyId::from_raw(1),
                constraint_id: ConstraintNameId::from_raw(3),
            }]),
            local_unique_claims: Some(vec![UniqueClaimDispatch {
                claim_ordinal: 0,
                constraint_id: ConstraintNameId::from_raw(4),
                encoded_value: vec![5, 6],
            }]),
            local_constrained_properties: Some(vec![ConstrainedPropertyDispatch {
                vertex_label_id: VertexLabelId::from_raw(2),
                property_id: PropertyId::from_raw(1),
                constraint_id: ConstraintNameId::from_raw(4),
            }]),
            indexed_embeddings: Some(crate::vector_index::IndexedEmbeddingCatalog {
                embeddings: vec![crate::vector_index::IndexedEmbeddingSpec {
                    embedding_name_id: 5,
                    index_id: 11,
                    kind: crate::vector_index::VectorIndexKind::IvfFlat,
                    metric: crate::vector_index::VectorMetric::L2Squared,
                    encoding: crate::vector_index::VectorEncoding::F32,
                    dims: 16,
                }],
            }),
            resolved_search_blob: None,
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn resolved_search_wire_roundtrip() {
        let wire = ResolvedSearchWire {
            binding: "d".into(),
            output_alias: "similarity".into(),
            vertex_hits: vec![
                ResolvedSearchVertexHitWire {
                    local_vertex_id: 7,
                    value: 0.75,
                },
                ResolvedSearchVertexHitWire {
                    local_vertex_id: 42,
                    value: f64::NEG_INFINITY,
                },
            ],
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: ResolvedSearchWire = Decode!(&bytes, ResolvedSearchWire).expect("decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn resolved_search_wire_empty_hits_roundtrip() {
        let wire = ResolvedSearchWire {
            binding: "d".into(),
            output_alias: "distance".into(),
            vertex_hits: Vec::new(),
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: ResolvedSearchWire = Decode!(&bytes, ResolvedSearchWire).expect("decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn execute_plan_args_legacy_blob_without_resolved_search_decodes_as_none() {
        #[derive(CandidType, Serialize)]
        struct LegacyExecutePlanArgs {
            target_shard_id: ShardId,
            element_id_encoding_key: [u8; 16],
            mutation_id: Option<MutationId>,
            plan_blob: Vec<u8>,
            params_blob: Vec<u8>,
            mode: GqlExecutionMode,
            seed_bindings_blob: Option<Vec<u8>>,
            resolved_labels: Option<ResolvedLabelTable>,
            resolved_properties: Option<ResolvedPropertyTable>,
            indexed_properties: Option<crate::index::IndexedPropertyCatalog>,
            unique_claims: Option<Vec<UniqueClaimDispatch>>,
            constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
            local_unique_claims: Option<Vec<UniqueClaimDispatch>>,
            local_constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
            indexed_embeddings: Option<crate::vector_index::IndexedEmbeddingCatalog>,
        }
        let legacy = LegacyExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
            mutation_id: None,
            plan_blob: vec![1, 2],
            params_blob: vec![3],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: None,
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
            local_unique_claims: None,
            local_constrained_properties: None,
            indexed_embeddings: None,
        };
        let bytes = Encode!(&legacy).expect("encode legacy");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode legacy");
        assert_eq!(decoded.resolved_search_blob, None);
    }

    #[test]
    fn execute_plan_args_with_resolved_search_blob_roundtrip() {
        let search_wire = ResolvedSearchWire {
            binding: "d".into(),
            output_alias: "similarity".into(),
            vertex_hits: vec![ResolvedSearchVertexHitWire {
                local_vertex_id: 7,
                value: 0.75,
            }],
        };
        let search_blob = Encode!(&search_wire).expect("search encode");
        let args = ExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
            mutation_id: None,
            plan_blob: vec![1, 2],
            params_blob: vec![3],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: None,
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
            local_unique_claims: None,
            local_constrained_properties: None,
            indexed_embeddings: None,
            resolved_search_blob: Some(search_blob),
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode");
        assert_eq!(decoded.resolved_search_blob, args.resolved_search_blob);
        let decoded_search: ResolvedSearchWire = Decode!(
            decoded.resolved_search_blob.as_ref().unwrap(),
            ResolvedSearchWire
        )
        .expect("decode inner search wire");
        assert_eq!(decoded_search, search_wire);
    }

    #[test]
    fn seed_bindings_wire_roundtrip() {
        let wire = SeedBindingsWire {
            entries: vec![
                SeedBindingEntry {
                    variable: "a".into(),
                    local_vertex_ids: vec![10],
                    local_edge_postings: Vec::new(),
                },
                SeedBindingEntry {
                    variable: "b".into(),
                    local_vertex_ids: vec![20, 21],
                    local_edge_postings: Vec::new(),
                },
            ],
            rows: vec![SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "d".into(),
                    local_vertex_id: 5,
                    required_vertex_label_ids: vec![2],
                }],
                float64_bindings: vec![SeedFloat64Binding {
                    variable: "score".into(),
                    value: 0.25,
                }],
            }],
            complete_prefix_rows: false,
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(decoded.entries, wire.entries);
        assert_eq!(decoded.rows, wire.rows);
    }

    #[test]
    fn edge_seed_bindings_wire_roundtrip() {
        let wire = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "e".into(),
                local_vertex_ids: Vec::new(),
                local_edge_postings: vec![
                    LocalEdgePosting {
                        owner_vertex_id: 3,
                        label_id: 7,
                        slot_index: 1,
                    },
                    LocalEdgePosting {
                        owner_vertex_id: 4,
                        label_id: 7,
                        slot_index: 0,
                    },
                ],
            }],
            rows: Vec::new(),
            complete_prefix_rows: false,
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(decoded.entries, wire.entries);
        assert_eq!(decoded.rows, wire.rows);
    }

    #[test]
    #[should_panic(expected = "field rows is not optional field")]
    fn seed_bindings_wire_entries_only_blob_rejects_decode() {
        // Pre-Slice 3 blobs carried only `entries`. With `rows` as a required field the decoder now
        // rejects them. This is acceptable for Slice 3 because the only stored `seed_bindings_blob`
        // values belong to DML mutation envelopes, and `SEARCH` lowering applies only to read
        // queries.
        #[derive(CandidType, Serialize)]
        struct LegacySeedBindingsWire {
            entries: Vec<SeedBindingEntry>,
        }
        let legacy = LegacySeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "u".into(),
                local_vertex_ids: vec![1, 2],
                local_edge_postings: Vec::new(),
            }],
        };
        let legacy_bytes = Encode!(&legacy).expect("encode legacy wire");
        let _: SeedBindingsWire = Decode!(&legacy_bytes, SeedBindingsWire).expect("decode legacy");
    }
    #[test]
    fn resolved_edge_label_inline_property_id_roundtrip() {
        let label = ResolvedEdgeLabel::with_inline_property(
            "ROAD".to_string(),
            EdgeLabelId::from_raw(7),
            EdgeInlinePropertyProfile {
                byte_width: 4,
                encoding: EdgeInlinePropertyEncoding::F32,
            },
            Some(PropertyId::from_raw(42)),
        );
        let bytes = Encode!(&label).expect("encode ResolvedEdgeLabel with inline property id");
        let decoded: ResolvedEdgeLabel = Decode!(&bytes, ResolvedEdgeLabel).expect("decode");
        assert_eq!(decoded, label);
        assert_eq!(decoded.inline_property_id(), Some(PropertyId::from_raw(42)));
        assert!(matches!(
            decoded.inline_schema,
            Some(ResolvedInlineSchema::Scalar { property_id })
            if property_id == PropertyId::from_raw(42)
        ));
    }

    #[test]
    fn edge_ordering_policy_candid_roundtrip() {
        for policy in [EdgeOrderingPolicy::Unordered, EdgeOrderingPolicy::Insertion] {
            let bytes = Encode!(&policy).expect("encode edge ordering policy");
            let decoded: EdgeOrderingPolicy = Decode!(&bytes, EdgeOrderingPolicy).expect("decode");
            assert_eq!(decoded, policy);
        }
    }

    #[test]
    fn resolved_edge_label_ordering_roundtrip_and_default() {
        let defaulted = ResolvedEdgeLabel::with_inline_property(
            "ROAD".to_string(),
            EdgeLabelId::from_raw(7),
            EdgeInlinePropertyProfile::no_inline_property(),
            None,
        );
        assert_eq!(defaulted.ordering, EdgeOrderingPolicy::Unordered);
        assert!(!defaulted.ordering.is_insertion());

        let insertion = defaulted
            .clone()
            .with_ordering(EdgeOrderingPolicy::Insertion);
        assert!(insertion.ordering.is_insertion());
        let bytes = Encode!(&insertion).expect("encode resolved edge label with ordering");
        let decoded: ResolvedEdgeLabel = Decode!(&bytes, ResolvedEdgeLabel).expect("decode");
        assert_eq!(decoded.ordering, EdgeOrderingPolicy::Insertion);
    }

    #[test]
    fn resolved_edge_label_struct_schema_roundtrip() {
        let label = ResolvedEdgeLabel::with_inline_schema(
            "AFFINITY".to_string(),
            EdgeLabelId::from_raw(7),
            EdgeInlinePropertyProfile::opaque_bytes(16),
            Some(ResolvedInlineSchema::Struct {
                property_id: PropertyId::from_raw(42),
                fields: vec![
                    ResolvedInlineStructField {
                        name: "score".to_string(),
                        byte_offset: 0,
                        profile: EdgeInlinePropertyProfile {
                            byte_width: 4,
                            encoding: EdgeInlinePropertyEncoding::F32,
                        },
                    },
                    ResolvedInlineStructField {
                        name: "confidence".to_string(),
                        byte_offset: 4,
                        profile: EdgeInlinePropertyProfile {
                            byte_width: 4,
                            encoding: EdgeInlinePropertyEncoding::F32,
                        },
                    },
                    ResolvedInlineStructField {
                        name: "updated_at".to_string(),
                        byte_offset: 8,
                        profile: EdgeInlinePropertyProfile {
                            byte_width: 8,
                            encoding: EdgeInlinePropertyEncoding::RawU64,
                        },
                    },
                ],
            }),
        );
        let bytes = Encode!(&label).expect("encode ResolvedEdgeLabel with struct schema");
        let decoded: ResolvedEdgeLabel = Decode!(&bytes, ResolvedEdgeLabel).expect("decode");
        assert_eq!(decoded, label);
        assert_eq!(decoded.inline_property_id(), Some(PropertyId::from_raw(42)));
        assert!(
            decoded
                .inline_schema()
                .is_some_and(ResolvedInlineSchema::is_struct)
        );
    }

    #[test]
    fn resolved_label_table_resolves_edge_label_with_inline_id() {
        let table = ResolvedLabelTable {
            vertex: Vec::new(),
            edge: vec![ResolvedEdgeLabel::with_inline_property(
                "ROAD".to_string(),
                EdgeLabelId::from_raw(7),
                EdgeInlinePropertyProfile {
                    byte_width: 4,
                    encoding: EdgeInlinePropertyEncoding::F32,
                },
                Some(PropertyId::from_raw(42)),
            )],
        };
        let entry = table
            .resolved_edge_label(EdgeLabelId::from_raw(7))
            .expect("label");
        assert_eq!(entry.inline_property_id(), Some(PropertyId::from_raw(42)));
        assert!(matches!(
            entry.inline_schema,
            Some(ResolvedInlineSchema::Scalar { property_id })
            if property_id == PropertyId::from_raw(42)
        ));
    }
}
