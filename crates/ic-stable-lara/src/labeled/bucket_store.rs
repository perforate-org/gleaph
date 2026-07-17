//! Dedicated stable storage for LabelBucket rows.
//!
//! LabelBuckets are grouped by VertexSegment (32 vertices by default). The
//! bucket store has no separate overflow log; per-label overflow into the shared
//! edge [`EdgeSlabStore`] segment log is recorded on each [`LabelBucket`]. When a
//! vertex gains a new [`LabelBucket`], the owning VertexSegment is rewritten
//! immediately into a physical span whose length is exactly the segment's live
//! bucket count.
//!
//! This store owns only bucket descriptors. It does not know or reserve edge
//! capacity. Edge capacity belongs to [`LabeledVertex::vertex_stored_slots`]
//! and is managed by `LabeledLaraGraph` when it rewrites a VertexEdgeSpan.
//!
//! Bucket rows for one vertex are strictly sorted by [`crate::labeled::BucketLabelKey`], so
//! undirected keys (MSB clear) and directed keys (MSB set) occupy **contiguous index ranges**;
//! see [`LabelBucketStore::directedness_bucket_index_range`].

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::BucketDirectedness,
        record::{
            LabelBucket, LabeledVertex, MAX_VERTEX_LABEL_BUCKET_SLACK, MAX_VERTEX_LABEL_BUCKETS,
        },
    },
    lara::{
        edge::{
            EdgeHeaderV1 as SlabHeaderV1, EdgeSlabStore,
            free_span::{FreeSpan, FreeSpanStore},
        },
        operation_error::{LaraOperationError, VertexAccess},
    },
    traits::CsrVertex,
};
use ic_stable_structures::Memory;
use std::fmt;

/// Reservation token for a bypass-to-bucket-mode promotion.
///
/// Captures the segment rewrite needed to add the source vertex's first label
/// bucket while preserving all fallible allocation until the source vertex row
/// ceases to be a valid bypass row.
pub(crate) struct BypassPromotionPlan {
    /// Vertex stored_slots to publish after the promotion.
    pub(crate) new_alloc: u32,
    /// First slot of the newly allocated segment-bucket span. `None` until
    /// [`LabelBucketStore::reserve_promote_bypass_to_bucket_mode`] fills it in,
    /// so a caller that skips the reserve step cannot silently commit an
    /// unallocated placeholder.
    pub(crate) new_base: Option<u64>,
    /// Rows of the rewritten vertex segment, in vertex order.
    pub(crate) rows: Vec<(u32, LabeledVertex, Vec<LabelBucket>, u32)>,
    /// Old physical spans to release after the rewrite.
    pub(crate) old_spans: Vec<(u64, u64)>,
    /// Total physical slots in the new span.
    pub(crate) total_physical: u64,
    /// Ordinal of the promoted vertex within `rows`.
    pub(crate) source_v_ord: u32,
}

/// Errors returned when reopening a [`LabelBucketStore`].
#[derive(Debug)]
pub enum InitError {
    /// The bucket slab could not be reopened.
    Slab(crate::lara::edge::SlabInitError),
    /// Free-span metadata could not be initialized.
    FreeSpan,
    /// The backing memories are partially initialized (some regions are empty
    /// while others are populated), so the store must not be reopened or recreated.
    PartialLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Slab(err) => write!(f, "bucket slab init failed: {err}"),
            Self::FreeSpan => write!(f, "bucket free-span init failed"),
            Self::PartialLayout => {
                write!(
                    f,
                    "bucket store memories are partially initialized; refusing to reopen"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable LabelBucket slab plus free-span metadata.
pub(crate) struct LabelBucketStore<M: Memory> {
    slab: EdgeSlabStore<LabelBucket, M>,
    free_spans: FreeSpanStore<M>,
}

const MIN_BUCKET_ROW_ALLOC: u32 = 4;

/// When the binary-search interval shrinks to this many bucket indices or fewer, finish the
/// partition with a linear scan (same result as pure binary lower-bound on the directed MSB).
const PARTITION_BINARY_TO_LINEAR_REM: u32 = 16;

/// How to locate the first **directed** bucket index `p` (half-open undirected prefix `[0, p)`).
///
/// LARA keeps buckets strictly sorted by [`crate::labeled::BucketLabelKey`], so `p` is unique.
/// Different strategies match how callers walk edges (ascending vs descending) for probe locality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirectednessPartitionStrategy {
    /// Binary search on the MSB, then linear over the final ≤[`PARTITION_BINARY_TO_LINEAR_REM`] candidates.
    HybridBinary,
    /// Scan ascending from index `0` until a directed bucket (early exit on undirected-only prefixes).
    LinearFromStart,
    /// Scan descending from index `degree - 1` until an undirected bucket (early exit when the tail is undirected-only).
    LinearFromEnd,
}

/// Numerator/denominator for multiplicative growth of label-bucket descriptor span on rewrite:
/// `new = max(ceil(max(current, MIN_BUCKET_ROW_ALLOC) * NUM / DEN), needed)`.
///
/// Default `5 / 4` (~1.25×, ceiling). Enable `bucket_row_grow_150` for `3 / 2`, or
/// `bucket_row_grow_double` for historical `2 / 1`. If both optional features are enabled,
/// `bucket_row_grow_double` wins.
#[cfg(feature = "bucket_row_grow_double")]
const BUCKET_ROW_GROW_NUM: u32 = 2;
#[cfg(feature = "bucket_row_grow_double")]
const BUCKET_ROW_GROW_DEN: u32 = 1;

#[cfg(all(
    feature = "bucket_row_grow_150",
    not(feature = "bucket_row_grow_double")
))]
const BUCKET_ROW_GROW_NUM: u32 = 3;
#[cfg(all(
    feature = "bucket_row_grow_150",
    not(feature = "bucket_row_grow_double")
))]
const BUCKET_ROW_GROW_DEN: u32 = 2;

#[cfg(not(any(feature = "bucket_row_grow_150", feature = "bucket_row_grow_double")))]
const BUCKET_ROW_GROW_NUM: u32 = 5;
#[cfg(not(any(feature = "bucket_row_grow_150", feature = "bucket_row_grow_double")))]
const BUCKET_ROW_GROW_DEN: u32 = 4;

impl<M: Memory> LabelBucketStore<M> {
    /// Opens a fresh LabelBucketStore over three stable memories.
    pub(crate) fn new(
        slab: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        slots_per_vertex: u32,
    ) -> Result<Self, crate::GrowFailed> {
        crate::slab_index::validate_elem_capacity_grow_failed(elem_capacity, slab.size())?;
        let header = SlabHeaderV1::new(
            elem_capacity,
            1,
            slots_per_vertex,
            LabelBucket::BYTES as u32,
            slots_per_vertex,
        );
        let slab = EdgeSlabStore::new(slab, header)?;
        let free_spans =
            FreeSpanStore::new(free_spans, free_span_by_start).map_err(|_| crate::GrowFailed {
                current_size: 0,
                delta: 0,
            })?;
        Ok(Self { slab, free_spans })
    }

    /// Reopens a LabelBucketStore, or creates one when the slab memory is empty.
    pub(crate) fn init(
        slab: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        slots_per_vertex: u32,
    ) -> Result<Self, InitError> {
        match crate::classify_composite_init([
            slab.size(),
            free_spans.size(),
            free_span_by_start.size(),
        ]) {
            crate::CompositeInit::Fresh => {
                return Self::new(
                    slab,
                    free_spans,
                    free_span_by_start,
                    elem_capacity,
                    slots_per_vertex,
                )
                .map_err(|_| InitError::FreeSpan);
            }
            crate::CompositeInit::Partial => return Err(InitError::PartialLayout),
            crate::CompositeInit::Reopen => {}
        }
        let slab = EdgeSlabStore::init(slab).map_err(InitError::Slab)?;
        let free_spans =
            FreeSpanStore::init(free_spans, free_span_by_start).map_err(|_| InitError::FreeSpan)?;
        Ok(Self { slab, free_spans })
    }

    /// Returns the bucket slab header (shared on-disk layout with edge slabs).
    pub(crate) fn header(&self) -> SlabHeaderV1 {
        self.slab.header().expect("bucket slab header")
    }

    /// Reads one bucket slab slot.
    pub(crate) fn read_label_bucket_slot(&self, slot: u64) -> Option<LabelBucket> {
        if slot >= self.header().elem_capacity {
            return None;
        }
        let mut bytes = [0u8; LabelBucket::BYTES];
        self.slab.read_slot(slot, &mut bytes);
        Some(LabelBucket::read_from(&bytes))
    }

    /// Reads `count` consecutive bucket slots starting at `start_slot`.
    pub(crate) fn read_label_bucket_slots_contiguous(
        &self,
        start_slot: u64,
        count: u32,
    ) -> Option<Vec<LabelBucket>> {
        if count == 0 {
            return Some(Vec::new());
        }
        let cap = self.header().elem_capacity;
        let last = start_slot.checked_add(u64::from(count - 1))?;
        if last >= cap {
            return None;
        }
        let nbytes = usize::try_from(count)
            .ok()?
            .checked_mul(LabelBucket::BYTES)?;
        let mut raw = vec![0u8; nbytes];
        self.slab.read_slots_contiguous(start_slot, &mut raw);
        let mut out = Vec::with_capacity(count as usize);
        for chunk in raw.as_chunks::<{ LabelBucket::BYTES }>().0 {
            out.push(LabelBucket::read_from(chunk));
        }
        debug_assert_eq!(out.len(), count as usize);
        Some(out)
    }

    /// Smallest `i in [0, degree]` such that bucket `i` is **directed** (MSB set), or `degree` if all undirected.
    ///
    /// Hybrid: binary search while `hi - lo` > [`PARTITION_BINARY_TO_LINEAR_REM`], then linear scan.
    pub(crate) fn partition_first_directed_hybrid(
        &self,
        base_slot_start: u64,
        degree: u32,
    ) -> Result<u32, LaraOperationError> {
        if degree == 0 {
            return Ok(0);
        }
        let mut lo = 0u32;
        let mut hi = degree;
        while hi - lo > PARTITION_BINARY_TO_LINEAR_REM {
            let mid = lo + (hi - lo) / 2;
            let slot = base_slot_start
                .checked_add(u64::from(mid))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key().is_undirected() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for i in lo..hi {
            let slot = base_slot_start
                .checked_add(u64::from(i))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key().is_directed() {
                return Ok(i);
            }
        }
        Ok(hi)
    }

    /// Same partition as [`Self::partition_first_directed_hybrid`], but scan `0..degree` in order
    /// (stops at the first directed bucket).
    pub(crate) fn partition_first_directed_linear_from_start(
        &self,
        base_slot_start: u64,
        degree: u32,
    ) -> Result<u32, LaraOperationError> {
        for i in 0..degree {
            let slot = base_slot_start
                .checked_add(u64::from(i))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key().is_directed() {
                return Ok(i);
            }
        }
        Ok(degree)
    }

    /// Same partition as [`Self::partition_first_directed_hybrid`], but scan from the last bucket
    /// backward until an undirected bucket (or the scan exhausts the row).
    pub(crate) fn partition_first_directed_linear_from_end(
        &self,
        base_slot_start: u64,
        degree: u32,
    ) -> Result<u32, LaraOperationError> {
        if degree == 0 {
            return Ok(0);
        }
        for i in (0..degree).rev() {
            let slot = base_slot_start
                .checked_add(u64::from(i))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if bucket.bucket_label_key().is_undirected() {
                return Ok(i.saturating_add(1));
            }
        }
        Ok(0)
    }

    /// Half-open `[lo, hi)` bucket indices (within `0..degree`) whose wire keys match `want`.
    ///
    /// `strategy` selects how the directed/undirected partition `p` is found (all strategies return
    /// the same `p` under LARA invariants; they differ in read order and early-exit behavior).
    pub(crate) fn directedness_bucket_index_range(
        &self,
        base_slot_start: u64,
        degree: u32,
        want: BucketDirectedness,
        strategy: DirectednessPartitionStrategy,
    ) -> Result<(u32, u32), LaraOperationError> {
        if degree == 0 {
            return Ok((0, 0));
        }
        let p = match strategy {
            DirectednessPartitionStrategy::HybridBinary => {
                self.partition_first_directed_hybrid(base_slot_start, degree)?
            }
            DirectednessPartitionStrategy::LinearFromStart => {
                self.partition_first_directed_linear_from_start(base_slot_start, degree)?
            }
            DirectednessPartitionStrategy::LinearFromEnd => {
                self.partition_first_directed_linear_from_end(base_slot_start, degree)?
            }
        };
        Ok(match want {
            BucketDirectedness::Undirected => (0, p),
            BucketDirectedness::Directed => (p, degree),
        })
    }

    /// Writes one bucket slab slot.
    pub(crate) fn write_label_bucket_slot(
        &self,
        slot: u64,
        bucket: LabelBucket,
    ) -> Result<(), LaraOperationError> {
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        self.slab
            .write_slot(slot, &bytes)
            .map_err(LaraOperationError::WriteEdgeSlotFailed)
    }

    /// Updates only the logical degree field of an existing bucket row.
    ///
    /// Callers may use this only when every physical edge and payload field is unchanged.
    #[inline]
    pub(crate) fn write_label_bucket_degree(
        &self,
        slot: u64,
        degree: u32,
    ) -> Result<(), LaraOperationError> {
        debug_assert!(slot < self.header().elem_capacity);
        self.slab
            .write_slot_range(slot, 8, &degree.to_le_bytes())
            .map_err(LaraOperationError::WriteEdgeSlotFailed)
    }

    /// Writes `buckets.len()` descriptors starting at `start_slot` in one slab write.
    pub(crate) fn write_label_bucket_slots_contiguous(
        &self,
        start_slot: u64,
        buckets: &[LabelBucket],
    ) -> Result<(), LaraOperationError> {
        let mut scratch = Vec::new();
        self.write_label_bucket_slots_contiguous_with_scratch(start_slot, buckets, &mut scratch)
    }

    fn write_label_bucket_slots_contiguous_with_scratch(
        &self,
        start_slot: u64,
        buckets: &[LabelBucket],
        scratch: &mut Vec<u8>,
    ) -> Result<(), LaraOperationError> {
        if buckets.is_empty() {
            return Ok(());
        }
        let nbytes = buckets
            .len()
            .checked_mul(LabelBucket::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        scratch.resize(nbytes, 0);
        for (i, bucket) in buckets.iter().enumerate() {
            let lo = i
                .checked_mul(LabelBucket::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let hi = lo
                .checked_add(LabelBucket::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            bucket.write_to(&mut scratch[lo..hi]);
        }
        self.slab
            .write_slots_contiguous(start_slot, scratch)
            .map_err(LaraOperationError::WriteEdgeSlotFailed)
    }

    /// Writes a label-bucket descriptor row, using per-slot writes for tiny rows (avoids a
    /// temporary encode buffer) and a single contiguous slab write for larger rows.
    pub(crate) fn write_label_bucket_row_adaptive(
        &self,
        start_slot: u64,
        row: &[LabelBucket],
    ) -> Result<(), LaraOperationError> {
        let mut scratch = Vec::new();
        self.write_label_bucket_row_adaptive_with_scratch(start_slot, row, &mut scratch)
    }

    pub(crate) fn write_label_bucket_row_adaptive_with_scratch(
        &self,
        start_slot: u64,
        row: &[LabelBucket],
        scratch: &mut Vec<u8>,
    ) -> Result<(), LaraOperationError> {
        const CONTIGUOUS_WRITE_AT: usize = 8;
        if row.len() < CONTIGUOUS_WRITE_AT {
            for (i, bucket) in row.iter().enumerate() {
                let slot = start_slot
                    .checked_add(i as u64)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.write_label_bucket_slot(slot, *bucket)?;
            }
            Ok(())
        } else {
            self.write_label_bucket_slots_contiguous_with_scratch(start_slot, row, scratch)
        }
    }

    fn grow_capacity_to_fit(&self, slot: u64) -> Result<(), LaraOperationError> {
        let cap = self.header().elem_capacity;
        if slot < cap {
            return Ok(());
        }
        if !crate::slab_index::slot_index_fits(slot) {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        let next = crate::slab_index::checked_add_slot_exclusive_end(slot, 1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.slab
            .set_elem_capacity(next)
            .map_err(LaraOperationError::ResizeFailed)
    }

    fn record_allocation(&self, last_slot: u64) -> Result<(), LaraOperationError> {
        let mut header = self.header();
        let tail = last_slot
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if tail > header.slab_occupied_tail {
            header.slab_occupied_tail = tail;
        }
        if tail > header.num_edges {
            header.num_edges = tail;
        }
        self.slab.write_header(&header);
        Ok(())
    }

    fn map_free_span_err(&self) -> LaraOperationError {
        LaraOperationError::RebalanceFailed(crate::GrowFailed {
            current_size: 0,
            delta: 0,
        })
    }

    pub(crate) fn allocate_span(&self, len: u64) -> Result<u64, LaraOperationError> {
        if len == 0 {
            return Ok(self.header().elem_capacity);
        }
        if let Some(span) = self
            .free_spans
            .take_best_fit(len)
            .map_err(|_| self.map_free_span_err())?
        {
            return Ok(span.start_slot);
        }
        let start = self.header().elem_capacity;
        let end = crate::slab_index::checked_add_slot_exclusive_end(start, len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let last_in_span = end
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.grow_capacity_to_fit(last_in_span)?;
        self.record_allocation(last_in_span)?;
        Ok(start)
    }

    pub(crate) fn release_span(&self, start_slot: u64, len: u64) -> Result<(), LaraOperationError> {
        if len > 0 {
            self.free_spans
                .release(FreeSpan { start_slot, len })
                .map_err(|_| self.map_free_span_err())?;
        }
        Ok(())
    }

    pub(crate) fn segment_vertex_bounds<V>(
        &self,
        vertices: &V,
        vid: VertexId,
    ) -> Result<(u32, u32), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let segment_size = self.header().segment_size.max(1);
        let start = (u32::from(vid) / segment_size) * segment_size;
        let raw_end = start
            .checked_add(segment_size)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok((start, raw_end.min(vertices.len())))
    }

    fn label_bucket_descriptor_span(vertex: LabeledVertex) -> Result<u32, LaraOperationError> {
        vertex
            .label_bucket_descriptor_span()
            .ok_or(LaraOperationError::CollectAllocationOverflow)
    }

    fn grow_label_bucket_descriptor_span(
        current_physical: u32,
        needed_live: u32,
    ) -> Result<u32, LaraOperationError> {
        if !LabeledVertex::label_bucket_count_fits(needed_live) {
            return Err(LaraOperationError::RowDegreeOverflow);
        }
        let base = current_physical.max(needed_live).max(MIN_BUCKET_ROW_ALLOC);
        let prod = u64::from(base)
            .checked_mul(u64::from(BUCKET_ROW_GROW_NUM))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let grown_u64 = prod.div_ceil(u64::from(BUCKET_ROW_GROW_DEN));
        let grown = u32::try_from(grown_u64).map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        let max_physical = MAX_VERTEX_LABEL_BUCKETS
            .checked_add(u32::from(MAX_VERTEX_LABEL_BUCKET_SLACK))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok(grown.max(needed_live).min(max_physical))
    }

    fn slack_for_physical_span(
        live_rows: u32,
        physical_span: u32,
    ) -> Result<u16, LaraOperationError> {
        LabeledVertex::bucket_slack_for_descriptor_span(live_rows, physical_span)
            .ok_or(LaraOperationError::CollectAllocationOverflow)
    }

    fn collect_segment_bucket_rows<V>(
        &self,
        vertices: &V,
        vid: VertexId,
    ) -> Result<Vec<(u32, LabeledVertex, Vec<LabelBucket>, u32)>, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let (start, end) = self.segment_vertex_bounds(vertices, vid)?;
        let mut rows = Vec::new();
        for v_ord in start..end {
            let v = vertices.get(VertexId::from(v_ord));
            if v.is_default_edge_labeled() {
                continue;
            }
            let alloc = Self::label_bucket_descriptor_span(v)?;
            let deg = v.degree();
            if !LabeledVertex::label_bucket_count_fits(deg) {
                return Err(LaraOperationError::RowDegreeOverflow);
            }
            let buckets = if deg == 0 {
                Vec::new()
            } else {
                self.read_label_bucket_slots_contiguous(v.base_slot_start(), deg)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?
            };
            if alloc > 0 || !buckets.is_empty() || v_ord == u32::from(vid) {
                rows.push((v_ord, v, buckets, alloc));
            }
        }
        Ok(rows)
    }

    fn rewrite_segment_bucket_rows<V>(
        &self,
        vertices: &V,
        rows: Vec<(u32, LabeledVertex, Vec<LabelBucket>, u32)>,
    ) -> Result<(), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let total: u64 = rows
            .iter()
            .map(|(_, _, _, physical)| u64::from(*physical))
            .sum();
        let mut old_spans = Vec::new();
        for (_, v, _, _) in &rows {
            let physical = Self::label_bucket_descriptor_span(*v)?;
            if physical > 0 {
                old_spans.push((v.base_slot_start(), u64::from(physical)));
            }
        }
        old_spans.sort_unstable_by_key(|(start, _)| *start);

        let new_base = if total == 0 {
            0
        } else {
            self.allocate_span(total)?
        };
        let mut cursor = new_base;
        for (v_ord, v, buckets, physical) in rows {
            let row_base = cursor;
            self.write_label_bucket_slots_contiguous(cursor, &buckets)?;
            cursor = row_base
                .checked_add(u64::from(physical))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let live = buckets.len() as u32;
            let slack = Self::slack_for_physical_span(live, physical)?;
            let updated = v.try_with_bucket_row_and_slack(row_base, live, slack)?;
            vertices.set(VertexId::from(v_ord), &updated);
        }

        for (start, len) in old_spans {
            self.release_span(start, len)?;
        }
        if total > 0 {
            let last = new_base
                .checked_add(total)
                .and_then(|end| end.checked_sub(1))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.record_allocation(last)?;
        }
        Ok(())
    }

    /// Rewrites the VertexSegment containing `vid` into its minimal physical span.
    pub(crate) fn compact_vertex_segment_for_vertex<V>(
        &self,
        vertices: &V,
        vid: VertexId,
    ) -> Result<(), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let mut rows = self.collect_segment_bucket_rows(vertices, vid)?;
        for (_, _, buckets, physical) in &mut rows {
            let live = buckets.len() as u32;
            if !LabeledVertex::label_bucket_count_fits(live) {
                return Err(LaraOperationError::RowDegreeOverflow);
            }
            *physical = live;
        }
        self.rewrite_segment_bucket_rows(vertices, rows)
    }

    /// Removes all LabelBuckets for `vid`, then rewrites the owning VertexSegment.
    pub(crate) fn clear_vertex_label_buckets<V>(
        &self,
        vertices: &V,
        vid: VertexId,
    ) -> Result<(), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let mut rows = self.collect_segment_bucket_rows(vertices, vid)?;
        for (v_ord, _, buckets, alloc) in &mut rows {
            if *v_ord == u32::from(vid) {
                buckets.clear();
                *alloc = 0;
                break;
            }
        }
        self.rewrite_segment_bucket_rows(vertices, rows)
    }

    /// Inserts one LabelBucket in label order, rewriting the owning VertexSegment immediately.
    ///
    /// The returned slot is stable only until the next rewrite of the same
    /// LabelBucketStore VertexSegment. Callers should use it immediately, then derive
    /// future bucket positions from the owning [`LabeledVertex`] again.
    #[cfg(test)]
    pub(crate) fn insert_label_bucket<V>(
        &self,
        vertices: &V,
        vid: VertexId,
        bucket: LabelBucket,
    ) -> Result<u64, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let v = vertices.get_in_range(vid)?;
        let buckets = self.read_label_bucket_slots_contiguous(v.base_slot_start(), v.degree());
        let buckets = buckets.ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let index = buckets
            .binary_search_by_key(&bucket.bucket_label_key(), |candidate| {
                candidate.bucket_label_key()
            })
            .unwrap_or_else(|index| index);
        self.insert_label_bucket_at(vertices, vid, bucket, index as u32)
            .map(|(slot, _)| slot)
    }

    /// Insert a label bucket; the `bool` is `true` when the owning [`LabelBucketStore`]
    /// vertex segment was physically rewritten (relocating descriptors for peers in that segment).
    pub(crate) fn insert_label_bucket_at<V>(
        &self,
        vertices: &V,
        vid: VertexId,
        bucket: LabelBucket,
        insert_index: u32,
    ) -> Result<(u64, bool), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let v = vertices.get_in_range(vid)?;
        if insert_index > v.degree() {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        if !v.is_default_edge_labeled() && v.bucket_slack_slots() > 0 {
            let base = v.base_slot_start();
            let deg = v.degree();
            let mut row = if deg == 0 {
                Vec::new()
            } else {
                self.read_label_bucket_slots_contiguous(base, deg)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?
            };
            row.insert(insert_index as usize, bucket);
            self.write_label_bucket_slots_contiguous(base, &row)?;
            let insert_at = base
                .checked_add(u64::from(insert_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let next_degree = v
                .degree()
                .checked_add(1)
                .filter(|count| LabeledVertex::label_bucket_count_fits(*count))
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
            let updated = v
                .try_with_label_bucket_count(next_degree)?
                .with_bucket_slack_slots(v.bucket_slack_slots().saturating_sub(1))
                .ensure_valid_normal_row()?;
            vertices.set(vid, &updated);
            return Ok((insert_at, false));
        }

        let mut rows = self.collect_segment_bucket_rows(vertices, vid)?;
        let mut inserted_index = None;
        for (v_ord, v, buckets, alloc) in &mut rows {
            if *v_ord == u32::from(vid) {
                let index = insert_index as usize;
                if index > buckets.len() {
                    return Err(LaraOperationError::CollectAllocationOverflow);
                }
                inserted_index = Some(u64::from(insert_index));
                buckets.insert(index, bucket);
                let needed_live = buckets.len() as u32;
                if !LabeledVertex::label_bucket_count_fits(needed_live) {
                    return Err(LaraOperationError::RowDegreeOverflow);
                }
                let current_physical = Self::label_bucket_descriptor_span(*v)?;
                *alloc = Self::grow_label_bucket_descriptor_span(current_physical, needed_live)?;
                break;
            }
        }
        let inserted_index = inserted_index.ok_or(LaraOperationError::VertexAccess(
            crate::lara::operation_error::VertexAccessError::OutOfRange,
        ))?;
        self.rewrite_segment_bucket_rows(vertices, rows)?;
        let v = vertices.get_in_range(vid)?;
        let out_slot = v
            .base_slot_start()
            .checked_add(inserted_index)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok((out_slot, true))
    }

    /// Computes the segment-bucket rewrite for promoting a bypass-mode vertex
    /// to bucket mode without mutating any vertex row or allocating slab space.
    ///
    /// This is the infallible planning step: it derives the target rows,
    /// physical span, old spans to release, and source-vertex updated fields.
    /// All fallible backing-memory reservation is performed by
    /// [`Self::reserve_promote_bypass_to_bucket_mode`] after any caller-side
    /// validation, so an error leaves the graph observable through its normal
    /// read API.
    pub(crate) fn plan_promote_bypass_to_bucket_mode<V>(
        &self,
        vertices: &V,
        vid: VertexId,
        bucket: LabelBucket,
        new_alloc: u32,
    ) -> Result<BypassPromotionPlan, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let source_v_ord = u32::from(vid);
        let (start, end) = self.segment_vertex_bounds(vertices, vid)?;
        let mut rows = Vec::new();
        let mut source_index = None;
        for v_ord in start..end {
            let v = vertices.get(VertexId::from(v_ord));
            if v_ord == source_v_ord {
                let physical = Self::grow_label_bucket_descriptor_span(0, 1)?;
                source_index = Some(rows.len());
                rows.push((v_ord, v, vec![bucket], physical));
            } else if !v.is_default_edge_labeled() {
                let physical = Self::label_bucket_descriptor_span(v)?;
                let deg = v.degree();
                let buckets = if deg == 0 {
                    Vec::new()
                } else {
                    self.read_label_bucket_slots_contiguous(v.base_slot_start(), deg)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?
                };
                if physical > 0 || !buckets.is_empty() {
                    rows.push((v_ord, v, buckets, physical));
                }
            }
        }
        let _source_index = source_index.ok_or(LaraOperationError::CollectAllocationOverflow)?;

        let total: u64 = rows
            .iter()
            .map(|(_, _, _, physical)| u64::from(*physical))
            .sum();
        let mut old_spans = Vec::new();
        for (_, v, _, _) in &rows {
            if v.is_default_edge_labeled() {
                // The source had no bucket span before promotion.
                continue;
            }
            let physical = Self::label_bucket_descriptor_span(*v)?;
            if physical > 0 {
                old_spans.push((v.base_slot_start(), u64::from(physical)));
            }
        }
        old_spans.sort_unstable_by_key(|(start, _)| *start);

        let new_base = None;

        Ok(BypassPromotionPlan {
            new_alloc,
            new_base,
            rows,
            old_spans,
            total_physical: total,
            source_v_ord,
        })
    }

    /// Reserves bucket-slab and free-span capacity for a planned bypass promotion.
    ///
    /// This is the fallible preflight step: it allocates the target slab span
    /// and reserves free-span record capacity, but does not mutate any vertex
    /// row or canonical metadata.
    pub(crate) fn reserve_promote_bypass_to_bucket_mode(
        &self,
        plan: &mut BypassPromotionPlan,
    ) -> Result<(), LaraOperationError> {
        self.free_spans
            .reserve_for_releases(plan.old_spans.len() as u64)
            .map_err(|_| self.map_free_span_err())?;

        let total = plan.total_physical;
        plan.new_base = Some(if total == 0 {
            0
        } else {
            self.allocate_span(total)?
        });
        Ok(())
    }

    /// Commits a prepared and reserved bypass promotion.
    ///
    /// # Failure atomicity
    ///
    /// Every fallible grow/allocation was preflighted by
    /// [`reserve_promote_bypass_to_bucket_mode`](Self::reserve_promote_bypass_to_bucket_mode),
    /// and every structural invariant was validated by
    /// [`plan_promote_bypass_to_bucket_mode`](Self::plan_promote_bypass_to_bucket_mode).
    /// The first canonical mutation here (writing the source vertex row) is
    /// therefore not followed by any recoverable error. Remaining internal
    /// errors are treated as invariant violations and panic in debug builds.
    pub(crate) fn commit_promote_bypass_to_bucket_mode<V>(
        &self,
        vertices: &V,
        plan: BypassPromotionPlan,
    ) -> Result<(), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let new_base = plan
            .new_base
            .expect("commit: new_base must be allocated by reserve");
        let mut cursor = new_base;
        for (v_ord, v, buckets, physical) in plan.rows {
            let row_base = cursor;
            self.write_label_bucket_slots_contiguous(cursor, &buckets)
                .expect("commit: bucket slab write must succeed after reserve");
            cursor = row_base
                .checked_add(u64::from(physical))
                .expect("commit: physical span cursor overflow should have been rejected by plan");
            let live = buckets.len() as u32;
            let slack = Self::slack_for_physical_span(live, physical)
                .expect("commit: slack should fit after plan");
            let updated = if v_ord == plan.source_v_ord {
                v.with_default_edge_labeled(false)
                    .try_with_bucket_row_and_slack(row_base, live, slack)
                    .expect("commit: source row fields should fit after plan")
                    .with_stored_slots(plan.new_alloc)
            } else {
                v.try_with_bucket_row_and_slack(row_base, live, slack)
                    .expect("commit: peer row fields should fit after plan")
            };
            vertices.set(VertexId::from(v_ord), &updated);
        }
        for (start, len) in plan.old_spans {
            self.release_span(start, len)
                .expect("commit: old-span release must succeed after reserve");
        }
        if plan.total_physical > 0 {
            let last = new_base
                .checked_add(plan.total_physical)
                .and_then(|end| end.checked_sub(1))
                .expect("commit: allocation end should fit after reserve");
            self.record_allocation(last)
                .expect("commit: allocation record must succeed after reserve");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_support::vector_memory, traits::CsrVertex};
    use std::cell::RefCell;

    struct VertexFixture {
        vertex: RefCell<LabeledVertex>,
    }

    impl VertexAccess<LabeledVertex> for VertexFixture {
        fn len(&self) -> u32 {
            1
        }

        fn get(&self, id: VertexId) -> LabeledVertex {
            debug_assert_eq!(u32::from(id), 0);
            *self.vertex.borrow()
        }

        fn set(&self, id: VertexId, item: &LabeledVertex) {
            debug_assert_eq!(u32::from(id), 0);
            *self.vertex.borrow_mut() = *item;
        }
    }

    fn store() -> LabelBucketStore<crate::VectorMemory> {
        LabelBucketStore::new(vector_memory(), vector_memory(), vector_memory(), 64, 4).unwrap()
    }

    #[test]
    fn degree_only_write_preserves_edge_and_payload_physical_state() {
        let buckets = store();
        let original = LabelBucket::from_parts_with_payload(
            crate::labeled::BucketLabelKey::from_raw(7),
            11,
            5,
            9,
            3,
            2,
            17,
            4,
            2,
            1,
        );
        buckets.write_label_bucket_slot(0, original).unwrap();
        buckets.write_label_bucket_degree(0, 4).unwrap();

        assert_eq!(
            buckets.read_label_bucket_slot(0).unwrap(),
            original.with_degree_field(4)
        );
    }

    #[test]
    fn init_rejects_partial_layout_when_free_spans_wiped() {
        let slab = vector_memory();
        let free_spans = vector_memory();
        let by_start = vector_memory();
        LabelBucketStore::new(slab.clone(), free_spans.clone(), by_start.clone(), 64, 4).unwrap();
        // Slab and by-start populated, free-span records wiped (miswired region).
        let result = LabelBucketStore::init(slab, vector_memory(), by_start, 64, 4);
        assert!(matches!(result, Err(InitError::PartialLayout)));
    }

    #[test]
    fn init_reopens_fully_populated_layout() {
        let slab = vector_memory();
        let free_spans = vector_memory();
        let by_start = vector_memory();
        LabelBucketStore::new(slab.clone(), free_spans.clone(), by_start.clone(), 64, 4).unwrap();
        assert!(LabelBucketStore::init(slab, free_spans, by_start, 64, 4).is_ok());
    }

    #[test]
    fn insert_label_bucket_rewrites_owning_segment() {
        let buckets = store();
        let vertices = VertexFixture {
            vertex: RefCell::new(LabeledVertex::default()),
        };
        for label in 0..5u16 {
            buckets
                .insert_label_bucket(
                    &vertices,
                    VertexId::from(0),
                    LabelBucket::default()
                        .with_bucket_label_key(crate::labeled::BucketLabelKey::from_raw(label)),
                )
                .unwrap();
        }
        let vertex = vertices.get(VertexId::from(0));
        assert_eq!(vertex.degree(), 5);
        assert_eq!(vertex.degree(), 5);
        for offset in 0..5u64 {
            let bucket = buckets
                .read_label_bucket_slot(vertex.base_slot_start() + offset)
                .unwrap();
            assert_eq!(
                bucket.bucket_label_key(),
                crate::labeled::BucketLabelKey::from_raw(offset as u16)
            );
        }
    }

    fn insert_buckets_with_keys(
        buckets: &LabelBucketStore<crate::VectorMemory>,
        vertices: &VertexFixture,
        keys: &[crate::labeled::BucketLabelKey],
    ) {
        for key in keys {
            buckets
                .insert_label_bucket(
                    vertices,
                    VertexId::from(0),
                    LabelBucket::default().with_bucket_label_key(*key),
                )
                .unwrap();
        }
    }

    #[test]
    fn partition_strategies_agree_on_mixed_directedness() {
        use crate::labeled::bucket_label_key::{BucketDirectedness, BucketLabelKey};

        let buckets = store();
        let vertices = VertexFixture {
            vertex: RefCell::new(LabeledVertex::default()),
        };
        let keys = [
            BucketLabelKey::undirected_from_index(1),
            BucketLabelKey::undirected_from_index(2),
            BucketLabelKey::directed_from_index(3),
            BucketLabelKey::directed_from_index(4),
        ];
        insert_buckets_with_keys(&buckets, &vertices, &keys);
        let vertex = vertices.get(VertexId::from(0));
        let base = vertex.base_slot_start();
        let degree = vertex.degree();

        let strategies = [
            DirectednessPartitionStrategy::HybridBinary,
            DirectednessPartitionStrategy::LinearFromStart,
            DirectednessPartitionStrategy::LinearFromEnd,
        ];
        let mut partition_points = Vec::new();
        for strategy in strategies {
            let (und_lo, und_hi) = buckets
                .directedness_bucket_index_range(
                    base,
                    degree,
                    BucketDirectedness::Undirected,
                    strategy,
                )
                .unwrap();
            partition_points.push(und_hi);
            assert_eq!(und_lo, 0);
            assert_eq!(und_hi, 2, "two undirected buckets precede directed keys");
            let (dir_lo, dir_hi) = buckets
                .directedness_bucket_index_range(
                    base,
                    degree,
                    BucketDirectedness::Directed,
                    strategy,
                )
                .unwrap();
            assert_eq!(dir_lo, 2);
            assert_eq!(dir_hi, degree);
        }
        assert!(
            partition_points.windows(2).all(|w| w[0] == w[1]),
            "all partition strategies must return the same boundary"
        );
    }

    #[test]
    fn undirected_only_vertex_has_full_range_for_undirected() {
        use crate::labeled::bucket_label_key::{BucketDirectedness, BucketLabelKey};

        let buckets = store();
        let vertices = VertexFixture {
            vertex: RefCell::new(LabeledVertex::default()),
        };
        insert_buckets_with_keys(
            &buckets,
            &vertices,
            &[
                BucketLabelKey::undirected_from_index(1),
                BucketLabelKey::undirected_from_index(2),
            ],
        );
        let vertex = vertices.get(VertexId::from(0));
        let (lo, hi) = buckets
            .directedness_bucket_index_range(
                vertex.base_slot_start(),
                vertex.degree(),
                BucketDirectedness::Undirected,
                DirectednessPartitionStrategy::HybridBinary,
            )
            .unwrap();
        assert_eq!((lo, hi), (0, vertex.degree()));
        let (dir_lo, dir_hi) = buckets
            .directedness_bucket_index_range(
                vertex.base_slot_start(),
                vertex.degree(),
                BucketDirectedness::Directed,
                DirectednessPartitionStrategy::HybridBinary,
            )
            .unwrap();
        assert_eq!((dir_lo, dir_hi), (vertex.degree(), vertex.degree()));
    }

    #[test]
    fn directedness_range_empty_when_degree_zero() {
        use crate::labeled::bucket_label_key::BucketDirectedness;

        let buckets = store();
        let (lo, hi) = buckets
            .directedness_bucket_index_range(
                0,
                0,
                BucketDirectedness::Undirected,
                DirectednessPartitionStrategy::HybridBinary,
            )
            .unwrap();
        assert_eq!((lo, hi), (0, 0));
    }

    #[test]
    fn compact_segment_releases_old_span_for_reuse() {
        let buckets = store();
        let vertices = VertexFixture {
            vertex: RefCell::new(LabeledVertex::default()),
        };
        for label in 0..5u16 {
            buckets
                .insert_label_bucket(
                    &vertices,
                    VertexId::from(0),
                    LabelBucket::default()
                        .with_bucket_label_key(crate::labeled::BucketLabelKey::from_raw(label)),
                )
                .unwrap();
        }
        let before = vertices.get(VertexId::from(0));
        buckets
            .compact_vertex_segment_for_vertex(&vertices, VertexId::from(0))
            .unwrap();
        let after = vertices.get(VertexId::from(0));
        assert_eq!(after.degree(), before.degree());
        assert_ne!(after.base_slot_start(), before.base_slot_start());
    }
}
