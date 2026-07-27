//! Labeled graph `traverse` implementation.
//!
//! ADR 0050 consolidated logical read surface. This module owns the canonical
//! labeled traversal API: typed bucket-local logical slots, topology-only and
//! inline-property reads, visitor-based early termination, and dense/hybrid/sparse
//! fast paths. It keeps raw storage geometry behind LARA and is the crate-wide default
//! after caller migration.
//!
//! The surface uses `std::ops::ControlFlow` for early termination and distinguishes
//! topology reads from inline-property reads.

use crate::{
    VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::{BucketDirectedness, BucketLabelKey},
        record::{LabelBucket, LabeledVertex},
    },
    lara::{
        edge::{OutEdgeSlabIter, OutEdgeVisitWindow},
        operation_error::LaraOperationError,
    },
    traits::CsrVertex,
    traits::{CsrEdge, CsrEdgeTombstone},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::ops::ControlFlow;

use crate::traverse::iter::{try_visit_indexed, visit_indexed};
use crate::traverse::{Traversal, TraversalOrder, TraversalRequest, TraversalWindow};

#[cfg(feature = "canbench")]
mod bench;

use super::{
    BucketSearch, LabeledLaraGraph, LabeledOutEdgesIter, OutEdgeOrder, error::LabeledOperationError,
};

use super::iter::{
    HybridOverflowEdgeReplay, LabeledEdgeInlinePropertyBatch,
    LabeledEdgeInlinePropertyBatchScratch, LabeledInlinePropertyValueBatch,
    LabeledInlinePropertyValueBatchScratch,
};

/// Scope for edge-finding operations: either a single label bucket or all buckets
/// owned by one vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeFindScope {
    /// Search every label bucket of the source vertex in the requested order.
    AllLabels,
    /// Search only the specified label bucket.
    Label(BucketLabelKey),
}

/// A live edge found by a predicate search, together with its logical identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundEdge<E> {
    /// The fully materialized edge value (topology plus inline property bytes when
    /// requested by the search variant).
    pub edge: E,
    /// The label bucket that owns the edge.
    pub label: BucketLabelKey,
    /// The logical slot of the edge within that bucket.
    pub slot: BucketEntryPosition,
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
/// This value enforces `bytes.len() == width`. Width zero is represented by an
/// empty byte vector and is a valid value; callers must not treat it as a missing
/// property. Small inline property byte slices (up to 16 bytes) are stored inline
/// to avoid per-edge heap allocations during sparse traversals; larger ones fall
/// back to a heap vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlinePropertyBytes {
    /// Declared byte width of the inline property for this edge's label bucket.
    width: u16,
    storage: InlinePropertyBytesStorage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InlinePropertyBytesStorage {
    Empty,
    Inline { len: u8, buf: [u8; 16] },
    Heap(Vec<u8>),
}

impl InlinePropertyBytes {
    /// Creates an empty zero-width inline-property value.
    #[inline]
    pub fn empty() -> Self {
        Self {
            width: 0,
            storage: InlinePropertyBytesStorage::Empty,
        }
    }

    /// Creates an inline-property value from a borrowed byte slice.
    ///
    /// # Panics
    ///
    /// Panics when `bytes.len() != width`. Callers that cannot guarantee the exact
    /// width must use the LARA read path, which returns a typed width-mismatch error.
    #[inline]
    pub fn from_bytes(width: u16, bytes: &[u8]) -> Self {
        assert_eq!(
            bytes.len(),
            usize::from(width),
            "InlinePropertyBytes width must match byte length"
        );
        if bytes.is_empty() {
            return Self::empty();
        }
        if bytes.len() <= 16 {
            let mut buf = [0u8; 16];
            buf[..bytes.len()].copy_from_slice(bytes);
            Self {
                width,
                storage: InlinePropertyBytesStorage::Inline {
                    len: bytes.len() as u8,
                    buf,
                },
            }
        } else {
            Self {
                width,
                storage: InlinePropertyBytesStorage::Heap(bytes.to_vec()),
            }
        }
    }

    /// Declared byte width of the inline property for this edge's label bucket.
    #[inline]
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Returns the exact byte contents of the inline property.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        match &self.storage {
            InlinePropertyBytesStorage::Empty => &[],
            InlinePropertyBytesStorage::Inline { len, buf } => &buf[..*len as usize],
            InlinePropertyBytesStorage::Heap(bytes) => bytes.as_slice(),
        }
    }

    /// Consumes the value and returns the bytes as a vector.
    #[inline]
    pub fn into_vec(self) -> Vec<u8> {
        match self.storage {
            InlinePropertyBytesStorage::Empty => Vec::new(),
            InlinePropertyBytesStorage::Inline { len, buf } => buf[..len as usize].to_vec(),
            InlinePropertyBytesStorage::Heap(bytes) => bytes,
        }
    }
}

/// A live edge row together with its exact inline-property bytes.
/// Target inline property bytes per batch; matches the legacy traverse batch sizing.
const EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES: usize = 2048;

#[inline]
fn slab_slot_deleted(slot: u32, deleted_slab_offsets: &[u32]) -> bool {
    deleted_slab_offsets.binary_search(&slot).is_ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeWithInlineProperty<E> {
    /// The topology-only edge row.
    pub edge: E,
    /// Exact inline-property bytes belonging to this row.
    pub inline_property: InlinePropertyBytes,
}

/// Raw storage location of a live edge row, contextualized by its owning bucket.
///
/// This is a measurement/maintenance primitive, not a query-time edge identity.
/// The high-bit encoding used internally for overflow-log entries is decoded at
/// this boundary; callers see an explicit [`StorageEdgeLocation`] variant.
#[cfg(feature = "adoption-fixtures")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageEdgeRef {
    /// Vertex that owns the bucket containing the row.
    pub owner: VertexId,
    /// Label of the bucket containing the row.
    pub label: BucketLabelKey,
    /// Physical storage location within the bucket.
    pub location: StorageEdgeLocation,
}

#[cfg(feature = "adoption-fixtures")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageEdgeLocation {
    /// Local slot inside the edge slab.
    SlabSlot(u32),
    /// Entry index inside the bucket's overflow-log chain.
    OverflowLogEntry(u32),
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

/// Ordering and deduplication helper for raw `u32` slot indices.
fn order_slot_indices(slots: &[u32], order: OutEdgeOrder) -> Vec<u32> {
    let mut ordered = slots.to_vec();
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

    /// Visits live edges in ascending logical slot order, stopping when the visitor breaks.
    ///
    /// This is a local helper for rank/select primitives over the logical slot extent; it does
    /// not expose order selection or offset semantics to callers.
    fn visit_live_edge_slots_until<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        logical_slots: u32,
        mut visit: impl FnMut(u32, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            return Ok(ControlFlow::Continue(()));
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(owner, &vertex, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        let overflow_chain = (bucket.overflow_log_head() >= 0).then(|| {
            self.edges.overflow_log_chain_asc_indices(
                self.inline_property_bytes_log_leaf(owner),
                bucket.overflow_log_head(),
            )
        });
        for slot_index in 0..logical_slots {
            if let EdgeSlotState::Live(edge) = self.read_edge_state_at_slot(
                owner,
                &vertex,
                bucket_index,
                &bucket,
                slot_index,
                label,
                overflow_chain.as_deref(),
            )? && let ControlFlow::Break(value) = visit(slot_index, edge)
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        Ok(ControlFlow::Continue(()))
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
        let _ = self.visit_live_edge_slots_until(owner, label, logical_slots, |slot, edge| {
            if slot >= before_slot.raw() {
                return ControlFlow::Break(());
            }
            if edge.neighbor_vid() == neighbor {
                count = count.saturating_add(1);
            }
            ControlFlow::Continue(())
        })?;
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
        let _ = self.visit_live_edge_slots_until(owner, label, logical_slots, |slot, edge| {
            if edge.neighbor_vid() != neighbor {
                return ControlFlow::Continue(());
            }
            if matching == ordinal {
                selected = Some(BucketEntryPosition::new(slot));
                return ControlFlow::Break(());
            }
            matching = matching.saturating_add(1);
            ControlFlow::Continue(())
        })?;
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

    /// Visits every live edge in a dense, tombstone-free label bucket by bulk-reading the
    /// topology slab in one call.
    #[inline]
    fn visit_dense_label_bucket_edges<B>(
        &self,
        _owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let degree = bucket.degree();
        if degree == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        let edge_bytes_len =
            (degree as usize)
                .checked_mul(E::BYTES)
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?;
        let mut raw_edges = vec![0u8; edge_bytes_len];
        self.edges
            .read_slots_contiguous(bucket.edge_start(), &mut raw_edges);
        match order {
            OutEdgeOrder::Ascending => {
                for slot in 0..degree {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                        .with_slot_index(slot)
                        .with_label_id(label.raw());
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    if let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for slot in (0..degree).rev() {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                        .with_slot_index(slot)
                        .with_label_id(label.raw());
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    if let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits every live edge together with its inline-property bytes in a dense,
    /// tombstone-free label bucket by bulk-reading both the topology slab and the
    /// inline-property bytes span in one call each.
    ///
    /// The returned [`EdgeWithInlineProperty`] carries the bytes in the dedicated
    /// wrapper; the edge itself is *not* redundantly modified via
    /// [`CsrEdge::with_stored_inline_property_bytes`], eliminating a per-edge byte copy.
    #[inline]
    fn visit_dense_label_bucket_edges_with_inline_property<B>(
        &self,
        _owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        inline_property_bytes: &[u8],
        width: u16,
        mut visit: impl FnMut(BucketEntryPosition, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert_eq!(
            inline_property_bytes.len(),
            bucket.degree() as usize * usize::from(width),
            "inline property span length must match dense degree times width"
        );
        let degree = bucket.degree();
        if degree == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        let edge_bytes_len =
            (degree as usize)
                .checked_mul(E::BYTES)
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?;
        let mut raw_edges = vec![0u8; edge_bytes_len];
        self.edges
            .read_slots_contiguous(bucket.edge_start(), &mut raw_edges);
        match order {
            OutEdgeOrder::Ascending => {
                for slot in 0..degree {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                        .with_slot_index(slot)
                        .with_label_id(label.raw());
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    let inline_property = if width == 0 {
                        InlinePropertyBytes::empty()
                    } else {
                        let start = slot as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytes::from_bytes(width, &inline_property_bytes[start..end])
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlineProperty {
                            edge,
                            inline_property,
                        },
                    ) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for slot in (0..degree).rev() {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                        .with_slot_index(slot)
                        .with_label_id(label.raw());
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    let inline_property = if width == 0 {
                        InlinePropertyBytes::empty()
                    } else {
                        let start = slot as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytes::from_bytes(width, &inline_property_bytes[start..end])
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlineProperty {
                            edge,
                            inline_property,
                        },
                    ) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }
    /// Visits every live edge for one label in the requested order.
    /// Build a single-bucket [`LabeledSpanIter`] for the sparse / hybrid traversal paths.
    fn single_bucket_span_iter<'a>(
        &'a self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_slot: u64,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        attach_inline_property_bytes: bool,
    ) -> Result<super::iter::LabeledSpanIter<'a, E, M>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.degree() == 0 {
            return Ok(super::iter::LabeledSpanIter::Empty);
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(vertex, bucket_slot)?;
        let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let successor_start =
            self.bucket_slab_window_end_exclusive_after_bucket(vertex, bucket_index, bucket)?;
        let acc =
            LabelEdgeSpanAccess::with_bucket(&self.buckets, slot, *bucket, successor_start, src);
        let log_chains = if attach_inline_property_bytes {
            self.bucket_inline_property_bytes_log_chain_opt(src, bucket)
        } else {
            None
        };
        match order {
            OutEdgeOrder::Descending => {
                let iter = self.edges.out_edges_iter(&acc, VertexId::from(0))?;
                Ok(super::iter::LabeledSpanIter::desc(
                    self,
                    src,
                    *vertex,
                    bucket_index,
                    *bucket,
                    bucket.bucket_label_key(),
                    log_chains,
                    attach_inline_property_bytes,
                    iter,
                ))
            }
            OutEdgeOrder::Ascending => {
                let iter = self.edges.asc_out_edges_iter(&acc, VertexId::from(0))?;
                Ok(super::iter::LabeledSpanIter::asc(
                    self,
                    src,
                    *vertex,
                    bucket_index,
                    *bucket,
                    bucket.bucket_label_key(),
                    log_chains,
                    attach_inline_property_bytes,
                    iter,
                ))
            }
        }
    }

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
        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, &vertex, label)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        if bucket.overflow_log_head() < 0
            && self.bucket_reserved_edge_slots(owner, &bucket) == bucket.degree()
        {
            return self.visit_dense_label_bucket_edges(owner, label, &bucket, order, visit);
        }
        let mut iter =
            self.single_bucket_span_iter(owner, &vertex, _bucket_slot, &bucket, order, false)?;
        while let Some(result) = iter.next_with_slot() {
            let (slot, edge) = result?;
            if let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge) {
                return Ok(ControlFlow::Break(value));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits every live logical slot for one label in the requested order.
    ///
    /// This is the slot-only counterpart to [`Self::visit_edges`]; the edge value
    /// is not materialized and inline-property bytes are not read.
    pub fn visit_edge_slots<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: impl FnMut(BucketEntryPosition) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.visit_edges(owner, label, order, |slot, _edge| visit(slot))
    }

    /// Collects outgoing edges for one label in the requested order.
    ///
    /// Topology-only collection that does not attach edge inline property bytes. Use
    /// [`Self::iter_edges_with_inline_property_for_label_next`] when the caller needs
    /// inline property bytes or comparisons that depend on them.
    pub(crate) fn iter_edges_for_label_next(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let mut out = Vec::new();
        let _ = self.visit_edges(owner, label, order, |_slot, edge| {
            out.push(edge);
            ControlFlow::<()>::Continue(())
        })?;
        Ok(out)
    }

    /// Collects outgoing edges with their inline-property bytes for one label.
    pub(crate) fn iter_edges_with_inline_property_for_label_next(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let mut out = Vec::new();
        let _ = self.visit_edges_with_inline_property(owner, label, order, |_slot, item| {
            let edge = item
                .edge
                .with_stored_inline_property_bytes(
                    item.inline_property.width,
                    item.inline_property.bytes(),
                )
                .with_label_id(label.raw());
            out.push(edge);
            ControlFlow::<()>::Continue(())
        })?;
        Ok(out)
    }

    /// Returns the n-th live edge (0-based) matching `pred` under `scope` and `order`.
    ///
    /// This is a rank/select primitive over the logical edge order: it counts
    /// matching live rows in the requested order and stops at the `nth` match.
    /// Inline-property bytes are read so that predicates may depend on them and the
    /// returned edge is fully materialized. Dense/hybrid/sparse fast paths inside
    /// [`Self::visit_edges_with_inline_property`] apply automatically.
    #[inline]
    pub(crate) fn find_nth_edge_with_inline_property_matching<F>(
        &self,
        owner: VertexId,
        scope: EdgeFindScope,
        order: OutEdgeOrder,
        nth: u32,
        mut pred: F,
    ) -> Result<Option<FoundEdge<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let mut matched = 0u32;
        let mut found: Option<FoundEdge<E>> = None;
        let mut visit = |label, slot, edge| {
            if pred(&edge) {
                if matched == nth {
                    found = Some(FoundEdge { edge, label, slot });
                    return ControlFlow::<()>::Break(());
                }
                matched = matched.saturating_add(1);
            }
            ControlFlow::<()>::Continue(())
        };

        match scope {
            EdgeFindScope::Label(label) => {
                let _ =
                    self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
                        let edge = item
                            .edge
                            .with_stored_inline_property_bytes(
                                item.inline_property.width,
                                item.inline_property.bytes(),
                            )
                            .with_label_id(label.raw());
                        visit(label, slot, edge)
                    })?;
            }
            EdgeFindScope::AllLabels => {
                let _ = self.find_visit_all_label_buckets(owner, order, |label, slot, edge| {
                    visit(label, slot, edge)
                })?;
            }
        }
        Ok(found)
    }

    /// Like [`Self::find_nth_edge_with_inline_property_matching`], but visits only
    /// topology values and does not read inline-property bytes. Use this when the
    /// predicate does not depend on inline-property bytes and materializing them
    /// would be wasted work.
    #[inline]
    pub(crate) fn find_nth_edge_matching<F>(
        &self,
        owner: VertexId,
        scope: EdgeFindScope,
        order: OutEdgeOrder,
        nth: u32,
        mut pred: F,
    ) -> Result<Option<FoundEdge<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let mut matched = 0u32;
        let mut found: Option<FoundEdge<E>> = None;
        let mut visit = |label, slot, edge| {
            if pred(&edge) {
                if matched == nth {
                    found = Some(FoundEdge { edge, label, slot });
                    return ControlFlow::<()>::Break(());
                }
                matched = matched.saturating_add(1);
            }
            ControlFlow::<()>::Continue(())
        };

        match scope {
            EdgeFindScope::Label(label) => {
                let _ = self.visit_edges(owner, label, order, |slot, edge| {
                    let edge = edge.with_label_id(label.raw());
                    visit(label, slot, edge)
                })?;
            }
            EdgeFindScope::AllLabels => {
                let _ = self.find_visit_all_label_buckets(owner, order, |label, slot, edge| {
                    visit(label, slot, edge)
                })?;
            }
        }
        Ok(found)
    }

    /// Visits every live edge of `owner` across all label buckets in the requested
    /// bucket order and slot order. Used by the all-label `find_nth_*` variants.
    ///
    /// Within each bucket the existing inline-property visitor is reused so
    /// dense/hybrid/sparse fast paths apply automatically.
    fn find_visit_all_label_buckets<B, V>(
        &self,
        owner: VertexId,
        order: OutEdgeOrder,
        mut visit: V,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        V: FnMut(BucketLabelKey, BucketEntryPosition, E) -> ControlFlow<B>,
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);

        if vertex.is_default_edge_labeled() {
            let label = self.bypass_storage_label_for(&vertex);
            return self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
                let edge = item
                    .edge
                    .with_stored_inline_property_bytes(
                        item.inline_property.width,
                        item.inline_property.bytes(),
                    )
                    .with_label_id(label.raw());
                visit(label, slot, edge)
            });
        }

        let deg = vertex.degree();
        if deg == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        match order {
            OutEdgeOrder::Ascending => {
                for bucket in &buckets {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) =
                        self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
                            let edge = item
                                .edge
                                .with_stored_inline_property_bytes(
                                    item.inline_property.width,
                                    item.inline_property.bytes(),
                                )
                                .with_label_id(label.raw());
                            visit(label, slot, edge)
                        })?
                    {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for bucket in buckets.iter().rev() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) =
                        self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
                            let edge = item
                                .edge
                                .with_stored_inline_property_bytes(
                                    item.inline_property.width,
                                    item.inline_property.bytes(),
                                )
                                .with_label_id(label.raw());
                            visit(label, slot, edge)
                        })?
                    {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits every live edge of `owner` across all label buckets in the requested
    /// bucket order and slot order, attaching each edge's inline-property bytes.
    ///
    /// Within each bucket the existing inline-property visitor is reused so
    /// dense/hybrid/sparse fast paths apply automatically. The visitor receives the
    /// fully materialized edge; callers that need offset/limit or predicate filtering
    /// can apply them inside the closure.
    #[inline]
    pub(crate) fn visit_all_labels_with_inline_property<B>(
        &self,
        owner: VertexId,
        order: OutEdgeOrder,
        mut visit: impl FnMut(E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);

        if vertex.is_default_edge_labeled() {
            let label = self.bypass_storage_label_for(&vertex);
            return match order {
                OutEdgeOrder::Ascending => {
                    for edge in self.edges.asc_out_edges(&self.vertices, owner)? {
                        if let ControlFlow::Break(value) = visit(edge.with_label_id(label.raw())) {
                            return Ok(ControlFlow::Break(value));
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                }
                OutEdgeOrder::Descending => {
                    let mut edges = OutEdgeSlabIter::try_new(
                        &self.edges,
                        vertex.base_slot_start(),
                        vertex.stored_degree(),
                        vertex.degree(),
                    )?;
                    let mut no_raw_matches = None;
                    while let Some(edge) = edges.next_live_edge_filtered(&mut no_raw_matches) {
                        if let ControlFlow::Break(value) = visit(edge.with_label_id(label.raw())) {
                            return Ok(ControlFlow::Break(value));
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                }
            };
        }

        let deg = vertex.degree();
        if deg == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        match order {
            OutEdgeOrder::Ascending => {
                for bucket in &buckets {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) = self.visit_edges_with_inline_property(
                        owner,
                        label,
                        order,
                        |_slot, item| {
                            let edge = item
                                .edge
                                .with_stored_inline_property_bytes(
                                    item.inline_property.width,
                                    item.inline_property.bytes(),
                                )
                                .with_label_id(label.raw());
                            visit(edge)
                        },
                    )? {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for bucket in buckets.iter().rev() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) = self.visit_edges_with_inline_property(
                        owner,
                        label,
                        order,
                        |_slot, item| {
                            let edge = item
                                .edge
                                .with_stored_inline_property_bytes(
                                    item.inline_property.width,
                                    item.inline_property.bytes(),
                                )
                                .with_label_id(label.raw());
                            visit(edge)
                        },
                    )? {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits every live edge of `owner` across all label buckets in the requested
    /// bucket order and slot order, without reading inline-property bytes.
    #[inline]
    pub(crate) fn visit_all_labels<B>(
        &self,
        owner: VertexId,
        order: OutEdgeOrder,
        mut visit: impl FnMut(E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);

        if vertex.is_default_edge_labeled() {
            let label = self.bypass_storage_label_for(&vertex);
            return self.visit_edges(owner, label, order, |_slot, edge| {
                visit(edge.with_label_id(label.raw()))
            });
        }

        let deg = vertex.degree();
        if deg == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        match order {
            OutEdgeOrder::Ascending => {
                for bucket in &buckets {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) =
                        self.visit_edges(owner, label, order, |_slot, edge| {
                            visit(edge.with_label_id(label.raw()))
                        })?
                    {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for bucket in buckets.iter().rev() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) =
                        self.visit_edges(owner, label, order, |_slot, edge| {
                            visit(edge.with_label_id(label.raw()))
                        })?
                    {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits outgoing edges whose bucket directedness matches `directedness` in the
    /// requested order, attaching each edge's inline-property bytes.
    ///
    /// This is the directedness-scoped counterpart to
    /// [`Self::visit_edges_with_inline_property`]: it walks only the label buckets whose
    /// [`BucketDirectedness`] matches, in bucket order and slot order determined by
    /// `order`, and yields fully materialized edges (topology + inline property bytes).
    #[inline]
    pub(crate) fn visit_out_edges_by_directedness_with_inline_property<B>(
        &self,
        owner: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: impl FnMut(E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        self.visit_out_edges_by_directedness_with_inline_property_for_vertex(
            owner,
            &vertex,
            directedness,
            order,
            visit,
        )
    }

    /// Unchecked variant of [`Self::visit_out_edges_by_directedness_with_inline_property`].
    ///
    /// The caller must ensure `owner` is within the vertex range.
    #[inline]
    pub(crate) fn visit_out_edges_by_directedness_with_inline_property_unchecked<B>(
        &self,
        owner: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: impl FnMut(E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert!(u32::from(owner) < self.vertices.len());
        let vertex = self.vertices.get(owner);
        self.visit_out_edges_by_directedness_with_inline_property_for_vertex(
            owner,
            &vertex,
            directedness,
            order,
            visit,
        )
    }

    /// Visits every live edge of `owner` across all label buckets whose directedness
    /// matches `directedness`, in the requested order, without reading inline-property
    /// bytes.
    #[inline]
    pub(crate) fn visit_out_edges_by_directedness<B>(
        &self,
        owner: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: impl FnMut(E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);

        if vertex.is_default_edge_labeled() {
            let label = self.bypass_storage_label_for(&vertex);
            if label.directedness() != directedness {
                return Ok(ControlFlow::Continue(()));
            }
            return self.visit_edges(owner, label, order, |_slot, edge| {
                visit(edge.with_label_id(label.raw()))
            });
        }

        let deg = vertex.degree();
        if deg == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        let strategy = Self::directedness_partition_strategy(directedness, order.ascending());
        let (lo, hi) = self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?;
        if lo >= hi {
            return Ok(ControlFlow::Continue(()));
        }
        let buckets = self.read_vertex_label_buckets_range(&vertex, lo, hi)?;

        match order {
            OutEdgeOrder::Ascending => {
                for bucket in &buckets {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) =
                        self.visit_edges(owner, label, order, |_slot, edge| {
                            visit(edge.with_label_id(label.raw()))
                        })?
                    {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for bucket in buckets.iter().rev() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) =
                        self.visit_edges(owner, label, order, |_slot, edge| {
                            visit(edge.with_label_id(label.raw()))
                        })?
                    {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_out_edges_by_directedness_with_inline_property_for_vertex<B>(
        &self,
        owner: VertexId,
        vertex: &LabeledVertex,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: impl FnMut(E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        // Bypass (default-label) vertex: a single bucket whose directedness must match.
        if vertex.is_default_edge_labeled() {
            let label = self.bypass_storage_label_for(vertex);
            if label.directedness() != directedness {
                return Ok(ControlFlow::Continue(()));
            }
            return self.visit_edges_with_inline_property(owner, label, order, |_slot, item| {
                let edge = item
                    .edge
                    .with_stored_inline_property_bytes(
                        item.inline_property.width,
                        item.inline_property.bytes(),
                    )
                    .with_label_id(label.raw());
                visit(edge)
            });
        }

        let deg = vertex.degree();
        if deg == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        let strategy = Self::directedness_partition_strategy(directedness, order.ascending());
        let (lo, hi) = self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?;
        if lo >= hi {
            return Ok(ControlFlow::Continue(()));
        }
        let buckets = self.read_vertex_label_buckets_range(vertex, lo, hi)?;

        // Visit matching buckets in the requested bucket order, and within each bucket
        // visit slots in the requested slot order.  Per-bucket we reuse the existing
        // inline-property visitor so dense/hybrid/sparse fast paths apply automatically.
        match order {
            OutEdgeOrder::Ascending => {
                for bucket in &buckets {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) = self.visit_edges_with_inline_property(
                        owner,
                        label,
                        order,
                        |_slot, item| {
                            let edge = item
                                .edge
                                .with_stored_inline_property_bytes(
                                    item.inline_property.width,
                                    item.inline_property.bytes(),
                                )
                                .with_label_id(label.raw());
                            visit(edge)
                        },
                    )? {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
            OutEdgeOrder::Descending => {
                for bucket in buckets.iter().rev() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let label = bucket.bucket_label_key();
                    if let ControlFlow::Break(value) = self.visit_edges_with_inline_property(
                        owner,
                        label,
                        order,
                        |_slot, item| {
                            let edge = item
                                .edge
                                .with_stored_inline_property_bytes(
                                    item.inline_property.width,
                                    item.inline_property.bytes(),
                                )
                                .with_label_id(label.raw());
                            visit(edge)
                        },
                    )? {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits a bounded window of live edges for one label in the requested order.
    ///
    /// For dense, tombstone-free label buckets this bulk-reads the topology slab
    /// and applies the offset/limit directly, avoiding the per-slot round trips
    /// that the generic sparse path pays.
    pub(crate) fn visit_edges_window<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        window: TraversalWindow,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if window.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            let mut offset = window.offset;
            let mut remaining = window.limit;
            return match order {
                OutEdgeOrder::Ascending => {
                    let mut iter = self.edges.asc_out_edges_iter(&self.vertices, owner)?;
                    while let Some((slot, edge)) = iter.next_with_slot() {
                        if offset != 0 {
                            offset -= 1;
                            continue;
                        }
                        if let ControlFlow::Break(value) =
                            visit(BucketEntryPosition::new(slot), edge)
                        {
                            return Ok(ControlFlow::Break(value));
                        }
                        if let Some(remaining) = remaining.as_mut() {
                            *remaining -= 1;
                            if *remaining == 0 {
                                break;
                            }
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                }
                OutEdgeOrder::Descending => {
                    let mut iter = self.edges.out_edges_iter(&self.vertices, owner)?;
                    while let Some((slot, edge)) = iter.next_with_slot() {
                        if offset != 0 {
                            offset -= 1;
                            continue;
                        }
                        if let ControlFlow::Break(value) =
                            visit(BucketEntryPosition::new(slot), edge)
                        {
                            return Ok(ControlFlow::Break(value));
                        }
                        if let Some(remaining) = remaining.as_mut() {
                            *remaining -= 1;
                            if *remaining == 0 {
                                break;
                            }
                        }
                    }
                    Ok(ControlFlow::Continue(()))
                }
            };
        }

        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, &vertex, label)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        // Dense, tombstone-free fast path: bulk-read the slab and apply offset/limit.
        if bucket.overflow_log_head() < 0
            && self.bucket_reserved_edge_slots(owner, &bucket) == bucket.degree()
        {
            let degree = bucket.degree();
            let edge_bytes_len =
                (degree as usize)
                    .checked_mul(E::BYTES)
                    .ok_or(LabeledOperationError::from(
                        LaraOperationError::CollectAllocationOverflow,
                    ))?;
            let mut raw_edges = vec![0u8; edge_bytes_len];
            self.edges
                .read_slots_contiguous(bucket.edge_start(), &mut raw_edges);

            let offset = window.offset.min(degree);
            let limit = window
                .limit
                .map(|l| l.min(degree - offset))
                .unwrap_or(degree - offset);

            match order {
                OutEdgeOrder::Ascending => {
                    let mut visited = 0u32;
                    for slot in offset..degree {
                        if visited >= limit {
                            break;
                        }
                        let off = slot as usize * E::BYTES;
                        let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(label.raw());
                        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                            continue;
                        }
                        if let ControlFlow::Break(value) =
                            visit(BucketEntryPosition::new(slot), edge)
                        {
                            return Ok(ControlFlow::Break(value));
                        }
                        visited += 1;
                    }
                }
                OutEdgeOrder::Descending => {
                    let start = degree.saturating_sub(offset + limit);
                    let end = degree.saturating_sub(offset);
                    let mut visited = 0u32;
                    for slot in (start..end).rev() {
                        if visited >= limit {
                            break;
                        }
                        let off = slot as usize * E::BYTES;
                        let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(label.raw());
                        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                            continue;
                        }
                        if let ControlFlow::Break(value) =
                            visit(BucketEntryPosition::new(slot), edge)
                        {
                            return Ok(ControlFlow::Break(value));
                        }
                        visited += 1;
                    }
                }
            }
            return Ok(ControlFlow::Continue(()));
        }

        // Sparse path: skip `offset` live rows via the span iterator, then take `limit`.
        let mut iter =
            self.single_bucket_span_iter(owner, &vertex, _bucket_slot, &bucket, order, false)?;
        if iter.try_advance_by(window.offset as usize).is_err() {
            return Ok(ControlFlow::Continue(()));
        }
        let mut remaining = window.limit;
        while let Some(result) = iter.next_with_slot() {
            let (slot, edge) = result?;
            if let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge) {
                return Ok(ControlFlow::Break(value));
            }
            if let Some(remaining) = remaining.as_mut() {
                *remaining -= 1;
                if *remaining == 0 {
                    break;
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    pub(crate) fn visit_edges_with_inline_property<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        visit: impl FnMut(BucketEntryPosition, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        self.visit_edges_with_inline_property_impl(owner, &vertex, label, order, visit)
    }

    /// Visits one label's edges while materializing inline properties directly on the edge.
    ///
    /// This preserves the allocation-free dense-bucket path used by the legacy public API.
    /// The typed `EdgeWithInlineProperty` path remains the general-purpose traversal surface,
    /// but it is unnecessarily expensive when the caller immediately needs an `E` value.
    pub(crate) fn visit_edges_for_label<Visit>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        self.visit_edges_for_label_impl(owner, &vertex, label, order, &mut visit)
    }

    /// Unchecked variant of [`Self::visit_edges_for_label`].
    #[inline]
    pub(crate) fn visit_edges_for_label_unchecked<Visit>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        debug_assert!(u32::from(owner) < self.vertices.len());
        let vertex = self.vertices.get(owner);
        self.visit_edges_for_label_impl(owner, &vertex, label, order, &mut visit)
    }

    fn visit_edges_for_label_impl<Visit>(
        &self,
        owner: VertexId,
        vertex: &LabeledVertex,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(vertex) {
                return Ok(());
            }
            return match order {
                OutEdgeOrder::Ascending => {
                    for edge in self.edges.asc_out_edges(&self.vertices, owner)? {
                        visit(edge.with_label_id(label.raw()));
                    }
                    Ok(())
                }
                OutEdgeOrder::Descending => self
                    .edges
                    .visit_out_edges(
                        &self.vertices,
                        owner,
                        None,
                        None,
                        None::<&mut dyn FnMut(&[u8]) -> bool>,
                        |_| true,
                        |edge| visit(edge.with_label_id(label.raw())),
                    )
                    .map_err(Into::into),
            };
        }

        let BucketSearch::Found { slot, bucket } = self.find_bucket(owner, vertex, label)? else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }

        if bucket.inline_property_bytes_log_head() < 0
            && bucket.overflow_log_head() < 0
            && self.bucket_reserved_edge_slots(owner, &bucket) == bucket.degree()
        {
            let width = bucket.inline_property_byte_width();
            let inline_property_bytes = if width > 0 {
                self.read_bucket_inline_property_bytes_span(owner, &bucket, 0, bucket.degree())?
            } else {
                Vec::new()
            };
            let degree = bucket.degree();
            let edge_bytes_len =
                (degree as usize)
                    .checked_mul(E::BYTES)
                    .ok_or(LabeledOperationError::from(
                        LaraOperationError::CollectAllocationOverflow,
                    ))?;
            let mut raw_edges = vec![0u8; edge_bytes_len];
            self.edges
                .read_slots_contiguous(bucket.edge_start(), &mut raw_edges);
            let mut visit_slot = |slot| {
                let off = slot as usize * E::BYTES;
                let mut edge = E::read_from(&raw_edges[off..off + E::BYTES])
                    .with_slot_index(slot)
                    .with_label_id(label.raw());
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    return;
                }
                if width > 0 {
                    let start = slot as usize * usize::from(width);
                    let end = start + usize::from(width);
                    edge = edge.with_stored_inline_property_bytes(
                        width,
                        &inline_property_bytes[start..end],
                    );
                }
                visit(edge);
            };
            match order {
                OutEdgeOrder::Ascending => {
                    for slot in 0..degree {
                        visit_slot(slot);
                    }
                }
                OutEdgeOrder::Descending => {
                    for slot in (0..degree).rev() {
                        visit_slot(slot);
                    }
                }
            }
            return Ok(());
        }

        let mut iter = self.single_bucket_span_iter(owner, vertex, slot, &bucket, order, true)?;
        while let Some(result) = iter.next_with_slot() {
            let (_, edge) = result?;
            visit(edge.with_label_id(label.raw()));
        }
        Ok(())
    }

    /// Unchecked variant of [`Self::visit_edges_with_inline_property`].
    ///
    /// The caller must ensure `owner` is within the vertex range.
    #[inline]
    pub(crate) fn visit_edges_with_inline_property_unchecked<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        visit: impl FnMut(BucketEntryPosition, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert!(u32::from(owner) < self.vertices.len());
        let vertex = self.vertices.get(owner);
        self.visit_edges_with_inline_property_impl(owner, &vertex, label, order, visit)
    }

    fn visit_edges_with_inline_property_impl<B>(
        &self,
        owner: VertexId,
        vertex: &LabeledVertex,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: impl FnMut(BucketEntryPosition, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(vertex) {
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
        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, vertex, label)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        let width = bucket.inline_property_byte_width();
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        // Fast path for dense, tombstone-free buckets: bulk-read the topology slab in one
        // call, and bulk-read the inline property bytes span when the bucket carries inline property bytes. This
        // avoids per-slot `read_slot` round-trips and preserves the performance contract of
        // `visit_edges_with_inline_property` for hot dense scans.
        if bucket.inline_property_bytes_log_head() < 0
            && bucket.overflow_log_head() < 0
            && self.bucket_reserved_edge_slots(owner, &bucket) == bucket.degree()
        {
            let inline_property_bytes = if width > 0 {
                self.read_bucket_inline_property_bytes_span(owner, &bucket, 0, bucket.degree())?
            } else {
                Vec::new()
            };
            return self.visit_dense_label_bucket_edges_with_inline_property(
                owner,
                label,
                &bucket,
                order,
                &inline_property_bytes,
                width,
                visit,
            );
        }

        // Fast path for slab-only buckets with tombstones: bulk-read edge slab and
        // inline-property bytes, then skip deleted/tombstoned slots locally.  This avoids
        // per-slot Vec<u8> allocations and redundant copies into the edge value.
        if bucket.overflow_log_head() < 0 && bucket.inline_property_bytes_log_head() < 0 {
            let reserved = self.bucket_reserved_edge_slots(owner, &bucket);
            let inline_property_bytes = if width > 0 {
                self.read_bucket_inline_property_bytes_span(owner, &bucket, 0, reserved)?
            } else {
                Vec::new()
            };
            return self.visit_slab_only_label_bucket_edges_with_inline_property(
                owner,
                label,
                &bucket,
                order,
                reserved,
                &inline_property_bytes,
                width,
                visit,
            );
        }

        // Hybrid and other sparse buckets stream through the single-bucket span iterator
        // with inline-property bytes attached, then wrap each edge in the typed property view.
        let width = bucket.inline_property_byte_width();
        let mut iter =
            self.single_bucket_span_iter(owner, vertex, _bucket_slot, &bucket, order, true)?;
        while let Some(result) = iter.next_with_slot() {
            let (slot, edge) = result?;
            let inline_property = if width == 0 {
                InlinePropertyBytes::empty()
            } else {
                InlinePropertyBytes::from_bytes(width, edge.edge_inline_property_bytes())
            };
            if let ControlFlow::Break(value) = visit(
                BucketEntryPosition::new(slot),
                EdgeWithInlineProperty {
                    edge,
                    inline_property,
                },
            ) {
                return Ok(ControlFlow::Break(value));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits every live edge together with its inline-property bytes in a slab-only
    /// label bucket by bulk-reading the edge slab and inline-property bytes span once each.
    ///
    /// `reserved` is the number of logical slots reserved by the edge store.  When there
    /// are tombstones, `reserved > degree` and we skip deleted/tombstoned slots.  The bucket
    /// must have no edge overflow log and no inline-property-bytes overflow log.
    ///
    /// The inline-property bytes span is indexed by **live ordinal**, not by physical slot
    /// index, so this path recomputes the live ordinal for each non-deleted slot it yields.
    #[inline]
    fn visit_slab_only_label_bucket_edges_with_inline_property<B>(
        &self,
        _owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        reserved: u32,
        inline_property_bytes: &[u8],
        width: u16,
        mut visit: impl FnMut(BucketEntryPosition, EdgeWithInlineProperty<E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if reserved == 0 || bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        let stored_slots = bucket.stored_slots;
        let edge_bytes_len =
            (stored_slots as usize)
                .checked_mul(E::BYTES)
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?;
        let mut raw_edges = vec![0u8; edge_bytes_len];
        self.edges
            .read_slots_contiguous(bucket.edge_start(), &mut raw_edges);

        // Walk slots in the requested scan order, maintaining the live ordinal that
        // indexes the dense inline-property-bytes span.
        match order {
            OutEdgeOrder::Ascending => {
                let mut ordinal = 0u32;
                for slot in 0..reserved {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                        .with_slot_index(slot)
                        .with_label_id(label.raw());
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    let inline_property = if width == 0 {
                        InlinePropertyBytes::empty()
                    } else {
                        let start = ordinal as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytes::from_bytes(width, &inline_property_bytes[start..end])
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlineProperty {
                            edge,
                            inline_property,
                        },
                    ) {
                        return Ok(ControlFlow::Break(value));
                    }
                    ordinal = ordinal.saturating_add(1);
                }
            }
            OutEdgeOrder::Descending => {
                // First pass: count live ordinals so we can walk them in reverse.
                let mut live_count = 0u32;
                for slot in 0..reserved {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES]);
                    if !edge.is_deleted_slot() && !edge.is_tombstone_edge() {
                        live_count = live_count.saturating_add(1);
                    }
                }
                let mut ordinal = live_count;
                for slot in (0..reserved).rev() {
                    let off = slot as usize * E::BYTES;
                    let edge = E::read_from(&raw_edges[off..off + E::BYTES])
                        .with_slot_index(slot)
                        .with_label_id(label.raw());
                    if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                        continue;
                    }
                    ordinal = ordinal.saturating_sub(1);
                    let inline_property = if width == 0 {
                        InlinePropertyBytes::empty()
                    } else {
                        let start = ordinal as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytes::from_bytes(width, &inline_property_bytes[start..end])
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlineProperty {
                            edge,
                            inline_property,
                        },
                    ) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits outgoing inline property value bytes for one label in batches.
    pub(crate) fn visit_out_inline_property_batches_for_label_next<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        mut visit: impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        // A phase-1 call always resets the cached replay so a stale replay from a previous
        // `(owner, label)` can never survive an early return and be reused by a later phase 2.
        scratch.hybrid_overflow_replay.clear();
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            // Default-label bypass edges have no inline property bytes; emit nothing.
            return Ok(ControlFlow::Continue(()));
        }
        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, &vertex, label)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 || bucket.inline_property_byte_width() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        if crate::labeled::invariants::bucket_dense_inline_property_batch_eligible(&bucket) {
            return self.visit_dense_out_inline_property_batches_for_bucket_next(
                owner, label, &bucket, order, scratch, &mut visit,
            );
        }

        if bucket.overflow_log_head() >= 0 {
            return self.visit_hybrid_out_inline_property_batches_for_bucket_next(
                owner, label, &bucket, order, scratch, &mut visit,
            );
        }

        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let mut collected_slot_indices = Vec::new();
        let mut collected_values = Vec::new();
        let result = self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
            if collected_slot_indices.len() == batch_edges {
                if let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id: label,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &collected_slot_indices,
                    values: &collected_values,
                    dense: false,
                }) {
                    return ControlFlow::Break(value);
                }
                scratch.clear();
                collected_slot_indices.clear();
                collected_values.clear();
            }
            collected_slot_indices.push(slot.raw());
            collected_values.extend_from_slice(item.inline_property.bytes());
            ControlFlow::Continue(())
        })?;
        if let ControlFlow::Break(value) = result {
            return Ok(ControlFlow::Break(value));
        }
        if !collected_slot_indices.is_empty()
            && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                label_id: label,
                byte_width: bucket.inline_property_byte_width(),
                order,
                slot_indices: &collected_slot_indices,
                values: &collected_values,
                dense: false,
            })
        {
            return Ok(ControlFlow::Break(value));
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_hybrid_out_inline_property_batches_for_bucket_next<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let slab_slots = self.bucket_slab_prefix_slots(owner, bucket);
        let inline_property_bytes_slab_slots = bucket.inline_property_bytes_slab_slots();
        let log_chains = self.bucket_inline_property_bytes_log_chain_opt(owner, bucket);

        if order == OutEdgeOrder::Descending {
            let prefetched = self.edges.prefetch_overflow_log_replay_desc(
                self.inline_property_bytes_log_leaf(owner),
                bucket.overflow_log_head(),
            )?;
            let reserved_log_slots = u32::try_from(prefetched.0.len())
                .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
            let reserved = bucket.stored_slots.saturating_add(reserved_log_slots);
            // If the bucket has tombstones in the overflow log, fall back to the sparse
            // iterator path so we emit only live edges and their values.
            if reserved != bucket.degree() {
                return self.visit_sparse_out_inline_property_batches_for_bucket_next(
                    owner, label, bucket, order, scratch, visit,
                );
            }
            let deleted_slab_offsets = prefetched.1.clone();
            let edge_slab_slots = slab_slots;
            let value_slab_slots = inline_property_bytes_slab_slots.min(edge_slab_slots);
            // Descending order: overflow log entries have the highest logical slots,
            // followed by edge-slab entries whose inline_property_bytes lives in the overflow log,
            // followed by the dense inline_property_bytes-slab prefix.
            let _ = self.emit_hybrid_overflow_log_inline_property_values_desc_next(
                owner,
                bucket,
                prefetched,
                order,
                scratch,
                visit,
                log_chains.as_ref(),
            )?;
            let _ = self.visit_inline_property_bytes_log_ordinals_in_edge_slab_next(
                owner,
                bucket,
                value_slab_slots,
                edge_slab_slots,
                order,
                scratch,
                visit,
                log_chains.as_ref(),
            )?;
            let _ = self.visit_dense_out_inline_property_batches_for_slab_prefix_next(
                bucket,
                value_slab_slots,
                &deleted_slab_offsets,
                order,
                scratch,
                visit,
                false,
                true,
            )?;
            return Ok(ControlFlow::Continue(()));
        }

        let prefetched = self.edges.prefetch_overflow_log_inserted_tags_asc(
            self.inline_property_bytes_log_leaf(owner),
            bucket.overflow_log_head(),
        )?;
        let reserved_log_slots =
            u32::try_from(prefetched.0.len()).map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        let reserved = bucket.stored_slots.saturating_add(reserved_log_slots);
        if reserved != bucket.degree() {
            return self.visit_sparse_out_inline_property_batches_for_bucket_next(
                owner, label, bucket, order, scratch, visit,
            );
        }
        let deleted_slab_offsets = prefetched.1.clone();
        let edge_slab_slots = slab_slots;
        let value_slab_slots = inline_property_bytes_slab_slots.min(edge_slab_slots);
        let _ = self.visit_dense_out_inline_property_batches_for_slab_prefix_next(
            bucket,
            value_slab_slots,
            &deleted_slab_offsets,
            order,
            scratch,
            visit,
            false,
            true,
        )?;
        let _ = self.visit_inline_property_bytes_log_ordinals_in_edge_slab_next(
            owner,
            bucket,
            value_slab_slots,
            edge_slab_slots,
            order,
            scratch,
            visit,
            log_chains.as_ref(),
        )?;
        let _ = self.emit_hybrid_overflow_log_inline_property_values_asc_next(
            owner,
            bucket,
            prefetched,
            order,
            scratch,
            visit,
            log_chains.as_ref(),
        )?;
        Ok(ControlFlow::Continue(()))
    }

    fn visit_dense_out_inline_property_batches_for_slab_prefix_next<B>(
        &self,
        bucket: &LabelBucket,
        scan_slots: u32,
        deleted_slab_offsets: &[u32],
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
        _dense: bool,
        omit_edge_slab_reads: bool,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if scan_slots == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let label_id = bucket.bucket_label_key();

        // Fast path: when the caller has already verified the edge slab is live and
        // there are no tombstone offsets to skip, bulk-read the contiguous inline_property_bytes
        // span and slice it into batches. This matches the legacy hybrid batch path
        // and avoids per-slot stable-memory round-trips.
        if omit_edge_slab_reads && deleted_slab_offsets.is_empty() {
            let mut remaining = scan_slots;
            while remaining > 0 {
                let take = remaining.min(batch_edges as u32);
                let first_ordinal = match order {
                    OutEdgeOrder::Descending => remaining - take,
                    OutEdgeOrder::Ascending => scan_slots - remaining,
                };
                scratch.clear();
                scratch.slot_indices.reserve(take as usize);
                scratch.values.reserve(take as usize * width);
                let offset = crate::labeled::invariants::inline_property_bytes_byte_offset_at_slot(
                    bucket,
                    first_ordinal,
                )?;
                let byte_len = take as usize * width;
                let raw_values = scratch.io_inline_property_bytes_slice_mut(byte_len);
                self.values.read_bytes(offset, raw_values);
                // Copy out the slice values into the public batch buffers so the borrow
                // of the reusable IO buffer can end before the visitor callback runs.
                scratch
                    .values
                    .extend_from_slice(&scratch.io_inline_property_bytes[..byte_len]);
                match order {
                    OutEdgeOrder::Ascending => {
                        for i in 0..take as usize {
                            let ordinal = first_ordinal + i as u32;
                            scratch.slot_indices.push(ordinal);
                        }
                    }
                    OutEdgeOrder::Descending => {
                        for i in (0..take as usize).rev() {
                            let ordinal = first_ordinal + i as u32;
                            scratch.slot_indices.push(ordinal);
                        }
                    }
                }
                if !scratch.slot_indices.is_empty()
                    && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                        label_id,
                        byte_width: bucket.inline_property_byte_width(),
                        order,
                        slot_indices: &scratch.slot_indices,
                        values: &scratch.values,
                        dense: false,
                    })
                {
                    return Ok(ControlFlow::Break(value));
                }
                remaining -= take;
            }
            return Ok(ControlFlow::Continue(()));
        }

        let mut ordered: Vec<(u32, u32)> = (0..scan_slots)
            .filter(|slot| {
                if slab_slot_deleted(*slot, deleted_slab_offsets) {
                    return false;
                }
                if omit_edge_slab_reads {
                    return true;
                }
                let edge = self.edges.read_slot(bucket.edge_start() + u64::from(*slot));
                !edge.is_deleted_slot() && !edge.is_tombstone_edge()
            })
            .enumerate()
            .map(|(ordinal, slot)| {
                u32::try_from(ordinal)
                    .map(|ordinal| (slot, ordinal))
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)
            })
            .collect::<Result<_, _>>()?;
        if matches!(order, OutEdgeOrder::Descending) {
            ordered.reverse();
        }
        for chunk in ordered.chunks(batch_edges) {
            scratch.clear();
            for &(slot, ordinal) in chunk {
                let offset = crate::labeled::invariants::inline_property_bytes_byte_offset_at_slot(
                    bucket, ordinal,
                )?;
                let value = scratch.io_inline_property_bytes_slice_mut(width);
                self.values.read_bytes(offset, value);
                scratch.slot_indices.push(slot);
                scratch
                    .values
                    .extend_from_slice(&scratch.io_inline_property_bytes[..width]);
            }
            if !scratch.slot_indices.is_empty()
                && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: false,
                })
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_inline_property_bytes_log_ordinals_in_edge_slab_next<B>(
        &self,
        owner: VertexId,
        bucket: &LabelBucket,
        start_ordinal: u32,
        end_ordinal: u32,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
        log_chain: Option<&Vec<u32>>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if start_ordinal >= end_ordinal {
            return Ok(ControlFlow::Continue(()));
        }
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let label_id = bucket.bucket_label_key();
        let mut remaining = end_ordinal - start_ordinal;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            scratch.clear();
            match order {
                OutEdgeOrder::Ascending => {
                    let first = start_ordinal + remaining - take;
                    for ordinal in first..first + take {
                        self.read_bucket_inline_property_bytes_for_slot_into(
                            owner,
                            bucket,
                            ordinal,
                            log_chain,
                            &mut scratch.io_inline_property_bytes,
                        )?;
                        scratch.slot_indices.push(ordinal);
                        scratch
                            .values
                            .extend_from_slice(&scratch.io_inline_property_bytes[..width]);
                    }
                }
                OutEdgeOrder::Descending => {
                    let high = start_ordinal + remaining;
                    for ordinal in (high - take..high).rev() {
                        self.read_bucket_inline_property_bytes_for_slot_into(
                            owner,
                            bucket,
                            ordinal,
                            log_chain,
                            &mut scratch.io_inline_property_bytes,
                        )?;
                        scratch.slot_indices.push(ordinal);
                        scratch
                            .values
                            .extend_from_slice(&scratch.io_inline_property_bytes[..width]);
                    }
                }
            }
            if !scratch.slot_indices.is_empty()
                && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: false,
                })
            {
                return Ok(ControlFlow::Break(value));
            }
            remaining -= take;
        }
        Ok(ControlFlow::Continue(()))
    }

    fn emit_hybrid_overflow_log_inline_property_values_desc_next<B>(
        &self,
        owner: VertexId,
        bucket: &LabelBucket,
        prefetched: (Vec<Option<u32>>, Vec<u32>, Vec<u8>),
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
        log_chains: Option<&Vec<u32>>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let leaf = self.inline_property_bytes_log_leaf(owner);
        let (mut replay_entries, mut deleted_slab_offsets, log_table) = prefetched;
        deleted_slab_offsets.sort_unstable();
        let slab_slots = self.bucket_slab_prefix_slots(owner, bucket);
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let reserved_log_slots = u32::try_from(replay_entries.len())
            .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        let mut next_log_slot = slab_slots
            .saturating_add(reserved_log_slots)
            .saturating_sub(1);
        let mut next_inline_property_bytes_ordinal = bucket.degree().saturating_sub(1);

        let mut live_slots = Vec::with_capacity(replay_entries.len());
        for &log_entry in &replay_entries {
            if log_entry.is_none() {
                next_log_slot = next_log_slot.saturating_sub(1);
                continue;
            }
            let slot = next_log_slot;
            next_log_slot = next_log_slot.saturating_sub(1);
            live_slots.push((slot, next_inline_property_bytes_ordinal));
            next_inline_property_bytes_ordinal =
                next_inline_property_bytes_ordinal.saturating_sub(1);
        }
        for chunk in live_slots.chunks(batch_edges) {
            scratch.clear();
            self.append_ordered_inline_property_bytes_ordinals_next(
                owner, bucket, chunk, log_chains, scratch,
            )?;
            if !scratch.slot_indices.is_empty()
                && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: false,
                })
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        let replay = &mut scratch.hybrid_overflow_replay;
        replay.clear();
        replay.src = owner;
        replay.leaf = leaf;
        replay.label_id = label_id;
        replay.slab_slots = slab_slots;
        replay.degree = bucket.degree();
        replay.stored_slots = bucket.stored_slots;
        replay.overflow_log_head = bucket.overflow_log_head();
        replay.edge_start = bucket.edge_start();
        replay.deleted_slab_offsets = deleted_slab_offsets.clone();
        replay.log_table = log_table;
        replay_entries.reverse();
        replay.log_indices_by_slot = replay_entries;
        Ok(ControlFlow::Continue(()))
    }

    fn emit_hybrid_overflow_log_inline_property_values_asc_next<B>(
        &self,
        owner: VertexId,
        bucket: &LabelBucket,
        prefetched: (Vec<Option<u32>>, Vec<u32>, Vec<u8>),
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
        log_chains: Option<&Vec<u32>>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let leaf = self.inline_property_bytes_log_leaf(owner);
        let (inserted_entries, deleted_slab_offsets, log_table) = prefetched;
        let slab_slots = self.bucket_slab_prefix_slots(owner, bucket);
        let label_id = bucket.bucket_label_key();
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let mut next_inserted_log_slot = slab_slots;
        let mut next_inline_property_bytes_ordinal = slab_slots
            .saturating_sub(u32::try_from(deleted_slab_offsets.len()).unwrap_or(u32::MAX));

        let mut live_slots = Vec::with_capacity(inserted_entries.len());
        for &log_entry in &inserted_entries {
            if log_entry.is_none() {
                next_inserted_log_slot = next_inserted_log_slot.saturating_add(1);
                continue;
            }
            let slot = next_inserted_log_slot;
            next_inserted_log_slot = next_inserted_log_slot.saturating_add(1);
            live_slots.push((slot, next_inline_property_bytes_ordinal));
            next_inline_property_bytes_ordinal =
                next_inline_property_bytes_ordinal.saturating_add(1);
        }
        for chunk in live_slots.chunks(batch_edges) {
            scratch.clear();
            self.append_ordered_inline_property_bytes_ordinals_next(
                owner, bucket, chunk, log_chains, scratch,
            )?;
            if !scratch.slot_indices.is_empty()
                && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: false,
                })
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        let replay = &mut scratch.hybrid_overflow_replay;
        replay.clear();
        replay.src = owner;
        replay.leaf = leaf;
        replay.label_id = label_id;
        replay.slab_slots = slab_slots;
        replay.degree = bucket.degree();
        replay.stored_slots = bucket.stored_slots;
        replay.overflow_log_head = bucket.overflow_log_head();
        replay.edge_start = bucket.edge_start();
        replay.deleted_slab_offsets = deleted_slab_offsets.clone();
        replay.log_table = log_table;
        replay.log_indices_by_slot = inserted_entries;
        Ok(ControlFlow::Continue(()))
    }

    fn append_ordered_inline_property_bytes_ordinals_next(
        &self,
        owner: VertexId,
        bucket: &LabelBucket,
        slots_and_ordinals: &[(u32, u32)],
        log_chain: Option<&Vec<u32>>,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
    ) -> Result<(), LabeledOperationError> {
        let width = usize::from(bucket.inline_property_byte_width());
        let slab_slots = bucket.inline_property_bytes_slab_slots();
        let mut i = 0usize;
        while i < slots_and_ordinals.len() {
            let (_, ordinal) = slots_and_ordinals[i];
            if ordinal >= slab_slots {
                self.read_bucket_inline_property_bytes_for_slot_into(
                    owner,
                    bucket,
                    ordinal,
                    log_chain,
                    &mut scratch.io_inline_property_bytes,
                )?;
                scratch.slot_indices.push(slots_and_ordinals[i].0);
                scratch
                    .values
                    .extend_from_slice(&scratch.io_inline_property_bytes[..width]);
                i += 1;
                continue;
            }

            let mut end = i + 1;
            while end < slots_and_ordinals.len() {
                let previous = slots_and_ordinals[end - 1].1;
                let next = slots_and_ordinals[end].1;
                if next >= slab_slots || previous.abs_diff(next) != 1 {
                    break;
                }
                end += 1;
            }
            let first = slots_and_ordinals[i].1;
            let last = slots_and_ordinals[end - 1].1;
            let low = first.min(last);
            let count = end - i;
            let byte_len = count
                .checked_mul(width)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let offset =
                crate::labeled::invariants::inline_property_bytes_byte_offset_at_slot(bucket, low)?;
            let bytes = scratch.io_inline_property_bytes_slice_mut(byte_len);
            self.values.read_bytes(offset, bytes);
            for &(slot, ordinal) in &slots_and_ordinals[i..end] {
                let value_index = usize::try_from(ordinal.saturating_sub(low))
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                let start = value_index
                    .checked_mul(width)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                scratch.slot_indices.push(slot);
                scratch
                    .values
                    .extend_from_slice(&scratch.io_inline_property_bytes[start..start + width]);
            }
            i = end;
        }
        Ok(())
    }

    fn visit_sparse_out_inline_property_batches_for_bucket_next<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(owner);
        let _bucket_slot = match self.find_bucket(owner, &vertex, label)? {
            BucketSearch::Found { slot, .. } => slot,
            BucketSearch::Missing { .. } => return Ok(ControlFlow::Continue(())),
        };
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let result = self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
            if scratch.slot_indices.len() == batch_edges {
                if let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id: label,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: false,
                }) {
                    return ControlFlow::Break(value);
                }
                scratch.clear();
            }
            scratch.slot_indices.push(slot.raw());
            scratch
                .values
                .extend_from_slice(item.inline_property.bytes());
            ControlFlow::Continue(())
        })?;
        if let ControlFlow::Break(value) = result {
            return Ok(ControlFlow::Break(value));
        }
        if !scratch.slot_indices.is_empty()
            && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                label_id: label,
                byte_width: bucket.inline_property_byte_width(),
                order,
                slot_indices: &scratch.slot_indices,
                values: &scratch.values,
                dense: false,
            })
        {
            return Ok(ControlFlow::Break(value));
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_dense_out_inline_property_batches_for_bucket_next<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let degree = bucket.degree();
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let inline_property_bytes =
            self.read_bucket_inline_property_bytes_span(owner, bucket, 0, degree)?;
        let mut remaining = degree;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            let first_slot = match order {
                OutEdgeOrder::Descending => remaining - take,
                OutEdgeOrder::Ascending => degree - remaining,
            };
            scratch.clear();
            scratch.slot_indices.reserve(take as usize);
            scratch.values.reserve(take as usize * width);
            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        let value_off = i * width;
                        scratch.slot_indices.push(slot);
                        scratch.values.extend_from_slice(
                            &inline_property_bytes[value_off..value_off + width],
                        );
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        let value_off = i * width;
                        scratch.slot_indices.push(slot);
                        scratch.values.extend_from_slice(
                            &inline_property_bytes[value_off..value_off + width],
                        );
                    }
                }
            }
            if !scratch.slot_indices.is_empty()
                && let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id: label,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: true,
                })
            {
                return Ok(ControlFlow::Break(value));
            }
            remaining -= take;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits outgoing edges and their parallel inline-property-bytes bytes for one label in batches.
    pub(crate) fn visit_out_edge_inline_property_batches_for_label_next<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<E>,
        mut visit: impl FnMut(LabeledEdgeInlinePropertyBatch<'_, E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        // Reset the reusable scratch buffers so prior callers cannot leak edges or
        // inline-property bytes into this label's batches.
        scratch.clear();
        scratch.reset_stop();
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            // Default-label bypass edges have no inline property bytes; emit nothing.
            return Ok(ControlFlow::Continue(()));
        }
        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, &vertex, label)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 || bucket.inline_property_byte_width() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        if crate::labeled::invariants::bucket_dense_inline_property_batch_eligible(&bucket) {
            return self.visit_dense_out_edge_inline_property_batches_for_bucket_next(
                owner, label, &bucket, order, scratch, &mut visit,
            );
        }

        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let result =
            self.visit_edges_with_inline_property(owner, label, order, |_slot, item| {
                if scratch.edges.len() == batch_edges {
                    if let ControlFlow::Break(value) = visit(LabeledEdgeInlinePropertyBatch {
                        label_id: label,
                        byte_width: bucket.inline_property_byte_width(),
                        order,
                        edges: &scratch.edges,
                        inline_property_bytes: &scratch.inline_property_bytes,
                        dense: false,
                    }) {
                        return ControlFlow::Break(value);
                    }
                    scratch.clear();
                }
                let edge = item
                    .edge
                    .with_stored_inline_property_bytes(
                        item.inline_property.width,
                        item.inline_property.bytes(),
                    )
                    .with_label_id(label.raw());
                scratch
                    .inline_property_bytes
                    .extend_from_slice(edge.edge_inline_property_bytes());
                scratch.edges.push(edge);
                ControlFlow::Continue(())
            })?;
        if let ControlFlow::Break(value) = result {
            return Ok(ControlFlow::Break(value));
        }
        if !scratch.edges.is_empty()
            && let ControlFlow::Break(value) = visit(LabeledEdgeInlinePropertyBatch {
                label_id: label,
                byte_width: bucket.inline_property_byte_width(),
                order,
                edges: &scratch.edges,
                inline_property_bytes: &scratch.inline_property_bytes,
                dense: false,
            })
        {
            return Ok(ControlFlow::Break(value));
        }
        Ok(ControlFlow::Continue(()))
    }

    fn visit_dense_out_edge_inline_property_batches_for_bucket_next<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<E>,
        visit: &mut impl FnMut(LabeledEdgeInlinePropertyBatch<'_, E>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let degree = bucket.degree();
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let edge_bytes_len =
            (degree as usize)
                .checked_mul(E::BYTES)
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?;
        let mut raw_edges = vec![0u8; edge_bytes_len];
        self.edges
            .read_slots_contiguous(bucket.edge_start(), &mut raw_edges);
        let inline_property_bytes =
            self.read_bucket_inline_property_bytes_span(owner, bucket, 0, degree)?;
        let mut remaining = degree;
        while remaining > 0 {
            let take = remaining.min(batch_edges as u32);
            let first_slot = match order {
                OutEdgeOrder::Descending => remaining - take,
                OutEdgeOrder::Ascending => degree - remaining,
            };
            scratch.clear();
            scratch.edges.reserve(take as usize);
            scratch.inline_property_bytes.reserve(take as usize * width);
            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        let edge_off = i * E::BYTES;
                        let value_off = i * width;
                        let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(label.raw());
                        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                            continue;
                        }
                        scratch.edges.push(edge);
                        scratch.inline_property_bytes.extend_from_slice(
                            &inline_property_bytes[value_off..value_off + width],
                        );
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        let edge_off = i * E::BYTES;
                        let value_off = i * width;
                        let edge = E::read_from(&raw_edges[edge_off..edge_off + E::BYTES])
                            .with_slot_index(slot)
                            .with_label_id(label.raw());
                        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                            continue;
                        }
                        scratch.edges.push(edge);
                        scratch.inline_property_bytes.extend_from_slice(
                            &inline_property_bytes[value_off..value_off + width],
                        );
                    }
                }
            }
            if !scratch.edges.is_empty()
                && let ControlFlow::Break(value) = visit(LabeledEdgeInlinePropertyBatch {
                    label_id: label,
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    edges: &scratch.edges,
                    inline_property_bytes: &scratch.inline_property_bytes,
                    dense: true,
                })
            {
                return Ok(ControlFlow::Break(value));
            }
            remaining -= take;
        }
        Ok(ControlFlow::Continue(()))
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
        replay: Option<&HybridOverflowEdgeReplay>,
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
        self.read_selected_edge_slots_with_optional_replay(
            owner,
            &vertex,
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

    /// Measurement-only storage-location visitor.
    ///
    /// Iterates live rows in slab local-slot ascending order, then the bucket's
    /// overflow-log chain in ascending chain order. Tombstoned and deleted rows are
    /// skipped. The callback receives a fully contextual [`StorageEdgeRef`] whose
    /// owner and label match the selected bucket.
    ///
    /// This is the only raw-location reader intended for LARA maintenance and
    /// in-crate adoption/contract tests; Graph/Router/graph-index callers must use
    /// the logical `visit_edges` family instead.
    #[cfg(feature = "adoption-fixtures")]
    pub(crate) fn visit_storage_edge_locations<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        mut visit: impl FnMut(StorageEdgeRef, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            return Ok(ControlFlow::Continue(()));
        }
        let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)? else {
            return Ok(ControlFlow::Continue(()));
        };

        for slot in 0..bucket.stored_slots {
            let edge_slot = crate::labeled::slot_index::checked_add_slot_index(
                bucket.edge_start(),
                u64::from(slot),
            )
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
            let edge = self.edges.read_slot(edge_slot);
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                continue;
            }
            if let ControlFlow::Break(value) = visit(
                StorageEdgeRef {
                    owner,
                    label,
                    location: StorageEdgeLocation::SlabSlot(slot),
                },
                edge.with_label_id(label.raw()),
            ) {
                return Ok(ControlFlow::Break(value));
            }
        }

        if bucket.overflow_log_head() >= 0 {
            let leaf = self.inline_property_bytes_log_leaf(owner);
            for entry_idx in self
                .edges
                .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head())
            {
                let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    continue;
                }
                if let ControlFlow::Break(value) = visit(
                    StorageEdgeRef {
                        owner,
                        label,
                        location: StorageEdgeLocation::OverflowLogEntry(entry_idx),
                    },
                    edge.with_label_id(label.raw()),
                ) {
                    return Ok(ControlFlow::Break(value));
                }
            }
        }

        Ok(ControlFlow::Continue(()))
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
        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, &vertex, label)?
        else {
            return Ok(EdgeSlotState::Missing);
        };
        let slot_index = slot.raw();
        if slot_index >= self.bucket_reserved_edge_slots(owner, &bucket) {
            return Ok(EdgeSlotState::Missing);
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, _bucket_slot)?;
        let overflow_chain = (bucket.overflow_log_head() >= 0).then(|| {
            self.edges.overflow_log_chain_asc_indices(
                self.inline_property_bytes_log_leaf(owner),
                bucket.overflow_log_head(),
            )
        });
        self.read_edge_state_at_slot(
            owner,
            &vertex,
            bucket_index,
            &bucket,
            slot_index,
            label,
            overflow_chain.as_deref(),
        )
    }

    /// Reads the state of one logical slot in a label bucket, including slab and overflow-log rows.
    fn read_edge_state_at_slot(
        &self,
        owner: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket: &LabelBucket,
        slot_index: u32,
        label: BucketLabelKey,
        overflow_chain: Option<&[u32]>,
    ) -> Result<EdgeSlotState<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if slot_index >= self.bucket_reserved_edge_slots(owner, bucket) {
            return Ok(EdgeSlotState::Missing);
        }
        if bucket.overflow_log_head() < 0 {
            if slot_index >= bucket.stored_slots {
                return Ok(EdgeSlotState::Missing);
            }
            let edge_slot = crate::labeled::slot_index::checked_add_slot_index(
                bucket.edge_start(),
                u64::from(slot_index),
            )
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
            let edge = self
                .edges
                .read_slot(edge_slot)
                .with_slot_index(slot_index)
                .with_label_id(label.raw());
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                return Ok(EdgeSlotState::Tombstone);
            }
            return Ok(EdgeSlotState::Live(edge));
        }

        let slab_prefix = self.bucket_slab_prefix_slots(owner, bucket);
        if slot_index < slab_prefix {
            let edge_slot = crate::labeled::slot_index::checked_add_slot_index(
                bucket.edge_start(),
                u64::from(slot_index),
            )
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))?;
            let edge = self
                .edges
                .read_slot(edge_slot)
                .with_slot_index(slot_index)
                .with_label_id(label.raw());
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                return Ok(EdgeSlotState::Tombstone);
            }
            return Ok(EdgeSlotState::Live(edge));
        }

        let log_ordinal =
            slot_index
                .checked_sub(slab_prefix)
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?;
        let leaf = self.inline_property_bytes_log_leaf(owner);
        let chain_storage;
        let chain = match overflow_chain {
            Some(chain) => chain,
            None => {
                chain_storage = self
                    .edges
                    .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
                &chain_storage
            }
        };
        let Some(&entry_idx) = chain.get(log_ordinal as usize) else {
            return Ok(EdgeSlotState::Missing);
        };
        let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
            return Ok(EdgeSlotState::Tombstone);
        }
        Ok(EdgeSlotState::Live(
            edge.with_slot_index(slot_index).with_label_id(label.raw()),
        ))
    }

    /// Reads a selected set of raw logical slots, optionally reusing a phase-1 hybrid overflow replay.
    fn read_selected_edge_slots_with_optional_replay(
        &self,
        owner: VertexId,
        vertex: &LabeledVertex,
        label: BucketLabelKey,
        raw_slots: &[u32],
        order: OutEdgeOrder,
        replay: Option<&HybridOverflowEdgeReplay>,
        mut visit: impl FnMut(u32, E),
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if raw_slots.is_empty() {
            return Ok(());
        }
        let BucketSearch::Found {
            slot: _bucket_slot,
            bucket,
        } = self.find_bucket(owner, vertex, label)?
        else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }
        let bucket_index = Self::labeled_bucket_descriptor_index(vertex, _bucket_slot)?;
        let visit_order = order_slot_indices(raw_slots, order);

        // Reuse a matching phase-1 replay when available and consistent.
        if let Some(replay) = replay
            && replay.is_active()
            && bucket.overflow_log_head() >= 0
            && replay.src == owner
            && replay.label_id == label
            && replay.slab_slots == self.bucket_slab_prefix_slots(owner, &bucket)
            && replay.degree == bucket.degree()
            && replay.stored_slots == bucket.stored_slots
            && replay.overflow_log_head == bucket.overflow_log_head()
            && replay.edge_start == bucket.edge_start()
        {
            return self.read_selected_slots_with_hybrid_replay(
                &bucket,
                label,
                &visit_order,
                replay,
                &mut visit,
            );
        }

        let overflow_chain = (bucket.overflow_log_head() >= 0).then(|| {
            #[cfg(test)]
            crate::lara::edge::scan_guard::record_overflow_chain_rebuild();
            self.edges.overflow_log_chain_asc_indices(
                self.inline_property_bytes_log_leaf(owner),
                bucket.overflow_log_head(),
            )
        });
        for slot_index in visit_order {
            if let EdgeSlotState::Live(edge) = self.read_edge_state_at_slot(
                owner,
                vertex,
                bucket_index,
                &bucket,
                slot_index,
                label,
                overflow_chain.as_deref(),
            )? {
                visit(slot_index, edge);
            }
        }
        Ok(())
    }

    fn read_selected_slots_with_hybrid_replay(
        &self,
        bucket: &LabelBucket,
        label: BucketLabelKey,
        visit_order: &[u32],
        replay: &HybridOverflowEdgeReplay,
        visit: &mut impl FnMut(u32, E),
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let log_table = (!replay.log_table.is_empty()).then_some(replay.log_table.as_slice());
        for &slot_index in visit_order {
            if slot_index < replay.slab_slots {
                if slab_slot_deleted(slot_index, &replay.deleted_slab_offsets) {
                    continue;
                }
                let edge_slot = crate::labeled::slot_index::checked_add_slot_index(
                    bucket.edge_start(),
                    u64::from(slot_index),
                )
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?;
                let edge = self
                    .edges
                    .read_slot(edge_slot)
                    .with_slot_index(slot_index)
                    .with_label_id(label.raw());
                if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                    continue;
                }
                visit(slot_index, edge);
                continue;
            }
            let Some(log_slot) = slot_index.checked_sub(replay.slab_slots) else {
                continue;
            };
            let Some(Some(log_idx)) = replay.log_indices_by_slot.get(log_slot as usize) else {
                continue;
            };
            let edge = self
                .edges
                .decode_overflow_log_edge_from_table(replay.leaf, *log_idx, log_table)
                .with_slot_index(slot_index)
                .with_label_id(label.raw());
            if edge.is_tombstone_edge() {
                continue;
            }
            visit(slot_index, edge);
        }
        Ok(())
    }

    /// Resolves the label bucket and, when the bucket stores inline property bytes in a
    /// suffix log, builds the ordered log chain used by
    /// [`Self::read_inline_property_bytes_for_ordinal`].
    fn resolve_label_bucket_and_inline_property_bytes_chains(
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
                let log_chains = self.bucket_inline_property_bytes_log_chain_opt(owner, &bucket);
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
            self.resolve_label_bucket_and_inline_property_bytes_chains(owner, label, &vertex)?;
        if bucket.inline_property_byte_width() == 0 {
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
        let width = bucket.inline_property_byte_width();
        if width == 0 {
            return Ok(InlinePropertyBytes::empty());
        }
        let bytes =
            self.read_bucket_inline_property_bytes_for_slot(owner, bucket, ordinal, log_chains)?;
        if bytes.len() != usize::from(width) {
            return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                bucket_width: width,
                edge_inline_property_width: u16::try_from(bytes.len()).unwrap_or(u16::MAX),
            });
        }
        Ok(InlinePropertyBytes::from_bytes(width, &bytes))
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
        visit: impl FnMut(Self::Slot, Self::Edge) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        if window.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        LabeledLaraGraph::visit_edges_window(
            self,
            request.owner,
            request.label,
            request.order,
            window,
            visit,
        )
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContiguousBucketRun {
    base: u64,
    total_edges: u32,
}

impl ContiguousBucketRun {
    fn new(base: u64, total_edges: u32) -> Self {
        Self { base, total_edges }
    }

    pub(super) fn base(self) -> u64 {
        self.base
    }

    pub(super) fn total_edges(self) -> u32 {
        self.total_edges
    }

    pub(super) fn byte_len<E: CsrEdge>(self) -> Result<usize, LabeledOperationError> {
        (self.total_edges as usize)
            .checked_mul(E::BYTES)
            .ok_or_else(|| LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn edge_chunk<'a, E: CsrEdge>(
        self,
        raw: &'a [u8],
        bucket: &LabelBucket,
        slot: u32,
    ) -> Result<&'a [u8], LabeledOperationError> {
        let rel = bucket
            .edge_start()
            .saturating_sub(self.base)
            .checked_add(u64::from(slot))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let byte_off = usize::try_from(rel)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let byte_end = byte_off
            .checked_add(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        raw.get(byte_off..byte_end)
            .ok_or_else(|| LaraOperationError::CollectAllocationOverflow.into())
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn try_contiguous_tiled_labeled_out_edges_slice(
        buckets: &[LabelBucket],
        span_end_exclusive: u64,
    ) -> Option<ContiguousBucketRun> {
        if buckets.is_empty() {
            return None;
        }
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            return None;
        }
        if buckets.iter().any(|b| b.stored_slots != b.degree()) {
            return None;
        }
        let base = buckets.first()?.edge_start();
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start() != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.stored_slots)?;
            pos =
                crate::labeled::slot_index::checked_add_slot_index(pos, u64::from(b.stored_slots))?;
        }
        if pos > span_end_exclusive {
            return None;
        }
        Some(ContiguousBucketRun::new(base, total_edges))
    }

    pub(super) fn try_contiguous_tiled_labeled_out_edges(
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Option<ContiguousBucketRun> {
        let deg = vertex.degree() as usize;
        if deg == 0 || buckets.len() != deg {
            return None;
        }
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            return None;
        }
        if buckets.iter().any(|b| b.stored_slots != b.degree()) {
            return None;
        }
        let base = buckets.first()?.edge_start();
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start() != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.stored_slots)?;
            pos =
                crate::labeled::slot_index::checked_add_slot_index(pos, u64::from(b.stored_slots))?;
        }
        let span_end = crate::labeled::slot_index::checked_add_slot_index(
            base,
            u64::from(vertex.stored_slots),
        )?;
        if pos > span_end {
            return None;
        }
        Some(ContiguousBucketRun::new(base, total_edges))
    }

    /// Visits outgoing edges for one label in descending scan order.
    pub fn for_each_edges_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.for_each_edges_for_label_ordered(src, label_id, OutEdgeOrder::Descending, visit)
    }

    /// Visits outgoing edges for one label in the requested order.
    pub fn for_each_edges_for_label_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.visit_edges_for_label(src, label_id, order, &mut visit)
    }

    /// Like [`Self::for_each_edges_for_label_ordered`], but skips edge-inline-property-bytes reads.
    pub fn for_each_edges_for_label_topology_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        let _ = self.visit_edges(src, label_id, order, |_slot, edge| {
            visit(edge.with_label_id(label_id.raw()));
            ControlFlow::<()>::Continue(())
        })?;
        Ok(())
    }
}
impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// Visits outgoing edges for one label without checking that `src` is in range.
    pub fn for_each_edges_for_label_unchecked<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_unchecked_bypass_slab");
        self.visit_edges_for_label_unchecked(src, label_id, OutEdgeOrder::Descending, &mut visit)?;
        Ok(())
    }

    pub(super) fn visit_label_out_edges_inner<Match, Visit>(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        ascending: bool,
        offset: Option<usize>,
        limit: Option<usize>,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: &mut Match,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        let mut window = OutEdgeVisitWindow::new(offset, limit);
        let order = if ascending {
            OutEdgeOrder::Ascending
        } else {
            OutEdgeOrder::Descending
        };
        let _ = self.visit_all_labels_with_inline_property(src, order, |edge| {
            let passes = if let Some(raw_m) = raw_matches.as_mut() {
                let mut buf = vec![0u8; E::BYTES];
                edge.write_to(&mut buf);
                raw_m(&buf) && matches(&edge)
            } else {
                matches(&edge)
            };
            if passes && !window.emit_edge(edge, visit) {
                return ControlFlow::<()>::Break(());
            }
            ControlFlow::<()>::Continue(())
        })?;
        Ok(())
    }

    pub(super) fn labeled_out_edges_iter(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
        directedness: Option<BucketDirectedness>,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.degree() == 0 {
            return Ok(LabeledOutEdgesIter::empty(self, src, order));
        }
        if vertex.is_default_edge_labeled() {
            let label = self.bypass_storage_label_for(&vertex);
            if let Some(directedness) = directedness
                && label.directedness() != directedness
            {
                return Ok(LabeledOutEdgesIter::empty(self, src, order));
            }
            // Bypass vertices use a single synthetic bucket so the iterator delegates
            // to traverse_next on first `next()`.
            let bucket = LabelBucket::from_parts(
                label,
                vertex.base_slot_start(),
                vertex.degree(),
                vertex.stored_degree(),
                -1,
            );
            return Ok(LabeledOutEdgesIter::from_buckets(
                self,
                src,
                order,
                0,
                vec![bucket],
            ));
        }

        let (base_bucket_index, buckets) = if let Some(directedness) = directedness {
            let strategy = Self::directedness_partition_strategy(directedness, order.ascending());
            let (lo, hi) = self.buckets.directedness_bucket_index_range(
                vertex.base_slot_start(),
                vertex.degree(),
                directedness,
                strategy,
            )?;
            if lo >= hi {
                return Ok(LabeledOutEdgesIter::empty(self, src, order));
            }
            (lo, self.read_vertex_label_buckets_range(&vertex, lo, hi)?)
        } else {
            (0, self.read_vertex_label_buckets(&vertex)?)
        };
        Ok(LabeledOutEdgesIter::from_buckets(
            self,
            src,
            order,
            base_bucket_index,
            buckets,
        ))
    }

    /// Visits outgoing edges and their inline-property bytes for one label in batches.
    pub fn visit_out_edge_inline_property_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledEdgeInlinePropertyBatchScratch<E>,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledEdgeInlinePropertyBatch<'b, E>),
    {
        let result = self.visit_out_edge_inline_property_batches_for_label_next(
            src,
            label_id,
            order,
            scratch,
            |batch| {
                visit(batch);
                ControlFlow::<()>::Continue(())
            },
        )?;
        assert!(
            result.is_continue(),
            "visit_out_edge_inline_property_batches_for_label closure cannot break"
        );
        Ok(())
    }

    /// Returns whether `(src, label_id)` is eligible for dense inline-property-bytes-only batch traversal.
    ///
    /// Hybrid and sparse overflow buckets return `false`; predicate expand should use the
    /// combined edge + inline property bytes batch path without probing phase 1 first.
    pub fn out_bucket_dense_inline_property_batch_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(false);
        }
        let bucket = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => return Ok(false),
        };
        Ok(super::super::invariants::bucket_dense_inline_property_batch_eligible(&bucket))
    }

    /// Returns whether predicate expand may use phase 1 (inline property values) + phase 2 (topology).
    pub fn out_bucket_inline_property_bytes_first_predicate_eligible(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, LabeledOperationError> {
        if self.out_bucket_dense_inline_property_batch_eligible(src, label_id)? {
            return Ok(true);
        }
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(false);
        }
        let bucket = match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => return Ok(false),
        };
        Ok(bucket.degree() > 0
            && bucket.inline_property_byte_width() > 0
            && bucket.overflow_log_head() >= 0)
    }

    /// Visits outgoing inline property bytes for one label as batches without materializing edge rows.
    ///
    /// Dense buckets bulk-read the inline property bytes slab; hybrid buckets combine slab bulk reads with
    /// per-log-entry inline property bytes resolution; sparse buckets walk the span iterator and emit slot
    /// indices with attached inline_property_bytes bytes only.
    pub fn visit_out_inline_property_batches_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: for<'b> FnMut(LabeledInlinePropertyValueBatch<'b>),
    {
        let result = self.visit_out_inline_property_batches_for_label_next(
            src,
            label_id,
            order,
            scratch,
            |batch| {
                visit(batch);
                ControlFlow::<()>::Continue(())
            },
        )?;
        assert!(
            result.is_continue(),
            "visit_out_inline_property_batches_for_label closure cannot break"
        );
        Ok(())
    }

    /// Visits matching outgoing edges in descending scan order with optional offset and limit.
    pub fn visit_out_edges<Match, Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            false,
            offset,
            limit,
            raw_matches,
            &mut matches,
            &mut visit,
        )
    }

    /// Visits all outgoing edges in descending scan order with optional offset and limit.
    pub fn visit_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.visit_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }

    /// Visits matching outgoing edges in ascending slot order with optional offset and limit.
    pub fn visit_asc_out_edges<Match, Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            true,
            offset,
            limit,
            raw_matches,
            &mut matches,
            &mut visit,
        )
    }

    /// Visits all outgoing edges in ascending slot order with optional offset and limit.
    pub fn visit_asc_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        Visit: FnMut(E),
    {
        self.visit_asc_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }

    /// Collects outgoing edges for `src` in descending scan order.
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        let mut out = Vec::new();
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            false,
            None,
            None,
            None,
            &mut |_| true,
            &mut |e| out.push(e),
        )?;
        Ok(out)
    }

    /// Collects outgoing edges for `src` in ascending slot order.
    pub fn asc_out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        let mut out = Vec::new();
        self.visit_label_out_edges_inner(
            src,
            &vertex,
            true,
            None,
            None,
            None,
            &mut |_| true,
            &mut |e| out.push(e),
        )?;
        Ok(out)
    }

    /// Returns an iterator over outgoing edges in descending scan order.
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Descending, None)
    }

    /// Returns an iterator over outgoing edges in ascending slot order.
    pub fn asc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Ascending, None)
    }

    /// Finds the first outgoing edge matching `pred`, returning its label when available.
    pub fn find_out_edge_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        pred: F,
    ) -> Result<Option<(E, Option<BucketLabelKey>)>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        Ok(self
            .find_nth_edge_with_inline_property_matching(
                src,
                EdgeFindScope::AllLabels,
                OutEdgeOrder::Descending,
                0,
                pred,
            )?
            .map(|found| (found.edge, Some(found.label))))
    }

    /// Finds the first outgoing edge matching `pred`, returning its label and bucket slot index.
    pub fn find_out_edge_slot_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        Ok(self
            .find_nth_edge_with_inline_property_matching(
                src,
                EdgeFindScope::AllLabels,
                OutEdgeOrder::Descending,
                0,
                pred,
            )?
            .map(|found| (found.edge, found.label, found.slot.raw())))
    }

    /// Collects outgoing edges for one label.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.iter_edges_with_inline_property_for_label_next(src, label_id, OutEdgeOrder::Descending)
    }

    /// Returns the bucket-index range that stores edges with `directedness`.
    pub fn out_edge_bucket_index_range_for_directedness(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<(u32, u32), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok((0, 0));
        }
        let deg = vertex.degree();
        let strategy = Self::directedness_partition_strategy(directedness, order.ascending());
        Ok(self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?)
    }

    /// Collects outgoing edges whose bucket directedness matches `directedness`.
    pub fn iter_out_edges_by_directedness(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        let mut out = Vec::new();
        let _ = self.visit_out_edges_by_directedness(src, directedness, order, |edge| {
            out.push(edge);
            ControlFlow::<()>::Continue(())
        })?;
        Ok(out)
    }

    /// Returns an iterator over outgoing edges whose bucket directedness matches `directedness`.
    pub fn out_edges_by_directedness_iter(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, order, Some(directedness))
    }

    /// Collects directed outgoing edges.
    pub fn iter_out_edges_directed_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Directed, order)
    }

    /// Collects undirected outgoing edges.
    pub fn iter_out_edges_undirected_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Undirected, order)
    }
}

#[cfg(test)]
mod tests {
    use super::super::LEAF_VERTEX_EDGE_SEGMENT_DENSITY;
    #[test]
    fn inline_property_bytes_zero_width_is_empty() {
        let v = InlinePropertyBytes::empty();
        assert_eq!(v.width(), 0);
        assert_eq!(v.bytes(), &[] as &[u8]);
        assert!(v.into_vec().is_empty());
    }

    #[test]
    fn inline_property_bytes_enforces_width_match() {
        let v = InlinePropertyBytes::from_bytes(4, &[1, 2, 3, 4]);
        assert_eq!(v.width(), 4);
        assert_eq!(v.bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    #[should_panic(expected = "InlinePropertyBytes width must match byte length")]
    fn inline_property_bytes_rejects_width_mismatch() {
        let _ = InlinePropertyBytes::from_bytes(4, &[1, 2]);
    }

    #[test]
    #[should_panic(expected = "InlinePropertyBytes width must match byte length")]
    fn inline_property_bytes_rejects_nonzero_width_with_empty_bytes() {
        let _ = InlinePropertyBytes::from_bytes(2, &[]);
    }

    use super::super::leaf_pin::labeled_leaf_physical_block_len;
    use super::super::test_support::*;
    use super::*;
    use std::num::NonZero;
    use std::ops::ControlFlow;

    use crate::{
        VertexId,
        labeled::{
            BucketDirectedness, BucketLabelKey, LabeledInlinePropertyValueBatchScratch,
            OutEdgeOrder,
            graph::test_support::{
                InlinePropertyTestEdge, TestEdge, inline_property_test_graph_with_capacity,
                test_graph, test_graph_with_default,
            },
            record::LabeledVertex,
        },
        lara::edge::scan_guard::ScanPathGuard,
        traverse::TraversalWindow,
    };

    fn bucket_graph() -> (
        super::LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        VertexId,
    ) {
        let graph = test_graph();
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        (graph, src)
    }

    fn bypass_graph() -> (
        super::LabeledLaraGraph<TestEdge, crate::VectorMemory>,
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
                assert!(item.inline_property.bytes().is_empty());
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
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, 2)
            .unwrap();
        for value in [10u16, 20u16] {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(7, &value.to_le_bytes()),
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
                        item.inline_property.bytes().to_vec(),
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
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, 4)
            .unwrap();

        // Insert enough edges to create both slab and overflow-log rows.
        for target in 1..=64u32 {
            let value = (target * 7).to_le_bytes();
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &value),
                )
                .unwrap();
        }

        // Phase 1: collect all logical slots via property-first batches.
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
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
                assert_eq!(with_prop.inline_property.bytes(), expected);
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
                    assert_eq!(item.inline_property.bytes(), expected);
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
                        item.inline_property.bytes(),
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
                        item.inline_property.bytes(),
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
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, 4)
            .unwrap();
        for target in 1..=64u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target * 7).to_le_bytes()),
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
                    item.inline_property.bytes().to_vec(),
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
                    item.inline_property.bytes().to_vec(),
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
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, 4)
            .unwrap();
        for target in 1..=8u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target * 7).to_le_bytes()),
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
                    item.inline_property.bytes().to_vec(),
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
                    item.inline_property.bytes().to_vec(),
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
        assert!(item.inline_property.bytes().is_empty());
    }

    #[test]
    fn stale_hybrid_overflow_replay_falls_back_to_canonical_read() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let a = graph.push_vertex(LabeledVertex::default()).unwrap();
        let b = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(a, label, 2)
            .unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(b, label, 2)
            .unwrap();

        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    a,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    label,
                    InlinePropertyTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let mut scratch_b = LabeledInlinePropertyValueBatchScratch::default();
        graph
            .visit_out_inline_property_batches_for_label(
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

        // A malformed inline_property_bytes-log length must fail closed rather than return
        // a partial or zero-filled property value.
        let vertex = graph.vertices.get(a);
        let BucketSearch::Found { slot, bucket } = graph.find_bucket(a, &vertex, label).unwrap()
        else {
            panic!("label bucket missing");
        };
        let malformed = bucket
            .with_inline_property_bytes_slab_slots(0)
            .try_with_inline_property_bytes_log(0, 1)
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
            LabeledOperationError::InlinePropertyBytesLogRead(
                crate::lara::edge_inline_property::InlinePropertyBytesLogReadError::MissingAscLogIndex { .. }
            )
        ));
        let visitor_error = graph
            .visit_edges_with_inline_property::<()>(a, label, OutEdgeOrder::Ascending, |_, _| {
                ControlFlow::Continue(())
            })
            .unwrap_err();
        assert!(matches!(
            visitor_error,
            LabeledOperationError::InlinePropertyBytesLogRead(
                crate::lara::edge_inline_property::InlinePropertyBytesLogReadError::MissingAscLogIndex { .. }
            )
        ));
    }

    #[test]
    fn out_edges_iterator_streams_desc_order() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        let walk = BucketLabelKey::from_raw(3);
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        let expected = graph.out_edges(VertexId::from(0)).unwrap();
        let lazy: Vec<_> = graph
            .desc_out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(lazy, expected);
    }

    #[test]
    fn labeled_desc_and_asc_out_edges_iters_match_materialized_rows() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();

        let desc = graph.out_edges(VertexId::from(0)).unwrap();
        let asc = graph.asc_out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            graph
                .desc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            desc
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            asc
        );
    }

    #[test]
    fn labeled_out_edges_iter_advance_by_and_nth_match_scan() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();

        let full: Vec<_> = graph
            .desc_out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(full.len(), 2);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(0).unwrap(), Ok(()));
        assert_eq!(it.next().transpose().unwrap(), Some(full[0]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(1).unwrap(), Ok(()));
        assert_eq!(it.next().transpose().unwrap(), Some(full[1]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(2).unwrap(), Ok(()));
        assert_eq!(it.next().transpose().unwrap(), None);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.try_advance_by(3).unwrap(), Err(NonZero::new(1).unwrap()));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.next().transpose().unwrap(), Some(full[0]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1).transpose().unwrap(), Some(full[1]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2).transpose().unwrap(), None);
    }

    #[test]
    fn out_edges_by_directedness_filters_and_orders() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::undirected_from_index(3),
                TestEdge { target: 30 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(2),
                TestEdge { target: 10 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(4),
                TestEdge { target: 40 },
            )
            .unwrap();

        assert_eq!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            vec![TestEdge { target: 40 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Ascending,
                )
                .unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 40 }]
        );
        assert_eq!(
            graph
                .iter_out_edges_undirected_only(VertexId::from(0), OutEdgeOrder::Descending)
                .unwrap(),
            vec![TestEdge { target: 30 }]
        );
        assert_eq!(
            graph
                .iter_out_edges_undirected_only(VertexId::from(0), OutEdgeOrder::Ascending)
                .unwrap(),
            vec![TestEdge { target: 30 }]
        );
    }

    #[test]
    fn normal_labeled_edges_update_pma_leaf_segment_counts() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 10 },
            )
            .unwrap();

        let header = graph.edges().header();
        let first_leaf = graph
            .edges()
            .counts_store()
            .get(u64::from(header.segment_count));
        assert_eq!(first_leaf.actual, 1);
        assert!(first_leaf.total > 0);
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn labeled_dense_leaf_triggers_leaf_rebalance() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        graph
            .insert_edge(vid, BucketLabelKey::from_raw(99), TestEdge { target: 999 })
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        let header = graph.edges().header();
        let block_len = labeled_leaf_physical_block_len(header.segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        graph.rebalance_cascade_after_labeled_mutation(vid).unwrap();
        let counts = graph.leaf_segment_counts_for_vid(vid);
        assert!(counts.total as u64 >= block_len);
        assert!(counts.actual > 0);
        assert!(
            graph.labeled_leaf_pma_density(vid) < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
                || counts.total as u64 > block_len,
            "dense leaf maintenance should slide or grow the pinned PMA block"
        );
    }

    #[test]
    fn unchecked_label_iteration_matches_checked_for_valid_vertices() {
        let graph = test_graph();
        let bypass_tail = graph.push_vertex(LabeledVertex::default()).unwrap();
        let catalog_tail = graph.push_vertex(LabeledVertex::default()).unwrap();

        let road = BucketLabelKey::from_raw(2);
        let walk = BucketLabelKey::from_raw(3);
        for target in [10, 11] {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        for target in [100, 101] {
            graph
                .insert_edge(bypass_tail, graph.default_label(), TestEdge { target })
                .unwrap();
        }

        let catalog = BucketLabelKey::from_raw(42);
        for target in [200, 201] {
            graph
                .insert_edge(catalog_tail, catalog, TestEdge { target })
                .unwrap();
        }

        for (src, label) in [
            (VertexId::from(0), road),
            (VertexId::from(0), walk),
            (VertexId::from(0), BucketLabelKey::from_raw(999)),
            (bypass_tail, graph.default_label()),
            (bypass_tail, road),
            (catalog_tail, catalog),
            (catalog_tail, graph.default_label()),
        ] {
            let mut checked = Vec::new();
            graph
                .for_each_edges_for_label(src, label, |edge| checked.push(edge))
                .unwrap();

            let mut unchecked = Vec::new();
            graph
                .for_each_edges_for_label_unchecked(src, label, |edge| unchecked.push(edge))
                .unwrap();

            assert_eq!(unchecked, checked, "src={src:?} label={label:?}");
        }
    }

    #[test]
    fn labeled_scan_never_reads_span_meta() {
        let (graph, hub, _) = build_mixed_label_hub(8, 25);
        let _guard = ScanPathGuard::enter();
        exercise_labeled_hub_scan_paths(&graph, hub);
        assert_eq!(ScanPathGuard::span_meta_reads(), 0);
    }

    #[test]
    fn labeled_scan_never_reads_free_span_store() {
        let (graph, hub, _) = build_mixed_label_hub(8, 25);
        let _guard = ScanPathGuard::enter();
        exercise_labeled_hub_scan_paths(&graph, hub);
        assert_eq!(ScanPathGuard::free_span_reads(), 0);
    }

    #[test]
    fn labeled_hub_materialized_matches_all_scan_iters() {
        let (graph, hub, _) = build_mixed_label_hub(6, 30);
        let materialized = materialized_labeled_edges(&graph, hub);
        exercise_labeled_hub_scan_paths(&graph, hub);
        for (label, expected_targets) in &materialized {
            let asc = graph
                .iter_edges_for_label(hub, *label)
                .unwrap()
                .into_iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>();
            assert_eq!(&asc, expected_targets, "label {label:?}");
        }
        let total: usize = materialized.iter().map(|(_, targets)| targets.len()).sum();
        assert_eq!(graph.asc_out_edges(hub).unwrap().len(), total);
        assert_eq!(
            graph
                .asc_out_edges_iter(hub)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            total
        );
    }

    /// A hybrid-overflow replay built for a **different vertex that shares the same inline_property_bytes-log
    /// leaf** (`leaf = src / segment_size`) must be rejected so phase 2 falls back to the sparse
    /// path. Reproduced with two real vertices in one leaf (not by mutating replay fields), since
    /// `leaf` + `label_id` + `slab_slots` alone cannot tell same-leaf vertices apart — only `src`
    /// can. Guards `read_out_edge_slots_for_label_with_replay`.
    #[test]
    fn hybrid_replay_from_other_vertex_in_same_leaf_falls_back_to_sparse() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let a = graph.push_vertex(LabeledVertex::default()).unwrap();
        let b = graph.push_vertex(LabeledVertex::default()).unwrap();
        assert_eq!(
            graph.inline_property_bytes_log_leaf(a),
            graph.inline_property_bytes_log_leaf(b),
            "test requires two vertices sharing one inline-property-bytes-log leaf"
        );
        let road = BucketLabelKey::from_raw(2);
        for v in [a, b] {
            graph
                .ensure_label_bucket_inline_property_byte_width(v, road, 2u16)
                .unwrap();
        }
        // Identically-shaped hybrid overflow buckets, but disjoint target ranges so the two
        // replays decode different overflow-log edges (A: 1..=48, B: 1001..=1048).
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    a,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    road,
                    InlinePropertyTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let bucket_of = |v: VertexId| {
            let vertex = graph.vertices().get(v);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let bucket_a = bucket_of(a);
        let bucket_b = bucket_of(b);
        assert!(bucket_a.overflow_log_head() >= 0 && bucket_b.overflow_log_head() >= 0);
        // Same leaf, same label, same slab split: only `src` distinguishes the two replays, so
        // without the `src` check B's replay would be wrongly adopted for A.
        assert_eq!(
            graph.bucket_slab_prefix_slots(a, &bucket_a),
            graph.bucket_slab_prefix_slots(b, &bucket_b),
        );

        // Phase 1 on A captures A's slot order; phase 1 on B populates a replay owned by B.
        let mut scratch_a = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut slots_a = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                a,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch_a,
                |batch| slots_a.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        let mut scratch_b = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        graph
            .visit_out_inline_property_batches_for_label(
                b,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch_b,
                |_| {},
            )
            .unwrap();
        assert!(scratch_b.hybrid_overflow_replay.is_active());
        assert!(
            !scratch_b
                .hybrid_overflow_replay
                .log_indices_by_slot
                .is_empty()
        );

        let read_a = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut targets = Vec::new();
            let positions_slots_a: Vec<_> = slots_a
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at_with_replay(
                    a,
                    road,
                    &positions_slots_a,
                    OutEdgeOrder::Ascending,
                    replay,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };
        let expected = read_a(None);
        assert_eq!(expected.len(), 48);
        assert!(
            expected.iter().all(|&t| (1..=48).contains(&t)),
            "A only owns targets 1..=48"
        );

        // A's own replay reproduces the ground truth.
        assert_eq!(read_a(Some(&scratch_a.hybrid_overflow_replay)), expected);
        // B's replay (same leaf/label/slab split, different vertex) must be rejected → sparse path.
        assert_eq!(
            read_a(Some(&scratch_b.hybrid_overflow_replay)),
            expected,
            "a replay owned by another vertex in the same leaf must not be reused"
        );
    }

    #[test]
    fn hybrid_inline_property_bytes_first_keeps_replay_with_tombstone_free_slab_prefix() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, road, 2)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        graph
            .rebalance_edge_log_leaf_for_labeled(src, true, true)
            .unwrap();
        for target in 49..=64u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.stored_slots > 0);
        assert!(bucket.overflow_log_head() >= 0);
        assert_eq!(
            graph.bucket_reserved_edge_slots(src, &bucket),
            bucket.degree()
        );

        let mut scratch = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut observed = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    observed.extend(
                        batch
                            .values
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|bytes| u16::from_le_bytes(*bytes)),
                    );
                },
            )
            .unwrap();

        assert_eq!(observed, (1..=64u16).collect::<Vec<_>>());
        assert!(scratch.hybrid_overflow_replay.is_active());
    }

    /// Phase-2 reuse of a matching replay must skip the overflow-log chain rebuild that the sparse
    /// fallback performs. Validates the `overflow_chain_rebuilds` instrumentation (used by the
    /// executor incoming/outgoing replay-reuse tests) distinguishes the two phase-2 paths.
    #[test]
    fn phase2_replay_reuse_avoids_overflow_chain_rebuild() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut scratch = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| slots.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        assert!(scratch.hybrid_overflow_replay.is_active());

        let read = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut targets = Vec::new();
            let positions_slots: Vec<_> = slots
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at_with_replay(
                    src,
                    road,
                    &positions_slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };

        let (with_replay, rebuilds_with_replay) = {
            let _guard = ScanPathGuard::enter();
            let targets = read(Some(&scratch.hybrid_overflow_replay));
            (targets, ScanPathGuard::overflow_chain_rebuilds())
        };
        let (without_replay, rebuilds_without_replay) = {
            let _guard = ScanPathGuard::enter();
            let targets = read(None);
            (targets, ScanPathGuard::overflow_chain_rebuilds())
        };

        assert_eq!(with_replay, without_replay);
        assert_eq!(
            rebuilds_with_replay, 0,
            "a reused replay must not rebuild the overflow-log chain"
        );
        assert!(
            rebuilds_without_replay >= 1,
            "the sparse fallback rebuilds the overflow-log chain"
        );
    }

    /// `phase 1 → delete an overflow edge → phase 2` must fall back to sparse. Removing an
    /// overflow-log edge tombstones the log entry in place: `src`, `label_id`, and the slab/log
    /// split are all unchanged, so only the bucket snapshot (`degree`) catches the mutation. Without
    /// it, the stale cached `log_table` would still decode and return the deleted edge.
    #[test]
    fn hybrid_replay_after_overflow_delete_falls_back_to_sparse() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(src, road, 2u16)
            .unwrap();
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let bucket_of = |v| {
            let vertex = graph.vertices().get(v);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let bucket = bucket_of(src);
        assert!(bucket.overflow_log_head() >= 0);
        let slab_prefix = graph.bucket_slab_prefix_slots(src, &bucket);

        // Phase 1: capture the replay and the slot order, then take a stale snapshot of the replay.
        let mut scratch = crate::labeled::LabeledInlinePropertyValueBatchScratch::default();
        let mut slots = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                road,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| slots.extend_from_slice(batch.slot_indices),
            )
            .unwrap();
        assert!(scratch.hybrid_overflow_replay.is_active());
        let stale_replay = scratch.hybrid_overflow_replay.clone();

        // Delete one overflow-log edge (first slot past the slab prefix): an in-place tombstone.
        let removed = graph
            .remove_edge_at_slot(src, road, slab_prefix)
            .unwrap()
            .expect("removed an overflow-log edge");
        let deleted_target = removed.target;

        // The slab/log split is unchanged, so the older `src`/`label`/`slab_slots` checks still
        // match — only the `degree` snapshot distinguishes the mutated bucket.
        let bucket_after = bucket_of(src);
        assert_eq!(
            graph.bucket_slab_prefix_slots(src, &bucket_after),
            stale_replay.slab_slots,
            "in-place tombstone delete leaves the slab/log split unchanged"
        );
        assert_ne!(
            bucket_after.degree(),
            stale_replay.degree,
            "the delete decrements degree, which the snapshot detects"
        );

        let read = |replay: Option<&crate::labeled::HybridOverflowEdgeReplay>| {
            let mut targets = Vec::new();
            let positions_slots: Vec<_> = slots
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at_with_replay(
                    src,
                    road,
                    &positions_slots,
                    OutEdgeOrder::Ascending,
                    replay,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };

        // Ground truth: the sparse path resolves canonical state and drops the deleted edge.
        let expected = read(None);
        assert_eq!(expected.len(), 47);
        assert!(!expected.contains(&deleted_target));

        // The stale replay must be rejected (snapshot mismatch) and fall back to sparse: it must not
        // resurrect the deleted edge from its cached log table.
        let (with_stale, rebuilds) = {
            let _guard = ScanPathGuard::enter();
            let targets = read(Some(&stale_replay));
            (targets, ScanPathGuard::overflow_chain_rebuilds())
        };
        assert_eq!(
            with_stale, expected,
            "a replay captured before an overflow delete must not return the deleted edge"
        );
        assert!(
            rebuilds >= 1,
            "snapshot mismatch must take the sparse fallback"
        );
    }
}
