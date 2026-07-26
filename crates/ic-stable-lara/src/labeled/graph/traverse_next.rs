//! ADR 0050 Phase 1–2 isolated labeled traverse read API.
//!
//! This module is the temporary home for the consolidated logical read surface
//! described in `design/adr/0050-lara-traverse-read-api.md`. It lives beside
//! the legacy `traverse` module and becomes the crate-wide default only after
//! caller migration and the benchmark parity gate pass.
//!
//! The surface uses a typed bucket-local logical slot, keeps raw storage geometry
//! behind LARA, distinguishes topology reads from inline-property reads, and
//! expresses early termination with `std::ops::ControlFlow`.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey,
        record::{LabelBucket, LabeledVertex},
    },
    lara::operation_error::LaraOperationError,
    traits::CsrVertex,
    traits::{CsrEdge, CsrEdgeTombstone},
};
use ic_stable_structures::Memory;
use std::{cell::Cell, ops::ControlFlow};

use crate::traverse::iter::{try_visit_indexed, visit_indexed};
use crate::traverse::{Traversal, TraversalOrder, TraversalRequest, TraversalWindow};

#[cfg(feature = "canbench")]
mod bench;

use super::{BucketSearch, LabeledLaraGraph, OutEdgeOrder, error::LabeledOperationError};

#[derive(Debug)]
enum LabeledWindowBreak<B> {
    LimitReached,
    Visitor(B),
}

fn finish_labeled_window<B>(
    result: Result<ControlFlow<LabeledWindowBreak<B>>, LabeledOperationError>,
) -> Result<ControlFlow<B>, LabeledOperationError> {
    match result? {
        ControlFlow::Continue(()) | ControlFlow::Break(LabeledWindowBreak::LimitReached) => {
            Ok(ControlFlow::Continue(()))
        }
        ControlFlow::Break(LabeledWindowBreak::Visitor(value)) => Ok(ControlFlow::Break(value)),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LabeledTraversalRequest {
    pub(crate) owner: VertexId,
    pub(crate) label: BucketLabelKey,
    pub(crate) order: OutEdgeOrder,
}

impl TraversalRequest for LabeledTraversalRequest {
    type Slot = BucketEntryPosition;
    type Error = LabeledOperationError;

    fn order(&self) -> TraversalOrder {
        match self.order {
            OutEdgeOrder::Ascending => TraversalOrder::Ascending,
            OutEdgeOrder::Descending => TraversalOrder::Descending,
        }
    }
}

pub use crate::traverse::BucketEntryPosition;

/// Exact inline-property bytes for one live edge row.
///
/// Width zero is represented by an empty `bytes` vector and is a valid value;
/// callers must not treat it as a missing property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlinePropertyBytes {
    /// Declared byte width of the inline property for this edge's label bucket.
    pub width: u16,
    /// Exact byte contents of the inline property, or empty when `width == 0`.
    pub bytes: Vec<u8>,
}

impl InlinePropertyBytes {
    /// Creates an empty zero-width inline-property value.
    pub fn empty() -> Self {
        Self {
            width: 0,
            bytes: Vec::new(),
        }
    }
}

/// A live edge row together with its exact inline-property bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeWithInlineProperty<E> {
    /// The topology-only edge row.
    pub edge: E,
    /// Exact inline-property bytes belonging to this row.
    pub inline_property: InlinePropertyBytes,
}

/// Result of reading a logical edge slot while preserving tombstone visibility.
pub use crate::traverse::EdgeSlotState;

/// Ordering and deduplication helper for selected-slot reads.
fn order_slots(slots: &[BucketEntryPosition], order: OutEdgeOrder) -> Vec<u32> {
    let mut ordered: Vec<u32> = slots.iter().map(|s| s.raw()).collect();
    match order {
        OutEdgeOrder::Ascending => ordered.sort_unstable(),
        OutEdgeOrder::Descending => ordered.sort_unstable_by(|a, b| b.cmp(a)),
    }
    ordered.dedup();
    ordered
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Reads one live edge row by its logical slot.
    pub(crate) fn read_edge(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        Ok(match self.read_edge_state(owner, label, slot)? {
            EdgeSlotState::Live(edge) => Some(edge),
            EdgeSlotState::Missing | EdgeSlotState::Tombstone => None,
        })
    }

    /// Reads one logical slot and distinguishes missing, tombstoned, and live rows.
    pub(crate) fn read_edge_state(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<EdgeSlotState<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.read_edge_state_internal(owner, label, slot)
    }

    /// Counts live edges with `neighbor` that precede `before_slot` in ascending
    /// bucket-local slot order. Used by CounterpartScan to compute a source row's
    /// PairOrdinal without materializing the whole bucket.
    pub(crate) fn count_preceding_live_edges_with_neighbor(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        before_slot: BucketEntryPosition,
        neighbor: VertexId,
    ) -> Result<u32, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(0);
            }
            // Fast path: bypass rows store all live edges at slot 0..degree-1 in ascending order.
            // A singleton relation can only have PairOrdinal 0.
            if vertex.degree() == 1 {
                return Ok(0);
            }
            let mut count = 0u32;
            let mut iter = self.edges.asc_out_edges_iter(&self.vertices, owner)?;
            while let Some((slot, edge)) = iter.next_with_slot() {
                if slot >= before_slot.raw() {
                    break;
                }
                if edge.neighbor_vid() == neighbor {
                    count = count.saturating_add(1);
                }
            }
            return Ok(count);
        }
        let Some(info) = self.read_label_bucket_placement_info(owner, label)? else {
            return Ok(0);
        };
        // Fast path: a singleton relation has PairOrdinal 0, so there are no preceding matches.
        if info.degree == 1 {
            return Ok(0);
        }
        let logical_slots = info
            .stored_edge_slots
            .checked_add(info.edge_overflow_log_len)
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
        let mut count = 0u32;
        let _ = self.for_each_live_edge_slot_for_label_direct_with_control_flow(
            owner,
            label,
            logical_slots,
            OutEdgeOrder::Ascending,
            0,
            |slot, edge| {
                if slot >= before_slot.raw() {
                    return ControlFlow::Break(());
                }
                if edge.neighbor_vid() == neighbor {
                    count = count.saturating_add(1);
                }
                ControlFlow::Continue(())
            },
        )?;
        Ok(count)
    }

    /// Returns true when the label bucket (or bypass row) for `owner` contains
    /// exactly one live edge and the requested slot is that edge's logical slot 0.
    ///
    /// Used by CounterpartScan as a fast-path guard: a singleton directed relation
    /// has PairOrdinal 0 and its counterpart must live at slot 0 of the opposite
    /// orientation, so no rank/select traversal is required.
    pub(crate) fn label_bucket_is_singleton(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<bool, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        M: Memory,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(false);
            }
            return Ok(vertex.degree() == 1 && slot.raw() == 0);
        }
        match self.read_label_bucket_placement_info(owner, label)? {
            Some(info) => Ok(info.degree == 1 && slot.raw() == 0),
            None => Ok(false),
        }
    }

    /// Selects the k-th live edge (0-based) whose neighbor equals `neighbor` in ascending
    /// bucket-local slot order. Used by CounterpartScan to select the counterpart row by
    /// PairOrdinal without materializing the whole bucket.
    pub(crate) fn select_live_edge_by_neighbor_ordinal(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        neighbor: VertexId,
        ordinal: u32,
    ) -> Result<Option<BucketEntryPosition>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(None);
            }
            let mut matching = 0u32;
            let mut iter = self.edges.asc_out_edges_iter(&self.vertices, owner)?;
            while let Some((slot, edge)) = iter.next_with_slot() {
                if edge.neighbor_vid() != neighbor {
                    continue;
                }
                if matching == ordinal {
                    return Ok(Some(BucketEntryPosition::new(slot)));
                }
                matching = matching.saturating_add(1);
            }
            return Ok(None);
        }
        let Some(info) = self.read_label_bucket_placement_info(owner, label)? else {
            return Ok(None);
        };
        // Fast path: a singleton relation's only live edge must be the requested one.
        if info.degree == 1 && ordinal == 0 {
            if let EdgeSlotState::Live(edge) =
                self.read_edge_state(owner, label, BucketEntryPosition::new(0))?
                && edge.neighbor_vid() == neighbor
            {
                return Ok(Some(BucketEntryPosition::new(0)));
            }
            return Ok(None);
        }
        let logical_slots = info
            .stored_edge_slots
            .checked_add(info.edge_overflow_log_len)
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
        let mut matching = 0u32;
        let mut selected: Option<BucketEntryPosition> = None;
        let _ = self.for_each_live_edge_slot_for_label_direct_with_control_flow(
            owner,
            label,
            logical_slots,
            OutEdgeOrder::Ascending,
            0,
            |slot, edge| {
                if edge.neighbor_vid() != neighbor {
                    return ControlFlow::Continue(());
                }
                if matching == ordinal {
                    selected = Some(BucketEntryPosition::new(slot));
                    return ControlFlow::Break(());
                }
                matching = matching.saturating_add(1);
                ControlFlow::Continue(())
            },
        )?;
        Ok(selected)
    }

    /// Reads one live edge row with its exact inline-property bytes.
    pub(crate) fn read_edge_with_inline_property(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<Option<EdgeWithInlineProperty<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let edge = match self.read_edge_state(owner, label, slot)? {
            EdgeSlotState::Live(edge) => edge,
            _ => return Ok(None),
        };
        let inline_property = self.read_inline_property_bytes_for_slot(owner, label, slot)?;
        Ok(Some(EdgeWithInlineProperty {
            edge,
            inline_property,
        }))
    }

    /// Visits every live edge for one label in the requested order.
    pub(crate) fn visit_edges<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            return match order {
                OutEdgeOrder::Ascending => {
                    let mut edges = self.edges.asc_out_edges_iter(&self.vertices, owner)?;
                    let indexed = std::iter::from_fn(move || {
                        edges.next_with_slot().map(|(slot, edge)| Ok((slot, edge)))
                    });
                    try_visit_indexed(indexed, |slot, edge| {
                        visit(BucketEntryPosition::new(slot), edge)
                    })
                }
                OutEdgeOrder::Descending => {
                    let mut edges = self.edges.out_edges_iter(&self.vertices, owner)?;
                    let indexed = std::iter::from_fn(move || {
                        edges.next_with_slot().map(|(slot, edge)| Ok((slot, edge)))
                    });
                    try_visit_indexed(indexed, |slot, edge| {
                        visit(BucketEntryPosition::new(slot), edge)
                    })
                }
            };
        }
        let Some(info) = self.read_label_bucket_placement_info(owner, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        let logical_slots = info
            .stored_edge_slots
            .checked_add(info.edge_overflow_log_len)
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
        self.for_each_live_edge_slot_for_label_direct_with_control_flow(
            owner,
            label,
            logical_slots,
            order,
            0,
            |slot, edge| visit(BucketEntryPosition::new(slot), edge),
        )
    }

    pub(crate) fn visit_edges_with_inline_property<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: impl FnMut(BucketEntryPosition, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            return match order {
                OutEdgeOrder::Ascending => {
                    let mut edges = self.edges.asc_out_edges_iter(&self.vertices, owner)?;
                    let indexed = std::iter::from_fn(move || {
                        edges.next_with_slot().map(|(slot, edge)| Ok((slot, edge)))
                    });
                    try_visit_indexed(indexed, |slot, edge| {
                        visit(
                            BucketEntryPosition::new(slot),
                            EdgeWithInlineProperty {
                                edge,
                                inline_property: InlinePropertyBytes::empty(),
                            },
                        )
                    })
                }
                OutEdgeOrder::Descending => {
                    let mut edges = self.edges.out_edges_iter(&self.vertices, owner)?;
                    let indexed = std::iter::from_fn(move || {
                        edges.next_with_slot().map(|(slot, edge)| Ok((slot, edge)))
                    });
                    try_visit_indexed(indexed, |slot, edge| {
                        visit(
                            BucketEntryPosition::new(slot),
                            EdgeWithInlineProperty {
                                edge,
                                inline_property: InlinePropertyBytes::empty(),
                            },
                        )
                    })
                }
            };
        }
        let (bucket, _bucket_index, log_chains) =
            self.resolve_label_bucket_and_payload_chains(owner, label, &vertex)?;
        let width = bucket.inline_value_byte_width();
        let mut ordinal = match order {
            OutEdgeOrder::Descending => bucket.degree().saturating_sub(1),
            OutEdgeOrder::Ascending => 0,
        };
        let Some(info) = self.read_label_bucket_placement_info(owner, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        let logical_slots = info
            .stored_edge_slots
            .checked_add(info.edge_overflow_log_len)
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
        let flow = self.for_each_live_edge_slot_for_label_direct_with_control_flow(
            owner,
            label,
            logical_slots,
            order,
            0,
            |slot, edge| {
                let inline_property = if width == 0 {
                    InlinePropertyBytes::empty()
                } else {
                    match self.read_inline_property_bytes_for_ordinal(
                        owner,
                        label,
                        &bucket,
                        ordinal,
                        log_chains.as_ref(),
                    ) {
                        Ok(property) => property,
                        Err(error) => return ControlFlow::Break(Err(error)),
                    }
                };
                match visit(
                    BucketEntryPosition::new(slot),
                    EdgeWithInlineProperty {
                        edge,
                        inline_property,
                    },
                ) {
                    ControlFlow::Continue(()) => {
                        match order {
                            OutEdgeOrder::Descending => ordinal = ordinal.saturating_sub(1),
                            OutEdgeOrder::Ascending => ordinal = ordinal.saturating_add(1),
                        }
                        ControlFlow::Continue(())
                    }
                    ControlFlow::Break(value) => ControlFlow::Break(Ok(value)),
                }
            },
        );
        match flow? {
            ControlFlow::Continue(()) => Ok(ControlFlow::Continue(())),
            ControlFlow::Break(Ok(value)) => Ok(ControlFlow::Break(value)),
            ControlFlow::Break(Err(error)) => Err(error),
        }
    }

    /// Visits a selected set of logical slots for one label in the requested order.
    pub(crate) fn visit_edges_at<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slots: &[BucketEntryPosition],
        order: OutEdgeOrder,
        visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.visit_edges_at_with_replay(owner, label, slots, order, None, visit)
    }

    /// Like [`Self::visit_edges_at`], but may reuse a hybrid overflow replay.
    pub(crate) fn visit_edges_at_with_replay<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slots: &[BucketEntryPosition],
        order: OutEdgeOrder,
        replay: Option<&super::iter::HybridOverflowEdgeReplay>,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if slots.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        let raw_slots = order_slots(slots, order);
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            for slot in raw_slots {
                if let EdgeSlotState::Live(edge) =
                    self.read_edge_state_internal(owner, label, BucketEntryPosition::new(slot))?
                    && let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge)
                {
                    return Ok(ControlFlow::Break(value));
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        let mut selected = Vec::with_capacity(raw_slots.len());
        self.read_out_edge_slots_for_label_with_replay_and_slot(
            owner,
            label,
            &raw_slots,
            order,
            replay,
            |slot, edge| selected.push((slot, edge)),
        )?;
        Ok(visit_indexed(selected, |slot, edge| {
            visit(BucketEntryPosition::new(slot), edge)
        }))
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn read_edge_state_internal(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<EdgeSlotState<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(EdgeSlotState::Missing);
            }
            let slot_index = slot.raw();
            if slot_index >= vertex.stored_degree() {
                return Ok(EdgeSlotState::Missing);
            }
            let edge_slot = crate::labeled::slot_index::checked_add_slot_index(
                vertex.base_slot_start(),
                u64::from(slot_index),
            )
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
            let edge = self.edges.read_slot(edge_slot).with_label_id(label.raw());
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                return Ok(EdgeSlotState::Tombstone);
            }
            return Ok(EdgeSlotState::Live(edge));
        }
        self.read_edge_slot_state_for_label(owner, label, slot.raw())
    }

    /// Resolves the label bucket and, when the bucket stores inline values in a
    /// suffix log, builds the ordered log chain used by
    /// [`Self::read_inline_property_bytes_for_ordinal`].
    fn resolve_label_bucket_and_payload_chains(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        vertex: &LabeledVertex,
    ) -> Result<(LabelBucket, u32, Option<Vec<u32>>), LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok((LabelBucket::default(), 0, None));
        }
        match self.find_bucket(owner, vertex, label)? {
            BucketSearch::Found { slot, bucket } => {
                let bucket_index = Self::labeled_bucket_descriptor_index(vertex, slot)?;
                let log_chains = self.bucket_payload_log_chain_opt(owner, &bucket);
                Ok((bucket, bucket_index, log_chains))
            }
            BucketSearch::Missing { .. } => Ok((LabelBucket::default(), 0, None)),
        }
    }

    /// Reads the exact inline-property bytes for the live row at `slot`.
    fn read_inline_property_bytes_for_slot(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slot: BucketEntryPosition,
    ) -> Result<InlinePropertyBytes, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        let (bucket, bucket_index, log_chains) =
            self.resolve_label_bucket_and_payload_chains(owner, label, &vertex)?;
        if bucket.inline_value_byte_width() == 0 {
            return Ok(InlinePropertyBytes::empty());
        }
        let ordinal = if vertex.is_default_edge_labeled() {
            slot.raw()
        } else {
            self.bucket_live_ordinal_at_edge_slot(
                owner,
                &vertex,
                bucket_index,
                Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?,
                &bucket,
                slot.raw(),
            )?
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?
        };
        self.read_inline_property_bytes_for_ordinal(
            owner,
            label,
            &bucket,
            ordinal,
            log_chains.as_ref(),
        )
    }

    /// Reads the exact inline-property bytes at a known live ordinal.
    fn read_inline_property_bytes_for_ordinal(
        &self,
        owner: VertexId,
        _label: BucketLabelKey,
        bucket: &LabelBucket,
        ordinal: u32,
        log_chains: Option<&Vec<u32>>,
    ) -> Result<InlinePropertyBytes, LabeledOperationError> {
        let width = bucket.inline_value_byte_width();
        if width == 0 {
            return Ok(InlinePropertyBytes::empty());
        }
        let bytes = self.read_bucket_payload_for_slot(owner, bucket, ordinal, log_chains)?;
        if bytes.len() != usize::from(width) {
            return Err(LabeledOperationError::PayloadByteWidthMismatch {
                bucket_width: width,
                edge_inline_value_width: u16::try_from(bytes.len()).unwrap_or(u16::MAX),
            });
        }
        Ok(InlinePropertyBytes { width, bytes })
    }
}

impl<E, M> Traversal for LabeledLaraGraph<E, M>
where
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
{
    type Request = LabeledTraversalRequest;
    type Slot = BucketEntryPosition;
    type Edge = E;
    type EdgeState = crate::traverse::EdgeSlotState<E>;
    type EdgeWithInlineProperty = EdgeWithInlineProperty<E>;
    type Replay = super::iter::HybridOverflowEdgeReplay;
    type Error = LabeledOperationError;

    fn read_edge(
        &self,
        request: &Self::Request,
        slot: Self::Slot,
    ) -> Result<Option<Self::Edge>, Self::Error> {
        LabeledLaraGraph::read_edge(self, request.owner, request.label, slot)
    }

    fn read_edge_state(
        &self,
        request: &Self::Request,
        slot: Self::Slot,
    ) -> Result<Self::EdgeState, Self::Error> {
        LabeledLaraGraph::read_edge_state(self, request.owner, request.label, slot)
    }

    fn read_edge_with_inline_property(
        &self,
        request: &Self::Request,
        slot: Self::Slot,
    ) -> Result<Option<Self::EdgeWithInlineProperty>, Self::Error> {
        LabeledLaraGraph::read_edge_with_inline_property(self, request.owner, request.label, slot)
    }

    fn visit_edges<B>(
        &self,
        request: &Self::Request,
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        LabeledLaraGraph::visit_edges(self, request.owner, request.label, request.order, visit)
    }

    fn visit_edges_window<B>(
        &self,
        request: &Self::Request,
        window: TraversalWindow,
        mut visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        if window.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        let offset = Cell::new(window.offset);
        let mut remaining = window.limit;
        let mut window_visit = |slot: BucketEntryPosition, edge: E| {
            if offset.get() != 0 {
                offset.set(offset.get() - 1);
                return ControlFlow::Continue(());
            }
            match visit(slot, edge) {
                ControlFlow::Break(value) => ControlFlow::Break(LabeledWindowBreak::Visitor(value)),
                ControlFlow::Continue(()) => match remaining.as_mut() {
                    Some(remaining) => {
                        *remaining -= 1;
                        if *remaining == 0 {
                            ControlFlow::Break(LabeledWindowBreak::LimitReached)
                        } else {
                            ControlFlow::Continue(())
                        }
                    }
                    None => ControlFlow::Continue(()),
                },
            }
        };

        self.ensure_vertex(request.owner)?;
        let vertex = self.vertices.get(request.owner);
        let result = if vertex.is_default_edge_labeled() {
            LabeledLaraGraph::visit_edges(
                self,
                request.owner,
                request.label,
                request.order,
                &mut window_visit,
            )
        } else {
            let placement = self.read_label_bucket_placement_info(request.owner, request.label)?;
            let (logical_slots, skip_live) = match placement {
                Some(info) => {
                    let logical_slots = info
                        .stored_edge_slots
                        .checked_add(info.edge_overflow_log_len)
                        .ok_or(LabeledOperationError::from(
                            LaraOperationError::CollectAllocationOverflow,
                        ))?;
                    let skip_live = if info.degree == logical_slots {
                        window.offset
                    } else {
                        0
                    };
                    (logical_slots, skip_live)
                }
                None => (0, 0),
            };
            if skip_live != 0 {
                offset.set(0);
            }
            self.for_each_live_edge_slot_for_label_direct_with_control_flow(
                request.owner,
                request.label,
                logical_slots,
                request.order,
                skip_live,
                |slot, edge| window_visit(BucketEntryPosition::new(slot), edge),
            )
        };
        finish_labeled_window(result)
    }

    fn visit_edges_with_inline_property<B>(
        &self,
        request: &Self::Request,
        visit: impl FnMut(Self::Slot, Self::EdgeWithInlineProperty) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        LabeledLaraGraph::visit_edges_with_inline_property(
            self,
            request.owner,
            request.label,
            request.order,
            visit,
        )
    }

    fn visit_edges_at<B>(
        &self,
        request: &Self::Request,
        slots: &[Self::Slot],
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        LabeledLaraGraph::visit_edges_at(
            self,
            request.owner,
            request.label,
            slots,
            request.order,
            visit,
        )
    }

    fn visit_edges_at_with_replay<B>(
        &self,
        request: &Self::Request,
        slots: &[Self::Slot],
        replay: Option<&Self::Replay>,
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        LabeledLaraGraph::visit_edges_at_with_replay(
            self,
            request.owner,
            request.label,
            slots,
            request.order,
            replay,
            visit,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        VertexId,
        labeled::{
            BucketLabelKey, LabeledPayloadValueBatchScratch, OutEdgeOrder,
            graph::test_support::{
                PayloadTestEdge, TestEdge, inline_value_test_graph_with_capacity, test_graph,
                test_graph_with_default,
            },
            record::LabeledVertex,
        },
        traverse::TraversalWindow,
    };
    use std::ops::ControlFlow;

    fn bucket_graph() -> (
        super::super::LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        VertexId,
    ) {
        let graph = test_graph();
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        (graph, src)
    }

    fn bypass_graph() -> (
        super::super::LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        VertexId,
        BucketLabelKey,
    ) {
        let default_label = BucketLabelKey::directed_from_index(1);
        let graph = test_graph_with_default(default_label);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        (graph, src, default_label)
    }

    #[test]
    fn read_edge_finds_live_slot_and_returns_none_for_missing_or_tombstoned() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=4u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        // Logical slot 1 is live.
        let edge = graph
            .read_edge(src, label, BucketEntryPosition::new(1))
            .unwrap()
            .expect("slot 1 is live");
        assert_eq!(edge.target, 2);

        // Out-of-range slot is absent.
        assert!(
            graph
                .read_edge(src, label, BucketEntryPosition::new(100))
                .unwrap()
                .is_none()
        );

        // Tombstone after removal.
        graph
            .remove_edge_at_slot(src, label, BucketEntryPosition::new(1).raw())
            .unwrap();
        assert!(matches!(
            graph
                .read_edge_state(src, label, BucketEntryPosition::new(1))
                .unwrap(),
            EdgeSlotState::Tombstone
        ));
        assert!(
            graph
                .read_edge(src, label, BucketEntryPosition::new(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn visit_edges_ascending_and_descending_match_logical_order() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }

        let mut asc = Vec::new();
        let flow = graph
            .visit_edges::<()>(src, label, OutEdgeOrder::Ascending, |slot, edge| {
                asc.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(asc, vec![(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]);

        let mut desc = Vec::new();
        let flow = graph
            .visit_edges::<()>(src, label, OutEdgeOrder::Descending, |slot, edge| {
                desc.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(desc, vec![(4, 5), (3, 4), (2, 3), (1, 2), (0, 1)]);
    }

    #[test]
    fn visit_edges_early_break_returns_break_value() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }

        let result = graph
            .visit_edges::<u32>(src, label, OutEdgeOrder::Ascending, |slot, edge| {
                if slot.raw() == 2 {
                    ControlFlow::Break(edge.target)
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert_eq!(result, ControlFlow::Break(3));
    }

    #[test]
    fn visit_edges_window_applies_offset_limit_and_preserves_breaks() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }
        let mut request = LabeledTraversalRequest {
            owner: src,
            label,
            order: OutEdgeOrder::Ascending,
        };

        let mut out = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(1, Some(2)),
            |slot, edge| {
                out.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(out, vec![(1, 2), (2, 3)]);

        request.order = OutEdgeOrder::Descending;
        let mut descending = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(1, Some(2)),
            |slot, edge| {
                descending.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(descending, vec![(3, 4), (2, 3)]);

        request.order = OutEdgeOrder::Ascending;
        let mut called = false;
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(0, Some(0)),
            |_slot, _edge| {
                called = true;
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert!(!called);

        let flow: ControlFlow<u32> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(0, Some(4)),
            |_slot, edge| ControlFlow::Break(edge.target),
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Break(1));

        let mut inline_targets = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_with_inline_property_window(
            &graph,
            &request,
            TraversalWindow::new(2, Some(2)),
            |_slot, item| {
                inline_targets.push(item.edge.target);
                assert_eq!(item.inline_property.width, 0);
                assert!(item.inline_property.bytes.is_empty());
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(inline_targets, vec![3, 4]);
    }

    #[test]
    fn visit_edges_window_counts_live_rows_when_tombstones_make_bucket_sparse() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        graph.remove_edge_at_slot(src, label, 2).unwrap();
        let request = LabeledTraversalRequest {
            owner: src,
            label,
            order: OutEdgeOrder::Ascending,
        };

        let mut out = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(1, Some(2)),
            |slot, edge| {
                out.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(out, vec![(1, 2), (3, 4)]);
    }

    #[test]
    fn visit_edges_at_sorts_dedupes_and_skips_missing_slots() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        graph.remove_edge_at_slot(src, label, 2).unwrap();

        let mut out = Vec::new();
        let flow = graph
            .visit_edges_at::<()>(
                src,
                label,
                &[
                    BucketEntryPosition::new(4),
                    BucketEntryPosition::new(1),
                    BucketEntryPosition::new(1),
                    BucketEntryPosition::new(2),
                    BucketEntryPosition::new(100),
                ],
                OutEdgeOrder::Ascending,
                |slot, edge| {
                    out.push((slot.raw(), edge.target));
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        // Request order is ignored; output follows canonical ascending order.
        assert_eq!(out, vec![(1, 2), (4, 5)]);
    }

    #[test]
    fn default_label_bypass_point_and_visitor_reads() {
        let (graph, src, default_label) = bypass_graph();
        for target in 10..=41u32 {
            graph
                .insert_edge(src, default_label, TestEdge { target })
                .unwrap();
        }

        assert_eq!(
            graph
                .read_edge(src, default_label, BucketEntryPosition::new(2))
                .unwrap()
                .map(|e| e.target),
            Some(12)
        );

        let mut asc = Vec::new();
        let flow = graph
            .visit_edges::<()>(src, default_label, OutEdgeOrder::Ascending, |slot, edge| {
                asc.push(slot.raw());
                assert!((10..=41).contains(&edge.target));
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(asc, (0..32).collect::<Vec<_>>());

        // Non-bypass label on a bypass vertex is empty.
        assert!(
            graph
                .read_edge(
                    src,
                    BucketLabelKey::directed_from_index(99),
                    BucketEntryPosition::new(0)
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn default_label_selected_slots_read_live_edges_in_requested_order() {
        let (graph, src, default_label) = bypass_graph();
        for target in 10..=14u32 {
            graph
                .insert_edge(src, default_label, TestEdge { target })
                .unwrap();
        }

        let mut out = Vec::new();
        let flow = graph
            .visit_edges_at::<()>(
                src,
                default_label,
                &[
                    BucketEntryPosition::new(4),
                    BucketEntryPosition::new(1),
                    BucketEntryPosition::new(4),
                ],
                OutEdgeOrder::Descending,
                |slot, edge| {
                    out.push((slot.raw(), edge.target));
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(out, vec![(4, 14), (1, 11)]);
    }

    #[test]
    fn parallel_edges_keep_ordinal_slots_and_exact_inline_bytes() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, label, 2)
            .unwrap();
        for value in [10u16, 20u16] {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    PayloadTestEdge::with_bytes(7, &value.to_le_bytes()),
                )
                .unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();

        let mut rows = Vec::new();
        let _ = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |slot, item| {
                    rows.push((
                        slot.raw(),
                        item.edge.target,
                        item.inline_property.bytes.clone(),
                    ));
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (0, 7, 10u16.to_le_bytes().to_vec()),
                (1, 7, 20u16.to_le_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn visit_edges_with_inline_property_reads_exact_bytes_for_slab_and_overflow() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, label, 4)
            .unwrap();

        // Insert enough edges to create both slab and overflow-log rows.
        for target in 1..=64u32 {
            let value = (target * 7).to_le_bytes();
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    PayloadTestEdge::with_bytes(target, &value),
                )
                .unwrap();
        }

        // Phase 1: collect all logical slots via property-first batches.
        let mut scratch = LabeledPayloadValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_value_batches_for_label(
                src,
                label,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    slots.extend(
                        batch
                            .slot_indices
                            .iter()
                            .copied()
                            .map(BucketEntryPosition::new),
                    )
                },
            )
            .unwrap();
        assert!(!slots.is_empty());

        // Phase 2: read topology at selected slots, then attach inline properties.
        let mut seen = 0u32;
        let _ = graph
            .visit_edges_at::<()>(src, label, &slots, OutEdgeOrder::Ascending, |slot, edge| {
                let with_prop = graph
                    .read_edge_with_inline_property(src, label, slot)
                    .unwrap()
                    .expect("selected slot is live");
                assert_eq!(edge.edge_slot_index_raw(), slot.raw());
                assert_eq!(with_prop.edge.target, edge.target);
                assert_eq!(with_prop.inline_property.width, 4);
                let expected = (edge.target * 7).to_le_bytes().to_vec();
                assert_eq!(with_prop.inline_property.bytes, expected);
                seen += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(seen, slots.len() as u32);

        // Streaming attached-property visitor also exposes exact bytes.
        let mut streamed = 0u32;
        let flow = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Descending,
                |_slot, item| {
                    let expected = (item.edge.target * 7).to_le_bytes().to_vec();
                    assert_eq!(item.inline_property.bytes, expected);
                    streamed += 1;
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(streamed, slots.len() as u32);

        // A second full traversal still sees exact bytes after an early break.
        let mut stopped_after = 0u32;
        let flow = graph
            .visit_edges_with_inline_property::<u32>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |_slot, item| {
                    assert_eq!(item.inline_property.width, 4);
                    assert_eq!(
                        item.inline_property.bytes,
                        &(item.edge.target * 7).to_le_bytes()[..]
                    );
                    stopped_after += 1;
                    ControlFlow::Break(stopped_after)
                },
            )
            .unwrap();
        assert_eq!(flow, ControlFlow::Break(1));

        let mut resumed = 0u32;
        let flow = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |_slot, item| {
                    assert_eq!(item.inline_property.width, 4);
                    assert_eq!(
                        item.inline_property.bytes,
                        &(item.edge.target * 7).to_le_bytes()[..]
                    );
                    resumed += 1;
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(resumed, slots.len() as u32);
    }

    #[test]
    fn inline_property_window_preserves_slots_edges_and_bytes_for_hybrid_orders() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, label, 4)
            .unwrap();
        for target in 1..=64u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    PayloadTestEdge::with_bytes(target, &(target * 7).to_le_bytes()),
                )
                .unwrap();
        }

        let request = LabeledTraversalRequest {
            owner: src,
            label,
            order: OutEdgeOrder::Ascending,
        };
        let mut ascending = Vec::new();
        let _ = Traversal::visit_edges_with_inline_property_window(
            &graph,
            &request,
            TraversalWindow::new(5, Some(3)),
            |slot, item| {
                ascending.push((
                    slot.raw(),
                    item.edge.target,
                    item.inline_property.bytes.clone(),
                ));
                ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
        assert_eq!(
            ascending,
            vec![
                (5, 6, (6u32 * 7).to_le_bytes().to_vec()),
                (6, 7, (7u32 * 7).to_le_bytes().to_vec()),
                (7, 8, (8u32 * 7).to_le_bytes().to_vec()),
            ]
        );

        let descending_request = LabeledTraversalRequest {
            order: OutEdgeOrder::Descending,
            ..request
        };
        let mut descending = Vec::new();
        let _ = Traversal::visit_edges_with_inline_property_window(
            &graph,
            &descending_request,
            TraversalWindow::new(2, Some(3)),
            |slot, item| {
                descending.push((
                    slot.raw(),
                    item.edge.target,
                    item.inline_property.bytes.clone(),
                ));
                ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
        assert_eq!(
            descending,
            vec![
                (61, 62, (62u32 * 7).to_le_bytes().to_vec()),
                (60, 61, (61u32 * 7).to_le_bytes().to_vec()),
                (59, 60, (60u32 * 7).to_le_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn inline_property_window_preserves_sparse_live_rows_and_bytes() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(src, label, 4)
            .unwrap();
        for target in 1..=8u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    PayloadTestEdge::with_bytes(target, &(target * 7).to_le_bytes()),
                )
                .unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        graph.remove_edge_at_slot(src, label, 2).unwrap();
        graph.remove_edge_at_slot(src, label, 5).unwrap();

        let request = LabeledTraversalRequest {
            owner: src,
            label,
            order: OutEdgeOrder::Ascending,
        };
        let mut ascending = Vec::new();
        let _ = Traversal::visit_edges_with_inline_property_window(
            &graph,
            &request,
            TraversalWindow::new(1, Some(3)),
            |slot, item| {
                ascending.push((
                    slot.raw(),
                    item.edge.target,
                    item.inline_property.bytes.clone(),
                ));
                ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
        assert_eq!(
            ascending,
            vec![
                (1, 2, (2u32 * 7).to_le_bytes().to_vec()),
                (3, 4, (4u32 * 7).to_le_bytes().to_vec()),
                (4, 5, (5u32 * 7).to_le_bytes().to_vec()),
            ]
        );

        let descending_request = LabeledTraversalRequest {
            order: OutEdgeOrder::Descending,
            ..request
        };
        let mut descending = Vec::new();
        let _ = Traversal::visit_edges_with_inline_property_window(
            &graph,
            &descending_request,
            TraversalWindow::new(1, Some(3)),
            |slot, item| {
                descending.push((
                    slot.raw(),
                    item.edge.target,
                    item.inline_property.bytes.clone(),
                ));
                ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
        assert_eq!(
            descending,
            vec![
                (6, 7, (7u32 * 7).to_le_bytes().to_vec()),
                (4, 5, (5u32 * 7).to_le_bytes().to_vec()),
                (3, 4, (4u32 * 7).to_le_bytes().to_vec()),
            ]
        );
    }

    #[test]
    fn inline_property_zero_width_returns_empty_bytes_without_storage_read() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=3u32 {
            graph.insert_edge(src, label, TestEdge { target }).unwrap();
        }

        let item = graph
            .read_edge_with_inline_property(src, label, BucketEntryPosition::new(0))
            .unwrap()
            .expect("live row");
        assert_eq!(item.inline_property.width, 0);
        assert!(item.inline_property.bytes.is_empty());
    }

    #[test]
    fn stale_hybrid_overflow_replay_falls_back_to_canonical_read() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let a = graph.push_vertex(LabeledVertex::default()).unwrap();
        let b = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(a, label, 2)
            .unwrap();
        graph
            .ensure_label_bucket_inline_value_byte_width(b, label, 2)
            .unwrap();

        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    a,
                    label,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    label,
                    PayloadTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let mut scratch_b = LabeledPayloadValueBatchScratch::default();
        graph
            .visit_out_inline_value_batches_for_label(
                b,
                label,
                OutEdgeOrder::Ascending,
                &mut scratch_b,
                |_| {},
            )
            .unwrap();
        assert!(scratch_b.hybrid_overflow_replay.is_active());

        // Use B's replay for A: it must be rejected and fall back to canonical sparse read.
        let mut targets = Vec::new();
        let _ = graph
            .visit_edges_at_with_replay::<()>(
                a,
                label,
                &[BucketEntryPosition::new(0), BucketEntryPosition::new(47)],
                OutEdgeOrder::Ascending,
                Some(&scratch_b.hybrid_overflow_replay),
                |slot, edge| {
                    targets.push((slot.raw(), edge.target));
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(targets, vec![(0, 1), (47, 48)]);

        // A malformed payload-log length must fail closed rather than return
        // a partial or zero-filled property value.
        let vertex = graph.vertices.get(a);
        let BucketSearch::Found { slot, bucket } = graph.find_bucket(a, &vertex, label).unwrap()
        else {
            panic!("label bucket missing");
        };
        let malformed = bucket
            .with_inline_value_slab_slots(0)
            .try_with_payload_log(0, 1)
            .unwrap();
        graph
            .buckets
            .write_label_bucket_slot_for_test(slot, malformed)
            .unwrap();
        let error = graph
            .read_edge_with_inline_property(a, label, BucketEntryPosition::new(1))
            .unwrap_err();
        assert!(matches!(
            error,
            LabeledOperationError::PayloadLogRead(
                crate::lara::edge_inline_value::InlineValueLogReadError::MissingAscLogIndex { .. }
            )
        ));
        let visitor_error = graph
            .visit_edges_with_inline_property::<()>(a, label, OutEdgeOrder::Ascending, |_, _| {
                ControlFlow::Continue(())
            })
            .unwrap_err();
        assert!(matches!(
            visitor_error,
            LabeledOperationError::PayloadLogRead(
                crate::lara::edge_inline_value::InlineValueLogReadError::MissingAscLogIndex { .. }
            )
        ));
    }
}
