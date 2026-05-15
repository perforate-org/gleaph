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
        record::{LabelBucket, LabelId, LabeledVertex},
    },
    lara::{
        edge::{
            EdgeStore, InitError as EdgeInitError,
            counts::{SegmentEdgeCounts, segment_span_density},
        },
        operation_error::LaraOperationError,
        vertex::{InitError as VertexInitError, VertexStore},
    },
    traits::{CsrEdge, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::{cmp::Ordering, fmt, marker::PhantomData, ops::Range};

const DEFAULT_SEGMENT_SIZE: u32 = 32;

/// Same threshold as core LARA leaf density (`actual/total` on one PMA leaf).
const LEAF_VERTEX_EDGE_SEGMENT_DENSITY: f64 = 1.0;

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
    _marker: PhantomData<E>,
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

        let start_vid = leaf.saturating_mul(seg);
        let end_vid = (start_vid + seg).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
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

    fn bucket_range(vertex: &LabeledVertex) -> Range<u64> {
        let start = vertex.base_slot_start();
        start..start.saturating_add(u64::from(vertex.degree()))
    }

    fn find_bucket_slot(
        &self,
        vertex: &LabeledVertex,
        label_id: LabelId,
    ) -> Result<Option<u64>, LabeledOperationError> {
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(None);
        }
        let start = vertex.base_slot_start();
        // Fast paths: avoid binary search + canbench scope overhead on tiny degree.
        if deg == 1 {
            let bucket = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            return Ok((bucket.label_id == label_id).then_some(start));
        }
        if deg == 2 {
            let b0 = self
                .buckets
                .read_label_bucket_slot(start)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            match label_id.cmp(&b0.label_id) {
                Ordering::Less => return Ok(None),
                Ordering::Equal => return Ok(Some(start)),
                Ordering::Greater => {
                    let slot1 = start.saturating_add(1);
                    let b1 = self
                        .buckets
                        .read_label_bucket_slot(slot1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    return Ok((label_id == b1.label_id).then_some(slot1));
                }
            }
        }

        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_find_bucket_slot");

        let mut lo = 0u32;
        let mut hi = deg;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let slot = start.saturating_add(u64::from(mid));
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.label_id == label_id {
                return Ok(Some(slot));
            }
            if bucket.label_id < label_id {
                lo = mid.saturating_add(1);
            } else {
                hi = mid;
            }
        }
        Ok(None)
    }

    fn read_vertex_label_buckets(
        &self,
        vertex: &LabeledVertex,
    ) -> Result<Vec<LabelBucket>, LabeledOperationError> {
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

    fn bucket_successor_start(
        &self,
        vertex: &LabeledVertex,
        bucket_index: u32,
    ) -> Result<u64, LabeledOperationError> {
        if bucket_index + 1 < vertex.degree() {
            let cur_slot = vertex
                .base_slot_start()
                .saturating_add(u64::from(bucket_index));
            let next_slot = vertex
                .base_slot_start()
                .saturating_add(u64::from(bucket_index + 1));
            let cur = self
                .buckets
                .read_label_bucket_slot(cur_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let next = self
                .buckets
                .read_label_bucket_slot(next_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            // Proportional slack placement does not guarantee strictly increasing
            // `edge_start` across bucket slots; CSR slab-window geometry requires a
            // non-decreasing neighbor base, so clamp the successor boundary.
            return Ok(next.edge_start.max(cur.edge_start));
        }

        if vertex.degree() == 0 {
            return Ok(0);
        }

        let first = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok(first
            .edge_start
            .saturating_add(u64::from(vertex.vertex_edge_alloc_slots())))
    }

    /// True when every bucket is slab-only and its live edges occupy a contiguous
    /// prefix `[edge_start, edge_start + edge_len)`, so bulk slab memcpy is sound.
    fn label_buckets_allow_contiguous_slab_copy(
        &self,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
    ) -> bool {
        if buckets.iter().any(|b| b.overflow_log_head >= 0) {
            return false;
        }
        for (index, bucket) in buckets.iter().enumerate() {
            let Ok(successor) = self.bucket_successor_start(vertex, index as u32) else {
                return false;
            };
            if successor.saturating_sub(bucket.edge_start) < u64::from(bucket.edge_len) {
                return false;
            }
        }
        true
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
            total_live = total_live.saturating_add(bucket.edge_len);
        }

        let min_required = total_live.saturating_add(preferred_extra);
        let new_alloc = if compact {
            total_live
        } else if force_slack_grow && old_alloc >= min_required && old_alloc > 0 {
            old_alloc
                .saturating_mul(2)
                .max(old_alloc.saturating_add(DEFAULT_SEGMENT_SIZE))
                .max(min_required)
        } else if old_alloc >= min_required && old_alloc > 0 {
            old_alloc
        } else {
            min_required
                .max(DEFAULT_SEGMENT_SIZE)
                .max(old_alloc.saturating_mul(2))
        };

        let moved = old_alloc == 0 || new_alloc > old_alloc || compact;
        let new_base = if new_alloc == 0 {
            0
        } else if moved {
            self.edges.allocate_span(u64::from(new_alloc))?
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
        );
        Ok((
            buckets, old_alloc, old_base, total_live, new_alloc, moved, positions,
        ))
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

        let slab_only_bulk = self.label_buckets_allow_contiguous_slab_copy(&vertex, &buckets);

        let disjoint_copy = moved && old_alloc > 0;
        if disjoint_copy {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_rewrite_copy_disjoint");
            if slab_only_bulk {
                let max_run = buckets
                    .iter()
                    .map(|b| {
                        usize::try_from(b.edge_len)
                            .unwrap_or(usize::MAX)
                            .saturating_mul(E::BYTES)
                    })
                    .max()
                    .unwrap_or(0);
                let mut buf = vec![0u8; max_run];
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = usize::try_from(bucket.edge_len)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(E::BYTES);
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
                let max_run = per_bucket
                    .iter()
                    .map(|edges| edges.len().saturating_mul(E::BYTES))
                    .max()
                    .unwrap_or(0);
                let mut buf = vec![0u8; max_run];
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let slot = vertex.base_slot_start().saturating_add(index as u64);
                    let edges = &per_bucket[index];
                    let el = edges.len() as u32;
                    if !edges.is_empty() {
                        let run = edges.len().saturating_mul(E::BYTES);
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
                let run_total = usize::try_from(total_live)
                    .unwrap_or(usize::MAX)
                    .saturating_mul(E::BYTES);
                let mut raw = vec![0u8; run_total];
                let mut off = 0usize;
                for bucket in &buckets {
                    let run = usize::try_from(bucket.edge_len)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(E::BYTES);
                    if run > 0 {
                        self.edges.read_slots_contiguous(
                            bucket.edge_start,
                            &mut raw[off..off.saturating_add(run)],
                        );
                        off = off.saturating_add(run);
                    }
                }
                off = 0;
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = usize::try_from(bucket.edge_len)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(E::BYTES);
                    if run > 0 {
                        self.edges.write_slots_contiguous(
                            row_start,
                            &raw[off..off.saturating_add(run)],
                        )?;
                        off = off.saturating_add(run);
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
                let run_total: usize = per_bucket
                    .iter()
                    .map(|v| v.len().saturating_mul(E::BYTES))
                    .sum();
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
                    let run = edges.len().saturating_mul(E::BYTES);
                    if run > 0 {
                        self.edges.write_slots_contiguous(
                            row_start,
                            &raw[pack..pack.saturating_add(run)],
                        )?;
                        pack = pack.saturating_add(run);
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
            self.edges.release_span(old_base, u64::from(old_alloc))?;
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
    ) -> Vec<u64> {
        let mut out = Vec::with_capacity(buckets.len());
        if buckets.is_empty() {
            return out;
        }

        let mut effective_live = 0u64;
        let mut total_weight = buckets.len() as u64;
        for (index, bucket) in buckets.iter().enumerate() {
            let extra = (preferred == Some(index))
                .then_some(preferred_extra)
                .unwrap_or(0);
            let degree = u64::from(bucket.edge_len.saturating_add(extra));
            effective_live = effective_live.saturating_add(degree);
            total_weight = total_weight.saturating_add(degree);
        }
        let gaps = u64::from(span_slots).saturating_sub(effective_live);

        // Same layout as the historical `f64` implementation: one fixed-point step
        // `floor((gaps/total_weight) * 1e8) / 1e8` per bucket weight `(deg+1)`.
        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u.saturating_mul(P) / tw
        };

        let mut cursor_fp = u128::from(start_slot).saturating_mul(P);
        for (index, bucket) in buckets.iter().enumerate() {
            let start = (cursor_fp / P) as u64;
            out.push(start);
            let extra = (preferred == Some(index))
                .then_some(preferred_extra)
                .unwrap_or(0);
            let deg = u128::from(bucket.edge_len.saturating_add(extra));
            let start_fp = u128::from(start).saturating_mul(P);
            cursor_fp = start_fp
                .saturating_add(deg.saturating_mul(P))
                .saturating_add(step_fp.saturating_mul(deg.saturating_add(1)));
        }
        out
    }

    /// Appends a new vertex row.
    pub fn push_vertex(&self, vertex: LabeledVertex) -> Result<VertexId, crate::GrowFailed> {
        let id = self.vertices.len();
        self.vertices.push(vertex)?;
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
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.default_label {
                return Err(LabeledOperationError::InvalidDefaultBypass);
            }
            self.edges.insert_edge(&self.vertices, src, edge)?;
            return Ok(());
        }

        let bucket_slot = self.find_or_create_bucket(src, &vertex, label_id)?;
        let bucket_index = bucket_slot.saturating_sub(vertex.base_slot_start()) as u32;
        for _attempt in 0..64u32 {
            let vertex = self.vertices.get(src);
            let successor_start = self.bucket_successor_start(&vertex, bucket_index)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(bucket_slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.overflow_log_head < 0
                && successor_start.saturating_sub(bucket.edge_start) > u64::from(bucket.edge_len)
            {
                let write_slot = bucket.edge_start.saturating_add(u64::from(bucket.edge_len));
                debug_assert!(write_slot < successor_start);
                self.edges.write_slot(write_slot, edge)?;
                self.buckets.write_label_bucket_slot(
                    bucket_slot,
                    bucket.with_edge_range(bucket.edge_start, bucket.edge_len.saturating_add(1)),
                )?;
                self.edges
                    .set_num_edges(self.edges.header().num_edges.saturating_add(1));
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
                    self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, false)?;
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
    ) -> Result<u64, LabeledOperationError> {
        if let Some(slot) = self.find_bucket_slot(vertex, label_id)? {
            return Ok(slot);
        }
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_insert_new_label_bucket");
        let slot = self
            .buckets
            .insert_label_bucket(
                &self.vertices,
                src,
                LabelBucket {
                    label_id,
                    ..LabelBucket::default()
                },
            )
            .map_err(LabeledOperationError::from)?;
        let vertex = self.vertices.get(src);
        let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
        self.rewrite_vertex_edge_span(src, Some(bucket_index), 1, false, false)?;
        Ok(slot)
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
            if bucket.label_id != self.default_label {
                return Err(LabeledOperationError::InvalidDefaultBypass);
            }
            if bucket.overflow_log_head >= 0 {
                self.rewrite_vertex_edge_span(src, Some(0), 0, false, false)?;
                bucket = self
                    .buckets
                    .read_label_bucket_slot(vertex.base_slot_start())
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
            let old_alloc = vertex.vertex_edge_alloc_slots();
            let live_edges = i64::from(bucket.edge_len);
            let updated = vertex
                .with_default_edge_labeled(true)
                .with_base_slot_start(bucket.edge_start)
                .with_degree(bucket.edge_len)
                .with_vertex_edge_alloc_slots(0);
            self.buckets
                .clear_vertex_label_buckets(&self.vertices, src)?;
            self.vertices.set(src, &updated);
            self.edges.bump_vertex_segment_counts(
                src,
                0,
                live_edges.saturating_sub(i64::from(old_alloc)),
            )?;
        } else {
            self.vertices
                .set(src, &vertex.with_default_edge_labeled(true));
        }
        Ok(())
    }

    /// Iterates all outgoing edges for one label without per-edge label checks.
    pub fn iter_edges_for_label(
        &self,
        src: VertexId,
        label_id: LabelId,
    ) -> Result<Vec<E>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.default_label {
                return Ok(Vec::new());
            }
            return Ok(self
                .edges
                .collect_out_edges_slot_order(&self.vertices, src)?);
        }
        if let Some(slot) = self.find_bucket_slot(&vertex, label_id)? {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_iter_edges_for_label_collect");
            let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
            let successor_start = self.bucket_successor_start(&vertex, bucket_index)?;
            return Ok(self.edges.collect_out_edges_slot_order(
                &LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src),
                VertexId::from(0),
            )?);
        }
        Ok(Vec::new())
    }

    /// Iterates all outgoing edges across every label bucket.
    pub fn iter_out_edges(&self, src: VertexId) -> Result<Vec<E>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(self
                .edges
                .collect_out_edges_slot_order(&self.vertices, src)?);
        }
        let mut out = Vec::new();
        #[cfg(feature = "canbench")]
        let _bench_scope = bench_scope("labeled_iter_out_edges_collect");
        for slot in Self::bucket_range(&vertex) {
            let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
            let successor_start = self.bucket_successor_start(&vertex, bucket_index)?;
            out.extend(self.edges.collect_out_edges_slot_order(
                &LabelEdgeSpanAccess::new(&self.buckets, slot, successor_start, src),
                VertexId::from(0),
            )?);
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
            if label_id != self.default_label {
                return Ok(None);
            }
            return self
                .edges
                .remove_edge_unordered_matching(&self.vertices, src, matches)
                .map_err(Into::into);
        }
        if let Some(slot) = self.find_bucket_slot(&vertex, label_id)? {
            #[cfg(feature = "canbench")]
            let _bench_scope = bench_scope("labeled_remove_edge_skip_leaf");
            let bucket_index = slot.saturating_sub(vertex.base_slot_start()) as u32;
            let mut bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
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
                let edge = self
                    .edges
                    .read_slot(bucket.edge_start.saturating_add(u64::from(offset)));
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
                let last = self
                    .edges
                    .read_slot(bucket.edge_start.saturating_add(u64::from(last_index)));
                self.edges.write_slot(
                    bucket.edge_start.saturating_add(u64::from(local_index)),
                    last,
                )?;
            }
            self.buckets.write_label_bucket_slot(
                slot,
                bucket
                    .with_edge_range(bucket.edge_start, last_index)
                    .with_overflow_log_head(-1),
            )?;
            self.edges
                .set_num_edges(self.edges.header().num_edges.saturating_sub(1));
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

    fn test_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
        let default_label = LabelId::from_raw(1);
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
                TestEdge { target: 10 },
                TestEdge { target: 11 },
                TestEdge { target: 20 },
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
        let label = LabelId::from_raw(2);
        for target in 0..127u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
        }
        let alloc_before = graph
            .vertices()
            .get(VertexId::from(0))
            .vertex_edge_alloc_slots();
        graph
            .insert_edge(VertexId::from(0), label, TestEdge { target: 127 })
            .unwrap();
        let alloc_after = graph
            .vertices()
            .get(VertexId::from(0))
            .vertex_edge_alloc_slots();
        assert!(
            alloc_after > alloc_before,
            "expected post-insert cascade to expand VertexEdgeSpan reservation (before={alloc_before}, after={alloc_after})"
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
                TestEdge { target: 20 },
                TestEdge { target: 21 },
                TestEdge { target: 70 },
                TestEdge { target: 100 },
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
        assert_eq!(last.base_slot_start(), first.base_slot_start() + 2);
        assert_eq!(
            graph.iter_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 10 }, TestEdge { target: 30 }]
        );
        assert_eq!(
            graph.iter_out_edges(VertexId::from(31)).unwrap(),
            vec![TestEdge { target: 20 }]
        );
    }

    #[test]
    fn insert_beyond_initial_label_edge_span_capacity_relocates_vertex_edge_span() {
        let graph = test_graph();
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
        assert_eq!(after.vertex_edge_alloc_slots(), 8);
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
            .insert_edge(
                VertexId::from(0),
                graph.default_label(),
                TestEdge { target: 7 },
            )
            .unwrap();

        let before = graph.vertices().get(VertexId::from(0));
        assert!(!before.is_default_edge_labeled());
        assert!(before.vertex_edge_alloc_slots() > 0);

        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert!(after.is_default_edge_labeled());
        assert_eq!(after.degree(), 1);
        assert_eq!(after.vertex_edge_alloc_slots(), 0);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 7 }]
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
