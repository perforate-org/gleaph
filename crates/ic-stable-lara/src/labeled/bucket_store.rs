//! Dedicated stable storage for LabelBucket rows.
//!
//! LabelBuckets are grouped by VertexSegment (32 vertices by default), but the
//! LabelBucketStore deliberately has no overflow log. When a vertex gains a new
//! LabelBucket, the owning VertexSegment is rewritten immediately into a
//! physical span whose length is exactly the segment's live bucket count.
//!
//! This store owns only bucket descriptors. It does not know or reserve edge
//! capacity. Edge capacity belongs to [`LabeledVertex::vertex_edge_alloc_slots`]
//! and is managed by `LabeledLaraGraph` when it rewrites a VertexEdgeSpan.

use crate::{
    VertexId,
    labeled::{record::LabelBucket, record::LabeledVertex},
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

/// Errors returned when reopening a [`LabelBucketStore`].
#[derive(Debug)]
pub enum InitError {
    /// The bucket slab could not be reopened.
    Slab(crate::lara::edge::SlabInitError),
    /// Free-span metadata could not be initialized.
    FreeSpan,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Slab(err) => write!(f, "bucket slab init failed: {err}"),
            Self::FreeSpan => write!(f, "bucket free-span init failed"),
        }
    }
}

impl std::error::Error for InitError {}

/// Stable LabelBucket slab plus free-span metadata.
pub struct LabelBucketStore<M: Memory> {
    slab: EdgeSlabStore<LabelBucket, M>,
    free_spans: FreeSpanStore<M>,
}

impl<M: Memory> LabelBucketStore<M> {
    /// Opens a fresh LabelBucketStore over three stable memories.
    pub fn new(
        slab: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        slots_per_vertex: u32,
    ) -> Result<Self, crate::GrowFailed> {
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
    pub fn init(
        slab: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        slots_per_vertex: u32,
    ) -> Result<Self, InitError> {
        if slab.size() == 0 {
            return Self::new(
                slab,
                free_spans,
                free_span_by_start,
                elem_capacity,
                slots_per_vertex,
            )
            .map_err(|_| InitError::FreeSpan);
        }
        let slab = EdgeSlabStore::init(slab).map_err(InitError::Slab)?;
        let free_spans =
            FreeSpanStore::init(free_spans, free_span_by_start).map_err(|_| InitError::FreeSpan)?;
        Ok(Self { slab, free_spans })
    }

    /// Returns the bucket slab header (shared on-disk layout with edge slabs).
    pub fn header(&self) -> SlabHeaderV1 {
        self.slab.header().expect("bucket slab header")
    }

    /// Number of bucket cells ever allocated in the slab tail.
    pub fn len(&self) -> u64 {
        self.header().slab_occupied_tail
    }

    /// Returns whether the bucket store is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads one bucket slab slot.
    pub fn read_label_bucket_slot(&self, slot: u64) -> Option<LabelBucket> {
        if slot >= self.header().elem_capacity {
            return None;
        }
        let mut bytes = [0u8; LabelBucket::BYTES];
        self.slab.read_slot(slot, &mut bytes);
        Some(LabelBucket::read_from(&bytes))
    }

    /// Writes one bucket slab slot.
    pub fn write_label_bucket_slot(
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

    fn grow_capacity_to_fit(&self, slot: u64) -> Result<(), LaraOperationError> {
        let cap = self.header().elem_capacity;
        if slot < cap {
            return Ok(());
        }
        let next = slot.saturating_add(1);
        self.slab
            .set_elem_capacity(next)
            .map_err(LaraOperationError::ResizeFailed)
    }

    fn record_allocation(&self, last_slot: u64) {
        let mut header = self.header();
        let tail = last_slot.saturating_add(1);
        if tail > header.slab_occupied_tail {
            header.slab_occupied_tail = tail;
        }
        if tail > header.num_edges {
            header.num_edges = tail;
        }
        self.slab.write_header(&header);
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
        self.grow_capacity_to_fit(start.saturating_add(len).saturating_sub(1))?;
        self.record_allocation(start.saturating_add(len).saturating_sub(1));
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

    fn segment_vertex_bounds<V>(&self, vertices: &V, vid: VertexId) -> (u32, u32)
    where
        V: VertexAccess<LabeledVertex>,
    {
        let segment_size = self.header().segment_size.max(1);
        let start = (u32::from(vid) / segment_size) * segment_size;
        let end = start.saturating_add(segment_size).min(vertices.len());
        (start, end)
    }

    fn collect_segment_bucket_rows<V>(
        &self,
        vertices: &V,
        vid: VertexId,
    ) -> Result<Vec<(u32, LabeledVertex, Vec<LabelBucket>)>, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let (start, end) = self.segment_vertex_bounds(vertices, vid);
        let mut rows = Vec::new();
        for v_ord in start..end {
            let v = vertices.get(VertexId::from(v_ord));
            if v.is_default_edge_labeled() {
                continue;
            }
            let mut buckets = Vec::with_capacity(v.degree() as usize);
            for offset in 0..u64::from(v.degree()) {
                buckets.push(
                    self.read_label_bucket_slot(v.base_slot_start().saturating_add(offset))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                );
            }
            rows.push((v_ord, v, buckets));
        }
        Ok(rows)
    }

    fn rewrite_segment_bucket_rows<V>(
        &self,
        vertices: &V,
        rows: Vec<(u32, LabeledVertex, Vec<LabelBucket>)>,
    ) -> Result<(), LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let total: u64 = rows
            .iter()
            .map(|(_, _, buckets)| buckets.len() as u64)
            .sum();
        let mut old_spans: Vec<(u64, u64)> = rows
            .iter()
            .filter_map(|(_, v, _)| {
                (v.degree() > 0).then_some((v.base_slot_start(), u64::from(v.degree())))
            })
            .collect();
        old_spans.sort_unstable_by_key(|(start, _)| *start);

        let new_base = if total == 0 {
            0
        } else {
            self.allocate_span(total)?
        };
        let mut cursor = new_base;
        for (v_ord, v, buckets) in rows {
            let row_base = cursor;
            for bucket in &buckets {
                self.write_label_bucket_slot(cursor, *bucket)?;
                cursor = cursor.saturating_add(1);
            }
            vertices.set(
                VertexId::from(v_ord),
                &v.with_bucket_row(row_base, buckets.len() as u32),
            );
        }

        for (start, len) in old_spans {
            self.release_span(start, len)?;
        }
        if total > 0 {
            self.record_allocation(new_base.saturating_add(total).saturating_sub(1));
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
        let rows = self.collect_segment_bucket_rows(vertices, vid)?;
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
        for (v_ord, _, buckets) in &mut rows {
            if *v_ord == u32::from(vid) {
                buckets.clear();
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
    pub(crate) fn insert_label_bucket<V>(
        &self,
        vertices: &V,
        vid: VertexId,
        bucket: LabelBucket,
    ) -> Result<u64, LaraOperationError>
    where
        V: VertexAccess<LabeledVertex>,
    {
        let mut rows = self.collect_segment_bucket_rows(vertices, vid)?;
        let mut inserted_index = None;
        for (v_ord, _, buckets) in &mut rows {
            if *v_ord == u32::from(vid) {
                let index = buckets
                    .binary_search_by_key(&bucket.label_id, |candidate| candidate.label_id)
                    .unwrap_or_else(|index| index);
                inserted_index = Some(index as u64);
                buckets.insert(index, bucket);
                break;
            }
        }
        let inserted_index = inserted_index.ok_or(LaraOperationError::VertexAccess(
            crate::lara::operation_error::VertexAccessError::OutOfRange,
        ))?;
        self.rewrite_segment_bucket_rows(vertices, rows)?;
        let v = vertices.get_in_range(vid)?;
        Ok(v.base_slot_start().saturating_add(inserted_index))
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
                    LabelBucket {
                        label_id: crate::labeled::record::LabelId::from_raw(label),
                        edge_start: 0,
                        edge_len: 0,
                    },
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
                bucket.label_id,
                crate::labeled::record::LabelId::from_raw(offset as u16)
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
                    LabelBucket {
                        label_id: crate::labeled::record::LabelId::from_raw(label),
                        edge_start: 0,
                        edge_len: 0,
                    },
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
