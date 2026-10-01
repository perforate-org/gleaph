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
    lara::{edge::OutEdgeVisitWindow, operation_error::LaraOperationError},
    traits::CsrVertex,
    traits::{CsrEdge, CsrEdgeTombstone},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::ops::ControlFlow;

use crate::traverse::{Traversal, TraversalOrder, TraversalRequest, TraversalWindow};

#[cfg(feature = "canbench")]
mod bench;

#[cfg(feature = "canbench")]
mod bench_workload;

#[cfg(feature = "canbench")]
mod bench_tree;

use super::{
    BucketMode, BucketSearch, LabeledLaraGraph, LabeledOutEdgesIter, OutEdgeOrder,
    error::LabeledOperationError,
};

use super::iter::{
    LabeledEdgeInlinePropertyBatch, LabeledEdgeInlinePropertyBatchScratch,
    LabeledInlinePropertyValueBatch, LabeledInlinePropertyValueBatchScratch,
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
        self.order.into()
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

    /// Borrows the exact bytes without allocating or copying them.
    #[inline]
    pub fn as_ref(&self) -> InlinePropertyBytesRef<'_> {
        InlinePropertyBytesRef {
            width: self.width,
            bytes: self.bytes(),
        }
    }
}

/// Borrowed view of inline-property bytes valid only for the duration of the
/// traversal callback that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlinePropertyBytesRef<'a> {
    width: u16,
    bytes: &'a [u8],
}

impl<'a> InlinePropertyBytesRef<'a> {
    #[inline]
    fn from_parts(width: u16, bytes: &'a [u8]) -> Self {
        debug_assert_eq!(bytes.len(), usize::from(width));
        Self { width, bytes }
    }

    /// Declared inline-property byte width.
    #[inline]
    pub fn width(self) -> u16 {
        self.width
    }

    /// Exact inline-property bytes.
    #[inline]
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

/// A live edge row together with its exact inline-property bytes.
/// Target inline property bytes per batch; matches the legacy traverse batch sizing.
const EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES: usize = 2048;

/// A live edge row with a borrowed view of its inline-property bytes.
///
/// The edge row remains an owned value because rows are decoded from fixed-width
/// storage as they are visited. The property bytes are borrowed and are valid only
/// for the callback invocation. Dense and slab-only paths borrow directly from their
/// bulk-read buffer; sparse/hybrid paths borrow from a reusable per-callback read buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct EdgeWithInlinePropertyRef<'a, E> {
    /// Topology edge row.
    pub edge: E,
    /// Borrowed inline-property bytes and width.
    pub inline_property: InlinePropertyBytesRef<'a>,
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

    /// Canonical topology point read: distinguishes missing, tombstoned and live
    /// rows in every storage mode. Live rows carry the requested logical position
    /// and storage label; inline-property hydration is a separate value-plane read.
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
        mut visit: impl FnMut(u32, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        // Rank/select callers handle bypass rows with their existing spill-aware iterator.
        if vertex.is_default_edge_labeled() {
            return Ok(ControlFlow::Continue(()));
        }
        let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        for slot_index in 0..self.bucket_reserved_edge_slots(owner, &bucket) {
            match self.read_edge_state_at_slot(&bucket, slot_index)? {
                EdgeSlotState::Missing => break,
                EdgeSlotState::Tombstone => (),
                EdgeSlotState::Live(edge) => {
                    if let ControlFlow::Break(value) = visit(slot_index, edge) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
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
            let mut iter = self.edges.out_edges_iter(&self.vertices, owner)?;
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
        let mut count = 0u32;
        let _ = self.visit_live_edge_slots_until(owner, label, |slot, edge| {
            if slot >= before_slot.raw() {
                return ControlFlow::Break(());
            }
            if edge.neighbor_vid() == neighbor {
                count = count.saturating_add(1);
            }
            ControlFlow::<()>::Continue(())
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
            let mut iter = self.edges.out_edges_iter(&self.vertices, owner)?;
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
        // Fast path: a singleton relation may still have a leading tombstone, so only
        // return early when slot zero is the live requested edge; otherwise scan the
        // logical bucket below.
        if info.degree == 1
            && ordinal == 0
            && let EdgeSlotState::Live(edge) =
                self.read_edge_state(owner, label, BucketEntryPosition::new(0))?
            && edge.neighbor_vid() == neighbor
        {
            return Ok(Some(BucketEntryPosition::new(0)));
        }
        let mut matching = 0u32;
        let mut selected: Option<BucketEntryPosition> = None;
        let _ = self.visit_live_edge_slots_until(owner, label, |slot, edge| {
            if edge.neighbor_vid() != neighbor {
                return ControlFlow::Continue(());
            }
            if matching == ordinal {
                selected = Some(BucketEntryPosition::new(slot));
                return ControlFlow::Break(());
            }
            matching = matching.saturating_add(1);
            ControlFlow::<()>::Continue(())
        })?;
        Ok(selected)
    }

    /// Visits a descriptor-resident window without renumbering survivors around holes.
    /// The tiny point reader and shared classifier preserve slot/label metadata in both orders.
    fn visit_tiny_bucket_edges<B>(
        &self,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        window: TraversalWindow,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert!(bucket.is_tiny_mode());
        let width = bucket.tiny_used_width();
        let offset = window.offset.min(width);
        let limit = window.limit.unwrap_or(width).min(width - offset);
        for index in offset..offset + limit {
            let slot = match order {
                OutEdgeOrder::Ascending => index,
                OutEdgeOrder::Descending => width - 1 - index,
            };
            if let EdgeSlotState::Live(edge) = Self::read_tiny_edge_state_at_slot(bucket, slot)
                && let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge)
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Adds label identity to the store's direct slab-prefix/spill-suffix visitor.
    /// The caller has already dispatched tiny and tree records to their own readers.
    #[inline]
    fn visit_slab_bucket_edges<B>(
        &self,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        window: TraversalWindow,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.edges
            .visit_row_edges(bucket, order.into(), window, |slot, edge| {
                visit(
                    BucketEntryPosition::new(slot),
                    edge.with_label_id(bucket.bucket_label_key().raw()),
                )
            })
            .map_err(Into::into)
    }

    /// Visits every live edge together with its inline-property bytes in a dense,
    /// tombstone-free label bucket by bulk-reading both the topology slab and the
    /// inline-property bytes span in one call each.
    ///
    /// The callback receives an [`EdgeWithInlinePropertyRef`] carrying the borrowed bytes; the
    /// edge itself is *not* redundantly modified via
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
        mut visit: impl for<'a> FnMut(
            BucketEntryPosition,
            EdgeWithInlinePropertyRef<'a, E>,
        ) -> ControlFlow<B>,
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
                        InlinePropertyBytesRef::from_parts(0, &[])
                    } else {
                        let start = slot as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytesRef::from_parts(
                            width,
                            &inline_property_bytes[start..end],
                        )
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlinePropertyRef {
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
                        InlinePropertyBytesRef::from_parts(0, &[])
                    } else {
                        let start = slot as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytesRef::from_parts(
                            width,
                            &inline_property_bytes[start..end],
                        )
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlinePropertyRef {
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
    #[allow(clippy::needless_return)]
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
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read, so a future mode cannot silently inherit a path.
        match BucketMode::from_bucket(bucket) {
            // ADR 0096 §5: tiny buckets scan inline (no slab/log/value reads).
            // Tombstone-inclusive prefix (ordinals are slots; holes skipped via
            // the layout predicate). Attach flag is
            // irrelevant (width ≡ 0 → always empty values); property entries divert
            // here and interpret the bare edges with empty values.
            BucketMode::Tiny => {
                let mut edges = Vec::with_capacity(bucket.degree() as usize);
                let _ = self.visit_tiny_bucket_edges(
                    bucket,
                    OutEdgeOrder::Ascending,
                    TraversalWindow::unbounded(),
                    |slot, edge| {
                        edges.push((slot.raw(), edge));
                        ControlFlow::<()>::Continue(())
                    },
                )?;
                return Ok(super::iter::LabeledSpanIter::inline(edges, order));
            }
            // Slow core iterators read LEG slab+log geometry, which misreads
            // tree roots (GAP-2026-09-02-001). Tree+w>0 diverts upstream (LPB);
            // tree+w=0 lands here: materialize topology via the tree
            // collector. Attach is vacuous (no values reach slow path for
            // tree buckets); Inline carries bare edges like the tiny arm.
            BucketMode::Tree => {
                debug_assert_eq!(
                    bucket.inline_property_byte_width(),
                    0,
                    "tree buckets reaching slow scan carry no values (w>0 diverts upstream)"
                );
                let mut tree_edges = Vec::with_capacity(bucket.degree() as usize);
                let flow: ControlFlow<()> = super::tree_read::visit_tree_mode_label_bucket_edges(
                    self,
                    bucket.bucket_label_key().raw(),
                    bucket,
                    bucket.degree(),
                    OutEdgeOrder::Ascending,
                    |slot, edge| {
                        tree_edges.push((slot, edge));
                        ControlFlow::<()>::Continue(())
                    },
                )?;
                debug_assert!(
                    matches!(flow, ControlFlow::Continue(())),
                    "tree collector never breaks (Continue-only closure)"
                );
                return Ok(super::iter::LabeledSpanIter::inline(tree_edges, order));
            }
            // Slab arm: existing core-iterator construction below.
            BucketMode::Slab => {
                let bucket_index = Self::labeled_bucket_descriptor_index(vertex, bucket_slot)?;
                let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                let successor_start = self.bucket_slab_window_end_exclusive_after_bucket(
                    vertex,
                    bucket_index,
                    bucket,
                )?;
                let acc = LabelEdgeSpanAccess::with_bucket(
                    &self.buckets,
                    slot,
                    *bucket,
                    successor_start,
                    src,
                );
                let log_chains = if attach_inline_property_bytes {
                    self.bucket_inline_property_bytes_log_chain_opt(bucket)
                } else {
                    None
                };
                match order {
                    OutEdgeOrder::Descending => {
                        let iter = self.edges.desc_out_edges_iter(&acc, VertexId::from(0))?;
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
                        let iter = self.edges.out_edges_iter(&acc, VertexId::from(0))?;
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
        }
    }

    #[allow(clippy::needless_return)]
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
            return self
                .edges
                .visit_row_edges(
                    &vertex,
                    order.into(),
                    TraversalWindow::unbounded(),
                    |slot, edge| {
                        visit(
                            BucketEntryPosition::new(slot),
                            edge.with_label_id(label.raw()),
                        )
                    },
                )
                .map_err(Into::into);
        }
        let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        // Mode dispatch precedes storage reads: tiny is inline, slab mode reads
        // initialized prefix plus spill directly, and tree has its own reader.
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets skip the dense bulk path (anchor,
            // not storage); the slow funnel serves them inline.
            BucketMode::Tiny => {
                self.visit_tiny_bucket_edges(&bucket, order, TraversalWindow::unbounded(), visit)
            }
            BucketMode::Slab => {
                self.visit_slab_bucket_edges(&bucket, order, TraversalWindow::unbounded(), visit)
            }
            // Tree arm: LTB walk (arm-guaranteed tree-only).
            BucketMode::Tree => {
                // Tree-mode visits yield tombstone-inclusive slot positions
                // (ADR 0088 §2). Forward Break directly; the ControlFlow<B>
                // primitive propagates it.
                return super::tree_read::visit_tree_mode_label_bucket_edges(
                    self,
                    label.raw(),
                    &bucket,
                    bucket.degree(),
                    order,
                    |slot, edge| visit(BucketEntryPosition::new(slot), edge),
                );
            }
        }
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

    #[allow(clippy::needless_return)]
    /// Visits a bounded window of live edges for one label in the requested order.
    ///
    /// The window cuts the bucket's **tombstone-inclusive position space**
    /// (ADR 0088 §2: `BucketEntryPosition` is the bucket-local slot id):
    /// `offset` skips the first `offset` positions — tombstoned slots
    /// included — and `limit` bounds the number of positions delivered.
    /// The visitor receives only live edges, each at its absolute
    /// tombstone-inclusive position, identically for slab and tree buckets
    /// (Plan 0327 polish: the dense/sparse slab paths previously counted
    /// live ordinals, which disagreed with the tree path on tombstoned
    /// buckets).
    ///
    /// Slab and bypass rows bulk-read only the selected slot range from the slab
    /// prefix and spill suffix. Holes consume positions but never callbacks.
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
            return self
                .edges
                .visit_row_edges(&vertex, order.into(), window, |slot, edge| {
                    visit(
                        BucketEntryPosition::new(slot),
                        edge.with_label_id(label.raw()),
                    )
                })
                .map_err(Into::into);
        }

        let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        // Mode dispatch precedes storage reads. Every arm cuts the requested
        // tombstone-inclusive positions before visiting live rows.
        match BucketMode::from_bucket(&bucket) {
            BucketMode::Tiny => self.visit_tiny_bucket_edges(&bucket, order, window, visit),
            BucketMode::Slab => self.visit_slab_bucket_edges(&bucket, order, window, visit),
            // Tree arm: LTB walk with window (arm-guaranteed tree-only).
            BucketMode::Tree => {
                // Tree-mode visits yield tombstone-inclusive slot positions
                // (ADR 0088 §2, Plan 0327 contract). The window's offset/limit
                // apply in this same space: ascending position == slot id,
                // descending position == `extent - 1 - slot`.
                //
                // S1 (ADR 0094 §4): only the blocks overlapping the window's
                // position range are resolved and walked. The start/end block
                // indices follow the ADR 0088 §2 depth-generic addressing
                // (`block_index = position / B` through `resolve_leaf_block_id`).
                //
                // S2 (ADR 0094 §4): a block whose FULL slot range lies strictly
                // inside the window range is header-first: when its
                // `tombstone_count == block used slots` the block is fully dead
                // and its payload is skipped entirely; otherwise the payload is
                // scanned and the scanned marker count must equal the header
                // count (fail closed on mismatch). Boundary blocks always scan
                // their payload; the position accounting is unchanged because
                // skipped positions are entirely inside the window.
                //
                // Capture `ControlFlow::Break` via a mutable cell to preserve
                // the `visit_edges_window` API contract.
                let extent = bucket.stored_slots_raw();
                let offset = window.offset.min(extent);
                let limit = window
                    .limit
                    .map(|l| l.min(extent - offset))
                    .unwrap_or(extent - offset);
                let end = offset + limit;
                // The window maps to a SLOT range in the tombstone-inclusive
                // position space (Plan 0327): ascending position == slot id, so
                // the slot range is [offset, end); descending position =
                // `extent - 1 - slot`, so positions [offset, end) map to slots
                // [extent - end, extent - offset). S1 resolves only the blocks
                // overlapping this slot range.
                let (range_lo, range_hi) = match order {
                    OutEdgeOrder::Ascending => (offset, end),
                    OutEdgeOrder::Descending => (extent - end, extent - offset),
                };
                let block_b = crate::labeled::tree_csr::B as u32;
                let start_block = range_lo / block_b;
                let end_block_exclusive = u64::from(range_hi).div_ceil(u64::from(block_b)) as u32;
                let leaf_count = u32::try_from(u64::from(extent).div_ceil(u64::from(block_b)))
                    .expect("leaf_count fits u32 for MAX_DEPTH=3");
                let end_block = end_block_exclusive.min(leaf_count);
                let mut break_value: Option<B> = None;
                let block_slots = |block_index: u32| -> (u32, u32) {
                    let first_slot = block_index * block_b;
                    let used = (extent - first_slot).min(block_b);
                    (first_slot, first_slot + used)
                };
                match order {
                    OutEdgeOrder::Ascending => {
                        for block_index in start_block..end_block {
                            let (block_first_slot, block_end_slot) = block_slots(block_index);
                            let block_id = super::tree_write::resolve_leaf_block_id::<E, M>(
                                self,
                                &bucket,
                                block_index,
                            )?;
                            let scan_lo = block_first_slot.max(offset);
                            let scan_hi = block_end_slot.min(end);
                            // S2: a block fully inside the window position range
                            // is header-first. Boundary (partial-overlap) blocks
                            // always scan.
                            let fully_inside = block_first_slot >= offset && block_end_slot <= end;
                            if fully_inside {
                                let header = self.ltb().read_block_header(block_id);
                                let used = block_end_slot - block_first_slot;
                                if u32::from(header.tombstone_count) == used {
                                    // Fully dead: skip the payload entirely.
                                    continue;
                                }
                                // Fully inside the window ⇒ the scan covers the
                                // whole block ⇒ the scan self-verifies against
                                // the header count (fail closed).
                                super::tree_read::scan_tree_block_range(
                                    self,
                                    label.raw(),
                                    block_id,
                                    block_first_slot,
                                    block_end_slot,
                                    scan_lo,
                                    scan_hi,
                                    header.tombstone_count,
                                    &mut |slot, edge| {
                                        if let ControlFlow::Break(value) =
                                            visit(BucketEntryPosition::new(slot), edge)
                                        {
                                            break_value = Some(value);
                                        }
                                    },
                                )?;
                                if break_value.is_some() {
                                    break;
                                }
                                continue;
                            }
                            // Boundary block: payload scan over the window cut;
                            // the in-closure position cut remains exact. Partial
                            // scans carry no header verification.
                            super::tree_read::scan_tree_block_range(
                                self,
                                label.raw(),
                                block_id,
                                block_first_slot,
                                block_end_slot,
                                scan_lo,
                                scan_hi,
                                0,
                                &mut |slot, edge| {
                                    if let ControlFlow::Break(value) =
                                        visit(BucketEntryPosition::new(slot), edge)
                                    {
                                        break_value = Some(value);
                                    }
                                },
                            )?;
                            if break_value.is_some() {
                                break;
                            }
                        }
                    }
                    OutEdgeOrder::Descending => {
                        // Descending position = extent - 1 - slot: the window
                        // positions [range_lo, range_hi) map to the SLOT range
                        // [extent - range_hi, extent - range_lo). Blocks are
                        // derived from the SLOT range and walked downward
                        // (ascending positions = descending slots).
                        for block_index in (start_block..end_block).rev() {
                            let (block_first_slot, block_end_slot) = block_slots(block_index);
                            let block_id = super::tree_write::resolve_leaf_block_id::<E, M>(
                                self,
                                &bucket,
                                block_index,
                            )?;
                            let fully_inside =
                                block_first_slot >= range_lo && block_end_slot <= range_hi;
                            let scan_slot_lo = block_first_slot.max(range_lo);
                            let scan_slot_hi = block_end_slot.min(range_hi);
                            if fully_inside {
                                let header = self.ltb().read_block_header(block_id);
                                let used = block_end_slot - block_first_slot;
                                if u32::from(header.tombstone_count) == used {
                                    continue;
                                }
                                // Fully inside ⇒ full-block scan ⇒ self-verified.
                                super::tree_read::scan_tree_block_range_desc(
                                    self,
                                    label.raw(),
                                    block_id,
                                    block_first_slot,
                                    block_end_slot,
                                    scan_slot_lo,
                                    scan_slot_hi,
                                    header.tombstone_count,
                                    &mut |slot, edge| {
                                        if let ControlFlow::Break(value) =
                                            visit(BucketEntryPosition::new(slot), edge)
                                        {
                                            break_value = Some(value);
                                        }
                                    },
                                )?;
                                if break_value.is_some() {
                                    break;
                                }
                                continue;
                            }
                            super::tree_read::scan_tree_block_range_desc(
                                self,
                                label.raw(),
                                block_id,
                                block_first_slot,
                                block_end_slot,
                                scan_slot_lo,
                                scan_slot_hi,
                                0,
                                &mut |slot, edge| {
                                    if let ControlFlow::Break(value) =
                                        visit(BucketEntryPosition::new(slot), edge)
                                    {
                                        break_value = Some(value);
                                    }
                                },
                            )?;
                            if break_value.is_some() {
                                break;
                            }
                        }
                    }
                }
                if let Some(value) = break_value {
                    return Ok(ControlFlow::Break(value));
                }
                return Ok(ControlFlow::Continue(()));
            }
        }
    }

    /// Visits a bounded logical-slot window and attaches the edge-inline bytes to each row.
    ///
    /// Unlike the offset-based visitors, this method never walks the prefix before `start_slot`.
    /// It selects at most `limit` logical positions directly and resolves only those positions,
    /// so callers can persist `next_slot` as an opaque page cursor without turning later pages
    /// into unbounded rescans. `exhausted` is computed from the bucket's stable logical extent,
    /// including the overflow-log suffix.
    pub(crate) fn visit_edges_from_slot_with_inline_property(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        start_slot: u32,
        limit: u32,
        mut visit: impl FnMut(BucketEntryPosition, E),
    ) -> Result<(u32, bool), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if limit == 0 {
            return Ok((start_slot, false));
        }

        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        let logical_extent = if vertex.is_default_edge_labeled() {
            // Bypass rows have no inline bytes, but their physical slab extent is still the
            // complete logical slot range accepted by `read_edge_state_internal`.
            vertex.stored_slots
        } else {
            let Some(info) = self.read_label_bucket_placement_info(owner, label)? else {
                return Ok((start_slot, true));
            };
            info.stored_edge_slots
                .checked_add(info.edge_spill_rows)
                .ok_or(LabeledOperationError::from(
                    LaraOperationError::CollectAllocationOverflow,
                ))?
        };

        if start_slot >= logical_extent {
            return Ok((logical_extent, true));
        }
        let end_slot = start_slot.saturating_add(limit).min(logical_extent);
        let slots: Vec<_> = (start_slot..end_slot)
            .map(BucketEntryPosition::new)
            .collect();
        let _ =
            self.visit_edges_at_with_inline_property(owner, label, &slots, order, |slot, item| {
                visit(
                    slot,
                    item.edge.with_stored_inline_property_bytes(
                        item.inline_property.width(),
                        item.inline_property.bytes(),
                    ),
                );
                ControlFlow::<()>::Continue(())
            })?;
        Ok((end_slot, end_slot >= logical_extent))
    }

    /// Visits every live edge with inline-property bytes borrowed for the callback.
    ///
    /// Dense buckets borrow directly from the bulk-read property span. Sparse and
    /// hybrid buckets read into one reusable buffer and borrow that buffer for each
    /// callback, avoiding attachment of property bytes to `E`.
    pub(crate) fn visit_edges_with_inline_property<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        visit: impl for<'a> FnMut(
            BucketEntryPosition,
            EdgeWithInlinePropertyRef<'a, E>,
        ) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        self.visit_edges_with_inline_property_impl(owner, &vertex, label, order, visit)
    }

    /// Unchecked borrowed variant of [`Self::visit_edges_with_inline_property`].
    ///
    /// The caller must ensure `owner` is within the vertex range. Property bytes remain valid
    /// only for the callback invocation, just as in the checked visitor.
    #[inline]
    pub(crate) fn visit_edges_with_inline_property_unchecked<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        visit: impl for<'a> FnMut(
            BucketEntryPosition,
            EdgeWithInlinePropertyRef<'a, E>,
        ) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert!(u32::from(owner) < self.vertices.len());
        let vertex = self.vertices.get(owner);
        self.visit_edges_with_inline_property_impl(owner, &vertex, label, order, visit)
    }

    #[allow(clippy::needless_return)]
    fn visit_edges_with_inline_property_impl<B>(
        &self,
        owner: VertexId,
        vertex: &LabeledVertex,
        label: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: impl for<'a> FnMut(
            BucketEntryPosition,
            EdgeWithInlinePropertyRef<'a, E>,
        ) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            let empty = InlinePropertyBytesRef::from_parts(0, &[]);
            return self
                .edges
                .visit_row_edges(
                    vertex,
                    order.into(),
                    TraversalWindow::unbounded(),
                    |slot, edge| {
                        visit(
                            BucketEntryPosition::new(slot),
                            EdgeWithInlinePropertyRef {
                                edge: edge.with_label_id(label.raw()),
                                inline_property: empty,
                            },
                        )
                    },
                )
                .map_err(Into::into);
        }

        let BucketSearch::Found {
            slot: bucket_slot,
            bucket,
        } = self.find_bucket(owner, vertex, label)?
        else {
            return Ok(ControlFlow::Continue(()));
        };
        let width = bucket.inline_property_byte_width();
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read. Tiny arm returns inline (topology + empty
        // values) before any width read; tree/slab share width-gated paths.
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets visit inline (topology + empty values).
            // Width bytes are payload on tiny and must never feed the dense bulk
            // path or the value attach below.
            BucketMode::Tiny => self.visit_tiny_bucket_edges(
                &bucket,
                order,
                TraversalWindow::unbounded(),
                |slot, edge| {
                    visit(
                        slot,
                        EdgeWithInlinePropertyRef {
                            edge,
                            inline_property: InlinePropertyBytesRef::from_parts(0, &[]),
                        },
                    )
                },
            ),
            BucketMode::Tree | BucketMode::Slab => {
                if !bucket.is_tree_mode() && width == 0 {
                    return self.visit_slab_bucket_edges(
                        &bucket,
                        order,
                        TraversalWindow::unbounded(),
                        |slot, edge| {
                            visit(
                                slot,
                                EdgeWithInlinePropertyRef {
                                    edge,
                                    inline_property: InlinePropertyBytesRef::from_parts(0, &[]),
                                },
                            )
                        },
                    );
                }
                // Width is bucket schema here (both modes carry real schema;
                // tiny diverged above). Inner routing below is arm-local: dense
                // bulk reads slab bytes, so tree diverts to LPB/slow; the
                // `!is_tree_mode()` conjunct is that routing, not a mode guard.
                if !bucket.is_tree_mode()
                    && bucket.values_spill_run() == crate::traits::NO_SPILL_RUN
                    && bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN
                    && self.bucket_reserved_edge_slots(owner, &bucket) == bucket.degree()
                {
                    let inline_property_bytes = self.read_bucket_inline_property_bytes_span(
                        owner,
                        &bucket,
                        0,
                        bucket.degree(),
                    )?;
                    return self.visit_slab_bucket_edges(
                        &bucket,
                        order,
                        TraversalWindow::unbounded(),
                        |slot, edge| {
                            let byte_width = usize::from(width);
                            let start = slot.raw() as usize * byte_width;
                            let end = start + byte_width;
                            visit(
                                slot,
                                EdgeWithInlinePropertyRef {
                                    edge,
                                    inline_property: InlinePropertyBytesRef::from_parts(
                                        width,
                                        &inline_property_bytes[start..end],
                                    ),
                                },
                            )
                        },
                    );
                }

                // Plan 0326 LPB-in-tree: tree-mode path with property
                // bytes. The dense path above is slab-only (`!is_tree_mode()`);
                // the slow `single_bucket_span_iter` below is slab-only too
                // (it reads from the LEG slab + log chain). For tree mode
                // we walk the LTB edge tree (depth 1) directly via
                // `visit_tree_mode_label_bucket_edges_with_property`,
                // reading each slot's property value from the LPB
                // (depth-1 property tree) via the property root region.
                //
                // **Position contract** (Plan 0324 REWORK-3): the visit
                // callback receives the tombstone-inclusive `BucketEntryPosition`
                // (slot index, not live ordinal). The closure
                // `visit_tree_mode_label_bucket_edges_with_property` yields
                // `(slot, edge, property_value)` directly from the LTB
                // walk; no `enumerate()` (which would yield live ordinals
                // and break the contract). Arm-local routing: tree+w>0 takes the
                // LPB walk; tree+w=0 falls to the slow span walk below (served
                // via the funnel's own Tree arm).
                if bucket.is_tree_mode() && width > 0 {
                    use super::tree_read::visit_tree_mode_label_bucket_edges_with_property;
                    // LPB tree walk: forward Break directly (ControlFlow<B> primitive).
                    return visit_tree_mode_label_bucket_edges_with_property(
                        self,
                        label.raw(),
                        &bucket,
                        bucket.degree(),
                        order,
                        |slot, edge, value_bytes| {
                            visit(
                                BucketEntryPosition::new(slot),
                                EdgeWithInlinePropertyRef {
                                    edge,
                                    inline_property: InlinePropertyBytesRef::from_parts(
                                        width,
                                        &value_bytes,
                                    ),
                                },
                            )
                        },
                    );
                }
                let mut iter = self.single_bucket_span_iter(
                    owner,
                    vertex,
                    bucket_slot,
                    &bucket,
                    order,
                    false,
                )?;
                let mut inline_property_bytes = Vec::new();
                let mut value_range = 0..0;
                let batch_rows = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES
                    / usize::from(width).max(1))
                .max(1) as u32;
                let mut ordinal = match order {
                    OutEdgeOrder::Ascending => 0,
                    OutEdgeOrder::Descending => bucket.degree().saturating_sub(1),
                };
                while let Some(result) = iter.next_with_slot() {
                    let (slot, edge) = result?;
                    if width > 0 && !value_range.contains(&ordinal) {
                        value_range = match order {
                            OutEdgeOrder::Ascending => {
                                ordinal..ordinal + batch_rows.min(bucket.degree() - ordinal)
                            }
                            OutEdgeOrder::Descending => {
                                (ordinal + 1).saturating_sub(batch_rows)..ordinal + 1
                            }
                        };
                        self.read_bucket_inline_property_bytes_range_into(
                            &bucket,
                            value_range.clone(),
                            &mut inline_property_bytes,
                        )?;
                    }
                    let start = (ordinal - value_range.start) as usize * usize::from(width);
                    let flow = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlinePropertyRef {
                            edge,
                            inline_property: InlinePropertyBytesRef::from_parts(
                                width,
                                &inline_property_bytes[start..start + usize::from(width)],
                            ),
                        },
                    );
                    if let ControlFlow::Break(value) = flow {
                        return Ok(ControlFlow::Break(value));
                    }
                    match order {
                        OutEdgeOrder::Ascending => ordinal = ordinal.saturating_add(1),
                        OutEdgeOrder::Descending => ordinal = ordinal.saturating_sub(1),
                    }
                }
                Ok(ControlFlow::Continue(()))
            }
        }
    }

    /// Visits one label's edges while materializing inline properties directly on the edge.
    ///
    /// This preserves the allocation-free dense-bucket path used by the legacy public API.
    /// The owned `EdgeWithInlineProperty` adapter remains available for callers that explicitly
    /// need an owned result, but the canonical traversal surface is the borrowed visitor above.
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

    #[allow(clippy::needless_return)]
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
            return self
                .edges
                .visit_row_edges(
                    vertex,
                    order.into(),
                    TraversalWindow::unbounded(),
                    |_, edge| {
                        visit(edge.with_label_id(label.raw()));
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .map_err(Into::into);
        }

        let BucketSearch::Found { slot, bucket } = self.find_bucket(owner, vertex, label)? else {
            return Ok(());
        };
        if bucket.degree() == 0 {
            return Ok(());
        }

        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read. Slab arm: dense bulk + slow span below;
        // tree has its own arm (LTB reads).
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets skip the dense bulk path (anchor,
            // not storage); the slow funnel serves them inline.
            BucketMode::Tiny => self
                .visit_tiny_bucket_edges(&bucket, order, TraversalWindow::unbounded(), |_, edge| {
                    visit(edge);
                    ControlFlow::<()>::Continue(())
                })
                .map(|_| ()),
            BucketMode::Slab => {
                let width = bucket.inline_property_byte_width();
                if width == 0 {
                    return self
                        .visit_slab_bucket_edges(
                            &bucket,
                            order,
                            TraversalWindow::unbounded(),
                            |_, edge| {
                                visit(edge);
                                ControlFlow::<()>::Continue(())
                            },
                        )
                        .map(|_| ());
                }
                if bucket.values_spill_run() == crate::traits::NO_SPILL_RUN
                    && bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN
                    && self.bucket_reserved_edge_slots(owner, &bucket) == bucket.degree()
                {
                    let inline_property_bytes = self.read_bucket_inline_property_bytes_span(
                        owner,
                        &bucket,
                        0,
                        bucket.degree(),
                    )?;
                    return self
                        .visit_slab_bucket_edges(
                            &bucket,
                            order,
                            TraversalWindow::unbounded(),
                            |slot, edge| {
                                let start = slot.raw() as usize * usize::from(width);
                                let end = start + usize::from(width);
                                visit(edge.with_stored_inline_property_bytes(
                                    width,
                                    &inline_property_bytes[start..end],
                                ));
                                ControlFlow::<()>::Continue(())
                            },
                        )
                        .map(|_| ());
                }

                let mut iter =
                    self.single_bucket_span_iter(owner, vertex, slot, &bucket, order, true)?;
                while let Some(result) = iter.next_with_slot() {
                    let (_, edge) = result?;
                    visit(edge.with_label_id(label.raw()));
                }
                Ok(())
            }
            // Tree arm: LTB-backed reads (arm-guaranteed tree-only).
            BucketMode::Tree => {
                super::tree_read::visit_tree_mode_label_bucket_edges(
                    self,
                    label.raw(),
                    &bucket,
                    bucket.degree(),
                    order,
                    |slot_idx, edge| {
                        // Mirror the slab path's visit semantics: yield the
                        // edge with the slot index attached. The visitor
                        // signature is `FnMut(E)` (no slot index), so we
                        // drop the slot index here; callers that need it use
                        // `visit_edges_with_inline_property` instead.
                        let _ = slot_idx;
                        visit(edge);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
            }
        }
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
        mut visit: impl for<'a> FnMut(
            BucketEntryPosition,
            EdgeWithInlinePropertyRef<'a, E>,
        ) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if reserved == 0 || bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }
        let stored_slots = bucket.stored_slots();
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
                        InlinePropertyBytesRef::from_parts(0, &[])
                    } else {
                        let start = ordinal as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytesRef::from_parts(
                            width,
                            &inline_property_bytes[start..end],
                        )
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlinePropertyRef {
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
                        InlinePropertyBytesRef::from_parts(0, &[])
                    } else {
                        let start = ordinal as usize * usize::from(width);
                        let end = start + usize::from(width);
                        InlinePropertyBytesRef::from_parts(
                            width,
                            &inline_property_bytes[start..end],
                        )
                    };
                    if let ControlFlow::Break(value) = visit(
                        BucketEntryPosition::new(slot),
                        EdgeWithInlinePropertyRef {
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

    #[allow(clippy::needless_return)]
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
        // A new phase one owns the batch buffers. Values and slot indices must not
        // leak from an earlier label, direction, or interrupted scan.
        scratch.clear();
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
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read. Tiny buckets hold no values: emit nothing
        // (bypass precedent for valueless rows). Tree/slab share the
        // dense/hybrid/slow value paths below (both carry real schema).
        match BucketMode::from_bucket(&bucket) {
            BucketMode::Tiny => {
                return Ok(ControlFlow::Continue(()));
            }
            BucketMode::Tree | BucketMode::Slab => {
                if bucket.degree() == 0 || bucket.inline_property_byte_width() == 0 {
                    return Ok(ControlFlow::Continue(()));
                }
                if crate::labeled::invariants::bucket_dense_inline_property_batch_eligible(&bucket)
                {
                    return self.visit_dense_out_inline_property_batches_for_bucket_next(
                        owner, label, &bucket, order, scratch, &mut visit,
                    );
                }

                if bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN {
                    return self.visit_hybrid_out_inline_property_batches_for_bucket_next(
                        owner, &bucket, order, scratch, &mut visit,
                    );
                }

                let width = usize::from(bucket.inline_property_byte_width());
                let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
                let mut collected_slot_indices = Vec::new();
                let mut collected_values = Vec::new();
                let result =
                    self.visit_edges_with_inline_property(owner, label, order, |slot, item| {
                        if collected_slot_indices.len() == batch_edges {
                            if let ControlFlow::Break(value) =
                                visit(LabeledInlinePropertyValueBatch {
                                    label_id: label,
                                    byte_width: bucket.inline_property_byte_width(),
                                    order,
                                    slot_indices: &collected_slot_indices,
                                    values: &collected_values,
                                    dense: false,
                                })
                            {
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
        }
    }

    fn visit_hybrid_out_inline_property_batches_for_bucket_next<B>(
        &self,
        owner: VertexId,
        bucket: &LabelBucket,
        order: OutEdgeOrder,
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
        visit: &mut impl FnMut(LabeledInlinePropertyValueBatch<'_>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let (slab_slots, used) = self.edges.initialized_row_slots(bucket)?;
        let extent = slab_slots + used;
        // Only a hole-free extent makes an edge slot also a value ordinal. Reading
        // run metadata is sufficient; sparse traversal owns live-rank mapping.
        if extent != bucket.degree() {
            return self.visit_sparse_out_inline_property_batches_for_bucket_next(
                owner,
                bucket.bucket_label_key(),
                bucket,
                order,
                scratch,
                visit,
            );
        }
        let value_slab_slots = bucket.inline_property_bytes_slab_slots().min(slab_slots);
        // Preserve the existing batch boundaries between physical planes, but
        // let one ordered-value reader pair ordinals with bytes in either order.
        let mut ranges = [
            0..value_slab_slots,
            value_slab_slots..slab_slots,
            slab_slots..extent,
        ];
        if order == OutEdgeOrder::Descending {
            ranges.reverse();
        }
        let width = usize::from(bucket.inline_property_byte_width());
        let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
        let mut ordinals = Vec::with_capacity(batch_edges.min(extent as usize));
        for range in ranges {
            let mut remaining = range.end - range.start;
            while remaining > 0 {
                let take = remaining.min(batch_edges as u32);
                let first = match order {
                    OutEdgeOrder::Ascending => range.end - remaining,
                    OutEdgeOrder::Descending => range.start + remaining - take,
                };
                ordinals.clear();
                ordinals.extend(first..first + take);
                if order == OutEdgeOrder::Descending {
                    ordinals.reverse();
                }
                scratch.clear();
                self.append_ordered_inline_property_bytes_ordinals_next(
                    bucket, &ordinals, scratch,
                )?;
                if let ControlFlow::Break(value) = visit(LabeledInlinePropertyValueBatch {
                    label_id: bucket.bucket_label_key(),
                    byte_width: bucket.inline_property_byte_width(),
                    order,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                    dense: false,
                }) {
                    return Ok(ControlFlow::Break(value));
                }
                remaining -= take;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn append_ordered_inline_property_bytes_ordinals_next(
        &self,
        bucket: &LabelBucket,
        ordinals: &[u32],
        scratch: &mut LabeledInlinePropertyValueBatchScratch,
    ) -> Result<(), LabeledOperationError> {
        let width = usize::from(bucket.inline_property_byte_width());
        for group in ordinals.chunk_by(|a, b| a.abs_diff(*b) == 1) {
            let first = group[0];
            let last = group[group.len() - 1];
            let low = first.min(last);
            let high = first
                .max(last)
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.read_bucket_inline_property_bytes_range_into(
                bucket,
                low..high,
                &mut scratch.io_inline_property_bytes,
            )?;
            for &ordinal in group {
                let start = (ordinal - low) as usize * width;
                scratch.slot_indices.push(ordinal);
                scratch
                    .values
                    .extend_from_slice(&scratch.io_inline_property_bytes[start..start + width]);
            }
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
            // Value offset is absolute in the whole-span buffer (first_slot +
            // i), not batch-relative: batch-relative indexing repeats the
            // first batch's values in every batch (slot/value misalignment).
            match order {
                OutEdgeOrder::Ascending => {
                    for i in 0..take as usize {
                        let slot = first_slot + i as u32;
                        let value_off = (first_slot as usize + i) * width;
                        scratch.slot_indices.push(slot);
                        scratch.values.extend_from_slice(
                            &inline_property_bytes[value_off..value_off + width],
                        );
                    }
                }
                OutEdgeOrder::Descending => {
                    for i in (0..take as usize).rev() {
                        let slot = first_slot + i as u32;
                        let value_off = (first_slot as usize + i) * width;
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

    #[allow(clippy::needless_return)]
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
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read. Tiny buckets hold no values: emit nothing
        // (bypass precedent for valueless rows). Tree/slab share the
        // dense/hybrid/slow value paths below (both carry real schema).
        match BucketMode::from_bucket(&bucket) {
            BucketMode::Tiny => {
                return Ok(ControlFlow::Continue(()));
            }
            BucketMode::Tree | BucketMode::Slab => {
                if bucket.degree() == 0 || bucket.inline_property_byte_width() == 0 {
                    return Ok(ControlFlow::Continue(()));
                }
                if crate::labeled::invariants::bucket_dense_inline_property_batch_eligible(&bucket)
                {
                    return self.visit_dense_out_edge_inline_property_batches_for_bucket_next(
                        owner, label, &bucket, order, scratch, &mut visit,
                    );
                }

                let width = usize::from(bucket.inline_property_byte_width());
                let batch_edges = (EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / width.max(1)).max(1);
                let mut iter = self.single_bucket_span_iter(
                    owner,
                    &vertex,
                    _bucket_slot,
                    &bucket,
                    order,
                    true,
                )?;
                loop {
                    scratch.clear();
                    scratch.edges.reserve(batch_edges);
                    scratch.inline_property_bytes.reserve(batch_edges * width);
                    for _ in 0..batch_edges {
                        let Some(result) = iter.next_with_slot() else {
                            break;
                        };
                        let (_, edge) = result?;
                        scratch
                            .inline_property_bytes
                            .extend_from_slice(edge.edge_inline_property_bytes());
                        scratch.edges.push(edge.with_label_id(label.raw()));
                    }
                    if scratch.edges.is_empty() {
                        break;
                    }
                    if let ControlFlow::Break(value) = visit(LabeledEdgeInlinePropertyBatch {
                        label_id: label,
                        byte_width: bucket.inline_property_byte_width(),
                        order,
                        edges: &scratch.edges,
                        inline_property_bytes: &scratch.inline_property_bytes,
                        dense: false,
                    }) {
                        return Ok(ControlFlow::Break(value));
                    }
                }
                Ok(ControlFlow::Continue(()))
            }
        }
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
        if slots.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }
        let selected = order_slots(slots, order);
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        let bucket = if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            self.bypass_bucket_view(&vertex)?
        } else {
            let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)?
            else {
                return Ok(ControlFlow::Continue(()));
            };
            bucket
        };
        self.visit_bucket_edge_slots(&bucket, &selected, order, visit)
    }

    /// Visits selected logical slots with borrowed inline-property bytes.
    ///
    /// Sorts and deduplicates the requested logical slots, emitting only live rows. Tree values
    /// use the original logical position; slab/run values use their independent live sequence.
    /// Property buffers may be prepared before callbacks and are borrowed only for each call.
    /// A callback's `Break` stops delivery and is returned unchanged.
    pub(crate) fn visit_edges_at_with_inline_property<B>(
        &self,
        owner: VertexId,
        label: BucketLabelKey,
        slots: &[BucketEntryPosition],
        order: OutEdgeOrder,
        mut visit: impl for<'a> FnMut(
            BucketEntryPosition,
            EdgeWithInlinePropertyRef<'a, E>,
        ) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if slots.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        let selected = order_slots(slots, order);
        self.ensure_vertex(owner)?;
        let vertex = self.vertices.get(owner);
        if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(ControlFlow::Continue(()));
            }
            return self.visit_bucket_edge_slots(
                &self.bypass_bucket_view(&vertex)?,
                &selected,
                order,
                |slot, edge| {
                    visit(
                        slot,
                        EdgeWithInlinePropertyRef {
                            edge,
                            inline_property: InlinePropertyBytesRef::from_parts(0, &[]),
                        },
                    )
                },
            );
        }

        let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)? else {
            return Ok(ControlFlow::Continue(()));
        };
        if bucket.degree() == 0 {
            return Ok(ControlFlow::Continue(()));
        }

        if bucket.is_tree_mode() {
            // Native values retain the original edge positions, including holes. Slab live-rank
            // mapping and the independent slab/run value sequence do not apply to either tree.
            let width = bucket.inline_property_byte_width();
            let mut bytes = vec![0; usize::from(width)];
            for slot_index in selected {
                let EdgeSlotState::Live(edge) =
                    self.read_edge_state_at_slot(&bucket, slot_index)?
                else {
                    continue;
                };
                if width > 0 {
                    super::tree_read::read_property_value_at_slot(
                        self, &bucket, slot_index, &mut bytes,
                    )?;
                }
                if let ControlFlow::Break(value) = visit(
                    BucketEntryPosition::new(slot_index),
                    EdgeWithInlinePropertyRef {
                        edge,
                        inline_property: InlinePropertyBytesRef::from_parts(width, &bytes),
                    },
                ) {
                    return Ok(ControlFlow::Break(value));
                }
            }
            return Ok(ControlFlow::Continue(()));
        }

        let width = bucket.inline_property_byte_width();
        if width == 0 {
            return self.visit_bucket_edge_slots(&bucket, &selected, order, |slot, edge| {
                visit(
                    slot,
                    EdgeWithInlinePropertyRef {
                        edge,
                        inline_property: InlinePropertyBytesRef::from_parts(0, &[]),
                    },
                )
            });
        }
        let (prefix, spill) = self.edges.initialized_row_slots(&bucket)?;
        let mut selected_edges = Vec::with_capacity(selected.len());
        if prefix + spill == bucket.degree() {
            let _ = self.visit_bucket_edge_slots(&bucket, &selected, order, |slot, edge| {
                selected_edges.push((slot, edge, slot.raw()));
                ControlFlow::<()>::Continue(())
            })?;
        } else {
            // Live values are compact; logical edge slots retain holes. Resolve ranks once in
            // ascending order, retaining only requested edges, then restore the delivery order.
            let requested = |i: usize| {
                selected[match order {
                    OutEdgeOrder::Ascending => i,
                    OutEdgeOrder::Descending => selected.len() - 1 - i,
                }]
            };
            let mut next = 0;
            let mut ordinal = 0u32;
            let end = requested(selected.len() - 1).saturating_add(1);
            let _ = self.visit_slab_bucket_edges(
                &bucket,
                OutEdgeOrder::Ascending,
                TraversalWindow::new(0, Some(end)),
                |slot, edge| {
                    let rank = ordinal;
                    ordinal += 1;
                    while next < selected.len() && requested(next) < slot.raw() {
                        next += 1;
                    }
                    if next < selected.len() && requested(next) == slot.raw() {
                        selected_edges.push((slot, edge, rank));
                        next += 1;
                    }
                    if next == selected.len() {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            )?;
            if order == OutEdgeOrder::Descending {
                selected_edges.reverse();
            }
        }
        let ordinals: Vec<_> = selected_edges
            .iter()
            .map(|(_, _, ordinal)| *ordinal)
            .collect();
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        self.append_ordered_inline_property_bytes_ordinals_next(&bucket, &ordinals, &mut scratch)?;
        for ((slot, edge, _), bytes) in selected_edges
            .into_iter()
            .zip(scratch.values.chunks_exact(usize::from(width)))
        {
            if let ControlFlow::Break(value) = visit(
                slot,
                EdgeWithInlinePropertyRef {
                    edge,
                    inline_property: InlinePropertyBytesRef::from_parts(width, bytes),
                },
            ) {
                return Ok(ControlFlow::Break(value));
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
        let bucket = if vertex.is_default_edge_labeled() {
            if label != self.bypass_storage_label_for(&vertex) {
                return Ok(EdgeSlotState::Missing);
            }
            self.bypass_bucket_view(&vertex)?
        } else {
            let BucketSearch::Found { bucket, .. } = self.find_bucket(owner, &vertex, label)?
            else {
                return Ok(EdgeSlotState::Missing);
            };
            bucket
        };
        self.read_edge_state_at_slot(&bucket, slot.raw())
    }

    /// Shared point-state owner for a resolved descriptor (including a read-only
    /// bypass projection). Mode selects addressing before interpreting any row.
    #[inline]
    fn read_edge_state_at_slot(
        &self,
        bucket: &LabelBucket,
        slot_index: u32,
    ) -> Result<EdgeSlotState<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let edge = match BucketMode::from_bucket(bucket) {
            BucketMode::Tiny => {
                return Ok(Self::read_tiny_edge_state_at_slot(bucket, slot_index));
            }
            BucketMode::Tree => {
                let Some(target) =
                    super::tree_read::tree_mode_random_ordinal_access(self, bucket, slot_index)?
                else {
                    return Ok(EdgeSlotState::Missing);
                };
                E::read_from(&target.to_le_bytes())
            }
            BucketMode::Slab => {
                let prefix = self.edges.initialized_slab_slots(bucket)?;
                if slot_index < prefix {
                    let physical = crate::labeled::slot_index::checked_add_slot_index(
                        bucket.edge_start(),
                        u64::from(slot_index),
                    )
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    self.edges.read_slot(physical)
                } else {
                    if bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN {
                        return Ok(EdgeSlotState::Missing);
                    }
                    let run = self.edges.spill_mint(bucket.edge_spill_run())?;
                    let row = slot_index - prefix;
                    if row >= self.edges.spill_used_len(run) {
                        return Ok(EdgeSlotState::Missing);
                    }
                    self.edges.spill_edge_at(run, row)?
                }
            }
        };
        Ok(Self::decoded_edge_state_at_slot(bucket, slot_index, edge))
    }

    /// Descriptor-only point reader shared by tiny scans and mode-dispatched point reads.
    /// It cannot touch slab, tree or spill storage after the caller has resolved tiny mode.
    #[inline]
    fn read_tiny_edge_state_at_slot(bucket: &LabelBucket, slot_index: u32) -> EdgeSlotState<E>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert!(bucket.is_tiny_mode());
        debug_assert_eq!(E::BYTES, 4, "tiny buckets require 4-byte edges");
        if slot_index >= bucket.tiny_used_width() {
            return EdgeSlotState::Missing;
        }
        let edge = E::read_from(&bucket.tiny_target(slot_index).to_le_bytes());
        Self::decoded_edge_state_at_slot(bucket, slot_index, edge)
    }

    /// One liveness and logical-identity rule, independent of the row's storage addressing.
    #[inline]
    fn decoded_edge_state_at_slot(
        bucket: &LabelBucket,
        slot_index: u32,
        edge: E,
    ) -> EdgeSlotState<E>
    where
        E: CsrEdgeTombstone,
    {
        if edge.is_deleted_slot() || edge.is_tombstone_edge() {
            return EdgeSlotState::Tombstone;
        }
        EdgeSlotState::Live(
            edge.with_slot_index(slot_index)
                .with_label_id(bucket.bucket_label_key().raw()),
        )
    }

    /// One selected-topology owner for resolved descriptors, including bypass projections.
    fn visit_bucket_edge_slots<B>(
        &self,
        bucket: &LabelBucket,
        slots: &[u32],
        order: OutEdgeOrder,
        mut visit: impl FnMut(BucketEntryPosition, E) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if BucketMode::from_bucket(bucket) == BucketMode::Slab {
            return self
                .edges
                .visit_row_edge_slots(bucket, slots, order.into(), |slot, edge| {
                    visit(
                        BucketEntryPosition::new(slot),
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                    )
                })
                .map_err(LabeledOperationError::from);
        }
        for &slot in slots {
            if let EdgeSlotState::Live(edge) = self.read_edge_state_at_slot(bucket, slot)?
                && let ControlFlow::Break(value) = visit(BucketEntryPosition::new(slot), edge)
            {
                return Ok(ControlFlow::Break(value));
            }
        }
        Ok(ControlFlow::Continue(()))
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
    type EdgeWithInlinePropertyRef<'a> = EdgeWithInlinePropertyRef<'a, E>;
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
        visit: impl for<'a> FnMut(Self::Slot, Self::EdgeWithInlinePropertyRef<'a>) -> ControlFlow<B>,
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

    fn visit_edges_at_with_inline_property<B>(
        &self,
        request: &Self::Request,
        slots: &[Self::Slot],
        visit: impl for<'a> FnMut(Self::Slot, Self::EdgeWithInlinePropertyRef<'a>) -> ControlFlow<B>,
    ) -> Result<ControlFlow<B>, Self::Error> {
        LabeledLaraGraph::visit_edges_at_with_inline_property(
            self,
            request.owner,
            request.label,
            slots,
            request.order,
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
        if buckets
            .iter()
            .any(|b| b.edge_spill_run() != crate::traits::NO_SPILL_RUN)
        {
            return None;
        }
        if buckets.iter().any(|b| b.stored_slots() != b.degree()) {
            return None;
        }
        let base = buckets.first()?.edge_start();
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start() != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.stored_slots())?;
            pos = crate::labeled::slot_index::checked_add_slot_index(
                pos,
                u64::from(b.stored_slots()),
            )?;
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
        if buckets
            .iter()
            .any(|b| b.edge_spill_run() != crate::traits::NO_SPILL_RUN)
        {
            return None;
        }
        if buckets.iter().any(|b| b.stored_slots() != b.degree()) {
            return None;
        }
        let base = buckets.first()?.edge_start();
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start() != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.stored_slots())?;
            pos = crate::labeled::slot_index::checked_add_slot_index(
                pos,
                u64::from(b.stored_slots()),
            )?;
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
        let _ =
            self.visit_all_labels_with_inline_property(src, OutEdgeOrder::Ascending, |edge| {
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
            // A bypass row carries its own spill run id (ADR 0097 §3) and its `stored_degree` is
            // the total backing width, so the synthetic bucket takes the *prefix* width (the
            // bypass reader's `stored_degree - run rows` derivation) and hands the run over:
            // without the run the iterator would read only the prefix and lose the spill rows.
            let spill_rows = match self.edges.spill_mint(vertex.bypass_spill_run()) {
                Ok(run) => self.edges.spill_used_len(run),
                Err(_) => 0,
            };
            let mut bucket = LabelBucket::from_parts(
                label,
                vertex.base_slot_start(),
                vertex.degree(),
                vertex.stored_degree().saturating_sub(spill_rows),
            );
            if spill_rows > 0 {
                bucket = bucket.with_edge_spill_run(vertex.bypass_spill_run());
            }
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
            && bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN)
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

    /// Visits matching outgoing edges in ascending materialization order with optional offset and
    /// limit.
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
            offset,
            limit,
            raw_matches,
            &mut matches,
            &mut visit,
        )
    }

    /// Visits all outgoing edges in ascending materialization order with optional offset and limit.
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

    /// Collects outgoing edges for `src` in ascending materialization order.
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
            None,
            None,
            None,
            &mut |_| true,
            &mut |e| out.push(e),
        )?;
        Ok(out)
    }

    /// Returns an iterator over outgoing edges in explicit descending scan order.
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Descending, None)
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
    use super::super::{EdgePlacementPolicy, LEAF_VERTEX_EDGE_SEGMENT_DENSITY};
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
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
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
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
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
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
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
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
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
    fn visit_edges_window_cuts_tombstone_inclusive_positions_when_tombstones_make_bucket_sparse() {
        let (graph, src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph.compact_vertex_edge_span(src, 0).unwrap();
        // Slot 2 (target 3) is tombstoned; positions are tombstone-inclusive.
        graph.remove_edge_at_slot(src, label, 2).unwrap();
        let mut request = LabeledTraversalRequest {
            owner: src,
            label,
            order: OutEdgeOrder::Ascending,
        };

        // Window (1, Some(2)) covers positions 1 and 2; position 2 is the
        // tombstone, which consumes window space but yields nothing.
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
        assert_eq!(out, vec![(1, 2)]);

        // Unbounded window yields all live edges at their absolute positions.
        let mut all = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::unbounded(),
            |slot, edge| {
                all.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(all, vec![(0, 1), (1, 2), (3, 4), (4, 5)]);

        // Descending: request-order positions run from the highest slot;
        // window (1, Some(2)) skips position 0 (slot 4) and covers
        // positions 1..3 (slots 3 and 2; position 2 is the tombstone).
        request.order = OutEdgeOrder::Descending;
        let mut desc = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(1, Some(2)),
            |slot, edge| {
                desc.push((slot.raw(), edge.target));
                ControlFlow::Continue(())
            },
        )
        .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(desc, vec![(3, 4)]);
    }

    #[test]
    fn visit_edges_window_tree_and_slab_agree_on_tombstone_inclusive_positions() {
        // Tree-side half lives in `tree_read.rs`
        // (`visit_edges_window_tree_matches_slab_tombstone_positions`),
        // where the promote helpers are in scope. This slab-side half
        // fixes the expectation: 5 live edges, slot 2 tombstoned, window
        // (1, Some(3)) covers positions 1..4; position 2 is the
        // tombstone (consumes window space, yields nothing), so the live
        // edges come back at positions 1 and 3.
        let (slab_graph, slab_src) = bucket_graph();
        let label = BucketLabelKey::directed_from_index(2);
        for target in 1..=5u32 {
            slab_graph
                .insert_edge(
                    slab_src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        slab_graph.compact_vertex_edge_span(slab_src, 0).unwrap();
        slab_graph.remove_edge_at_slot(slab_src, label, 2).unwrap();
        let mut out = Vec::new();
        let flow: ControlFlow<()> = Traversal::visit_edges_window(
            &slab_graph,
            &LabeledTraversalRequest {
                owner: slab_src,
                label,
                order: OutEdgeOrder::Ascending,
            },
            TraversalWindow::new(1, Some(3)),
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
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
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
                .insert_edge(
                    src,
                    default_label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
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
                .insert_edge(
                    src,
                    default_label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
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

    fn selected_property_value(target: u32, width: u16) -> Vec<u8> {
        if width == 0 {
            Vec::new()
        } else {
            retained_compaction_value(target, width)
        }
    }

    // Existing edge/memory specialization, with retained handles for a complete read-only oracle.
    fn selected_property_tree_fixture(
        width: u16,
    ) -> (
        LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory>,
        [crate::VectorMemory; 16],
    ) {
        let memories = labeled_memories();
        let graph = LabeledLaraGraph::new(
            memories[0].clone(),
            memories[1].clone(),
            memories[2].clone(),
            memories[3].clone(),
            memories[4].clone(),
            memories[5].clone(),
            memories[6].clone(),
            memories[7].clone(),
            memories[8].clone(),
            memories[9].clone(),
            memories[10].clone(),
            memories[11].clone(),
            memories[12].clone(),
            memories[13].clone(),
            memories[14].clone(),
            memories[15].clone(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
        let tree = BucketLabelKey::from_raw(3);
        for (label, targets) in [
            (BucketLabelKey::from_raw(2), 20_000..20_016),
            (tree, 1000..2024),
        ] {
            if width > 0 {
                graph
                    .ensure_label_bucket_inline_property_byte_width(owner, label, width)
                    .unwrap();
            }
            for target in targets {
                graph
                    .insert_edge_skip_leaf_cascade(
                        owner,
                        label,
                        InlinePropertyTestEdge::with_bytes(
                            target,
                            &selected_property_value(target, width),
                        ),
                        EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
            if label != tree {
                graph.compact_vertex_edge_span(owner, 0).unwrap();
            }
        }
        // Real slab/run removals, never the stale live-property tree writer.
        for slot in [0, 3, 511, 1023] {
            assert_eq!(
                graph
                    .remove_edge_at_slot(owner, tree, slot)
                    .unwrap()
                    .unwrap()
                    .target,
                1000 + slot
            );
        }
        super::super::promote::promote_bucket_to_tree_mode(&graph, owner, tree).unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                owner,
                tree,
                InlinePropertyTestEdge::with_bytes(2024, &selected_property_value(2024, width)),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
        if width > 0 {
            graph
                .ensure_label_bucket_inline_property_byte_width(peer, tree, width)
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                peer,
                tree,
                InlinePropertyTestEdge::with_bytes(30_000, &selected_property_value(30_000, width)),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        (graph, memories)
    }

    fn check_selected_tree_property(width: u16) {
        let (graph, memories) = selected_property_tree_fixture(width);
        let snapshot = || {
            memories
                .iter()
                .map(|m| m.borrow().clone())
                .collect::<Vec<_>>()
        };
        let before = snapshot();
        let owner = VertexId::from(0);
        let label = BucketLabelKey::from_raw(3);
        let BucketSearch::Found { bucket, .. } = graph
            .find_bucket(owner, &graph.vertices.get(owner), label)
            .unwrap()
        else {
            panic!("tree missing");
        };
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.bucket_label_key(), label);
        assert_eq!((bucket.degree(), bucket.stored_slots()), (1021, 1025));
        assert_eq!(graph.edges().header().num_edges, 1038);
        assert_eq!(super::super::compact::bucket_span_region_len(&bucket), 2);
        // Both runs are absent: the compact-range bulk fallback cannot mask the direct path.
        assert_eq!(bucket.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(bucket.values_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(bucket.inline_property_byte_width(), width);
        let slots: Vec<_> = [
            1024,
            3,
            512,
            1,
            124,
            1025,
            123,
            0,
            511,
            1023,
            2,
            125,
            510,
            513,
            1022,
            u32::MAX,
            124,
        ]
        .into_iter()
        .map(BucketEntryPosition::new)
        .collect();
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            // Independently constructed raw oracle includes all four transcribed holes.
            let mut raw_expected: Vec<_> = (0..1025)
                .map(|slot| {
                    if [0, 3, 511, 1023].contains(&slot) {
                        (slot, u32::MAX, vec![0; usize::from(width)])
                    } else {
                        (
                            slot,
                            1000 + slot,
                            selected_property_value(1000 + slot, width),
                        )
                    }
                })
                .collect();
            if order == OutEdgeOrder::Descending {
                raw_expected.reverse();
            }
            let mut raw = Vec::new();
            let flow = if width == 0 {
                super::super::tree_read::visit_tree_mode_label_bucket_edges(
                    &graph,
                    label.raw(),
                    &bucket,
                    bucket.degree(),
                    order,
                    |slot, edge| {
                        raw.push((slot, edge.target, Vec::new()));
                        ControlFlow::<()>::Continue(())
                    },
                )
            } else {
                super::super::tree_read::visit_tree_mode_label_bucket_edges_with_property(
                    &graph,
                    label.raw(),
                    &bucket,
                    bucket.degree(),
                    order,
                    |slot, edge, bytes| {
                        raw.push((slot, edge.target, bytes.to_vec()));
                        ControlFlow::<()>::Continue(())
                    },
                )
            }
            .unwrap();
            assert_eq!(flow, ControlFlow::Continue(()));
            assert_eq!(raw, raw_expected);

            let mut expected: Vec<_> = [1, 2, 123, 124, 125, 510, 512, 513, 1022, 1024]
                .into_iter()
                .map(|slot| {
                    (
                        slot,
                        1000 + slot,
                        selected_property_value(1000 + slot, width),
                    )
                })
                .collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            let request = LabeledTraversalRequest {
                owner,
                label,
                order,
            };
            let mut seen = Vec::new();
            let result = Traversal::visit_edges_at_with_inline_property(
                &graph,
                &request,
                &slots,
                |slot, item| {
                    assert_eq!(item.edge.edge_slot_index_raw(), slot.raw());
                    assert_eq!(item.inline_property.width(), width);
                    seen.push((
                        slot.raw(),
                        item.edge.target,
                        item.inline_property.bytes().to_vec(),
                    ));
                    ControlFlow::<()>::Continue(())
                },
            );
            eprintln!(
                "SELECTED_PROPERTY width={width} order={order:?} result={result:?} rows={}",
                seen.len()
            );
            assert_eq!(
                snapshot(),
                before,
                "selected reads must not change any of the 16 memories"
            );
            assert_eq!(result.unwrap(), ControlFlow::Continue(()));
            assert_eq!(seen, expected);

            let mut stopped = Vec::new();
            assert_eq!(
                graph
                    .visit_edges_at_with_inline_property(
                        owner,
                        label,
                        &slots,
                        order,
                        |slot, item| {
                            stopped.push((
                                slot.raw(),
                                item.edge.target,
                                item.inline_property.bytes().to_vec(),
                            ));
                            ControlFlow::Break(String::from("selected stop"))
                        }
                    )
                    .unwrap(),
                ControlFlow::Break(String::from("selected stop"))
            );
            assert_eq!(stopped, expected[..1]);

            for (start, limit, end, exhausted, live) in [
                (510, 4, 514, false, vec![510, 512, 513]),
                (1022, 4, 1025, true, vec![1022, 1024]),
                (1025, 1, 1025, true, vec![]),
            ] {
                let mut window = Vec::new();
                assert_eq!(
                    graph
                        .visit_edges_from_slot_with_inline_property(
                            owner,
                            label,
                            order,
                            start,
                            limit,
                            |slot, edge| {
                                assert_eq!(edge.edge_slot_index_raw(), slot.raw());
                                assert_eq!(edge.edge_inline_property_byte_width(), width);
                                window.push((
                                    slot.raw(),
                                    edge.target,
                                    edge.edge_inline_property_bytes().to_vec(),
                                ));
                            }
                        )
                        .unwrap(),
                    (end, exhausted)
                );
                let mut expected: Vec<_> = live
                    .into_iter()
                    .map(|slot| {
                        (
                            slot,
                            1000 + slot,
                            selected_property_value(1000 + slot, width),
                        )
                    })
                    .collect();
                if order == OutEdgeOrder::Descending {
                    expected.reverse();
                }
                assert_eq!(window, expected);
            }
            assert_eq!(snapshot(), before);
        }
        // Request labels/owners select distinct canonical rows, never the tree's adjacent slab or peer.
        for (owner, label, selected, expected) in [
            (
                VertexId::from(0),
                label,
                vec![0, 3, 511, 1023, 1025, u32::MAX],
                vec![],
            ),
            (
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                vec![1],
                vec![],
            ),
            (
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                vec![1, 15, 16],
                vec![(1, 20_001), (15, 20_015)],
            ),
            (VertexId::from(1), label, vec![0, 1], vec![(0, 30_000)]),
            (VertexId::from(0), label, vec![], vec![]),
        ] {
            let mut seen = Vec::new();
            let slots: Vec<_> = selected.into_iter().map(BucketEntryPosition::new).collect();
            assert_eq!(
                graph
                    .visit_edges_at_with_inline_property(
                        owner,
                        label,
                        &slots,
                        OutEdgeOrder::Ascending,
                        |slot, item| {
                            assert_eq!(
                                item.inline_property.bytes(),
                                selected_property_value(item.edge.target, width)
                            );
                            seen.push((slot.raw(), item.edge.target));
                            ControlFlow::<()>::Continue(())
                        }
                    )
                    .unwrap(),
                ControlFlow::Continue(())
            );
            assert_eq!(seen, expected);
        }
        assert_eq!(snapshot(), before);
    }

    #[test]
    fn selected_property_tree_eight_byte_boundaries() {
        check_selected_tree_property(8);
    }

    #[test]
    fn selected_property_tree_thirty_three_byte_boundaries() {
        check_selected_tree_property(33);
    }

    #[test]
    fn selected_property_tree_zero_width() {
        check_selected_tree_property(0);
    }

    #[test]
    fn selected_property_tree_corrupt_roots_propagate() {
        use crate::labeled::ltb_raw_block_store::BlockError;
        let (graph, memories) = selected_property_tree_fixture(33);
        let snapshot = || {
            memories
                .iter()
                .map(|m| m.borrow().clone())
                .collect::<Vec<_>>()
        };
        let before = snapshot();
        let owner = VertexId::from(0);
        let label = BucketLabelKey::from_raw(3);
        let BucketSearch::Found { bucket, .. } = graph
            .find_bucket(owner, &graph.vertices.get(owner), label)
            .unwrap()
        else {
            panic!("tree missing");
        };
        let property_root =
            bucket.edge_start() + u64::from(super::super::compact::bucket_span_region_len(&bucket));
        let bad_id = graph.ltb().tail_next();
        // Bad property root in the second 124-value leaf, then the partial second edge leaf.
        for (root, selected_slot, other_slot) in [
            (property_root + 1, 124, 1024),
            (bucket.edge_start() + 1, 1024, 124),
        ] {
            let mut saved = [0u8; 4];
            graph.edges().read_slot_bytes(root, &mut saved);
            graph
                .edges()
                .write_slots_contiguous(root, &bad_id.to_le_bytes())
                .unwrap();
            let corrupt = snapshot();
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                let mut calls = 0;
                let result = graph.visit_edges_at_with_inline_property(
                    owner,
                    label,
                    &[BucketEntryPosition::new(selected_slot)],
                    order,
                    |_, _| {
                        calls += 1;
                        ControlFlow::<()>::Continue(())
                    },
                );
                assert_eq!(calls, 0);
                assert_eq!(snapshot(), corrupt);
                assert!(
                    matches!(result, Err(LabeledOperationError::LtbBlock(BlockError::NotMinted { id })) if id == bad_id),
                    "{result:?}"
                );
                // A sparse selection in another leaf must not walk the poisoned unselected root.
                // This has no callback Break and adds no error-after-Break/prefetch guarantee.
                let mut seen = Vec::new();
                assert_eq!(
                    graph
                        .visit_edges_at_with_inline_property(
                            owner,
                            label,
                            &[BucketEntryPosition::new(other_slot)],
                            order,
                            |slot, item| {
                                seen.push((
                                    slot.raw(),
                                    item.edge.target,
                                    item.inline_property.bytes().to_vec(),
                                ));
                                ControlFlow::<()>::Continue(())
                            },
                        )
                        .unwrap(),
                    ControlFlow::Continue(())
                );
                assert_eq!(
                    seen,
                    vec![(
                        other_slot,
                        1000 + other_slot,
                        selected_property_value(1000 + other_slot, 33)
                    )]
                );
                assert_eq!(snapshot(), corrupt);
            }
            graph.edges().write_slots_contiguous(root, &saved).unwrap();
            assert_eq!(snapshot(), before);
        }
    }

    #[test]
    fn selected_property_retained_independent_planes() {
        for width in [8, 33] {
            let graph = retained_compaction_fixture(width);
            let owner = VertexId::from(0);
            let label = BucketLabelKey::from_raw(2);
            let before = graph
                .read_vertex_label_buckets(&graph.vertices.get(owner))
                .unwrap();
            // The shared fixture asserts prefix8/run9 versus value-prefix13/run1 and real holes.
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                let mut expected: Vec<_> = [1, 7, 8, 15, 16]
                    .into_iter()
                    .map(|slot| (slot, slot, retained_compaction_value(slot, width)))
                    .collect();
                if order == OutEdgeOrder::Descending {
                    expected.reverse();
                }
                let mut seen = Vec::new();
                let slots: Vec<_> = [16, 9, 8, 0, 1, 7, 15, 3, 17, u32::MAX, 16]
                    .into_iter()
                    .map(BucketEntryPosition::new)
                    .collect();
                assert_eq!(
                    graph
                        .visit_edges_at_with_inline_property(
                            owner,
                            label,
                            &slots,
                            order,
                            |slot, item| {
                                assert_eq!(item.edge.edge_slot_index_raw(), slot.raw());
                                assert_eq!(item.inline_property.width(), width);
                                seen.push((
                                    slot.raw(),
                                    item.edge.target,
                                    item.inline_property.bytes().to_vec(),
                                ));
                                ControlFlow::<()>::Continue(())
                            }
                        )
                        .unwrap(),
                    ControlFlow::Continue(())
                );
                assert_eq!(seen, expected);
                assert_eq!(
                    graph
                        .read_vertex_label_buckets(&graph.vertices.get(owner))
                        .unwrap(),
                    before
                );
                assert_eq!(
                    retained_compaction_rows(&graph, VertexId::from(1), label, order),
                    vec![(0, 19_000, retained_compaction_value(19_000, width))]
                );
            }
            assert_eq!(graph.edges().header().num_edges, 18);
        }
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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

        let mut borrowed_rows = Vec::new();
        let _ = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |slot, item| {
                    borrowed_rows.push((
                        slot.raw(),
                        item.edge.target,
                        item.inline_property.width(),
                        item.inline_property.bytes().to_vec(),
                    ));
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(
            borrowed_rows,
            vec![
                (0, 7, 2, 10u16.to_le_bytes().to_vec()),
                (1, 7, 2, 20u16.to_le_bytes().to_vec()),
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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

        // Phase 2: read topology and borrowed inline properties at selected slots.
        let mut seen = 0u32;
        let _ = graph
            .visit_edges_at_with_inline_property::<()>(
                src,
                label,
                &slots,
                OutEdgeOrder::Ascending,
                |slot, item| {
                    let edge = item.edge;
                    assert_eq!(item.inline_property.width(), 4);
                    let expected = (edge.target * 7).to_le_bytes().to_vec();
                    assert_eq!(item.inline_property.bytes(), expected);
                    assert_eq!(edge.edge_slot_index_raw(), slot.raw());
                    seen += 1;
                    ControlFlow::Continue(())
                },
            )
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

        let mut borrowed_streamed = 0u32;
        let flow = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Descending,
                |_slot, item| {
                    let expected = (item.edge.target * 7).to_le_bytes();
                    assert_eq!(item.inline_property.width(), 4);
                    assert_eq!(item.inline_property.bytes(), &expected);
                    borrowed_streamed += 1;
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(borrowed_streamed, slots.len() as u32);

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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
            graph
                .insert_edge(
                    src,
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let mut called = false;
        let _ = graph
            .visit_edges_at_with_inline_property::<()>(
                src,
                label,
                &[BucketEntryPosition::new(0)],
                OutEdgeOrder::Ascending,
                |_slot, item| {
                    called = true;
                    assert_eq!(item.inline_property.width(), 0);
                    assert!(item.inline_property.bytes().is_empty());
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert!(called);
    }

    #[test]
    fn selected_reads_keep_owner_identity_and_reject_missing_values() {
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    label,
                    InlinePropertyTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
        // A neighboring owner's phase-one scan must not affect A's selected reads.
        let mut targets = Vec::new();
        let _ = graph
            .visit_edges_at::<()>(
                a,
                label,
                &[BucketEntryPosition::new(0), BucketEntryPosition::new(47)],
                OutEdgeOrder::Ascending,
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
        // A descriptor that claims values beyond its slab while no run holds them.
        let malformed = bucket
            .with_inline_property_bytes_slab_slots(0)
            .without_values_spill_run();
        graph
            .buckets
            .write_label_bucket_slot_for_test(slot, malformed)
            .unwrap();
        let visitor_error = graph
            .visit_edges_at_with_inline_property::<()>(
                a,
                label,
                &[BucketEntryPosition::new(1)],
                OutEdgeOrder::Ascending,
                |_, _| ControlFlow::Continue(()),
            )
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
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 11 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let walk = BucketLabelKey::from_raw(3);
        graph
            .insert_edge(
                VertexId::from(0),
                walk,
                TestEdge { target: 20 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let mut expected = graph.out_edges(VertexId::from(0)).unwrap();
        expected.reverse();
        let lazy: Vec<_> = graph
            .desc_out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(lazy, expected);
    }

    #[test]
    fn labeled_desc_out_edges_iter_matches_reversed_materialized_rows() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 11 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let mut expected = graph.out_edges(VertexId::from(0)).unwrap();
        expected.reverse();
        assert_eq!(
            graph
                .desc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
    }

    #[test]
    fn labeled_out_edges_iter_advance_by_and_nth_match_scan() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 11 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(2),
                TestEdge { target: 10 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(4),
                TestEdge { target: 40 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
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

    /// GAP-2026-09-20-004: leaf counts are canonical, the internal path is
    /// derived. The hot insert/remove path writes the leaf row only, and
    /// `rebuild_counts_internal_nodes` restores every internal row from the
    /// leaves on demand.
    ///
    /// Wrong implementation this fails on: eager ancestor maintenance (the
    /// pre-change shape), where the internal root would already match the leaf
    /// sums before the rebuild — the test asserts it does not, i.e. that the
    /// rebuild is load-bearing.
    #[test]
    fn counts_internal_nodes_are_derived_and_repaired_on_demand() {
        use crate::labeled::record::LabeledVertex;
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        // Five edges per bucket promote tiny -> slab (K=4 keeps four inline and
        // those bank nothing in the leaf), so every edge walks (and used to
        // walk) the counts path. The fixture's segment size is 32, so 40
        // vertices span two PMA leaves.
        const VERTICES: u32 = 40;
        const PER_VERTEX: u32 = 5;
        for _ in 0..VERTICES {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        for vid in 0..VERTICES {
            for k in 0..PER_VERTEX {
                graph
                    .insert_edge(
                        VertexId::from(vid),
                        road,
                        TestEdge {
                            target: vid * 16 + k,
                        },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }

        let seg = graph.edges().header().segment_count;
        let leaf_rows: Vec<i64> = (0..seg)
            .map(|leaf| {
                graph
                    .edges()
                    .counts_store()
                    .get(u64::from(seg + leaf))
                    .actual
            })
            .collect();
        assert_eq!(
            leaf_rows.iter().sum::<i64>(),
            i64::from(VERTICES * PER_VERTEX),
            "leaves are canonical and carry every live edge"
        );
        assert!(
            leaf_rows.iter().filter(|a| **a > 0).count() >= 2,
            "fixture spans two leaves"
        );

        // Before the repair the internal path is stale (eager maintenance is gone).
        let root_before = graph.edges().counts_store().get(1).actual;
        assert_ne!(
            root_before,
            leaf_rows.iter().sum::<i64>(),
            "internal rows must be stale until repaired (leaf-only hot path)"
        );

        graph.rebuild_counts_internal_nodes();
        let counts = graph.edges().counts_store();
        assert_eq!(
            counts.get(1).actual,
            leaf_rows.iter().sum::<i64>(),
            "repaired root equals the leaf sum"
        );
        for idx in 1..seg {
            let left = counts.get(u64::from(idx * 2));
            let right = counts.get(u64::from(idx * 2 + 1));
            let node = counts.get(u64::from(idx));
            assert_eq!(
                node.actual,
                left.actual + right.actual,
                "repaired internal row {idx} must equal its children"
            );
        }

        // A further mutation leaves the root stale again (the repair is on demand,
        // not automatic), while the leaf row tracks it immediately.
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 9_999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert_eq!(
            graph.edges().counts_store().get(u64::from(seg)).actual,
            leaf_rows[0] + 1
        );
        assert_eq!(
            graph.edges().counts_store().get(1).actual,
            leaf_rows.iter().sum::<i64>(),
            "internal rows stay derived until the next repair"
        );
    }

    #[test]
    fn normal_labeled_edges_update_pma_leaf_segment_counts() {
        let graph = test_graph();
        // ADR 0096 §4/§3b: tiny edges reserve no slots (invisible to PMA
        // counts), so promote the four inline seeds explicitly to materialize
        // the slab span and its counts.
        let label = BucketLabelKey::from_raw(2);
        for target in [10u32, 11, 12, 13] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        crate::labeled::graph::test_support::promote_bucket_to_slab(
            &graph,
            VertexId::from(0),
            label,
        );

        let header = graph.edges().header();
        let first_leaf = graph
            .edges()
            .counts_store()
            .get(u64::from(header.segment_count));
        assert_eq!(first_leaf.actual, 4);
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
            .insert_edge(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .insert_edge(
                VertexId::from(0),
                walk,
                TestEdge { target: 20 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        for target in [100, 101] {
            graph
                .insert_edge(
                    bypass_tail,
                    graph.default_label(),
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let catalog = BucketLabelKey::from_raw(42);
        for target in [200, 201] {
            graph
                .insert_edge(
                    catalog_tail,
                    catalog,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
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
        assert_eq!(graph.out_edges(hub).unwrap().len(), total);
        assert_eq!(
            graph
                .desc_out_edges_iter(hub)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .len(),
            total
        );
    }

    fn hybrid_contract_value(target: u32, width: u16) -> Vec<u8> {
        let mut bytes: Vec<_> = (0..width)
            .map(|i| (target.wrapping_add(u32::from(i) * 17) & 255) as u8)
            .collect();
        bytes[..8].copy_from_slice(&(u64::from(target) + 1).to_le_bytes());
        bytes
    }

    fn hybrid_contract_fixture(
        width: u16,
        holes: bool,
    ) -> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(owner, label, width)
            .unwrap();
        let bucket = || {
            let vertex = graph.vertices().get(owner);
            let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        for target in 0..96 {
            if target == 8 {
                super::super::promote::compact_run_into_prefix_with_growth_retry(
                    &graph,
                    owner,
                    0,
                    bucket(),
                )
                .unwrap();
            }
            if target == 32 {
                let other = BucketLabelKey::from_raw(3);
                graph
                    .ensure_label_bucket_inline_property_byte_width(owner, other, width)
                    .unwrap();
                graph
                    .insert_edge_skip_leaf_cascade(
                        owner,
                        other,
                        InlinePropertyTestEdge::with_bytes(
                            9000,
                            &hybrid_contract_value(9000, width),
                        ),
                        EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
            graph
                .insert_edge_skip_leaf_cascade(
                    owner,
                    label,
                    InlinePropertyTestEdge::with_bytes(
                        target,
                        &hybrid_contract_value(target, width),
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        if holes {
            for slot in [0, 3, 31, 63] {
                graph
                    .remove_edge_at_slot(owner, label, slot)
                    .unwrap()
                    .unwrap();
            }
        }
        let other = BucketLabelKey::from_raw(4);
        graph
            .ensure_label_bucket_inline_property_byte_width(owner, other, width)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                owner,
                other,
                InlinePropertyTestEdge::with_bytes(9001, &hybrid_contract_value(9001, width)),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                owner,
                label,
                InlinePropertyTestEdge::with_bytes(96, &hybrid_contract_value(96, width)),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let b = bucket();
        assert!(b.stored_slots() > 3 && b.stored_slots() <= 31);
        assert_ne!(b.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_ne!(b.values_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_ne!(b.stored_slots(), b.inline_property_bytes_slab_slots());
        graph
    }

    #[test]
    fn hybrid_range_reads_bound_io_without_topology_prefetch() {
        use crate::test_support::CountMemory;
        for width in [8, 33] {
            let memories: [CountMemory; 16] = std::array::from_fn(|_| CountMemory::default());
            let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = memories.clone();
            let graph = LabeledLaraGraph::<InlinePropertyTestEdge, _>::new(
                a,
                b,
                c,
                d,
                e,
                f,
                g,
                h,
                i,
                j,
                k,
                l,
                m,
                n,
                o,
                p,
                crate::labeled::InitialCapacities::uniform(4096),
                BucketLabelKey::from_raw(1),
            )
            .unwrap();
            let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
            let label = BucketLabelKey::from_raw(2);
            for slot in 0..256 {
                if slot == 32 {
                    let peer = BucketLabelKey::from_raw(3);
                    graph
                        .ensure_label_bucket_inline_property_byte_width(owner, peer, width)
                        .unwrap();
                    graph
                        .insert_edge_skip_leaf_cascade(
                            owner,
                            peer,
                            InlinePropertyTestEdge::with_bytes(
                                9000,
                                &hybrid_contract_value(9000, width),
                            ),
                            EdgePlacementPolicy::Insertion,
                        )
                        .unwrap();
                }
                graph
                    .ensure_label_bucket_inline_property_byte_width(owner, label, width)
                    .unwrap();
                graph
                    .insert_edge_skip_leaf_cascade(
                        owner,
                        label,
                        InlinePropertyTestEdge::with_bytes(
                            slot + 10,
                            &hybrid_contract_value(slot, width),
                        ),
                        EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
            let vertex = graph.vertices().get(owner);
            let bucket = graph
                .buckets()
                .read_label_bucket_slot(graph.find_bucket_slot(&vertex, label).unwrap().unwrap())
                .unwrap();
            assert_eq!(bucket.stored_slots(), 0);
            assert_eq!(bucket.inline_property_bytes_slab_slots(), 32);
            assert_ne!(bucket.edge_spill_run(), crate::traits::NO_SPILL_RUN);
            assert_ne!(bucket.values_spill_run(), crate::traits::NO_SPILL_RUN);
            let snapshot = || {
                memories
                    .iter()
                    .map(|m| m.inner.borrow().clone())
                    .collect::<Vec<_>>()
            };
            let before = snapshot();
            let reset = || {
                for m in &memories {
                    m.reads.set(0);
                    m.bytes.set(0);
                }
            };
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                reset();
                let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
                let mut actual = Vec::new();
                graph
                    .visit_out_inline_property_batches_for_label(
                        owner,
                        label,
                        order,
                        &mut scratch,
                        |batch| {
                            actual.extend(
                                batch.slot_indices.iter().copied().zip(
                                    batch
                                        .values
                                        .chunks_exact(width as usize)
                                        .map(<[u8]>::to_vec),
                                ),
                            );
                        },
                    )
                    .unwrap();
                let mut expected: Vec<_> = (0..256)
                    .map(|slot| (slot, hybrid_contract_value(slot, width)))
                    .collect();
                if order == OutEdgeOrder::Descending {
                    expected.reverse();
                }
                assert_eq!(actual, expected);
                assert_eq!(memories[5].bytes.get(), 0);
                assert_eq!(
                    memories[6].bytes.get(),
                    4 * memories[6].reads.get(),
                    "phase one reads headers, not edge rows"
                );
                if width == 8 {
                    assert!(
                        memories[13].reads.get() <= 8,
                        "value run must be read in ranges"
                    );
                    assert!(memories[13].bytes.get() <= 224 * 8 + 32);
                }
                reset();
                let selected: Vec<_> = (0..256).map(BucketEntryPosition::new).collect();
                let mut seen = Vec::new();
                let _ = graph
                    .visit_edges_at(owner, label, &selected, order, |slot, edge| {
                        seen.push((slot.raw(), edge.target));
                        ControlFlow::<()>::Continue(())
                    })
                    .unwrap();
                assert_eq!(
                    seen,
                    expected
                        .iter()
                        .map(|(slot, _)| (*slot, slot + 10))
                        .collect::<Vec<_>>()
                );
                assert_eq!(memories[6].reads.get(), 3);
                assert_eq!(memories[6].bytes.get(), 256 * 4 + 8);
                assert_eq!(memories[10].reads.get(), 0);
                assert_eq!(memories[13].reads.get(), 0);
                assert_eq!(snapshot(), before);
            }
            for slot in [0, 64, 128] {
                graph
                    .remove_edge_at_slot(owner, label, slot)
                    .unwrap()
                    .unwrap();
            }
            let before = snapshot();
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                reset();
                let mut seen = Vec::new();
                let flow = graph
                    .visit_edges_at_with_inline_property(
                        owner,
                        label,
                        &[
                            BucketEntryPosition::new(4),
                            BucketEntryPosition::new(1),
                            BucketEntryPosition::new(0),
                        ],
                        order,
                        |slot, item| {
                            seen.push((
                                slot.raw(),
                                item.edge.target,
                                item.inline_property.bytes().to_vec(),
                            ));
                            ControlFlow::<()>::Continue(())
                        },
                    )
                    .unwrap();
                let mut expected: Vec<_> = [1, 4]
                    .into_iter()
                    .map(|slot| (slot, slot + 10, hybrid_contract_value(slot, width)))
                    .collect();
                if order == OutEdgeOrder::Descending {
                    expected.reverse();
                }
                assert_eq!(flow, ControlFlow::Continue(()));
                assert_eq!(seen, expected);
                assert_eq!(
                    memories[6].bytes.get(),
                    5 * 4 + 3 * 4,
                    "rank reads stop at the last selected position"
                );
                assert_eq!(snapshot(), before);
            }
        }
    }

    #[test]
    fn hybrid_contract_holes_keep_exact_nonzero_values() {
        for width in [8, 33] {
            let graph = hybrid_contract_fixture(width, true);
            let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
            graph
                .visit_out_inline_property_batches_for_label(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(3),
                    OutEdgeOrder::Ascending,
                    &mut scratch,
                    |_| {},
                )
                .unwrap();
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                let mut actual = Vec::new();
                let flow = graph
                    .visit_out_inline_property_batches_for_label_next(
                        VertexId::from(0),
                        BucketLabelKey::from_raw(2),
                        order,
                        &mut scratch,
                        |batch| {
                            assert_eq!(batch.byte_width, width);
                            actual.extend(
                                batch.slot_indices.iter().copied().zip(
                                    batch
                                        .values
                                        .chunks_exact(width as usize)
                                        .map(<[u8]>::to_vec),
                                ),
                            );
                            ControlFlow::<()>::Continue(())
                        },
                    )
                    .unwrap();
                let mut expected: Vec<_> = (0..97)
                    .filter(|slot| ![0, 3, 31, 63].contains(slot))
                    .map(|slot| (slot, hybrid_contract_value(slot, width)))
                    .collect();
                if order == OutEdgeOrder::Descending {
                    expected.reverse();
                }
                assert_eq!(flow, ControlFlow::Continue(()));
                assert_eq!(actual, expected, "width={width} order={order:?}");
            }
        }
    }

    #[test]
    fn hybrid_contract_break_stops_between_stages() {
        for width in [8, 33] {
            for holes in [false, true] {
                let graph = hybrid_contract_fixture(width, holes);
                let vertex = graph.vertices().get(VertexId::from(0));
                let slot = graph
                    .find_bucket_slot(&vertex, BucketLabelKey::from_raw(2))
                    .unwrap()
                    .unwrap();
                let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
                assert!(bucket.inline_property_bytes_slab_slots() >= bucket.stored_slots());
                for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                    let mut expected: Vec<_> = (0..97)
                        .filter(|slot| {
                            if holes {
                                ![0, 3, 31, 63].contains(slot)
                            } else if order == OutEdgeOrder::Ascending {
                                *slot < bucket.stored_slots()
                            } else {
                                *slot >= bucket.stored_slots()
                            }
                        })
                        .collect();
                    if order == OutEdgeOrder::Descending {
                        expected.reverse();
                    }
                    expected.truncate(EDGE_INLINE_PROPERTY_BATCH_TARGET_BYTES / usize::from(width));
                    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
                    let mut calls = 0;
                    let flow = graph
                        .visit_out_inline_property_batches_for_label_next(
                            VertexId::from(0),
                            BucketLabelKey::from_raw(2),
                            order,
                            &mut scratch,
                            |batch| {
                                assert_eq!(batch.slot_indices, expected);
                                assert_eq!(
                                    batch.values,
                                    expected
                                        .iter()
                                        .flat_map(|&slot| hybrid_contract_value(slot, width))
                                        .collect::<Vec<_>>()
                                );
                                calls += 1;
                                ControlFlow::Break(17)
                            },
                        )
                        .unwrap();
                    assert_eq!(
                        flow,
                        ControlFlow::Break(17),
                        "width={width} holes={holes} order={order:?}"
                    );
                    assert_eq!(calls, 1);
                }
            }
        }
    }

    #[test]
    fn hybrid_contract_phase_two_keeps_run_ordinals() {
        for width in [8, 33] {
            let graph = hybrid_contract_fixture(width, false);
            for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
                let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
                let mut selected = Vec::new();
                graph
                    .visit_out_inline_property_batches_for_label(
                        VertexId::from(0),
                        BucketLabelKey::from_raw(2),
                        order,
                        &mut scratch,
                        |batch| {
                            for (&slot, value) in batch
                                .slot_indices
                                .iter()
                                .zip(batch.values.chunks_exact(width as usize))
                            {
                                assert_eq!(value, hybrid_contract_value(slot, width));
                                if [8, 31, 63, 96].contains(&slot) {
                                    selected.push(BucketEntryPosition::new(slot));
                                }
                            }
                        },
                    )
                    .unwrap();
                let mut actual = Vec::new();
                let flow = graph
                    .visit_edges_at(
                        VertexId::from(0),
                        BucketLabelKey::from_raw(2),
                        &selected,
                        order,
                        |slot, edge| {
                            actual.push((slot.raw(), edge.target));
                            ControlFlow::<()>::Continue(())
                        },
                    )
                    .unwrap();
                let mut expected = vec![(8, 8), (31, 31), (63, 63), (96, 96)];
                if order == OutEdgeOrder::Descending {
                    expected.reverse();
                }
                assert_eq!(flow, ControlFlow::Continue(()));
                assert_eq!(actual, expected, "width={width} order={order:?}");
            }
        }
    }

    /// Identically shaped buckets belonging to different owners must never share selected rows.
    #[test]
    fn hybrid_selected_reads_keep_same_leaf_owners_separate() {
        let graph = inline_property_test_graph_with_capacity(1 << 16);
        let a = graph.push_vertex(LabeledVertex::default()).unwrap();
        let b = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        for v in [a, b] {
            graph
                .ensure_label_bucket_inline_property_byte_width(v, road, 2u16)
                .unwrap();
        }
        // Identical geometry, disjoint target ranges (A: 1..=48, B: 1001..=1048).
        for target in 1..=48u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    a,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            let bt = 1000 + target;
            graph
                .insert_edge_skip_leaf_cascade(
                    b,
                    road,
                    InlinePropertyTestEdge::with_bytes(bt, &(bt as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
        assert!(
            bucket_a.edge_spill_run() != crate::traits::NO_SPILL_RUN
                && bucket_b.edge_spill_run() != crate::traits::NO_SPILL_RUN
        );
        // The matching prefix widths must not be mistaken for matching owner identity.
        assert_eq!(
            graph.bucket_slab_prefix_slots(a, &bucket_a),
            graph.bucket_slab_prefix_slots(b, &bucket_b),
        );

        // Phase 1 on A captures slots; a subsequent scan of B cannot change A's reads.
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
        let read_a = || {
            let mut targets = Vec::new();
            let positions_slots_a: Vec<_> = slots_a
                .iter()
                .copied()
                .map(crate::traverse::BucketEntryPosition::new)
                .collect();
            graph
                .visit_edges_at(
                    a,
                    road,
                    &positions_slots_a,
                    OutEdgeOrder::Ascending,
                    |_slot, edge| {
                        targets.push(edge.target);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            targets
        };
        assert_eq!(read_a(), (1..=48).collect::<Vec<_>>());
    }

    #[test]
    fn hybrid_inline_property_bytes_first_keeps_exact_values() {
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for target in 49..=64u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        // This fixture requires a retained run, but not a nonempty slab prefix.
        assert!(bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN);
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
    }

    /// Full phase-two selection preserves every phase-one logical position and target.
    #[test]
    fn phase2_selection_yields_exact_canonical_edges() {
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(src);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN);

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
        let positions: Vec<_> = slots.into_iter().map(BucketEntryPosition::new).collect();
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut actual = Vec::new();
            let _ = graph
                .visit_edges_at(src, road, &positions, order, |slot, edge| {
                    actual.push((slot.raw(), edge.target));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            let mut expected: Vec<_> = (0..48).map(|slot| (slot, slot + 1)).collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            assert_eq!(actual, expected);
        }
    }

    /// An in-place spill tombstone between phases must be observed by selected reads.
    #[test]
    fn hybrid_selected_reads_observe_in_place_spill_delete() {
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let bucket_of = |v| {
            let vertex = graph.vertices().get(v);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let bucket = bucket_of(src);
        assert!(bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN);
        let slab_prefix = graph.bucket_slab_prefix_slots(src, &bucket);

        // Capture positions before an in-place delete; this mutation does not renumber slots.
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

        // Delete one overflow-log edge (first slot past the slab prefix): an in-place tombstone.
        let removed = graph
            .remove_edge_at_slot(src, road, slab_prefix)
            .unwrap()
            .expect("removed an overflow-log edge");
        let deleted_target = removed.target;

        let bucket_after = bucket_of(src);
        assert_eq!(
            graph.bucket_slab_prefix_slots(src, &bucket_after),
            slab_prefix
        );
        assert_eq!(bucket_after.degree(), bucket.degree() - 1);
        let positions: Vec<_> = slots.into_iter().map(BucketEntryPosition::new).collect();
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut actual = Vec::new();
            let _ = graph
                .visit_edges_at(src, road, &positions, order, |slot, edge| {
                    actual.push((slot.raw(), edge.target));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            let mut expected: Vec<_> = (0..48)
                .filter(|s| *s != slab_prefix)
                .map(|slot| (slot, slot + 1))
                .collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            assert_eq!(actual, expected);
            assert!(actual.iter().all(|(_, target)| *target != deleted_target));
        }
    }
}

#[cfg(test)]
mod g6_zero_read_tests {
    use super::super::test_support::*;
    use super::*;

    /// G6 scan arm: visiting a tiny bucket performs zero edge-slab / edge-log /
    /// inline-property / span reads. Same harness as the insert proof; the scan
    /// must cost no more than the insert's descriptor+vertex rows (it reads one
    /// fewer row — no publish — so strictly fewer writes).
    ///
    /// Wrong-implementation probe (executed 2026-09-19): a 4B slab-span read
    /// planted in this arm moves the shape (98,0) -> (102,0) and fails the
    /// bound. Serving the scan from the slab span (`[anchor, anchor+degree)`
    /// bytes — the pre-§5 funnel behavior) reads slab bytes and fails likewise.
    #[test]
    fn g6_tiny_scan_reads_no_edge_bytes() {
        let (graph, reads, writes) = counting_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        force_tiny_bucket(&graph, vid, label, &[10, 11, 12]);
        reads.set(0);
        writes.set(0);
        let mut seen = Vec::new();
        graph
            .visit_edges(vid, label, OutEdgeOrder::Ascending, |_, edge| {
                seen.push(u32::from(edge.neighbor_vid()));
                ControlFlow::<()>::Continue(())
            })
            .unwrap();
        assert_eq!(seen, vec![10, 11, 12]);
        let r = reads.get();
        let w = writes.get();
        // Exact shape: one descriptor-row read + vertex/lookup reads, zero writes
        // (scan publishes nothing). Any slab-span read would add >= 4B (one
        // edge target) plus span-meta bytes — the bound has no room for it.
        // MEASURED at the spill swap, and reported as a disagreement with the derived table
        // (predicted 105R/0W, measured 109R/0W: the wider descriptor row straddles page boundaries
        // on the descriptor reads).
        assert_eq!((r, w), (107, 0), "tiny scan byte shape changed");
    }
}
