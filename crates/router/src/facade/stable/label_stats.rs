//! Router-owned label stats aggregates and client mutation records (ADR 0015).

use crate::state::RouterError;
use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    GqlExecutionMode, GraphOrderedEdgeBatchReceiptV1, GraphOrderedMixedBatchReceiptV1,
    GraphOrderedVertexBatchReceiptV1, MutationId, MutationLifecyclePhase, MutationTokenShard,
    OrderedEdgeBatchGraphRequest, OrderedEdgeBatchGraphRequestV1, OrderedMixedBatchGraphRequest,
    OrderedMixedBatchGraphRequestV1, OrderedVertexBatchGraphRequest,
    OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedPropertyTable, SeedBindingsWire,
    ordered_edge_batch_graph_request_fingerprint, ordered_mixed_batch_graph_request_fingerprint,
    ordered_vertex_batch_graph_request_fingerprint,
};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabelStats {
    pub live_count: u64,
    pub total_adds: u64,
    pub total_removes: u64,
}

impl Storable for LabelStats {
    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.live_count.to_le_bytes());
        out.extend_from_slice(&self.total_adds.to_le_bytes());
        out.extend_from_slice(&self.total_removes.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut live = [0; 8];
        let mut adds = [0; 8];
        let mut removes = [0; 8];
        live.copy_from_slice(&bytes[0..8]);
        adds.copy_from_slice(&bytes[8..16]);
        removes.copy_from_slice(&bytes[16..24]);
        Self {
            live_count: u64::from_le_bytes(live),
            total_adds: u64::from_le_bytes(adds),
            total_removes: u64::from_le_bytes(removes),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphLabelKey {
    pub graph_id: GraphId,
    pub label_id: u16,
}

impl GraphLabelKey {
    pub const fn new(graph_id: GraphId, label_id: u16) -> Self {
        Self { graph_id, label_id }
    }
}

impl Storable for GraphLabelKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut label = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        label.copy_from_slice(&bytes[4..6]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphLabelShardKey {
    pub graph_id: GraphId,
    pub shard_id: ShardId,
    pub label_id: u16,
}

impl GraphLabelShardKey {
    pub const fn new(graph_id: GraphId, shard_id: ShardId, label_id: u16) -> Self {
        Self {
            graph_id,
            shard_id,
            label_id,
        }
    }
}

impl Storable for GraphLabelShardKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 10,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut shard = [0; 4];
        let mut label = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        shard.copy_from_slice(&bytes[4..8]);
        label.copy_from_slice(&bytes[8..10]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            shard_id: ShardId::from_le_bytes(shard),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientMutationKey {
    pub caller: Principal,
    pub graph_id: GraphId,
    pub client_key: String,
}

impl ClientMutationKey {
    pub fn new(caller: Principal, graph_id: GraphId, client_key: String) -> Self {
        Self {
            caller,
            graph_id,
            client_key,
        }
    }
}

impl Storable for ClientMutationKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ClientMutationKey"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ClientMutationKey")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ClientMutationKey")
    }
}

/// Reverse index row `mutation_id → (ClientMutationKey, nonterminal reservation count)` (ADR 0030
/// slice 6). It exists **iff** `nonterminal > 0`: created when a mutation's first unique reservation
/// is taken (Try) and removed when its last non-terminal reservation leaves (`FreshlyCommitted`
/// Confirm, or reclaim Cancel). It lets the reclaim reconciler resolve a reservation's `ClaimId`
/// (`mutation_id`) to the owning `RouterMutationRecord`, and pins that record against TTL GC while
/// any non-terminal reservation still depends on it for a terminal-failure decision.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MutationReservationIndexEntry {
    pub client_key: ClientMutationKey,
    pub nonterminal: u32,
}

impl Storable for MutationReservationIndexEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode MutationReservationIndexEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode MutationReservationIndexEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode MutationReservationIndexEntry")
    }
}

/// Versioned Router mutation saga record (ADR 0044).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationRecord {
    V1(RouterMutationRecordV1),
}

/// Request identity owned by the Router mutation record.
///
/// The existing scalar, legacy-bulk, and typed-seed paths use `PlanExecution`.
/// Ordered-edge identity will be added as a sibling variant when its public
/// replay envelope is installed; keeping the identity here prevents a future
/// ordered payload from reusing the untyped fingerprint field.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationRequestIdentityV1 {
    PlanExecution {
        request_fingerprint: Vec<u8>,
    },
    OrderedEdgeBatch {
        public_fingerprint: [u8; 32],
        public_item_count: u32,
    },
    OrderedVertexBatch {
        public_fingerprint: [u8; 32],
        public_item_count: u32,
    },
    OrderedMixedBatch {
        public_fingerprint: [u8; 32],
        public_operation_count: u32,
        public_vertex_count: u32,
        public_edge_count: u32,
    },
}

impl RouterMutationRequestIdentityV1 {
    pub fn request_fingerprint(&self) -> &[u8] {
        match self {
            Self::PlanExecution {
                request_fingerprint,
            } => request_fingerprint,
            Self::OrderedEdgeBatch {
                public_fingerprint, ..
            } => public_fingerprint,
            Self::OrderedVertexBatch {
                public_fingerprint, ..
            } => public_fingerprint,
            Self::OrderedMixedBatch {
                public_fingerprint, ..
            } => public_fingerprint,
        }
    }

    pub fn public_item_count(&self) -> Option<u32> {
        match self {
            Self::PlanExecution { .. } => None,
            Self::OrderedEdgeBatch {
                public_item_count, ..
            } => Some(*public_item_count),
            Self::OrderedVertexBatch {
                public_item_count, ..
            } => Some(*public_item_count),
            Self::OrderedMixedBatch {
                public_operation_count,
                ..
            } => Some(*public_operation_count),
        }
    }
}

/// Router-owned progress for one single-target ordered edge batch.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum OrderedEdgeBatchTargetProgressV1 {
    CanonicalPending,
    CanonicalCommitted(GraphOrderedEdgeBatchReceiptV1),
    ProjectionPending(GraphOrderedEdgeBatchReceiptV1),
    ProjectionAdvanced(GraphOrderedEdgeBatchReceiptV1),
    RetirementPending(GraphOrderedEdgeBatchReceiptV1),
}

impl OrderedEdgeBatchTargetProgressV1 {
    #[allow(dead_code)]
    fn receipt(&self) -> Option<&GraphOrderedEdgeBatchReceiptV1> {
        match self {
            Self::CanonicalPending => None,
            Self::CanonicalCommitted(receipt)
            | Self::ProjectionPending(receipt)
            | Self::ProjectionAdvanced(receipt)
            | Self::RetirementPending(receipt) => Some(receipt),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        if let Some(receipt) = self.receipt() {
            receipt
                .validate()
                .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        }
        Ok(())
    }
}

/// Durable replay target for one ordered Graph request.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterOrderedEdgeBatchTargetV1 {
    pub graph_request_fingerprint: [u8; 32],
    pub request: OrderedEdgeBatchGraphRequestV1,
    pub progress: OrderedEdgeBatchTargetProgressV1,
    /// Projection watermark retained until the ordered mutation is retired.
    pub projection_watermark: Option<MutationTokenShard>,
}

impl RouterOrderedEdgeBatchTargetV1 {
    #[allow(dead_code)]
    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        let request = OrderedEdgeBatchGraphRequest::V1(self.request.clone());
        let fingerprint = ordered_edge_batch_graph_request_fingerprint(&request)
            .map_err(|error| RouterError::InvalidArgument(error.to_string()))?;
        if fingerprint != self.graph_request_fingerprint {
            return Err(RouterError::Conflict(
                "ordered Graph request fingerprint mismatch in Router replay target".into(),
            ));
        }
        let watermark_required = matches!(
            self.progress,
            OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(_)
                | OrderedEdgeBatchTargetProgressV1::RetirementPending(_)
        );
        if self.projection_watermark.is_some() != watermark_required {
            return Err(RouterError::Conflict(
                "ordered projection watermark does not match target progress".into(),
            ));
        }
        if let Some(watermark) = &self.projection_watermark
            && watermark.shard_id != self.request.target_shard_id
        {
            return Err(RouterError::Conflict(
                "ordered projection watermark targets a different shard".into(),
            ));
        }
        self.progress.validate()
    }
}

/// Durable ordered replay payload for a single Graph target.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterOrderedEdgeBatchReplayV1 {
    pub target: RouterOrderedEdgeBatchTargetV1,
}

/// Router-owned progress for one single-target ordered vertex batch.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum OrderedVertexBatchTargetProgressV1 {
    CanonicalPending,
    CanonicalCommitted(GraphOrderedVertexBatchReceiptV1),
    ProjectionPending(GraphOrderedVertexBatchReceiptV1),
    ProjectionAdvanced(GraphOrderedVertexBatchReceiptV1),
    RetirementPending(GraphOrderedVertexBatchReceiptV1),
}

impl OrderedVertexBatchTargetProgressV1 {
    fn receipt(&self) -> Option<&GraphOrderedVertexBatchReceiptV1> {
        match self {
            Self::CanonicalPending => None,
            Self::CanonicalCommitted(receipt)
            | Self::ProjectionPending(receipt)
            | Self::ProjectionAdvanced(receipt)
            | Self::RetirementPending(receipt) => Some(receipt),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        if let Some(receipt) = self.receipt() {
            receipt
                .validate()
                .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        }
        Ok(())
    }
}

/// Durable replay target for one ordered Graph vertex request.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterOrderedVertexBatchTargetV1 {
    pub graph_request_fingerprint: [u8; 32],
    pub request: OrderedVertexBatchGraphRequestV1,
    pub progress: OrderedVertexBatchTargetProgressV1,
    pub projection_watermark: Option<MutationTokenShard>,
}

impl RouterOrderedVertexBatchTargetV1 {
    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        let request = OrderedVertexBatchGraphRequest::V1(self.request.clone());
        let fingerprint = ordered_vertex_batch_graph_request_fingerprint(&request)
            .map_err(|error| RouterError::InvalidArgument(error.to_string()))?;
        if fingerprint != self.graph_request_fingerprint {
            return Err(RouterError::Conflict(
                "ordered vertex Graph request fingerprint mismatch in Router replay target".into(),
            ));
        }
        let watermark_required = matches!(
            self.progress,
            OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(_)
                | OrderedVertexBatchTargetProgressV1::RetirementPending(_)
        );
        if self.projection_watermark.is_some() != watermark_required {
            return Err(RouterError::Conflict(
                "ordered vertex projection watermark does not match target progress".into(),
            ));
        }
        if let Some(watermark) = &self.projection_watermark
            && watermark.shard_id != self.request.target_shard_id
        {
            return Err(RouterError::Conflict(
                "ordered vertex projection watermark targets a different shard".into(),
            ));
        }
        self.progress.validate()
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterOrderedVertexBatchReplayV1 {
    pub target: RouterOrderedVertexBatchTargetV1,
}

/// Router-owned progress for one single-target ordered mixed vertex/edge batch.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum OrderedMixedBatchTargetProgressV1 {
    CanonicalPending,
    CanonicalCommitted(GraphOrderedMixedBatchReceiptV1),
    ProjectionPending(GraphOrderedMixedBatchReceiptV1),
    ProjectionAdvanced(GraphOrderedMixedBatchReceiptV1),
    RetirementPending(GraphOrderedMixedBatchReceiptV1),
}

impl OrderedMixedBatchTargetProgressV1 {
    fn receipt(&self) -> Option<&GraphOrderedMixedBatchReceiptV1> {
        match self {
            Self::CanonicalPending => None,
            Self::CanonicalCommitted(receipt)
            | Self::ProjectionPending(receipt)
            | Self::ProjectionAdvanced(receipt)
            | Self::RetirementPending(receipt) => Some(receipt),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        if let Some(receipt) = self.receipt() {
            receipt
                .validate()
                .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        }
        Ok(())
    }
}

/// Durable replay target for one ordered Graph mixed request.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterOrderedMixedBatchTargetV1 {
    pub graph_request_fingerprint: [u8; 32],
    pub request: OrderedMixedBatchGraphRequestV1,
    pub progress: OrderedMixedBatchTargetProgressV1,
    pub projection_watermark: Option<MutationTokenShard>,
}

impl RouterOrderedMixedBatchTargetV1 {
    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        let request = OrderedMixedBatchGraphRequest::V1(self.request.clone());
        let fingerprint = ordered_mixed_batch_graph_request_fingerprint(&request)
            .map_err(|error| RouterError::InvalidArgument(error.to_string()))?;
        if fingerprint != self.graph_request_fingerprint {
            return Err(RouterError::Conflict(
                "ordered mixed Graph request fingerprint mismatch in Router replay target".into(),
            ));
        }
        let watermark_required = matches!(
            self.progress,
            OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(_)
                | OrderedMixedBatchTargetProgressV1::RetirementPending(_)
        );
        if self.projection_watermark.is_some() != watermark_required {
            return Err(RouterError::Conflict(
                "ordered mixed projection watermark does not match target progress".into(),
            ));
        }
        if let Some(watermark) = &self.projection_watermark
            && watermark.shard_id != self.request.target_shard_id
        {
            return Err(RouterError::Conflict(
                "ordered mixed projection watermark targets a different shard".into(),
            ));
        }
        self.progress.validate()
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterOrderedMixedBatchReplayV1 {
    pub target: RouterOrderedMixedBatchTargetV1,
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterMutationRecordV1 {
    pub mutation_id: MutationId,
    pub created_at_ns: u64,
    pub request_identity: RouterMutationRequestIdentityV1,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub completed_row_count: Option<u64>,
    pub routing_in_progress: bool,
    pub payload: RouterMutationPayloadV1,
    /// Wall-clock time the current routing lease was acquired (ADR 0029 Phase 4). Set
    /// whenever `routing_in_progress` is flipped to `true`; lets a retry reclaim a routing
    /// reservation whose owner trapped before persisting the dispatch envelope. `None` for
    /// records that never held an active routing lease (pre-Phase-4 records decode as `None`).
    #[serde(default)]
    pub routing_lease_ns: Option<u64>,
    /// Last recovery diagnostic (ADR 0029 Phase 4), surfaced by `mutation_status` for
    /// operators. `None` until a recovery step records why a saga cannot yet converge.
    #[serde(default)]
    pub last_error: Option<String>,
    /// **Irreversible** terminal-failure marker (ADR 0030 slice 6). `Some(error)` means the
    /// mutation failed permanently and must **not** be re-dispatched under this client key — a
    /// retry returns the stored error verbatim, so only a *new* client key may attempt the work
    /// again. Distinct from the *retryable* `Failed` lifecycle phase (`shards.is_empty() &&
    /// completed_row_count.is_none()`), which a same-key retry can still re-route. It is the only
    /// state the reclaim reconciler may use as Cancel grounds: it guarantees no later canonical
    /// dispatch for this mutation can still arrive and commit after the proof's absence read.
    #[serde(default)]
    pub terminal_failure: Option<String>,
}

/// Exhaustive payload for a V1 Router mutation saga. Exactly one variant is active at a time;
/// no parallel `shards`/`is_bulk`/`bulk_state` combination exists.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationPayloadV1 {
    /// Single-operation or multi-shard DML with one legacy seed blob per shard.
    Scalar { shards: Vec<RouterMutationShardV1> },
    /// Homogeneous bulk group replay where one legacy seed blob per shard is sufficient.
    LegacyBulk {
        total_ops: u32,
        shards: Vec<RouterMutationShardV1>,
    },
    /// Ordered per-operation typed seed replay for a single target shard. Router admission persists
    /// this exact relation before dispatch; retry and maintenance recovery reconstruct the same
    /// typed request without a blob-plus-typed dual authority.
    TypedSeedBulk(Box<TypedSeedBulkReplayV1>),
    /// Ordered edge batch admitted but not yet materialized into its Graph replay target.
    OrderedEdgeBatchRouting,
    /// Ordered edge batch with one durable Graph target and explicit progress.
    OrderedEdgeBatch(Box<RouterOrderedEdgeBatchReplayV1>),
    /// Ordered vertex batch admitted but not yet materialized into its Graph replay target.
    OrderedVertexBatchRouting,
    /// Ordered vertex batch with one durable Graph target and explicit progress.
    OrderedVertexBatch(Box<RouterOrderedVertexBatchReplayV1>),
    /// Ordered mixed batch admitted but not yet materialized into its Graph replay target.
    OrderedMixedBatchRouting,
    /// Ordered mixed batch with one durable Graph target and explicit progress.
    OrderedMixedBatch(Box<RouterOrderedMixedBatchReplayV1>),
    /// Terminal compacted form for both legacy and typed bulk. Typed completion retains the
    /// ordered per-operation row counts required to reproduce the original batch result; legacy
    /// completion uses an empty vector because that path only owns the aggregate row count.
    CompletedBulk {
        total_ops: u32,
        operation_row_counts: Vec<u64>,
    },
    /// Compacted ordered completion retained after projection convergence.
    CompletedOrderedEdgeBatch {
        receipt: GraphOrderedEdgeBatchReceiptV1,
        projection_watermark: MutationTokenShard,
    },
    /// Compacted ordered vertex completion retained after projection convergence.
    CompletedOrderedVertexBatch {
        receipt: GraphOrderedVertexBatchReceiptV1,
        projection_watermark: MutationTokenShard,
    },
    /// Compacted ordered mixed completion retained after projection convergence.
    CompletedOrderedMixedBatch {
        receipt: GraphOrderedMixedBatchReceiptV1,
        projection_watermark: MutationTokenShard,
    },
}

/// Durable typed seed replay state for one bulk group (ADR 0047).
impl RouterMutationPayloadV1 {
    /// Clear the shard vector of a `Scalar` payload. No-op for other variants.
    pub(crate) fn scalar_clear_shards(&mut self) {
        if let RouterMutationPayloadV1::Scalar { shards } = self {
            shards.clear();
        }
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TypedSeedBulkReplayV1 {
    /// Total number of logical operations in the bulk group.
    pub total_ops: u32,
    /// The single graph shard that owns every operation in this group.
    pub target: TypedSeedBulkTargetV1,
    /// Plan, catalog, and execution mode shared by all operations. Resolved label/property tables
    /// are owned at the `RouterMutationRecordV1` top level, not duplicated here.
    pub shared: TypedSeedBulkSharedHeaderV1,
    /// Ordered per-operation parameters and typed seed bindings.
    pub operations: Vec<TypedSeedBulkOperationV1>,
    /// Number of wire operations emitted for each logical operation. `None` is the legacy
    /// representation where every logical operation occupies exactly one wire operation.
    #[serde(default)]
    pub logical_operation_chunk_counts: Option<Vec<u32>>,
}

/// Outcome and identity of the single graph shard that owns every operation in a typed bulk
/// group. This is a dedicated stable type (not `RouterMutationShardV1`) so that typed replay can
/// never carry a legacy `seed_bindings_blob`.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TypedSeedBulkTargetV1 {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub completed: bool,
    pub projection_advanced: bool,
    pub row_count: u64,
    /// Ordered row counts copied from the terminal Graph journal before projection advancement.
    /// Empty until canonical completion; then its length must equal `total_ops`.
    pub operation_row_counts: Vec<u64>,
}

impl TypedSeedBulkTargetV1 {
    pub fn new(shard_id: ShardId, graph_canister: Principal) -> Self {
        Self {
            shard_id,
            graph_canister,
            completed: false,
            projection_advanced: false,
            row_count: 0,
            operation_row_counts: Vec::new(),
        }
    }
}

/// Shared header for a typed bulk group.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TypedSeedBulkSharedHeaderV1 {
    /// Per-graph key for ELEMENT_ID/path id encoding.
    pub element_id_encoding_key: [u8; 16],
    /// Serialized physical plan executed by every operation in the group.
    pub plan_blob: Vec<u8>,
    /// Execution mode for the group. Typed V1 is update-only.
    pub mode: GqlExecutionMode,
    /// Router-sourced indexed-property catalog for this operation (ADR 0023 D1/D3).
    pub indexed_properties: Option<gleaph_graph_kernel::index::IndexedPropertyCatalog>,
}

/// One typed bulk operation with decoded seed bindings.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TypedSeedBulkOperationV1 {
    /// Candid-encoded per-operation parameters.
    pub params: Vec<u8>,
    /// Router-resolved seed bindings for this operation.
    pub seed_bindings: SeedBindingsWire,
}

impl TypedSeedBulkReplayV1 {
    /// Validate the invariants required for a durable typed seed replay payload.
    ///
    /// - `mode == Update`
    /// - `total_ops` is non-zero
    /// - the chunk mapping covers `total_ops` logical operations and all wire operations
    /// - every operation has empty grouped `entries` and `complete_prefix_rows == true`
    /// - every `params` blob is at most `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES`
    pub(crate) fn validate(&self) -> Result<(), RouterError> {
        if self.shared.mode != GqlExecutionMode::Update {
            return Err(RouterError::InvalidArgument(
                "typed bulk V1 requires Update mode".into(),
            ));
        }
        if self.total_ops == 0 {
            return Err(RouterError::InvalidArgument(
                "typed bulk total_ops must be non-zero".into(),
            ));
        }
        let chunk_counts = self
            .logical_operation_chunk_counts
            .as_deref()
            .unwrap_or_else(|| &[]);
        if self.logical_operation_chunk_counts.is_some()
            && chunk_counts.len() != self.total_ops as usize
        {
            return Err(RouterError::InvalidArgument(
                "typed bulk logical operation chunk mapping must equal total_ops".into(),
            ));
        }
        if chunk_counts.iter().any(|&count| count == 0) {
            return Err(RouterError::InvalidArgument(
                "typed bulk logical operations must have at least one wire chunk".into(),
            ));
        }
        let wire_operation_count = if self.logical_operation_chunk_counts.is_some() {
            chunk_counts.iter().try_fold(0usize, |sum, &count| {
                sum.checked_add(count as usize).ok_or_else(|| {
                    RouterError::InvalidArgument("typed bulk chunk count overflow".into())
                })
            })?
        } else {
            self.total_ops as usize
        };
        if self.operations.len() != wire_operation_count {
            return Err(RouterError::InvalidArgument(
                "typed bulk wire operation count does not match chunk mapping".into(),
            ));
        }
        for (i, op) in self.operations.iter().enumerate() {
            if !op.seed_bindings.entries.is_empty() {
                return Err(RouterError::InvalidArgument(format!(
                    "typed bulk op {i} must not contain legacy grouped seed entries"
                )));
            }
            if !op.seed_bindings.complete_prefix_rows {
                return Err(RouterError::InvalidArgument(format!(
                    "typed bulk op {i} requires complete_prefix_rows=true"
                )));
            }
            if op.params.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
                return Err(RouterError::InvalidArgument(format!(
                    "typed bulk op {i} params exceed safe payload bound"
                )));
            }
        }
        Ok(())
    }
}

impl RouterMutationRecord {
    pub fn new(mutation_id: MutationId, created_at_ns: u64, request_fingerprint: Vec<u8>) -> Self {
        Self::V1(RouterMutationRecordV1 {
            mutation_id,
            created_at_ns,
            request_identity: RouterMutationRequestIdentityV1::PlanExecution {
                request_fingerprint,
            },
            resolved_labels: None,
            resolved_properties: None,
            completed_row_count: None,
            routing_in_progress: true,
            payload: RouterMutationPayloadV1::Scalar { shards: Vec::new() },
            routing_lease_ns: Some(created_at_ns),
            last_error: None,
            terminal_failure: None,
        })
    }

    /// Create a non-terminal typed seed bulk saga record with bounded replay state.
    ///
    /// The payload is validated and the encoded stable record is checked against the 2 MiB
    /// portable IC payload bound. No per-seed Candid encode is performed for the size check:
    /// the single full-record encode is the only serialization.
    #[cfg(any(test, feature = "pocket-ic-e2e"))]
    #[allow(dead_code)]
    pub(crate) fn new_typed_seed_bulk(
        mutation_id: MutationId,
        created_at_ns: u64,
        request_fingerprint: Vec<u8>,
        resolved_labels: Option<ResolvedLabelTable>,
        resolved_properties: Option<ResolvedPropertyTable>,
        replay: TypedSeedBulkReplayV1,
    ) -> Result<Self, RouterError> {
        replay.validate()?;
        if replay.target.completed
            || replay.target.projection_advanced
            || replay.target.row_count != 0
            || !replay.target.operation_row_counts.is_empty()
        {
            return Err(RouterError::InvalidArgument(
                "typed bulk V1 target must be pristine at admission".into(),
            ));
        }
        let mut record = Self::new(mutation_id, created_at_ns, request_fingerprint);
        {
            let v1 = record.as_v1_mut();
            v1.resolved_labels = resolved_labels;
            v1.resolved_properties = resolved_properties;
            v1.payload = RouterMutationPayloadV1::TypedSeedBulk(Box::new(replay));
        }
        if record.to_bytes().len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err(RouterError::InvalidArgument(
                "typed seed bulk record exceeds safe inter-canister payload bound".into(),
            ));
        }
        Ok(record)
    }

    pub(crate) fn as_v1(&self) -> &RouterMutationRecordV1 {
        match self {
            RouterMutationRecord::V1(v1) => v1,
        }
    }

    pub(crate) fn as_v1_mut(&mut self) -> &mut RouterMutationRecordV1 {
        match self {
            RouterMutationRecord::V1(v1) => v1,
        }
    }

    /// Return a reference to the active payload variant.
    pub fn payload(&self) -> &RouterMutationPayloadV1 {
        &self.as_v1().payload
    }

    pub(crate) fn payload_mut(&mut self) -> &mut RouterMutationPayloadV1 {
        &mut self.as_v1_mut().payload
    }

    /// Return the scalar/legacy shard slice, or an empty slice for non-shard payloads.
    pub fn shards(&self) -> &[RouterMutationShardV1] {
        match &self.as_v1().payload {
            RouterMutationPayloadV1::Scalar { shards }
            | RouterMutationPayloadV1::LegacyBulk { shards, .. } => shards,
            _ => &[],
        }
    }

    pub(crate) fn shards_mut(&mut self) -> Option<&mut Vec<RouterMutationShardV1>> {
        match self.payload_mut() {
            RouterMutationPayloadV1::Scalar { shards }
            | RouterMutationPayloadV1::LegacyBulk { shards, .. } => Some(shards),
            _ => None,
        }
    }

    /// `true` if the record is a bulk group (legacy, typed, or completed), as distinct from a
    /// scalar saga.
    pub fn is_bulk(&self) -> bool {
        matches!(
            self.payload(),
            RouterMutationPayloadV1::LegacyBulk { .. }
                | RouterMutationPayloadV1::TypedSeedBulk(_)
                | RouterMutationPayloadV1::OrderedEdgeBatchRouting
                | RouterMutationPayloadV1::OrderedEdgeBatch(_)
                | RouterMutationPayloadV1::OrderedVertexBatchRouting
                | RouterMutationPayloadV1::OrderedVertexBatch(_)
                | RouterMutationPayloadV1::OrderedMixedBatchRouting
                | RouterMutationPayloadV1::OrderedMixedBatch(_)
                | RouterMutationPayloadV1::CompletedBulk { .. }
                | RouterMutationPayloadV1::CompletedOrderedEdgeBatch { .. }
                | RouterMutationPayloadV1::CompletedOrderedVertexBatch { .. }
                | RouterMutationPayloadV1::CompletedOrderedMixedBatch { .. }
        )
    }

    /// Total operation count for bulk payloads; `None` for scalar.
    pub fn bulk_total_ops(&self) -> Option<u32> {
        match self.payload() {
            RouterMutationPayloadV1::LegacyBulk { total_ops, .. } => Some(*total_ops),
            RouterMutationPayloadV1::TypedSeedBulk(replay) => Some(replay.total_ops),
            RouterMutationPayloadV1::OrderedEdgeBatch(replay) => Some(
                replay
                    .target
                    .request
                    .items
                    .len()
                    .try_into()
                    .expect("ordered request item bound fits u32"),
            ),
            RouterMutationPayloadV1::OrderedVertexBatch(replay) => Some(
                replay
                    .target
                    .request
                    .items
                    .len()
                    .try_into()
                    .expect("ordered vertex request item bound fits u32"),
            ),
            RouterMutationPayloadV1::OrderedMixedBatch(replay) => Some(
                replay
                    .target
                    .request
                    .operations
                    .len()
                    .try_into()
                    .expect("ordered mixed request operation bound fits u32"),
            ),
            RouterMutationPayloadV1::CompletedBulk { total_ops, .. } => Some(*total_ops),
            _ => None,
        }
    }

    /// Return the typed bulk single target, if the payload is `TypedSeedBulk`.
    pub fn typed_target(&self) -> Option<&TypedSeedBulkTargetV1> {
        match self.payload() {
            RouterMutationPayloadV1::TypedSeedBulk(replay) => Some(&replay.target),
            _ => None,
        }
    }

    /// Ordered result cardinalities retained by a completed typed bulk record.
    pub fn completed_bulk_operation_row_counts(&self) -> Option<&[u64]> {
        match self.payload() {
            RouterMutationPayloadV1::CompletedBulk {
                operation_row_counts,
                ..
            } if !operation_row_counts.is_empty() => Some(operation_row_counts),
            _ => None,
        }
    }

    /// Mutable access to the typed bulk single target, for lifecycle tests and recovery steps.
    #[allow(dead_code)]
    pub(crate) fn typed_target_mut(&mut self) -> Option<&mut TypedSeedBulkTargetV1> {
        match self.payload_mut() {
            RouterMutationPayloadV1::TypedSeedBulk(replay) => Some(&mut replay.target),
            _ => None,
        }
    }

    /// `true` once the saga reaches a terminal phase. An irreversible `terminal_failure` takes
    /// priority (it forces [`Self::lifecycle_phase`] to `Failed`); otherwise terminality is the
    /// progress-derived `Completed`/`Failed`. Terminal records are the only ones eligible for TTL
    /// eviction (gated additionally by the non-terminal reservation count in ADR 0030 slice 6);
    /// non-terminal sagas are retained as recovery targets (ADR 0029 Phase 4).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.lifecycle_phase(),
            MutationLifecyclePhase::Completed | MutationLifecyclePhase::Failed
        )
    }

    /// `true` once the saga is **irreversibly** terminally failed (ADR 0030 slice 6): a same-key
    /// retry returns the stored error rather than re-dispatching.
    pub fn is_terminally_failed(&self) -> bool {
        self.as_v1().terminal_failure.is_some()
    }

    /// `true` while a unique-reservation-holding mutation is eligible to be flipped to irreversible
    /// `terminal_failure` by the reclaim reconciler (ADR 0030 slice 6): a durable dispatch envelope
    /// exists, **no** shard's canonical write has committed, routing is released, and it is not
    /// already terminal-failed.
    pub fn is_uncommitted_dispatch(&self) -> bool {
        self.as_v1().terminal_failure.is_none()
            && !self.as_v1().routing_in_progress
            && match &self.as_v1().payload {
                RouterMutationPayloadV1::Scalar { shards }
                | RouterMutationPayloadV1::LegacyBulk { shards, .. } => {
                    !shards.is_empty() && shards.iter().all(|shard| !shard.completed)
                }
                _ => false,
            }
    }

    /// Derive the ADR 0029 federated mutation lifecycle phase from the existing saga
    /// progress fields. This is a pure projection of the record's state, not a separate
    /// stored field, so the per-shard `completed`/`projection_advanced` flags and
    /// `completed_row_count` remain the single source of truth.
    pub fn lifecycle_phase(&self) -> MutationLifecyclePhase {
        // An irreversible terminal failure (ADR 0030 slice 6) is authoritative over the
        // progress-derived phase.
        if self.as_v1().terminal_failure.is_some() {
            return MutationLifecyclePhase::Failed;
        }
        // A pinned row count is the terminal "all canonical + all projections converged"
        // signal; the heavy shard fan-out is compacted away once it is set.
        if self.as_v1().completed_row_count.is_some() {
            return MutationLifecyclePhase::Completed;
        }
        if self.as_v1().routing_in_progress {
            return MutationLifecyclePhase::Routing;
        }
        if let RouterMutationPayloadV1::OrderedEdgeBatch(replay) = self.payload() {
            return match replay.target.progress {
                OrderedEdgeBatchTargetProgressV1::CanonicalPending => {
                    MutationLifecyclePhase::CanonicalPending
                }
                OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(_) => {
                    MutationLifecyclePhase::CanonicalCommitted
                }
                OrderedEdgeBatchTargetProgressV1::ProjectionPending(_)
                | OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(_)
                | OrderedEdgeBatchTargetProgressV1::RetirementPending(_) => {
                    MutationLifecyclePhase::ProjectionPending
                }
            };
        }
        if let RouterMutationPayloadV1::OrderedVertexBatch(replay) = self.payload() {
            return match replay.target.progress {
                OrderedVertexBatchTargetProgressV1::CanonicalPending => {
                    MutationLifecyclePhase::CanonicalPending
                }
                OrderedVertexBatchTargetProgressV1::CanonicalCommitted(_) => {
                    MutationLifecyclePhase::CanonicalCommitted
                }
                OrderedVertexBatchTargetProgressV1::ProjectionPending(_)
                | OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(_)
                | OrderedVertexBatchTargetProgressV1::RetirementPending(_) => {
                    MutationLifecyclePhase::ProjectionPending
                }
            };
        }
        if let RouterMutationPayloadV1::OrderedMixedBatch(replay) = self.payload() {
            return match replay.target.progress {
                OrderedMixedBatchTargetProgressV1::CanonicalPending => {
                    MutationLifecyclePhase::CanonicalPending
                }
                OrderedMixedBatchTargetProgressV1::CanonicalCommitted(_) => {
                    MutationLifecyclePhase::CanonicalCommitted
                }
                OrderedMixedBatchTargetProgressV1::ProjectionPending(_)
                | OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(_)
                | OrderedMixedBatchTargetProgressV1::RetirementPending(_) => {
                    MutationLifecyclePhase::ProjectionPending
                }
            };
        }
        // Scalar/legacy payload: derive from the shard envelope.
        let shards = self.shards();
        if !shards.is_empty() {
            if shards.iter().any(|shard| !shard.completed) {
                return MutationLifecyclePhase::CanonicalPending;
            }
            // Every shard's canonical write is durable from here on.
            if shards.iter().all(|shard| shard.projection_advanced) {
                return MutationLifecyclePhase::Completed;
            }
            if shards.iter().any(|shard| shard.projection_advanced) {
                return MutationLifecyclePhase::ProjectionPending;
            }
            return MutationLifecyclePhase::CanonicalCommitted;
        }
        // Typed payload: derive from the single dedicated target outcome.
        if let Some(target) = self.typed_target() {
            if !target.completed {
                return MutationLifecyclePhase::CanonicalPending;
            }
            if target.projection_advanced {
                return MutationLifecyclePhase::Completed;
            }
            return MutationLifecyclePhase::CanonicalCommitted;
        }
        // Routing was released without a durable dispatch envelope and no canonical
        // write committed (e.g. a validation/planning failure that freed the
        // reservation). The key is still re-reservable for a fresh attempt.
        MutationLifecyclePhase::Failed
    }
}

impl Storable for RouterMutationRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode RouterMutationRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode RouterMutationRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode RouterMutationRecord")
    }
}

/// Router mutation shard outcome (ADR 0044).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterMutationShardV1 {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub seed_bindings_blob: Option<Vec<u8>>,
    pub completed: bool,
    pub projection_advanced: bool,
    pub row_count: u64,
}

impl RouterMutationShardV1 {
    pub fn new(
        shard_id: ShardId,
        graph_canister: Principal,
        seed_bindings_blob: Option<Vec<u8>>,
    ) -> Self {
        Self {
            shard_id,
            graph_canister,
            seed_bindings_blob,
            completed: false,
            projection_advanced: false,
            row_count: 0,
        }
    }

    // Field accessors.
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }
    pub fn graph_canister(&self) -> Principal {
        self.graph_canister
    }
    pub fn seed_bindings_blob(&self) -> &Option<Vec<u8>> {
        &self.seed_bindings_blob
    }
    pub fn completed(&self) -> bool {
        self.completed
    }
    pub fn projection_advanced(&self) -> bool {
        self.projection_advanced
    }
    pub fn row_count(&self) -> u64 {
        self.row_count
    }
    pub fn set_completed(&mut self, completed: bool) {
        self.completed = completed;
    }
    pub fn set_projection_advanced(&mut self, advanced: bool) {
        self.projection_advanced = advanced;
    }
    pub fn set_row_count(&mut self, row_count: u64) {
        self.row_count = row_count;
    }
    pub fn set_seed_bindings_blob(&mut self, blob: Option<Vec<u8>>) {
        self.seed_bindings_blob = blob;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::store::compact_completed_record;
    use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedEdgeBatchReceiptV1, GraphOrderedVertexBatchReceiptV1,
        OrderedEdgeBatchGraphItemV1, OrderedVertexBatchGraphItemV1, OrderedVertexBatchGraphRequest,
        OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedProperty,
        ResolvedPropertyTable, ResolvedVertexLabel, SeedBindingEntry, SeedRowWire,
        ordered_edge_batch_graph_request_fingerprint,
        ordered_vertex_batch_graph_request_fingerprint,
    };
    use ic_stable_structures::Storable;

    #[test]
    fn router_mutation_record_round_trips_through_storable() {
        let record = RouterMutationRecord::new(1, 42, vec![9, 8]);
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert_eq!(decoded.as_v1().mutation_id, 1);
        assert!(decoded.as_v1().routing_in_progress);
    }

    #[test]
    fn ordered_request_identity_round_trips_without_a_payload_variant() {
        let mut record = RouterMutationRecord::new(2, 42, vec![9, 8]);
        record.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::OrderedEdgeBatch {
            public_fingerprint: [7; 32],
            public_item_count: 3,
        };
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.as_v1().request_identity.request_fingerprint(),
            [7; 32]
        );
        assert_eq!(
            decoded.as_v1().request_identity.public_item_count(),
            Some(3)
        );
    }

    #[test]
    fn ordered_vertex_router_replay_round_trips_and_validates() {
        let request = OrderedVertexBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(1),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::from_slice(&[1]),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: vec![7],
                resolved_initial_properties: Vec::new(),
            }],
        };
        let request_envelope = OrderedVertexBatchGraphRequest::V1(request.clone());
        let fingerprint = ordered_vertex_batch_graph_request_fingerprint(&request_envelope)
            .expect("ordered vertex request fingerprint");
        let target = RouterOrderedVertexBatchTargetV1 {
            graph_request_fingerprint: fingerprint,
            request,
            progress: OrderedVertexBatchTargetProgressV1::CanonicalCommitted(
                GraphOrderedVertexBatchReceiptV1 {
                    logical_vertex_count: 1,
                    emitted_delta_first_seq: None,
                    emitted_delta_last_seq: None,
                    hot_forward_vertices: Vec::new(),
                },
            ),
            projection_watermark: None,
        };
        target
            .validate()
            .expect("valid ordered vertex replay target");

        let mut record = RouterMutationRecord::new(3, 42, vec![9; 32]);
        record.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::OrderedVertexBatch {
            public_fingerprint: [7; 32],
            public_item_count: 1,
        };
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().payload = RouterMutationPayloadV1::OrderedVertexBatch(Box::new(
            RouterOrderedVertexBatchReplayV1 { target },
        ));
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalCommitted
        );
        assert_eq!(decoded.bulk_total_ops(), Some(1));
    }

    #[test]
    fn ordered_router_replay_target_validates_request_and_progress() {
        let request = OrderedEdgeBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(1),
            target_shard_id: ShardId::new(2),
            target_graph_canister: Principal::from_slice(&[1]),
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedEdgeBatchGraphItemV1 {
                source_local_vertex_id: 10,
                target_local_vertex_id: 11,
                directed: true,
                catalog_edge_label_id: None,
                inline_property_bytes: Vec::new(),
                resolved_initial_edge_properties: Vec::new(),
            }],
        };
        let request_envelope = OrderedEdgeBatchGraphRequest::V1(request.clone());
        let fingerprint = ordered_edge_batch_graph_request_fingerprint(&request_envelope)
            .expect("ordered request fingerprint");
        let target = RouterOrderedEdgeBatchTargetV1 {
            graph_request_fingerprint: fingerprint,
            request,
            progress: OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(
                GraphOrderedEdgeBatchReceiptV1 {
                    logical_edge_count: 1,
                    emitted_delta_first_seq: None,
                    emitted_delta_last_seq: None,
                    hot_forward_vertices: Vec::new(),
                },
            ),
            projection_watermark: None,
        };
        target.validate().expect("valid ordered replay target");
    }

    fn shard(shard_id: u32, completed: bool, projection_advanced: bool) -> RouterMutationShardV1 {
        let mut s = RouterMutationShardV1::new(ShardId(shard_id), Principal::anonymous(), None);
        s.set_completed(completed);
        s.set_projection_advanced(projection_advanced);
        s
    }

    fn record_with_shards(shards: Vec<RouterMutationShardV1>) -> RouterMutationRecord {
        let mut record = RouterMutationRecord::new(1, 0, Vec::new());
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().payload = RouterMutationPayloadV1::Scalar { shards };
        record
    }

    // ADR 0029 Phase 0 characterization: each saga progress state maps to exactly one
    // lifecycle phase, derived from the existing fields (no new stored status).
    #[test]
    fn lifecycle_phase_tracks_saga_progress() {
        // Routing: reservation taken, no envelope persisted yet.
        let routing = RouterMutationRecord::new(1, 0, Vec::new());
        assert_eq!(routing.lifecycle_phase(), MutationLifecyclePhase::Routing);

        // Canonical pending: at least one shard outcome unknown.
        let canonical_pending =
            record_with_shards(vec![shard(0, true, true), shard(1, false, false)]);
        assert_eq!(
            canonical_pending.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalPending
        );

        // Canonical committed: all shards durable, no projection advanced.
        let canonical_committed =
            record_with_shards(vec![shard(0, true, false), shard(1, true, false)]);
        assert_eq!(
            canonical_committed.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalCommitted
        );

        // Projection pending: canonical durable, some (not all) projections caught up.
        let projection_pending =
            record_with_shards(vec![shard(0, true, true), shard(1, true, false)]);
        assert_eq!(
            projection_pending.lifecycle_phase(),
            MutationLifecyclePhase::ProjectionPending
        );

        // Completed: all shards canonical + projected.
        let completed_via_shards =
            record_with_shards(vec![shard(0, true, true), shard(1, true, true)]);
        assert_eq!(
            completed_via_shards.lifecycle_phase(),
            MutationLifecyclePhase::Completed
        );

        // Completed: compacted record with a pinned row count.
        let mut completed_compacted = RouterMutationRecord::new(1, 0, Vec::new());
        completed_compacted.as_v1_mut().routing_in_progress = false;
        completed_compacted.as_v1_mut().completed_row_count = Some(7);
        assert_eq!(
            completed_compacted.lifecycle_phase(),
            MutationLifecyclePhase::Completed
        );

        // Failed: routing released with no durable shard envelope.
        let failed = record_with_shards(Vec::new());
        assert_eq!(failed.lifecycle_phase(), MutationLifecyclePhase::Failed);
    }

    // ADR 0029 Phase 0 contract: Router must never report `Completed` while any required
    // canonical shard outcome or projection watermark is still outstanding.
    #[test]
    fn lifecycle_phase_never_completes_with_outstanding_work() {
        let unfinished_states = [
            record_with_shards(vec![shard(0, false, false)]),
            record_with_shards(vec![shard(0, true, false)]),
            record_with_shards(vec![shard(0, true, true), shard(1, false, false)]),
            record_with_shards(vec![shard(0, true, true), shard(1, true, false)]),
        ];
        for record in unfinished_states {
            assert_ne!(
                record.lifecycle_phase(),
                MutationLifecyclePhase::Completed,
                "incomplete saga must not report Completed: {:?}",
                record.shards()
            );
        }
    }

    fn typed_seed_replay(total_ops: u32) -> TypedSeedBulkReplayV1 {
        typed_seed_replay_with_rows(total_ops, vec![])
    }

    fn typed_seed_replay_with_rows(
        total_ops: u32,
        rows: Vec<SeedRowWire>,
    ) -> TypedSeedBulkReplayV1 {
        let target = TypedSeedBulkTargetV1::new(ShardId(0), Principal::anonymous());
        let shared = TypedSeedBulkSharedHeaderV1 {
            element_id_encoding_key: [0u8; 16],
            plan_blob: vec![1, 2, 3],
            mode: GqlExecutionMode::Update,
            indexed_properties: None,
        };
        let operations = (0..total_ops)
            .map(|i| TypedSeedBulkOperationV1 {
                params: vec![i as u8],
                seed_bindings: SeedBindingsWire {
                    entries: vec![],
                    rows: rows.clone(),
                    complete_prefix_rows: true,
                },
            })
            .collect();
        TypedSeedBulkReplayV1 {
            total_ops,
            target,
            shared,
            operations,
            logical_operation_chunk_counts: None,
        }
    }

    fn sample_resolved_labels() -> ResolvedLabelTable {
        ResolvedLabelTable {
            vertex: vec![ResolvedVertexLabel {
                name: "Person".into(),
                id: VertexLabelId::from_raw(1),
            }],
            edge: vec![],
        }
    }

    fn sample_resolved_properties() -> ResolvedPropertyTable {
        ResolvedPropertyTable {
            properties: vec![ResolvedProperty {
                name: "name".into(),
                id: PropertyId::from_raw(1),
            }],
        }
    }

    fn typed_seed_bulk_record(total_ops: u32) -> RouterMutationRecord {
        RouterMutationRecord::new_typed_seed_bulk(
            1,
            0,
            Vec::new(),
            Some(sample_resolved_labels()),
            Some(sample_resolved_properties()),
            typed_seed_replay(total_ops),
        )
        .expect("valid typed seed bulk record")
    }

    #[test]
    fn legacy_bulk_payload_round_trips() {
        let mut record = RouterMutationRecord::new(1, 0, Vec::new());
        record.as_v1_mut().payload = RouterMutationPayloadV1::LegacyBulk {
            total_ops: 2,
            shards: vec![RouterMutationShardV1::new(
                ShardId(0),
                Principal::anonymous(),
                None,
            )],
        };
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert!(decoded.is_bulk());
        assert_eq!(decoded.bulk_total_ops(), Some(2));
    }

    #[test]
    fn typed_seed_bulk_payload_round_trips() {
        let record = typed_seed_bulk_record(3);
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert!(decoded.is_bulk());
        assert_eq!(decoded.bulk_total_ops(), Some(3));
        assert_eq!(decoded.shards().len(), 0);
        assert!(decoded.as_v1().resolved_labels.is_some());
        assert!(decoded.as_v1().resolved_properties.is_some());
        assert_eq!(
            decoded.as_v1().resolved_labels.as_ref().unwrap().vertex[0].name,
            "Person"
        );
    }

    #[test]
    fn typed_seed_bulk_lifecycle_stages() {
        let mut record = typed_seed_bulk_record(1);
        record.as_v1_mut().routing_in_progress = false;
        assert_eq!(
            record.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalPending
        );

        record.typed_target_mut().unwrap().completed = true;
        assert_eq!(
            record.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalCommitted
        );

        record.typed_target_mut().unwrap().projection_advanced = true;
        assert_eq!(record.lifecycle_phase(), MutationLifecyclePhase::Completed);

        // A pinned row count overrides the payload-derived terminal signal after compaction.
        record.as_v1_mut().completed_row_count = Some(7);
        assert_eq!(record.lifecycle_phase(), MutationLifecyclePhase::Completed);
    }

    #[test]
    fn typed_payload_is_not_uncommitted_dispatch() {
        let mut record = typed_seed_bulk_record(1);
        record.as_v1_mut().routing_in_progress = false;
        assert!(!record.is_uncommitted_dispatch());
        assert_eq!(
            record.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalPending
        );
    }

    #[test]
    fn completed_bulk_is_terminal() {
        let mut record = RouterMutationRecord::new(1, 0, Vec::new());
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().completed_row_count = Some(7);
        record.as_v1_mut().payload = RouterMutationPayloadV1::CompletedBulk {
            total_ops: 7,
            operation_row_counts: Vec::new(),
        };
        assert_eq!(record.lifecycle_phase(), MutationLifecyclePhase::Completed);
        assert!(record.is_bulk());
        assert!(!record.is_uncommitted_dispatch());
    }

    #[test]
    fn completed_bulk_typed_seed_drops_replay_and_resolved_tables() {
        let mut record = typed_seed_bulk_record(5);
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().completed_row_count = Some(5);
        record.typed_target_mut().unwrap().operation_row_counts = vec![1; 5];
        compact_completed_record(&mut record);
        assert!(matches!(
            record.payload(),
            RouterMutationPayloadV1::CompletedBulk {
                total_ops: 5,
                operation_row_counts,
            } if operation_row_counts == &[1; 5]
        ));
        assert_eq!(
            record.completed_bulk_operation_row_counts(),
            Some(&[1; 5][..])
        );
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert!(record.as_v1().resolved_labels.is_none());
        assert!(record.as_v1().resolved_properties.is_none());
        assert_eq!(record.lifecycle_phase(), MutationLifecyclePhase::Completed);
    }

    #[test]
    fn typed_seed_bulk_validation_rejects_zero_ops() {
        let mut replay = typed_seed_replay(1);
        replay.total_ops = 0;
        assert!(replay.validate().is_err());
    }

    #[test]
    fn typed_seed_bulk_validation_rejects_ops_count_mismatch() {
        let mut replay = typed_seed_replay(2);
        replay.operations.pop();
        assert!(replay.validate().is_err());
    }

    #[test]
    fn typed_seed_bulk_validation_rejects_query_mode() {
        let mut replay = typed_seed_replay(1);
        replay.shared.mode = GqlExecutionMode::Query;
        assert!(replay.validate().is_err());
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay)
                .is_err()
        );
    }

    #[test]
    fn typed_seed_bulk_validation_rejects_grouped_entries() {
        let mut replay = typed_seed_replay(1);
        replay.operations[0]
            .seed_bindings
            .entries
            .push(SeedBindingEntry {
                variable: "x".into(),
                local_vertex_ids: vec![1],
                local_edge_postings: vec![],
            });
        assert!(replay.validate().is_err());
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay)
                .is_err()
        );
    }

    #[test]
    fn typed_seed_bulk_validation_rejects_incomplete_prefix_rows() {
        let mut replay = typed_seed_replay(1);
        replay.operations[0].seed_bindings.complete_prefix_rows = false;
        assert!(replay.validate().is_err());
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay)
                .is_err()
        );
    }

    #[test]
    fn typed_seed_bulk_validation_accepts_zero_rows() {
        let replay = typed_seed_replay(1);
        assert!(replay.validate().is_ok());
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay).is_ok()
        );
    }

    #[test]
    fn typed_seed_bulk_validation_accepts_rows_without_fixed_cardinality_cap() {
        let mut replay = typed_seed_replay(1);
        replay.operations[0].seed_bindings.rows = (0..1025)
            .map(|_| SeedRowWire {
                vertex_bindings: vec![],
                float64_bindings: vec![],
            })
            .collect();
        assert!(replay.validate().is_ok());
    }

    #[test]
    fn typed_seed_bulk_validation_rejects_oversized_params() {
        let mut replay = typed_seed_replay(1);
        replay.operations[0].params = vec![0u8; MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES + 1];
        assert!(replay.validate().is_err());
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay)
                .is_err()
        );
    }

    #[test]
    fn typed_seed_bulk_validation_accepts_logical_to_wire_chunk_mapping() {
        let mut replay = typed_seed_replay(2);
        replay.operations.push(replay.operations[1].clone());
        replay.logical_operation_chunk_counts = Some(vec![2, 1]);
        replay.validate().expect("chunked replay is valid");

        replay.logical_operation_chunk_counts = Some(vec![2, 2]);
        assert!(replay.validate().is_err());
    }

    #[test]
    fn typed_seed_bulk_record_rejects_non_pristine_target() {
        let mut replay = typed_seed_replay(1);
        replay.target.completed = true;
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay)
                .is_err()
        );
    }

    #[test]
    fn typed_seed_bulk_record_rejects_exceeding_safe_payload_bound() {
        let mut replay = typed_seed_replay_with_rows(
            1024,
            vec![SeedRowWire {
                vertex_bindings: vec![],
                float64_bindings: vec![],
            }],
        );
        // Each params is within the per-operation limit, but the full record still exceeds the
        // portable inter-canister payload bound because of the cumulative Candid overhead.
        for op in replay.operations.iter_mut() {
            op.params = vec![0u8; 3 * 1024];
        }
        assert!(
            RouterMutationRecord::new_typed_seed_bulk(1, 0, Vec::new(), None, None, replay)
                .is_err()
        );
    }

    #[test]
    fn typed_seed_bulk_target_has_no_blob_field() {
        // The target type has no `seed_bindings_blob` accessor and cannot be constructed from a
        // legacy shard envelope. This is a compile-time invariant; the test just exercises the
        // dedicated constructor and field set.
        let target = TypedSeedBulkTargetV1::new(ShardId(7), Principal::anonymous());
        assert_eq!(target.shard_id, ShardId(7));
        assert_eq!(target.graph_canister, Principal::anonymous());
        assert!(!target.completed);
    }
}
