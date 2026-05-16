//! Single-orientation labeled LARA graph orchestration.
//!
//! [`LabeledLaraGraph`] mirrors [`crate::LaraGraph`]: it owns the vertex column
//! plus the storage layers required to mutate one CSR orientation. The extra
//! bucket layer is kept small and relocatable. Normal labeled edge bytes live in
//! the regular [`EdgeStore`] slab/free-span store and participate in the same
//! PMA segment [`crate::lara::edge::counts::SegmentEdgeCounts`] accounting as
//! core LARA: each [`LabeledVertex`]'s [`LabeledVertex::vertex_edge_alloc_slots`]
//! contributes `total` while live edges contribute `actual`. A **cascade** from
//! per-label edge span grow/shrink propagates through the owning **VertexEdgeSpan**
//! into per-leaf density checks (compaction then optional slack growth).

use crate::{
    VertexCount, VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_store::LabelBucketStore,
        record::{LABEL_UNDIRECTED_BIT, LabelBucket, LabelId, LabeledVertex},
    },
    lara::{
        edge::{
            EdgeStore, InitError as EdgeInitError,
            counts::{SegmentEdgeCounts, segment_span_density},
            segment_tree_leaf_count,
        },
        operation_error::LaraOperationError,
        vertex::{InitError as VertexInitError, VertexStore},
    },
    traits::{CsrEdge, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::{cell::Cell, cmp::Ordering, fmt, iter::FusedIterator, marker::PhantomData};

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
    label_id: LabelId,
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
        }
    }
}

impl std::error::Error for LabeledOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::VertexOutOfRange { .. } | Self::InvalidDefaultBypass => None,
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
    Buckets(crate::labeled::bucket_store::InitError),
    /// The edge subsystem could not be reopened.
    Edges(EdgeInitError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vertices(e) => write!(f, "vertex init failed: {e}"),
            Self::Buckets(e) => write!(f, "bucket init failed: {e}"),
            Self::Edges(e) => write!(f, "edge init failed: {e}"),
        }
    }
}

impl std::error::Error for InitError {}

/// Single-orientation multi-level labeled CSR graph.
pub struct LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    vertices: VertexStore<LabeledVertex, M>,
    buckets: LabelBucketStore<M>,
    edges: EdgeStore<E, M>,
    default_label: LabelId,
    last_bucket_lookup: Cell<Option<BucketLookupCache>>,
    bucket_lookup_cache: [Cell<Option<BucketLookupCache>>; BUCKET_LOOKUP_CACHE_ENTRIES],
    _marker: PhantomData<E>,
}

/// Iterator over every outgoing edge at `src` across all labels in the **fast scan order** used by
/// [`EdgeStore::iter_out_edges`] on each [`LabelEdgeSpanAccess`]: **label buckets from high sort
/// order to low** (reverse of ascending [`LabelId`] bucket slots), and within each bucket
/// **overflow log chain first**, then **slab slots from high index to low**.
///
/// This order matches [`LabeledLaraGraph::out_edges_iter`], [`LabeledLaraGraph::iter_out_edges`],
/// and [`LabeledLaraGraph::for_each_out_edge_matching_with_raw`].
pub struct LabeledOutEdgesIter<'a, E: CsrEdge, M: Memory> {
    inner: std::vec::IntoIter<E>,
    _marker: PhantomData<&'a M>,
}

impl<'a, E, M> Iterator for LabeledOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

impl<'a, E, M> FusedIterator for LabeledOutEdgesIter<'a, E, M>
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
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, crate::GrowFailed> {
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
            default_label,
            last_bucket_lookup: Cell::new(None),
            bucket_lookup_cache: std::array::from_fn(|_| Cell::new(None)),
            _marker: PhantomData,
        })
    }

    /// Opens a labeled graph from stable memories, creating stores when empty.
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
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, InitError> {
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
            edges: EdgeStore::init(
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
            .map_err(InitError::Edges)?,
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

    /// Returns the LabelBucketStore.
    pub fn buckets(&self) -> &LabelBucketStore<M> {
        &self.buckets
    }

    /// Returns the edge storage used by every label bucket.
    pub fn edges(&self) -> &EdgeStore<E, M> {
        &self.edges
    }

    /// Returns the label used by default-label bypass rows.
    pub fn default_label(&self) -> LabelId {
        self.default_label
    }

    #[inline]
    fn is_homogeneous_bypass_label(&self, label_id: LabelId) -> bool {
        let raw = label_id.raw();
        let default = self.default_label.raw();
        raw == default || raw == default | LABEL_UNDIRECTED_BIT
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
    fn bypass_storage_label_for(&self, vertex: &LabeledVertex) -> LabelId {
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
                self.vertices
                    .set(vid, &successor.with_base_slot_start(region_end));
            }
        }
        Ok(())
    }

    /// Consolidates a logged label bucket back onto its slab span when needed for promotion.
    fn consolidate_single_label_bucket_for_promotion(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.overflow_log_head < 0 {
            return Ok(*bucket);
        }
        self.rewrite_vertex_edge_span(src, Some(0), 0, false, false)?;
        self.buckets
            .read_label_bucket_slot(self.vertices.get(src).base_slot_start())
            .ok_or(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ))
    }

    /// Promotes a **single-label** vertex row into homogeneous bypass (core CSR).
    ///
    /// Invariant: parallel edges under one catalog label must not accumulate in a
    /// [`LabelBucket`] VertexEdgeSpan (slab + segment log). The first edge may use a
    /// bucket; from the second edge onward a tail row can be stored like default-label
    /// bypass with the label id tagged in [`LabeledVertex::vertex_edge_alloc_slots`].
    fn try_promote_single_label_bucket_to_bypass(
        &self,
        src: VertexId,
        label_id: LabelId,
    ) -> Result<(), LabeledOperationError> {
        if self.is_homogeneous_bypass_label(label_id) {
            return Ok(());
        }
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() != 1 {
            return Ok(());
        }
        if !self.may_use_homogeneous_bypass(src) {
            return Ok(());
        }
        let bucket_slot = vertex.base_slot_start();
        let bucket = self
            .buckets
            .read_label_bucket_slot(bucket_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if bucket.label_id != label_id || bucket.edge_len < 1 {
            return Ok(());
        }
        let bucket = self.consolidate_single_label_bucket_for_promotion(src, &bucket)?;
        let old_alloc = vertex.vertex_edge_alloc_slots();
        let region_end = bucket
            .edge_start
            .checked_add(u64::from(bucket.edge_len))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let updated = vertex
            .with_homogeneous_bypass_label(bucket.label_id)
            .with_base_slot_start(bucket.edge_start)
            .with_degree(bucket.edge_len);
        self.buckets
            .clear_vertex_label_buckets(&self.vertices, src)?;
        self.vertices.set(src, &updated);
        self.edges
            .bump_vertex_segment_counts(src, 0, -i64::from(old_alloc))?;
        self.bump_successor_origins_after_bypass_end(src, region_end)
    }

    /// Appends one edge on a homogeneous bypass row using direct slab slots.
    ///
    /// Core [`EdgeStore::insert_edge`] caps the CSR window at `initial_vertex_edge_slots`
    /// inside a PMA leaf, which is correct for log-spill workloads but too small for
    /// catalog-label parallel hubs. Bypass rows own `[base_slot_start, base+degree)`
    /// globally; successors are re-chained after each append.
    fn insert_homogeneous_bypass_edge(
        &self,
        src: VertexId,
        label_id: LabelId,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        debug_assert_eq!(label_id, self.bypass_storage_label_for(&vertex));
        self.ensure_bypass_edge_origin(src)?;
        let vertex = self.vertices.get(src);
        let next_degree = vertex
            .degree()
            .checked_add(1)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let write_slot = vertex
            .base_slot_start()
            .checked_add(u64::from(vertex.degree()))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let write_end = write_slot
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if write_end > self.edges.header().elem_capacity {
            self.edges
                .set_elem_capacity(write_end)
                .map_err(LabeledOperationError::from)?;
        }
        self.edges.write_slot(write_slot, edge)?;
        self.vertices.set(src, &vertex.with_degree(next_degree));
        self.edges.set_num_edges(
            self.edges
                .header()
                .num_edges
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?,
        );
        self.edges
            .bump_vertex_segment_counts(src, 1, 0)
            .map_err(LabeledOperationError::from)?;
        let region_end = self.bypass_region_end(src)?;
        self.bump_successor_origins_after_bypass_end(src, region_end)
    }

    #[inline]
    fn bypass_region_end(&self, src: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        vertex
            .base_slot_start()
            .checked_add(u64::from(vertex.degree()))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Exclusive end of the global edge-slot prefix owned by `vid` (bypass or bucket span).
    fn vertex_prefix_end(&self, vid: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            vertex
                .base_slot_start()
                .checked_add(u64::from(vertex.degree()))
                .ok_or(LaraOperationError::CollectAllocationOverflow.into())
        } else if vertex.degree() == 0 {
            Ok(vertex.base_slot_start())
        } else {
            let first = self
                .buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            first
                .edge_start
                .checked_add(u64::from(vertex.vertex_edge_alloc_slots()))
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
        if vertex.degree() > 0 {
            return Ok(());
        }
        let edge_base = if u32::from(src) == 0 {
            0
        } else {
            let pred_idx = u32::from(src) - 1;
            self.vertex_prefix_end(VertexId::from(pred_idx))?
        };
        if edge_base != vertex.base_slot_start() {
            self.vertices
                .set(src, &vertex.with_base_slot_start(edge_base));
        }
        Ok(())
    }

    fn insert_homogeneous_bypass(
        &self,
        src: VertexId,
        label_id: LabelId,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_bypass_edge_origin(src)?;
        let vertex = self.vertices.get(src);
        self.vertices
            .set(src, &vertex.with_homogeneous_bypass_label(label_id));
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
        let edge_len = vertex.degree();
        if edge_len == 0 {
            self.vertices
                .set(src, &vertex.with_default_edge_labeled(false));
            return Ok(());
        }

        // Bucket collection must not read edge slots while bypass is still active.
        self.vertices.set(src, &LabeledVertex::default());

        let new_alloc = DEFAULT_SEGMENT_SIZE.max(edge_len);
        let _slot = self.buckets.insert_label_bucket_at(
            &self.vertices,
            src,
            LabelBucket {
                label_id: bypass_label,
                edge_start,
                edge_len,
                overflow_log_head: -1,
            },
            0,
        )?;
        let updated = self
            .vertices
            .get(src)
            .with_vertex_edge_alloc_slots(new_alloc);
        self.vertices.set(src, &updated);
        self.edges
            .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))?;
        Ok(())
    }

    /// Returns the number of vertex rows.
    pub fn vertex_count(&self) -> VertexCount {
        VertexCount::from(self.vertices.len())
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
        self.edges
            .counts_store()
            .get(u64::from(leaf.saturating_add(header.segment_count)))
    }

    /// `true` when `vid`'s PMA leaf has `actual/total >= 1.0` under incremental labeled accounting.
    pub(crate) fn labeled_leaf_segment_is_dense(&self, vid: VertexId) -> bool {
        segment_span_density(self.leaf_segment_counts_for_vid(vid))
            >= LEAF_VERTEX_EDGE_SEGMENT_DENSITY
    }

    /// Compacts then optionally grows slack for every normal labeled vertex in `src`'s PMA leaf.
    fn rebalance_cascade_after_labeled_mutation(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, header.segment_size);
        let idx = u64::from(leaf.saturating_add(header.segment_count));
        if segment_span_density(self.edges.counts_store().get(idx))
            < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
        {
            return Ok(());
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_rebalance_leaf_cascade");

        let src_vertex = self.vertices.get(src);
        if !src_vertex.is_default_edge_labeled() && src_vertex.degree() > 0 {
            self.rewrite_vertex_edge_span(src, None, 0, false, true)?;
            if segment_span_density(self.edges.counts_store().get(idx))
                < LEAF_VERTEX_EDGE_SEGMENT_DENSITY
            {
                return Ok(());
            }
        }

        let start_vid = leaf.saturating_mul(seg);
        let end_vid = (start_vid + seg).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            if vid == src {
                continue;
            }
            let v = self.vertices.get(vid);
            if v.is_default_edge_labeled() {
                continue;
            }
            if v.degree() > 0 {
                self.rewrite_vertex_edge_span(vid, None, 0, true, false)?;
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
            let v = self.vertices.get(vid);
            if v.is_default_edge_labeled() {
                continue;
            }
            if v.degree() > 0 {
                self.rewrite_vertex_edge_span(vid, None, 0, false, true)?;
            }
        }
        Ok(())
    }

    fn find_bucket(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        label_id: LabelId,
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
                if cache.label_id == label_id && (start..range_end).contains(&cache.slot) {
                    let bucket = self
                        .buckets
                        .read_label_bucket_slot(cache.slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if bucket.label_id == label_id {
                        return Ok(BucketSearch::Found {
                            slot: cache.slot,
                            bucket,
                        });
                    }
                }
                if let Some(slot_after_cache) = cache.slot.checked_add(1) {
                    if slot_after_cache == range_end && cache.label_id < label_id {
                        let bucket = self
                            .buckets
                            .read_label_bucket_slot(cache.slot)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        if bucket.label_id == cache.label_id {
                            return Ok(BucketSearch::Missing { insert_index: deg });
                        }
                    }
                }
            }
        }
        let cache_index = Self::bucket_lookup_cache_index(src, label_id);
        if let Some(cache) = self.bucket_lookup_cache[cache_index].get() {
            if cache.vid == src
                && cache.label_id == label_id
                && cache.base_slot_start == start
                && cache.degree == deg
            {
                if (start..range_end).contains(&cache.slot) {
                    let bucket = self
                        .buckets
                        .read_label_bucket_slot(cache.slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if bucket.label_id == label_id {
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
            return Ok(match label_id.cmp(&bucket.label_id) {
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
            match label_id.cmp(&b0.label_id) {
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
                    return Ok(match label_id.cmp(&b1.label_id) {
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
                match buckets.binary_search_by_key(&label_id, |bucket| bucket.label_id) {
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
            if bucket.label_id == label_id {
                self.cache_bucket_lookup(src, label_id, vertex, slot);
                return Ok(BucketSearch::Found { slot, bucket });
            }
            if bucket.label_id < label_id {
                lo = mid
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            } else {
                hi = mid;
            }
        }
        Ok(BucketSearch::Missing { insert_index: lo })
    }

    fn cache_bucket_lookup(
        &self,
        src: VertexId,
        label_id: LabelId,
        vertex: &LabeledVertex,
        slot: u64,
    ) {
        let cache = BucketLookupCache {
            vid: src,
            label_id,
            base_slot_start: vertex.base_slot_start(),
            degree: vertex.degree(),
            slot,
        };
        self.last_bucket_lookup.set(Some(cache));
        self.bucket_lookup_cache[Self::bucket_lookup_cache_index(src, label_id)].set(Some(cache));
    }

    fn bucket_lookup_cache_index(src: VertexId, label_id: LabelId) -> usize {
        let mixed = u32::from(src)
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(u32::from(label_id.raw()));
        (mixed as usize) & (BUCKET_LOOKUP_CACHE_ENTRIES - 1)
    }

    #[cfg(test)]
    fn find_bucket_slot(
        &self,
        vertex: &LabeledVertex,
        label_id: LabelId,
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

    fn vertex_label_buckets_have_overflow(&self, vertex: &LabeledVertex) -> bool {
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return false;
        }
        for offset in 0..vertex.degree() {
            let slot = vertex.base_slot_start().saturating_add(u64::from(offset));
            if let Some(bucket) = self.buckets.read_label_bucket_slot(slot)
                && bucket.overflow_log_head >= 0
            {
                return true;
            }
        }
        false
    }

    fn bucket_successor_start(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        let cur_slot = vertex
            .base_slot_start()
            .saturating_add(u64::from(bucket_index));
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
            let next_slot = vertex
                .base_slot_start()
                .saturating_add(u64::from(bucket_index + 1));
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            // Proportional slack placement does not guarantee strictly increasing
            // `edge_start` across bucket slots; CSR slab-window geometry requires a
            // non-decreasing neighbor base, so clamp the successor boundary.
            return Ok(next.edge_start.max(bucket.edge_start));
        }

        if vertex.degree() == 0 {
            return Ok(0);
        }

        let first_edge_start = if bucket_index == 0 {
            bucket.edge_start
        } else {
            self.buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .edge_start
        };
        first_edge_start
            .checked_add(u64::from(vertex.vertex_edge_alloc_slots()))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// True when every bucket is slab-only and its live edges occupy a contiguous
    /// prefix `[edge_start, edge_start + edge_len)`, so bulk slab memcpy is sound.
    fn label_buckets_allow_contiguous_slab_copy(
        &self,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<bool, LabeledOperationError> {
        if buckets.iter().any(|b| b.overflow_log_head >= 0) {
            return Ok(false);
        }
        for (index, bucket) in buckets.iter().enumerate() {
            let successor = buckets
                .get(index.saturating_add(1))
                .map(|next| next.edge_start.max(bucket.edge_start))
                .map(Ok)
                .unwrap_or_else(|| {
                    buckets[0]
                        .edge_start
                        .checked_add(u64::from(vertex.vertex_edge_alloc_slots()))
                        .ok_or_else(|| {
                            LabeledOperationError::from(
                                LaraOperationError::CollectAllocationOverflow,
                            )
                        })
                })?;
            if successor.saturating_sub(bucket.edge_start) < u64::from(bucket.edge_len) {
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
        if buckets.iter().any(|b| b.overflow_log_head >= 0) {
            return None;
        }
        let base = buckets.first()?.edge_start;
        let mut pos = base;
        let mut total_edges: u32 = 0;
        for b in buckets {
            if b.edge_start != pos {
                return None;
            }
            total_edges = total_edges.checked_add(b.edge_len)?;
            pos = pos.checked_add(u64::from(b.edge_len))?;
        }
        let span_end = base.checked_add(u64::from(vertex.vertex_edge_alloc_slots()))?;
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
        let old_alloc = vertex.vertex_edge_alloc_slots();
        let old_base = buckets.first().map(|bucket| bucket.edge_start).unwrap_or(0);
        let mut total_live = 0u32;
        for bucket in &buckets {
            total_live = total_live
                .checked_add(bucket.edge_len)
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
            let end = start
                .checked_add(u64::from(new_alloc))
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

    fn edge_bytes_for_bucket(bucket: &LabelBucket) -> Result<usize, LabeledOperationError> {
        let edge_count = usize::try_from(bucket.edge_len)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        Self::edge_bytes_for_len(edge_count)
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
        // LabelId order without reserving space in the bucket descriptor. Edge
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
                        max_run.max(Self::edge_bytes_for_bucket(bucket)?),
                    )
                })?;
                let mut buf = vec![0u8; max_run];
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = Self::edge_bytes_for_bucket(bucket)?;
                    if run > 0 {
                        self.edges
                            .read_slots_contiguous(bucket.edge_start, &mut buf[..run]);
                        self.edges.write_slots_contiguous(row_start, &buf[..run])?;
                    }
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
                    self.buckets.write_label_bucket_slot(
                        slot,
                        bucket
                            .with_edge_range(row_start, bucket.edge_len)
                            .with_overflow_log_head(-1),
                    )?;
                }
            } else {
                let mut per_bucket: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
                for (index, _) in buckets.iter().enumerate() {
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
                    let bucket_index = index as u32;
                    let successor = self.bucket_successor_start(&vertex, bucket_index)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    per_bucket.push(
                        self.edges
                            .collect_out_edges_slot_order(&acc, VertexId::from(0))
                            .map_err(LabeledOperationError::from)?,
                    );
                }
                let max_run = per_bucket.iter().try_fold(0usize, |max_run, edges| {
                    Ok::<usize, LabeledOperationError>(
                        max_run.max(Self::edge_bytes_for_len(edges.len())?),
                    )
                })?;
                let mut buf = vec![0u8; max_run];
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
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
                    self.buckets.write_label_bucket_slot(
                        slot,
                        bucket
                            .with_edge_range(row_start, el)
                            .with_overflow_log_head(-1),
                    )?;
                }
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
                    let run = Self::edge_bytes_for_bucket(bucket)?;
                    if run > 0 {
                        let end = off
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .read_slots_contiguous(bucket.edge_start, &mut raw[off..end]);
                        off = end;
                    }
                }
                off = 0;
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = Self::edge_bytes_for_bucket(bucket)?;
                    if run > 0 {
                        let end = off
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .write_slots_contiguous(row_start, &raw[off..end])?;
                        off = end;
                    }
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
                    self.buckets.write_label_bucket_slot(
                        slot,
                        bucket
                            .with_edge_range(row_start, bucket.edge_len)
                            .with_overflow_log_head(-1),
                    )?;
                }
            } else {
                let mut per_bucket: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
                for (index, _) in buckets.iter().enumerate() {
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
                    let bucket_index = index as u32;
                    let successor = self.bucket_successor_start(&vertex, bucket_index)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    per_bucket.push(
                        self.edges
                            .collect_out_edges_slot_order(&acc, VertexId::from(0))
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
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
                    self.buckets.write_label_bucket_slot(
                        slot,
                        bucket
                            .with_edge_range(row_start, edges.len() as u32)
                            .with_overflow_log_head(-1),
                    )?;
                }
            }
        } else {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_rewrite_metadata_only");
            for (index, bucket) in buckets.iter().enumerate() {
                let row_start = positions[index];
                let slot = vertex.base_slot_start().saturating_add(index as u64);
                self.buckets.write_label_bucket_slot(
                    slot,
                    bucket.with_edge_range(row_start, bucket.edge_len),
                )?;
            }
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_rewrite_finalize");
        if moved && old_alloc > 0 {
            self.release_vertex_edge_span_slab(old_base, u64::from(old_alloc))?;
        }
        self.vertices
            .set(src, &vertex.with_vertex_edge_alloc_slots(new_alloc));

        let d_total = i64::from(new_alloc).saturating_sub(i64::from(old_alloc));
        if d_total != 0 {
            self.edges
                .bump_vertex_segment_counts(src, 0, d_total)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(())
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
                    .edge_len
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
                    .edge_len
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
    pub fn push_vertex(&self, mut vertex: LabeledVertex) -> Result<VertexId, crate::GrowFailed> {
        let id = self.vertices.len();
        if id > 0 {
            let prev_end = self
                .vertex_prefix_end(VertexId::from(id as u32 - 1))
                .unwrap_or(0);
            if vertex.base_slot_start() < prev_end {
                vertex = vertex.with_base_slot_start(prev_end);
            }
        }
        self.vertices.push(vertex)?;
        let header = self.edges.header();
        let target = segment_tree_leaf_count(self.vertices.len().into(), header.segment_size);
        if target > header.segment_count {
            self.edges.grow_segment_tree_to(target)?;
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
            .map_err(Into::into)
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
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(vid)?;
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        if bucket_index >= vertex.degree() {
            return Ok(());
        }
        self.rewrite_vertex_edge_span(vid, None, 0, true, false)
    }

    /// Inserts one edge under `label_id` at `src`.
    ///
    /// After a successful normal labeled insert, runs an immediate **cascade**
    /// pass only when the owning PMA leaf is already dense (`actual/total ≥ 1`);
    /// sparse leaves skip the leaf-wide compaction / slack-grow scan.
    pub fn insert_edge(
        &self,
        src: VertexId,
        label_id: LabelId,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
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
        label_id: LabelId,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_vertex(src)?;
        self.try_promote_single_label_bucket_to_bypass(src, label_id)?;
        let mut vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id == self.bypass_storage_label_for(&vertex) {
                return self.insert_homogeneous_bypass_edge(src, label_id, edge);
            }
            self.promote_bypass_to_bucket_mode(src)?;
            vertex = self.vertices.get(src);
        } else if vertex.degree() == 0
            && self.is_homogeneous_bypass_label(label_id)
            && self.may_use_homogeneous_bypass(src)
        {
            return self.insert_homogeneous_bypass(src, label_id, edge);
        }

        let (bucket_slot, mut bucket) = self.find_or_create_bucket(src, &vertex, label_id)?;
        let bucket_index = bucket_slot.saturating_sub(vertex.base_slot_start()) as u32;
        for _attempt in 0..64u32 {
            let vertex = self.vertices.get(src);
            let successor_start =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
            if bucket.overflow_log_head < 0
                && successor_start.saturating_sub(bucket.edge_start) > u64::from(bucket.edge_len)
            {
                let new_bucket_len = bucket
                    .edge_len
                    .checked_add(1)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?;
                let write_slot = bucket
                    .edge_start
                    .checked_add(u64::from(bucket.edge_len))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                debug_assert!(write_slot < successor_start);
                self.edges.write_slot(write_slot, edge)?;
                self.buckets.write_label_bucket_slot(
                    bucket_slot,
                    bucket.with_edge_range(bucket.edge_start, new_bucket_len),
                )?;
                let hdr = self.edges.header();
                let next_num_edges = hdr
                    .num_edges
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.edges.set_num_edges(next_num_edges);
                self.edges
                    .bump_vertex_segment_counts(src, 1, 0)
                    .map_err(LabeledOperationError::from)?;
                return Ok(());
            }
            let access = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor_start, src);
            match self.edges.insert_edge(&access, VertexId::from(0), edge) {
                Ok(_) => {
                    return Ok(());
                }
                Err(LaraOperationError::SegmentLogFull) => {
                    self.try_promote_single_label_bucket_to_bypass(src, label_id)?;
                    let vertex = self.vertices.get(src);
                    if vertex.is_default_edge_labeled()
                        && label_id == self.bypass_storage_label_for(&vertex)
                    {
                        return self.insert_homogeneous_bypass_edge(src, label_id, edge);
                    }
                    self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, true)?;
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
        label_id: LabelId,
    ) -> Result<(u64, LabelBucket), LabeledOperationError> {
        let insert_index = match self.find_bucket(src, vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => return Ok((slot, bucket)),
            BucketSearch::Missing { insert_index } => insert_index,
        };
        if insert_index > 0 && self.vertex_label_buckets_have_overflow(vertex) {
            self.rewrite_vertex_edge_span(src, None, 0, false, false)?;
        }
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_insert_new_label_bucket");
        let slot = self
            .buckets
            .insert_label_bucket_at(
                &self.vertices,
                src,
                LabelBucket {
                    label_id,
                    ..LabelBucket::default()
                },
                insert_index,
            )
            .map_err(LabeledOperationError::from)?;
        let vertex = self.vertices.get(src);
        let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
        if !self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
            let vertex = self.vertices.get(src);
            if self.vertex_label_buckets_have_overflow(&vertex) {
                self.rewrite_vertex_edge_span(src, None, 0, false, false)?;
                let vertex = self.vertices.get(src);
                if !self.try_place_new_bucket_edge_span(src, &vertex, slot, bucket_index)? {
                    self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, false)?;
                }
            } else {
                self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, false)?;
            }
        }
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.cache_bucket_lookup(src, label_id, &vertex, slot);
        Ok((slot, bucket))
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
            self.vertices
                .set(src, &vertex.with_vertex_edge_alloc_slots(new_alloc));
            self.edges
                .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))
                .map_err(LabeledOperationError::from)?;
            return Ok(true);
        }

        if bucket_index + 1 != vertex.degree() {
            return Ok(false);
        }
        let prev_slot = slot.saturating_sub(1);
        let prev = self
            .buckets
            .read_label_bucket_slot(prev_slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if prev.overflow_log_head >= 0 {
            return Ok(false);
        }
        if prev.edge_len > DEFAULT_SEGMENT_SIZE {
            return Ok(false);
        }
        let first = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_end = first
            .edge_start
            .saturating_add(u64::from(vertex.vertex_edge_alloc_slots()));
        let edge_start = prev.edge_start.saturating_add(u64::from(prev.edge_len));
        if span_end.saturating_sub(edge_start) == 0 {
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
            if bucket.overflow_log_head >= 0 {
                self.rewrite_vertex_edge_span(src, Some(0), 0, false, false)?;
                bucket = self
                    .buckets
                    .read_label_bucket_slot(vertex.base_slot_start())
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
            let old_alloc = vertex.vertex_edge_alloc_slots();
            let updated = vertex
                .with_default_edge_labeled(true)
                .with_bypass_undirected(bucket.label_id.raw() & LABEL_UNDIRECTED_BIT != 0)
                .with_base_slot_start(bucket.edge_start)
                .with_degree(bucket.edge_len)
                .with_vertex_edge_alloc_slots(0);
            self.buckets
                .clear_vertex_label_buckets(&self.vertices, src)?;
            self.vertices.set(src, &updated);
            self.edges
                .bump_vertex_segment_counts(src, 0, -i64::from(old_alloc))?;
        } else {
            self.vertices.set(
                src,
                &vertex.with_homogeneous_bypass_label(self.default_label),
            );
        }
        Ok(())
    }

    /// Visits outgoing edges for `label_id` without materializing the full bucket row.
    pub fn for_each_edges_for_label<Visit>(
        &self,
        src: VertexId,
        label_id: LabelId,
        visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(());
            }
            return self
                .edges
                .for_each_out_edge_matching(
                    &self.vertices,
                    src,
                    None::<&mut dyn FnMut(&[u8]) -> bool>,
                    |_| true,
                    visit,
                )
                .map_err(Into::into);
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                #[cfg(feature = "canbench")]
                let _bench_scope = bench_scope("labeled_for_each_edges_for_label");
                let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
                self.edges
                    .for_each_out_edge_matching(
                        &LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src),
                        VertexId::from(0),
                        None::<&mut dyn FnMut(&[u8]) -> bool>,
                        |_| true,
                        visit,
                    )
                    .map_err(Into::into)
            }
            BucketSearch::Missing { .. } => Ok(()),
        }
    }

    /// Like [`Self::for_each_edges_for_label`], but skips [`Self::ensure_vertex`].
    ///
    /// Caller must guarantee `src` is in range: `u32::from(src) < self.vertices.len()`. Correct
    /// shortest-path / BFS traversals satisfy this when `src` is only taken from graph neighbors.
    pub fn for_each_edges_for_label_unchecked<Visit>(
        &self,
        src: VertexId,
        label_id: LabelId,
        visit: Visit,
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
                .for_each_out_edge_matching(
                    &self.vertices,
                    src,
                    None::<&mut dyn FnMut(&[u8]) -> bool>,
                    |_| true,
                    visit,
                )
                .map_err(Into::into);
        }
        match self.find_bucket(src, &vertex, label_id)? {
            BucketSearch::Found { slot, bucket } => {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _bench_scope = bench_scope("labeled_unchecked_find_bucket");
                let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
                let successor_start =
                    self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _walk = bench_scope("labeled_unchecked_bucket_slab_walk");
                self.edges
                    .for_each_out_edge_matching(
                        &LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src),
                        VertexId::from(0),
                        None::<&mut dyn FnMut(&[u8]) -> bool>,
                        |_| true,
                        visit,
                    )
                    .map_err(Into::into)
            }
            BucketSearch::Missing { .. } => Ok(()),
        }
    }

    /// Like [`EdgeStore::for_each_out_edge_matching`], but spans every forward label bucket at
    /// `src` (or the homogeneous bypass slab when in default-label mode).
    ///
    /// Visitation follows [`Self::out_edges_iter`] order (reverse label-bucket walk with
    /// [`EdgeStore::iter_out_edges`] semantics per span). Used by query expansion with optional
    /// slab-byte prefiltering.
    pub fn for_each_out_edge_matching_with_raw<Match, Visit>(
        &self,
        src: VertexId,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LabeledOperationError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            let raw_arg: Option<&mut dyn FnMut(&[u8]) -> bool> = match &mut raw_matches {
                Some(r) => Some(&mut **r),
                None => None,
            };
            return self
                .edges
                .for_each_out_edge_matching(&self.vertices, src, raw_arg, matches, visit)
                .map_err(Into::into);
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets)
        {
            if total_edges == 0 {
                return Ok(());
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
                if bucket.edge_len == 0 {
                    bucket_rev_idx -= 1;
                    slot_rev = None;
                    continue;
                }
                let slot = slot_rev.unwrap_or(bucket.edge_len - 1);
                let rel = bucket
                    .edge_start
                    .saturating_sub(base)
                    .saturating_add(u64::from(slot));
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
                if let Some(raw_m) = raw_matches.as_mut() {
                    if raw_m(chunk) {
                        visit(E::read_from(chunk));
                    }
                } else {
                    let edge = E::read_from(chunk);
                    if matches(&edge) {
                        visit(edge);
                    }
                }
                if slot == 0 {
                    bucket_rev_idx -= 1;
                    slot_rev = None;
                } else {
                    slot_rev = Some(slot - 1);
                }
            }
            return Ok(());
        }
        for bucket_index in (0..buckets.len()).rev() {
            let bucket_index = bucket_index as u32;
            let bucket = &buckets[bucket_index as usize];
            let slot = vertex
                .base_slot_start()
                .saturating_add(u64::from(bucket_index));
            let successor_start =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
            let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src);
            let raw_arg: Option<&mut dyn FnMut(&[u8]) -> bool> = match &mut raw_matches {
                Some(r) => Some(&mut **r),
                None => None,
            };
            self.edges.for_each_out_edge_matching(
                &acc,
                VertexId::from(0),
                raw_arg,
                |edge| matches(edge),
                |edge| visit(edge),
            )?;
        }
        Ok(())
    }

    /// Iterates all outgoing edges for one label without per-edge label checks.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: LabelId,
    ) -> Result<Vec<E>, LabeledOperationError> {
        let mut out = Vec::new();
        self.for_each_edges_for_label(src, label_id, |edge| out.push(edge))?;
        Ok(out)
    }

    /// All outgoing edges across every label bucket (same order as [`Self::out_edges_iter`]:
    /// reverse label-bucket walk, [`EdgeStore::iter_out_edges`] per span).
    pub fn iter_out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError> {
        self.out_edges_iter(src).map(|iter| iter.collect())
    }

    /// Iterator over a checked snapshot: reverse label-bucket walk; within each bucket,
    /// log chain then slab high-to-low ([`EdgeStore::iter_out_edges`] on
    /// [`LabelEdgeSpanAccess`]).
    pub fn out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        let mut edges = Vec::new();
        if vertex.is_default_edge_labeled() {
            edges.extend(self.edges.iter_out_edges(&self.vertices, src)?);
            return Ok(LabeledOutEdgesIter {
                inner: edges.into_iter(),
                _marker: PhantomData,
            });
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some((base, total_edges)) =
            Self::try_contiguous_tiled_labeled_out_edges(&vertex, &buckets)
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_out_edges_iter_bulk_tiled");
            if total_edges > 0 {
                let nbytes = (total_edges as usize)
                    .checked_mul(E::BYTES)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let mut raw = vec![0u8; nbytes];
                self.edges.read_slots_contiguous(base, &mut raw);
                let mut bucket_rev_idx = buckets.len() as isize - 1;
                let mut slot_rev: Option<u32> = None;
                while bucket_rev_idx >= 0 {
                    let bidx = bucket_rev_idx as usize;
                    let bucket = &buckets[bidx];
                    if bucket.edge_len == 0 {
                        bucket_rev_idx -= 1;
                        slot_rev = None;
                        continue;
                    }
                    let slot = slot_rev.unwrap_or(bucket.edge_len - 1);
                    let rel = bucket
                        .edge_start
                        .saturating_sub(base)
                        .saturating_add(u64::from(slot));
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
                    edges.push(E::read_from(&raw[byte_off..byte_end]));
                    if slot == 0 {
                        bucket_rev_idx -= 1;
                        slot_rev = None;
                    } else {
                        slot_rev = Some(slot - 1);
                    }
                }
            }
            return Ok(LabeledOutEdgesIter {
                inner: edges.into_iter(),
                _marker: PhantomData,
            });
        }
        for bucket_index in (0..buckets.len()).rev() {
            let bucket_index = bucket_index as u32;
            let bucket = &buckets[bucket_index as usize];
            let slot = vertex
                .base_slot_start()
                .saturating_add(u64::from(bucket_index));
            let successor =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, bucket)?;
            let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
            edges.extend(self.edges.iter_out_edges(&acc, VertexId::from(0))?);
        }
        Ok(LabeledOutEdgesIter {
            inner: edges.into_iter(),
            _marker: PhantomData,
        })
    }

    /// Explicit alias for [`Self::out_edges_iter`].
    pub fn out_edges_rev_iter(
        &self,
        src: VertexId,
    ) -> Result<LabeledOutEdgesIter<'_, E, M>, LabeledOperationError> {
        self.out_edges_iter(src)
    }

    /// Returns the label id of the bucket that contains `needle`, if any.
    ///
    /// Scans buckets in ascending [`LabelBucket::edge_len`] so small selective buckets
    /// are checked before large noise buckets on skewed hubs.
    pub fn find_edge_label(
        &self,
        src: VertexId,
        needle: &E,
    ) -> Result<Option<LabelId>, LabeledOperationError>
    where
        E: PartialEq,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(None);
            }
            let mut found = false;
            self.edges.for_each_out_edge_matching(
                &self.vertices,
                src,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |_| true,
                |edge| {
                    if &edge == needle {
                        found = true;
                    }
                },
            )?;
            return Ok(found.then(|| self.bypass_storage_label_for(&vertex)));
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(None);
        }
        let start = vertex.base_slot_start();
        let mut buckets = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = start.saturating_add(u64::from(offset));
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            buckets.push((slot, offset, bucket));
        }
        buckets.sort_by_key(|(_, _, bucket)| bucket.edge_len);
        for (slot, bucket_index, bucket) in buckets {
            let successor =
                self.bucket_successor_start_after_bucket(&vertex, bucket_index, &bucket)?;
            let found_label = Cell::new(None);
            self.edges.for_each_out_edge_matching(
                &LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src),
                VertexId::from(0),
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                |_| found_label.get().is_none(),
                |edge| {
                    if &edge == needle {
                        found_label.set(Some(bucket.label_id));
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
    pub fn out_edge_label_ids(&self, src: VertexId) -> Result<Vec<LabelId>, LabeledOperationError> {
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
        let start = vertex.base_slot_start();
        let mut out = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = start.saturating_add(u64::from(offset));
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            out.push(bucket.label_id);
        }
        Ok(out)
    }

    /// Removes the first edge that satisfies `matches`.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: LabelId,
        matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
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
        label_id: LabelId,
        mut matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(None);
            }
            return self
                .edges
                .remove_edge_unordered_matching(&self.vertices, src, matches)
                .map_err(Into::into);
        }
        if let BucketSearch::Found { slot, mut bucket } =
            self.find_bucket(src, &vertex, label_id)?
        {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_remove_edge_skip_leaf");
            let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
            if bucket.edge_len == 0 {
                return Ok(None);
            }
            if bucket.overflow_log_head >= 0 {
                self.rewrite_vertex_edge_span(src, Some(bucket_index), 0, false, false)?;
                bucket = self
                    .buckets
                    .read_label_bucket_slot(slot)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
            let mut found = None;
            for offset in 0..bucket.edge_len {
                let edge_slot = bucket
                    .edge_start
                    .checked_add(u64::from(offset))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(edge_slot);
                if matches(&edge) {
                    found = Some((offset, edge));
                    break;
                }
            }
            let Some((local_index, removed)) = found else {
                return Ok(None);
            };
            let last_index = bucket.edge_len - 1;
            if local_index != last_index {
                let last_slot = bucket
                    .edge_start
                    .checked_add(u64::from(last_index))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let rm_slot = bucket
                    .edge_start
                    .checked_add(u64::from(local_index))
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let last = self.edges.read_slot(last_slot);
                self.edges.write_slot(rm_slot, last)?;
            }
            self.buckets.write_label_bucket_slot(
                slot,
                bucket
                    .with_edge_range(bucket.edge_start, last_index)
                    .with_overflow_log_head(-1),
            )?;
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
    use crate::{VertexId, test_support::vector_memory, traits::CsrEdge};

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

    fn mem() -> crate::VectorMemory {
        vector_memory()
    }

    fn test_graph_with_default(
        default_label: LabelId,
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
            256,
            default_label,
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
    }

    fn test_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
        test_graph_with_default(LabelId::from_raw(1))
    }

    #[test]
    fn homogeneous_bypass_append_extends_edge_capacity() {
        let default = LabelId::from_raw(7);
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
        let default = LabelId::from_raw(7);
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
    fn homogeneous_bypass_region_end_rejects_slot_overflow() {
        let default = LabelId::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_base_slot_start(u64::MAX)
                .with_degree(1),
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
                LabelBucket {
                    label_id: LabelId::from_raw(42),
                    edge_start: u64::MAX,
                    edge_len: 1,
                    ..LabelBucket::default()
                },
            )
            .unwrap();
        graph.vertices().set(
            hub,
            &graph.vertices().get(hub).with_vertex_edge_alloc_slots(1),
        );

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
        let label = LabelId::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket {
                    label_id: label,
                    edge_start: u64::MAX,
                    ..LabelBucket::default()
                },
            )
            .unwrap();
        graph.vertices().set(
            hub,
            &graph.vertices().get(hub).with_vertex_edge_alloc_slots(1),
        );

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
            .with_vertex_edge_alloc_slots(1);
        let buckets = [LabelBucket {
            label_id: LabelId::from_raw(42),
            edge_start: u64::MAX,
            edge_len: 0,
            ..LabelBucket::default()
        }];

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
        let label = LabelId::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket {
                    label_id: label,
                    edge_len: u32::MAX,
                    ..LabelBucket::default()
                },
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
        assert_eq!(bucket.edge_len, u32::MAX);
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
                LabelBucket {
                    label_id: LabelId::from_raw(10),
                    edge_len: u32::MAX,
                    ..LabelBucket::default()
                },
            )
            .unwrap();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket {
                    label_id: LabelId::from_raw(20),
                    edge_len: 1,
                    ..LabelBucket::default()
                },
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
                &[LabelBucket {
                    label_id: LabelId::from_raw(10),
                    edge_len: 2,
                    ..LabelBucket::default()
                }],
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
            1 << 20,
            LabelId::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let a = LabelId::from_raw(10_000);
        let b = LabelId::from_raw(10_001);
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
            1 << 20,
            LabelId::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..20u16 {
            let label = LabelId::from_raw(10_000 + label_idx);
            for edge_i in 0..500u32 {
                graph
                    .insert_edge(
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
            1 << 20,
            LabelId::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..10u16 {
            let label = LabelId::from_raw(1000 + label_idx);
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
        let graph = test_graph_with_default(LabelId::from_raw(1));
        for _ in 1..33 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let high = VertexId::from(32);
        graph
            .insert_edge(high, LabelId::from_raw(2), TestEdge { target: 0 })
            .unwrap();
        assert!(graph.edges().header().segment_count >= 2);
    }

    #[test]
    fn labeled_insert_and_iter_by_label() {
        let graph = test_graph();
        let road = LabelId::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        let walk = LabelId::from_raw(3);
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 11 }]
        );
        assert_eq!(
            graph.iter_out_edges(VertexId::from(0)).unwrap(),
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
    fn out_edges_iter_matches_iter_out_edges() {
        let graph = test_graph();
        let road = LabelId::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();
        let walk = LabelId::from_raw(3);
        graph
            .insert_edge(VertexId::from(0), walk, TestEdge { target: 20 })
            .unwrap();

        let expected = graph.iter_out_edges(VertexId::from(0)).unwrap();
        let lazy: Vec<_> = graph.out_edges_iter(VertexId::from(0)).unwrap().collect();
        assert_eq!(lazy, expected);
    }

    #[test]
    fn normal_labeled_edges_update_pma_leaf_segment_counts() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                LabelId::from_raw(2),
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
                LabelId::from_raw(2),
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
                LabelId::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let label = LabelId::from_raw(2);
        let mut alloc = graph
            .vertices()
            .get(VertexId::from(0))
            .vertex_edge_alloc_slots();
        let mut grew = false;
        for target in 0..512u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
            let next = graph
                .vertices()
                .get(VertexId::from(0))
                .vertex_edge_alloc_slots();
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
                    LabelId::from_raw(label),
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
                    .label_id
                    .raw()
            })
            .collect();
        assert_eq!(labels, vec![2, 7, 10]);
        assert_eq!(
            graph.iter_out_edges(VertexId::from(0)).unwrap(),
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
    fn find_bucket_on_homogeneous_bypass_vertex_does_not_touch_bucket_store() {
        let graph = test_graph();
        let label = LabelId::from_raw(42);
        for target in 0..8u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(
            graph
                .find_bucket_slot(&vertex, label)
                .unwrap()
                .ok_or(LabeledOperationError::Store(
                    LaraOperationError::CollectAllocationOverflow
                ))
                .ok(),
            None
        );
    }

    #[test]
    fn parallel_catalog_edges_on_high_index_vertex_stay_on_slab() {
        let graph = test_graph();
        for _ in 0..64 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = LabelId::from_raw(42);
        for target in 0..240u32 {
            graph.insert_edge(hub, label, TestEdge { target }).unwrap();
        }
        let vertex = graph.vertices().get(hub);
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 240);
        assert_eq!(graph.iter_edges_for_label(hub, label).unwrap().len(), 240);
    }

    #[test]
    fn catalog_label_parallel_inserts_promote_to_bypass_on_second_edge() {
        let graph = test_graph();
        let road = LabelId::from_raw(42);
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
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 24);
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
            1 << 16,
            LabelId::from_raw(1),
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = LabelId::from_raw(42);
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

        let road = LabelId::from_raw(2);
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
            vec![TestEdge { target: 10 }, TestEdge { target: 11 }]
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

        let road = LabelId::from_raw(2);
        let walk = LabelId::from_raw(3);
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

        let catalog = LabelId::from_raw(42);
        for target in [200, 201] {
            graph
                .insert_edge(catalog_tail, catalog, TestEdge { target })
                .unwrap();
        }

        for (src, label) in [
            (VertexId::from(0), road),
            (VertexId::from(0), walk),
            (VertexId::from(0), LabelId::from_raw(999)),
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
        let low = LabelId::from_raw(10);
        let old_tail = LabelId::from_raw(20);
        let inserted = LabelId::from_raw(30);
        let new_tail = LabelId::from_raw(40);

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
            .write_label_bucket_slot(
                tail_slot,
                LabelBucket {
                    label_id: new_tail,
                    ..tail_bucket
                },
            )
            .unwrap();

        graph
            .insert_edge(VertexId::from(0), inserted, TestEdge { target: 30 })
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
        let labels: Vec<_> = buckets.iter().map(|bucket| bucket.label_id).collect();
        assert_eq!(labels, vec![low, inserted, new_tail]);
    }

    #[test]
    fn homogeneous_undirected_bypass_and_promotion_on_named_label() {
        let graph = test_graph_with_default(LabelId::from_raw(0));
        let undirected = LabelId::from_raw(LABEL_UNDIRECTED_BIT);
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
            vec![TestEdge { target: 1 }, TestEdge { target: 2 }]
        );

        let road = LabelId::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 99 })
            .unwrap();
        let after = graph.vertices().get(VertexId::from(0));
        assert!(!after.is_default_edge_labeled());
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), undirected)
                .unwrap(),
            vec![TestEdge { target: 1 }, TestEdge { target: 2 }]
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
    fn remove_edge_uses_unordered_swap_remove() {
        let graph = test_graph();
        let road = LabelId::from_raw(2);
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
            vec![TestEdge { target: 10 }, TestEdge { target: 12 }]
        );
    }

    #[test]
    fn remove_edge_from_one_label_keeps_next_label_isolated() {
        let graph = test_graph();
        let road = LabelId::from_raw(2);
        let walk = LabelId::from_raw(3);
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
            vec![TestEdge { target: 20 }, TestEdge { target: 21 }]
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
        let low = LabelId::from_raw(2);
        let middle = LabelId::from_raw(3);
        let high = LabelId::from_raw(4);

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
            graph.iter_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );

        let mut raw_scanned = Vec::new();
        graph
            .for_each_out_edge_matching_with_raw(
                VertexId::from(0),
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
        assert_eq!(middle_bucket.edge_len, 0);
        assert!(successor >= middle_bucket.edge_start);

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
                    LabelId::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.degree(), 33);
        assert_eq!(graph.iter_out_edges(VertexId::from(0)).unwrap().len(), 33);
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
                LabelId::from_raw(2),
                TestEdge { target: 10 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(31),
                LabelId::from_raw(3),
                TestEdge { target: 20 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                LabelId::from_raw(4),
                TestEdge { target: 30 },
            )
            .unwrap();

        let first = graph.vertices().get(VertexId::from(0));
        let last = graph.vertices().get(VertexId::from(31));
        assert_eq!(first.degree(), 2);
        assert_eq!(last.degree(), 1);
        assert!(
            last.base_slot_start()
                >= first
                    .base_slot_start()
                    .saturating_add(u64::from(first.bucket_alloc_slots()))
        );
        assert_eq!(
            graph.iter_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph.iter_out_edges(VertexId::from(31)).unwrap(),
            vec![TestEdge { target: 20 }]
        );
    }

    #[test]
    fn insert_beyond_initial_label_edge_span_capacity_relocates_vertex_edge_span() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                LabelId::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let road = LabelId::from_raw(2);
        for target in 0..128u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        let edges = graph.iter_edges_for_label(VertexId::from(0), road).unwrap();
        assert_eq!(edges.len(), 128);
        assert_eq!(edges[0], TestEdge { target: 0 });
        assert_eq!(edges[127], TestEdge { target: 127 });
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.edge_len, 128);
        assert!(vertex.vertex_edge_alloc_slots() >= 128);
    }

    #[test]
    fn vertex_edge_span_rewrite_weights_slack_by_label_degree() {
        let graph = test_graph();
        let hot = LabelId::from_raw(2);
        let cold = LabelId::from_raw(3);
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
            .saturating_sub(hot_bucket.edge_start);
        let cold_capacity = graph
            .bucket_successor_start(&vertex, cold_index)
            .unwrap()
            .saturating_sub(cold_bucket.edge_start);

        assert!(hot_capacity > cold_capacity);
        assert!(hot_capacity > u64::from(hot_bucket.edge_len));
        assert!(cold_capacity >= u64::from(cold_bucket.edge_len));
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
                LabelId::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let road = LabelId::from_raw(2);
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
        assert!(before.vertex_edge_alloc_slots() > 8);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert_eq!(after.vertex_edge_alloc_slots(), 9);
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
                LabelBucket {
                    label_id: graph.default_label(),
                    ..LabelBucket::default()
                },
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
        assert_eq!(after.vertex_edge_alloc_slots(), 0);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 7 }, TestEdge { target: 8 }]
        );
    }

    #[test]
    fn compact_label_bucket_vertex_segment_preserves_rows() {
        let graph = test_graph();
        for label in 1..=6u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    LabelId::from_raw(label),
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
        assert_eq!(graph.iter_out_edges(VertexId::from(0)).unwrap().len(), 6);
    }
}
