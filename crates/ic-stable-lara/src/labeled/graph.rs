//! Single-orientation labeled LARA graph orchestration.
//!
//! [`LabeledLaraGraph`] mirrors [`crate::LaraGraph`]: it owns the vertex column
//! plus the storage layers required to mutate one CSR orientation. The extra
//! bucket layer is kept small and relocatable. Normal labeled edge bytes live in
//! the regular [`EdgeStore`] slab/free-span store and participate in the same
//! PMA segment [`crate::lara::edge::counts::SegmentEdgeCounts`] accounting as
//! core LARA: each [`LabeledVertex`]'s [`LabeledVertex::stored_slots`]
//! contributes `total` while live edges contribute `actual`. A **cascade** from
//! per-label edge span grow/shrink propagates through the owning **VertexEdgeSpan**
//! into per-leaf density checks (compaction then optional slack growth).

use crate::{
    SegmentId, VertexCount, VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketDirectedness, BucketLabelKey},
        bucket_store::{DirectednessPartitionStrategy, LabelBucketStore},
        record::{LabelBucket, LabeledVertex, LabeledVertexFieldError},
        slot_index::{ValueWidthCode, checked_add_slot_index},
    },
    lara::{
        edge::{
            AscOutEdgesIter, EdgeStore, InitError as EdgeInitError, InsertLocation,
            OutEdgeSlabIter, OutEdgeVisitWindow, OutEdgesIter,
            counts::{SegmentEdgeCounts, segment_span_density},
            segment_tree_leaf_count,
        },
        edge_value::{EdgeValueStore, InitError as ValueInitError},
        operation_error::LaraOperationError,
        vertex::{InitError as VertexInitError, VertexStore},
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::{cell::Cell, cmp::Ordering, fmt, iter::FusedIterator, marker::PhantomData, num::NonZero};

const DEFAULT_SEGMENT_SIZE: u32 = 32;
const BULK_BUCKET_SEARCH_MIN_DEGREE: u32 = 16;
const BUCKET_LOOKUP_CACHE_ENTRIES: usize = 64;

/// Same threshold as core LARA leaf density (`actual/total` on one PMA leaf).
const LEAF_VERTEX_EDGE_SEGMENT_DENSITY: f64 = 1.0;

enum BucketSearch {
    Found { slot: u64, bucket: LabelBucket },
    Missing { insert_index: u32 },
}

#[derive(Clone, Copy)]
struct BucketLookupCache {
    vid: VertexId,
    bucket_key: BucketLabelKey,
    base_slot_start: u64,
    degree: u32,
    slot: u64,
}

/// Errors returned by labeled graph operations.
#[derive(Debug)]
pub enum LabeledOperationError {
    /// Addressing a vertex outside `0..vertex_count`.
    VertexOutOfRange {
        /// Requested vertex id.
        vid: VertexId,
        /// Current vertex column length.
        len: VertexCount,
    },
    /// Underlying LARA store operation failed.
    Store(LaraOperationError),
    /// A default-label bypass was requested for a row that cannot use it.
    InvalidDefaultBypass,
    /// Vertex row fields are inconsistent with labeled bucket-mode limits.
    InvalidVertexRow(LabeledVertexFieldError),
}

impl fmt::Display for LabeledOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
            Self::Store(err) => write!(f, "{err}"),
            Self::InvalidDefaultBypass => write!(
                f,
                "default-label bypass requires exactly one default adjacency label"
            ),
            Self::InvalidVertexRow(err) => write!(f, "invalid labeled vertex row: {err:?}"),
        }
    }
}

impl std::error::Error for LabeledOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::VertexOutOfRange { .. }
            | Self::InvalidDefaultBypass
            | Self::InvalidVertexRow(_) => None,
        }
    }
}

impl From<LabeledVertexFieldError> for LabeledOperationError {
    fn from(err: LabeledVertexFieldError) -> Self {
        Self::InvalidVertexRow(err)
    }
}

impl From<LabeledVertexFieldError> for LaraOperationError {
    fn from(err: LabeledVertexFieldError) -> Self {
        match err {
            LabeledVertexFieldError::LabelBucketCountOverflow
            | LabeledVertexFieldError::LabelBucketDescriptorSpanOverflow => Self::RowDegreeOverflow,
            LabeledVertexFieldError::SlotIndexOverflow
            | LabeledVertexFieldError::MetadataReservedBitSet
            | LabeledVertexFieldError::BypassOverflowLogHeadOutOfRange
            | LabeledVertexFieldError::ValueAllocatedBytesOverflow => {
                Self::CollectAllocationOverflow
            }
        }
    }
}

impl From<super::record::LabelBucketFieldError> for LabeledOperationError {
    fn from(err: super::record::LabelBucketFieldError) -> Self {
        Self::Store(err.into())
    }
}

impl From<super::record::LabelBucketFieldError> for LaraOperationError {
    fn from(err: super::record::LabelBucketFieldError) -> Self {
        match err {
            super::record::LabelBucketFieldError::SlotIndexOverflow => {
                Self::CollectAllocationOverflow
            }
            super::record::LabelBucketFieldError::ReservedTopBitSet
            | super::record::LabelBucketFieldError::OverflowLogHeadOutOfRange
            | super::record::LabelBucketFieldError::ValueOffsetOverflow
            | super::record::LabelBucketFieldError::ValueLogHeadOutOfRange
            | super::record::LabelBucketFieldError::ValueWidthCodeReserved => {
                Self::CollectAllocationOverflow
            }
        }
    }
}

impl From<LaraOperationError> for LabeledOperationError {
    fn from(value: LaraOperationError) -> Self {
        Self::Store(value)
    }
}

impl From<crate::GrowFailed> for LabeledOperationError {
    fn from(value: crate::GrowFailed) -> Self {
        Self::Store(LaraOperationError::RebalanceFailed(value))
    }
}

/// Errors returned when reopening a labeled graph.
#[derive(Debug)]
pub enum InitError {
    /// The vertex column could not be reopened.
    Vertices(VertexInitError),
    /// The label-bucket subsystem could not be reopened.
    Buckets(crate::labeled::LabelBucketStoreInitError),
    /// The edge subsystem could not be reopened.
    Edges(EdgeInitError),
    /// The edge-value byte slab could not be reopened.
    Values(ValueInitError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vertices(e) => write!(f, "vertex init failed: {e}"),
            Self::Buckets(e) => write!(f, "bucket init failed: {e}"),
            Self::Edges(e) => write!(f, "edge init failed: {e}"),
            Self::Values(e) => write!(f, "value slab init failed: {e}"),
        }
    }
}

impl std::error::Error for InitError {}

/// Outgoing-edge scan order for APIs that expose both the hot descending walk and the stable
/// ascending materialization order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OutEdgeOrder {
    /// Default hot-path order: label buckets high→low; within each span, overflow log head first
    /// and then slab slots high→low.
    #[default]
    Descending,
    /// Stable materialization order: label buckets low→high; within each span, CSR slots low→high.
    Ascending,
}

impl OutEdgeOrder {
    fn ascending(self) -> bool {
        matches!(self, Self::Ascending)
    }
}

/// Single-orientation multi-level labeled CSR graph.
pub struct LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    vertices: VertexStore<LabeledVertex, M>,
    buckets: LabelBucketStore<M>,
    edges: EdgeStore<E, M>,
    values: EdgeValueStore<M>,
    default_label: BucketLabelKey,
    last_bucket_lookup: Cell<Option<BucketLookupCache>>,
    bucket_lookup_cache: [Cell<Option<BucketLookupCache>>; BUCKET_LOOKUP_CACHE_ENTRIES],
    _marker: PhantomData<E>,
}

/// Slot relocation produced while compacting one labeled adjacency row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeSlotMove {
    /// Label row whose local slot changed.
    pub label_id: BucketLabelKey,
    /// Old slot index inside the label row.
    pub old_slot_index: u32,
    /// New slot index inside the label row.
    pub new_slot_index: u32,
}

/// Result of one incremental [`LabeledLaraGraph::compact_vertex_edge_span_one_step`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VertexEdgeSpanCompactOneStep {
    /// One live edge was relocated inside its label bucket; sidecars should follow `move`.
    EdgeMoved(EdgeSlotMove),
    /// The current label bucket is packed; continue from the next bucket index.
    AdvanceBucket(u32),
    /// Overflow-log buckets required a full span rewrite; `moves` lists slot rewrites.
    OverflowRewrite(Vec<EdgeSlotMove>),
    /// The vertex span is fully compacted.
    Finished,
}

/// Streaming iterator over outgoing edges in a fixed scan order (see
/// [`LabeledLaraGraph::desc_out_edges_iter`], [`LabeledLaraGraph::asc_out_edges_iter`], and
/// [`LabeledLaraGraph::out_edges_by_directedness_iter`]).
pub struct LabeledOutEdgesIter<'a, E: CsrEdge, M: Memory> {
    graph: &'a LabeledLaraGraph<E, M>,
    src: VertexId,
    order: OutEdgeOrder,
    kind: LabeledOutEdgesIterKind<'a, E, M>,
}

enum LabeledOutEdgesIterKind<'a, E: CsrEdge, M: Memory> {
    Empty,
    BypassDesc {
        label_id: BucketLabelKey,
        iter: OutEdgeSlabIter<'a, E, M>,
    },
    BypassAsc {
        label_id: BucketLabelKey,
        iter: AscOutEdgesIter<'a, E, M>,
    },
    Buckets {
        vertex: LabeledVertex,
        buckets: Vec<LabelBucket>,
        base_bucket_index: u32,
        next_bucket: Option<usize>,
        current: LabeledSpanIter<'a, E, M>,
    },
}

enum LabeledSpanIter<'a, E: CsrEdge, M: Memory> {
    Empty,
    Desc {
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        vertex: LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        log_chains: Option<(Vec<u32>, Vec<u32>)>,
        iter: OutEdgesIter<'a, E, M>,
    },
    Asc {
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        vertex: LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        label_id: BucketLabelKey,
        log_chains: Option<(Vec<u32>, Vec<u32>)>,
        iter: AscOutEdgesIter<'a, E, M>,
    },
}

impl<'a, E, M> Iterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.kind {
                LabeledOutEdgesIterKind::Empty => return None,
                LabeledOutEdgesIterKind::BypassDesc { label_id, iter } => {
                    return iter.next().map(|edge| edge.with_label_id(label_id.raw()));
                }
                LabeledOutEdgesIterKind::BypassAsc { label_id, iter } => {
                    return iter.next().map(|edge| edge.with_label_id(label_id.raw()));
                }
                LabeledOutEdgesIterKind::Buckets {
                    vertex,
                    buckets,
                    base_bucket_index,
                    next_bucket,
                    current,
                } => {
                    if let Some(edge) = current.next() {
                        return Some(edge);
                    }
                    let local = next_bucket.take()?;
                    if self.order == OutEdgeOrder::Descending {
                        *next_bucket = local.checked_sub(1);
                    } else {
                        let next = local + 1;
                        if next < buckets.len() {
                            *next_bucket = Some(next);
                        }
                    }
                    if buckets[local].degree() == 0 {
                        continue;
                    }
                    let bucket_index = base_bucket_index.checked_add(local as u32)?;
                    *current = self
                        .graph
                        .labeled_bucket_span_iter(
                            self.src,
                            self.order,
                            vertex,
                            buckets,
                            local,
                            bucket_index,
                        )
                        .ok()?;
                }
            }
        }
    }

    fn advance_by(&mut self, mut n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        loop {
            match &mut self.kind {
                LabeledOutEdgesIterKind::Empty => {
                    return Err(NonZero::new(n).expect("n > 0"));
                }
                LabeledOutEdgesIterKind::BypassDesc { iter, .. } => return iter.advance_by(n),
                LabeledOutEdgesIterKind::BypassAsc { iter, .. } => return iter.advance_by(n),
                LabeledOutEdgesIterKind::Buckets { current, .. } => match current {
                    LabeledSpanIter::Empty => match self.roll_to_next_bucket_span() {
                        Ok(()) => continue,
                        Err(()) => return Err(NonZero::new(n).expect("n > 0")),
                    },
                    LabeledSpanIter::Desc { iter, .. } => match iter.advance_by(n) {
                        Ok(()) => return Ok(()),
                        Err(left) => {
                            n = left.get();
                            *current = LabeledSpanIter::Empty;
                        }
                    },
                    LabeledSpanIter::Asc { iter, .. } => match iter.advance_by(n) {
                        Ok(()) => return Ok(()),
                        Err(left) => {
                            n = left.get();
                            *current = LabeledSpanIter::Empty;
                        }
                    },
                },
            }
        }
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }
}

impl<'a, E, M> FusedIterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

impl<'a, E, M> LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn empty(graph: &'a LabeledLaraGraph<E, M>, src: VertexId, order: OutEdgeOrder) -> Self {
        Self {
            graph,
            src,
            order,
            kind: LabeledOutEdgesIterKind::Empty,
        }
    }

    /// Advances past the exhausted current span to the next non-empty bucket span, mirroring
    /// [`Iterator::next`] roll logic for [`LabeledOutEdgesIterKind::Buckets`].
    fn roll_to_next_bucket_span(&mut self) -> Result<(), ()> {
        let LabeledOutEdgesIterKind::Buckets {
            vertex,
            buckets,
            base_bucket_index,
            next_bucket,
            current,
        } = &mut self.kind
        else {
            return Err(());
        };
        loop {
            let local = match next_bucket.take() {
                Some(l) => l,
                None => {
                    *current = LabeledSpanIter::Empty;
                    return Err(());
                }
            };
            if self.order == OutEdgeOrder::Descending {
                *next_bucket = local.checked_sub(1);
            } else {
                let next = local + 1;
                if next < buckets.len() {
                    *next_bucket = Some(next);
                }
            }
            if buckets[local].degree() == 0 {
                continue;
            }
            let bucket_index = match base_bucket_index.checked_add(local as u32) {
                Some(b) => b,
                None => continue,
            };
            *current = self
                .graph
                .labeled_bucket_span_iter(
                    self.src,
                    self.order,
                    vertex,
                    buckets,
                    local,
                    bucket_index,
                )
                .map_err(|_| ())?;
            return Ok(());
        }
    }
}

impl<E, M> Iterator for LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Empty => None,
            Self::Desc {
                graph,
                src,
                vertex,
                bucket_index,
                bucket,
                label_id,
                log_chains,
                iter,
            } => iter.next().map(|edge| {
                let slot = edge.edge_slot_index_raw();
                graph.attach_edge_value(
                    *src,
                    vertex,
                    *bucket_index,
                    *bucket,
                    slot,
                    edge.with_label_id(label_id.raw()),
                    log_chains.as_ref(),
                )
            }),
            Self::Asc {
                graph,
                src,
                vertex,
                bucket_index,
                bucket,
                label_id,
                log_chains,
                iter,
            } => iter.next().map(|edge| {
                let slot = edge.edge_slot_index_raw();
                graph.attach_edge_value(
                    *src,
                    vertex,
                    *bucket_index,
                    *bucket,
                    slot,
                    edge.with_label_id(label_id.raw()),
                    log_chains.as_ref(),
                )
            }),
        }
    }

    fn advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        match self {
            Self::Empty => Err(NonZero::new(n).expect("n > 0")),
            Self::Desc { iter, .. } => iter.advance_by(n),
            Self::Asc { iter, .. } => iter.advance_by(n),
        }
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }
}

impl<E, M> FusedIterator for LabeledSpanIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn edge_matches_label_lookup(candidate: &E, needle: &E) -> bool
    where
        E: PartialEq,
    {
        if candidate.neighbor_vid() != needle.neighbor_vid() {
            return false;
        }
        if let Some(label_id) = needle.edge_label_id_raw() {
            if candidate.edge_label_id_raw() != Some(label_id) {
                return false;
            }
            if candidate.edge_slot_index_raw() != needle.edge_slot_index_raw() {
                return false;
            }
            let width = needle.edge_value_byte_width();
            if width != 0 {
                return candidate.edge_value_byte_width() == width
                    && candidate.edge_value_bytes() == needle.edge_value_bytes();
            }
            return true;
        }
        let width = needle.edge_value_byte_width();
        if width != 0 {
            return candidate.edge_value_byte_width() == width
                && candidate.edge_value_bytes() == needle.edge_value_bytes();
        }
        candidate == needle
    }

    /// Creates a fresh labeled graph over the supplied stable memories.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vertices: M,
        buckets: M,
        bucket_free_spans: M,
        bucket_free_span_by_start: M,
        edge_counts: M,
        edges: M,
        edge_log: M,
        edge_span_meta: M,
        edge_free_spans: M,
        edge_free_span_by_start: M,
        value_slab: M,
        value_free_spans: M,
        value_free_span_by_start: M,
        value_log: M,
        elem_capacity: u64,
        default_label: BucketLabelKey,
    ) -> Result<Self, crate::GrowFailed> {
        crate::slab_index::validate_elem_capacity_grow_failed(elem_capacity, edges.size())?;
        let segment_count = segment_tree_leaf_count(VertexCount::default(), DEFAULT_SEGMENT_SIZE);
        Ok(Self {
            vertices: VertexStore::new(vertices)?,
            buckets: LabelBucketStore::new(
                buckets,
                bucket_free_spans,
                bucket_free_span_by_start,
                elem_capacity,
                DEFAULT_SEGMENT_SIZE,
            )?,
            edges: EdgeStore::new(
                edge_counts,
                edges,
                edge_log,
                edge_span_meta,
                edge_free_spans,
                edge_free_span_by_start,
                elem_capacity,
                DEFAULT_SEGMENT_SIZE,
                DEFAULT_SEGMENT_SIZE,
            )?,
            values: EdgeValueStore::new(
                value_slab,
                value_log,
                value_free_spans,
                value_free_span_by_start,
                elem_capacity,
                segment_count,
            )?,
            default_label,
            last_bucket_lookup: Cell::new(None),
            bucket_lookup_cache: std::array::from_fn(|_| Cell::new(None)),
            _marker: PhantomData,
        })
    }

    /// Opens a labeled graph from stable memories, creating stores when empty.
    ///
    /// See [`crate::lara::edge::EdgeStore::init`] for how `elem_capacity` is interpreted on reopen.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        vertices: M,
        buckets: M,
        bucket_free_spans: M,
        bucket_free_span_by_start: M,
        edge_counts: M,
        edges: M,
        edge_log: M,
        edge_span_meta: M,
        edge_free_spans: M,
        edge_free_span_by_start: M,
        value_slab: M,
        value_free_spans: M,
        value_free_span_by_start: M,
        value_log: M,
        elem_capacity: u64,
        default_label: BucketLabelKey,
    ) -> Result<Self, InitError> {
        let edges = EdgeStore::init(
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            elem_capacity,
            DEFAULT_SEGMENT_SIZE,
            DEFAULT_SEGMENT_SIZE,
        )
        .map_err(InitError::Edges)?;
        let edge_segment_count = edges.header().segment_count;
        Ok(Self {
            vertices: VertexStore::init(vertices).map_err(InitError::Vertices)?,
            buckets: LabelBucketStore::init(
                buckets,
                bucket_free_spans,
                bucket_free_span_by_start,
                elem_capacity,
                DEFAULT_SEGMENT_SIZE,
            )
            .map_err(InitError::Buckets)?,
            edges,
            values: EdgeValueStore::init(
                value_slab,
                value_log,
                value_free_spans,
                value_free_span_by_start,
                elem_capacity,
                edge_segment_count,
            )
            .map_err(InitError::Values)?,
            default_label,
            last_bucket_lookup: Cell::new(None),
            bucket_lookup_cache: std::array::from_fn(|_| Cell::new(None)),
            _marker: PhantomData,
        })
    }

    /// Returns the stable vertex column.
    pub fn vertices(&self) -> &VertexStore<LabeledVertex, M> {
        &self.vertices
    }

    /// Returns the LabelBucketStore (crate-internal; mutating slab slots without
    /// coordinating [`LabeledLaraGraph`] invariants corrupts the layout).
    pub(crate) fn buckets(&self) -> &LabelBucketStore<M> {
        &self.buckets
    }

    /// Returns the edge storage used by every label bucket.
    pub fn edges(&self) -> &EdgeStore<E, M> {
        &self.edges
    }

    /// Returns the edge-value byte slab for this orientation.
    pub fn values(&self) -> &EdgeValueStore<M> {
        &self.values
    }

    /// Returns the label used by default-label bypass rows.
    pub fn default_label(&self) -> BucketLabelKey {
        self.default_label
    }

    #[inline]
    fn is_homogeneous_bypass_label(&self, label_id: BucketLabelKey) -> bool {
        let raw = label_id.raw();
        let default = self.default_label.raw();
        raw == default || raw == (default & BUCKET_LABEL_INDEX_MASK)
    }

    /// Homogeneous bypass is only valid on the highest-index vertex row unless
    /// [`Self::ensure_bypass_edge_origin`] has already assigned a distinct origin.
    #[inline]
    fn may_use_homogeneous_bypass(&self, src: VertexId) -> bool {
        match u32::from(src).checked_add(1) {
            Some(next) => next >= self.vertices.len(),
            None => false,
        }
    }

    #[inline]
    fn bypass_storage_label_for(&self, vertex: &LabeledVertex) -> BucketLabelKey {
        vertex.bypass_storage_label(self.default_label)
    }

    /// Keeps bypass [`LabeledVertex::base_slot_start`] (edge-slot origin) non-decreasing
    /// when a bypass row grows under later vertices.
    ///
    /// Bucket-mode successors store [`LabelBucket`] indices in `base_slot_start`; they must
    /// not be rewritten by a predecessor's edge-region growth.
    fn bump_successor_origins_after_bypass_end(
        &self,
        src: VertexId,
        region_end: u64,
    ) -> Result<(), LabeledOperationError> {
        let first = u32::from(src)
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        for idx in first..self.vertices.len() as u32 {
            let vid = VertexId::from(idx);
            let successor = self.vertices.get(vid);
            if successor.is_default_edge_labeled() && successor.base_slot_start() < region_end {
                self.set_labeled_vertex(vid, successor.with_base_slot_start(region_end))?;
            }
        }
        Ok(())
    }

    /// Appends one edge on a homogeneous bypass row via [`EdgeStore::insert_edge`].
    ///
    /// Slab growth uses [`CsrVertex::slab_append_exclusive_end`] so bypass hubs can extend
    /// past the PMA `initial_vertex_edge_slots` window without spurious overflow-log spills.
    /// Homogeneous bypass appends normally stay on the slab; metadata log head (bits 4–11) is
    /// for other paths (for example after promoting a labeled bucket row with an active log).
    fn insert_homogeneous_bypass_edge(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        debug_assert_eq!(label_id, self.bypass_storage_label_for(&vertex));
        self.ensure_bypass_edge_origin(src)?;
        self.edges
            .insert_edge(&self.vertices, src, edge)
            .map_err(LabeledOperationError::from)?;
        let region_end = self.bypass_region_end(src)?;
        self.bump_successor_origins_after_bypass_end(src, region_end)
    }

    #[inline]
    fn bypass_region_end(&self, src: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        crate::labeled::slot_index::checked_add_slot_index(
            vertex.base_slot_start(),
            u64::from(vertex.stored_degree()),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Exclusive end of the global edge-slot prefix owned by `vid` (bypass or bucket span).
    fn vertex_prefix_end(&self, vid: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            crate::labeled::slot_index::checked_add_slot_index(
                vertex.base_slot_start(),
                u64::from(vertex.stored_degree()),
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
        } else if vertex.degree() == 0 {
            Ok(vertex.base_slot_start())
        } else {
            let first = self
                .buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            crate::labeled::slot_index::checked_add_slot_index(
                first.edge_start(),
                u64::from(vertex.stored_slots),
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
        }
    }

    /// Assigns a distinct `base_slot_start` for the first edge on a bypass row.
    ///
    /// Fresh rows default to `base_slot_start == 0`; chaining from the previous
    /// vertex's live edge prefix keeps global slots unique and non-decreasing in
    /// `VertexId` order (required by [`EdgeStore::have_space_on_slab`]).
    fn ensure_bypass_edge_origin(&self, src: VertexId) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if vertex.stored_degree() > 0 {
            return Ok(());
        }
        let edge_base = if u32::from(src) == 0 {
            0
        } else {
            let pred_idx = u32::from(src) - 1;
            self.vertex_prefix_end(VertexId::from(pred_idx))?
        };
        if edge_base != vertex.base_slot_start() {
            self.set_labeled_vertex(src, vertex.with_base_slot_start(edge_base))?;
        }
        Ok(())
    }

    fn insert_homogeneous_bypass(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_bypass_edge_origin(src)?;
        let vertex = self.vertices.get(src);
        self.set_labeled_vertex(src, vertex.with_homogeneous_bypass_label(label_id))?;
        self.insert_homogeneous_bypass_edge(src, label_id, edge)
    }

    /// Materializes a homogeneous bypass row into a single LabelBucket.
    fn promote_bypass_to_bucket_mode(&self, src: VertexId) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if !vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let bypass_label = self.bypass_storage_label_for(&vertex);
        let edge_start = vertex.base_slot_start();
        let stored_slots = vertex.stored_slots;
        let logical_degree = vertex.degree;
        if logical_degree == 0 {
            // Clearing default-label bypass must also reset locator fields so the row is a
            // coherent empty *normal* bucket row (`base_slot_start` is LabelBucket slab space).
            let cleared = vertex
                .with_default_edge_labeled(false)
                .with_bucket_row_and_slack(0, 0, 0)
                .with_stored_slots(0);
            self.set_labeled_vertex(src, cleared)?;
            return Ok(());
        }

        // Bucket collection must not read edge slots while bypass is still active.
        self.set_labeled_vertex(src, LabeledVertex::default())?;

        let new_alloc = DEFAULT_SEGMENT_SIZE.max(stored_slots);
        let (_, rewrote_bucket_segment) = self.buckets.insert_label_bucket_at(
            &self.vertices,
            src,
            LabelBucket::from_parts(bypass_label, edge_start, logical_degree, stored_slots, -1),
            0,
        )?;
        if rewrote_bucket_segment {
            self.invalidate_bucket_lookup_caches_for_bucket_segment(src)?;
        }
        let updated = self
            .vertices
            .get(src)
            .with_degree(1)
            .with_stored_slots(new_alloc);
        self.set_labeled_vertex(src, updated)?;
        self.edges
            .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))?;
        Ok(())
    }

    /// Returns the number of vertex rows.
    pub fn vertex_count(&self) -> VertexCount {
        VertexCount::from(self.vertices.len())
    }

    fn set_labeled_vertex(
        &self,
        vid: VertexId,
        vertex: LabeledVertex,
    ) -> Result<(), LabeledOperationError> {
        vertex.ensure_valid_normal_row()?;
        self.vertices.set(vid, &vertex);
        Ok(())
    }

    fn ensure_vertex(&self, vid: VertexId) -> Result<(), LabeledOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LabeledOperationError::VertexOutOfRange {
                vid,
                len: self.vertex_count(),
            });
        }
        Ok(())
    }

    #[inline]
    fn leaf_index_for_vid(vid: VertexId, segment_size: u32) -> u32 {
        u32::from(vid) / segment_size.max(1)
    }

    fn leaf_segment_counts_for_vid(&self, vid: VertexId) -> SegmentEdgeCounts {
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(vid, header.segment_size);
        let Some(idx) = leaf.checked_add(header.segment_count) else {
            return SegmentEdgeCounts {
                actual: 0,
                total: 0,
            };
        };
        self.edges.counts_store().get(u64::from(idx))
    }

    /// `true` when `vid`'s PMA leaf has `actual/total >= 1.0` under incremental labeled accounting.
    pub(crate) fn labeled_leaf_segment_is_dense(&self, vid: VertexId) -> bool {
        segment_span_density(self.leaf_segment_counts_for_vid(vid))
            >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY
    }

    /// `true` when any labeled vertex in PMA leaf `leaf` still has overflow-log buckets.
    fn leaf_any_vertex_has_overflow_log(
        &self,
        leaf: u32,
        seg_size: u32,
    ) -> Result<bool, LabeledOperationError> {
        let start_vid = leaf.saturating_mul(seg_size);
        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vid_u));
            if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
                continue;
            }
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Compacts then optionally grows slack for every normal labeled vertex in `src`'s PMA leaf.
    fn rebalance_cascade_after_labeled_mutation(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, header.segment_size);
        let idx_u32 = leaf
            .checked_add(header.segment_count)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let idx = u64::from(idx_u32);
        if segment_span_density(self.edges.counts_store().get(idx))
            < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
        {
            return Ok(());
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_rebalance_leaf_cascade");

        if self.leaf_any_vertex_has_overflow_log(leaf, seg)? {
            self.reclaim_edge_log_leaf_for_labeled(src)?;
            if segment_span_density(self.edges.counts_store().get(idx))
                < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
            {
                return Ok(());
            }
        }

        self.maintain_vertex_edge_span_light(src, true)?;
        if segment_span_density(self.edges.counts_store().get(idx))
            < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
        {
            return Ok(());
        }

        let start_vid = leaf
            .checked_mul(seg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let end_vid = start_vid
            .checked_add(seg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            .min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            if self.vertices.get(vid).degree() > 0 {
                self.maintain_vertex_edge_span_light(vid, false)?;
            }
        }

        if segment_span_density(self.edges.counts_store().get(idx))
            < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
        {
            return Ok(());
        }

        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            if self.vertices.get(vid).degree() > 0 {
                self.maintain_vertex_edge_span_light(vid, true)?;
            }
        }
        Ok(())
    }

    fn find_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<BucketSearch, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok(BucketSearch::Missing { insert_index: 0 });
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(BucketSearch::Missing { insert_index: 0 });
        }
        let start = vertex.base_slot_start();
        let range_end = start
            .checked_add(u64::from(deg))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        if let Some(cache) = self.last_bucket_lookup.get() {
            if cache.vid == src && cache.base_slot_start == start && cache.degree == deg {
                if cache.bucket_key == label_id && (start..range_end).contains(&cache.slot) {
                    let bucket = self
                        .buckets
                        .read_label_bucket_slot(cache.slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if bucket.bucket_label_key() == label_id {
                        return Ok(BucketSearch::Found {
                            slot: cache.slot,
                            bucket,
                        });
                    }
                }
                if let Some(slot_after_cache) = cache.slot.checked_add(1) {
                    if slot_after_cache == range_end && cache.bucket_key < label_id {
                        let bucket = self
                            .buckets
                            .read_label_bucket_slot(cache.slot)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        if bucket.bucket_label_key() == cache.bucket_key {
                            return Ok(BucketSearch::Missing { insert_index: deg });
                        }
                    }
                }
            }
        }
        let cache_index = Self::bucket_lookup_cache_index(src, label_id);
        if let Some(cache) = self.bucket_lookup_cache[cache_index].get() {
            if cache.vid == src
                && cache.bucket_key == label_id
                && cache.base_slot_start == start
                && cache.degree == deg
            {
                if (start..range_end).contains(&cache.slot) {
                    let bucket = self
                        .buckets
                        .read_label_bucket_slot(cache.slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if bucket.bucket_label_key() == label_id {
                        self.last_bucket_lookup.set(Some(cache));
                        return Ok(BucketSearch::Found {
                            slot: cache.slot,
                            bucket,
                        });
                    }
                }
            }
        }
        // Fast paths: avoid binary search + canbench scope overhead on tiny degree.
        if deg == 1 {
            let bucket = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            return Ok(match label_id.cmp(&bucket.bucket_label_key()) {
                Ordering::Less => BucketSearch::Missing { insert_index: 0 },
                Ordering::Equal => {
                    self.cache_bucket_lookup(src, label_id, vertex, start);
                    BucketSearch::Found {
                        slot: start,
                        bucket,
                    }
                }
                Ordering::Greater => BucketSearch::Missing { insert_index: 1 },
            });
        }
        if deg == 2 {
            let b0 = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            match label_id.cmp(&b0.bucket_label_key()) {
                Ordering::Less => return Ok(BucketSearch::Missing { insert_index: 0 }),
                Ordering::Equal => {
                    self.cache_bucket_lookup(src, label_id, vertex, start);
                    return Ok(BucketSearch::Found {
                        slot: start,
                        bucket: b0,
                    });
                }
                Ordering::Greater => {
                    let slot1 = start
                        .checked_add(1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let b1 = self
                        .buckets
                        .read_label_bucket_slot(slot1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    return Ok(match label_id.cmp(&b1.bucket_label_key()) {
                        Ordering::Less => BucketSearch::Missing { insert_index: 1 },
                        Ordering::Equal => {
                            self.cache_bucket_lookup(src, label_id, vertex, slot1);
                            BucketSearch::Found {
                                slot: slot1,
                                bucket: b1,
                            }
                        }
                        Ordering::Greater => BucketSearch::Missing { insert_index: 2 },
                    });
                }
            }
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_find_bucket_slot");

        if deg >= BULK_BUCKET_SEARCH_MIN_DEGREE {
            let buckets = self
                .buckets
                .read_label_bucket_slots_contiguous(start, deg)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            return Ok(
                match buckets.binary_search_by_key(&label_id, |bucket| bucket.bucket_label_key()) {
                    Ok(index) => {
                        let slot = start
                            .checked_add(index as u64)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        let bucket = buckets[index];
                        self.cache_bucket_lookup(src, label_id, vertex, slot);
                        BucketSearch::Found { slot, bucket }
                    }
                    Err(index) => BucketSearch::Missing {
                        insert_index: index as u32,
                    },
                },
            );
        }

        let mut lo = 0u32;
        let mut hi = deg;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let slot = start
                .checked_add(u64::from(mid))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key() == label_id {
                self.cache_bucket_lookup(src, label_id, vertex, slot);
                return Ok(BucketSearch::Found { slot, bucket });
            }
            if bucket.bucket_label_key() < label_id {
                lo = mid
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            } else {
                hi = mid;
            }
        }
        Ok(BucketSearch::Missing { insert_index: lo })
    }

    /// Clears lookup caches touched by descriptor relocations inside `vid`'s LabelBucket PMA segment.
    pub(crate) fn invalidate_bucket_lookup_caches_for_bucket_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let (start, end) = self.buckets.segment_vertex_bounds(&self.vertices, vid)?;
        if let Some(cache) = self.last_bucket_lookup.get() {
            let v_ord = u32::from(cache.vid);
            if (start..end).contains(&v_ord) {
                self.last_bucket_lookup.set(None);
            }
        }
        for cell in &self.bucket_lookup_cache {
            if let Some(cache) = cell.get() {
                let v_ord = u32::from(cache.vid);
                if (start..end).contains(&v_ord) {
                    cell.set(None);
                }
            }
        }
        Ok(())
    }

    /// Clears all label buckets for `vid`, then drops bucket lookup caches for its LabelBucket PMA segment.
    ///
    /// Callers must use this instead of [`LabelBucketStore::clear_vertex_label_buckets`] alone:
    /// segment rewrites relocate descriptor slabs for peer vertices in the same segment, which
    /// invalidates [`Self::find_bucket`] fast-path caches.
    pub(crate) fn clear_vertex_label_buckets_for_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        self.buckets
            .clear_vertex_label_buckets(&self.vertices, vid)?;
        self.invalidate_bucket_lookup_caches_for_bucket_segment(vid)?;
        Ok(())
    }

    fn invalidate_bucket_lookup_for_label(&self, src: VertexId, label_id: BucketLabelKey) {
        self.last_bucket_lookup.set(None);
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(None);
    }

    fn cache_bucket_lookup(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: &LabeledVertex,
        slot: u64,
    ) {
        let cache = BucketLookupCache {
            vid: src,
            bucket_key: label_id,
            base_slot_start: vertex.base_slot_start(),
            degree: vertex.degree(),
            slot,
        };
        self.last_bucket_lookup.set(Some(cache));
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(Some(cache));
    }

    fn bucket_lookup_cache_index(src: VertexId, label_id: BucketLabelKey) -> usize {
        let mixed = u32::from(src)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(u32::from(label_id.raw()));
        (mixed as usize) & (BUCKET_LOOKUP_CACHE_ENTRIES - 1)
    }

    /// Index of bucket descriptor slab slot `bucket_slot` in this vertex row.
    #[inline]
    fn labeled_bucket_descriptor_index(
        vertex: &LabeledVertex,
        bucket_slot: u64,
    ) -> Result<u32, LabeledOperationError> {
        let d = bucket_slot
            .checked_sub(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        u32::try_from(d).map_err(|_| LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Slab slot of the `bucket_index`-th descriptor in this vertex bucket row.
    #[inline]
    fn labeled_vertex_bucket_slot(
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        vertex
            .base_slot_start()
            .checked_add(u64::from(bucket_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    #[cfg(test)]
    fn find_bucket_slot(
        &self,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(
            match self.find_bucket(VertexId::from(0), vertex, label_id)? {
                BucketSearch::Found { slot, .. } => Some(slot),
                BucketSearch::Missing { .. } => None,
            },
        )
    }

    fn read_vertex_label_buckets(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if vertex.is_default_edge_labeled() {
            return Ok(Vec::new());
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(Vec::new());
        }
        let start = vertex.base_slot_start();
        Ok(self
            .buckets
            .read_label_bucket_slots_contiguous(start, deg)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?)
    }

    /// Reads `vertex`'s label buckets with global indices in `[lo, hi)` (half-open).
    fn read_vertex_label_buckets_range(
        &self,
        vertex: &LabeledVertex,
        lo: u32,
        hi: u32,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
        if lo >= hi {
            return Ok(Vec::new());
        }
        let deg = vertex.degree();
        if hi > deg {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
        let n = hi - lo;
        let start = vertex
            .base_slot_start()
            .checked_add(u64::from(lo))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.buckets
            .read_label_bucket_slots_contiguous(start, n)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    fn vertex_label_edge_span_end_exclusive(
        vertex: &LabeledVertex,
        first_bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        checked_add_slot_index(first_bucket.edge_start(), u64::from(vertex.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Like [`Self::try_contiguous_tiled_labeled_out_edges`], but for a contiguous fragment of one
    /// vertex's bucket row (for example the undirected or directed index range only).
    fn try_contiguous_tiled_labeled_out_edges_slice(
        buckets: &[LabelBucket],
        span_end_exclusive: u64,
    ) -> Option<(u64, u32)> {
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
        Some((base, total_edges))
    }

    fn vertex_label_buckets_have_overflow(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<bool, LabeledOperationError> {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        let buckets = self.read_vertex_label_buckets(vertex)?;
        Ok(buckets.iter().any(|b| b.overflow_log_head() >= 0))
    }

    fn bucket_successor_start(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        let cur_slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let cur = self
            .buckets
            .read_label_bucket_slot(cur_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.bucket_successor_start_after_bucket(vertex, bucket_index, &cur)
    }

    fn bucket_successor_start_after_bucket(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        if bucket_index + 1 < vertex.degree() {
            let next_ix = bucket_index
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let next_slot = Self::labeled_vertex_bucket_slot(vertex, next_ix)?;
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            // Proportional slack placement does not guarantee strictly increasing
            // `edge_start` across bucket slots; CSR slab-window geometry requires a
            // non-decreasing neighbor base, so clamp the successor boundary.
            return Ok(next.edge_start().max(bucket.edge_start()));
        }

        if vertex.degree() == 0 {
            return Ok(0);
        }

        let first_edge_start = if bucket_index == 0 {
            bucket.edge_start()
        } else {
            self.buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .edge_start()
        };
        crate::labeled::slot_index::checked_add_slot_index(
            first_edge_start,
            u64::from(vertex.stored_slots),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// True when every bucket is slab-only, has no deferred packing gaps
    /// (`edge_len == degree()`), and its slab window tiles contiguously with
    /// successors so bulk slab memcpy is sound.
    fn label_buckets_allow_contiguous_slab_copy(
        &self,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<bool, LabeledOperationError> {
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            return Ok(false);
        }
        if buckets.iter().any(|b| b.stored_slots != b.degree()) {
            return Ok(false);
        }
        for (index, bucket) in buckets.iter().enumerate() {
            let successor = match index.checked_add(1).and_then(|next_i| buckets.get(next_i)) {
                Some(next) => next.edge_start().max(bucket.edge_start()),
                None => {
                    checked_add_slot_index(buckets[0].edge_start(), u64::from(vertex.stored_slots))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?
                }
            };
            let span_width = successor.saturating_sub(bucket.edge_start());
            if span_width < u64::from(bucket.stored_slots) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// When every bucket is overflow-free and on-slab edges tile contiguously in the
    /// edge store (`bucket[i].edge_start + edge_len` meets `bucket[i+1].edge_start`),
    /// returns `(first_edge_slot, total_live_edges)` so one `read_slots_contiguous` can
    /// replace per-bucket slab walks.
    fn try_contiguous_tiled_labeled_out_edges(
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Option<(u64, u32)> {
        let deg = vertex.degree() as usize;
        if deg == 0 || buckets.len() != deg {
            return None;
        }
        if buckets.iter().any(|b| b.overflow_log_head() >= 0) {
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
        Some((base, total_edges))
    }

    /// Returns freed slab slots from a relocated [`LabeledVertex`] edge span.
    ///
    /// A single `release_span` can fail when earlier best-fit splits left interior holes
    /// inside what was one contiguous vertex allocation. Release the largest feasible
    /// prefix chunks instead so parallel multi-label rewrites can relocate safely.
    fn release_vertex_edge_span_slab(
        &self,
        base: u64,
        len: u64,
    ) -> Result<(), LabeledOperationError> {
        if len == 0 {
            return Ok(());
        }
        if self.edges.release_span(base, len).is_ok() {
            return Ok(());
        }
        if let Some(free_at_base) = self.edges.free_span_store().free_span_starting_at(base) {
            let skip = free_at_base.len.min(len);
            let new_base = base
                .checked_add(skip)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let new_len = len
                .checked_sub(skip)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            return self.release_vertex_edge_span_slab(new_base, new_len);
        }
        let mut lo = 1u64;
        let mut hi = len;
        let mut best = 0u64;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            if self.edges.release_span(base, mid).is_ok() {
                best = mid;
                lo = mid
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            } else if mid == 0 {
                break;
            } else {
                hi = mid
                    .checked_sub(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
        }
        if best > 0 {
            let tail_base = base
                .checked_add(best)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let tail_len = len
                .checked_sub(best)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.release_vertex_edge_span_slab(tail_base, tail_len)
        } else {
            let tail_base = base
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let tail_len = len
                .checked_sub(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.release_vertex_edge_span_slab(tail_base, tail_len)
        }
    }

    fn rewrite_vertex_edge_span_read_and_plan(
        &self,
        vertex: &LabeledVertex,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        compact: bool,
        force_slack_grow: bool,
    ) -> Result<(Vec<LabelBucket>, u32, u64, u32, u32, bool, Vec<u64>), LabeledOperationError> {
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_rewrite_read_and_plan");
        let buckets = self.read_vertex_label_buckets(vertex)?;
        let old_alloc = vertex.stored_slots;
        let old_base = buckets
            .first()
            .map(|bucket| bucket.edge_start())
            .unwrap_or(0);
        let mut total_live = 0u32;
        for bucket in &buckets {
            total_live = total_live
                .checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
        }

        let min_required = total_live
            .checked_add(preferred_extra)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let new_alloc = if compact {
            total_live
        } else if force_slack_grow && old_alloc >= min_required && old_alloc > 0 {
            let doubled_old_alloc = old_alloc
                .checked_mul(2)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let old_plus_segment = old_alloc
                .checked_add(DEFAULT_SEGMENT_SIZE)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            old_alloc
                .max(doubled_old_alloc)
                .max(old_plus_segment)
                .max(min_required)
        } else if old_alloc >= min_required && old_alloc > 0 {
            old_alloc
        } else {
            let doubled_old_alloc = old_alloc
                .checked_mul(2)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            min_required
                .max(DEFAULT_SEGMENT_SIZE)
                .max(doubled_old_alloc)
        };

        let moved = old_alloc == 0 || new_alloc > old_alloc || compact;
        let new_base = if new_alloc == 0 {
            0
        } else if moved {
            // Always append when relocating a VertexEdgeSpan. Reusing a best-fit free
            // span can overlap the live slab we are about to release and corrupt the
            // allocator (DuplicateStart on release).
            let start = self.edges.header().elem_capacity;
            let end =
                crate::slab_index::checked_add_slot_exclusive_end(start, u64::from(new_alloc))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .set_elem_capacity(end)
                .map_err(LabeledOperationError::from)?;
            start
        } else {
            old_base
        };

        let preferred = preferred_bucket.map(|index| index as usize);
        let positions = Self::calculate_label_edge_span_positions(
            new_base,
            new_alloc,
            buckets.as_slice(),
            preferred,
            preferred_extra,
        )?;
        Ok((
            buckets, old_alloc, old_base, total_live, new_alloc, moved, positions,
        ))
    }

    fn edge_bytes_for_len(edge_count: usize) -> Result<usize, LabeledOperationError> {
        edge_count
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    fn rewrite_vertex_edge_span(
        &self,
        src: VertexId,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        compact: bool,
        force_slack_grow: bool,
    ) -> Result<(), LabeledOperationError> {
        // Rebuild the VertexEdgeSpan from LabelEdgeSpan live prefixes.
        //
        // The bucket layer is exact-fit, so a new label can appear anywhere in
        // BucketLabelKey order without reserving space in the bucket descriptor. Edge
        // slack is instead distributed inside this VertexEdgeSpan. As in the
        // regular LARA PMA rebalance, row gaps are weighted by `degree + 1`, so a
        // high-degree label receives more spare room than a cold label. The
        // preferred bucket receives `preferred_extra` before proportional gap
        // placement because the caller is about to insert there.
        //
        // Copy strategy: when relocating to a fresh slab span (`moved` with a prior
        // allocation), source and destination are disjoint. If every bucket is
        // slab-only (`overflow_log_head < 0`) and each label's live edges sit in a
        // contiguous slab prefix, use cheap `read_slots_contiguous` / `write_slots_contiguous`.
        // Otherwise collect through `EdgeStore` (slab + overflow log) before writing.
        // In-place layout changes can overlap; the same fast vs full split applies.
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }

        let (buckets, old_alloc, old_base, total_live, new_alloc, moved, positions) = self
            .rewrite_vertex_edge_span_read_and_plan(
                &vertex,
                preferred_bucket,
                preferred_extra,
                compact,
                force_slack_grow,
            )?;

        let slab_only_bulk = self.label_buckets_allow_contiguous_slab_copy(&vertex, &buckets)?;

        let disjoint_copy = moved && old_alloc > 0;
        if disjoint_copy {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_rewrite_copy_disjoint");
            if slab_only_bulk {
                let max_run = buckets.iter().try_fold(0usize, |max_run, bucket| {
                    Ok::<usize, LabeledOperationError>(
                        max_run.max(Self::edge_bytes_for_len(bucket.degree() as usize)?),
                    )
                })?;
                let mut buf = vec![0u8; max_run];
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = Self::edge_bytes_for_len(bucket.degree() as usize)?;
                    if run > 0 {
                        self.edges
                            .read_slots_contiguous(bucket.edge_start(), &mut buf[..run]);
                        self.edges.write_slots_contiguous(row_start, &buf[..run])?;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, bucket.degree())
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            } else {
                let mut per_bucket: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
                for (index, _) in buckets.iter().enumerate() {
                    let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
                    let bucket_index = index as u32;
                    let successor = self.bucket_successor_start(&vertex, bucket_index)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    per_bucket.push(
                        self.edges
                            .asc_out_edges(&acc, VertexId::from(0))
                            .map_err(LabeledOperationError::from)?,
                    );
                }
                let max_run = per_bucket.iter().try_fold(0usize, |max_run, edges| {
                    Ok::<usize, LabeledOperationError>(
                        max_run.max(Self::edge_bytes_for_len(edges.len())?),
                    )
                })?;
                let mut buf = vec![0u8; max_run];
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let edges = &per_bucket[index];
                    let el = edges.len() as u32;
                    if !edges.is_empty() {
                        let run = Self::edge_bytes_for_len(edges.len())?;
                        debug_assert!(run <= buf.len());
                        let mut o = 0usize;
                        for e in edges {
                            e.write_to(&mut buf[o..o + E::BYTES]);
                            o += E::BYTES;
                        }
                        self.edges.write_slots_contiguous(row_start, &buf[..run])?;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, el)
                            .with_degree_field(el)
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            }
        } else if total_live > 0 {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_rewrite_copy_inplace_vec");
            if slab_only_bulk {
                let run_total = Self::edge_bytes_for_len(
                    usize::try_from(total_live)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                )?;
                let mut raw = vec![0u8; run_total];
                let mut off = 0usize;
                for bucket in &buckets {
                    let run = Self::edge_bytes_for_len(bucket.degree() as usize)?;
                    if run > 0 {
                        let end = off
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .read_slots_contiguous(bucket.edge_start(), &mut raw[off..end]);
                        off = end;
                    }
                }
                off = 0;
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = Self::edge_bytes_for_len(bucket.degree() as usize)?;
                    if run > 0 {
                        let end = off
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .write_slots_contiguous(row_start, &raw[off..end])?;
                        off = end;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, bucket.degree())
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            } else {
                let mut per_bucket: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
                for (index, _) in buckets.iter().enumerate() {
                    let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
                    let bucket_index = index as u32;
                    let successor = self.bucket_successor_start(&vertex, bucket_index)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    per_bucket.push(
                        self.edges
                            .asc_out_edges(&acc, VertexId::from(0))
                            .map_err(LabeledOperationError::from)?,
                    );
                }
                let run_total: usize = per_bucket.iter().try_fold(0usize, |total, edges| {
                    let run = Self::edge_bytes_for_len(edges.len())?;
                    total.checked_add(run).ok_or_else(|| {
                        LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow)
                    })
                })?;
                let mut raw = vec![0u8; run_total];
                let mut pack = 0usize;
                for edges in &per_bucket {
                    for e in edges {
                        e.write_to(&mut raw[pack..pack + E::BYTES]);
                        pack += E::BYTES;
                    }
                }
                pack = 0;
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let edges = &per_bucket[index];
                    let run = Self::edge_bytes_for_len(edges.len())?;
                    if run > 0 {
                        let end = pack
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .write_slots_contiguous(row_start, &raw[pack..end])?;
                        pack = end;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, edges.len() as u32)
                            .with_degree_field(edges.len() as u32)
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            }
        } else {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_rewrite_metadata_only");
            let mut row_buckets = Vec::with_capacity(buckets.len());
            for (index, bucket) in buckets.iter().enumerate() {
                let row_start = positions[index];
                row_buckets.push(
                    bucket
                        .with_edge_range(row_start, bucket.degree())
                        .with_overflow_log_head(-1),
                );
            }
            self.buckets
                .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_rewrite_finalize");
        let new_buckets = self.read_vertex_label_buckets(&vertex)?;
        if Self::vertex_value_spans_need_sync_after_rewrite(&buckets, moved, old_alloc, compact) {
            self.sync_vertex_value_spans_after_edge_rewrite(src, &buckets, &new_buckets)?;
        }
        if moved && old_alloc > 0 {
            self.release_vertex_edge_span_slab(old_base, u64::from(old_alloc))?;
        }
        self.vertices.set(src, &vertex.with_stored_slots(new_alloc));

        let d_total = i64::from(new_alloc) - i64::from(old_alloc);
        if d_total != 0 {
            self.edges
                .bump_vertex_segment_counts(src, 0, d_total)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(())
    }

    fn vertex_edge_span_slot_moves(
        &self,
        buckets: &[LabelBucket],
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let mut moves = Vec::new();
        for bucket in buckets {
            if bucket.degree() == 0 {
                continue;
            }
            if bucket.overflow_log_head() >= 0 {
                continue;
            }
            let mut next_live = 0u32;
            for old_slot_index in 0..bucket.stored_slots {
                let edge_slot =
                    checked_add_slot_index(bucket.edge_start(), u64::from(old_slot_index))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if self.edges.read_slot(edge_slot).is_deleted_slot() {
                    continue;
                }
                if old_slot_index != next_live {
                    moves.push(EdgeSlotMove {
                        label_id: bucket.bucket_label_key(),
                        old_slot_index,
                        new_slot_index: next_live,
                    });
                }
                next_live = next_live
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
            debug_assert_eq!(next_live, bucket.degree());
        }
        Ok(moves)
    }

    fn first_edge_slot_move_in_bucket(
        bucket: &LabelBucket,
        edges: &EdgeStore<E, M>,
    ) -> Result<Option<EdgeSlotMove>, LabeledOperationError> {
        if bucket.degree() == 0 || bucket.overflow_log_head() >= 0 {
            return Ok(None);
        }
        let mut next_live = 0u32;
        for old_slot_index in 0..bucket.stored_slots {
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(old_slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if edges.read_slot(edge_slot).is_deleted_slot() {
                continue;
            }
            if old_slot_index != next_live {
                return Ok(Some(EdgeSlotMove {
                    label_id: bucket.bucket_label_key(),
                    old_slot_index,
                    new_slot_index: next_live,
                }));
            }
            next_live = next_live
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(None)
    }

    fn apply_edge_slot_move_in_bucket(
        &self,
        bucket: &LabelBucket,
        moved: EdgeSlotMove,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let from = checked_add_slot_index(bucket.edge_start(), u64::from(moved.old_slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let to = checked_add_slot_index(bucket.edge_start(), u64::from(moved.new_slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let edge = self.edges.read_slot(from);
        self.edges
            .write_slot(to, edge)
            .map_err(LabeledOperationError::from)?;
        self.edges
            .write_slot(from, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        let width = bucket.value_width();
        if bucket.is_value_allocated() {
            let from_off = bucket
                .value_offset()
                .checked_add(u64::from(moved.old_slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let to_off = bucket
                .value_offset()
                .checked_add(u64::from(moved.new_slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut buf = vec![0u8; usize::from(width)];
            self.values.read_bytes(from_off, &mut buf);
            self.values
                .write_value_slot(to_off, width, &buf)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(())
    }

    fn finalize_bucket_slab_metadata(bucket: LabelBucket) -> LabelBucket {
        bucket
            .with_stored_slots(bucket.degree())
            .with_overflow_log_head(-1)
    }

    /// One incremental compaction step for a vertex edge span (one edge move, bucket advance, or finish).
    pub(crate) fn compact_vertex_edge_span_one_step(
        &self,
        vid: VertexId,
        resume_bucket_index: u32,
    ) -> Result<VertexEdgeSpanCompactOneStep, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(vid)?;
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(VertexEdgeSpanCompactOneStep::Finished);
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let mut total_live = 0u32;
        for bucket in &buckets {
            total_live = total_live
                .checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
        }
        if resume_bucket_index == 0 && buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            self.reclaim_vertex_overflow_buckets(vid)?;
            return self.compact_vertex_edge_span_one_step(vid, 0);
        }
        if resume_bucket_index >= vertex.degree() {
            // Per-bucket steps may already pack each label row (`stored_slots == degree`) while
            // the vertex-wide VertexEdgeSpan width (`vertex.stored_slots`) stays oversized.
            if vertex.stored_slots > total_live
                || buckets
                    .iter()
                    .any(|b| b.overflow_log_head() >= 0 || b.stored_slots != b.degree())
            {
                self.rewrite_vertex_edge_span(vid, None, 0, true, false)?;
            }
            return Ok(VertexEdgeSpanCompactOneStep::Finished);
        }
        let bucket_index = resume_bucket_index;
        let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if let Some(moved) = Self::first_edge_slot_move_in_bucket(&bucket, &self.edges)? {
            self.apply_edge_slot_move_in_bucket(&bucket, moved)?;
            return Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(moved));
        }
        let finalized = Self::finalize_bucket_slab_metadata(bucket);
        if finalized != bucket {
            self.buckets.write_label_bucket_slot(slot, finalized)?;
        }
        Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(
            bucket_index.saturating_add(1),
        ))
    }

    fn calculate_label_edge_span_positions(
        start_slot: u64,
        span_slots: u32,
        buckets: &[LabelBucket],
        preferred: Option<usize>,
        preferred_extra: u32,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        let mut out = Vec::with_capacity(buckets.len());
        if buckets.is_empty() {
            return Ok(out);
        }

        let mut effective_live = 0u64;
        let mut total_weight = buckets.len() as u64;
        for (index, bucket) in buckets.iter().enumerate() {
            let extra = (preferred == Some(index))
                .then_some(preferred_extra)
                .unwrap_or(0);
            let degree = u64::from(
                bucket
                    .degree()
                    .checked_add(extra)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?,
            );
            effective_live = effective_live
                .checked_add(degree)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            total_weight = total_weight
                .checked_add(degree)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        let gaps = u64::from(span_slots)
            .checked_sub(effective_live)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        // Same layout as the historical `f64` implementation: one fixed-point step
        // `floor((gaps/total_weight) * 1e8) / 1e8` per bucket weight `(deg+1)`.
        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                / tw
        };

        let mut cursor_fp = u128::from(start_slot)
            .checked_mul(P)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        for (index, bucket) in buckets.iter().enumerate() {
            let start = u64::try_from(cursor_fp / P)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            out.push(start);
            let extra = (preferred == Some(index))
                .then_some(preferred_extra)
                .unwrap_or(0);
            let deg = u128::from(
                bucket
                    .degree()
                    .checked_add(extra)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?,
            );
            let start_fp = u128::from(start)
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            cursor_fp = start_fp
                .checked_add(
                    deg.checked_mul(P)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                )
                .and_then(|cursor| {
                    let weight = deg.checked_add(1)?;
                    let gap = step_fp.checked_mul(weight)?;
                    cursor.checked_add(gap)
                })
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(out)
    }

    /// Appends a new vertex row.
    ///
    /// **Breaking change (v0.1 wire refresh):** returns [`LabeledOperationError`] instead of
    /// [`crate::GrowFailed`]. Invalid normal-mode rows (`degree` above [`MAX_VERTEX_LABEL_BUCKETS`])
    /// yield [`LabeledOperationError::InvalidVertexRow`].
    pub fn push_vertex(
        &self,
        mut vertex: LabeledVertex,
    ) -> Result<VertexId, LabeledOperationError> {
        vertex.ensure_valid_normal_row()?;
        let id = self.vertices.len();
        if id > 0 {
            let prev_end = self
                .vertex_bucket_descriptor_row_end(VertexId::from(id as u32 - 1))
                .map_err(LabeledOperationError::from)?;
            if vertex.base_slot_start() < prev_end {
                vertex = vertex.with_base_slot_start(prev_end);
            }
        }
        self.vertices
            .push(vertex)
            .map_err(LabeledOperationError::from)?;
        let header = self.edges.header();
        let target = segment_tree_leaf_count(self.vertices.len().into(), header.segment_size);
        if target > header.segment_count {
            self.edges
                .grow_segment_tree_to(target)
                .map_err(LabeledOperationError::from)?;
            self.values
                .grow_segment_count_to(target)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(VertexId::from(id))
    }

    /// Compacts the LabelBucketStore VertexSegment containing `vid`.
    pub fn compact_label_bucket_vertex_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(vid)?;
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_compact_label_bucket_vertex_segment");
        self.buckets
            .compact_vertex_segment_for_vertex(&self.vertices, vid)
            .map_err(LabeledOperationError::from)?;
        self.invalidate_bucket_lookup_caches_for_bucket_segment(vid)?;
        Ok(())
    }

    /// Compacts the VertexEdgeSpan that contains `bucket_index`.
    ///
    /// This rewrites all LabelEdgeSpans owned by `vid`, not only the selected
    /// one. `bucket_index` is used only to validate that the queued work still
    /// refers to an existing LabelEdgeSpan.
    pub fn compact_vertex_edge_span(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.compact_vertex_edge_span_with_moves(vid, bucket_index)
            .map(|_| ())
    }

    /// Compacts one VertexEdgeSpan and returns local slot rewrites caused by tombstone removal.
    pub fn compact_vertex_edge_span_with_moves(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let _ = bucket_index;
        let mut moves = Vec::new();
        let mut resume = 0u32;
        loop {
            match self.compact_vertex_edge_span_one_step(vid, resume)? {
                VertexEdgeSpanCompactOneStep::EdgeMoved(moved) => {
                    moves.push(moved);
                }
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => {
                    resume = next;
                }
                VertexEdgeSpanCompactOneStep::OverflowRewrite(batch) => {
                    moves.extend(batch);
                    break;
                }
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        Ok(moves)
    }

    /// Runs incremental edge-span compaction; grows slack only when still tight afterward.
    fn maintain_vertex_edge_span_light(
        &self,
        vid: VertexId,
        allow_slack_grow: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        self.compact_vertex_edge_span(vid, 0)?;
        if !allow_slack_grow {
            return Ok(());
        }
        let vertex = self.vertices.get(vid);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let total_live = buckets.iter().try_fold(0u32, |acc, bucket| {
            acc.checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)
        })?;
        if vertex.stored_slots > total_live.saturating_add(1) {
            self.rewrite_vertex_edge_span(vid, None, 0, false, true)?;
        }
        Ok(())
    }

    /// Exclusive end of `vid`'s LabelBucket descriptor row (not the edge prefix).
    fn vertex_bucket_descriptor_row_end(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            return self.vertex_prefix_end(vid);
        }
        let span = vertex
            .label_bucket_descriptor_span()
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        vertex
            .base_slot_start()
            .checked_add(u64::from(span))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Assigns a non-overlapping descriptor-row origin for the first label bucket on `src`.
    fn ensure_vertex_bucket_row_origin(&self, src: VertexId) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() > 0 {
            return Ok(());
        }
        let row_base = if u32::from(src) == 0 {
            0
        } else {
            let pred = VertexId::from(u32::from(src).saturating_sub(1));
            self.vertex_bucket_descriptor_row_end(pred)?
        };
        if vertex.base_slot_start() != row_base {
            self.set_labeled_vertex(src, vertex.with_base_slot_start(row_base))?;
        }
        Ok(())
    }

    /// Byte length of the live value span owned by `bucket` (0 when unallocated).
    fn bucket_resident_value_bytes(&self, bucket: &LabelBucket) -> u64 {
        if !bucket.is_value_allocated() {
            return 0;
        }
        u64::from(bucket.stored_slots.max(bucket.degree))
            .saturating_mul(u64::from(bucket.value_width()))
    }

    /// Syncs [`LabeledVertex::value_allocated_bytes`] with live per-bucket value spans.
    fn reconcile_vertex_value_allocated_bytes(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<(), LabeledOperationError> {
        let total: u64 = buckets
            .iter()
            .map(|b| self.bucket_resident_value_bytes(b))
            .try_fold(0u64, |acc, bytes| {
                acc.checked_add(bytes)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)
            })?;
        if vertex.value_allocated_bytes() == total {
            return Ok(());
        }
        let updated = vertex
            .try_with_value_allocated_bytes(total)
            .map_err(LabeledOperationError::from)?;
        self.vertices.set(src, &updated);
        Ok(())
    }

    fn value_log_leaf(&self, src: VertexId) -> u32 {
        u32::from(src) / self.edges.header().segment_size.max(1)
    }

    /// True when an edge-span rewrite must relocate or repack value bytes (not in-place only).
    fn vertex_value_spans_need_sync_after_rewrite(
        old_buckets: &[LabelBucket],
        moved: bool,
        old_alloc: u32,
        compact: bool,
    ) -> bool {
        if moved && old_alloc > 0 {
            return true;
        }
        if compact {
            return old_buckets.iter().any(|b| b.is_value_allocated());
        }
        old_buckets.iter().any(|b| {
            b.is_value_allocated() && (b.value_log_head() >= 0 || b.stored_slots != b.degree())
        })
    }

    /// Dense slab-only bucket: one read of `degree × width` bytes in slot order.
    fn read_bucket_values_slab_dense(&self, bucket: &LabelBucket) -> Option<Vec<Vec<u8>>> {
        if !bucket.is_value_allocated()
            || bucket.value_width() == 0
            || bucket.value_log_head() >= 0
            || bucket.stored_slots != bucket.degree()
        {
            return None;
        }
        let degree = bucket.degree() as usize;
        let width = usize::from(bucket.value_width());
        let nbytes = degree.checked_mul(width)?;
        let mut raw = vec![0u8; nbytes];
        self.values.read_bytes(bucket.value_offset(), &mut raw);
        Some(
            raw.chunks(width)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>(),
        )
    }

    /// Folds one label bucket's overflow log into its slab prefix (same [`value_offset`]).
    fn fold_label_bucket_to_slab(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.overflow_log_head() < 0 {
            return Ok(bucket);
        }
        let degree = bucket.degree();
        if degree == 0 {
            return Ok(bucket.with_overflow_log_head(-1));
        }
        let successor = self.bucket_successor_start(vertex, bucket_index)?;
        let slack = successor.saturating_sub(bucket.edge_start());
        if slack < u64::from(degree) {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let acc = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor, src);
        let edges = self
            .edges
            .asc_out_edges(&acc, VertexId::from(0))
            .map_err(LabeledOperationError::from)?;
        let run = Self::edge_bytes_for_len(edges.len())?;
        if run > 0 {
            let mut buf = vec![0u8; run];
            let mut offset = 0usize;
            for edge in &edges {
                edge.write_to(&mut buf[offset..offset + E::BYTES]);
                offset += E::BYTES;
            }
            self.edges
                .write_slots_contiguous(bucket.edge_start(), &buf[..run])?;
        }
        let had_value_log = bucket.value_log_head() >= 0;
        let saved = if bucket.is_value_allocated() && bucket.value_width() > 0 {
            if had_value_log {
                Some(self.collect_bucket_values_asc_order(src, vertex, bucket_index, &bucket)?)
            } else {
                self.read_bucket_values_slab_dense(&bucket)
            }
        } else {
            None
        };
        let mut bucket = bucket
            .with_overflow_log_head(-1)
            .with_stored_slots(degree)
            .with_degree_field(degree);
        if had_value_log {
            bucket = bucket
                .try_with_value_log_head(-1)
                .map_err(LabeledOperationError::from)?;
        }
        if let Some(saved) = saved {
            let width = bucket.value_width();
            let flat_len = saved
                .len()
                .checked_mul(usize::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut flat = Vec::with_capacity(flat_len);
            for bytes in &saved {
                flat.extend_from_slice(bytes);
            }
            self.values
                .write_bytes(bucket.value_offset(), &flat)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(bucket)
    }

    /// Folds one bucket's overflow log onto slab when possible; otherwise compacts the vertex row.
    fn ensure_label_bucket_folded_to_slab(
        &self,
        src: VertexId,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.overflow_log_head() < 0 {
            return Ok(bucket);
        }
        let vertex = self.vertices.get(src);
        match self.fold_label_bucket_to_slab(src, &vertex, bucket_index, bucket_slot, bucket) {
            Ok(folded) => {
                self.buckets.write_label_bucket_slot(bucket_slot, folded)?;
                Ok(folded)
            }
            Err(_) => {
                self.rewrite_vertex_edge_span(src, Some(bucket_index), 0, true, false)?;
                let vertex = self.vertices.get(src);
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                self.buckets
                    .read_label_bucket_slot(slot)
                    .ok_or(LaraOperationError::CollectAllocationOverflow.into())
            }
        }
    }

    /// Moves value-log bytes onto the bucket slab prefix; edges already live on slab.
    fn fold_label_bucket_value_log_to_slab(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.value_log_head() < 0 || !bucket.is_value_allocated() {
            return Ok(bucket);
        }
        let saved = self.collect_bucket_values_asc_order(src, vertex, bucket_index, &bucket)?;
        let bucket = bucket
            .try_with_value_log_head(-1)
            .map_err(LabeledOperationError::from)?;
        let width = bucket.value_width();
        let flat_len = saved
            .len()
            .checked_mul(usize::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if flat_len > 0 {
            let mut flat = Vec::with_capacity(flat_len);
            for bytes in &saved {
                flat.extend_from_slice(bytes);
            }
            self.values
                .write_bytes(bucket.value_offset(), &flat)
                .map_err(LabeledOperationError::from)?;
        }
        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        Ok(bucket)
    }

    /// Folds overflow/value logs on `vid`, or rewrites the row if edge slack is insufficient.
    fn reclaim_vertex_overflow_buckets(&self, vid: VertexId) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let mut needs_rewrite = false;
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            let bucket_index = bucket_index as u32;
            let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            if bucket.overflow_log_head() >= 0 {
                match self.fold_label_bucket_to_slab(vid, &vertex, bucket_index, slot, *bucket) {
                    Ok(folded) => {
                        self.buckets.write_label_bucket_slot(slot, folded)?;
                    }
                    Err(_) => {
                        needs_rewrite = true;
                        break;
                    }
                }
            } else if bucket.value_log_head() >= 0 {
                self.fold_label_bucket_value_log_to_slab(
                    vid,
                    &vertex,
                    bucket_index,
                    slot,
                    *bucket,
                )?;
            }
        }
        if needs_rewrite {
            self.rewrite_vertex_edge_span(vid, None, 0, true, false)?;
        }
        Ok(())
    }

    /// Clears a full PMA leaf overflow log by folding labeled buckets to slab, then resetting the segment.
    fn reclaim_edge_log_leaf_for_labeled(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let seg_size = self.edges.header().segment_size.max(1);
        let leaf = u32::from(src) / seg_size;
        let start_vid = leaf.saturating_mul(seg_size);
        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            self.reclaim_vertex_overflow_buckets(VertexId::from(vid_u))?;
        }
        self.edges
            .release_log_segment(SegmentId::from(leaf))
            .map_err(LabeledOperationError::from)?;
        self.values
            .release_value_log_segment(leaf)
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }

    /// Collects per-edge value bytes in the same ascending order as [`EdgeStore::asc_out_edges`].
    fn collect_bucket_values_asc_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        if !bucket.is_value_allocated() || bucket.value_width() == 0 {
            return Ok(Vec::new());
        }
        if let Some(dense) = self.read_bucket_values_slab_dense(bucket) {
            return Ok(dense);
        }
        let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let successor = self.bucket_successor_start(vertex, bucket_index)?;
        let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
        let edges = self
            .edges
            .asc_out_edges(&acc, VertexId::from(0))
            .map_err(LabeledOperationError::from)?;
        let log_chains =
            (bucket.overflow_log_head() >= 0).then(|| self.bucket_log_chains(src, bucket));
        let mut out = Vec::with_capacity(edges.len());
        for edge in edges {
            out.push(self.read_bucket_value_for_edge(src, bucket, &edge, log_chains.as_ref())?);
        }
        Ok(out)
    }

    fn bucket_log_chains(&self, src: VertexId, bucket: &LabelBucket) -> (Vec<u32>, Vec<u32>) {
        let leaf = self.value_log_leaf(src);
        (
            self.edges
                .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head()),
            self.values
                .value_log_chain_asc_indices(leaf, bucket.value_log_head()),
        )
    }

    fn read_bucket_value_for_edge(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        edge: &E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> Result<Vec<u8>, LabeledOperationError> {
        let width = bucket.value_width();
        if width == 0 {
            return Ok(Vec::new());
        }
        let slot_index = edge.edge_slot_index_raw();
        if bucket.value_log_head() < 0 && bucket.overflow_log_head() < 0 {
            let mut buf = vec![0u8; usize::from(width)];
            let offset = bucket
                .value_offset()
                .checked_add(u64::from(slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.values.read_bytes(offset, &mut buf);
            return Ok(buf);
        }

        let leaf = self.value_log_leaf(src);
        if let Some((edge_chain, value_chain)) = log_chains {
            if let Some(bytes) = Self::lookup_bucket_value_in_log_chains(
                &self.edges,
                &self.values,
                leaf,
                width,
                edge,
                edge_chain,
                value_chain,
            ) {
                return Ok(bytes);
            }
        } else {
            let (edge_chain, value_chain) = self.bucket_log_chains(src, bucket);
            if let Some(bytes) = Self::lookup_bucket_value_in_log_chains(
                &self.edges,
                &self.values,
                leaf,
                width,
                edge,
                &edge_chain,
                &value_chain,
            ) {
                return Ok(bytes);
            }
        }

        let mut buf = vec![0u8; usize::from(width)];
        let offset = bucket
            .value_offset()
            .checked_add(u64::from(slot_index) * u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.values.read_bytes(offset, &mut buf);
        Ok(buf)
    }

    fn lookup_bucket_value_in_log_chains(
        edges: &EdgeStore<E, M>,
        values: &EdgeValueStore<M>,
        leaf: u32,
        width: u8,
        edge: &E,
        edge_chain: &[u32],
        value_chain: &[u32],
    ) -> Option<Vec<u8>> {
        let slot_index = edge.edge_slot_index_raw();
        for (&entry_idx, &value_idx) in edge_chain.iter().zip(value_chain.iter()) {
            if entry_idx != slot_index {
                continue;
            }
            let logged = edges.decode_overflow_log_edge_at(leaf, entry_idx);
            if logged.neighbor_vid() == edge.neighbor_vid() {
                let mut buf = vec![0u8; usize::from(width)];
                values.read_value_log_entry(leaf, value_idx, width, &mut buf);
                return Some(buf);
            }
        }
        None
    }

    fn write_edge_value_to_log(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        entry_idx: i32,
        edge: &E,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let width = bucket.value_width();
        if width == 0 {
            return Ok(*bucket);
        }
        let src_i32 = i32::try_from(u32::from(src))
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        self.values
            .write_value_log_entry(
                self.value_log_leaf(src),
                u32::try_from(entry_idx)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                bucket.value_log_head(),
                src_i32,
                width,
                edge.edge_value_bytes(),
            )
            .map_err(LabeledOperationError::from)?;
        bucket
            .try_with_value_log_head(entry_idx)
            .map_err(LabeledOperationError::from)
    }

    fn release_bucket_value_span(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Result<(), LabeledOperationError> {
        let len = self.bucket_resident_value_bytes(bucket);
        if len == 0 {
            return Ok(());
        }
        self.values
            .retire_byte_span(bucket.value_offset(), len)
            .map_err(LabeledOperationError::from)?;
        let vertex = self.vertices.get(src);
        let new_alloc = vertex.value_allocated_bytes().saturating_sub(len);
        let updated = vertex
            .try_with_value_allocated_bytes(new_alloc)
            .map_err(LabeledOperationError::from)?;
        self.vertices.set(src, &updated);
        Ok(())
    }

    /// Reads per-edge value bytes in the same ascending order used when rewriting edge spans.
    fn read_bucket_values_in_edge_slot_order(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: &LabelBucket,
    ) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        self.collect_bucket_values_asc_order(src, vertex, bucket_index, bucket)
    }

    fn ensure_bucket_value_width_on_slot(
        &self,
        _src: VertexId,
        _bucket_slot: u64,
        bucket: LabelBucket,
        value_width_code: ValueWidthCode,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.value_width_code() == value_width_code {
            return Ok(bucket);
        }
        if bucket.is_value_allocated() && value_width_code != ValueWidthCode::Zero {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        Ok(bucket.with_value_width_code(value_width_code))
    }

    /// Ensures the label bucket at `src` declares `value_width_code` before valued inserts.
    pub fn ensure_label_bucket_value_width(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        value_width_code: ValueWidthCode,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let (bucket_slot, bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let bucket =
            self.ensure_bucket_value_width_on_slot(src, bucket_slot, bucket, value_width_code)?;
        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        Ok(())
    }

    fn ensure_bucket_value_span(
        &self,
        src: VertexId,
        bucket_slot: u64,
        mut bucket: LabelBucket,
        prev_stored_slots: u32,
    ) -> Result<LabelBucket, LabeledOperationError> {
        let width = bucket.value_width();
        let needed_slots = bucket.stored_slots.max(bucket.degree);
        if width == 0 || needed_slots == 0 {
            return Ok(bucket);
        }
        let needed_bytes = u64::from(needed_slots)
            .checked_mul(u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let had_bytes = u64::from(prev_stored_slots)
            .checked_mul(u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let tail = self.values.header().slab_occupied_tail;
        let old_offset = bucket.value_offset();
        let span_ends_at_tail = old_offset
            .checked_add(had_bytes)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            == tail;
        if needed_bytes <= had_bytes && span_ends_at_tail {
            return Ok(bucket);
        }
        let extra = needed_bytes.saturating_sub(had_bytes);
        let alloc_delta;

        if had_bytes == 0 {
            // First span for this bucket: bump the occupied tail when the slab already
            // has bytes so we do not place a second bucket at offset 0.
            let offset = if tail == 0 {
                self.values
                    .allocate_byte_span(needed_bytes)
                    .map_err(LabeledOperationError::from)?
            } else {
                self.values
                    .append_byte_span(needed_bytes)
                    .map_err(LabeledOperationError::from)?
            };
            bucket = bucket
                .with_value_offset(offset)
                .try_with_value_log_head(-1)
                .map_err(LabeledOperationError::from)?;
            alloc_delta = needed_bytes;
        } else if span_ends_at_tail
            && self
                .values
                .grow_byte_span_in_place(old_offset, had_bytes, needed_bytes)
                .map_err(LabeledOperationError::from)?
        {
            alloc_delta = extra;
        } else {
            let mut old_buf = vec![
                0u8;
                usize::try_from(had_bytes).map_err(|_| {
                    LaraOperationError::CollectAllocationOverflow
                })?
            ];
            self.values.read_bytes(old_offset, &mut old_buf);
            let new_offset = self
                .values
                .append_byte_span(needed_bytes)
                .map_err(LabeledOperationError::from)?;
            self.values
                .write_bytes(new_offset, &old_buf)
                .map_err(LabeledOperationError::from)?;
            if extra > 0 {
                let pad = vec![
                    0u8;
                    usize::try_from(extra)
                        .map_err(|_| { LaraOperationError::CollectAllocationOverflow })?
                ];
                self.values
                    .write_bytes(
                        new_offset
                            .checked_add(had_bytes)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                        &pad,
                    )
                    .map_err(LabeledOperationError::from)?;
            }
            if new_offset != old_offset {
                self.values
                    .retire_byte_span(old_offset, had_bytes)
                    .map_err(LabeledOperationError::from)?;
            }
            bucket = bucket.with_value_offset(new_offset);
            alloc_delta = extra;
            debug_assert_eq!(bucket.value_offset(), new_offset);
        }

        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;

        if alloc_delta > 0 {
            let vertex = self.vertices.get(src);
            let new_alloc = vertex
                .value_allocated_bytes()
                .checked_add(alloc_delta)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let updated = vertex
                .try_with_value_allocated_bytes(new_alloc)
                .map_err(LabeledOperationError::from)?;
            self.vertices.set(src, &updated);
        }
        if bucket.is_value_allocated() {
            let vertex = self.vertices.get(src);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            self.reconcile_vertex_value_allocated_bytes(src, &vertex, &buckets)?;
        }
        Ok(bucket)
    }

    fn write_edge_value_at_slot(
        &self,
        bucket: &LabelBucket,
        slot_index: u32,
        edge: &E,
    ) -> Result<(), LabeledOperationError> {
        let width = bucket.value_width();
        if width == 0 {
            return Ok(());
        }
        let value_width = edge.edge_value_byte_width();
        if value_width == 0 {
            return Ok(());
        }
        if value_width != width {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let offset = bucket
            .value_offset()
            .checked_add(u64::from(slot_index) * u64::from(width))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.values
            .write_value_slot(offset, width, edge.edge_value_bytes())
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }

    fn attach_edge_value(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket: LabelBucket,
        slot_index: u32,
        edge: E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> E {
        if !bucket.is_value_allocated() {
            return edge;
        }
        let width = bucket.value_width();
        let edge = edge.with_slot_index(slot_index);
        let buf = self
            .read_bucket_value_for_edge(src, &bucket, &edge, log_chains)
            .unwrap_or_else(|_| vec![0u8; usize::from(width)]);
        edge.with_stored_value_bytes(width, &buf)
    }

    fn bucket_value_log_chains_opt(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Option<(Vec<u32>, Vec<u32>)> {
        if bucket.is_value_allocated()
            && (bucket.overflow_log_head() >= 0 || bucket.value_log_head() >= 0)
        {
            Some(self.bucket_log_chains(src, bucket))
        } else {
            None
        }
    }

    fn labeled_edge_with_value(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket: LabelBucket,
        slot_index: u32,
        edge: E,
        log_chains: Option<&(Vec<u32>, Vec<u32>)>,
    ) -> E {
        self.attach_edge_value(
            src,
            vertex,
            bucket_index,
            bucket,
            slot_index,
            edge,
            log_chains,
        )
    }

    fn sync_vertex_value_spans_after_edge_rewrite(
        &self,
        src: VertexId,
        old_buckets: &[LabelBucket],
        new_buckets: &[LabelBucket],
    ) -> Result<(), LabeledOperationError> {
        if old_buckets.len() != new_buckets.len() {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let vertex = self.vertices.get(src);
        let mut saved: Vec<Vec<Vec<u8>>> = Vec::with_capacity(old_buckets.len());
        for (index, old) in old_buckets.iter().enumerate() {
            let values = match self.read_bucket_values_slab_dense(old) {
                Some(v) => v,
                None => {
                    self.read_bucket_values_in_edge_slot_order(src, &vertex, index as u32, old)?
                }
            };
            saved.push(values);
            self.release_bucket_value_span(src, old)?;
        }
        for (index, new_bucket) in new_buckets.iter().enumerate() {
            if new_bucket.value_width() == 0 {
                continue;
            }
            let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
            let mut bucket = new_bucket.with_value_offset(0);
            bucket = self.ensure_bucket_value_span(src, slot, bucket, 0)?;
            let width = bucket.value_width();
            let flat_len = saved[index]
                .len()
                .checked_mul(usize::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if flat_len > 0 {
                let mut flat = Vec::with_capacity(flat_len);
                for bytes in &saved[index] {
                    flat.extend_from_slice(bytes);
                }
                self.values
                    .write_bytes(bucket.value_offset(), &flat)
                    .map_err(LabeledOperationError::from)?;
            }
            self.buckets.write_label_bucket_slot(slot, bucket)?;
        }
        Ok(())
    }

    /// When peers in this vertex row already carry values, grow edge slack before insert.
    fn ensure_bucket_slack_insert_when_peers_have_values(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(src);
        if vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let has_live_value_span = buckets
            .iter()
            .any(|b| b.is_value_allocated() && self.bucket_resident_value_bytes(b) > 0);
        if has_live_value_span {
            return self.reconcile_vertex_value_allocated_bytes(src, &vertex, &buckets);
        }
        if vertex.value_allocated_bytes() > 0 {
            return Ok(());
        }
        if buckets.iter().any(|b| b.is_value_allocated()) {
            if self.vertex_label_buckets_have_overflow(&vertex)? {
                self.reclaim_vertex_overflow_buckets(src)?;
            }
            self.compact_vertex_edge_span(src, 0)?;
            let vertex = self.vertices.get(src);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let total_live = buckets.iter().try_fold(0u32, |acc, bucket| {
                acc.checked_add(bucket.degree())
                    .ok_or(LaraOperationError::RowDegreeOverflow)
            })?;
            if vertex.stored_slots.saturating_sub(total_live) < 2 {
                self.rewrite_vertex_edge_span(src, None, 1, false, true)?;
            }
        }
        Ok(())
    }

    /// Inserts one edge under `label_id` at `src`.
    ///
    /// After a successful normal labeled insert, runs an immediate **cascade**
    /// pass only when the owning PMA leaf is already dense (`actual/total ≥ 1`);
    /// sparse leaves skip the leaf-wide compaction / slack-grow scan.
    pub fn insert_edge(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.insert_edge_skip_leaf_cascade(src, label_id, edge)?;
        if self.labeled_leaf_segment_is_dense(src) {
            self.rebalance_cascade_after_labeled_mutation(src)?;
        }
        Ok(())
    }

    /// Like [`Self::insert_edge`], but skips the post-insert leaf cascade (used
    /// by deferred wrappers that enqueue maintenance instead).
    pub(crate) fn insert_edge_skip_leaf_cascade(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let mut vertex = self.vertices.get(src);
        let has_edge_value = edge.edge_value_byte_width() != 0;
        if vertex.is_default_edge_labeled() {
            if !has_edge_value && label_id == self.bypass_storage_label_for(&vertex) {
                return self.insert_homogeneous_bypass_edge(src, label_id, edge);
            }
            self.promote_bypass_to_bucket_mode(src)?;
            vertex = self.vertices.get(src);
        } else if vertex.degree() == 0
            && self.is_homogeneous_bypass_label(label_id)
            && self.may_use_homogeneous_bypass(src)
            && !has_edge_value
        {
            return self.insert_homogeneous_bypass(src, label_id, edge);
        }

        let (bucket_slot, mut bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let vertex = self.vertices.get(src);
        if let Some(code) = ValueWidthCode::from_byte_width(edge.edge_value_byte_width()) {
            if code != ValueWidthCode::Zero && code != bucket.value_width_code() {
                bucket = self.ensure_bucket_value_width_on_slot(src, bucket_slot, bucket, code)?;
                self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
            }
        }
        self.ensure_bucket_slack_insert_when_peers_have_values(src, &vertex)?;
        let vertex = self.vertices.get(src);
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, bucket_slot)?;
        for _attempt in 0..64u32 {
            let vertex = self.vertices.get(src);
            let successor_start =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
            let slack_span = successor_start.saturating_sub(bucket.edge_start());
            if bucket.overflow_log_head() < 0 && slack_span > u64::from(bucket.stored_slots) {
                let write_slot =
                    checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                debug_assert!(write_slot < successor_start);
                self.edges.write_slot(write_slot, edge)?;
                let prev_stored_slots = bucket.stored_slots;
                let bucket = bucket.grow_packed_slab_by_one();
                let bucket =
                    self.ensure_bucket_value_span(src, bucket_slot, bucket, prev_stored_slots)?;
                let slot_index = bucket.stored_slots.saturating_sub(1);
                self.write_edge_value_at_slot(&bucket, slot_index, &edge)?;
                self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                let hdr = self.edges.header();
                let next_num_edges = hdr
                    .num_edges
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.edges.set_num_edges(next_num_edges);
                self.edges
                    .bump_vertex_segment_counts(src, 1, 0)
                    .map_err(LabeledOperationError::from)?;
                self.invalidate_bucket_lookup_for_label(src, label_id);
                return Ok(());
            }
            let access = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor_start, src);
            match self.edges.insert_edge(&access, VertexId::from(0), edge) {
                Ok(InsertLocation::Slab(written_slot)) => {
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let prev_stored_slots = bucket.stored_slots;
                    let new_stored = written_slot.saturating_add(1).max(bucket.stored_slots);
                    if new_stored != bucket.stored_slots {
                        bucket = bucket.with_stored_slots(new_stored);
                    }
                    let bucket =
                        self.ensure_bucket_value_span(src, bucket_slot, bucket, prev_stored_slots)?;
                    self.write_edge_value_at_slot(&bucket, written_slot, &edge)?;
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                    self.invalidate_bucket_lookup_for_label(src, label_id);
                    return Ok(());
                }
                Ok(InsertLocation::Log) => {
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let prev_stored_slots = bucket.stored_slots;
                    let mut bucket =
                        self.ensure_bucket_value_span(src, bucket_slot, bucket, prev_stored_slots)?;
                    let entry_idx = bucket.overflow_log_head();
                    if bucket.is_value_allocated() && entry_idx >= 0 {
                        bucket = self.write_edge_value_to_log(src, &bucket, entry_idx, &edge)?;
                    }
                    self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
                    self.invalidate_bucket_lookup_for_label(src, label_id);
                    return Ok(());
                }
                Err(LaraOperationError::SegmentLogFull) => {
                    let vertex = self.vertices.get(src);
                    if vertex.is_default_edge_labeled()
                        && !has_edge_value
                        && label_id == self.bypass_storage_label_for(&vertex)
                    {
                        return self.insert_homogeneous_bypass_edge(src, label_id, edge);
                    }
                    self.reclaim_edge_log_leaf_for_labeled(src)?;
                    let vertex = self.vertices.get(src);
                    let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                    bucket = self
                        .buckets
                        .read_label_bucket_slot(bucket_slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                }
                Err(e) => return Err(LabeledOperationError::from(e)),
            }
        }
        Err(LabeledOperationError::from(
            LaraOperationError::SegmentLogFull,
        ))
    }

    fn find_or_create_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: BucketLabelKey,
    ) -> Result<(u64, LabelBucket), LabeledOperationError> {
        let insert_index = match self.find_bucket(src, vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => return Ok((slot, bucket)),
            BucketSearch::Missing { insert_index } => insert_index,
        };
        if insert_index > 0 && self.vertex_label_buckets_have_overflow(vertex)? {
            self.reclaim_vertex_overflow_buckets(src)?;
        }
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_insert_new_label_bucket");
        let (slot, rewrote_bucket_segment) = self
            .buckets
            .insert_label_bucket_at(
                &self.vertices,
                src,
                LabelBucket::default().with_bucket_label_key(label_id),
                insert_index,
            )
            .map_err(LabeledOperationError::from)?;
        if rewrote_bucket_segment {
            self.invalidate_bucket_lookup_caches_for_bucket_segment(src)?;
        }
        self.ensure_vertex_bucket_row_origin(src)?;
        let vertex = self.vertices.get(src);
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        if !self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
            let vertex = self.vertices.get(src);
            if self.vertex_label_buckets_have_overflow(&vertex)? {
                self.reclaim_vertex_overflow_buckets(src)?;
            }
            let vertex = self.vertices.get(src);
            if !self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
                self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, false)?;
            }
        }
        let vertex = self.vertices.get(src);
        let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        let bucket = self
            .buckets
            .read_label_bucket_slot(bucket_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.cache_bucket_lookup(src, label_id, &vertex, bucket_slot);
        Ok((bucket_slot, bucket))
    }

    fn try_place_new_bucket_edge_span(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        slot: u64,
        bucket_index: u32,
    ) -> Result<bool, LabeledOperationError> {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        if vertex.degree() == 1 {
            let new_alloc = DEFAULT_SEGMENT_SIZE;
            let edge_start = self.edges.allocate_span(u64::from(new_alloc))?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .with_edge_range(edge_start, 0)
                .with_overflow_log_head(-1);
            self.buckets.write_label_bucket_slot(slot, bucket)?;
            self.vertices.set(src, &vertex.with_stored_slots(new_alloc));
            self.edges
                .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))
                .map_err(LabeledOperationError::from)?;
            return Ok(true);
        }

        if bucket_index + 1 != vertex.degree() {
            return Ok(false);
        }
        let prev_slot = slot
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let prev = self
            .buckets
            .read_label_bucket_slot(prev_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if prev.overflow_log_head() >= 0 {
            return Ok(false);
        }
        if prev.stored_slots > DEFAULT_SEGMENT_SIZE {
            return Ok(false);
        }
        let first = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_end = checked_add_slot_index(first.edge_start(), u64::from(vertex.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let edge_start = checked_add_slot_index(prev.edge_start(), u64::from(prev.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let gap = span_end.saturating_sub(edge_start);
        if gap == 0 {
            return Ok(false);
        }
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?
            .with_edge_range(edge_start, 0)
            .with_overflow_log_head(-1);
        self.buckets.write_label_bucket_slot(slot, bucket)?;
        Ok(true)
    }

    /// Enables default-label bypass for `src` when it has exactly one default label.
    pub fn enable_default_edge_bypass(&self, src: VertexId) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        if vertex.degree() > 1 {
            return Err(LabeledOperationError::InvalidDefaultBypass);
        }
        if vertex.degree() == 1 {
            let mut bucket = self
                .buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.overflow_log_head() >= 0 {
                bucket = self.ensure_label_bucket_folded_to_slab(
                    src,
                    0,
                    vertex.base_slot_start(),
                    bucket,
                )?;
            }
            let old_alloc = vertex.stored_slots;
            let updated = vertex
                .with_default_edge_labeled(true)
                .with_bypass_undirected(bucket.bucket_label_key().is_undirected())
                .with_base_slot_start(bucket.edge_start())
                .with_degree(bucket.degree)
                .with_stored_slots(bucket.stored_slots);
            self.clear_vertex_label_buckets_for_segment(src)?;
            self.set_labeled_vertex(src, updated)?;
            self.edges
                .bump_vertex_segment_counts(src, 0, -i64::from(old_alloc))?;
        } else {
            self.set_labeled_vertex(
                src,
                vertex.with_homogeneous_bypass_label(self.default_label),
            )?;
        }
        Ok(())
    }

    /// Visits outgoing edges for `label_id` without materializing the full bucket row.
    pub fn for_each_edges_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        let mut visit = visit;
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            return self
                .edges
                .visit_out_edges(
                    &self.vertices,
                    src,
                    None,
                    None,
                    None::<&mut dyn FnMut(&[u8]) -> bool>,
                    |_| true,
                    |edge| visit(edge.with_label_id(label_id.raw())),
                )
                .map_err(Into::into);
        }
        for edge in
            self.out_edges_iter_for_label_ordered(src, label_id, OutEdgeOrder::Descending)?
        {
            visit(edge.with_label_id(label_id.raw()));
        }
        Ok(())
    }

    /// Visits outgoing edges for `label_id` in `order` without materializing the full bucket row.
    pub fn for_each_edges_for_label_ordered<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        for edge in self.out_edges_iter_for_label_ordered(src, label_id, order)? {
            visit(edge.with_label_id(label_id.raw()));
        }
        Ok(())
    }

    /// Iterator over one label's outgoing span (bypass row or one bucket), without materializing the
    /// full multi-label row.
    fn out_edges_iter_for_label_ordered(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        order: OutEdgeOrder,
    ) -> Result<LabeledSpanIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(LabeledSpanIter::Empty);
            }
            return match order {
                OutEdgeOrder::Descending => Ok(LabeledSpanIter::Desc {
                    graph: self,
                    src,
                    vertex,
                    bucket_index: 0,
                    bucket: LabelBucket::default(),
                    label_id,
                    log_chains: None,
                    iter: self.edges.out_edges_iter(&self.vertices, src)?,
                }),
                OutEdgeOrder::Ascending => Ok(LabeledSpanIter::Asc {
                    graph: self,
                    src,
                    vertex,
                    bucket_index: 0,
                    bucket: LabelBucket::default(),
                    label_id,
                    log_chains: None,
                    iter: self.edges.asc_out_edges_iter(&self.vertices, src)?,
                }),
            };
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
                self.labeled_bucket_span_iter(src, order, &vertex, &[bucket], 0, bucket_index)
            }
            BucketSearch::Missing { .. } => Ok(LabeledSpanIter::Empty),
        }
    }

    /// Descending-scan iterator over one label's outgoing span (bypass row or one bucket), without
    /// materializing the full multi-label row.
    pub(crate) fn out_edges_iter_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<OutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(OutEdgesIter::empty(&self.edges));
            }
            return self
                .edges
                .out_edges_iter(&self.vertices, src)
                .map_err(LabeledOperationError::Store);
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
                self.edges
                    .out_edges_iter(
                        &LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src),
                        VertexId::from(0),
                    )
                    .map_err(LabeledOperationError::Store)
            }
            BucketSearch::Missing { .. } => Ok(OutEdgesIter::empty(&self.edges)),
        }
    }

    /// Applies `advance_by` for `*offset_remaining`, then visits subsequent edges in descending CSR
    /// order until `visit` returns `true` (stop) or the iterator ends.
    ///
    /// On success, `*offset_remaining` is set to `0` when the full skip is applied inside this label
    /// span. If the span ends before the skip completes, `*offset_remaining` is set to the
    /// shortfall (same contract as [`Iterator::advance_by`] error value).
    pub(crate) fn skip_then_visit_each_out_edge_for_label<Visit, Err>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        offset_remaining: &mut usize,
        mut visit: Visit,
    ) -> Result<Result<bool, Err>, LabeledOperationError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        let skip = *offset_remaining;
        let mut it =
            self.out_edges_iter_for_label_ordered(src, label_id, OutEdgeOrder::Descending)?;
        match Iterator::advance_by(&mut it, skip) {
            Ok(()) => {
                *offset_remaining = 0;
            }
            Err(nz) => {
                *offset_remaining = nz.get();
                return Ok(Ok(false));
            }
        }
        loop {
            let Some(edge) = it.next() else {
                return Ok(Ok(false));
            };
            match visit(edge) {
                Ok(false) => continue,
                Ok(true) => return Ok(Ok(true)),
                Err(e) => return Ok(Err(e)),
            }
        }
    }

    /// Applies `advance_by` for `*offset_remaining`, then visits outgoing edges whose
    /// [`BucketLabelKey`] matches `directedness` in descending scan order.
    pub(crate) fn skip_then_visit_each_out_edge_by_directedness<Visit, Err>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        offset_remaining: &mut usize,
        mut visit: Visit,
    ) -> Result<Result<bool, Err>, LabeledOperationError>
    where
        Visit: FnMut(E) -> Result<bool, Err>,
    {
        let skip = *offset_remaining;
        let mut it =
            self.out_edges_by_directedness_iter(src, directedness, OutEdgeOrder::Descending)?;
        match Iterator::advance_by(&mut it, skip) {
            Ok(()) => {
                *offset_remaining = 0;
            }
            Err(nz) => {
                *offset_remaining = nz.get();
                return Ok(Ok(false));
            }
        }
        loop {
            let Some(edge) = it.next() else {
                return Ok(Ok(false));
            };
            match visit(edge) {
                Ok(false) => continue,
                Ok(true) => return Ok(Ok(true)),
                Err(e) => return Ok(Err(e)),
            }
        }
    }

    /// Like [`Self::for_each_edges_for_label`], but skips [`Self::ensure_vertex`].
    ///
    /// Caller must guarantee `src` is in range: `u32::from(src) < self.vertices.len()`. Correct
    /// shortest-path / BFS traversals satisfy this when `src` is only taken from graph neighbors.
    pub fn for_each_edges_for_label_unchecked<Visit>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        debug_assert!(u32::from(src) < self.vertices.len());
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_unchecked_bypass_slab");
            return self
                .edges
                .visit_out_edges(
                    &self.vertices,
                    src,
                    None,
                    None,
                    None::<&mut dyn FnMut(&[u8]) -> bool>,
                    |_| true,
                    |edge| visit(edge.with_label_id(label_id.raw())),
                )
                .map_err(Into::into);
        }
        for edge in
            self.out_edges_iter_for_label_ordered(src, label_id, OutEdgeOrder::Descending)?
        {
            visit(edge.with_label_id(label_id.raw()));
        }
        Ok(())
    }

    fn visit_label_out_edges_inner<Match, Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        ascending: bool,
        offset: Option<usize>,
        limit: Option<usize>,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: &mut Match,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        let mut window = OutEdgeVisitWindow::new(offset, limit);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(());
            }
            let label = self.bypass_storage_label_for(vertex).raw();
            if !ascending {
                let mut it = OutEdgeSlabIter::try_new(
                    &self.edges,
                    vertex.base_slot_start(),
                    vertex.stored_degree(),
                    vertex.degree(),
                )?;
                let has_raw = raw_matches.is_some();
                while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                    let edge = edge.with_label_id(label);
                    if has_raw {
                        if matches(&edge) && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    } else if matches(&edge) && !window.emit_edge(edge, visit) {
                        return Ok(());
                    }
                }
                return Ok(());
            }
            for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                let edge = edge.with_label_id(label);
                let passes = if let Some(raw_m) = raw_matches.as_mut() {
                    let mut buf = vec![0u8; E::BYTES];
                    edge.write_to(&mut buf);
                    raw_m(&buf) && matches(&edge)
                } else {
                    matches(&edge)
                };
                if passes && !window.emit_edge(edge, visit) {
                    return Ok(());
                }
            }
            return Ok(());
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets)
        {
            if total_edges == 0 {
                return Ok(());
            }
            let nbytes = (total_edges as usize)
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            if !ascending {
                let mut bucket_rev_idx = buckets.len() as isize - 1;
                let mut slot_rev: Option<u32> = None;
                while bucket_rev_idx >= 0 {
                    let bidx = bucket_rev_idx as usize;
                    let bucket = &buckets[bidx];
                    if bucket.degree() == 0 {
                        bucket_rev_idx -= 1;
                        slot_rev = None;
                        continue;
                    }
                    let bucket_index = bidx as u32;
                    let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                    let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                    let rel = bucket
                        .edge_start()
                        .saturating_sub(base)
                        .checked_add(u64::from(slot))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let byte_off = usize::try_from(rel)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                        .checked_mul(E::BYTES)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    let byte_end = byte_off
                        .checked_add(E::BYTES)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if byte_end > raw.len() {
                        return Err(LaraOperationError::CollectAllocationOverflow.into());
                    }
                    let chunk = &raw[byte_off..byte_end];
                    let cont = if let Some(raw_m) = raw_matches.as_mut() {
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        if raw_m(chunk) {
                            if matches(&edge) {
                                window.emit_edge(edge, visit)
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    } else {
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        if matches(&edge) {
                            window.emit_edge(edge, visit)
                        } else {
                            true
                        }
                    };
                    if !cont {
                        return Ok(());
                    }
                    if slot == 0 {
                        bucket_rev_idx -= 1;
                        slot_rev = None;
                    } else {
                        slot_rev = Some(slot - 1);
                    }
                }
            } else {
                for (bucket_index, bucket) in buckets.iter().enumerate() {
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let bucket_index = bucket_index as u32;
                    let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                    for slot in 0..bucket.degree() {
                        let rel = bucket
                            .edge_start()
                            .saturating_sub(base)
                            .checked_add(u64::from(slot))
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        let byte_off = usize::try_from(rel)
                            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                            .checked_mul(E::BYTES)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        let byte_end = byte_off
                            .checked_add(E::BYTES)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        if byte_end > raw.len() {
                            return Err(LaraOperationError::CollectAllocationOverflow.into());
                        }
                        let chunk = &raw[byte_off..byte_end];
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot,
                            E::read_from(chunk)
                                .with_slot_index(slot)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            raw_m(chunk) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                }
            }
            return Ok(());
        }

        if !ascending {
            for bucket_index in (0..buckets.len()).rev() {
                let bucket_index = bucket_index as u32;
                let bucket = &buckets[bucket_index as usize];
                if bucket.degree() == 0 {
                    continue;
                }
                let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
                let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
                if bucket.overflow_log_head() < 0 {
                    let mut it = OutEdgeSlabIter::try_new(
                        &self.edges,
                        bucket.edge_start(),
                        bucket.stored_slots,
                        bucket.degree(),
                    )?;
                    let has_raw = raw_matches.is_some();
                    while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                        let slot_index = edge.edge_slot_index_raw();
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        if has_raw {
                            if matches(&edge) && !window.emit_edge(edge, visit) {
                                return Ok(());
                            }
                        } else if matches(&edge) && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                } else {
                    for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            let mut buf = vec![0u8; E::BYTES];
                            edge.write_to(&mut buf);
                            raw_m(&buf) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                }
            }
        } else {
            for bucket_index in 0..buckets.len() as u32 {
                let bucket = &buckets[bucket_index as usize];
                if bucket.degree() == 0 {
                    continue;
                }
                let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
                let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
                if bucket.overflow_log_head() < 0 {
                    for slot_idx in 0..bucket.degree() {
                        let at = checked_add_slot_index(bucket.edge_start(), u64::from(slot_idx))
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        if self.edges.read_slot(at).is_deleted_slot() {
                            continue;
                        }
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_idx,
                            self.edges
                                .read_slot(at)
                                .with_slot_index(slot_idx)
                                .with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            let mut buf = vec![0u8; E::BYTES];
                            edge.write_to(&mut buf);
                            raw_m(&buf) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                } else {
                    for edge in self.edges.asc_out_edges(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        let edge = self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        );
                        let passes = if let Some(raw_m) = raw_matches.as_mut() {
                            let mut buf = vec![0u8; E::BYTES];
                            edge.write_to(&mut buf);
                            raw_m(&buf) && matches(&edge)
                        } else {
                            matches(&edge)
                        };
                        if passes && !window.emit_edge(edge, visit) {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn labeled_out_edges_iter(
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
            if let Some(directedness) = directedness {
                if self.bypass_storage_label_for(&vertex).directedness() != directedness {
                    return Ok(LabeledOutEdgesIter::empty(self, src, order));
                }
            }
            return match order {
                OutEdgeOrder::Descending => Ok(LabeledOutEdgesIter {
                    graph: self,
                    src,
                    order,
                    kind: LabeledOutEdgesIterKind::BypassDesc {
                        label_id: self.bypass_storage_label_for(&vertex),
                        iter: OutEdgeSlabIter::try_new(
                            &self.edges,
                            vertex.base_slot_start(),
                            vertex.stored_degree(),
                            vertex.degree(),
                        )?,
                    },
                }),
                OutEdgeOrder::Ascending => Ok(LabeledOutEdgesIter {
                    graph: self,
                    src,
                    order,
                    kind: LabeledOutEdgesIterKind::BypassAsc {
                        label_id: self.bypass_storage_label_for(&vertex),
                        iter: self.edges.asc_out_edges_iter(&self.vertices, src)?,
                    },
                }),
            };
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
        let next_bucket = match order {
            OutEdgeOrder::Descending => buckets.len().checked_sub(1),
            OutEdgeOrder::Ascending => (!buckets.is_empty()).then_some(0),
        };
        Ok(LabeledOutEdgesIter {
            graph: self,
            src,
            order,
            kind: LabeledOutEdgesIterKind::Buckets {
                vertex,
                buckets,
                base_bucket_index,
                next_bucket,
                current: LabeledSpanIter::Empty,
            },
        })
    }

    fn labeled_bucket_span_iter<'a>(
        &'a self,
        src: VertexId,
        order: OutEdgeOrder,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
        local_bucket_index: usize,
        bucket_index: u32,
    ) -> Result<LabeledSpanIter<'a, E, M>, LabeledOperationError> {
        let bucket = buckets[local_bucket_index];
        if bucket.degree() == 0 {
            return Ok(LabeledSpanIter::Empty);
        }
        let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
        let successor_start =
            self.bucket_successor_start_after_bucket(vertex, bucket_index, &bucket)?;
        let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
        let log_chains = self.bucket_value_log_chains_opt(src, &bucket);
        match order {
            OutEdgeOrder::Descending => Ok(LabeledSpanIter::Desc {
                graph: self,
                src,
                vertex: *vertex,
                bucket_index,
                bucket,
                label_id: bucket.bucket_label_key(),
                log_chains,
                iter: self.edges.out_edges_iter(&acc, VertexId::from(0))?,
            }),
            OutEdgeOrder::Ascending => Ok(LabeledSpanIter::Asc {
                graph: self,
                src,
                vertex: *vertex,
                bucket_index,
                bucket,
                label_id: bucket.bucket_label_key(),
                log_chains,
                iter: self.edges.asc_out_edges_iter(&acc, VertexId::from(0))?,
            }),
        }
    }

    /// Visits outgoing edges in **descending** scan order (reverse label-bucket walk; within each
    /// span, overflow log head first then slab high→low when a log exists, otherwise a lightweight
    /// slab walk).
    ///
    /// `offset` / `limit` apply to the stream of edges **accepted** by `raw_matches` (when
    /// present) and `matches`, matching [`EdgeStore::visit_out_edges`]: when
    /// `raw_matches` is `Some`, slab slots consult raw bytes **before** decode and `matches` still
    /// gates every decoded edge.
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

    /// [`Self::visit_out_edges`] with a trivial `matches` predicate (`|_| true`).
    pub fn visit_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.visit_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }

    /// Like [`Self::visit_out_edges`], but **ascending** materialization order (ascending bucket index,
    /// and within each span [`EdgeStore::asc_out_edges`]).
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

    /// [`Self::visit_asc_out_edges`] with a trivial `matches` predicate (`|_| true`).
    pub fn visit_asc_out_edges_unfiltered<Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.visit_asc_out_edges(src, offset, limit, raw_matches, |_| true, visit)
    }

    /// All outgoing edges in descending scan order (see [`Self::visit_out_edges`]).
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError> {
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

    /// All outgoing edges in ascending slot/materialization order (see [`Self::visit_asc_out_edges`]).
    pub fn asc_out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError> {
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

    /// Descending-scan iterator (same order as [`Self::out_edges`]; see [`LabeledOutEdgesIter`]).
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Descending, None)
    }

    /// Ascending slot/materialization iterator (same order as [`Self::asc_out_edges`]).
    pub fn asc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, OutEdgeOrder::Ascending, None)
    }

    /// Scans outgoing edges in [`Self::out_edges`] order and returns the first edge accepted by
    /// `pred`, together with its label bucket id.
    ///
    /// In default-label bypass mode the label is the vertex bypass storage label when a match
    /// exists.
    pub fn find_out_edge_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        mut pred: F,
    ) -> Result<Option<(E, Option<BucketLabelKey>)>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            let found = self.edges.find_first_out_edge_matching(
                &self.vertices,
                src,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                &mut pred,
            )?;
            return Ok(found.map(|e| (e.with_label_id(label.raw()), Some(label))));
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets)
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_find_out_edge_with_label_tiled");
            if total_edges == 0 {
                return Ok(None);
            }
            let degree = total_edges as usize;
            let nbytes = degree
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            let mut bucket_rev_idx = buckets.len() as isize - 1;
            let mut slot_rev: Option<u32> = None;
            while bucket_rev_idx >= 0 {
                let bidx = bucket_rev_idx as usize;
                let bucket = &buckets[bidx];
                if bucket.degree() == 0 {
                    bucket_rev_idx -= 1;
                    slot_rev = None;
                    continue;
                }
                let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                let rel = bucket
                    .edge_start()
                    .saturating_sub(base)
                    .checked_add(u64::from(slot))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let byte_off = usize::try_from(rel)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                    .checked_mul(E::BYTES)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let byte_end = byte_off
                    .checked_add(E::BYTES)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if byte_end > raw.len() {
                    return Err(LaraOperationError::CollectAllocationOverflow.into());
                }
                let edge = E::read_from(&raw[byte_off..byte_end]).with_slot_index(slot);
                if pred(&edge) {
                    return Ok(Some((
                        edge.with_slot_index(slot)
                            .with_label_id(bucket.bucket_label_key().raw()),
                        Some(bucket.bucket_label_key()),
                    )));
                }
                if slot == 0 {
                    bucket_rev_idx -= 1;
                    slot_rev = None;
                } else {
                    slot_rev = Some(slot - 1);
                }
            }
            return Ok(None);
        }
        for bucket_index in (0..buckets.len()).rev() {
            let bucket_index = bucket_index as u32;
            let bucket = &buckets[bucket_index as usize];
            let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            let successor_start =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
            let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
            if let Some(edge) = self.edges.find_first_out_edge_matching(
                &acc,
                VertexId::from(0),
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                &mut pred,
            )? {
                return Ok(Some((
                    edge.with_label_id(bucket.bucket_label_key().raw()),
                    Some(bucket.bucket_label_key()),
                )));
            }
        }
        Ok(None)
    }

    /// Scans outgoing edges in default descending order and returns the first matching edge with
    /// its label and physical slot index within that label row.
    pub fn find_out_edge_slot_with_label_by_predicate<F>(
        &self,
        src: VertexId,
        mut pred: F,
    ) -> Result<Option<(E, BucketLabelKey, u32)>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            let found =
                self.edges
                    .find_first_out_edge_slot_matching(&self.vertices, src, |edge| pred(edge))?;
            return Ok(found.map(|(slot, edge)| (edge.with_label_id(label.raw()), label, slot)));
        }

        let buckets = self.read_vertex_label_buckets(&vertex)?;
        for bucket_index in (0..buckets.len()).rev() {
            let bucket_index = bucket_index as u32;
            let mut bucket = buckets[bucket_index as usize];
            if bucket.degree() == 0 {
                continue;
            }
            if bucket.overflow_log_head() >= 0 {
                let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                bucket = self.ensure_label_bucket_folded_to_slab(
                    src,
                    bucket_index,
                    bucket_slot,
                    bucket,
                )?;
            }
            for slot_index in (0..bucket.stored_slots).rev() {
                let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(edge_slot).with_slot_index(slot_index);
                if edge.is_deleted_slot() {
                    continue;
                }
                if pred(&edge) {
                    return Ok(Some((
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        bucket.bucket_label_key(),
                        slot_index,
                    )));
                }
            }
        }
        Ok(None)
    }

    /// Removes the edge at one physical slot within a labeled adjacency row.
    pub fn remove_edge_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(None);
            }
            return self
                .edges
                .remove_edge_at_slab_slot(&self.vertices, src, slot_index)
                .map_err(Into::into);
        }
        let BucketSearch::Found { slot, mut bucket } = self.find_bucket(src, &vertex, label_id)?
        else {
            return Ok(None);
        };
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        if bucket.overflow_log_head() >= 0 {
            bucket = self.ensure_label_bucket_folded_to_slab(src, bucket_index, slot, bucket)?;
        }
        if slot_index >= bucket.stored_slots {
            return Ok(None);
        }
        let rm_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.edges.read_slot(rm_slot);
        if removed.is_deleted_slot() {
            return Ok(None);
        }
        self.edges
            .write_slot(rm_slot, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        let updated = bucket
            .after_slab_tombstone_delete()
            .with_overflow_log_head(-1);
        let updated = if updated.degree() == 0 {
            self.release_bucket_value_span(src, &bucket)?;
            updated
                .with_value_log_head(-1)
                .with_edge_range(updated.edge_start(), 0)
        } else {
            updated
        };
        self.buckets.write_label_bucket_slot(slot, updated)?;
        let hdr = self.edges.header();
        let next_global = hdr
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_global);
        self.edges
            .bump_vertex_segment_counts(src, -1, 0)
            .map_err(LabeledOperationError::from)?;
        Ok(Some(removed))
    }

    /// Iterates all outgoing edges for one label without per-edge label checks.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, LabeledOperationError> {
        let mut out = Vec::new();
        self.for_each_edges_for_label(src, label_id, |edge| out.push(edge))?;
        Ok(out)
    }

    #[inline]
    fn directedness_partition_strategy(
        directedness: BucketDirectedness,
        ascending: bool,
    ) -> DirectednessPartitionStrategy {
        match (directedness, ascending) {
            (BucketDirectedness::Directed, false) => DirectednessPartitionStrategy::LinearFromEnd,
            (BucketDirectedness::Directed, true) => DirectednessPartitionStrategy::HybridBinary,
            (BucketDirectedness::Undirected, false) => DirectednessPartitionStrategy::HybridBinary,
            (BucketDirectedness::Undirected, true) => {
                DirectednessPartitionStrategy::LinearFromStart
            }
        }
    }

    /// Half-open global bucket indices `[lo, hi)` on `src` whose [`LabelBucket::bucket_label_key`]
    /// matches `directedness`.
    ///
    /// Under LARA's ascending-key invariant, undirected (MSB clear) and directed (MSB set) buckets
    /// occupy contiguous runs; the partition is found using a strategy derived from `ascending`
    /// (see [`DirectednessPartitionStrategy`]).
    ///
    /// Homogeneous bypass vertices have no bucket slab row: returns `(0, 0)` — use
    /// [`LabeledVertex::is_default_edge_labeled`] and [`LabeledVertex::bypass_storage_label`] for edge bytes.
    ///
    /// `order` selects the same partition probe strategy as [`Self::out_edges_by_directedness_iter`]
    /// (directed + descending probes from the tail; undirected + ascending from the head; the other
    /// two quadrants use hybrid binary→linear).
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

    /// Outgoing edges whose [`BucketLabelKey`] matches `directedness`, in `order`.
    ///
    /// Homogeneous bypass rows contribute edges only when
    /// [`LabeledVertex::bypass_storage_label`] matches `directedness`; otherwise the result is empty.
    ///
    /// Label-bucket mode uses [`LabelBucketStore::directedness_bucket_index_range`] with a probe
    /// strategy derived from `order`: directed + descending scans from the tail; undirected +
    /// ascending from the head; the other two quadrants use hybrid binary search then a short linear
    /// finish ([`DirectednessPartitionStrategy`]).
    ///
    /// Prefer [`OutEdgeOrder::Descending`] on hot paths: it aligns with [`Self::out_edges`].
    /// Default-label bypass rows use [`OutEdgeSlabIter`] for descending walks.
    pub fn iter_out_edges_by_directedness(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.out_edges_by_directedness_iter(src, directedness, order)
            .map(|iter| iter.collect())
    }

    fn for_each_out_edges_by_directedness_impl<Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        directedness: BucketDirectedness,
        ascending: bool,
        visit: &mut Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        if vertex.is_default_edge_labeled() {
            if self.bypass_storage_label_for(vertex).directedness() != directedness {
                return Ok(());
            }
            match ascending {
                false => {
                    let slab_iter = OutEdgeSlabIter::try_new(
                        &self.edges,
                        vertex.base_slot_start(),
                        vertex.stored_degree(),
                        vertex.degree(),
                    )?;
                    let label = self.bypass_storage_label_for(vertex).raw();
                    for edge in slab_iter {
                        visit(edge.with_label_id(label));
                    }
                }
                true => {
                    let label = self.bypass_storage_label_for(vertex).raw();
                    for edge in self.edges.asc_out_edges(&self.vertices, src)? {
                        visit(edge.with_label_id(label));
                    }
                }
            }
            return Ok(());
        }
        let deg = vertex.degree();
        let strategy = Self::directedness_partition_strategy(directedness, ascending);
        let (lo, hi) = self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?;
        if lo >= hi {
            return Ok(());
        }
        let first_global = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_end_exclusive = Self::vertex_label_edge_span_end_exclusive(vertex, &first_global)?;
        let buckets = self.read_vertex_label_buckets_range(vertex, lo, hi)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges_slice(&buckets, span_end_exclusive)
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_out_edges_by_directedness_tiled");
            if total_edges > 0 {
                let nbytes = (total_edges as usize)
                    .checked_mul(E::BYTES)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let mut raw = vec![0u8; nbytes];
                self.edges.read_slots_contiguous(base, &mut raw);
                match ascending {
                    false => {
                        let mut bucket_rev_idx = buckets.len() as isize - 1;
                        let mut slot_rev: Option<u32> = None;
                        while bucket_rev_idx >= 0 {
                            let bidx = bucket_rev_idx as usize;
                            let bucket = &buckets[bidx];
                            if bucket.degree() == 0 {
                                bucket_rev_idx -= 1;
                                slot_rev = None;
                                continue;
                            }
                            let bucket_index = lo + bidx as u32;
                            let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                            let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                            let rel = bucket
                                .edge_start()
                                .saturating_sub(base)
                                .checked_add(u64::from(slot))
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let byte_off = usize::try_from(rel)
                                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                                .checked_mul(E::BYTES)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let byte_end = byte_off
                                .checked_add(E::BYTES)
                                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            if byte_end > raw.len() {
                                return Err(LaraOperationError::CollectAllocationOverflow.into());
                            }
                            visit(
                                self.labeled_edge_with_value(
                                    src,
                                    vertex,
                                    bucket_index,
                                    *bucket,
                                    slot,
                                    E::read_from(&raw[byte_off..byte_end])
                                        .with_slot_index(slot)
                                        .with_label_id(bucket.bucket_label_key().raw()),
                                    log_chains.as_ref(),
                                ),
                            );
                            if slot == 0 {
                                bucket_rev_idx -= 1;
                                slot_rev = None;
                            } else {
                                slot_rev = Some(slot - 1);
                            }
                        }
                    }
                    true => {
                        for (local, bucket) in buckets.iter().enumerate() {
                            if bucket.degree() == 0 {
                                continue;
                            }
                            let bucket_index = lo + local as u32;
                            let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                            for slot in 0..bucket.degree() {
                                let rel = bucket
                                    .edge_start()
                                    .saturating_sub(base)
                                    .checked_add(u64::from(slot))
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                                let byte_off = usize::try_from(rel)
                                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                                    .checked_mul(E::BYTES)
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                                let byte_end = byte_off
                                    .checked_add(E::BYTES)
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                                if byte_end > raw.len() {
                                    return Err(
                                        LaraOperationError::CollectAllocationOverflow.into()
                                    );
                                }
                                visit(
                                    self.labeled_edge_with_value(
                                        src,
                                        vertex,
                                        bucket_index,
                                        *bucket,
                                        slot,
                                        E::read_from(&raw[byte_off..byte_end])
                                            .with_slot_index(slot)
                                            .with_label_id(bucket.bucket_label_key().raw()),
                                        log_chains.as_ref(),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        match ascending {
            false => {
                for local_rev in (0..buckets.len()).rev() {
                    let bucket_index = lo + local_rev as u32;
                    let bucket = &buckets[local_rev];
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor =
                        self.bucket_successor_start_after_bucket(vertex, bucket_index, bucket)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    if bucket.overflow_log_head() < 0 {
                        let it = OutEdgeSlabIter::try_new(
                            &self.edges,
                            bucket.edge_start(),
                            bucket.stored_slots,
                            bucket.degree(),
                        )?;
                        for edge in it {
                            let slot_index = edge.edge_slot_index_raw();
                            visit(self.labeled_edge_with_value(
                                src,
                                vertex,
                                bucket_index,
                                *bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            ));
                        }
                    } else {
                        for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                            let slot_index = edge.edge_slot_index_raw();
                            visit(self.labeled_edge_with_value(
                                src,
                                vertex,
                                bucket_index,
                                *bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            ));
                        }
                    }
                }
            }
            true => {
                for local in 0..buckets.len() {
                    let bucket_index = lo + local as u32;
                    let bucket = &buckets[local];
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let log_chains = self.bucket_value_log_chains_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor =
                        self.bucket_successor_start_after_bucket(vertex, bucket_index, bucket)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    for edge in self.edges.asc_out_edges(&acc, VertexId::from(0))? {
                        let slot_index = edge.edge_slot_index_raw();
                        visit(self.labeled_edge_with_value(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Visits outgoing edges whose [`BucketLabelKey`] matches `directedness`, in `order`.
    ///
    /// Same visitation contract as [`Self::out_edges_by_directedness_iter`] without materializing
    /// the full adjacency list.
    pub fn for_each_out_edges_by_directedness<Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.for_each_out_edges_by_directedness_impl(
            src,
            &vertex,
            directedness,
            order.ascending(),
            &mut visit,
        )
    }

    /// Like [`Self::for_each_out_edges_by_directedness`], but skips [`Self::ensure_vertex`].
    pub fn for_each_out_edges_by_directedness_unchecked<Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        debug_assert!(u32::from(src) < self.vertices.len());
        let vertex = self.vertices.get(src);
        self.for_each_out_edges_by_directedness_impl(
            src,
            &vertex,
            directedness,
            order.ascending(),
            &mut visit,
        )
    }

    /// Iterator form of [`Self::iter_out_edges_by_directedness`].
    pub fn out_edges_by_directedness_iter(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.labeled_out_edges_iter(src, order, Some(directedness))
    }

    /// Directed buckets only: [`Self::iter_out_edges_by_directedness`] with [`BucketDirectedness::Directed`].
    #[inline]
    pub fn iter_out_edges_directed_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Directed, order)
    }

    /// Undirected buckets only: [`Self::iter_out_edges_by_directedness`] with [`BucketDirectedness::Undirected`].
    #[inline]
    pub fn iter_out_edges_undirected_only(
        &self,
        src: VertexId,
        order: OutEdgeOrder,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.iter_out_edges_by_directedness(src, BucketDirectedness::Undirected, order)
    }

    /// Returns the label id of the bucket that contains `needle`, if any.
    ///
    /// Scans buckets in ascending [`LabelBucket::edge_len`] so small selective buckets
    /// are checked before large noise buckets on skewed hubs.
    pub fn find_edge_label(
        &self,
        src: VertexId,
        needle: &E,
    ) -> Result<Option<BucketLabelKey>, LabeledOperationError>
    where
        E: PartialEq,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            if needle
                .edge_label_id_raw()
                .is_some_and(|needle_label| needle_label != label.raw())
            {
                return Ok(None);
            }
            let mut found = false;
            self.edges.visit_out_edges(
                &self.vertices,
                src,
                None,
                None,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |_| true,
                |edge| {
                    let edge = edge.with_label_id(label.raw());
                    if Self::edge_matches_label_lookup(&edge, needle) {
                        found = true;
                    }
                },
            )?;
            return Ok(found.then_some(label));
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(None);
        }
        let mut buckets = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = Self::labeled_vertex_bucket_slot(&vertex, offset)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            buckets.push((slot, offset, bucket));
        }
        buckets.sort_by_key(|(_, _, bucket)| bucket.stored_slots);
        for (slot, bucket_index, bucket) in buckets {
            let successor =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
            if needle
                .edge_label_id_raw()
                .is_some_and(|needle_label| needle_label != bucket.bucket_label_key().raw())
            {
                continue;
            }
            let log_chains = self.bucket_value_log_chains_opt(src, &bucket);
            let found_label = Cell::new(None);
            self.edges.visit_out_edges(
                &LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src),
                VertexId::from(0),
                None,
                None,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |_| found_label.get().is_none(),
                |edge| {
                    let slot_index = edge.edge_slot_index_raw();
                    let edge = self.labeled_edge_with_value(
                        src,
                        &vertex,
                        bucket_index,
                        bucket,
                        slot_index,
                        edge.with_label_id(bucket.bucket_label_key().raw()),
                        log_chains.as_ref(),
                    );
                    if Self::edge_matches_label_lookup(&edge, needle) {
                        found_label.set(Some(bucket.bucket_label_key()));
                    }
                },
            )?;
            if let Some(label_id) = found_label.into_inner() {
                return Ok(Some(label_id));
            }
        }
        Ok(None)
    }

    /// Returns the sorted label ids that own at least one outgoing edge bucket for `src`.
    ///
    /// In default-label bypass mode this returns a single-element slice containing
    /// [`Self::default_label`].
    pub fn out_edge_label_ids(
        &self,
        src: VertexId,
    ) -> Result<Vec<BucketLabelKey>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(Vec::new());
            }
            return Ok(vec![self.bypass_storage_label_for(&vertex)]);
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = Self::labeled_vertex_bucket_slot(&vertex, offset)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            out.push(bucket.bucket_label_key());
        }
        Ok(out)
    }

    /// Removes the first edge that satisfies `matches`.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let removed = self.remove_edge_matching_skip_leaf_cascade(src, label_id, matches)?;
        if removed.is_some() && self.labeled_leaf_segment_is_dense(src) {
            self.rebalance_cascade_after_labeled_mutation(src)?;
        }
        Ok(removed)
    }

    /// Like [`Self::remove_edge_matching`], but skips the post-remove leaf cascade.
    pub(crate) fn remove_edge_matching_skip_leaf_cascade<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(None);
            }
            if vertex.degree() == 0 {
                return Ok(None);
            }
            return self
                .edges
                .remove_edge_slab_tombstone_matching(&self.vertices, src, matches)
                .map_err(Into::into);
        }
        if let BucketSearch::Found { slot, mut bucket } =
            self.find_bucket(src, &vertex, label_id)?
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_remove_edge_skip_leaf");
            let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
            if bucket.degree() == 0 {
                return Ok(None);
            }
            if bucket.overflow_log_head() >= 0 {
                bucket =
                    self.ensure_label_bucket_folded_to_slab(src, bucket_index, slot, bucket)?;
                if bucket.degree() == 0 {
                    return Ok(None);
                }
            }
            let log_chains = self.bucket_value_log_chains_opt(src, &bucket);
            let stored = bucket.stored_slots;
            let mut found = None;
            for offset in 0..stored {
                let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(offset))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(edge_slot);
                if edge.is_tombstone_edge() {
                    continue;
                }
                let edge_with_value = self.labeled_edge_with_value(
                    src,
                    &vertex,
                    bucket_index,
                    bucket,
                    offset,
                    edge,
                    log_chains.as_ref(),
                );
                if matches(&edge_with_value) {
                    found = Some((offset, edge_with_value));
                    break;
                }
            }
            let Some((local_index, removed)) = found else {
                return Ok(None);
            };
            let rm_slot = checked_add_slot_index(bucket.edge_start(), u64::from(local_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .write_slot(rm_slot, E::tombstone_edge())
                .map_err(LabeledOperationError::from)?;
            let updated = bucket
                .after_slab_tombstone_delete()
                .with_overflow_log_head(-1);
            let updated = if updated.degree() == 0 {
                self.release_bucket_value_span(src, &bucket)?;
                updated
                    .with_value_log_head(-1)
                    .with_edge_range(updated.edge_start(), 0)
            } else {
                updated
            };
            self.buckets.write_label_bucket_slot(slot, updated)?;
            let hdr = self.edges.header();
            let next_global = hdr
                .num_edges
                .checked_sub(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges.set_num_edges(next_global);
            self.edges
                .bump_vertex_segment_counts(src, -1, 0)
                .map_err(LabeledOperationError::from)?;
            return Ok(Some(removed));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        VertexId,
        labeled::MAX_VERTEX_LABEL_BUCKETS,
        test_support::vector_memory,
        traits::{CsrEdge, CsrEdgeTombstone},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEdge {
        target: u32,
    }

    impl CsrEdge for TestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self {
                target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            }
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.target)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self {
                target: u32::from(vid),
            }
        }
    }

    impl CsrEdgeTombstone for TestEdge {
        fn tombstone_edge() -> Self {
            Self {
                target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            }
        }
    }

    fn mem() -> crate::VectorMemory {
        vector_memory()
    }

    fn test_graph_with_default(
        default_label: BucketLabelKey,
    ) -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            256,
            default_label,
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
    }

    fn test_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
        test_graph_with_default(BucketLabelKey::directed_from_index(1))
    }

    #[test]
    fn homogeneous_bypass_append_extends_edge_capacity() {
        let default = BucketLabelKey::from_raw(7);
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            1,
            default,
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();

        for target in 0..4 {
            graph
                .insert_edge(hub, default, TestEdge { target })
                .unwrap_or_else(|e| panic!("insert target={target}: {e:?}"));
        }

        assert_eq!(graph.vertices().get(hub).degree(), 4);
        assert!(graph.edges().header().elem_capacity >= 4);
        assert_eq!(graph.iter_edges_for_label(hub, default).unwrap().len(), 4);
    }

    #[test]
    fn homogeneous_bypass_append_rejects_degree_overflow() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_degree(u32::MAX),
        );

        let err = graph
            .insert_edge(hub, default, TestEdge { target: 1 })
            .expect_err("max degree must be rejected");
        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
        assert_eq!(graph.vertices().get(hub).degree(), u32::MAX);
    }

    #[test]
    fn homogeneous_bypass_appends_stay_slab_only_without_overflow_log() {
        let default = BucketLabelKey::from_raw(7);
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            256,
            default,
        )
        .unwrap();
        let hub = graph
            .push_vertex(LabeledVertex::default().with_homogeneous_bypass_label(default))
            .unwrap();

        for target in 0..8u32 {
            graph
                .insert_edge(hub, default, TestEdge { target })
                .unwrap();
        }

        let vertex = graph.vertices().get(hub);
        assert_eq!(vertex.bypass_overflow_log_head(), -1);
        assert_eq!(vertex.degree(), 8);
        assert_eq!(graph.iter_edges_for_label(hub, default).unwrap().len(), 8);
    }

    #[test]
    fn push_vertex_rejects_normal_row_with_label_bucket_overflow() {
        let graph = test_graph();
        let vertex = LabeledVertex::default().with_degree(MAX_VERTEX_LABEL_BUCKETS + 1);
        let err = graph
            .push_vertex(vertex)
            .expect_err("push must reject normal rows over MAX_VERTEX_LABEL_BUCKETS");
        assert!(matches!(
            err,
            LabeledOperationError::InvalidVertexRow(
                LabeledVertexFieldError::LabelBucketCountOverflow
            )
        ));
    }

    #[test]
    fn homogeneous_bypass_region_end_rejects_slot_overflow() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_base_slot_start(crate::labeled::slot_index::SLOT_INDEX_MASK)
                .with_degree(1)
                .with_stored_slots(1),
        );

        let err = graph
            .bypass_region_end(hub)
            .expect_err("bypass end overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn bucket_vertex_prefix_end_rejects_slot_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(
                    BucketLabelKey::from_raw(42),
                    crate::labeled::slot_index::SLOT_INDEX_MASK,
                    1,
                    1,
                    -1,
                ),
            )
            .unwrap();
        graph
            .vertices()
            .set(hub, &graph.vertices().get(hub).with_stored_slots(1));

        let err = graph
            .vertex_prefix_end(hub)
            .expect_err("bucket prefix end overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn last_bucket_successor_rejects_span_end_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        let label = BucketLabelKey::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(
                    label,
                    crate::labeled::slot_index::SLOT_INDEX_MASK,
                    0,
                    0,
                    -1,
                ),
            )
            .unwrap();
        graph
            .vertices()
            .set(hub, &graph.vertices().get(hub).with_stored_slots(1));

        let vertex = graph.vertices().get(hub);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        let err = graph
            .bucket_successor_start_after_bucket(&vertex, 0, &bucket)
            .expect_err("last bucket successor overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn contiguous_tiled_out_edges_rejects_span_end_overflow() {
        let vertex = LabeledVertex::default()
            .with_bucket_row(0, 1)
            .with_stored_slots(1);
        let buckets = [LabelBucket::from_parts(
            BucketLabelKey::from_raw(42),
            crate::labeled::slot_index::SLOT_INDEX_MASK,
            0,
            0,
            -1,
        )];

        assert_eq!(
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::try_contiguous_tiled_labeled_out_edges(
                &vertex,
                &buckets,
            ),
            None
        );
    }

    #[test]
    fn normal_label_bucket_insert_rejects_edge_len_overflow() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let hub = VertexId::from(0);
        let label = BucketLabelKey::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(label, 0, u32::MAX, u32::MAX, -1),
            )
            .unwrap();

        let err = graph
            .insert_edge(hub, label, TestEdge { target: 1 })
            .expect_err("max bucket edge_len must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
        let vertex = graph.vertices().get(hub);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.stored_slots, u32::MAX);
    }

    #[test]
    fn vertex_edge_span_rewrite_rejects_total_live_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(BucketLabelKey::from_raw(10), 0, u32::MAX, u32::MAX, -1),
            )
            .unwrap();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(BucketLabelKey::from_raw(20), 0, 1, 1, -1),
            )
            .unwrap();

        let err = graph
            .compact_vertex_edge_span(hub, 0)
            .expect_err("bucket edge_len sum overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
    }

    #[test]
    fn rewrite_copy_byte_len_rejects_usize_overflow() {
        let oversized = usize::MAX / TestEdge::BYTES + 1;
        let err = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::edge_bytes_for_len(oversized)
            .expect_err("edge byte length overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn label_edge_span_positioning_rejects_impossible_live_width() {
        let err =
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::calculate_label_edge_span_positions(
                0,
                1,
                &[LabelBucket::from_parts(
                    BucketLabelKey::from_raw(10),
                    0,
                    2,
                    2,
                    -1,
                )],
                None,
                0,
            )
            .expect_err("live edges wider than span must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn two_label_hub_500_then_173_parallel_edges() {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let a = BucketLabelKey::from_raw(10_000);
        let b = BucketLabelKey::from_raw(10_001);
        for edge_i in 0..500u32 {
            graph
                .insert_edge(
                    hub,
                    a,
                    TestEdge {
                        target: u32::from(dst),
                    },
                )
                .unwrap_or_else(|e| panic!("label_a edge_i={edge_i}: {e:?}"));
        }
        for edge_i in 0..174u32 {
            graph
                .insert_edge(
                    hub,
                    b,
                    TestEdge {
                        target: u32::from(dst),
                    },
                )
                .unwrap_or_else(|e| panic!("label_b edge_i={edge_i}: {e:?}"));
        }
    }

    #[test]
    fn mixed_label_hub_20_labels_500_edges_each() {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..20u16 {
            let label = BucketLabelKey::from_raw(10_000 + label_idx);
            for edge_i in 0..500u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
    }

    #[test]
    fn mixed_label_hub_parallel_edges_do_not_corrupt_overflow_log() {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..10u16 {
            let label = BucketLabelKey::from_raw(1000 + label_idx);
            for edge_i in 0..24u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn push_vertex_grows_pma_segment_tree_before_high_leaf_edge_insert() {
        let graph = test_graph_with_default(BucketLabelKey::from_raw(1));
        for _ in 1..33 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let high = VertexId::from(32);
        graph
            .insert_edge(high, BucketLabelKey::from_raw(2), TestEdge { target: 0 })
            .unwrap();
        assert!(graph.edges().header().segment_count >= 2);
    }

    #[test]
    fn labeled_insert_and_iter_by_label() {
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

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 20 },
                TestEdge { target: 11 },
                TestEdge { target: 10 },
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
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
            .collect();
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
                .collect::<Vec<_>>(),
            desc
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
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
            .collect();
        assert_eq!(full.len(), 2);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(0), Ok(()));
        assert_eq!(it.next(), Some(full[0]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(1), Ok(()));
        assert_eq!(it.next(), Some(full[1]));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(2), Ok(()));
        assert_eq!(it.next(), None);

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.advance_by(3), Err(NonZero::new(1).unwrap()));

        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(0), Some(full[0]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1), Some(full[1]));
        let mut it = graph.desc_out_edges_iter(VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2), None);
    }

    #[test]
    fn visit_out_edges_with_raw_still_applies_matches_on_log_backed_bucket() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        for target in 1..=33u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        let mut visited = Vec::new();
        let mut raw_all = |_bytes: &[u8]| true;
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                Some(2),
                Some(&mut raw_all),
                |edge| edge.target < 100 && edge.target % 2 == 0,
                |edge| visited.push(edge),
            )
            .unwrap();

        assert_eq!(
            visited.iter().map(|e| e.target).collect::<Vec<_>>(),
            vec![32, 30]
        );
        for edge in &visited {
            assert_eq!(edge.value_len, 2);
            assert_eq!(
                u16::from_le_bytes([edge.value[0], edge.value[1]]),
                u16::try_from(edge.target).unwrap()
            );
        }
    }

    #[test]
    fn out_edges_by_directedness_filters_and_orders() {
        use crate::labeled::BucketDirectedness;

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
    fn out_edge_bucket_index_range_agrees_with_slice_partition() {
        use crate::labeled::BucketDirectedness;

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

        let v = graph.vertices().get(VertexId::from(0));
        let base = v.base_slot_start();
        let deg = v.degree();
        let full = graph
            .buckets()
            .read_label_bucket_slots_contiguous(base, deg)
            .unwrap();
        let p = full.partition_point(|b| b.bucket_label_key().is_undirected());

        assert_eq!(
            graph
                .out_edge_bucket_index_range_for_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Undirected,
                    OutEdgeOrder::Ascending,
                )
                .unwrap(),
            (0, p as u32)
        );
        assert_eq!(
            graph
                .out_edge_bucket_index_range_for_directedness(
                    VertexId::from(0),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            (p as u32, deg)
        );

        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_linear_from_start(base, deg)
                .unwrap(),
            p as u32
        );
        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_linear_from_end(base, deg)
                .unwrap(),
            p as u32
        );
        assert_eq!(
            graph
                .buckets()
                .partition_first_directed_hybrid(base, deg)
                .unwrap(),
            p as u32
        );
    }

    #[test]
    fn out_edges_by_directedness_bypass_empty_when_directedness_mismatches() {
        use crate::labeled::BucketDirectedness;

        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 9 },
            )
            .unwrap();
        assert!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(1),
                    BucketDirectedness::Undirected,
                    OutEdgeOrder::Descending,
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(1),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            vec![TestEdge { target: 9 }]
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
    fn mixed_default_bypass_and_normal_labeled_pma_counts_stay_consistent() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 1 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 2 },
            )
            .unwrap();
        graph.enable_default_edge_bypass(VertexId::from(1)).unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 3 },
            )
            .unwrap();
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn labeled_dense_leaf_triggers_slack_growth_cascade() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        let mut alloc = graph.vertices().get(VertexId::from(0)).stored_slots;
        let mut grew = false;
        for target in 0..512u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
            let next = graph.vertices().get(VertexId::from(0)).stored_slots;
            if next > alloc {
                grew = true;
                break;
            }
            alloc = next;
        }
        assert!(
            grew,
            "expected dense-leaf cascade to expand VertexEdgeSpan reservation"
        );
    }

    #[test]
    fn label_buckets_and_edges_follow_label_order() {
        let graph = test_graph();
        for (label, target) in [(10, 100), (2, 20), (7, 70), (2, 21)] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge { target },
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let labels: Vec<_> = (0..vertex.degree())
            .map(|offset| {
                graph
                    .buckets()
                    .read_label_bucket_slot(vertex.base_slot_start() + u64::from(offset))
                    .unwrap()
                    .bucket_label_key()
                    .raw()
            })
            .collect();
        assert_eq!(labels, vec![2, 7, 10]);
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 100 },
                TestEdge { target: 70 },
                TestEdge { target: 21 },
                TestEdge { target: 20 },
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn default_bypass_points_directly_into_edge_csr() {
        let graph = test_graph();
        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                graph.default_label(),
                TestEdge { target: 7 },
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 7 }]
        );
    }

    #[test]
    fn find_bucket_resolves_non_default_label_after_parallel_inserts() {
        let graph = test_graph();
        let label = BucketLabelKey::from_raw(42);
        for target in 0..8u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert!(graph.find_bucket_slot(&vertex, label).unwrap().is_some());
    }

    #[test]
    fn parallel_catalog_edges_on_high_index_vertex_stay_on_slab() {
        let graph = test_graph();
        for _ in 0..64 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(42);
        for target in 0..240u32 {
            graph.insert_edge(hub, label, TestEdge { target }).unwrap();
        }
        let vertex = graph.vertices().get(hub);
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(graph.iter_edges_for_label(hub, label).unwrap().len(), 240);
    }

    #[test]
    fn catalog_label_parallel_inserts_use_single_bucket_row() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(42);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 0 })
            .unwrap();
        let bucket_only = graph.vertices().get(VertexId::from(0));
        assert!(!bucket_only.is_default_edge_labeled());
        for target in 1..24u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            24
        );
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            vec![road]
        );
    }

    #[test]
    fn bypass_grow_does_not_repoint_bucket_mode_successor_bucket_base() {
        let graph = LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            1 << 16,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(42);
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let mut prefixes = Vec::new();
        for _ in 0..8 {
            prefixes.push(graph.push_vertex(LabeledVertex::default()).unwrap());
        }
        for &prefix in &prefixes {
            graph
                .insert_edge(
                    prefix,
                    road,
                    TestEdge {
                        target: u32::from(hub),
                    },
                )
                .unwrap();
        }
        let src = VertexId::from(0);
        for (i, &prefix) in prefixes.iter().enumerate() {
            graph
                .insert_edge(
                    src,
                    road,
                    TestEdge {
                        target: u32::from(prefix),
                    },
                )
                .unwrap();
            let bucket_base = graph.vertices().get(prefix).base_slot_start();
            graph
                .buckets()
                .read_label_bucket_slot(bucket_base)
                .expect("prefix bucket still readable after src bypass growth");
            assert_eq!(
                graph.vertices().get(prefix).degree(),
                1,
                "prefix {i} still has one label bucket"
            );
        }
        graph
            .insert_edge(
                hub,
                road,
                TestEdge {
                    target: u32::from(dst),
                },
            )
            .unwrap();
        assert!(!graph.vertices().get(hub).is_default_edge_labeled());
        assert_eq!(graph.vertices().get(hub).degree(), 1);
        assert_eq!(
            graph.iter_edges_for_label(hub, road).unwrap(),
            vec![TestEdge {
                target: u32::from(dst)
            }]
        );
    }

    #[test]
    fn first_homogeneous_insert_enters_bypass_without_enable() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 9 },
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(1));
        assert!(vertex.is_default_edge_labeled());
        assert!(!vertex.is_bypass_undirected());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(1)).unwrap(),
            vec![graph.default_label()]
        );
        let earlier = graph.vertices().get(VertexId::from(0));
        assert!(!earlier.is_default_edge_labeled());
    }

    #[test]
    fn non_tail_single_label_insert_does_not_rebase_successor_bypass_edges() {
        let graph = test_graph();
        let successor = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(successor, graph.default_label(), TestEdge { target: 900 })
            .unwrap();

        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();

        assert!(
            !graph
                .vertices()
                .get(VertexId::from(0))
                .is_default_edge_labeled()
        );
        assert_eq!(
            graph
                .iter_edges_for_label(successor, graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 900 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }, TestEdge { target: 10 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
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
    fn bucket_tail_missing_cache_revalidates_cached_slot_label() {
        let graph = test_graph();
        let low = BucketLabelKey::from_raw(10);
        let old_tail = BucketLabelKey::from_raw(20);
        let inserted = BucketLabelKey::from_raw(30);
        let new_tail = BucketLabelKey::from_raw(40);

        graph
            .insert_edge(VertexId::from(0), low, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), old_tail, TestEdge { target: 20 })
            .unwrap();

        // Populate `last_bucket_lookup` with the old tail label.
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), old_tail)
                .unwrap(),
            vec![TestEdge { target: 20 }]
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let tail_slot = graph.find_bucket_slot(&vertex, old_tail).unwrap().unwrap();
        let tail_bucket = graph.buckets().read_label_bucket_slot(tail_slot).unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(tail_slot, tail_bucket.with_bucket_label_key(new_tail))
            .unwrap();

        graph
            .insert_edge(VertexId::from(0), inserted, TestEdge { target: 30 })
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
        let labels: Vec<_> = buckets
            .iter()
            .map(|bucket| bucket.bucket_label_key())
            .collect();
        assert_eq!(labels, vec![low, inserted, new_tail]);
    }

    #[test]
    fn homogeneous_undirected_bypass_and_promotion_on_named_label() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        let undirected = BucketLabelKey::UNLABELED_UNDIRECTED;
        graph
            .insert_edge(VertexId::from(0), undirected, TestEdge { target: 1 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), undirected, TestEdge { target: 2 })
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert!(vertex.is_bypass_undirected());
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), undirected)
                .unwrap(),
            vec![TestEdge { target: 2 }, TestEdge { target: 1 }]
        );

        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 99 })
            .unwrap();
        let after = graph.vertices().get(VertexId::from(0));
        assert!(!after.is_default_edge_labeled());
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), undirected)
                .unwrap(),
            vec![TestEdge { target: 2 }, TestEdge { target: 1 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 99 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn remove_edge_leaves_slab_tombstone_until_rebalance() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 12 })
            .unwrap();
        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 11)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 12 }, TestEdge { target: 10 }]
        );
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, 3);
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 1);
        assert_eq!(bucket.degree(), 2);

        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 13 })
            .unwrap();
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 13 },
                TestEdge { target: 12 },
                TestEdge { target: 10 },
            ]
        );
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, 4);
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 1);
        assert_eq!(bucket.degree(), 3);
    }

    #[test]
    fn incremental_vertex_edge_span_compact_clears_many_tombstones() {
        use super::VertexEdgeSpanCompactOneStep;

        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        for target in 1..=60u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in 1..=55u32 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket_before = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(
            bucket_before
                .stored_slots
                .saturating_sub(bucket_before.degree),
            55
        );
        assert_eq!(bucket_before.degree(), 5);

        let mut resume = 0u32;
        let mut edge_moves = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume)
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => edge_moves += 1,
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => break,
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        assert!(edge_moves > 0, "expected at least one in-bucket edge move");

        let bucket_after = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket_after.stored_slots, bucket_after.degree);
        assert_eq!(bucket_after.degree, 5);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            (56..=60)
                .rev()
                .map(|target| TestEdge { target })
                .collect::<Vec<_>>()
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn incremental_vertex_edge_span_compact_preserves_other_label_bucket() {
        use super::VertexEdgeSpanCompactOneStep;

        let graph = test_graph();
        let anchor = BucketLabelKey::from_raw(99);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), anchor, TestEdge { target: 999 })
            .unwrap();
        for target in 1..=20u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in 1..=17u32 {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let anchor_slot = graph.find_bucket_slot(&vertex, anchor).unwrap().unwrap();
        let road_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        assert_eq!(
            road_bucket.stored_slots.saturating_sub(road_bucket.degree),
            17
        );

        let mut resume = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume)
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => {}
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_)
                | VertexEdgeSpanCompactOneStep::Finished => {
                    break;
                }
            }
        }

        let anchor_bucket = graph.buckets().read_label_bucket_slot(anchor_slot).unwrap();
        assert_eq!(anchor_bucket.stored_slots, anchor_bucket.degree);
        assert_eq!(anchor_bucket.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), anchor)
                .unwrap(),
            vec![TestEdge { target: 999 }]
        );

        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        assert_eq!(road_bucket.stored_slots, road_bucket.degree);
        assert_eq!(road_bucket.degree, 3);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 20 },
                TestEdge { target: 19 },
                TestEdge { target: 18 },
            ]
        );
    }

    #[test]
    fn label_bucket_accumulates_many_slab_tombstones_before_compact() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let total = 202u32;
        for target in 1..=total {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }

        for target in 1..=200 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 200);
        assert_eq!(bucket.degree(), 2);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, bucket.degree);
        assert_eq!(bucket.degree, 2);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 202 }, TestEdge { target: 201 }]
        );
    }

    #[test]
    fn bypass_accumulates_many_slab_tombstones_without_promotion() {
        let graph = test_graph();
        let default = graph.default_label();
        let total = 202u32;
        for target in 1..=total {
            graph
                .insert_edge(VertexId::from(0), default, TestEdge { target })
                .unwrap();
        }

        for target in 1..=200 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), default, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.stored_slots.saturating_sub(vertex.degree), 200);
        assert_eq!(vertex.degree(), 2);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), default)
                .unwrap(),
            vec![TestEdge { target: 202 }, TestEdge { target: 201 }]
        );
    }

    #[test]
    fn empty_bypass_promotes_as_empty_when_next_insert_uses_different_label() {
        let graph = test_graph();
        let default = graph.default_label();
        let road = BucketLabelKey::from_raw(42);

        graph
            .insert_edge(VertexId::from(0), default, TestEdge { target: 10 })
            .unwrap();
        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), default, |edge| edge.target == 10)
                .unwrap()
                .is_some()
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 0);
        assert_eq!(vertex.stored_degree(), 1);

        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 20 })
            .unwrap();

        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            vec![road]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 20 }]
        );
        assert!(
            graph
                .iter_edges_for_label(VertexId::from(0), default)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remove_edge_from_one_label_keeps_next_label_isolated() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let walk = BucketLabelKey::from_raw(3);
        for target in [10, 11] {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in [20, 21] {
            graph
                .insert_edge(VertexId::from(0), walk, TestEdge { target })
                .unwrap();
        }

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 10)
            .unwrap();

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), walk).unwrap(),
            vec![TestEdge { target: 21 }, TestEdge { target: 20 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn empty_middle_label_bucket_does_not_expose_neighbor_edges() {
        let graph = test_graph();
        let low = BucketLabelKey::from_raw(2);
        let middle = BucketLabelKey::from_raw(3);
        let high = BucketLabelKey::from_raw(4);

        graph
            .insert_edge(VertexId::from(0), low, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), middle, TestEdge { target: 20 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), high, TestEdge { target: 30 })
            .unwrap();

        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), middle, |edge| edge.target == 20)
                .unwrap()
                .is_some()
        );

        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), middle)
                .unwrap(),
            Vec::<TestEdge>::new()
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );

        let mut raw_scanned = Vec::new();
        graph
            .visit_out_edges(
                VertexId::from(0),
                None,
                None,
                None,
                |_| true,
                |edge| raw_scanned.push(edge),
            )
            .unwrap();
        assert_eq!(
            raw_scanned,
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let middle_slot = graph.find_bucket_slot(&vertex, middle).unwrap().unwrap();
        let middle_bucket = graph.buckets().read_label_bucket_slot(middle_slot).unwrap();
        let middle_index = middle_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let successor = graph.bucket_successor_start(&vertex, middle_index).unwrap();
        assert_eq!(middle_bucket.stored_slots, 0);
        assert!(successor >= middle_bucket.edge_start());

        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn insert_beyond_initial_label_bucket_store_vertex_segment_relocates_buckets() {
        let graph = test_graph();
        for label in 1..=33u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.degree(), 33);
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap().len(), 33);
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn label_bucket_store_vertex_segment_compacts_across_thirty_two_vertices() {
        let graph = test_graph();
        for _ in 1..32 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 10 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(31),
                BucketLabelKey::from_raw(3),
                TestEdge { target: 20 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(4),
                TestEdge { target: 30 },
            )
            .unwrap();

        let first = graph.vertices().get(VertexId::from(0));
        let last = graph.vertices().get(VertexId::from(31));
        assert_eq!(first.degree(), 2);
        assert_eq!(last.degree(), 1);
        assert!(
            last.base_slot_start()
                >= first.base_slot_start().saturating_add(u64::from(
                    first
                        .label_bucket_descriptor_span()
                        .expect("hub vertex is in bucket mode"),
                ))
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(31)).unwrap(),
            vec![TestEdge { target: 20 }]
        );
    }

    #[test]
    fn insert_beyond_initial_label_edge_span_capacity_relocates_vertex_edge_span() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        for target in 0..128u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        let edges = graph.iter_edges_for_label(VertexId::from(0), road).unwrap();
        assert_eq!(edges.len(), 128);
        assert_eq!(edges[0], TestEdge { target: 127 });
        assert_eq!(edges[127], TestEdge { target: 0 });
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.stored_slots, 128);
        assert!(vertex.stored_slots >= 128);
    }

    #[test]
    fn vertex_edge_span_rewrite_weights_slack_by_label_degree() {
        let graph = test_graph();
        let hot = BucketLabelKey::from_raw(2);
        let cold = BucketLabelKey::from_raw(3);
        for target in 0..64u32 {
            graph
                .insert_edge(VertexId::from(0), hot, TestEdge { target })
                .unwrap();
        }
        graph
            .insert_edge(VertexId::from(0), cold, TestEdge { target: 900 })
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let hot_slot = graph.find_bucket_slot(&vertex, hot).unwrap().unwrap();
        let cold_slot = graph.find_bucket_slot(&vertex, cold).unwrap().unwrap();
        let hot_index = hot_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let cold_index = cold_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let hot_bucket = graph.buckets().read_label_bucket_slot(hot_slot).unwrap();
        let cold_bucket = graph.buckets().read_label_bucket_slot(cold_slot).unwrap();
        let hot_capacity = graph
            .bucket_successor_start(&vertex, hot_index)
            .unwrap()
            .saturating_sub(hot_bucket.edge_start());
        let cold_capacity = graph
            .bucket_successor_start(&vertex, cold_index)
            .unwrap()
            .saturating_sub(cold_bucket.edge_start());

        assert!(hot_capacity > cold_capacity);
        assert!(hot_capacity > u64::from(hot_bucket.stored_slots));
        assert!(cold_capacity >= u64::from(cold_bucket.stored_slots));
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn compact_vertex_edge_span_shrinks_vertex_edge_span() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        for target in 0..80u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in 0..72u32 {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap();
        }

        let before = graph.vertices().get(VertexId::from(0));
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            8
        );
        assert!(before.stored_slots > 8);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert_eq!(after.stored_slots, 9);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            8
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn default_bypass_conversion_clears_vertex_edge_span_allocation() {
        let graph = test_graph();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                VertexId::from(0),
                LabelBucket::default().with_bucket_label_key(graph.default_label()),
            )
            .unwrap();
        for target in [7u32, 8] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    graph.default_label(),
                    TestEdge { target },
                )
                .unwrap();
        }

        let before = graph.vertices().get(VertexId::from(0));
        assert!(!before.is_default_edge_labeled());
        assert_eq!(before.degree(), 1);

        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert!(after.is_default_edge_labeled());
        assert_eq!(after.degree(), 2);
        assert_eq!(after.stored_slots, 2);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 8 }, TestEdge { target: 7 }]
        );
    }

    #[test]
    fn compact_label_bucket_vertex_segment_preserves_rows() {
        let graph = test_graph();
        for label in 1..=6u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                )
                .unwrap();
        }
        let before = graph.vertices().get(VertexId::from(0));
        graph
            .compact_label_bucket_vertex_segment(VertexId::from(0))
            .unwrap();
        let after = graph.vertices().get(VertexId::from(0));
        assert_eq!(after.degree(), before.degree());
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap().len(), 6);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ValuedTestEdge {
        target: u32,
        slot_index: u32,
        value: [u8; 8],
        value_len: u8,
    }

    impl ValuedTestEdge {
        fn with_u16(target: u32, inline: u16) -> Self {
            let mut value = [0u8; 8];
            value[0..2].copy_from_slice(&inline.to_le_bytes());
            Self {
                target,
                slot_index: 0,
                value,
                value_len: 2,
            }
        }
    }

    impl CsrEdge for ValuedTestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self {
                target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                slot_index: 0,
                value: [0u8; 8],
                value_len: 0,
            }
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.target)
        }

        fn with_neighbor_vid(self, vid: VertexId) -> Self {
            Self {
                target: u32::from(vid),
                ..self
            }
        }

        fn with_slot_index(self, slot_index: u32) -> Self {
            Self { slot_index, ..self }
        }

        fn edge_slot_index_raw(&self) -> u32 {
            self.slot_index
        }

        fn edge_value_byte_width(&self) -> u8 {
            self.value_len
        }

        fn edge_value_bytes(&self) -> &[u8] {
            &self.value[..usize::from(self.value_len)]
        }

        fn with_stored_value_bytes(mut self, width: u8, bytes: &[u8]) -> Self {
            self.value = [0u8; 8];
            let len = usize::from(width).min(bytes.len()).min(8);
            self.value[..len].copy_from_slice(&bytes[..len]);
            self.value_len = width;
            self
        }
    }

    impl CsrEdgeTombstone for ValuedTestEdge {
        fn tombstone_edge() -> Self {
            Self {
                target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
                slot_index: 0,
                value: [0u8; 8],
                value_len: 0,
            }
        }
    }

    fn valued_test_graph() -> LabeledLaraGraph<ValuedTestEdge, crate::VectorMemory> {
        valued_test_graph_with_capacity(256)
    }

    fn valued_test_graph_with_capacity(
        elem_capacity: u64,
    ) -> LabeledLaraGraph<ValuedTestEdge, crate::VectorMemory> {
        LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            elem_capacity,
            BucketLabelKey::directed_from_index(1),
        )
        .unwrap()
    }

    #[test]
    fn edge_values_round_trip_via_unchecked_label_iteration() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 1))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(2, 100),
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        if let BucketSearch::Found { bucket, .. } =
            graph.find_bucket(VertexId::from(0), &vertex, road).unwrap()
        {
            let mut raw = vec![0u8; 4];
            graph.values().read_bytes(bucket.value_offset(), &mut raw);
            assert_eq!(u16::from_le_bytes([raw[0], raw[1]]), 1);
            assert_eq!(u16::from_le_bytes([raw[2], raw[3]]), 100);
        }
        let mut edges = Vec::new();
        graph
            .for_each_edges_for_label_unchecked(VertexId::from(0), road, |edge| {
                edges.push(edge);
            })
            .unwrap();
        assert_eq!(edges.len(), 2);
        let mut weights: Vec<u16> = edges
            .iter()
            .filter(|e| e.value_len == 2)
            .map(|e| u16::from_le_bytes([e.value[0], e.value[1]]))
            .collect();
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 100]);
    }

    #[test]
    fn edge_values_survive_middle_vertex_insert() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 1))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(1), road, ValuedTestEdge::with_u16(2, 1))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(2, 100),
            )
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    weights.push(u16::from_le_bytes([edge.value[0], edge.value[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 100]);
    }

    #[test]
    fn edge_values_preserved() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        for (target, weight) in [(1u32, 3u16), (2, 7), (3, 11)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    weights.push(u16::from_le_bytes([edge.value[0], edge.value[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        assert_eq!(weights, vec![3, 7, 11]);
    }

    #[test]
    fn edge_values_survive_unrelated() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 42))
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), rail, ValuedTestEdge::with_u16(2, 0))
            .unwrap();
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    weights.push(u16::from_le_bytes([edge.value[0], edge.value[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![42]);
    }

    #[test]
    fn edge_values_round_trip_when_edge_and_value_use_overflow_log() {
        let graph = valued_test_graph_with_capacity(1 << 16);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        for target in 1..=31u32 {
            let weight = u16::try_from(target.saturating_mul(10)).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(33, 320),
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(33, 330),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        assert!(bucket.value_log_head() >= 0);

        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    weights.push(u16::from_le_bytes([edge.value[0], edge.value[1]]));
                }
            })
            .unwrap();
        weights.sort_unstable();
        let mut expected: Vec<u16> = (1..=31u32)
            .map(|t| u16::try_from(t.saturating_mul(10)).expect("weight fits u16"))
            .collect();
        expected.extend([320, 330]);
        expected.sort_unstable();
        assert_eq!(weights, expected);
    }

    #[test]
    fn releasing_one_bucket_value_span_keeps_other_bucket_value_log() {
        let graph = valued_test_graph_with_capacity(1 << 20);
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            graph
                .ensure_label_bucket_value_width(src, label, ValueWidthCode::W2)
                .unwrap();
        }
        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            for target in 1..=33u32 {
                let weight = if label == road {
                    target
                } else {
                    target.saturating_mul(10)
                };
                graph
                    .insert_edge_skip_leaf_cascade(
                        src,
                        label,
                        ValuedTestEdge::with_u16(target, u16::try_from(weight).unwrap()),
                    )
                    .unwrap();
            }
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let road_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        let rail_vertex = graph.vertices().get(VertexId::from(1));
        let rail_slot = graph.find_bucket_slot(&rail_vertex, rail).unwrap().unwrap();
        let rail_bucket = graph.buckets().read_label_bucket_slot(rail_slot).unwrap();
        assert!(road_bucket.value_log_head() >= 0);
        assert!(rail_bucket.value_log_head() >= 0);
        let leaf = graph.value_log_leaf(VertexId::from(1));
        let rail_head = u32::try_from(rail_bucket.value_log_head()).unwrap();
        let mut before = [0u8; 2];
        graph.values().read_value_log_entry(
            leaf,
            rail_head,
            rail_bucket.value_width(),
            &mut before,
        );

        let road_bucket_log_only = road_bucket.with_degree_field(0).with_stored_slots(0);
        graph
            .release_bucket_value_span(VertexId::from(0), &road_bucket_log_only)
            .unwrap();

        let mut after = [0u8; 2];
        graph
            .values()
            .read_value_log_entry(leaf, rail_head, rail_bucket.value_width(), &mut after);
        assert_ne!(before, [0, 0]);
        assert_eq!(after, before);
    }

    #[test]
    fn valued_default_label_insert_uses_bucket_storage() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let default = graph.default_label();
        graph
            .insert_edge(VertexId::from(0), default, ValuedTestEdge::with_u16(1, 42))
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(
            !vertex.is_default_edge_labeled(),
            "valued default-label edges need value bucket metadata"
        );
        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), default, |edge| {
                if edge.value_len == 2 {
                    weights.push(u16::from_le_bytes([edge.value[0], edge.value[1]]));
                }
            })
            .unwrap();
        assert_eq!(weights, vec![42]);
    }

    #[test]
    fn removing_last_valued_edge_clears_vertex_value_allocation() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 42))
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .value_allocated_bytes(),
            2
        );

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 1)
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.value_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_value_allocated());
        assert_eq!(bucket.value_log_head(), -1);
    }

    #[test]
    fn removing_last_valued_edge_by_slot_clears_vertex_value_allocation() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(1, 42))
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .value_allocated_bytes(),
            2
        );

        let removed = graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed edge");
        assert_eq!(removed.target, 1);

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.value_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_value_allocated());
        assert_eq!(bucket.value_log_head(), -1);
    }

    #[test]
    fn removing_non_last_valued_edge_by_slot_preserves_value_log_head() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        for (target, weight) in [(1, 42), (2, 99)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .unwrap()
            .try_with_value_log_head(0)
            .unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(bucket_slot, bucket)
            .unwrap();

        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed edge");

        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert_eq!(bucket.degree(), 1);
        assert_eq!(bucket.value_log_head(), 0);
    }

    #[test]
    fn valued_insert_reusing_low_tombstone_preserves_existing_values() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        for (target, weight) in [(1, 10), (2, 20), (3, 30)] {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }

        graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed low slot");
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), road, ValuedTestEdge::with_u16(4, 40))
            .unwrap();

        let mut values = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 {
                    values.push((
                        edge.target,
                        u16::from_le_bytes([edge.value[0], edge.value[1]]),
                    ));
                }
            })
            .unwrap();
        values.sort_unstable();
        assert_eq!(values, vec![(2, 20), (3, 30), (4, 40)]);
    }

    #[test]
    fn edge_values_survive_middle_vertex_insert_with_overflow_log() {
        let graph = valued_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_value_width(VertexId::from(0), road, ValueWidthCode::W2)
            .unwrap();
        for target in 1..=32u32 {
            let weight = u16::try_from(target).expect("weight fits u16");
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    ValuedTestEdge::with_u16(target, weight),
                )
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(1), road, ValuedTestEdge::with_u16(2, 2))
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                ValuedTestEdge::with_u16(2, 200),
            )
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        assert!(bucket.value_log_head() >= 0);

        let mut weights = Vec::new();
        graph
            .for_each_edges_for_label(VertexId::from(0), road, |edge| {
                if edge.value_len == 2 && edge.target == 2 {
                    weights.push(u16::from_le_bytes([edge.value[0], edge.value[1]]));
                }
            })
            .unwrap();
        assert!(weights.contains(&200), "newest insert weight: {weights:?}");
    }
}
