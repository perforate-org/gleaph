//! Deferred-maintenance wrapper for the labeled LARA graph.

use crate::{
    VertexId,
    labeled::graph::{
        EdgePlacementPolicy, InitError, LabeledLaraGraph, LabeledOperationError,
        values::MaterializeStep,
    },
    lara::maintenance::{MaintenanceBudget, MaintenanceWorkReport},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, cell::RefCell, fmt};

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

/// One versioned deferred maintenance item for a labeled graph.
///
/// Each variant is a `{Tag}V{Version}` entry: the stable byte layout is
/// `[magic 0x5A][tag][version][payload]` with the tag/version bytes derived from the
/// enum variant (see [`Self::tag`] and [`Self::version`]), so the `(tag, version)`
/// dispatch table is enforced by the compiler. A future change to one variant adds a
/// `{Tag}V2` variant with a decode arm beside the V1 arm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MaintenanceWorkItem {
    /// v1: Compact the LabelBucketStore VertexSegment containing one vertex.
    CompactLabelBucketVertexSegmentV1 {
        /// Vertex whose LabelBucketStore VertexSegment should be compacted.
        vid: VertexId,
    },
    /// v1: Compact the VertexEdgeSpan containing one LabelEdgeSpan (one edge step per queue pop).
    CompactVertexEdgeSpanV1 {
        /// Vertex owning the VertexEdgeSpan.
        vid: VertexId,
        /// Label-bucket index used to validate that the work item is still relevant.
        anchor_bucket_index: u32,
        /// Next label-bucket index to compact.
        resume_bucket_index: u32,
        /// Next in-bucket slot to scan (compaction cursor; GAP-2026-07-14-001).
        resume_slot_index: u32,
    },
    /// v1: Compact the label-bucket vertex segment, then the owning vertex edge span
    /// (dense-leaf deferred enqueue).
    CompactDenseLabeledVertexMaintenanceV1 {
        /// Vertex to compact in both stores.
        vid: VertexId,
    },
    /// v1: Reserved stable tag for independently scheduled value-span maintenance.
    CompactVertexValueSpanV1 {
        /// Vertex owning the value span.
        vid: VertexId,
    },
    /// v1: Legacy stable tag. It now advances edge compaction only; value maintenance is independent.
    CompactVertexEdgeAndValueSpanV1 {
        /// Vertex owning both spans.
        vid: VertexId,
        /// Label-bucket index used to validate relevance.
        anchor_bucket_index: u32,
        /// Next label-bucket index to compact.
        resume_bucket_index: u32,
    },
    /// v1: Compact the inline property bytes slab when aggregate free space is fragmented.
    CompactInlinePropertyBytesSlabV1,
    /// v1: Materialize an inline property stream for one bucket
    /// (Plan 0320 §Step 2). The work item carries the bucket slot
    /// (validates relevance), the target width, the fill byte, the
    /// reserved slab offset (set on first step), the stream length
    /// (S × width), and the resume cursor (Plan 0320 §0.7). The
    /// worker pops, writes one step's worth of `fill_byte` to the
    /// reserved slab region, advances `resume_position`, and
    /// re-enqueues until finished; on the final step the descriptor
    /// (width + offset + slab_slots + log reset) is published and
    /// the vertex-level accounting is updated atomically (Plan 0320
    /// §0.1, §0.6, §0.12).
    MaterializeInlinePropertyStreamV1 {
        /// Vertex whose bucket receives the materialized stream.
        vid: VertexId,
        /// Bucket slot used to validate that the work item is still relevant.
        bucket_slot: u64,
        /// Target width (always > 0; u16 representation bound).
        width: u16,
        /// Fill byte for every position in the stream.
        fill_byte: u8,
        /// Reserved slab byte offset (set on first step, `u64::MAX` until then).
        reserved_offset: u64,
        /// Total stream byte count (S × width).
        total_bytes: u64,
        /// Resume cursor: next byte position to write (0..total_bytes).
        resume_position: u64,
    },
}

impl MaintenanceWorkItem {
    /// Stable variant tag byte (0..=5).
    fn tag(&self) -> u8 {
        match self {
            Self::CompactLabelBucketVertexSegmentV1 { .. } => 0,
            Self::CompactVertexEdgeSpanV1 { .. } => 1,
            Self::CompactDenseLabeledVertexMaintenanceV1 { .. } => 2,
            Self::CompactVertexValueSpanV1 { .. } => 3,
            Self::CompactVertexEdgeAndValueSpanV1 { .. } => 4,
            Self::CompactInlinePropertyBytesSlabV1 => 5,
            Self::MaterializeInlinePropertyStreamV1 { .. } => 6,
        }
    }

    /// Stable variant version byte. All variants start at v1; a future version of one
    /// variant is a new enum variant with its own (tag, version) decode arm.
    fn version(&self) -> u8 {
        match self {
            Self::CompactLabelBucketVertexSegmentV1 { .. }
            | Self::CompactVertexEdgeSpanV1 { .. }
            | Self::CompactDenseLabeledVertexMaintenanceV1 { .. }
            | Self::CompactVertexValueSpanV1 { .. }
            | Self::CompactVertexEdgeAndValueSpanV1 { .. }
            | Self::CompactInlinePropertyBytesSlabV1 => 1,
            Self::MaterializeInlinePropertyStreamV1 { .. } => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        labeled::{
            BucketLabelKey,
            graph::{LabeledLaraGraph, test_support::InlinePropertyTestEdge},
        },
        test_support::{labeled_lara_memories, vector_memory},
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestEdge(u32);

    impl CsrEdge for TestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
        }

        fn write_to(&self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.0)
        }

        fn with_neighbor_vid(&self, vid: VertexId) -> Self {
            Self(u32::from(vid))
        }
    }

    impl CsrEdgeTombstone for TestEdge {
        fn tombstone_edge() -> Self {
            Self(u32::from(crate::VertexId::EDGE_TOMBSTONE_SENTINEL))
        }
    }

    fn graph_with_segment_size(
        segment_size: u32,
    ) -> DeferredLabeledLaraGraph<TestEdge, crate::VectorMemory> {
        let (v, b, bfs, bfsbs, ec, e, el, esm, efs, efsbs, vs, vffs, vffsbs, vlog, vblobs, ltb) =
            labeled_lara_memories();
        let inner = LabeledLaraGraph::new_with_segment_size(
            v,
            b,
            bfs,
            bfsbs,
            ec,
            e,
            el,
            esm,
            efs,
            efsbs,
            vs,
            vffs,
            vffsbs,
            vlog,
            vblobs,
            ltb,
            crate::labeled::InitialCapacities::uniform(1024),
            BucketLabelKey::from_raw(1),
            segment_size,
        )
        .expect("inner graph");
        DeferredLabeledLaraGraph::new(inner, vector_memory()).expect("deferred graph")
    }

    fn graph() -> DeferredLabeledLaraGraph<TestEdge, crate::VectorMemory> {
        graph_with_segment_size(32)
    }

    fn inline_property_graph()
    -> DeferredLabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
        let (
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            inline_property_bytes_slab,
            value_free_spans,
            value_free_span_by_start,
            inline_property_bytes_log,
            value_blobs,
            ltb,
        ) = labeled_lara_memories();
        let inner = LabeledLaraGraph::new_with_segment_size(
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            inline_property_bytes_slab,
            value_free_spans,
            value_free_span_by_start,
            inline_property_bytes_log,
            value_blobs,
            ltb,
            crate::labeled::InitialCapacities::uniform(1024),
            BucketLabelKey::from_raw(1),
            32,
        )
        .unwrap();
        DeferredLabeledLaraGraph::new(inner, vector_memory()).unwrap()
    }

    fn drain_vertex_edge_span_compact_queue(
        graph: &DeferredLabeledLaraGraph<TestEdge, crate::VectorMemory>,
    ) {
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(budget);
        }
    }

    /// Plan 0320 §Step 2: enqueue a `MaterializeInlinePropertyStreamV1`
    /// work item directly, drain the queue, and verify the bucket
    /// descriptor is published (width > 0, slab_slots == degree,
    /// log reset). Mirrors the `CompactVertexEdgeSpanV1` drain test
    /// pattern.
    #[test]
    fn materialize_inline_property_stream_drains_via_maintenance_queue() {
        use crate::labeled::bucket_label_key::BucketLabelKey;
        let graph = inline_property_graph();
        let src = graph
            .inner
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .expect("push_vertex");
        // Insert 3 edges with w=0 (no inline property bytes yet).
        for i in 0..3 {
            graph
                .inner
                .insert_edge_skip_leaf_cascade(
                    src,
                    BucketLabelKey::directed_from_index(2),
                    InlinePropertyTestEdge::with_bytes(100 + i, &[]),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.inner.vertices().get(src);
        let search = graph
            .inner
            .find_bucket(src, &vertex, BucketLabelKey::directed_from_index(2))
            .expect("find");
        let bucket_slot = match search {
            crate::labeled::graph::BucketSearch::Found { slot, .. } => slot,
            _ => panic!("bucket missing"),
        };
        // Enqueue the materialize work item directly (Step 2 path).
        graph
            .queue
            .enqueue(&MaintenanceWorkItem::MaterializeInlinePropertyStreamV1 {
                vid: src,
                bucket_slot,
                width: 4,
                fill_byte: 0xAB,
                reserved_offset: u64::MAX,
                total_bytes: 12, // 3 slots * 4 bytes
                resume_position: 0,
            });
        // Drain.
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(budget);
        }
        // Verify the descriptor was published.
        let bucket = graph
            .inner
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .expect("read bucket");
        assert_eq!(bucket.inline_property_byte_width(), 4);
        assert_eq!(bucket.inline_property_bytes_slab_slots(), 3);
        assert_eq!(bucket.inline_property_bytes_log_head(), -1);
        // Vertex accounting: 3 * 4 = 12 bytes.
        let v = graph.inner.vertices().get(src);
        assert_eq!(v.inline_property_bytes_allocated_bytes(), 12);
    }

    #[test]
    fn leaf_mate_insert_does_not_corrupt_oversized_hub_span() {
        // Regression: a vertex whose weighted edge span exceeds the fixed per-vertex
        // configured leaf quota used to be overwritten when a leaf-mate's
        // first edge was pinned at the (now occupied) fixed quota offset.
        let graph = graph();
        // Leaf-mates: hub (vertex 0) and vertices 1..=8.
        for _ in 0..9 {
            graph
                .inner()
                .push_vertex(crate::labeled::record::LabeledVertex::default())
                .unwrap();
        }
        let label = BucketLabelKey::from_raw(2);
        let hub = VertexId::from(0);
        let hub_edges = || -> Vec<u32> {
            graph
                .inner()
                .out_edges(hub)
                .unwrap()
                .iter()
                .map(|e| e.0)
                .collect()
        };
        let hub_missing =
            |edges: &[u32]| -> Vec<u32> { (1..=300u32).filter(|t| !edges.contains(t)).collect() };

        for target in 1..=300u32 {
            graph
                .insert_edge(
                    hub,
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        drain_vertex_edge_span_compact_queue(&graph);
        assert!(
            hub_missing(&hub_edges()).is_empty(),
            "hub missing edges before any leaf-mate insert"
        );

        // Each leaf-mate's first edge lands at the configured quota offset, which now overlaps
        // the hub's oversized span; placement must instead find free, non-overlapping room.
        for mate in 1..=8u32 {
            let value = 10_000 + mate;
            graph
                .insert_edge(
                    VertexId::from(mate),
                    label,
                    TestEdge(value),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            drain_vertex_edge_span_compact_queue(&graph);

            let edges = hub_edges();
            assert!(
                hub_missing(&edges).is_empty(),
                "leaf-mate {mate} insert dropped hub edges: {:?}",
                hub_missing(&edges)
            );
            for leaked in 10_001..=10_008u32 {
                assert!(
                    !edges.contains(&leaked),
                    "leaf-mate value {leaked} leaked into hub row after inserting mate {mate}"
                );
            }
            let mate_edges: Vec<u32> = graph
                .inner()
                .out_edges(VertexId::from(mate))
                .unwrap()
                .iter()
                .map(|e| e.0)
                .collect();
            assert!(
                mate_edges.contains(&value),
                "leaf-mate {mate} lost its own edge {value}"
            );
        }
    }

    #[test]
    fn dense_maintenance_enqueue_deduplicates_per_leaf() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        let mut target = 0;
        while graph.maintenance_queue_len() == 0 && target < 1024 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            target += 1;
        }
        let before = graph.maintenance_queue_len();
        assert!(
            before > 0,
            "fixture must cross the dense-maintenance threshold"
        );
        for target in target..target + 128 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(graph.maintenance_queue_len(), before);
    }

    #[test]
    fn segment16_tail_headroom_scales_with_segment_size() {
        use crate::labeled::graph::leaf_pin::labeled_leaf_physical_block_len;

        let graph = graph_with_segment_size(16);
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        const EDGE_COUNT: u32 = 1024;
        for target in 0..EDGE_COUNT {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.inner().vertices().get(VertexId::from(0));
        let bucket = graph
            .inner()
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.degree(), EDGE_COUNT);
        assert!(
            vertex.stored_slots >= 16,
            "vertex stored={} bucket stored={}",
            vertex.stored_slots,
            bucket.stored_slots
        );
        assert!(
            vertex.stored_slots >= bucket.stored_slots,
            "vertex stored={} bucket stored={}",
            vertex.stored_slots,
            bucket.stored_slots
        );
        let block_len = labeled_leaf_physical_block_len(16);
        let required = u64::from(EDGE_COUNT)
            .saturating_add(1)
            .saturating_add(u64::from(EDGE_COUNT).div_ceil(8).max(16));
        let growth_bound = required.div_ceil(block_len).saturating_mul(block_len);
        assert!(
            u64::from(vertex.stored_slots) <= growth_bound,
            "segment16 growth exceeded resident + active + 12.5% headroom: stored={} bound={growth_bound}",
            vertex.stored_slots
        );
    }

    #[test]
    fn segment16_quota1_first_bucket_stays_on_slab() {
        let graph = graph_with_segment_size(16);
        let label = BucketLabelKey::from_raw(2);
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                TestEdge(10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let vertex = graph.inner().vertices().get(VertexId::from(0));
        let bucket = graph
            .inner()
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(vertex.stored_slots, 1);
        assert_eq!(bucket.stored_slots, 1);
        assert_eq!(bucket.degree(), 1);
        assert_eq!(bucket.overflow_log_head(), -1);
    }

    #[test]
    fn segment16_dense_maintenance_enqueue_deduplicates_per_leaf() {
        let graph = graph_with_segment_size(16);
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        let mut target = 0;
        while graph.maintenance_queue_len() == 0 && target < 1024 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            target += 1;
        }
        let before = graph.maintenance_queue_len();
        assert!(
            before > 0,
            "fixture must cross the dense-maintenance threshold"
        );
        for target in target..target + 128 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(graph.maintenance_queue_len(), before);
    }

    #[test]
    fn segment16_dense_maintenance_deduplicates_leaf_mates() {
        let graph = graph_with_segment_size(16);
        for _ in 0..2 {
            graph
                .inner()
                .push_vertex(crate::labeled::record::LabeledVertex::default())
                .unwrap();
        }
        let label = BucketLabelKey::from_raw(2);
        for target in 0..1024u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(graph.maintenance_queue_len(), 1);

        graph
            .insert_edge(
                VertexId::from(1),
                label,
                TestEdge(10_000),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert_eq!(graph.maintenance_queue_len(), 1);

        let report = graph.maintenance(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        });
        assert_eq!(report.processed_work_items, 1);
        assert_eq!(graph.maintenance_queue_len(), 0);
        assert_eq!(
            graph.inner().out_edges(VertexId::from(0)).unwrap().len(),
            1024
        );
        assert_eq!(graph.inner().out_edges(VertexId::from(1)).unwrap().len(), 1);
    }

    #[test]
    fn segment16_hub_churn_preserves_edges_across_leaf_boundary() {
        let graph = graph_with_segment_size(16);
        for _ in 0..17 {
            graph
                .inner()
                .push_vertex(crate::labeled::record::LabeledVertex::default())
                .unwrap();
        }
        let label = BucketLabelKey::from_raw(2);
        let hub = VertexId::from(0);
        for target in 0..256u32 {
            graph
                .insert_edge(
                    hub,
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .insert_edge(
                VertexId::from(16),
                label,
                TestEdge(16_000),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        drain_vertex_edge_span_compact_queue(&graph);

        {
            let sparse = graph.inner().vertices().get(VertexId::from(16));
            assert!(
                sparse.stored_slots <= 1 + 16,
                "one-edge mate received excessive initial span: {}",
                sparse.stored_slots
            );
        }

        for target in (0..256u32).step_by(2) {
            assert!(
                graph
                    .inner()
                    .remove_edge_matching(hub, label, |edge| edge.0 == target)
                    .unwrap()
                    .is_some()
            );
        }
        for target in 1_000..1_128u32 {
            graph
                .insert_edge(
                    hub,
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        drain_vertex_edge_span_compact_queue(&graph);

        {
            let sparse = graph.inner().vertices().get(VertexId::from(16));
            assert!(
                sparse.stored_slots <= 1 + 16,
                "one-edge mate grew with hub capacity: {}",
                sparse.stored_slots
            );
        }

        let edges = graph
            .inner()
            .out_edges(hub)
            .unwrap()
            .iter()
            .map(|edge| edge.0)
            .collect::<Vec<_>>();
        assert_eq!(edges.len(), 256);
        for target in (1..256u32).step_by(2) {
            assert!(edges.contains(&target));
        }
        for target in 1_000..1_128u32 {
            assert!(edges.contains(&target));
        }
        assert_eq!(
            graph
                .inner()
                .out_edges(VertexId::from(16))
                .unwrap()
                .iter()
                .map(|edge| edge.0)
                .collect::<Vec<_>>(),
            vec![16_000]
        );
    }

    #[test]
    fn maintenance_vertex_edge_span_compact_one_work_item_per_step() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge(999),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..30u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for target in 0..25u32 {
            graph
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == target)
                .unwrap();
        }
        assert_eq!(
            graph
                .inner()
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap(),
            vec![
                TestEdge(29),
                TestEdge(28),
                TestEdge(27),
                TestEdge(26),
                TestEdge(25),
            ]
        );

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let budget_one = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        };

        let mut steps = 0u32;
        while graph.maintenance_queue_len() > 0 {
            let report = graph.maintenance(budget_one);
            assert_eq!(
                report.processed_work_items, 1,
                "each maintenance call should advance exactly one compaction step"
            );
            steps = steps.saturating_add(1);
            assert!(
                steps < 512,
                "incremental compaction should finish within a bounded number of steps"
            );
        }
        assert!(
            steps > 1,
            "tombstone-heavy bucket should need multiple steps"
        );

        let inner = graph.inner();
        assert_eq!(
            inner
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap(),
            vec![
                TestEdge(29),
                TestEdge(28),
                TestEdge(27),
                TestEdge(26),
                TestEdge(25)
            ]
        );
    }

    #[test]
    fn maintenance_vertex_edge_span_compact_clears_many_slab_tombstones() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        for target in 1..=120u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for target in 1..=115u32 {
            graph
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == target)
                .unwrap();
        }

        let inner = graph.inner();
        assert_eq!(
            inner
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap()
                .len(),
            5
        );

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        drain_vertex_edge_span_compact_queue(&graph);

        assert_eq!(
            inner
                .iter_edges_for_label(VertexId::from(0), label)
                .unwrap(),
            vec![
                TestEdge(120),
                TestEdge(119),
                TestEdge(118),
                TestEdge(117),
                TestEdge(116)
            ]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            inner.vertices(),
            inner.buckets(),
            inner.edges(),
        );
    }

    #[test]
    fn maintenance_compacts_vertex_edge_span() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge(999),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..80u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .inner()
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        for target in 0..72u32 {
            graph
                .remove_edge_matching(VertexId::from(0), label, |edge| edge.0 == target)
                .unwrap();
        }

        let before = graph.inner().vertices().get(VertexId::from(0));
        assert!(before.stored_slots > 8);

        graph
            .mark_compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        drain_vertex_edge_span_compact_queue(&graph);

        let after = graph.inner().vertices().get(VertexId::from(0));
        assert_eq!(after.stored_slots, 9);
    }

    #[test]
    fn deferred_insert_enqueues_maintenance_when_leaf_dense() {
        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge(999),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        for target in 0..1024u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert!(
            graph.maintenance_queue_len() > 0,
            "expected deferred wrapper to enqueue compaction when PMA leaf is dense"
        );
    }

    #[test]
    fn inline_property_bytes_compaction_maintenance_is_deduplicated_and_consumed() {
        let encoded = MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1.into_bytes();
        assert_eq!(
            MaintenanceWorkItem::from_bytes(Cow::Owned(encoded)),
            MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1
        );

        let graph = graph();
        graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();

        graph.mark_compact_inline_property_bytes_slab().unwrap();
        graph.mark_compact_inline_property_bytes_slab().unwrap();
        assert_eq!(graph.maintenance_queue_len(), 1);

        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        };
        let report = graph.maintenance(budget);
        assert_eq!(report.processed_work_items, 1);
        assert_eq!(graph.maintenance_queue_len(), 0);
    }

    #[test]
    fn inline_property_bytes_compaction_queue_survives_reopen() {
        let (
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            inline_property_bytes_slab,
            value_free_spans,
            value_free_span_by_start,
            inline_property_bytes_log,
            value_blobs,
            ltb,
        ) = labeled_lara_memories();
        let inner = LabeledLaraGraph::<TestEdge, _>::new(
            vertices.clone(),
            buckets.clone(),
            bucket_free_spans.clone(),
            bucket_free_span_by_start.clone(),
            edge_counts.clone(),
            edges.clone(),
            edge_log.clone(),
            edge_span_meta.clone(),
            edge_free_spans.clone(),
            edge_free_span_by_start.clone(),
            inline_property_bytes_slab.clone(),
            value_free_spans.clone(),
            value_free_span_by_start.clone(),
            inline_property_bytes_log.clone(),
            value_blobs.clone(),
            ltb.clone(),
            crate::labeled::InitialCapacities::uniform(1024),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let queue_memory = vector_memory();
        let graph = DeferredLabeledLaraGraph::new(inner, queue_memory.clone()).unwrap();
        graph.mark_compact_inline_property_bytes_slab().unwrap();
        assert_eq!(graph.maintenance_queue_len(), 1);
        drop(graph);

        let reopened = DeferredLabeledLaraGraph::<TestEdge, _>::init(
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            inline_property_bytes_slab,
            value_free_spans,
            value_free_span_by_start,
            inline_property_bytes_log,
            value_blobs,
            ltb,
            queue_memory,
            crate::labeled::InitialCapacities::uniform(1024),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        assert_eq!(reopened.maintenance_queue_len(), 1);
        reopened.maintenance(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        });
        assert_eq!(reopened.maintenance_queue_len(), 0);
    }

    #[test]
    fn failed_inline_property_bytes_compaction_is_requeued_for_retry() {
        let graph = graph();
        crate::labeled::graph::force_next_inline_property_bytes_compaction_error();
        graph.mark_compact_inline_property_bytes_slab().unwrap();

        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        };
        graph.maintenance(budget);
        assert_eq!(graph.maintenance_queue_len(), 1);

        graph.maintenance(budget);
        assert_eq!(graph.maintenance_queue_len(), 0);
    }

    #[test]
    fn deferred_inline_property_bytes_compaction_preserves_values_and_reclaims_holes() {
        let graph = inline_property_graph();
        let src = graph
            .inner()
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        for (label, target, width) in [(2, 0, 2), (3, 1, 2), (4, 2, 4)] {
            let label = BucketLabelKey::from_raw(label);
            let bytes = vec![target as u8; usize::from(width)];
            graph
                .inner()
                .ensure_label_bucket_inline_property_byte_width(src, label, width)
                .unwrap();
            graph
                .insert_edge(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &bytes),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for (label, target) in [(2, 0), (4, 2)] {
            let label = BucketLabelKey::from_raw(label);
            graph
                .remove_edge_matching(src, label, |edge| edge.target == target)
                .unwrap()
                .expect("inline property edge removed");
        }
        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: Some(1),
                max_segments: None,
                max_delete_edge_steps: None,
            });
        }

        let target_label = BucketLabelKey::from_raw(5);
        let before_target = graph.inner().inline_property_bytes_storage_stats().unwrap();
        assert_eq!(
            before_target.free_bytes, 6,
            "before target: {before_target:?}"
        );
        assert!(
            graph
                .inner()
                .inline_property_bytes_compaction_needed(6)
                .unwrap()
        );
        graph
            .inner()
            .ensure_label_bucket_inline_property_byte_width(src, target_label, 6)
            .unwrap();
        graph
            .insert_edge(
                src,
                target_label,
                InlinePropertyTestEdge::with_bytes(3, &[3u8; 6]),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert!(graph.maintenance_queue_len() > 0);
        assert_eq!(
            graph
                .inner()
                .inline_property_bytes_storage_stats()
                .unwrap()
                .free_bytes,
            6
        );

        while graph.maintenance_queue_len() > 0 {
            graph.maintenance(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: Some(1),
                max_segments: None,
                max_delete_edge_steps: None,
            });
        }
        let after = graph.inner().inline_property_bytes_storage_stats().unwrap();
        assert_eq!(
            after.free_bytes, 6,
            "unexpected inline property stats: {after:?}"
        );
        assert_eq!(after.largest_free_span, after.free_bytes);
        assert!(
            !graph
                .inner()
                .inline_property_bytes_compaction_needed(6)
                .unwrap()
        );
        assert_eq!(
            graph
                .inner()
                .iter_edges_for_label(src, BucketLabelKey::from_raw(3))
                .unwrap()[0]
                .target,
            1
        );
        assert_eq!(
            graph
                .inner()
                .iter_edges_for_label(src, target_label)
                .unwrap()[0]
                .target,
            3
        );
    }
}

/// Magic byte prefix of the versioned maintenance work-item encoding. The pre-slice-6
/// fixed-16-byte layout had the variant tag at byte 0 (values 0..=5), so any legacy item
/// fails the magic check by construction and is rejected instead of mis-decoded.
const MAINTENANCE_WORK_ITEM_MAGIC: u8 = 0x5A;

fn maintenance_work_item_bytes(item: &MaintenanceWorkItem) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.push(MAINTENANCE_WORK_ITEM_MAGIC);
    out.push(item.tag());
    out.push(item.version());
    match item {
        MaintenanceWorkItem::CompactLabelBucketVertexSegmentV1 { vid } => {
            out.extend_from_slice(&u32::from(*vid).to_le_bytes());
        }
        MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
            vid,
            anchor_bucket_index,
            resume_bucket_index,
            resume_slot_index,
        } => {
            out.extend_from_slice(&u32::from(*vid).to_le_bytes());
            out.extend_from_slice(&anchor_bucket_index.to_le_bytes());
            out.extend_from_slice(&resume_bucket_index.to_le_bytes());
            out.extend_from_slice(&resume_slot_index.to_le_bytes());
        }
        MaintenanceWorkItem::CompactDenseLabeledVertexMaintenanceV1 { vid } => {
            out.extend_from_slice(&u32::from(*vid).to_le_bytes());
        }
        MaintenanceWorkItem::CompactVertexValueSpanV1 { vid } => {
            out.extend_from_slice(&u32::from(*vid).to_le_bytes());
        }
        MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 {
            vid,
            anchor_bucket_index,
            resume_bucket_index,
        } => {
            out.extend_from_slice(&u32::from(*vid).to_le_bytes());
            out.extend_from_slice(&anchor_bucket_index.to_le_bytes());
            out.extend_from_slice(&resume_bucket_index.to_le_bytes());
        }
        MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1 => {}
        MaintenanceWorkItem::MaterializeInlinePropertyStreamV1 {
            vid,
            bucket_slot,
            width,
            fill_byte,
            reserved_offset,
            total_bytes,
            resume_position,
        } => {
            out.extend_from_slice(&u32::from(*vid).to_le_bytes());
            out.extend_from_slice(&bucket_slot.to_le_bytes());
            out.extend_from_slice(&width.to_le_bytes());
            out.push(*fill_byte);
            out.extend_from_slice(&reserved_offset.to_le_bytes());
            out.extend_from_slice(&total_bytes.to_le_bytes());
            out.extend_from_slice(&resume_position.to_le_bytes());
        }
    }
    out
}

impl Storable for MaintenanceWorkItem {
    const BOUND: Bound = Bound::Bounded {
        max_size: 3 + 39,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(maintenance_work_item_bytes(self))
    }

    fn into_bytes(self) -> Vec<u8> {
        maintenance_work_item_bytes(&self)
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let b = bytes.as_ref();
        let Some(&MAINTENANCE_WORK_ITEM_MAGIC) = b.first() else {
            panic!(
                "maintenance work item: rejected legacy or corrupt bytes (expected magic 0x5A, got {:?})",
                b.first()
            );
        };
        let tag = b[1];
        let version = b[2];
        let payload = &b[3..];
        match (tag, version) {
            (0, 1) => Self::CompactLabelBucketVertexSegmentV1 {
                vid: VertexId::from(u32::from_le_bytes(payload[0..4].try_into().unwrap())),
            },
            (1, 1) => Self::CompactVertexEdgeSpanV1 {
                vid: VertexId::from(u32::from_le_bytes(payload[0..4].try_into().unwrap())),
                anchor_bucket_index: u32::from_le_bytes(payload[4..8].try_into().unwrap()),
                resume_bucket_index: u32::from_le_bytes(payload[8..12].try_into().unwrap()),
                resume_slot_index: u32::from_le_bytes(payload[12..16].try_into().unwrap()),
            },
            (2, 1) => Self::CompactDenseLabeledVertexMaintenanceV1 {
                vid: VertexId::from(u32::from_le_bytes(payload[0..4].try_into().unwrap())),
            },
            (3, 1) => Self::CompactVertexValueSpanV1 {
                vid: VertexId::from(u32::from_le_bytes(payload[0..4].try_into().unwrap())),
            },
            (4, 1) => Self::CompactVertexEdgeAndValueSpanV1 {
                vid: VertexId::from(u32::from_le_bytes(payload[0..4].try_into().unwrap())),
                anchor_bucket_index: u32::from_le_bytes(payload[4..8].try_into().unwrap()),
                resume_bucket_index: u32::from_le_bytes(payload[8..12].try_into().unwrap()),
            },
            (5, 1) => Self::CompactInlinePropertyBytesSlabV1,
            (6, 1) => Self::MaterializeInlinePropertyStreamV1 {
                vid: VertexId::from(u32::from_le_bytes(payload[0..4].try_into().unwrap())),
                bucket_slot: u64::from_le_bytes(payload[4..12].try_into().unwrap()),
                width: u16::from_le_bytes(payload[12..14].try_into().unwrap()),
                fill_byte: payload[14],
                reserved_offset: u64::from_le_bytes(payload[15..23].try_into().unwrap()),
                total_bytes: u64::from_le_bytes(payload[23..31].try_into().unwrap()),
                resume_position: u64::from_le_bytes(payload[31..39].try_into().unwrap()),
            },
            (unknown_tag, unknown_version) => panic!(
                "maintenance work item: unsupported (tag, version) pair ({unknown_tag}, {unknown_version})"
            ),
        }
    }
}

#[inline]
fn current_instruction_counter() -> u64 {
    gleaph_instruction_budget::instruction_counter()
}

/// Errors returned by deferred labeled graph operations.
#[derive(Debug)]
pub enum DeferredError {
    /// Inner graph operation failed.
    Inner(LabeledOperationError),
}

impl fmt::Display for DeferredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inner(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for DeferredError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inner(err) => Some(err),
        }
    }
}

/// Stable queue key: `(priority, tag, discriminator)`. The variant tag is part of the key,
/// so a Dense-hash discriminator can never collide with another variant's discriminator
/// range. Lexicographic key order is the pop order: Retry before Routine.
mod priority {
    /// A stalled compaction step requeued for retry (ADR 0020 retry-never-drop).
    pub(super) const RETRY: u8 = 1;
    /// Routine compaction: dense / span / inline-property-bytes.
    pub(super) const ROUTINE: u8 = 2;
}

type MaintenanceQueueKey = (u8, u8, u32);

fn work_item_discriminator(item: &MaintenanceWorkItem) -> u32 {
    match item {
        MaintenanceWorkItem::CompactLabelBucketVertexSegmentV1 { vid } => {
            u32::from(*vid).saturating_mul(2)
        }
        MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
            vid,
            anchor_bucket_index,
            ..
        } => 0x4000_0000 | anchor_bucket_index ^ (u32::from(*vid) << 1),
        MaintenanceWorkItem::CompactDenseLabeledVertexMaintenanceV1 { vid } => {
            0xC000_0000u32 ^ u32::from(*vid).wrapping_mul(2_654_435_761)
        }
        MaintenanceWorkItem::CompactVertexValueSpanV1 { vid } => {
            0x8000_0000 | u32::from(*vid).wrapping_mul(2)
        }
        MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 {
            vid,
            anchor_bucket_index,
            ..
        } => 0xA000_0000 | anchor_bucket_index ^ (u32::from(*vid) << 1),
        MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1 => 0x6000_0000,
        MaintenanceWorkItem::MaterializeInlinePropertyStreamV1 {
            vid, bucket_slot, ..
        } => 0xE000_0000u32 ^ (u32::from(*vid).wrapping_mul(2_654_435_761) ^ *bucket_slot as u32),
    }
}
fn maintenance_queue_key(item: &MaintenanceWorkItem) -> MaintenanceQueueKey {
    (priority::ROUTINE, item.tag(), work_item_discriminator(item))
}

/// Priority-ordered maintenance queue backed by a stable B-tree map.
///
/// Replaces the FIFO `StableVecDeque`: key presence is the dedup promise (no
/// scan-and-avoid duplication for exact keys), and the Retry priority keeps stalled-step
/// retries ahead of routine compaction (ADR 0020). The queue must be empty at the format
/// switch (ADR 0052 activation resets stable data); work items are versioned per variant.
struct LabeledMaintenanceQueue<M: Memory> {
    map: RefCell<StableBTreeMap<MaintenanceQueueKey, MaintenanceWorkItem, M>>,
}

impl<M: Memory> LabeledMaintenanceQueue<M> {
    fn new(queue_memory: M) -> Self {
        Self {
            map: RefCell::new(StableBTreeMap::new(queue_memory)),
        }
    }

    fn init(queue_memory: M) -> Self {
        Self {
            map: RefCell::new(StableBTreeMap::init(queue_memory)),
        }
    }

    fn len(&self) -> u64 {
        self.map.borrow().len()
    }

    /// Inserts a routine work item unless an identical key is already pending (dedup).
    fn enqueue(&self, item: &MaintenanceWorkItem) {
        let key = maintenance_queue_key(item);
        let mut map = self.map.borrow_mut();
        if !map.contains_key(&key) {
            map.insert(key, item.clone());
        }
    }

    /// Pops the highest-priority pending item (first key), returning the exact key it was
    /// stored under so a stalled-step retry can re-insert at the Retry priority.
    fn pop_next(&self) -> Option<(MaintenanceQueueKey, MaintenanceWorkItem)> {
        self.map.borrow_mut().pop_first()
    }

    /// Re-inserts a popped item at `priority` (Retry for stalled steps). Re-insertion
    /// after a pop cannot collide: the item's own key was removed by the pop.
    fn requeue(&self, key: &MaintenanceQueueKey, priority: u8, item: &MaintenanceWorkItem) {
        self.map
            .borrow_mut()
            .insert((priority, key.1, key.2), item.clone());
    }

    /// Any span/dense/edge-value item pending for `vid` (coarse dedup scan; short-circuits).
    fn vertex_edge_span_maintenance_pending(&self, vid: VertexId) -> bool {
        self.map.borrow().iter().any(|entry| match entry.value() {
            MaintenanceWorkItem::CompactDenseLabeledVertexMaintenanceV1 { vid: queued } => {
                queued == vid
            }
            MaintenanceWorkItem::CompactVertexEdgeSpanV1 { vid: queued, .. } => queued == vid,
            MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 { vid: queued, .. } => {
                queued == vid
            }
            MaintenanceWorkItem::CompactLabelBucketVertexSegmentV1 { .. }
            | MaintenanceWorkItem::CompactVertexValueSpanV1 { .. }
            | MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1
            | MaintenanceWorkItem::MaterializeInlinePropertyStreamV1 { .. } => false,
        })
    }

    /// Any dense item pending for any vertex of `vid`'s leaf (short-circuits).
    fn dense_maintenance_pending_in_leaf(&self, vid: VertexId, segment_size: u32) -> bool {
        let segment_size = segment_size.max(1);
        let leaf = u32::from(vid) / segment_size;
        self.map.borrow().iter().any(|entry| match entry.value() {
            MaintenanceWorkItem::CompactDenseLabeledVertexMaintenanceV1 { vid: queued } => {
                u32::from(queued) / segment_size == leaf
            }
            _ => false,
        })
    }

    /// Any inline-property-bytes slab compaction pending (short-circuits).
    fn inline_property_bytes_compaction_pending(&self) -> bool {
        self.map.borrow().iter().any(|entry| {
            matches!(
                entry.value(),
                MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1
            )
        })
    }
}

/// Deferred-maintenance labeled LARA graph wrapper.
pub struct DeferredLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    inner: LabeledLaraGraph<E, M>,
    queue: LabeledMaintenanceQueue<M>,
}

impl<E, M> DeferredLabeledLaraGraph<E, M>
where
    E: CsrEdge + CsrEdgeTombstone,
    M: Memory,
{
    /// Wraps an existing labeled graph with a deferred maintenance queue.
    pub fn new(inner: LabeledLaraGraph<E, M>, queue_memory: M) -> Result<Self, DeferredError> {
        Ok(Self {
            inner,
            queue: LabeledMaintenanceQueue::new(queue_memory),
        })
    }

    /// Opens a deferred labeled graph from stable memories.
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
        inline_property_bytes_slab: M,
        value_free_spans: M,
        value_free_span_by_start: M,
        inline_property_bytes_log: M,
        value_blobs: M,
        ltb: M,
        queue_memory: M,
        capacities: crate::labeled::InitialCapacities,
        default_label: crate::labeled::BucketLabelKey,
    ) -> Result<Self, InitError> {
        let inner = LabeledLaraGraph::init(
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            inline_property_bytes_slab,
            value_free_spans,
            value_free_span_by_start,
            inline_property_bytes_log,
            value_blobs,
            ltb,
            capacities,
            default_label,
        )?;
        let queue = LabeledMaintenanceQueue::init(queue_memory);
        Ok(Self { inner, queue })
    }

    /// Returns the inner single-orientation labeled graph.
    pub fn inner(&self) -> &LabeledLaraGraph<E, M> {
        &self.inner
    }

    /// Returns the number of pending deferred maintenance items.
    pub fn maintenance_queue_len(&self) -> u64 {
        self.queue.len()
    }

    /// Inserts one labeled edge without an immediate leaf cascade; enqueues compaction
    /// when the owning PMA leaf is still dense afterward.
    pub fn insert_edge(
        &self,
        src: VertexId,
        label_id: crate::labeled::BucketLabelKey,
        edge: E,
        placement: crate::labeled::graph::EdgePlacementPolicy,
    ) -> Result<(), DeferredError> {
        let inline_property_bytes_compaction_needed = edge.edge_inline_property_byte_width() != 0
            && self
                .inner
                .inline_property_bytes_compaction_needed(u64::from(
                    edge.edge_inline_property_byte_width(),
                ))
                .map_err(DeferredError::Inner)?;
        self.inner
            .insert_edge_skip_leaf_cascade_deferred_inline_property(src, label_id, edge, placement)
            .map_err(DeferredError::Inner)?;
        if inline_property_bytes_compaction_needed {
            self.mark_compact_inline_property_bytes_slab()?;
        }
        self.maybe_enqueue_dense_vertex_maintenance(src)
    }

    /// Removes one edge without an immediate leaf cascade; may enqueue compaction when dense.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: crate::labeled::BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, DeferredError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let removed = self
            .inner
            .remove_edge_matching_skip_leaf_cascade(src, label_id, matches)
            .map_err(DeferredError::Inner)?;
        if removed.is_some() {
            self.maybe_enqueue_tombstone_vertex_edge_span_maintenance(src)?;
            if self.inner.labeled_leaf_segment_is_dense(src) {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _bench_scope = bench_scope("labeled_deferred_remove_dense_enqueue");
                self.maybe_enqueue_dense_vertex_maintenance(src)?;
            }
        }
        Ok(removed)
    }

    fn vertex_edge_span_maintenance_pending(&self, vid: VertexId) -> bool {
        self.queue.vertex_edge_span_maintenance_pending(vid)
    }

    fn dense_maintenance_pending_in_leaf(&self, vid: VertexId) -> bool {
        let segment_size = self.inner.edges().header().segment_size.max(1);
        self.queue
            .dense_maintenance_pending_in_leaf(vid, segment_size)
    }

    fn inline_property_bytes_compaction_pending(&self) -> bool {
        self.queue.inline_property_bytes_compaction_pending()
    }

    /// Enqueues inline-property-bytes-only compaction, deduplicated across the graph.
    pub fn mark_compact_inline_property_bytes_slab(&self) -> Result<(), DeferredError> {
        if self.inline_property_bytes_compaction_pending() {
            return Ok(());
        }
        self.queue
            .enqueue(&MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1);
        Ok(())
    }

    fn maybe_enqueue_dense_vertex_maintenance(&self, vid: VertexId) -> Result<(), DeferredError> {
        if !self.inner.labeled_leaf_segment_is_dense(vid) {
            return Ok(());
        }
        if self.dense_maintenance_pending_in_leaf(vid) {
            return Ok(());
        }
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_deferred_dense_enqueue");
        self.mark_compact_dense_labeled_vertex_maintenance(vid)
    }

    fn maybe_enqueue_tombstone_vertex_edge_span_maintenance(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
        if self.vertex_edge_span_maintenance_pending(vid) {
            return Ok(());
        }
        if !self
            .inner
            .vertex_has_slab_tombstone_slack_pressure(vid)
            .map_err(DeferredError::Inner)?
        {
            return Ok(());
        }
        self.mark_compact_vertex_edge_span(vid, 0)
    }

    /// Enqueues bucket-segment compaction followed by vertex-edge-span compaction for one vertex.
    pub fn mark_compact_dense_labeled_vertex_maintenance(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
        if self.dense_maintenance_pending_in_leaf(vid) {
            return Ok(());
        }
        self.queue
            .enqueue(&MaintenanceWorkItem::CompactDenseLabeledVertexMaintenanceV1 { vid });
        Ok(())
    }

    /// Enqueues exact-fit compaction of the owning LabelBucketStore VertexSegment.
    pub fn mark_compact_label_bucket_vertex_segment(
        &self,
        vid: VertexId,
    ) -> Result<(), DeferredError> {
        self.queue
            .enqueue(&MaintenanceWorkItem::CompactLabelBucketVertexSegmentV1 { vid });
        Ok(())
    }

    /// Enqueues compaction of one VertexEdgeSpan.
    pub fn mark_compact_vertex_edge_span(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<(), DeferredError> {
        if self.vertex_edge_span_maintenance_pending(vid) {
            return Ok(());
        }
        self.queue
            .enqueue(&MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
                vid,
                anchor_bucket_index: bucket_index,
                resume_bucket_index: 0,
                resume_slot_index: 0,
            });
        Ok(())
    }

    /// Processes queued labeled maintenance work up to `budget`.
    pub fn maintenance(&self, budget: MaintenanceBudget) -> MaintenanceWorkReport
    where
        E: CsrEdgeTombstone,
    {
        use crate::labeled::graph::VertexEdgeSpanCompactOneStep;

        let mut report = MaintenanceWorkReport::default();
        let baseline = current_instruction_counter();
        let max_items = budget.max_work_items.unwrap_or(u32::MAX);
        let mut checkpoint_tick = 0u32;

        while report.processed_work_items < max_items {
            checkpoint_tick = checkpoint_tick.wrapping_add(1);
            let should_check = budget.checkpoint_every <= 1
                || checkpoint_tick.is_multiple_of(budget.checkpoint_every);
            if should_check
                && budget.max_instructions > 0
                && gleaph_instruction_budget::should_cutoff(
                    budget.max_instructions,
                    current_instruction_counter().saturating_sub(baseline),
                    0,
                    budget.reserve_instructions,
                    0,
                )
            {
                break;
            }

            let Some((item_key, item)) = self.queue.pop_next() else {
                break;
            };
            report.processed_work_items = report.processed_work_items.saturating_add(1);
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_deferred_maintenance_item");
            // Set when a compaction step fails; the partially mutated span must be retried, not
            // dropped. We requeue it at the Retry priority and stop the pass to avoid
            // hot-looping a deterministic failure; the next maintenance call retries it with a
            // fresh budget (ADR 0020 retry-never-drop).
            let mut stalled = false;
            let requeue = match &item {
                MaintenanceWorkItem::CompactLabelBucketVertexSegmentV1 { vid } => {
                    if self.inner.compact_label_bucket_vertex_segment(*vid).is_ok() {
                        report.rebalanced_segments = report.rebalanced_segments.saturating_add(1);
                    }
                    None
                }
                MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
                    vid,
                    anchor_bucket_index,
                    resume_bucket_index,
                    resume_slot_index,
                } => {
                    let vertex = self.inner.vertices().get(*vid);
                    if *anchor_bucket_index >= vertex.degree() {
                        None
                    } else {
                        match self.inner.compact_vertex_edge_span_one_step(
                            *vid,
                            *resume_bucket_index,
                            *resume_slot_index,
                            &|_| EdgePlacementPolicy::Insertion,
                        ) {
                            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(moved)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
                                    vid: *vid,
                                    anchor_bucket_index: *anchor_bucket_index,
                                    resume_bucket_index: *resume_bucket_index,
                                    resume_slot_index: moved.new_slot_index.saturating_add(1),
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
                                    vid: *vid,
                                    anchor_bucket_index: *anchor_bucket_index,
                                    resume_bucket_index: next,
                                    resume_slot_index: 0,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeSpanV1 {
                                    vid: *vid,
                                    anchor_bucket_index: *anchor_bucket_index,
                                    resume_bucket_index: 0,
                                    resume_slot_index: 0,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                                report.rebalanced_segments =
                                    report.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => {
                                stalled = true;
                                None
                            }
                        }
                    }
                }
                MaintenanceWorkItem::CompactDenseLabeledVertexMaintenanceV1 { vid } => {
                    if self
                        .inner
                        .compact_label_bucket_vertex_segment(*vid)
                        .is_err()
                    {
                        stalled = true;
                        None
                    } else {
                        match self.inner.rebalance_cascade_after_labeled_mutation(*vid) {
                            Ok(()) => {
                                report.rebalanced_segments =
                                    report.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => {
                                stalled = true;
                                None
                            }
                        }
                    }
                }
                MaintenanceWorkItem::CompactVertexValueSpanV1 { .. } => None,
                MaintenanceWorkItem::CompactInlinePropertyBytesSlabV1 => {
                    match self.inner.compact_inline_property_bytes_slab() {
                        Ok(result) => {
                            if result.moved_spans > 0 {
                                report.rebalanced_segments =
                                    report.rebalanced_segments.saturating_add(1);
                            }
                            None
                        }
                        Err(_) => {
                            stalled = true;
                            None
                        }
                    }
                }
                MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 {
                    vid,
                    anchor_bucket_index,
                    resume_bucket_index,
                } => {
                    let vertex = self.inner.vertices().get(*vid);
                    if *anchor_bucket_index >= vertex.degree() {
                        None
                    } else {
                        match self.inner.compact_vertex_edge_span_one_step(
                            *vid,
                            *resume_bucket_index,
                            0,
                            &|_| EdgePlacementPolicy::Insertion,
                        ) {
                            Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 {
                                    vid: *vid,
                                    anchor_bucket_index: *anchor_bucket_index,
                                    resume_bucket_index: *resume_bucket_index,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(next)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 {
                                    vid: *vid,
                                    anchor_bucket_index: *anchor_bucket_index,
                                    resume_bucket_index: next,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(_)) => {
                                Some(MaintenanceWorkItem::CompactVertexEdgeAndValueSpanV1 {
                                    vid: *vid,
                                    anchor_bucket_index: *anchor_bucket_index,
                                    resume_bucket_index: 0,
                                })
                            }
                            Ok(VertexEdgeSpanCompactOneStep::Finished) => {
                                report.rebalanced_segments =
                                    report.rebalanced_segments.saturating_add(1);
                                None
                            }
                            Err(_) => {
                                stalled = true;
                                None
                            }
                        }
                    }
                }
                MaintenanceWorkItem::MaterializeInlinePropertyStreamV1 {
                    vid,
                    bucket_slot,
                    width,
                    fill_byte,
                    reserved_offset,
                    total_bytes,
                    resume_position,
                } => {
                    match self.inner.materialize_inline_property_stream_step(
                        *vid,
                        *bucket_slot,
                        *width,
                        *fill_byte,
                        *reserved_offset,
                        *total_bytes,
                        *resume_position,
                    ) {
                        Ok(MaterializeStep::Finished) => {
                            report.rebalanced_segments =
                                report.rebalanced_segments.saturating_add(1);
                            None
                        }
                        Ok(MaterializeStep::Resumed {
                            reserved_offset: next_reserved,
                            resume_position: next_resume,
                        }) => Some(MaintenanceWorkItem::MaterializeInlinePropertyStreamV1 {
                            vid: *vid,
                            bucket_slot: *bucket_slot,
                            width: *width,
                            fill_byte: *fill_byte,
                            reserved_offset: next_reserved,
                            total_bytes: *total_bytes,
                            resume_position: next_resume,
                        }),
                        Err(_) => {
                            stalled = true;
                            None
                        }
                    }
                }
            };
            if stalled {
                // Keep the failed item queued at the Retry priority (its key is unchanged)
                // and stop the pass.
                self.queue.requeue(&item_key, priority::RETRY, &item);
                break;
            }
            if let Some(next) = requeue {
                self.queue.requeue(&item_key, priority::ROUTINE, &next);
            }
        }
        report.remaining_queue_len = self.queue.len();
        report
    }
}
