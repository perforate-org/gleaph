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
//! capacity. Edge capacity belongs to [`LabeledVertex::stored_slots`]
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
            BackedSlabImage, EdgeHeaderV1 as SlabHeaderV1, EdgeSlabStore,
            free_span::{FreeSpan, FreeSpanPreparation, FreeSpanStore, PreparedFreeSpans},
        },
        operation_error::{LaraOperationError, VertexAccess},
        vertex::PreparedVertexWrite,
    },
    traits::CsrVertex,
};
use ic_stable_structures::Memory;
use std::fmt;

#[cfg(any(test, feature = "canbench"))]
use crate::labeled::bucket_label_key::BucketLabelKey;

struct BucketRowRewrite {
    ordinal: u32,
    vertex: LabeledVertex,
    buckets: Vec<LabelBucket>,
    physical: u32,
}

/// Current source identity and bytes, shared by planning and ready admission.
struct SegmentSource<'a, V, M: Memory> {
    owner: &'a LabelBucketStore<M>,
    vertices: &'a V,
    header: SlabHeaderV1,
    vertex_rows: Vec<(VertexId, LabeledVertex)>,
    descriptors: Vec<(VertexId, LabeledVertex, Vec<LabelBucket>)>,
}

impl<V: VertexAccess<LabeledVertex>, M: Memory> SegmentSource<'_, V, M> {
    fn validate(&self) -> Result<(), LaraOperationError> {
        if self.owner.header() != self.header {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        for (id, row) in &self.vertex_rows {
            if self.vertices.get_in_range(*id)? != *row {
                return Err(LaraOperationError::CollectAllocationOverflow);
            }
        }
        for (_, row, buckets) in &self.descriptors {
            if self
                .owner
                .read_label_bucket_slots_contiguous(row.base_slot_start(), row.degree())
                .as_deref()
                != Some(buckets.as_slice())
            {
                return Err(LaraOperationError::CollectAllocationOverflow);
            }
        }
        Ok(())
    }
}

fn encode_checked_bucket(bucket: LabelBucket, bytes: &mut [u8]) -> Result<(), LaraOperationError> {
    bucket.write_to(bytes);
    if LabelBucket::try_read_from(bytes).ok() != Some(bucket) {
        return Err(LaraOperationError::CollectAllocationOverflow);
    }
    Ok(())
}

/// Existing slots only: no descriptor relocation, header or allocator publication.
pub(crate) struct PreparedExistingBucketRow<'a, M: Memory>(BackedSlabImage<'a, LabelBucket, M>);

impl<M: Memory> PreparedExistingBucketRow<'_, M> {
    pub(crate) fn write(&self) {
        self.0.write_payloads();
    }
}

/// Generalized current-segment plan; no allocator or canonical writes.
pub(crate) struct SegmentRewritePlan<'a, V, M: Memory> {
    source: SegmentSource<'a, V, M>,
    rows: Vec<BucketRowRewrite>,
}

pub(crate) struct SegmentRewritePreparation<'a, V, M: Memory> {
    source: SegmentSource<'a, V, M>,
    rows: Vec<(VertexId, LabeledVertex)>,
    slab: BackedSlabImage<'a, LabelBucket, M>,
    writes: Vec<PreparedVertexWrite<'a, LabeledVertex>>,
    allocator: FreeSpanPreparation<'a, M>,
}

pub(crate) struct ReadySegmentRewrite<'a, M: Memory> {
    writes: &'a [PreparedVertexWrite<'a, LabeledVertex>],
    slab: &'a BackedSlabImage<'a, LabelBucket, M>,
    allocator: PreparedFreeSpans<'a, M>,
}

impl<'a, V: VertexAccess<LabeledVertex>, M: Memory> SegmentRewritePlan<'a, V, M> {
    pub(crate) fn reserve(self) -> Result<SegmentRewritePreparation<'a, V, M>, LaraOperationError> {
        self.source.validate()?;
        let owner = self.source.owner;
        if self
            .rows
            .windows(2)
            .any(|pair| pair[0].ordinal >= pair[1].ordinal)
            || self.rows.iter().any(|row| {
                !self
                    .source
                    .vertex_rows
                    .iter()
                    .any(|(id, _)| u32::from(*id) == row.ordinal)
            })
            || self
                .source
                .descriptors
                .iter()
                .any(|(id, _, _)| !self.rows.iter().any(|row| row.ordinal == u32::from(*id)))
        {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        let total = self.rows.iter().try_fold(0u64, |sum, row| {
            let live = u32::try_from(row.buckets.len())
                .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
            if row.vertex.is_default_edge_labeled() {
                if row.physical != 0 || !row.buckets.is_empty() {
                    return Err(LaraOperationError::CollectAllocationOverflow);
                }
            } else {
                LabelBucketStore::<M>::slack_for_physical_span(live, row.physical)?;
                if row
                    .buckets
                    .windows(2)
                    .any(|pair| pair[0].bucket_label_key() >= pair[1].bucket_label_key())
                {
                    return Err(LaraOperationError::CollectAllocationOverflow);
                }
            }
            sum.checked_add(u64::from(row.physical))
                .ok_or(LaraOperationError::overflow())
        })?;
        let mut retired = Vec::new();
        retired
            .try_reserve(self.source.descriptors.len())
            .map_err(|_| owner.map_free_span_err())?;
        for (_, vertex, _) in &self.source.descriptors {
            let len = LabelBucketStore::<M>::label_bucket_descriptor_span(
                *vertex,
                self.source.header.elem_capacity,
            )?;
            if len != 0 {
                retired.push(FreeSpan {
                    start_slot: vertex.base_slot_start(),
                    len: u64::from(len),
                });
            }
        }
        let allocator = owner
            .free_spans
            .prepare_replacement(total, &retired)
            .map_err(|_| owner.map_free_span_err())?;
        let (base, header) = LabelBucketStore::<M>::descriptor_destination(
            self.source.header,
            allocator.selected_span(),
            total,
        )?;
        let nbytes = usize::try_from(total)
            .ok()
            .and_then(|n| n.checked_mul(LabelBucket::BYTES))
            .ok_or(LaraOperationError::overflow())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(nbytes)
            .map_err(|_| owner.map_free_span_err())?;
        bytes.resize(nbytes, 0);
        let mut rows = Vec::new();
        rows.try_reserve(self.rows.len())
            .map_err(|_| owner.map_free_span_err())?;
        let mut cursor = base;
        for row in self.rows {
            let live = u32::try_from(row.buckets.len())
                .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
            let vertex = if row.vertex.is_default_edge_labeled() {
                row.vertex
            } else {
                let slack = LabelBucketStore::<M>::slack_for_physical_span(live, row.physical)?;
                row.vertex
                    .try_with_bucket_row_and_slack(cursor, live, slack)?
            };
            let mut encoded_vertex = [0; LabeledVertex::BYTES];
            vertex.write_to(&mut encoded_vertex);
            if LabeledVertex::try_read_from(&encoded_vertex).ok() != Some(vertex) {
                return Err(LaraOperationError::CollectAllocationOverflow);
            }
            let offset = usize::try_from(cursor - base)
                .ok()
                .and_then(|n| n.checked_mul(LabelBucket::BYTES))
                .ok_or(LaraOperationError::overflow())?;
            for (index, bucket) in row.buckets.into_iter().enumerate() {
                let lo = offset + index * LabelBucket::BYTES;
                let encoded = &mut bytes[lo..lo + LabelBucket::BYTES];
                encode_checked_bucket(bucket, encoded)?;
            }
            cursor = cursor
                .checked_add(u64::from(row.physical))
                .ok_or(LaraOperationError::overflow())?;
            rows.push((VertexId::from(row.ordinal), vertex));
        }
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(1)
            .map_err(|_| owner.map_free_span_err())?;
        ranges.push((base, bytes));
        let slab = owner
            .slab
            .prepare_physical_image(header, ranges)
            .map_err(LaraOperationError::ResizeFailed)?;
        let mut writes = Vec::new();
        writes
            .try_reserve(rows.len())
            .map_err(|_| owner.map_free_span_err())?;
        for (id, row) in &rows {
            writes.push(self.source.vertices.prepare_set(*id, row)?);
        }
        Ok(SegmentRewritePreparation {
            source: self.source,
            rows,
            slab,
            writes,
            allocator,
        })
    }
}

impl<V: VertexAccess<LabeledVertex>, M: Memory> SegmentRewritePreparation<'_, V, M> {
    fn row_slot(&self, id: VertexId, index: u32) -> Result<u64, LaraOperationError> {
        let (_, row) = self
            .rows
            .iter()
            .find(|(owner, _)| *owner == id)
            .ok_or(LaraOperationError::overflow())?;
        if index >= row.degree() {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        row.base_slot_start()
            .checked_add(u64::from(index))
            .ok_or(LaraOperationError::overflow())
    }

    pub(crate) fn ready(&mut self) -> Result<ReadySegmentRewrite<'_, M>, LaraOperationError> {
        self.source.validate()?;
        let allocator = self
            .allocator
            .ready()
            .map_err(|_| self.source.owner.map_free_span_err())?;
        Ok(ReadySegmentRewrite {
            writes: &self.writes,
            slab: &self.slab,
            allocator,
        })
    }
}

impl<M: Memory> ReadySegmentRewrite<'_, M> {
    pub(crate) fn publish(self) {
        self.slab.write();
        for write in self.writes {
            write.write();
        }
        self.allocator.publish();
    }
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
const BUCKET_ROW_GROW_NUM: u32 = 5;
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

    #[cfg(test)]
    pub(crate) fn write_label_bucket_slot_for_test(
        &self,
        slot: u64,
        bucket: LabelBucket,
    ) -> Result<(), crate::GrowFailed> {
        let mut bytes = [0u8; LabelBucket::BYTES];
        bucket.write_to(&mut bytes);
        self.slab.write_slot(slot, &bytes)
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
        let mut raw = Vec::new();
        raw.try_reserve_exact(nbytes).ok()?;
        raw.resize(nbytes, 0);
        let mut out = Vec::new();
        out.try_reserve_exact(count as usize).ok()?;
        self.slab.read_slots_contiguous(start_slot, &mut raw);
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
                .ok_or(LaraOperationError::overflow())?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::overflow())?;
            if bucket.bucket_label_key().is_undirected() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for i in lo..hi {
            let slot = base_slot_start
                .checked_add(u64::from(i))
                .ok_or(LaraOperationError::overflow())?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::overflow())?;
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
                .ok_or(LaraOperationError::overflow())?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::overflow())?;
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
                .ok_or(LaraOperationError::overflow())?;
            let bucket = self
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::overflow())?;
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

    pub(crate) fn prepare_existing_row_image(
        &self,
        vertex: LabeledVertex,
        buckets: &[LabelBucket],
    ) -> Result<PreparedExistingBucketRow<'_, M>, LaraOperationError> {
        let header = self.header();
        Self::label_bucket_descriptor_span(vertex, header.elem_capacity)?;
        if vertex.is_default_edge_labeled()
            || buckets.len() != vertex.degree() as usize
            || buckets
                .windows(2)
                .any(|pair| pair[0].bucket_label_key() >= pair[1].bucket_label_key())
        {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        let len = buckets
            .len()
            .checked_mul(LabelBucket::BYTES)
            .ok_or(LaraOperationError::overflow())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| self.map_free_span_err())?;
        bytes.resize(len, 0);
        for (bucket, encoded) in buckets
            .iter()
            .zip(bytes.as_chunks_mut::<{ LabelBucket::BYTES }>().0.iter_mut())
        {
            encode_checked_bucket(*bucket, encoded)?;
        }
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(1)
            .map_err(|_| self.map_free_span_err())?;
        ranges.push((vertex.base_slot_start(), bytes));
        self.slab
            .prepare_physical_image(header, ranges)
            .map(PreparedExistingBucketRow)
            .map_err(LaraOperationError::ResizeFailed)
    }

    /// Prepare one existing descriptor with the same checked codec/backing as a row image.
    pub(crate) fn prepare_existing_bucket_slot(
        &self,
        slot: u64,
        bucket: LabelBucket,
    ) -> Result<PreparedExistingBucketRow<'_, M>, LaraOperationError> {
        let mut bytes = vec![0u8; LabelBucket::BYTES];
        encode_checked_bucket(bucket, &mut bytes)?;
        self.slab
            .prepare_physical_image(self.header(), vec![(slot, bytes)])
            .map(PreparedExistingBucketRow)
            .map_err(LaraOperationError::ResizeFailed)
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
    /// Callers may use this only when every physical edge and inline property bytes field is unchanged.
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
            .ok_or(LaraOperationError::overflow())?;
        scratch.resize(nbytes, 0);
        for (i, bucket) in buckets.iter().enumerate() {
            let lo = i
                .checked_mul(LabelBucket::BYTES)
                .ok_or(LaraOperationError::overflow())?;
            let hi = lo
                .checked_add(LabelBucket::BYTES)
                .ok_or(LaraOperationError::overflow())?;
            bucket.write_to(&mut scratch[lo..hi]);
        }
        self.slab
            .write_slots_contiguous(start_slot, scratch)
            .map_err(LaraOperationError::WriteEdgeSlotFailed)
    }

    /// One destination/accounting rule for the segment owner and native fixture seeding.
    fn descriptor_destination(
        mut header: SlabHeaderV1,
        selected: Option<FreeSpan>,
        len: u64,
    ) -> Result<(u64, SlabHeaderV1), LaraOperationError> {
        let base = if len == 0 {
            0
        } else {
            selected.map_or(header.elem_capacity, |span| span.start_slot)
        };
        let end = crate::slab_index::checked_add_slot_exclusive_end(base, len)
            .ok_or(LaraOperationError::overflow())?;
        if let Some(span) = selected
            && span
                .start_slot
                .checked_add(span.len)
                .is_none_or(|end| end > header.elem_capacity)
        {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        if len > 0 {
            header.elem_capacity = header.elem_capacity.max(end);
            header.slab_occupied_tail = header.slab_occupied_tail.max(end);
            header.num_edges = header.num_edges.max(end);
        }
        Ok((base, header))
    }

    fn map_free_span_err(&self) -> LaraOperationError {
        LaraOperationError::RebalanceFailed(crate::GrowFailed {
            current_size: 0,
            delta: 0,
        })
    }

    /// Seed an allocated fixture span through the same allocator/slab preparation.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn allocate_span(&self, len: u64) -> Result<u64, LaraOperationError> {
        let header = self.header();
        if len == 0 {
            return Ok(header.elem_capacity);
        }
        let mut allocator = self
            .free_spans
            .prepare_replacement(len, &[])
            .map_err(|_| self.map_free_span_err())?;
        let (base, header) = Self::descriptor_destination(header, allocator.selected_span(), len)?;
        let slab = self
            .slab
            .prepare_physical_image(header, Vec::new())
            .map_err(LaraOperationError::ResizeFailed)?;
        let ready = allocator.ready().map_err(|_| self.map_free_span_err())?;
        slab.write();
        ready.publish();
        Ok(base)
    }

    /// Fixture-only free-range seeding; production retirement is grouped preparation.
    #[cfg(test)]
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
            .ok_or(LaraOperationError::overflow())?;
        Ok((start, raw_end.min(vertices.len())))
    }

    /// A source row owns live descriptors and slack only inside the logical slab.
    fn label_bucket_descriptor_span(
        vertex: LabeledVertex,
        capacity: u64,
    ) -> Result<u32, LaraOperationError> {
        let len = vertex
            .label_bucket_descriptor_span()
            .ok_or(LaraOperationError::overflow())?;
        if len != 0
            && crate::slab_index::checked_add_slot_exclusive_end(
                vertex.base_slot_start(),
                u64::from(len),
            )
            .is_none_or(|end| end > capacity)
        {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        Ok(len)
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
            .ok_or(LaraOperationError::overflow())?;
        let grown_u64 = prod.div_ceil(u64::from(BUCKET_ROW_GROW_DEN));
        let grown = u32::try_from(grown_u64).map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        let max_physical = MAX_VERTEX_LABEL_BUCKETS
            .checked_add(u32::from(MAX_VERTEX_LABEL_BUCKET_SLACK))
            .ok_or(LaraOperationError::overflow())?;
        Ok(grown.max(needed_live).min(max_physical))
    }

    fn slack_for_physical_span(
        live_rows: u32,
        physical_span: u32,
    ) -> Result<u16, LaraOperationError> {
        LabeledVertex::bucket_slack_for_descriptor_span(live_rows, physical_span)
            .ok_or(LaraOperationError::overflow())
    }

    fn plan_segment_rewrite<'a, V>(
        &'a self,
        vertices: &'a V,
        targets: &[VertexId],
    ) -> Result<SegmentRewritePlan<'a, V, M>, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let mut bounds = Vec::new();
        for (index, id) in targets.iter().enumerate() {
            vertices.get_in_range(*id)?;
            if targets[..index].contains(id) {
                return Err(LaraOperationError::CollectAllocationOverflow);
            }
            let bound = self.segment_vertex_bounds(vertices, *id)?;
            if !bounds.contains(&bound) {
                bounds.push(bound);
            }
        }
        bounds.sort_unstable();
        let mut source = SegmentSource {
            owner: self,
            vertices,
            header: self.header(),
            vertex_rows: Vec::new(),
            descriptors: Vec::new(),
        };
        let mut rows = Vec::new();
        for (start, end) in bounds {
            for ordinal in start..end {
                let id = VertexId::from(ordinal);
                let vertex = vertices.get(id);
                source.vertex_rows.push((id, vertex));
                if vertex.is_default_edge_labeled() {
                    continue;
                }
                vertex.ensure_valid_normal_row()?;
                let physical =
                    Self::label_bucket_descriptor_span(vertex, source.header.elem_capacity)?;
                let buckets = self
                    .read_label_bucket_slots_contiguous(vertex.base_slot_start(), vertex.degree())
                    .ok_or(LaraOperationError::overflow())?;
                if physical != 0 || !buckets.is_empty() || targets.contains(&id) {
                    source.descriptors.push((id, vertex, buckets.clone()));
                    rows.push(BucketRowRewrite {
                        ordinal,
                        vertex,
                        buckets,
                        physical,
                    });
                }
            }
        }
        Ok(SegmentRewritePlan { source, rows })
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
        let mut plan = self.plan_segment_rewrite(vertices, &[vid])?;
        for row in &mut plan.rows {
            row.physical = u32::try_from(row.buckets.len())
                .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        }
        let mut preparation = plan.reserve()?;
        preparation.ready()?.publish();
        Ok(())
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
        let mut plan = self.plan_segment_rewrite(vertices, &[vid])?;
        if let Some(row) = plan
            .rows
            .iter_mut()
            .find(|row| row.ordinal == u32::from(vid))
        {
            row.buckets.clear();
            row.physical = 0;
        }
        let mut preparation = plan.reserve()?;
        preparation.ready()?.publish();
        Ok(())
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
        let buckets = buckets.ok_or(LaraOperationError::overflow())?;
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
            Self::label_bucket_descriptor_span(v, self.header().elem_capacity)?;
            let mut row = if deg == 0 {
                Vec::new()
            } else {
                self.read_label_bucket_slots_contiguous(base, deg)
                    .ok_or(LaraOperationError::overflow())?
            };
            row.insert(insert_index as usize, bucket);
            let insert_at = base
                .checked_add(u64::from(insert_index))
                .ok_or(LaraOperationError::overflow())?;
            let next_degree = v
                .degree()
                .checked_add(1)
                .filter(|count| LabeledVertex::label_bucket_count_fits(*count))
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
            let updated = v
                .try_with_label_bucket_count(next_degree)?
                .with_bucket_slack_slots(v.bucket_slack_slots().saturating_sub(1))
                .ensure_valid_normal_row()?;
            // Validate the full old row and prepare its updated locator before
            // shifting canonical descriptors. Only the suffix needs re-encoding.
            self.write_label_bucket_slots_contiguous(insert_at, &row[insert_index as usize..])?;
            vertices.set(vid, &updated);
            return Ok((insert_at, false));
        }

        let mut plan = self.plan_segment_rewrite(vertices, &[vid])?;
        let row = plan
            .rows
            .iter_mut()
            .find(|row| row.ordinal == u32::from(vid))
            .ok_or(LaraOperationError::VertexAccess(
                crate::lara::operation_error::VertexAccessError::OutOfRange,
            ))?;
        let index = insert_index as usize;
        if index > row.buckets.len() {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        row.buckets.insert(index, bucket);
        let needed =
            u32::try_from(row.buckets.len()).map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        row.physical = Self::grow_label_bucket_descriptor_span(row.physical, needed)?;
        let mut preparation = plan.reserve()?;
        let out_slot = preparation.row_slot(vid, insert_index)?;
        preparation.ready()?.publish();
        Ok((out_slot, true))
    }

    #[cfg(test)]
    /// Removes one label bucket and rewrites its owning vertex segment.
    ///
    /// This is used by a failed batch reservation to undo a bucket descriptor
    /// that was created only for that reservation. The physical descriptor span
    /// is retained as reusable slack; edge rows and bucket contents are untouched.
    pub(crate) fn remove_label_bucket<V>(
        &self,
        vertices: &V,
        vid: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<bool, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let mut plan = self.plan_segment_rewrite(vertices, &[vid])?;
        let Some(row) = plan
            .rows
            .iter_mut()
            .find(|row| row.ordinal == u32::from(vid))
        else {
            return Ok(false);
        };
        let Some(index) = row
            .buckets
            .iter()
            .position(|bucket| bucket.bucket_label_key() == label_id)
        else {
            return Ok(false);
        };
        row.buckets.remove(index);
        let mut preparation = plan.reserve()?;
        preparation.ready()?.publish();
        Ok(true)
    }

    /// Retire a normal descriptor row and publish its final bypass through the segment owner.
    pub(crate) fn plan_enable_default_edge_bypass<'a, V>(
        &'a self,
        vertices: &'a V,
        vid: VertexId,
        final_row: LabeledVertex,
    ) -> Result<SegmentRewritePlan<'a, V, M>, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        if !final_row.is_default_edge_labeled() {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        let mut plan = self.plan_segment_rewrite(vertices, &[vid])?;
        let row = plan
            .rows
            .iter_mut()
            .find(|row| row.ordinal == u32::from(vid))
            .ok_or(LaraOperationError::overflow())?;
        row.vertex = final_row;
        row.buckets.clear();
        row.physical = 0;
        Ok(plan)
    }

    /// Build a bypass conversion using the same grouped segment source and publisher.
    /// Backing, fields, locators and grouped retirement are admitted by reserve/ready.
    pub(crate) fn plan_promote_bypass_to_bucket_mode<'a, V>(
        &'a self,
        vertices: &'a V,
        vid: VertexId,
        bucket: LabelBucket,
        new_alloc: u32,
    ) -> Result<SegmentRewritePlan<'a, V, M>, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let source = vertices.get_in_range(vid)?;
        if !source.is_default_edge_labeled() {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        let mut plan = self.plan_segment_rewrite(vertices, &[vid])?;
        plan.rows.push(BucketRowRewrite {
            ordinal: u32::from(vid),
            vertex: source
                .without_bypass_spill_run()
                .with_default_edge_labeled(false)
                .with_stored_slots(new_alloc),
            buckets: vec![bucket],
            physical: Self::grow_label_bucket_descriptor_span(0, 1)?,
        });
        plan.rows.sort_unstable_by_key(|row| row.ordinal);
        Ok(plan)
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

        fn prepare_set(
            &self,
            id: VertexId,
            item: &LabeledVertex,
        ) -> Result<PreparedVertexWrite<'_, LabeledVertex>, LaraOperationError> {
            PreparedVertexWrite::value(self, id, *item)
        }
    }

    fn store() -> LabelBucketStore<crate::VectorMemory> {
        LabelBucketStore::new(vector_memory(), vector_memory(), vector_memory(), 64, 4).unwrap()
    }

    #[test]
    fn in_place_descriptor_insert_preserves_prefix_and_shifts_suffix() {
        for (index, label) in [(0, 5), (1, 20), (3, 60)] {
            let buckets = store();
            let base = buckets.allocate_span(4).unwrap();
            let mut expected: Vec<_> = [10, 30, 50]
                .into_iter()
                .map(|label| {
                    LabelBucket::default()
                        .with_bucket_label_key(crate::labeled::BucketLabelKey::from_raw(label))
                })
                .collect();
            buckets
                .write_label_bucket_slots_contiguous(base, &expected)
                .unwrap();
            let vertices = VertexFixture {
                vertex: RefCell::new(
                    LabeledVertex::default()
                        .try_with_bucket_row_and_slack(base, 3, 1)
                        .unwrap(),
                ),
            };
            let inserted = LabelBucket::default()
                .with_bucket_label_key(crate::labeled::BucketLabelKey::from_raw(label));
            assert_eq!(
                buckets
                    .insert_label_bucket_at(&vertices, VertexId::from(0), inserted, index)
                    .unwrap(),
                (base + u64::from(index), false)
            );
            expected.insert(index as usize, inserted);
            assert_eq!(
                buckets.read_label_bucket_slots_contiguous(base, 4).unwrap(),
                expected
            );
            let vertex = vertices.get(VertexId::from(0));
            assert_eq!(
                (
                    vertex.base_slot_start(),
                    vertex.degree(),
                    vertex.bucket_slack_slots()
                ),
                (base, 4, 0)
            );
        }
    }

    #[test]
    fn in_place_descriptor_append_still_validates_unchanged_prefix() {
        let memory = vector_memory();
        let buckets =
            LabelBucketStore::new(memory.clone(), vector_memory(), vector_memory(), 64, 4).unwrap();
        let base = buckets.allocate_span(2).unwrap();
        buckets
            .write_label_bucket_slot(base, LabelBucket::default())
            .unwrap();
        // Invalid values extent at width zero, inside the unchanged prefix.
        buckets
            .slab
            .write_slot_range(base, 16, &1u32.to_le_bytes())
            .unwrap();
        let row = LabeledVertex::default()
            .try_with_bucket_row_and_slack(base, 1, 1)
            .unwrap();
        let vertices = VertexFixture {
            vertex: RefCell::new(row),
        };
        let before = memory.borrow().clone();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            buckets.insert_label_bucket_at(&vertices, VertexId::from(0), LabelBucket::default(), 1)
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<String>().unwrap(),
            "invalid LabelBucket wire bytes: InlinePropertyBytesStateWithoutSchema"
        );
        assert_eq!(*memory.borrow(), before);
        assert_eq!(vertices.get(VertexId::from(0)), row);
    }

    #[test]
    fn in_place_descriptor_count_rejection_keeps_backed_rows_unchanged() {
        use crate::labeled::{BucketLabelKey, record::MAX_VERTEX_LABEL_BUCKETS};

        let count = MAX_VERTEX_LABEL_BUCKETS;
        let mut mutated_indices = Vec::new();
        for index in [0, count / 2, count] {
            let memories = std::array::from_fn::<_, 3, _>(|_| vector_memory());
            let buckets = LabelBucketStore::new(
                memories[0].clone(),
                memories[1].clone(),
                memories[2].clone(),
                u64::from(count) + 1,
                4,
            )
            .unwrap();
            let base = buckets.allocate_span(u64::from(count) + 1).unwrap();
            let descriptors: Vec<_> = (0..count)
                .map(|key| {
                    LabelBucket::default().with_bucket_label_key(BucketLabelKey::from_raw(
                        u16::try_from(key).unwrap(),
                    ))
                })
                .collect();
            buckets
                .write_label_bucket_slots_contiguous(base, &descriptors)
                .unwrap();
            let row = LabeledVertex::default()
                .try_with_bucket_row_and_slack(base, count, 1)
                .unwrap();
            assert_eq!(row.ensure_valid_normal_row(), Ok(row));
            assert!(!row.is_default_edge_labeled());
            assert_eq!(row.bucket_slack_slots(), 1);
            assert_eq!(
                buckets
                    .read_label_bucket_slots_contiguous(base, count)
                    .unwrap(),
                descriptors
            );
            let vertices = VertexFixture {
                vertex: RefCell::new(row),
            };
            let before = memories.each_ref().map(|memory| memory.borrow().clone());
            let mut vertex_before = [0; LabeledVertex::BYTES];
            row.write_to(&mut vertex_before);
            // All wire keys are already present. The store must reject its own
            // count limit before shifting any descriptor, including duplicate keys.
            let inserted = LabelBucket::default().with_bucket_label_key(BucketLabelKey::from_raw(
                u16::try_from(index.min(count - 1)).unwrap(),
            ));
            assert!(matches!(
                buckets.insert_label_bucket_at(&vertices, VertexId::from(0), inserted, index),
                Err(LaraOperationError::RowDegreeOverflow)
            ));
            let mut vertex_after = [0; LabeledVertex::BYTES];
            vertices.get(VertexId::from(0)).write_to(&mut vertex_after);
            assert_eq!(vertex_after, vertex_before);
            if memories
                .iter()
                .zip(&before)
                .any(|(memory, bytes)| *memory.borrow() != *bytes)
            {
                mutated_indices.push(index);
            }
        }
        assert_eq!(
            mutated_indices,
            Vec::<u32>::new(),
            "rejected insertion mutated backed bytes"
        );
    }

    #[test]
    fn degree_only_write_preserves_edge_and_inline_property_bytes_physical_state() {
        let buckets = store();
        let original = LabelBucket::from_parts_with_inline_property(
            crate::labeled::BucketLabelKey::from_raw(7),
            11,
            5,
            9,
            2,
            17,
            4,
            // A slab row carries no log state: bytes 27-28 are the run ids' low bytes there.
            -1,
            0,
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
    fn segment_retirement_grow_rejection_is_unpublished() {
        use crate::{lara::edge::free_span::FreeSpanError, test_support::FailpointMemory};

        struct SegmentVertices(RefCell<[LabeledVertex; 3]>);

        impl VertexAccess<LabeledVertex> for SegmentVertices {
            fn len(&self) -> u32 {
                3
            }

            fn get(&self, id: VertexId) -> LabeledVertex {
                self.0.borrow()[u32::from(id) as usize]
            }

            fn set(&self, id: VertexId, item: &LabeledVertex) {
                self.0.borrow_mut()[u32::from(id) as usize] = *item;
            }

            fn prepare_set(
                &self,
                id: VertexId,
                item: &LabeledVertex,
            ) -> Result<PreparedVertexWrite<'_, LabeledVertex>, LaraOperationError> {
                PreparedVertexWrite::value(self, id, *item)
            }
        }

        let memories = std::array::from_fn::<_, 3, _>(|_| FailpointMemory::new());
        let buckets = LabelBucketStore::new(
            memories[0].clone(),
            memories[1].clone(),
            memories[2].clone(),
            64,
            4,
        )
        .unwrap();
        let pool = buckets.allocate_span(2 * 4096 + 32).unwrap();
        let descriptors: [Vec<LabelBucket>; 3] = std::array::from_fn(|row| {
            (0..2)
                .map(|index| {
                    LabelBucket::from_parts(
                        BucketLabelKey::from_raw((10 * row + index + 1) as u16),
                        (1000 + 100 * row + index) as u64,
                        1,
                        1,
                    )
                })
                .collect()
        });
        let original = std::array::from_fn(|row| {
            let base = pool + 4 * row as u64;
            buckets
                .write_label_bucket_slots_contiguous(base, &descriptors[row])
                .unwrap();
            LabeledVertex::default()
                .try_with_bucket_row_and_slack(base, 2, 2)
                .unwrap()
        });
        let vertices = SegmentVertices(RefCell::new(original));

        // Discover the record boundary through the real writer, without copying
        // its record size/header layout. Short free spans cannot satisfy this rewrite.
        let first_grow = memories[1].grow_count() + 1;
        memories[1].fail_at_grow(first_grow);
        let mut reached_boundary = false;
        for index in 0..4096 {
            match buckets.free_spans.release(FreeSpan {
                start_slot: pool + 32 + 2 * index,
                len: 1,
            }) {
                Ok(()) => {}
                Err(FreeSpanError::OutOfMemory) => {
                    assert_eq!(memories[1].grow_count(), first_grow);
                    reached_boundary = true;
                    break;
                }
                Err(error) => panic!("unexpected fixture admission error: {error}"),
            }
        }
        assert!(reached_boundary, "fixture must reach an actual record grow");
        let spans = buckets.free_spans.spans();
        assert!(spans.len() > 1);
        assert!(spans.iter().all(|span| span.len == 1));
        buckets.free_spans.validate().unwrap();
        let header = buckets.header();
        let before = memories.each_ref().map(|memory| {
            let mut bytes = vec![0; memory.byte_len()];
            memory.read(0, &mut bytes);
            bytes
        });
        let retirement_grow = memories[1].grow_count() + 1;
        memories[1].fail_at_grow(retirement_grow);

        let result = buckets.compact_vertex_segment_for_vertex(&vertices, VertexId::from(0));
        assert!(matches!(
            result,
            Err(LaraOperationError::RebalanceFailed(crate::GrowFailed {
                current_size: 0,
                delta: 0,
            }))
        ));
        assert_eq!(memories[1].grow_count(), retirement_grow);
        eprintln!("reached actual segment-retirement record grow: {retirement_grow}");
        assert_eq!(buckets.free_spans.spans(), spans);
        buckets.free_spans.validate().unwrap();
        for index in [1, 2] {
            let mut bytes = vec![0; memories[index].byte_len()];
            memories[index].read(0, &mut bytes);
            assert_eq!(bytes, before[index], "paired free-span bytes changed");
        }
        assert_eq!(
            *vertices.0.borrow(),
            original,
            "returned rejection published replacement vertex rows"
        );
        assert_eq!(
            buckets.header(),
            header,
            "rejection changed allocator accounting"
        );
        for (row, vertex) in original.iter().enumerate() {
            assert_eq!(
                buckets.read_label_bucket_slots_contiguous(vertex.base_slot_start(), 2),
                Some(descriptors[row].clone())
            );
        }
        drop(buckets);
        let reopened = LabelBucketStore::init(
            memories[0].clone(),
            memories[1].clone(),
            memories[2].clone(),
            64,
            4,
        )
        .unwrap();
        assert_eq!(reopened.header(), header);
        assert_eq!(reopened.free_spans.spans(), spans);
        for (row, vertex) in original.iter().enumerate() {
            assert_eq!(
                reopened.read_label_bucket_slots_contiguous(vertex.base_slot_start(), 2),
                Some(descriptors[row].clone())
            );
        }
    }

    struct SegmentFixtureVertices(RefCell<Vec<LabeledVertex>>);

    impl VertexAccess<LabeledVertex> for SegmentFixtureVertices {
        fn len(&self) -> u32 {
            self.0.borrow().len() as u32
        }
        fn get(&self, id: VertexId) -> LabeledVertex {
            self.0.borrow()[u32::from(id) as usize]
        }
        fn set(&self, id: VertexId, item: &LabeledVertex) {
            self.0.borrow_mut()[u32::from(id) as usize] = *item;
        }
        fn prepare_set(
            &self,
            id: VertexId,
            item: &LabeledVertex,
        ) -> Result<PreparedVertexWrite<'_, LabeledVertex>, LaraOperationError> {
            PreparedVertexWrite::value(self, id, *item)
        }
    }

    struct SegmentFixture {
        store: LabelBucketStore<crate::test_support::FailpointMemory>,
        memories: [crate::test_support::FailpointMemory; 3],
        vertices: SegmentFixtureVertices,
        descriptors: [Vec<LabelBucket>; 3],
        pool: u64,
    }

    struct SegmentSnapshot {
        bytes: [Vec<u8>; 3],
        header: SlabHeaderV1,
        vertices: Vec<LabeledVertex>,
    }

    impl SegmentFixture {
        fn new(segment_size: u32) -> Self {
            let memories = std::array::from_fn(|_| crate::test_support::FailpointMemory::new());
            let store = LabelBucketStore::new(
                memories[0].clone(),
                memories[1].clone(),
                memories[2].clone(),
                64,
                segment_size,
            )
            .unwrap();
            let pool = store.allocate_span(2 * 4096 + 32).unwrap();
            let descriptors = std::array::from_fn(|ordinal| {
                [1, 3]
                    .map(|label| {
                        LabelBucket::from_parts_with_inline_property(
                            BucketLabelKey::from_raw(10 * ordinal as u16 + label),
                            1000 + 100 * ordinal as u64 + u64::from(label),
                            2,
                            8,
                            2,
                            2000 + 100 * ordinal as u64 + u64::from(label),
                            8,
                            -1,
                            0,
                        )
                        .with_edge_spill_run(10 + ordinal as u32)
                        .with_values_spill_run(30 + ordinal as u32)
                    })
                    .to_vec()
            });
            let vertices = SegmentFixtureVertices(RefCell::new(
                (0..3)
                    .map(|ordinal| {
                        let base = pool + 4 * ordinal as u64;
                        store
                            .write_label_bucket_slots_contiguous(base, &descriptors[ordinal])
                            .unwrap();
                        LabeledVertex::default()
                            .try_with_bucket_row_and_slack(base, 2, 2)
                            .unwrap()
                            .with_stored_slots(16 + ordinal as u32)
                    })
                    .collect(),
            ));
            store
                .free_spans
                .release(FreeSpan {
                    start_slot: pool + 300,
                    len: 3,
                })
                .unwrap();
            Self {
                store,
                memories,
                vertices,
                descriptors,
                pool,
            }
        }

        fn bypass_source(&self) -> (LabeledVertex, LabelBucket) {
            // These descriptor slots remain fixture-owned padding. The bypass's
            // locator/run belong to the edge plane, not to this bucket allocator.
            let vertex = LabeledVertex::default()
                .with_default_edge_labeled(true)
                .with_base_slot_start(42)
                .with_degree(6)
                .with_stored_slots(8)
                .with_bypass_spill_run(37);
            self.vertices.set(VertexId::from(0), &vertex);
            let bucket = LabelBucket::from_parts(BucketLabelKey::from_raw(7), 42, 6, 8)
                .with_edge_spill_run(37);
            (vertex, bucket)
        }

        fn snapshot(&self) -> SegmentSnapshot {
            SegmentSnapshot {
                bytes: self.memories.each_ref().map(|memory| {
                    let mut bytes = vec![0; memory.byte_len()];
                    memory.read(0, &mut bytes);
                    bytes
                }),
                header: self.store.header(),
                vertices: self.vertices.0.borrow().clone(),
            }
        }

        fn assert_unchanged(&self, before: &SegmentSnapshot) {
            assert_eq!(self.store.header(), before.header);
            assert_eq!(*self.vertices.0.borrow(), before.vertices);
            for (memory, bytes) in self.memories.iter().zip(&before.bytes) {
                let mut current = vec![0; bytes.len()];
                memory.read(0, &mut current);
                assert_eq!(
                    &current, bytes,
                    "rejection changed pre-existing backed bytes"
                );
            }
            self.assert_reopened(&self.vertices);
        }

        fn assert_rows(
            &self,
            vertices: &impl VertexAccess<LabeledVertex>,
            expected: &[LabeledVertex; 3],
            descriptors: &[Vec<LabelBucket>; 3],
        ) {
            for (ordinal, row) in expected.iter().enumerate() {
                assert_eq!(
                    vertices.get(VertexId::from(ordinal as u32)),
                    *row,
                    "vertex {ordinal}"
                );
                assert_eq!(
                    self.store
                        .read_label_bucket_slots_contiguous(row.base_slot_start(), row.degree()),
                    Some(descriptors[ordinal].clone()),
                    "descriptors for vertex {ordinal}"
                );
            }
            self.assert_reopened(vertices);
        }

        fn assert_reopened(&self, vertices: &impl VertexAccess<LabeledVertex>) {
            self.store.free_spans.validate().unwrap();
            let reopened = LabelBucketStore::init(
                self.memories[0].clone(),
                self.memories[1].clone(),
                self.memories[2].clone(),
                64,
                4,
            )
            .unwrap();
            assert_eq!(reopened.header(), self.store.header());
            assert_eq!(reopened.free_spans.spans(), self.store.free_spans.spans());
            assert_eq!(
                reopened.free_spans.allocator_stats(),
                self.store.free_spans.allocator_stats()
            );
            reopened.free_spans.validate().unwrap();
            for ordinal in 0..vertices.len() {
                let row = vertices.get(VertexId::from(ordinal));
                let mut bytes = [0; LabeledVertex::BYTES];
                row.write_to(&mut bytes);
                assert_eq!(LabeledVertex::try_read_from(&bytes), Ok(row));
                if !row.is_default_edge_labeled() {
                    assert_eq!(
                        reopened.read_label_bucket_slots_contiguous(
                            row.base_slot_start(),
                            row.degree()
                        ),
                        self.store.read_label_bucket_slots_contiguous(
                            row.base_slot_start(),
                            row.degree()
                        )
                    );
                }
            }
        }
    }

    #[test]
    fn segment_group_same_segment_preserves_both_updates() {
        for (segment_size, targets) in [(4, [0, 1]), (4, [1, 0]), (2, [0, 2]), (2, [2, 0])] {
            let fixture = SegmentFixture::new(segment_size);
            let originals: [LabeledVertex; 3] =
                std::array::from_fn(|ordinal| fixture.vertices.get(VertexId::from(ordinal as u32)));
            let mut expected = fixture.descriptors.clone();
            let low = targets[0].min(targets[1]);
            let high = targets[0].max(targets[1]);
            let low_bucket =
                LabelBucket::from_parts(BucketLabelKey::from_raw(10 * low as u16 + 5), 5001, 1, 4);
            let high_bucket =
                LabelBucket::from_parts(BucketLabelKey::from_raw(10 * high as u16 + 2), 6002, 2, 5);
            expected[low as usize].push(low_bucket);
            expected[high as usize].insert(1, high_bucket);
            let mut cursor = fixture.store.header().elem_capacity;
            let expected_rows = std::array::from_fn(|ordinal| {
                let physical = if ordinal == high as usize { 3 } else { 4 };
                let live = expected[ordinal].len() as u32;
                let row = originals[ordinal]
                    .try_with_bucket_row_and_slack(cursor, live, (physical - live) as u16)
                    .unwrap();
                cursor += u64::from(physical);
                row
            });
            let ids = targets.map(VertexId::from);
            let mut plan = fixture
                .store
                .plan_segment_rewrite(&fixture.vertices, &ids)
                .unwrap();
            for row in &mut plan.rows {
                if row.ordinal == low {
                    row.buckets.push(low_bucket);
                } else if row.ordinal == high {
                    row.buckets.insert(1, high_bucket);
                    row.physical = 3;
                }
            }
            let mut preparation = plan.reserve().unwrap();
            preparation.ready().unwrap().publish();
            drop(preparation);
            for (before, after) in originals.iter().zip(&expected_rows) {
                assert_ne!(after.base_slot_start(), before.base_slot_start());
            }
            fixture.assert_rows(&fixture.vertices, &expected_rows, &expected);
            assert_eq!(
                fixture.store.free_spans.spans(),
                vec![
                    FreeSpan {
                        start_slot: fixture.pool,
                        len: 12
                    },
                    FreeSpan {
                        start_slot: fixture.pool + 300,
                        len: 3
                    }
                ]
            );
        }
    }

    #[test]
    fn segment_reservation_abort_preserves_actual_origin() {
        for reuse in [false, true] {
            let first = SegmentFixture::new(4);
            let origin = reuse.then_some(FreeSpan {
                start_slot: first.pool + 64,
                len: 40,
            });
            if let Some(span) = origin {
                first.store.free_spans.release(span).unwrap();
            }
            let second = SegmentFixture::new(4);
            // Fill through the real slab writer until its next backing grow rejects.
            let boundary = second.memories[0].grow_count() + 1;
            second.memories[0].fail_at_grow(boundary);
            let mut reached = false;
            for _ in 0..8192 {
                match second.store.allocate_span(1) {
                    Ok(_) => {}
                    Err(LaraOperationError::ResizeFailed(_)) => {
                        reached = true;
                        break;
                    }
                    Err(error) => panic!("unexpected slab-boundary fixture error: {error}"),
                }
            }
            assert!(reached);
            assert_eq!(second.memories[0].grow_count(), boundary);
            let before_first = first.snapshot();
            let before_second = second.snapshot();
            let original_spans = first.store.free_spans.spans();
            let rejection = second.memories[0].grow_count() + 1;
            second.memories[0].fail_at_grow(rejection);
            let mut preparation = first
                .store
                .plan_segment_rewrite(&first.vertices, &[VertexId::from(0)])
                .unwrap()
                .reserve()
                .unwrap();
            assert_eq!(preparation.allocator.selected_span(), origin);
            assert_eq!(
                preparation.row_slot(VertexId::from(0), 0).unwrap(),
                origin.map_or(before_first.header.elem_capacity, |span| span.start_slot)
            );
            let ready = preparation.ready().unwrap();
            assert!(matches!(
                second
                    .store
                    .plan_segment_rewrite(&second.vertices, &[VertexId::from(0)])
                    .unwrap()
                    .reserve(),
                Err(LaraOperationError::ResizeFailed(_))
            ));
            assert_eq!(second.memories[0].grow_count(), rejection);
            // The rejected second preparation has already discarded its guard;
            // discard the first ready image and then its owner guard, in reverse order.
            drop(ready);
            drop(preparation);
            first.assert_unchanged(&before_first);
            second.assert_unchanged(&before_second);
            assert_eq!(first.store.free_spans.spans(), original_spans);
            if let Some(span) = origin {
                assert!(first.store.free_spans.spans().contains(&span));
            }
        }
    }

    #[test]
    fn segment_ready_rejects_stale_source_and_invalid_rows() {
        // Live descriptors are backed, but declared slack crosses the logical slab end.
        // Retiring that unchecked span could make the appended destination free.
        let fixture = SegmentFixture::new(4);
        let id = VertexId::from(2);
        let base = fixture.store.header().elem_capacity - 2;
        fixture
            .store
            .write_label_bucket_slots_contiguous(base, &fixture.descriptors[2])
            .unwrap();
        let row = fixture
            .vertices
            .get(id)
            .try_with_bucket_row_and_slack(base, 2, 2)
            .unwrap();
        fixture.vertices.set(id, &row);
        let before = fixture.snapshot();
        let result = fixture
            .store
            .compact_vertex_segment_for_vertex(&fixture.vertices, VertexId::from(0));
        assert!(
            matches!(result, Err(LaraOperationError::CollectAllocationOverflow)),
            "untargeted peer slack must remain inside source capacity: {result:?}"
        );
        fixture.assert_unchanged(&before);
        assert!(
            matches!(
                fixture.store.insert_label_bucket_at(
                    &fixture.vertices,
                    id,
                    LabelBucket::from_parts(BucketLabelKey::from_raw(22), 7100, 1, 4),
                    1
                ),
                Err(LaraOperationError::CollectAllocationOverflow)
            ),
            "in-place insertion must share complete source-span validation"
        );
        fixture.assert_unchanged(&before);

        for changed in ["vertex", "descriptor", "header"] {
            let fixture = SegmentFixture::new(4);
            let mut preparation = fixture
                .store
                .plan_segment_rewrite(&fixture.vertices, &[VertexId::from(0)])
                .unwrap()
                .reserve()
                .unwrap();
            match changed {
                "vertex" => {
                    let row = fixture
                        .vertices
                        .get(VertexId::from(1))
                        .with_stored_slots(77);
                    fixture.vertices.set(VertexId::from(1), &row);
                }
                "descriptor" => {
                    let row = fixture.vertices.get(VertexId::from(1));
                    fixture
                        .store
                        .write_label_bucket_slot(
                            row.base_slot_start(),
                            fixture.descriptors[1][0].with_degree_field(1),
                        )
                        .unwrap();
                }
                "header" => fixture
                    .store
                    .slab
                    .set_num_edges(fixture.store.header().num_edges - 1),
                _ => unreachable!(),
            }
            // Snapshot the newer source: discard must not undo another invocation's work.
            let current = fixture.snapshot();
            assert!(matches!(
                preparation.ready(),
                Err(LaraOperationError::CollectAllocationOverflow)
            ));
            drop(preparation);
            fixture.assert_unchanged(&current);
        }
        for invalid in [
            "physical",
            "encoding",
            "order",
            "missing",
            "duplicate",
            "mode",
        ] {
            let fixture = SegmentFixture::new(4);
            let before = fixture.snapshot();
            let mut plan = fixture
                .store
                .plan_segment_rewrite(&fixture.vertices, &[VertexId::from(0)])
                .unwrap();
            match invalid {
                "physical" => plan.rows[0].physical = 1,
                "encoding" => {
                    plan.rows[0].buckets[0] = plan.rows[0].buckets[0].with_rewritten_value_width(0)
                }
                "order" => plan.rows[0].buckets.swap(0, 1),
                "missing" => {
                    plan.rows.pop();
                }
                "duplicate" => plan.rows[2].ordinal = 1,
                "mode" => plan.rows[0].vertex = plan.rows[0].vertex.with_default_edge_labeled(true),
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    plan.reserve(),
                    Err(LaraOperationError::CollectAllocationOverflow)
                ),
                "{invalid} must reject before publication"
            );
            fixture.assert_unchanged(&before);
        }
        let fixture = SegmentFixture::new(4);
        let before = fixture.snapshot();
        assert!(matches!(
            fixture
                .store
                .plan_segment_rewrite(&fixture.vertices, &[VertexId::from(0), VertexId::from(0)]),
            Err(LaraOperationError::CollectAllocationOverflow)
        ));
        let plan = fixture
            .store
            .plan_segment_rewrite(&fixture.vertices, &[VertexId::from(0)])
            .unwrap();
        let row = fixture
            .vertices
            .get(VertexId::from(2))
            .with_stored_slots(99);
        fixture.vertices.set(VertexId::from(2), &row);
        let current = fixture.snapshot();
        assert!(matches!(
            plan.reserve(),
            Err(LaraOperationError::CollectAllocationOverflow)
        ));
        fixture.assert_unchanged(&current);
        assert_eq!(fixture.store.header(), before.header);

        struct Unprepared<'a>(&'a SegmentFixtureVertices);
        impl VertexAccess<LabeledVertex> for Unprepared<'_> {
            fn len(&self) -> u32 {
                self.0.len()
            }
            fn get(&self, id: VertexId) -> LabeledVertex {
                self.0.get(id)
            }
            fn set(&self, id: VertexId, item: &LabeledVertex) {
                self.0.set(id, item);
            }
        }
        let fixture = SegmentFixture::new(4);
        let before = fixture.snapshot();
        let unprepared = Unprepared(&fixture.vertices);
        let plan = fixture
            .store
            .plan_segment_rewrite(&unprepared, &[VertexId::from(0)])
            .unwrap();
        assert!(matches!(
            plan.reserve(),
            Err(LaraOperationError::CollectAllocationOverflow)
        ));
        fixture.assert_unchanged(&before);
    }

    #[test]
    fn segment_publication_success_paths_reopen() {
        let fixture = SegmentFixture::new(4);
        let vertex_memory = crate::test_support::FailpointMemory::new();
        let vertices =
            crate::lara::vertex::VertexStore::<LabeledVertex, _>::new(vertex_memory.clone())
                .unwrap();
        for row in fixture.vertices.0.borrow().iter() {
            vertices.push(*row).unwrap();
        }
        let originals: [LabeledVertex; 3] =
            std::array::from_fn(|ordinal| vertices.get(VertexId::from(ordinal as u32)));
        let mut expected_descriptors = fixture.descriptors.clone();
        let compact_base = fixture.store.header().elem_capacity;
        let mut expected_rows = std::array::from_fn(|ordinal| {
            originals[ordinal]
                .try_with_bucket_row_and_slack(compact_base + 2 * ordinal as u64, 2, 0)
                .unwrap()
        });
        fixture
            .store
            .compact_vertex_segment_for_vertex(&vertices, VertexId::from(0))
            .unwrap();
        fixture.assert_rows(&vertices, &expected_rows, &expected_descriptors);
        let inserted = LabelBucket::from_parts(BucketLabelKey::from_raw(2), 7000, 3, 5);
        let (slot, moved) = fixture
            .store
            .insert_label_bucket_at(&vertices, VertexId::from(0), inserted, 1)
            .unwrap();
        assert!(moved);
        assert_eq!(slot, fixture.pool + 1);
        assert_eq!(fixture.store.read_label_bucket_slot(slot), Some(inserted));
        expected_descriptors[0].insert(1, inserted);
        for (ordinal, (offset, live, slack)) in
            [(0, 3, 2), (5, 2, 0), (7, 2, 0)].into_iter().enumerate()
        {
            expected_rows[ordinal] = originals[ordinal]
                .try_with_bucket_row_and_slack(fixture.pool + offset, live, slack)
                .unwrap();
        }
        fixture.assert_rows(&vertices, &expected_rows, &expected_descriptors);
        let removal_base = fixture.store.header().elem_capacity;
        assert!(
            fixture
                .store
                .remove_label_bucket(&vertices, VertexId::from(0), inserted.bucket_label_key())
                .unwrap()
        );
        expected_descriptors[0] = fixture.descriptors[0].clone();
        for (ordinal, (offset, slack)) in [(0, 3), (5, 0), (7, 0)].into_iter().enumerate() {
            expected_rows[ordinal] = originals[ordinal]
                .try_with_bucket_row_and_slack(removal_base + offset, 2, slack)
                .unwrap();
        }
        fixture.assert_rows(&vertices, &expected_rows, &expected_descriptors);
        fixture
            .store
            .clear_vertex_label_buckets(&vertices, VertexId::from(1))
            .unwrap();
        expected_descriptors[1].clear();
        for (ordinal, (offset, live, slack)) in
            [(0, 2, 3), (5, 0, 0), (5, 2, 0)].into_iter().enumerate()
        {
            expected_rows[ordinal] = originals[ordinal]
                .try_with_bucket_row_and_slack(fixture.pool + offset, live, slack)
                .unwrap();
        }
        fixture.assert_rows(&vertices, &expected_rows, &expected_descriptors);
        let reopened_vertices =
            crate::lara::vertex::VertexStore::<LabeledVertex, _>::init(vertex_memory).unwrap();
        for ordinal in 0..3 {
            assert_eq!(
                reopened_vertices.get(VertexId::from(ordinal)),
                vertices.get(VertexId::from(ordinal))
            );
        }

        let bypass = SegmentFixture::new(4);
        let (_, bucket) = bypass.bypass_source();
        let plan = bypass
            .store
            .plan_promote_bypass_to_bucket_mode(&bypass.vertices, VertexId::from(0), bucket, 8)
            .unwrap();
        let mut preparation = plan.reserve().unwrap();
        preparation.ready().unwrap().publish();
        drop(preparation);
        let row = bypass.vertices.get(VertexId::from(0));
        assert!(!row.is_default_edge_labeled());
        assert_eq!(
            (
                row.degree(),
                row.bucket_slack_slots(),
                row.stored_slots,
                row.bypass_spill_run()
            ),
            (1, 4, 8, crate::traits::NO_SPILL_RUN)
        );
        assert_eq!(
            bypass.store.read_label_bucket_slot(row.base_slot_start()),
            Some(bucket)
        );
        for ordinal in [1, 2] {
            let peer = bypass.vertices.get(VertexId::from(ordinal));
            assert_eq!(
                (peer.degree(), peer.bucket_slack_slots(), peer.stored_slots),
                (2, 2, 16 + ordinal)
            );
            assert_eq!(
                bypass
                    .store
                    .read_label_bucket_slots_contiguous(peer.base_slot_start(), 2),
                Some(bypass.descriptors[ordinal as usize].clone())
            );
        }
        assert_eq!(
            bypass.store.free_spans.spans(),
            vec![
                FreeSpan {
                    start_slot: bypass.pool + 4,
                    len: 8
                },
                FreeSpan {
                    start_slot: bypass.pool + 300,
                    len: 3
                }
            ]
        );
        bypass.assert_reopened(&bypass.vertices);
    }

    #[test]
    fn segment_bypass_rejection_is_unpublished() {
        use crate::lara::edge::free_span::FreeSpanError;
        let fixture = SegmentFixture::new(4);
        let (source, bucket) = fixture.bypass_source();
        let boundary = fixture.memories[2].grow_count() + 1;
        fixture.memories[2].fail_at_grow(boundary);
        let mut reached = false;
        // Descending keys fill the same leftmost page that retirement must edit.
        // Fixture growth discovers the actual directory/page boundary, not a copied layout.
        for index in 0..4096 {
            let span = FreeSpan {
                start_slot: fixture.pool + 32 + 2 * (4095 - index),
                len: 1,
            };
            match fixture.store.free_spans.release(span) {
                Ok(()) => {}
                Err(FreeSpanError::OutOfMemory) => {
                    reached = true;
                    break;
                }
                Err(error) => panic!("unexpected by-start-boundary fixture error: {error}"),
            }
        }
        assert!(reached);
        assert_eq!(fixture.memories[2].grow_count(), boundary);
        let before = fixture.snapshot();
        let spans = fixture.store.free_spans.spans();
        let rejection = fixture.memories[2].grow_count() + 1;
        fixture.memories[2].fail_at_grow(rejection);
        let plan = fixture
            .store
            .plan_promote_bypass_to_bucket_mode(&fixture.vertices, VertexId::from(0), bucket, 8)
            .unwrap();
        let mut preparation = plan.reserve().unwrap();
        assert!(matches!(
            preparation.ready(),
            Err(LaraOperationError::RebalanceFailed(crate::GrowFailed {
                current_size: 0,
                delta: 0
            }))
        ));
        assert_eq!(fixture.memories[2].grow_count(), rejection);
        eprintln!("reached actual bypass retirement by-start grow: {rejection}");
        drop(preparation);
        fixture.assert_unchanged(&before);
        assert_eq!(fixture.store.free_spans.spans(), spans);
        assert_eq!(fixture.vertices.get(VertexId::from(0)), source);
        for ordinal in [1, 2] {
            let row = fixture.vertices.get(VertexId::from(ordinal));
            assert_eq!(
                fixture
                    .store
                    .read_label_bucket_slots_contiguous(row.base_slot_start(), 2),
                Some(fixture.descriptors[ordinal as usize].clone())
            );
        }
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

#[cfg(feature = "canbench")]
fn bench_segment_rewrite(n: u32) -> canbench_rs::BenchResult {
    use crate::bench::{BenchMemory, BenchMemoryFactory};
    use std::cell::RefCell;

    struct Rows(RefCell<Vec<LabeledVertex>>);

    impl VertexAccess<LabeledVertex> for Rows {
        fn len(&self) -> u32 {
            self.0.borrow().len() as u32
        }

        fn get(&self, id: VertexId) -> LabeledVertex {
            self.0.borrow()[u32::from(id) as usize]
        }

        fn set(&self, id: VertexId, item: &LabeledVertex) {
            self.0.borrow_mut()[u32::from(id) as usize] = *item;
        }

        fn prepare_set(
            &self,
            id: VertexId,
            item: &LabeledVertex,
        ) -> Result<PreparedVertexWrite<'_, LabeledVertex>, LaraOperationError> {
            PreparedVertexWrite::value(self, id, *item)
        }
    }

    fn fixture(memories: &mut BenchMemoryFactory, n: u32) -> (LabelBucketStore<BenchMemory>, Rows) {
        let store = LabelBucketStore::new(
            memories.memory(),
            memories.memory(),
            memories.memory(),
            64,
            32,
        )
        .expect("bucket store");
        let base = store.allocate_span(4 * u64::from(n)).expect("source span");
        let mut rows = Vec::new();
        for ordinal in 0..n {
            let start = base + 4 * u64::from(ordinal);
            let buckets = [1, 2].map(|key| {
                LabelBucket::from_parts(
                    BucketLabelKey::from_raw(key),
                    100 * u64::from(ordinal) + u64::from(key),
                    1,
                    1,
                )
            });
            store
                .write_label_bucket_slots_contiguous(start, &buckets)
                .expect("source descriptors");
            rows.push(
                LabeledVertex::default()
                    .try_with_bucket_row_and_slack(start, 2, 2)
                    .expect("source row"),
            );
        }
        (store, Rows(RefCell::new(rows)))
    }

    // Separate regions: validate the actual mutation before measuring an identical fixture.
    let mut memories = BenchMemoryFactory::new();
    let (probe, probe_rows) = fixture(&mut memories, n);
    let originals = probe_rows.0.borrow().clone();
    let expected: Vec<_> = originals
        .iter()
        .map(|vertex| {
            probe
                .read_label_bucket_slots_contiguous(vertex.base_slot_start(), 2)
                .expect("source descriptors")
        })
        .collect();
    probe
        .compact_vertex_segment_for_vertex(&probe_rows, VertexId::from(0))
        .expect("probe rewrite");
    for (ordinal, vertex) in probe_rows.0.borrow().iter().enumerate() {
        assert_ne!(
            vertex.base_slot_start(),
            originals[ordinal].base_slot_start()
        );
        assert_eq!((vertex.degree(), vertex.bucket_slack_slots()), (2, 0));
        assert_eq!(
            probe.read_label_bucket_slots_contiguous(vertex.base_slot_start(), 2),
            Some(expected[ordinal].clone())
        );
    }
    assert_eq!(
        probe.free_spans.allocator_stats().free_bytes,
        4 * u64::from(n)
    );
    probe.free_spans.validate().expect("probe allocator");
    let (store, rows) = fixture(&mut memories, n);
    canbench_rs::bench_fn(|| {
        store
            .compact_vertex_segment_for_vertex(&rows, VertexId::from(0))
            .expect("segment rewrite");
    })
}

#[cfg(feature = "canbench")]
#[canbench_rs::bench(raw)]
fn bench_r_bs_segment_rw_2() -> canbench_rs::BenchResult {
    bench_segment_rewrite(2)
}

#[cfg(feature = "canbench")]
#[canbench_rs::bench(raw)]
fn bench_r_bs_segment_rw_32() -> canbench_rs::BenchResult {
    bench_segment_rewrite(32)
}
