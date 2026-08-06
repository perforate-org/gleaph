//! Durable Router bulk-load chunk receipts (ADR 0057).
//!
//! The parent job record owns identity, lifecycle, placement, and aggregate counters.  This
//! region owns one immutable row per accepted chunk so status pagination and response-loss
//! recovery never require rewriting an unbounded parent value.  The key is deliberately fixed
//! width and ordered by `(job_mutation_id, chunk_index)` so a job's rows form one contiguous
//! range.

#![allow(
    dead_code,
    reason = "public bulk-load workflow is wired by the Router API slice"
)]

use crate::types::{AtomicInsertReceiptV1, BulkLoadChunkV1, BulkLoadEdgeV1};
use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::plan_exec::{
    MutationId, OrderedEdgeBatchGraphRequest, OrderedVertexBatchGraphRequest,
    ordered_edge_batch_graph_request_fingerprint, ordered_vertex_batch_graph_request_fingerprint,
};
use gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Maximum number of receipt rows returned by one status page.
pub use gleaph_bulk_load_api::MAX_BULK_LOAD_RECEIPTS_PER_PAGE;

/// Maximum consecutive child rows verified by one bounded Finalize step.
pub const BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP: u32 = 32;

/// Maximum consecutive receipt rows deleted by one bounded GC step.
pub const BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP: u32 = 32;

/// Fixed-width stable key for one accepted bulk-load chunk.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, CandidType, Serialize, Deserialize,
)]
pub struct BulkLoadChunkReceiptKey {
    pub job_mutation_id: MutationId,
    pub chunk_index: u32,
}

impl BulkLoadChunkReceiptKey {
    pub const BYTE_WIDTH: usize = 12;

    pub const fn new(job_mutation_id: MutationId, chunk_index: u32) -> Self {
        Self {
            job_mutation_id,
            chunk_index,
        }
    }
}

impl Storable for BulkLoadChunkReceiptKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: Self::BYTE_WIDTH as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::BYTE_WIDTH);
        bytes.extend_from_slice(&self.job_mutation_id.to_le_bytes());
        bytes.extend_from_slice(&self.chunk_index.to_le_bytes());
        bytes
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(
            bytes.len(),
            Self::BYTE_WIDTH,
            "invalid bulk receipt key width"
        );
        let job_mutation_id = MutationId::from_le_bytes(bytes[0..8].try_into().unwrap());
        let chunk_index = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        Self {
            job_mutation_id,
            chunk_index,
        }
    }
}

/// Durable progress for one child chunk.  `CanonicalPending` is the only state that permits a
/// first client replay dispatch; maintenance recovery may advance the later states only after
/// Graph evidence exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum BulkLoadChunkProgressV1 {
    CanonicalPending,
    CanonicalCommitted,
    ProjectionPending,
    RetirementPending,
    Completed,
}

impl BulkLoadChunkProgressV1 {
    pub const fn has_graph_receipt(self) -> bool {
        !matches!(self, Self::CanonicalPending)
    }

    pub const fn is_completed(self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// One immutable child Graph request retained for exact replay.  The mutation id is kept beside
/// the request in [`BulkLoadChunkReceiptRecordV1`] and is never embedded in this envelope.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum BulkLoadGraphRequestV1 {
    Vertex(gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequestV1),
    Edge(gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequestV1),
}

impl BulkLoadGraphRequestV1 {
    pub fn fingerprint(&self) -> Result<[u8; 32], String> {
        match self {
            Self::Vertex(request) => ordered_vertex_batch_graph_request_fingerprint(
                &OrderedVertexBatchGraphRequest::V1(request.clone()),
            ),
            Self::Edge(request) => ordered_edge_batch_graph_request_fingerprint(
                &OrderedEdgeBatchGraphRequest::V1(request.clone()),
            ),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Vertex(request) => OrderedVertexBatchGraphRequest::V1(request.clone()).validate(),
            Self::Edge(request) => OrderedEdgeBatchGraphRequest::V1(request.clone()).validate(),
        }
    }

    pub fn target(
        &self,
    ) -> (
        gleaph_graph_kernel::entry::GraphId,
        gleaph_graph_kernel::federation::ShardId,
        candid::Principal,
    ) {
        match self {
            Self::Vertex(request) => (
                request.graph_id,
                request.target_shard_id,
                request.target_graph_canister,
            ),
            Self::Edge(request) => (
                request.graph_id,
                request.target_shard_id,
                request.target_graph_canister,
            ),
        }
    }
}

/// Graph's exact canonical receipt, retained until the Graph journal retirement call succeeds.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum BulkLoadGraphReceiptV1 {
    Vertex(gleaph_graph_kernel::plan_exec::GraphOrderedVertexBatchReceiptV1),
    Edge(gleaph_graph_kernel::plan_exec::GraphOrderedEdgeBatchReceiptV1),
}

impl BulkLoadGraphReceiptV1 {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Vertex(receipt) => receipt.validate().map_err(str::to_owned),
            Self::Edge(receipt) => receipt.validate().map_err(str::to_owned),
        }
    }
}

/// Immutable normalized public chunk envelope retained in MemoryId 49.  It deliberately mirrors
/// the public vertex/edge variants without widening the public API; conversion back to
/// [`BulkLoadChunkV1`] delegates fingerprinting and validation to that single public SSOT.
#[derive(Clone, Debug, PartialEq, CandidType, Deserialize)]
pub enum BulkLoadChunkEnvelopeV1 {
    Vertices(Vec<crate::types::AtomicInsertVertexV1>),
    Edges(Vec<BulkLoadEdgeV1>),
}

impl BulkLoadChunkEnvelopeV1 {
    pub fn from_chunk(chunk: &BulkLoadChunkV1) -> Self {
        let mut normalized = chunk.clone();
        match &mut normalized {
            BulkLoadChunkV1::Vertices(items) => {
                for item in items {
                    item.vertex_labels
                        .sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                    item.initial_properties.sort_by(|left, right| {
                        left.property_name
                            .as_bytes()
                            .cmp(right.property_name.as_bytes())
                    });
                }
            }
            BulkLoadChunkV1::Edges(items) => {
                for item in items {
                    item.initial_edge_properties.sort_by(|left, right| {
                        left.property_name
                            .as_bytes()
                            .cmp(right.property_name.as_bytes())
                    });
                }
            }
        }
        match normalized {
            BulkLoadChunkV1::Vertices(items) => Self::Vertices(items),
            BulkLoadChunkV1::Edges(items) => Self::Edges(items),
        }
    }

    fn as_public(&self) -> BulkLoadChunkV1 {
        match self {
            Self::Vertices(items) => BulkLoadChunkV1::Vertices(items.clone()),
            Self::Edges(items) => BulkLoadChunkV1::Edges(items.clone()),
        }
    }

    pub fn fingerprint(&self) -> Result<[u8; 32], String> {
        self.as_public().fingerprint()
    }

    pub fn validate(&self) -> Result<(), String> {
        self.as_public().validate()
    }

    pub fn operation_count(&self) -> usize {
        match self {
            Self::Vertices(items) => items.len(),
            Self::Edges(items) => items.len(),
        }
    }
}

/// Stable value for one `(job, chunk_index)` row.
#[derive(Clone, Debug, PartialEq, CandidType, Deserialize)]
pub struct BulkLoadChunkReceiptRecordV1 {
    /// Router-owned fingerprint of the normalized incoming chunk. Retained as the resume
    /// idempotency key; the chunk envelope itself is not persisted (its only role was read-time
    /// re-derivation of this digest, which the Graph-request fingerprint handshake already covers
    /// for replay correctness).
    pub chunk_fingerprint: [u8; 32],
    /// Resolved Graph request retained only while the child can still be replayed
    /// (progress != Completed). `complete_bulk_load_child` compacts it away together with its
    /// fingerprint, so completed rows carry receipts only and never re-decode chunk payloads.
    pub graph_request: Option<BulkLoadGraphRequestV1>,
    pub graph_request_fingerprint: Option<[u8; 32]>,
    pub child_mutation_id: MutationId,
    pub progress: BulkLoadChunkProgressV1,
    pub public_receipt: Option<AtomicInsertReceiptV1>,
    pub graph_receipt: Option<BulkLoadGraphReceiptV1>,
    pub completed_at_ns: Option<u64>,
}

impl BulkLoadChunkReceiptRecordV1 {
    /// Validate every cross-field invariant before a receipt-map write.  The parent store facade
    /// additionally checks job identity, target pinning, and active-child agreement.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.graph_request, self.graph_request_fingerprint) {
            (Some(request), Some(fingerprint)) => {
                request.validate()?;
                let computed = request.fingerprint()?;
                if computed != fingerprint {
                    return Err("bulk-load Graph request fingerprint mismatch".into());
                }
            }
            (None, None) => {}
            _ => {
                return Err(
                    "bulk-load Graph request and fingerprint must be present or compacted together"
                        .into(),
                );
            }
        }
        if self.child_mutation_id == 0 {
            return Err("bulk-load child mutation id must be non-zero".into());
        }
        match self.progress {
            BulkLoadChunkProgressV1::CanonicalPending => {
                if self.graph_request.is_none() {
                    return Err(
                        "bulk-load CanonicalPending row must retain the Graph request".into(),
                    );
                }
                if self.public_receipt.is_some()
                    || self.graph_receipt.is_some()
                    || self.completed_at_ns.is_some()
                {
                    return Err(
                        "bulk-load CanonicalPending row must not carry a receipt or completion time"
                            .into(),
                    );
                }
            }
            BulkLoadChunkProgressV1::CanonicalCommitted
            | BulkLoadChunkProgressV1::ProjectionPending
            | BulkLoadChunkProgressV1::RetirementPending => {
                if self.graph_request.is_none() {
                    return Err(
                        "bulk-load non-terminal row must retain the Graph request for replay"
                            .into(),
                    );
                }
                let graph_receipt = self
                    .graph_receipt
                    .as_ref()
                    .ok_or_else(|| "bulk-load progress requires Graph receipt".to_string())?;
                graph_receipt.validate()?;
                let public_receipt = self
                    .public_receipt
                    .as_ref()
                    .ok_or_else(|| "bulk-load progress requires public receipt".to_string())?;
                public_receipt.validate()?;
                validate_public_receipt_matches_graph(public_receipt, graph_receipt)?;
                if self.completed_at_ns.is_some() {
                    return Err("non-terminal bulk-load row must not have completion time".into());
                }
            }
            BulkLoadChunkProgressV1::Completed => {
                if self.graph_request.is_some() {
                    return Err(
                        "completed bulk-load row must be compacted (Graph request removed)".into(),
                    );
                }
                let graph_receipt = self
                    .graph_receipt
                    .as_ref()
                    .ok_or_else(|| "completed bulk-load row requires Graph receipt".to_string())?;
                graph_receipt.validate()?;
                let public_receipt = self
                    .public_receipt
                    .as_ref()
                    .ok_or_else(|| "completed bulk-load row requires public receipt".to_string())?;
                public_receipt.validate()?;
                validate_public_receipt_matches_graph(public_receipt, graph_receipt)?;
                if self.completed_at_ns.is_none() {
                    return Err("completed bulk-load row requires completion time".into());
                }
            }
        }
        let encoded =
            Encode!(self).map_err(|error| format!("bulk-load receipt encode failed: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("bulk-load chunk receipt exceeds the safe payload bound".into());
        }
        Ok(())
    }
}

fn validate_public_receipt_matches_graph(
    public_receipt: &AtomicInsertReceiptV1,
    graph_receipt: &BulkLoadGraphReceiptV1,
) -> Result<(), String> {
    public_receipt.validate()?;
    match graph_receipt {
        BulkLoadGraphReceiptV1::Edge(receipt) => {
            if public_receipt.logical_operation_count != receipt.logical_edge_count
                || public_receipt.logical_vertex_count != 0
                || public_receipt.logical_edge_count != receipt.logical_edge_count
                || !public_receipt.allocated_vertex_ids.is_empty()
            {
                return Err("bulk-load edge public receipt does not match Graph receipt".into());
            }
        }
        BulkLoadGraphReceiptV1::Vertex(receipt) => {
            if public_receipt.logical_operation_count != receipt.logical_vertex_count
                || public_receipt.logical_vertex_count != receipt.logical_vertex_count
                || public_receipt.logical_edge_count != 0
                || public_receipt.allocated_vertex_ids.len() != receipt.allocated_vertex_ids.len()
            {
                return Err("bulk-load vertex public receipt does not match Graph receipt".into());
            }
        }
    }
    Ok(())
}

pub(crate) type StableBulkLoadChunkReceiptMap = ic_stable_structures::BTreeMap<
    BulkLoadChunkReceiptKey,
    BulkLoadChunkReceiptRecordV1,
    super::memory::Memory,
>;

pub(crate) fn init_bulk_load_chunk_receipts() -> StableBulkLoadChunkReceiptMap {
    ic_stable_structures::BTreeMap::init(super::memory::memory_manager_get_bulk_load_receipts())
}

impl Storable for BulkLoadChunkReceiptRecordV1 {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode BulkLoadChunkReceiptRecordV1"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode BulkLoadChunkReceiptRecordV1")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let row = Decode!(bytes.as_ref(), Self).expect("decode BulkLoadChunkReceiptRecordV1");
        row.validate()
            .unwrap_or_else(|error| panic!("invalid durable bulk-load receipt: {error}"));
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::memory;
    use candid::Principal;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedVertexBatchReceiptV1, OrderedVertexBatchGraphItemV1,
        OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedPropertyTable,
    };

    fn fixture_active_row() -> (BulkLoadChunkReceiptKey, BulkLoadChunkReceiptRecordV1) {
        // CanonicalPending: retains the resolved Graph request for replay, carries no receipts.
        let graph_canister = Principal::self_authenticating([7; 32]);
        let request = OrderedVertexBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(1),
            target_shard_id: ShardId::new(0),
            target_graph_canister: graph_canister,
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: Vec::new(),
                resolved_initial_properties: Vec::new(),
            }],
        };
        let graph_request = BulkLoadGraphRequestV1::Vertex(request);
        let graph_request_fingerprint = graph_request.fingerprint().unwrap();
        let chunk = BulkLoadChunkV1::Vertices(vec![crate::types::AtomicInsertVertexV1 {
            vertex_labels: Vec::new(),
            initial_properties: Vec::new(),
        }]);
        let chunk_fingerprint = BulkLoadChunkEnvelopeV1::from_chunk(&chunk)
            .fingerprint()
            .unwrap();
        let row = BulkLoadChunkReceiptRecordV1 {
            chunk_fingerprint,
            graph_request: Some(graph_request),
            graph_request_fingerprint: Some(graph_request_fingerprint),
            child_mutation_id: 2,
            progress: BulkLoadChunkProgressV1::CanonicalPending,
            public_receipt: None,
            graph_receipt: None,
            completed_at_ns: None,
        };
        row.validate().unwrap();
        (BulkLoadChunkReceiptKey::new(1, 0), row)
    }

    fn fixture_completed_row() -> (BulkLoadChunkReceiptKey, BulkLoadChunkReceiptRecordV1) {
        // Completed rows are compacted: the Graph request and its fingerprint are dropped.
        let graph_receipt = BulkLoadGraphReceiptV1::Vertex(GraphOrderedVertexBatchReceiptV1 {
            logical_vertex_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
            allocated_vertex_ids: vec![1],
        });
        let public_receipt = AtomicInsertReceiptV1 {
            logical_operation_count: 1,
            logical_vertex_count: 1,
            logical_edge_count: 0,
            allocated_vertex_ids: vec![
                vec![0; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES],
            ],
        };
        let row = BulkLoadChunkReceiptRecordV1 {
            chunk_fingerprint: [7; 32],
            graph_request: None,
            graph_request_fingerprint: None,
            child_mutation_id: 2,
            progress: BulkLoadChunkProgressV1::Completed,
            public_receipt: Some(public_receipt),
            graph_receipt: Some(graph_receipt),
            completed_at_ns: Some(10),
        };
        row.validate().unwrap();
        (BulkLoadChunkReceiptKey::new(1, 0), row)
    }

    #[test]
    fn bulk_load_receipt_key_is_fixed_width_and_ordered() {
        let low = BulkLoadChunkReceiptKey::new(7, 1);
        let high = BulkLoadChunkReceiptKey::new(7, 2);
        assert_eq!(low.into_bytes().len(), BulkLoadChunkReceiptKey::BYTE_WIDTH);
        assert!(low < high);
        let decoded = BulkLoadChunkReceiptKey::from_bytes(Cow::Owned(low.into_bytes()));
        assert_eq!(decoded, low);
    }

    #[test]
    fn bulk_load_receipt_validation_rejects_tampered_fingerprints() {
        let (_key, row) = fixture_active_row();

        let mut tampered_graph = row.clone();
        tampered_graph.graph_request_fingerprint.as_mut().unwrap()[0] ^= 1;
        assert!(
            tampered_graph
                .validate()
                .is_err_and(|error| error.contains("Graph request fingerprint"))
        );
    }

    #[test]
    fn bulk_load_receipt_compaction_invariants() {
        let (_key, active) = fixture_active_row();

        // Non-terminal rows must retain the Graph request for replay.
        let mut missing_request = active.clone();
        missing_request.graph_request = None;
        missing_request.graph_request_fingerprint = None;
        assert!(
            missing_request
                .validate()
                .is_err_and(|error| error.contains("retain the Graph request"))
        );

        // Completed rows must be compacted; a request-bearing completed row is invalid.
        let mut uncompacted = active;
        uncompacted.progress = BulkLoadChunkProgressV1::Completed;
        uncompacted.completed_at_ns = Some(10);
        assert!(
            uncompacted
                .validate()
                .is_err_and(|error| error.contains("compacted"))
        );
    }

    #[test]
    fn bulk_load_receipt_decode_revalidates_tampered_graph_request() {
        let (key, row) = fixture_active_row();
        let mut tampered = row;
        let BulkLoadGraphRequestV1::Vertex(request) = tampered.graph_request.as_mut().unwrap()
        else {
            panic!("vertex fixture request");
        };
        request.items.push(request.items[0].clone());
        let bytes = Encode!(&tampered).expect("encode tampered receipt");
        let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            BulkLoadChunkReceiptRecordV1::from_bytes(Cow::Owned(bytes))
        }));
        assert!(
            decoded.is_err(),
            "tampered durable receipt must fail decode"
        );

        let mut map = memory::init_bulk_load_chunk_receipts();
        map.clear_new();
        map.insert(key, tampered);
        drop(map);
        let reopened = memory::init_bulk_load_chunk_receipts();
        let reopened_read =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reopened.get(&key)));
        assert!(
            reopened_read.is_err(),
            "reopened tampered receipt must fail durable validation"
        );
        let mut reset = memory::init_bulk_load_chunk_receipts();
        reset.clear_new();
    }

    #[test]
    fn bulk_load_receipt_map_reopens_memory_id_49() {
        let (key, row) = fixture_completed_row();
        {
            let mut map = memory::init_bulk_load_chunk_receipts();
            map.clear_new();
            map.insert(key, row.clone());
        }
        let reopened = memory::init_bulk_load_chunk_receipts();
        assert_eq!(reopened.get(&key), Some(row));
        let mut reset = memory::init_bulk_load_chunk_receipts();
        reset.clear_new();
    }
}
