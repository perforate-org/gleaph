//! Router-owned label stats aggregates and client mutation records (ADR 0015).

use crate::state::RouterError;
use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    GraphOrderedEdgeBatchReceiptV1, GraphOrderedMixedBatchReceiptV1,
    GraphOrderedVertexBatchReceiptV1, MutationId, MutationLifecyclePhase, MutationTokenShard,
    OrderedEdgeBatchGraphRequest, OrderedEdgeBatchGraphRequestV1, OrderedMixedBatchGraphRequest,
    OrderedMixedBatchGraphRequestV1, OrderedVertexBatchGraphRequest,
    OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedPropertyTable,
    ordered_edge_batch_graph_request_fingerprint, ordered_mixed_batch_graph_request_fingerprint,
    ordered_vertex_batch_graph_request_fingerprint,
};
use gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Maximum UTF-8 byte length retained for a mutation recovery diagnostic.
///
/// Public status response admission reserves this full bound.
pub(crate) const MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES: usize = 4 * 1024;

pub(crate) fn bound_mutation_recovery_diagnostic(mut diagnostic: String) -> String {
    if diagnostic.len() <= MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES {
        return diagnostic;
    }
    let mut end = MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES;
    while !diagnostic.is_char_boundary(end) {
        end -= 1;
    }
    diagnostic.truncate(end);
    diagnostic
}

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

/// Versioned Router mutation saga record (ADR 0029 and ADR 0057).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationRecord {
    V1(RouterMutationRecordV1),
}

/// Request identity owned by the Router mutation record.
///
/// Scalar GQL/prepared mutations use `PlanExecution`; ordered atomic-insert and durable bulk-load
/// identities are exhaustive sibling variants, so no family can reuse another fingerprint field.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationRequestIdentityV1 {
    /// Unit identity for an ADR 0057 durable bulk-load job.  The graph-scoped
    /// `(caller, graph_id, client_key)` remains the canonical map key; this variant carries no
    /// duplicate identity fields.
    BulkLoadJob,
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

/// Internal family classification derived independently from both durable authorities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouterMutationFamilyV1 {
    BulkLoadJob,
    PlanExecution,
    OrderedEdgeBatch,
    OrderedVertexBatch,
    OrderedMixedBatch,
}

impl RouterMutationRequestIdentityV1 {
    fn mutation_family(&self) -> RouterMutationFamilyV1 {
        match self {
            Self::BulkLoadJob => RouterMutationFamilyV1::BulkLoadJob,
            Self::PlanExecution { .. } => RouterMutationFamilyV1::PlanExecution,
            Self::OrderedEdgeBatch { .. } => RouterMutationFamilyV1::OrderedEdgeBatch,
            Self::OrderedVertexBatch { .. } => RouterMutationFamilyV1::OrderedVertexBatch,
            Self::OrderedMixedBatch { .. } => RouterMutationFamilyV1::OrderedMixedBatch,
        }
    }

    pub fn request_fingerprint(&self) -> &[u8] {
        match self {
            Self::BulkLoadJob => &BULK_LOAD_JOB_IDENTITY_FINGERPRINT,
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
            Self::BulkLoadJob => None,
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

/// Pinned physical target for one durable bulk-load job.  It is internal Router recovery data and
/// is never projected through the public Candid status surface.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BulkLoadTargetV1 {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
}

impl BulkLoadTargetV1 {
    pub fn validate(&self) -> Result<(), RouterError> {
        if self.graph_canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "bulk-load target graph canister must not be anonymous".into(),
            ));
        }
        Ok(())
    }
}

/// Bounded Finalize state.  V1 has one verification stage; the enum keeps the persisted state
/// exhaustive so a future stage cannot be smuggled in as a parallel flag.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkLoadFinalizeStageV1 {
    VerifyReceipts,
}

/// Durable parent lifecycle for ADR 0057.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum BulkLoadLifecycleV1 {
    Open,
    AppendPending {
        chunk_index: u32,
        fingerprint: [u8; 32],
        child_mutation_id: MutationId,
    },
    FinalizePending {
        stage: BulkLoadFinalizeStageV1,
        cursor: u32,
    },
    AbortPending {
        active_chunk: u32,
    },
    Completed,
    Aborted,
    Failed {
        reason: String,
    },
}

impl BulkLoadLifecycleV1 {
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Aborted | Self::Failed { .. })
    }

    pub const fn active_child(&self) -> Option<(u32, MutationId)> {
        match self {
            Self::AppendPending {
                chunk_index,
                child_mutation_id,
                ..
            } => Some((*chunk_index, *child_mutation_id)),
            Self::AbortPending { active_chunk } => Some((*active_chunk, 0)),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), RouterError> {
        match self {
            Self::AppendPending {
                chunk_index,
                child_mutation_id,
                ..
            } => {
                if *child_mutation_id == 0 {
                    return Err(RouterError::Conflict(
                        "bulk-load AppendPending child mutation id must be non-zero".into(),
                    ));
                }
                let _ = chunk_index;
            }
            Self::FinalizePending { stage, .. } => match stage {
                BulkLoadFinalizeStageV1::VerifyReceipts => {}
            },
            Self::AbortPending { .. } => {}
            Self::Failed { reason } if reason.is_empty() => {
                return Err(RouterError::Conflict(
                    "bulk-load Failed state requires a reason".into(),
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

/// Router-owned aggregate and lifecycle payload stored in the parent mutation record.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BulkLoadCoordinatorV1 {
    pub target: BulkLoadTargetV1,
    pub logical_operation_count: u64,
    pub logical_vertex_count: u64,
    pub logical_edge_count: u64,
    pub next_chunk_index: u32,
    pub committed_chunk_count: u32,
    pub completed_chunk_count: u32,
    pub lifecycle: BulkLoadLifecycleV1,
    /// Orthogonal receipt cleanup cursor. It does not replace or erase the terminal lifecycle.
    pub receipt_gc_cursor: Option<u32>,
}

impl BulkLoadCoordinatorV1 {
    pub fn new(target: BulkLoadTargetV1) -> Self {
        Self {
            target,
            logical_operation_count: 0,
            logical_vertex_count: 0,
            logical_edge_count: 0,
            next_chunk_index: 0,
            committed_chunk_count: 0,
            completed_chunk_count: 0,
            lifecycle: BulkLoadLifecycleV1::Open,
            receipt_gc_cursor: None,
        }
    }

    pub fn validate(&self) -> Result<(), RouterError> {
        self.target.validate()?;
        self.lifecycle.validate()?;
        let expected = self
            .logical_vertex_count
            .checked_add(self.logical_edge_count)
            .ok_or_else(|| RouterError::Conflict("bulk-load aggregate count overflow".into()))?;
        if expected != self.logical_operation_count {
            return Err(RouterError::Conflict(
                "bulk-load aggregate counts must sum to logical operation count".into(),
            ));
        }
        if self.committed_chunk_count > self.next_chunk_index.saturating_add(1)
            || self.completed_chunk_count > self.committed_chunk_count
        {
            return Err(RouterError::Conflict(
                "bulk-load aggregate chunk counters are not monotonic".into(),
            ));
        }
        let active_child = matches!(
            self.lifecycle,
            BulkLoadLifecycleV1::AppendPending { .. } | BulkLoadLifecycleV1::AbortPending { .. }
        );
        if active_child {
            if self.completed_chunk_count != self.next_chunk_index
                || self.committed_chunk_count < self.next_chunk_index
            {
                return Err(RouterError::Conflict(
                    "bulk-load active-child counters do not describe the accepted prefix".into(),
                ));
            }
        } else if self.committed_chunk_count != self.next_chunk_index
            || self.completed_chunk_count != self.next_chunk_index
        {
            return Err(RouterError::Conflict(
                "bulk-load inactive lifecycle counters do not describe a completed prefix".into(),
            ));
        }
        if let BulkLoadLifecycleV1::FinalizePending { cursor, .. } = self.lifecycle
            && cursor > self.next_chunk_index
        {
            return Err(RouterError::Conflict(
                "bulk-load Finalize cursor exceeds accepted chunk prefix".into(),
            ));
        }
        if let BulkLoadLifecycleV1::AbortPending { active_chunk } = self.lifecycle
            && active_chunk != self.next_chunk_index
        {
            return Err(RouterError::Conflict(
                "bulk-load AbortPending active chunk is not the next accepted index".into(),
            ));
        }
        if self.lifecycle.is_terminal() {
            if self.receipt_gc_cursor.is_none()
                && self.committed_chunk_count != self.next_chunk_index
            {
                return Err(RouterError::Conflict(
                    "terminal bulk-load lifecycle requires committed prefix counters".into(),
                ));
            }
        } else if self.receipt_gc_cursor.is_some() {
            return Err(RouterError::Conflict(
                "bulk-load receipt GC cursor requires terminal lifecycle".into(),
            ));
        }
        let encoded = Encode!(self).map_err(|error| {
            RouterError::Internal(format!("bulk-load coordinator encode failed: {error}"))
        })?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err(RouterError::InvalidArgument(
                "bulk-load coordinator exceeds the safe stable payload bound".into(),
            ));
        }
        Ok(())
    }
}

/// SHA-256(`gleaph:bulk-load-job:v1\0`), the sole Router parent identity fingerprint.
pub const BULK_LOAD_JOB_IDENTITY_FINGERPRINT: [u8; 32] = [
    0x3a, 0x8c, 0x1f, 0x7f, 0x39, 0xa5, 0x0f, 0x2a, 0x59, 0x0d, 0x7b, 0x40, 0x6e, 0x9f, 0x9c, 0xe8,
    0xcd, 0xad, 0x2c, 0xed, 0xa9, 0xdf, 0x41, 0x3e, 0x70, 0xec, 0x6b, 0xed, 0x7d, 0x2e, 0xbf, 0x61,
];

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
    /// Sole retention anchor for every terminal mutation family. Non-terminal records must keep
    /// this unset; terminal records set it exactly once at the first irreversible transition.
    #[serde(default)]
    pub terminal_at_ns: Option<u64>,
}

/// Exhaustive payload for a V1 Router mutation saga. Exactly one variant is active at a time;
/// no parallel `shards`/`is_bulk`/`bulk_state` combination exists.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationPayloadV1 {
    /// Durable graph-scoped Router bulk-load coordinator (ADR 0057). Chunk envelopes live in the
    /// dedicated MemoryId 49 receipt map; this payload owns only lifecycle and aggregate state.
    BulkLoadCoordinator(Box<BulkLoadCoordinatorV1>),
    /// Single-operation or multi-shard DML with one encoded seed relation per shard.
    Scalar { shards: Vec<RouterMutationShardV1> },
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

impl RouterMutationPayloadV1 {
    fn mutation_family(&self) -> RouterMutationFamilyV1 {
        match self {
            Self::BulkLoadCoordinator(_) => RouterMutationFamilyV1::BulkLoadJob,
            Self::Scalar { .. } => RouterMutationFamilyV1::PlanExecution,
            Self::OrderedEdgeBatchRouting
            | Self::OrderedEdgeBatch(_)
            | Self::CompletedOrderedEdgeBatch { .. } => RouterMutationFamilyV1::OrderedEdgeBatch,
            Self::OrderedVertexBatchRouting
            | Self::OrderedVertexBatch(_)
            | Self::CompletedOrderedVertexBatch { .. } => {
                RouterMutationFamilyV1::OrderedVertexBatch
            }
            Self::OrderedMixedBatchRouting
            | Self::OrderedMixedBatch(_)
            | Self::CompletedOrderedMixedBatch { .. } => RouterMutationFamilyV1::OrderedMixedBatch,
        }
    }

    /// Clear the shard vector of a `Scalar` payload. No-op for other variants.
    pub(crate) fn scalar_clear_shards(&mut self) {
        if let RouterMutationPayloadV1::Scalar { shards } = self {
            shards.clear();
        }
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
            terminal_at_ns: None,
        })
    }

    /// Construct the final durable parent row for a graph-scoped bulk-load job.  The caller must
    /// perform graph/key and target admission before invoking this constructor; this function
    /// validates the payload and encoded stable-record bound so the store can co-write it with the
    /// mutation counter without a recoverable post-write error.
    pub fn new_bulk_load(
        mutation_id: MutationId,
        created_at_ns: u64,
        coordinator: BulkLoadCoordinatorV1,
    ) -> Result<Self, RouterError> {
        if mutation_id == 0 {
            return Err(RouterError::IdExhausted("mutation_id".into()));
        }
        coordinator.validate()?;
        let record = Self::V1(RouterMutationRecordV1 {
            mutation_id,
            created_at_ns,
            request_identity: RouterMutationRequestIdentityV1::BulkLoadJob,
            resolved_labels: None,
            resolved_properties: None,
            completed_row_count: None,
            routing_in_progress: false,
            payload: RouterMutationPayloadV1::BulkLoadCoordinator(Box::new(coordinator)),
            routing_lease_ns: None,
            last_error: None,
            terminal_failure: None,
            terminal_at_ns: None,
        });
        if record.to_bytes().len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err(RouterError::InvalidArgument(
                "bulk-load parent record exceeds the safe payload bound".into(),
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

    fn mutation_family(&self) -> Result<RouterMutationFamilyV1, RouterError> {
        let identity_family = self.as_v1().request_identity.mutation_family();
        let payload_family = self.payload().mutation_family();
        if identity_family != payload_family {
            return Err(RouterError::Conflict(
                "mutation record request identity and payload families disagree".into(),
            ));
        }
        Ok(identity_family)
    }

    /// Validate that both durable family authorities identify a GQL/prepared mutation.
    pub fn ensure_gql_mutation_family(&self) -> Result<(), RouterError> {
        match self.mutation_family()? {
            RouterMutationFamilyV1::BulkLoadJob => Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into(),
            )),
            RouterMutationFamilyV1::PlanExecution => Ok(()),
            RouterMutationFamilyV1::OrderedEdgeBatch
            | RouterMutationFamilyV1::OrderedVertexBatch
            | RouterMutationFamilyV1::OrderedMixedBatch => Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into(),
            )),
        }
    }

    /// Validate that both durable family authorities identify one atomic-insert subtype.
    pub fn ensure_atomic_insert_family(&self) -> Result<(), RouterError> {
        match self.mutation_family()? {
            RouterMutationFamilyV1::BulkLoadJob => Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into(),
            )),
            RouterMutationFamilyV1::PlanExecution => Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into(),
            )),
            RouterMutationFamilyV1::OrderedEdgeBatch
            | RouterMutationFamilyV1::OrderedVertexBatch
            | RouterMutationFamilyV1::OrderedMixedBatch => Ok(()),
        }
    }

    /// Validate that both durable family authorities identify an ADR 0057 bulk-load job.
    pub fn ensure_bulk_load_family(&self) -> Result<(), RouterError> {
        match self.mutation_family()? {
            RouterMutationFamilyV1::BulkLoadJob => Ok(()),
            RouterMutationFamilyV1::PlanExecution
            | RouterMutationFamilyV1::OrderedEdgeBatch
            | RouterMutationFamilyV1::OrderedVertexBatch
            | RouterMutationFamilyV1::OrderedMixedBatch => Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into(),
            )),
        }
    }

    pub(crate) fn payload_mut(&mut self) -> &mut RouterMutationPayloadV1 {
        &mut self.as_v1_mut().payload
    }

    /// Return the scalar shard slice, or an empty slice for non-shard payloads.
    pub fn shards(&self) -> &[RouterMutationShardV1] {
        match &self.as_v1().payload {
            RouterMutationPayloadV1::Scalar { shards } => shards,
            _ => &[],
        }
    }

    pub(crate) fn shards_mut(&mut self) -> Option<&mut Vec<RouterMutationShardV1>> {
        match self.payload_mut() {
            RouterMutationPayloadV1::Scalar { shards } => Some(shards),
            _ => None,
        }
    }

    /// `true` once the saga reaches an irreversible terminal state. A progress-derived `Failed`
    /// without `terminal_failure` is retryable and therefore remains non-terminal.
    pub fn is_terminal(&self) -> bool {
        self.as_v1().terminal_failure.is_some()
            || self.lifecycle_phase() == MutationLifecyclePhase::Completed
            || matches!(
                self.payload(),
                RouterMutationPayloadV1::BulkLoadCoordinator(coordinator)
                    if coordinator.lifecycle.is_terminal()
            )
    }

    /// Set the terminal retention anchor exactly once after a terminal transition.
    pub(crate) fn mark_terminal_at_ns(&mut self, now: u64) {
        if self.is_terminal() && self.as_v1().terminal_at_ns.is_none() {
            self.as_v1_mut().terminal_at_ns = Some(now);
        }
    }

    /// `true` once the saga is **irreversibly** terminally failed (ADR 0030 slice 6): a same-key
    /// retry returns the stored error rather than re-dispatching.
    pub fn is_terminally_failed(&self) -> bool {
        self.as_v1().terminal_failure.is_some()
            || matches!(
                self.payload(),
                RouterMutationPayloadV1::BulkLoadCoordinator(coordinator)
                    if matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Failed { .. })
            )
    }

    /// `true` while a unique-reservation-holding mutation is eligible to be flipped to irreversible
    /// `terminal_failure` by the reclaim reconciler (ADR 0030 slice 6): a durable dispatch envelope
    /// exists, **no** shard's canonical write has committed, routing is released, and it is not
    /// already terminal-failed.
    pub fn is_uncommitted_dispatch(&self) -> bool {
        self.as_v1().terminal_failure.is_none()
            && !self.as_v1().routing_in_progress
            && match &self.as_v1().payload {
                RouterMutationPayloadV1::Scalar { shards } => {
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
        if let RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) = self.payload() {
            return match coordinator.lifecycle {
                BulkLoadLifecycleV1::Open => MutationLifecyclePhase::CanonicalCommitted,
                BulkLoadLifecycleV1::AppendPending { .. }
                | BulkLoadLifecycleV1::AbortPending { .. } => {
                    MutationLifecyclePhase::CanonicalPending
                }
                BulkLoadLifecycleV1::FinalizePending { .. } => {
                    MutationLifecyclePhase::ProjectionPending
                }
                BulkLoadLifecycleV1::Completed | BulkLoadLifecycleV1::Aborted => {
                    MutationLifecyclePhase::Completed
                }
                BulkLoadLifecycleV1::Failed { .. } => MutationLifecyclePhase::Failed,
            };
        }
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

/// Router mutation shard outcome for scalar plan execution (ADR 0029).
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
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedEdgeBatchReceiptV1, GraphOrderedVertexBatchReceiptV1,
        OrderedEdgeBatchGraphItemV1, OrderedVertexBatchGraphItemV1, OrderedVertexBatchGraphRequest,
        OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedPropertyTable,
        ordered_edge_batch_graph_request_fingerprint,
        ordered_vertex_batch_graph_request_fingerprint,
    };
    use ic_stable_structures::Storable;

    #[test]
    fn bulk_load_job_identity_uses_one_domain_fingerprint_and_family() {
        let identity = RouterMutationRequestIdentityV1::BulkLoadJob;
        assert_eq!(
            identity.request_fingerprint(),
            &BULK_LOAD_JOB_IDENTITY_FINGERPRINT
        );
        assert_eq!(identity.public_item_count(), None);
        let record = RouterMutationRecord::new_bulk_load(
            7,
            1,
            BulkLoadCoordinatorV1::new(BulkLoadTargetV1 {
                shard_id: ShardId::new(0),
                graph_canister: Principal::self_authenticating([41; 32]),
            }),
        )
        .unwrap();
        assert_eq!(record.ensure_bulk_load_family(), Ok(()));
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
    }

    #[test]
    fn bulk_load_coordinator_rejects_terminal_counter_mismatch() {
        let mut coordinator = BulkLoadCoordinatorV1::new(BulkLoadTargetV1 {
            shard_id: ShardId::new(0),
            graph_canister: Principal::self_authenticating([42; 32]),
        });
        coordinator.lifecycle = BulkLoadLifecycleV1::Completed;
        coordinator.next_chunk_index = 1;
        assert!(coordinator.validate().is_err());
    }

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
    fn mutation_family_validation_uses_both_identity_and_payload() {
        let mut record = RouterMutationRecord::new(2, 42, vec![9; 32]);
        record
            .ensure_gql_mutation_family()
            .expect("matching plan identity and scalar payload");
        assert_eq!(
            record.ensure_atomic_insert_family(),
            Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into()
            ))
        );

        record.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::OrderedEdgeBatch {
            public_fingerprint: [7; 32],
            public_item_count: 1,
        };
        assert_eq!(
            record.ensure_atomic_insert_family(),
            Err(RouterError::Conflict(
                "mutation record request identity and payload families disagree".into()
            )),
            "an identity-only classifier would wrongly admit this corrupt row"
        );

        record.as_v1_mut().payload = RouterMutationPayloadV1::OrderedEdgeBatchRouting;
        record
            .ensure_atomic_insert_family()
            .expect("matching ordered edge identity and payload");
        assert_eq!(
            record.ensure_gql_mutation_family(),
            Err(RouterError::Conflict(
                "client_mutation_key belongs to a different mutation family".into()
            ))
        );

        record.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::OrderedVertexBatch {
            public_fingerprint: [8; 32],
            public_item_count: 1,
        };
        assert_eq!(
            record.ensure_atomic_insert_family(),
            Err(RouterError::Conflict(
                "mutation record request identity and payload families disagree".into()
            )),
            "atomic subtypes must also agree"
        );

        record.as_v1_mut().payload = RouterMutationPayloadV1::OrderedVertexBatchRouting;
        record
            .ensure_atomic_insert_family()
            .expect("matching ordered vertex identity and payload");
        record.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::OrderedMixedBatch {
            public_fingerprint: [9; 32],
            public_operation_count: 1,
            public_vertex_count: 1,
            public_edge_count: 0,
        };
        record.as_v1_mut().payload = RouterMutationPayloadV1::OrderedMixedBatchRouting;
        record
            .ensure_atomic_insert_family()
            .expect("matching ordered mixed identity and payload");
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
                    allocated_vertex_ids: vec![7],
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
        assert!(!failed.is_terminal(), "retryable Failed is not terminal");
    }

    #[test]
    fn terminal_anchor_excludes_retryable_failure_and_is_set_once() {
        let mut retryable_failure = record_with_shards(Vec::new());
        retryable_failure.mark_terminal_at_ns(10);
        assert_eq!(retryable_failure.as_v1().terminal_at_ns, None);

        retryable_failure.as_v1_mut().completed_row_count = Some(0);
        retryable_failure.mark_terminal_at_ns(20);
        retryable_failure.mark_terminal_at_ns(30);
        assert!(retryable_failure.is_terminal());
        assert_eq!(retryable_failure.as_v1().terminal_at_ns, Some(20));

        let mut terminal_failure = record_with_shards(Vec::new());
        terminal_failure.as_v1_mut().terminal_failure = Some("permanent".into());
        terminal_failure.mark_terminal_at_ns(40);
        assert!(terminal_failure.is_terminal());
        assert_eq!(terminal_failure.as_v1().terminal_at_ns, Some(40));
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
}
