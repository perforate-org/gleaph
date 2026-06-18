//! EdgeStore `init` implementation.

use crate::{GrowFailed, SegmentId, VertexCount, traits::CsrEdge};
use ic_stable_structures::Memory;
use std::cell::Cell;

use super::EdgeStore;
use super::InitError;
use super::counts::{SegmentEdgeCounts, SegmentEdgeCountsStore};
pub(crate) use super::edges::EdgeSlabStore;
use super::edges::tree_height_for_segment_count;
use super::edges::{HeaderV1 as EdgeHeaderV1, segment_tree_leaf_count};
use super::free_span::FreeSpanStore;
use super::log::HeaderV1 as LogHeaderV1;
use super::log::LogStore;
use super::span_meta::{SegmentSpanMeta, SegmentSpanMetaStore};

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    /// Creates a new edge store over empty stable memories.
    pub fn new(
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, GrowFailed> {
        crate::slab_index::validate_elem_capacity_grow_failed(elem_capacity, edges.size())?;
        let segment_count = segment_tree_leaf_count(VertexCount::default(), segment_size);
        let header = EdgeHeaderV1::new(
            elem_capacity,
            segment_count,
            segment_size,
            E::BYTES as u32,
            initial_vertex_edge_slots,
        );
        let counts = SegmentEdgeCountsStore::new(counts)?;
        for _ in 0..u64::from(header.segment_count).saturating_mul(2) {
            counts.push(SegmentEdgeCounts {
                actual: 0,
                total: 0,
            })?;
        }
        let log_header = LogHeaderV1::new(header.segment_count, header.stride);
        let span_meta = SegmentSpanMetaStore::new(span_meta)?;
        for _ in 0..u64::from(header.segment_count) {
            span_meta.push(SegmentSpanMeta::default())?;
        }
        let edges = EdgeSlabStore::new(edges, header)?;
        let log = LogStore::new(log, log_header)?;
        let free_spans =
            FreeSpanStore::new(free_spans, free_span_by_start).map_err(|_| GrowFailed {
                current_size: 0,
                delta: 0,
            })?;
        Ok(Self {
            counts,
            edges,
            header: Cell::new(header),
            log,
            span_meta,
            free_spans,
        })
    }

    /// Reopens an edge store from stable memories, creating it when the edge slab is empty.
    pub fn init(
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, InitError> {
        match crate::classify_composite_init([
            counts.size(),
            edges.size(),
            log.size(),
            span_meta.size(),
            free_spans.size(),
            free_span_by_start.size(),
        ]) {
            crate::CompositeInit::Fresh => {
                return Self::new(
                    counts,
                    edges,
                    log,
                    span_meta,
                    free_spans,
                    free_span_by_start,
                    elem_capacity,
                    segment_size,
                    initial_vertex_edge_slots,
                )
                .map_err(|_| InitError::OutOfMemory);
            }
            crate::CompositeInit::Partial => {
                return Err(InitError::PartialLayout);
            }
            crate::CompositeInit::Reopen => {}
        }
        let counts = SegmentEdgeCountsStore::init(counts).map_err(InitError::Counts)?;
        let edges = EdgeSlabStore::init(edges).map_err(InitError::Edges)?;
        let header = edges.header().map_err(InitError::Edges)?;
        let _ = elem_capacity;
        let log = LogStore::init(log).map_err(InitError::Log)?;
        let span_meta = SegmentSpanMetaStore::init(span_meta).map_err(InitError::SpanMeta)?;
        let free_spans = FreeSpanStore::init(free_spans, free_span_by_start)
            .map_err(|_| InitError::SpanMetaLayoutMismatch)?;
        let log_header = log.header();
        if log_header.segment_count != header.segment_count {
            return Err(InitError::LogLayoutMismatch);
        }
        if span_meta.len() != u64::from(header.segment_count) {
            return Err(InitError::SpanMetaLayoutMismatch);
        }
        if counts.len() != u64::from(header.segment_count).saturating_mul(2) {
            return Err(InitError::SpanMetaLayoutMismatch);
        }
        Ok(Self {
            counts,
            edges,
            header: Cell::new(header),
            log,
            span_meta,
            free_spans,
        })
    }

    pub(crate) fn grow_segment_tree_to(&self, new_segment_count: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let old = h.segment_count;
        if new_segment_count <= old {
            return Ok(());
        }
        self.migrate_counts_for_segment_grow(old, new_segment_count)?;
        for _ in old..new_segment_count {
            self.span_meta.push(SegmentSpanMeta::default())?;
        }
        self.log.grow_segment_count_to(new_segment_count)?;
        let mut nh = h;
        nh.segment_count = new_segment_count;
        nh.tree_height = tree_height_for_segment_count(new_segment_count);
        self.write_header(&nh);
        Ok(())
    }

    pub(super) fn migrate_counts_for_segment_grow(
        &self,
        old_l: u32,
        new_l: u32,
    ) -> Result<(), GrowFailed> {
        let mut leaf_vals: Vec<SegmentEdgeCounts> = Vec::with_capacity(old_l as usize);
        for leaf in 0..old_l {
            let idx = u64::from(old_l + leaf);
            leaf_vals.push(self.counts.get(idx));
        }
        let target_len = u64::from(new_l).saturating_mul(2);
        while self.counts.len() < target_len {
            self.counts.push(SegmentEdgeCounts {
                actual: 0,
                total: 0,
            })?;
        }
        for leaf in 0..old_l {
            self.counts
                .set(u64::from(new_l + leaf), &leaf_vals[leaf as usize]);
        }
        for leaf in old_l..new_l {
            self.counts.set(
                u64::from(new_l + leaf),
                &SegmentEdgeCounts {
                    actual: 0,
                    total: 0,
                },
            );
        }
        for idx in (1..new_l).rev() {
            let left = self.counts.get(u64::from(idx * 2));
            let right = self.counts.get(u64::from(idx * 2 + 1));
            self.counts.set(
                u64::from(idx),
                &SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                },
            );
        }
        self.counts.set(
            0,
            &SegmentEdgeCounts {
                actual: 0,
                total: 0,
            },
        );
        Ok(())
    }

    /// Returns the cached edge-store header.
    pub fn header(&self) -> EdgeHeaderV1 {
        self.header.get()
    }

    pub(super) fn write_header(&self, header: &EdgeHeaderV1) {
        self.edges.write_header(header);
        self.header.set(*header);
    }

    /// Returns the segment edge-count store.
    pub fn counts_store(&self) -> &SegmentEdgeCountsStore<E, M> {
        &self.counts
    }

    /// Returns the segment span-metadata store.
    pub fn span_meta_store(&self) -> &SegmentSpanMetaStore<M> {
        &self.span_meta
    }

    /// Returns the free-span index for retired slab ranges.
    pub fn free_span_store(&self) -> &FreeSpanStore<M> {
        &self.free_spans
    }

    /// Decomposes the edge store into its backing memories.
    pub fn into_memories(self) -> (M, M, M, M, M, M) {
        let (free_spans, free_span_by_start) = self.free_spans.into_memories();
        (
            self.counts.into_memory(),
            self.edges.into_memory(),
            self.log.into_memory(),
            self.span_meta.into_memory(),
            free_spans,
            free_span_by_start,
        )
    }

    /// Clears and releases the overflow-log segment for `leaf_segment`.
    pub fn release_log_segment(&self, leaf_segment: SegmentId) -> Result<(), GrowFailed> {
        self.log.release_segment(u32::from(leaf_segment))
    }

    /// Returns the high-water entry index for `leaf_segment` (`0` when unused).
    pub(crate) fn overflow_log_segment_high_water(&self, leaf_segment: u32) -> u32 {
        let h = self.log.header();
        self.log.read_idx_with_header(&h, leaf_segment).max(0) as u32
    }

    pub(crate) fn set_num_edges(&self, n: u64) {
        self.edges.set_num_edges(n);
        let mut header = self.header();
        header.num_edges = n;
        self.header.set(header);
    }

    pub(crate) fn set_elem_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        self.edges.set_elem_capacity(n)?;
        let mut header = self.header();
        header.elem_capacity = n;
        self.header.set(header);
        Ok(())
    }

    pub(crate) fn set_count(&self, index: u64, count: SegmentEdgeCounts) {
        self.counts.set(index, &count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::test_support::{TestEdge, vector_memory};

    type Memories = (
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
        VectorMemory,
    );

    fn populated_memories() -> Memories {
        let store = EdgeStore::<TestEdge, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            1,
            0,
        )
        .unwrap();
        store.into_memories()
    }

    #[test]
    fn init_reopens_fully_populated_layout() {
        let (counts, edges, log, span_meta, free_spans, free_span_by_start) = populated_memories();
        let reopened = EdgeStore::<TestEdge, _>::init(
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            8,
            1,
            0,
        );
        assert!(reopened.is_ok());
    }

    #[test]
    fn init_rejects_partial_layout_when_one_region_is_wiped() {
        let (counts, _edges, log, span_meta, free_spans, free_span_by_start) = populated_memories();
        // The edge slab is empty while every other region is populated, e.g. a
        // miswired MemoryId. Recreating would overwrite the live regions.
        let result = EdgeStore::<TestEdge, _>::init(
            counts,
            vector_memory(),
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            8,
            1,
            0,
        );
        assert!(matches!(result, Err(InitError::PartialLayout)));
    }
}
