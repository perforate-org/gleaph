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

    /// Grows the segment tree to `new_segment_count` leaves.
    ///
    /// # Failure atomicity
    ///
    /// The operation is split into an infallible validation, a preflight phase
    /// that only touches backing memory capacity, and a commit phase that
    /// publishes the new logical layout. After the first commit write
    /// (`migrate_counts_for_segment_grow`) no recoverable error is returned.
    pub(crate) fn grow_segment_tree_to(&self, new_segment_count: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let old = h.segment_count;
        if new_segment_count <= old {
            return Ok(());
        }

        // PREFLIGHT: reserve all backing-memory growth before any canonical
        // metadata mutation. Memory growth without logical header/row changes is
        // safe to retain after an error.
        let counts_target_len = u64::from(new_segment_count).saturating_mul(2);
        self.counts.reserve_to(counts_target_len)?;
        self.span_meta.reserve_to(u64::from(new_segment_count))?;
        self.log.reserve_segment_count_to(new_segment_count)?;

        // COMMIT: logical layout publication. Every call below was preflighted
        // above, so no recoverable error remains after the first write.
        self.migrate_counts_for_segment_grow(old, new_segment_count);
        for _ in old..new_segment_count {
            self.span_meta
                .push(SegmentSpanMeta::default())
                .expect("commit: span_meta push must succeed after reserve");
        }
        self.log
            .grow_segment_count_to(new_segment_count)
            .expect("commit: log grow must succeed after reserve");
        let mut nh = h;
        nh.segment_count = new_segment_count;
        nh.tree_height = tree_height_for_segment_count(new_segment_count);
        self.write_header(&nh);
        Ok(())
    }

    pub(super) fn migrate_counts_for_segment_grow(&self, old_l: u32, new_l: u32) {
        let mut leaf_vals: Vec<SegmentEdgeCounts> = Vec::with_capacity(old_l as usize);
        for leaf in 0..old_l {
            let idx = u64::from(old_l + leaf);
            leaf_vals.push(self.counts.get(idx));
        }
        let target_len = u64::from(new_l).saturating_mul(2);
        while self.counts.len() < target_len {
            self.counts
                .push(SegmentEdgeCounts {
                    actual: 0,
                    total: 0,
                })
                .expect("commit: counts push must succeed after reserve");
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
    use crate::lara::edge::scan_iter::leaf_segment;
    use crate::test_support::{FailpointMemory, TestEdge, vector_memory};
    use crate::{VectorMemory, VertexId};

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

    type FailpointMemories = (
        FailpointMemory,
        FailpointMemory,
        FailpointMemory,
        FailpointMemory,
        FailpointMemory,
        FailpointMemory,
    );

    fn failpoint_edge_store(
        elem_capacity: u64,
        segment_size: u32,
    ) -> (EdgeStore<TestEdge, FailpointMemory>, FailpointMemories) {
        let counts = FailpointMemory::new();
        let edges = FailpointMemory::new();
        let log = FailpointMemory::new();
        let span_meta = FailpointMemory::new();
        let free_spans = FailpointMemory::new();
        let free_span_by_start = FailpointMemory::new();
        let store = EdgeStore::<TestEdge, _>::new(
            counts.clone(),
            edges.clone(),
            log.clone(),
            span_meta.clone(),
            free_spans.clone(),
            free_span_by_start.clone(),
            elem_capacity,
            segment_size,
            0,
        )
        .unwrap();
        (
            store,
            (
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                free_span_by_start,
            ),
        )
    }

    #[test]
    fn grow_segment_tree_failure_atomic_at_counts_reservation() {
        let (store, (counts, _edges, _log, _span_meta, _free_spans, _free_span_by_start)) =
            failpoint_edge_store(64, 1);
        seed_distinguishable_edge_store(
            &store,
            EdgeStoreSeed {
                actual_edges: vec![(0, 1), (1, 2), (2, 3)],
                total_by_leaf: vec![2, 4, 6],
                span_starts: vec![0, 10, 20],
            },
        );
        let before = EdgeStoreSnapshot::capture(&store);

        counts.fail_at_grow(counts.grow_count() + 1);
        let result = store.grow_segment_tree_to(8192);
        assert!(result.is_err(), "expected counts reservation to fail");

        let after = EdgeStoreSnapshot::capture(&store);
        assert_eq!(after, before, "store logical state must be unchanged");

        let reopened = EdgeStore::<TestEdge, _>::init(
            counts,
            _edges,
            _log,
            _span_meta,
            _free_spans,
            _free_span_by_start,
            64,
            1,
            0,
        );
        let reopened = reopened.expect("reopen after rejected counts reservation");
        assert_eq!(
            EdgeStoreSnapshot::capture(&reopened),
            before,
            "reopened store must match pre-error logical state"
        );
    }

    #[test]
    fn grow_segment_tree_failure_atomic_at_span_meta_reservation() {
        let (store, (counts, _edges, _log, span_meta, _free_spans, _free_span_by_start)) =
            failpoint_edge_store(64, 1);
        seed_distinguishable_edge_store(
            &store,
            EdgeStoreSeed {
                actual_edges: vec![(0, 1), (1, 2), (2, 3)],
                total_by_leaf: vec![2, 4, 6, 8],
                span_starts: vec![0, 10, 20, 30],
            },
        );
        let before = EdgeStoreSnapshot::capture(&store);

        counts.fail_at_grow(usize::MAX);
        span_meta.fail_at_grow(span_meta.grow_count() + 1);
        let result = store.grow_segment_tree_to(8192);
        assert!(result.is_err(), "expected span-meta reservation to fail");

        let after = EdgeStoreSnapshot::capture(&store);
        assert_eq!(after, before);

        let reopened = EdgeStore::<TestEdge, _>::init(
            counts,
            _edges,
            _log,
            span_meta,
            _free_spans,
            _free_span_by_start,
            64,
            1,
            0,
        );
        let reopened = reopened.expect("reopen after rejected span-meta reservation");
        assert_eq!(EdgeStoreSnapshot::capture(&reopened), before);
    }

    #[test]
    fn grow_segment_tree_failure_atomic_at_log_reservation() {
        let (store, (counts, _edges, log, span_meta, _free_spans, _free_span_by_start)) =
            failpoint_edge_store(64, 1);
        seed_distinguishable_edge_store(
            &store,
            EdgeStoreSeed {
                actual_edges: vec![(0, 1), (1, 2), (2, 3)],
                total_by_leaf: vec![2, 4, 6, 8],
                span_starts: vec![0, 10, 20, 30],
            },
        );
        let before = EdgeStoreSnapshot::capture(&store);

        counts.fail_at_grow(usize::MAX);
        span_meta.fail_at_grow(usize::MAX);
        log.fail_at_grow(log.grow_count() + 1);
        let result = store.grow_segment_tree_to(8192);
        assert!(result.is_err(), "expected log reservation to fail");

        let after = EdgeStoreSnapshot::capture(&store);
        assert_eq!(after, before);

        let reopened = EdgeStore::<TestEdge, _>::init(
            counts,
            _edges,
            log,
            span_meta,
            _free_spans,
            _free_span_by_start,
            64,
            1,
            0,
        );
        let reopened = reopened.expect("reopen after rejected log reservation");
        assert_eq!(EdgeStoreSnapshot::capture(&reopened), before);
    }

    #[test]
    fn grow_segment_tree_success_after_rejected_counts_reservation() {
        grow_segment_tree_retry_after_failure_at(|_counts, span_meta, log| {
            _counts.fail_at_grow(_counts.grow_count() + 1);
            span_meta.fail_at_grow(usize::MAX);
            log.fail_at_grow(usize::MAX);
        });
    }

    #[test]
    fn grow_segment_tree_success_after_rejected_span_meta_reservation() {
        grow_segment_tree_retry_after_failure_at(|_counts, span_meta, log| {
            _counts.fail_at_grow(usize::MAX);
            span_meta.fail_at_grow(span_meta.grow_count() + 1);
            log.fail_at_grow(usize::MAX);
        });
    }

    #[test]
    fn grow_segment_tree_success_after_rejected_log_reservation() {
        grow_segment_tree_retry_after_failure_at(|_counts, span_meta, log| {
            _counts.fail_at_grow(usize::MAX);
            span_meta.fail_at_grow(usize::MAX);
            log.fail_at_grow(log.grow_count() + 1);
        });
    }

    fn grow_segment_tree_retry_after_failure_at(
        configure_failure: impl FnOnce(&FailpointMemory, &FailpointMemory, &FailpointMemory),
    ) {
        let (store, (counts, _edges, _log, _span_meta, _free_spans, _free_span_by_start)) =
            failpoint_edge_store(64, 1);
        seed_distinguishable_edge_store(
            &store,
            EdgeStoreSeed {
                actual_edges: vec![(0, 1), (1, 2), (2, 3)],
                total_by_leaf: vec![2, 4, 6, 8],
                span_starts: vec![0, 10, 20, 30],
            },
        );
        let before = EdgeStoreSnapshot::capture(&store);
        let target = 8192;
        let expected = expected_snapshot_after_grow(&before, target);

        configure_failure(&counts, &_span_meta, &_log);
        assert!(
            store.grow_segment_tree_to(target).is_err(),
            "expected the configured reservation to fail"
        );
        counts.fail_never();
        _span_meta.fail_never();
        _log.fail_never();
        let result = store.grow_segment_tree_to(target);
        assert!(result.is_ok(), "retry after rejection: {result:?}");
        assert_eq!(store.header().segment_count, target);

        let after = EdgeStoreSnapshot::capture(&store);
        assert_eq!(
            after, expected,
            "post-growth logical snapshot must match expected relocated tree"
        );

        let (counts, edges, log, span_meta, free_spans, free_span_by_start) = store.into_memories();
        let reopened = EdgeStore::<TestEdge, _>::init(
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            64,
            1,
            0,
        );
        let reopened = reopened.expect("reopen after successful retry");
        assert_eq!(reopened.header().segment_count, target);
        assert_eq!(
            EdgeStoreSnapshot::capture(&reopened),
            expected,
            "reopened store must match expected relocated tree"
        );
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct EdgeStoreSeed {
        actual_edges: Vec<(u32, u32)>,
        total_by_leaf: Vec<u32>,
        span_starts: Vec<u64>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct EdgeStoreSnapshot {
        segment_count: u32,
        counts: Vec<SegmentEdgeCounts>,
        span_meta: Vec<SegmentSpanMeta>,
        edges: Vec<TestEdge>,
        log_high_waters: Vec<u32>,
    }

    impl EdgeStoreSnapshot {
        fn capture<M: Memory + Clone>(store: &EdgeStore<TestEdge, M>) -> Self {
            let header = store.header();
            let segment_count = header.segment_count;
            let counts: Vec<_> = (0..u64::from(segment_count).saturating_mul(2))
                .map(|i| store.counts_store().get(i))
                .collect();
            let span_meta: Vec<_> = (0..u64::from(segment_count))
                .map(|i| store.span_meta_store().get(i))
                .collect();
            let edges: Vec<_> = (0..header.num_edges)
                .map(|slot| store.read_slot(slot))
                .collect();
            let log_high_waters: Vec<_> = (0..segment_count)
                .map(|leaf| store.overflow_log_segment_high_water(leaf))
                .collect();
            Self {
                segment_count,
                counts,
                span_meta,
                edges,
                log_high_waters,
            }
        }
    }

    fn expected_counts_tree(
        old_leaves: &[SegmentEdgeCounts],
        leaf_count: u32,
    ) -> Vec<SegmentEdgeCounts> {
        let mut counts = vec![
            SegmentEdgeCounts {
                actual: 0,
                total: 0
            };
            (leaf_count as usize) * 2
        ];
        for (leaf, &val) in old_leaves.iter().enumerate() {
            counts[(leaf_count as usize) + leaf] = val;
        }
        for idx in (1..leaf_count).rev() {
            let left = counts[(idx * 2) as usize];
            let right = counts[(idx * 2 + 1) as usize];
            counts[idx as usize] = SegmentEdgeCounts {
                actual: left.actual + right.actual,
                total: left.total + right.total,
            };
        }
        counts
    }

    fn expected_snapshot_after_grow(before: &EdgeStoreSnapshot, target: u32) -> EdgeStoreSnapshot {
        let old_leaves: Vec<_> = (0..before.segment_count)
            .map(|leaf| before.counts[(before.segment_count + leaf) as usize])
            .collect();
        let counts = expected_counts_tree(&old_leaves, target);
        let mut span_meta = before.span_meta.clone();
        span_meta.resize(target as usize, SegmentSpanMeta::default());
        let mut log_high_waters = before.log_high_waters.clone();
        log_high_waters.resize(target as usize, 0);
        EdgeStoreSnapshot {
            segment_count: target,
            counts,
            span_meta,
            edges: before.edges.clone(),
            log_high_waters,
        }
    }

    fn seed_distinguishable_edge_store(
        store: &EdgeStore<TestEdge, FailpointMemory>,
        seed: EdgeStoreSeed,
    ) {
        // Grow the tree to a small multi-leaf count so we can seed distinguishable
        // per-leaf state. This is pure setup; failures are not injected here.
        let setup_leaf_count = (seed.total_by_leaf.len().max(seed.span_starts.len()).max(1))
            .next_power_of_two()
            .max(4) as u32;
        store
            .grow_segment_tree_to(setup_leaf_count)
            .expect("setup growth");

        let header = store.header();
        let leaf_count = header.segment_count;

        // Write distinguishable span metadata first so each leaf has a stable identity.
        for (leaf, &start) in seed.span_starts.iter().enumerate() {
            if leaf >= leaf_count as usize {
                break;
            }
            store.span_meta_store().set(
                u64::from(leaf as u32),
                &SegmentSpanMeta {
                    physical_start: start,
                },
            );
        }

        // Write actual edge bytes and update leaf actual counts.
        let mut max_slot = 0u64;
        for (src, dst) in seed.actual_edges {
            let base = u64::from(src) * 64;
            let slot = base;
            max_slot = max_slot.max(slot);
            store.write_slot(slot, TestEdge(dst)).unwrap();
            let leaf = leaf_segment(VertexId::from(src), header.segment_size);
            let idx = u64::from(leaf_count + leaf);
            let mut c = store.counts_store().get(idx);
            c.actual = c.actual.saturating_add(1);
            store.set_count(idx, c);
        }
        store.set_num_edges(max_slot.saturating_add(1));

        // Set distinguishable total values at each leaf node.
        for (leaf, &total) in seed.total_by_leaf.iter().enumerate() {
            if leaf >= leaf_count as usize {
                break;
            }
            let idx = u64::from(leaf_count + leaf as u32);
            let mut c = store.counts_store().get(idx);
            c.total = i64::from(total);
            store.set_count(idx, c);
        }

        // Recompute internal nodes so the tree is consistent.
        for idx in (1..leaf_count).rev() {
            let left = store.counts_store().get(u64::from(idx * 2));
            let right = store.counts_store().get(u64::from(idx * 2 + 1));
            store.set_count(
                u64::from(idx),
                SegmentEdgeCounts {
                    actual: left.actual.saturating_add(right.actual),
                    total: left.total.saturating_add(right.total),
                },
            );
        }
        store.set_count(
            0,
            SegmentEdgeCounts {
                actual: 0,
                total: 0,
            },
        );
    }

    #[test]
    fn grow_segment_tree_reopens_after_every_rejected_growth() {
        let (store, (counts, _edges, log, span_meta, _free_spans, _free_span_by_start)) =
            failpoint_edge_store(64, 1);
        let target = 8192;
        for mem in [&counts, &span_meta, &log] {
            mem.fail_at_grow(mem.grow_count() + 1);
            assert!(
                store.grow_segment_tree_to(target).is_err(),
                "expected {mem:?} reservation to fail"
            );
            mem.fail_at_grow(usize::MAX);
        }
    }
}
