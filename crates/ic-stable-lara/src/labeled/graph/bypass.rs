//! Labeled graph `bypass` implementation.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketLabelKey},
        record::{LabelBucket, LabeledVertex},
    },
    lara::operation_error::LaraOperationError,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::LabeledLaraGraph;
use super::error::LabeledOperationError;

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn is_homogeneous_bypass_label(&self, label_id: BucketLabelKey) -> bool {
        let raw = label_id.raw();
        let default = self.default_label.raw();
        raw == default || raw == (default & BUCKET_LABEL_INDEX_MASK)
    }

    pub(super) fn may_use_homogeneous_bypass(&self, src: VertexId) -> bool {
        match u32::from(src).checked_add(1) {
            Some(next) => next >= self.vertices.len(),
            None => false,
        }
    }

    pub(super) fn bypass_storage_label_for(&self, vertex: &LabeledVertex) -> BucketLabelKey {
        vertex.bypass_storage_label(self.default_label)
    }

    pub(super) fn bump_successor_origins_after_bypass_end(
        &self,
        src: VertexId,
        region_end: u64,
    ) -> Result<(), LabeledOperationError> {
        let first = u32::from(src)
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        for idx in first..self.vertices.len() {
            let vid = VertexId::from(idx);
            let successor = self.vertices.get(vid);
            if successor.is_default_edge_labeled() && successor.base_slot_start() < region_end {
                self.set_labeled_vertex(vid, successor.with_base_slot_start(region_end))?;
            }
        }
        Ok(())
    }

    pub(super) fn insert_homogeneous_bypass_edge(
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

    pub(super) fn bypass_region_end(&self, src: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        crate::labeled::slot_index::checked_add_slot_index(
            vertex.base_slot_start(),
            u64::from(vertex.stored_degree()),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn ensure_bypass_edge_origin(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
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

    pub(super) fn insert_homogeneous_bypass(
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

    pub(super) fn promote_bypass_to_bucket_mode(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
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

        // Plan the segment-bucket rewrite without allocating or mutating anything.
        let new_alloc = self.edges.header().segment_size.max(1).max(stored_slots);
        let mut plan = self.buckets.plan_promote_bypass_to_bucket_mode(
            &self.vertices,
            src,
            LabelBucket::from_parts(bypass_label, edge_start, logical_degree, stored_slots, -1),
            new_alloc,
        )?;

        // Verify the segment-count tree already covers `src` before any
        // fallible allocation. This is the only remaining check inside
        // `bump_vertex_segment_counts` and it cannot grow the store, so checking
        // it here keeps the subsequent reserve and commit infallible.
        let edge_layout = crate::lara::edge::EdgeLayout::from(self.edges.header());
        let leaf = Self::leaf_index_for_vid(src, edge_layout.segment_size);
        let counts_idx = leaf
            .checked_add(edge_layout.segment_count)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if counts_idx as u64 >= self.edges.counts_store().len() {
            return Err(LaraOperationError::SegmentCountsTreeTooSmall.into());
        }

        // Reserve all fallible backing capacity while the bypass row is still the
        // canonical interpretation. No vertex row is mutated until reservation
        // succeeds.
        self.buckets
            .reserve_promote_bypass_to_bucket_mode(&mut plan)?;

        // Commit: publish the bucket-mode row. The first vertex write is the
        // source row ceasing to be a valid bypass interpretation; every
        // subsequent operation below was preflighted and cannot return a
        // recoverable error.
        self.buckets
            .commit_promote_bypass_to_bucket_mode(&self.vertices, plan)?;
        self.invalidate_bucket_lookup_caches_for_bucket_segment(src)
            .expect("commit: cache invalidation must succeed after preflight");
        self.edges
            .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))
            .expect("commit: segment count bump must succeed after preflight");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use super::*;
    use crate::{
        VertexId,
        test_support::{FailpointMemory, failpoint_labeled_memories},
    };

    fn failpoint_labeled_graph(
        default_label: BucketLabelKey,
    ) -> (
        LabeledLaraGraph<TestEdge, FailpointMemory>,
        [FailpointMemory; 15],
    ) {
        let mems = failpoint_labeled_memories();
        let graph = LabeledLaraGraph::<TestEdge, _>::new(
            mems[0].clone(),
            mems[1].clone(),
            mems[2].clone(),
            mems[3].clone(),
            mems[4].clone(),
            mems[5].clone(),
            mems[6].clone(),
            mems[7].clone(),
            mems[8].clone(),
            mems[9].clone(),
            mems[10].clone(),
            mems[11].clone(),
            mems[12].clone(),
            mems[13].clone(),
            mems[14].clone(),
            crate::labeled::InitialCapacities::uniform(256),
            default_label,
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        (graph, mems)
    }

    fn reopen_failpoint_labeled_graph(
        mems: &[FailpointMemory; 15],
        default_label: BucketLabelKey,
    ) -> LabeledLaraGraph<TestEdge, FailpointMemory> {
        LabeledLaraGraph::<TestEdge, _>::init(
            mems[0].clone(),
            mems[1].clone(),
            mems[2].clone(),
            mems[3].clone(),
            mems[4].clone(),
            mems[5].clone(),
            mems[6].clone(),
            mems[7].clone(),
            mems[8].clone(),
            mems[9].clone(),
            mems[10].clone(),
            mems[11].clone(),
            mems[12].clone(),
            mems[13].clone(),
            mems[14].clone(),
            crate::labeled::InitialCapacities::uniform(256),
            default_label,
        )
        .unwrap()
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
            mem(),
            crate::labeled::InitialCapacities::uniform(1),
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
    fn bypass_grow_does_not_repoint_bucket_mode_successor_bucket_base() {
        let graph = LabeledLaraGraph::new_with_segment_size(
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
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 16),
            BucketLabelKey::from_raw(1),
            32,
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

    /// Fills the bucket slab by promoting vertices until a direct
    /// bypass-to-bucket-mode promotion of the next vertex requires a bucket-slab
    /// grow.
    ///
    /// Returns that vertex. It is still in homogeneous bypass mode, so a
    /// subsequent call to [`LabeledLaraGraph::promote_bypass_to_bucket_mode`] will
    /// hit the same bucket-slab grow that we failed above.
    fn fill_bucket_slab_to_next_grow(
        graph: &LabeledLaraGraph<TestEdge, FailpointMemory>,
        mems: &[FailpointMemory; 15],
        default: BucketLabelKey,
    ) -> VertexId {
        let mut next: u32 = 1;
        loop {
            let vid = VertexId::from(next);
            while graph.vertices().len() <= next {
                graph.push_vertex(LabeledVertex::default()).unwrap();
            }
            // Give the vertex a single default-label bypass edge so the vertex is
            // eligible for bypass promotion.
            graph
                .insert_edge(vid, default, TestEdge { target: next + 1 })
                .unwrap();

            // Try to promote the vertex directly with the next bucket-slab grow failed.
            // If promotion succeeds, the slab still has slack; continue. If it fails,
            // the vertex is unchanged and its next real promotion will require the grow.
            let grow_before = mems[1].grow_count();
            mems[1].fail_at_grow(grow_before.saturating_add(1));
            let result = graph.promote_bypass_to_bucket_mode(vid);
            mems[1].fail_never();

            if result.is_err() {
                return vid;
            }

            next += 1;
            assert!(
                next <= 10000,
                "bucket slab did not reach a grow boundary within the bounded probe"
            );
        }
    }

    #[test]
    fn bypass_promotion_failure_atomic_at_bucket_slab_allocation() {
        let default = BucketLabelKey::directed_from_index(1);
        let (graph, mems) = failpoint_labeled_graph(default);
        for target in 1..=4 {
            graph
                .insert_edge(VertexId::from(0), default, TestEdge { target })
                .unwrap();
        }

        let source = fill_bucket_slab_to_next_grow(&graph, &mems, default);
        let before_vertex = graph.vertices().get(source);
        assert!(before_vertex.is_default_edge_labeled());
        let before_edges = graph.iter_edges_for_label(source, default).unwrap();
        let before_labels = graph.out_edge_label_ids(source).unwrap();

        mems[1].fail_at_grow(mems[1].grow_count().saturating_add(1));
        let result = graph.promote_bypass_to_bucket_mode(source);
        assert!(result.is_err(), "expected bucket-slab grow to fail");

        let after_vertex = graph.vertices().get(source);
        assert_eq!(after_vertex, before_vertex, "bypass row must be unchanged");
        assert_eq!(
            graph.iter_edges_for_label(source, default).unwrap(),
            before_edges,
            "default-label edges must be unchanged"
        );
        assert_eq!(
            graph.out_edge_label_ids(source).unwrap(),
            before_labels,
            "out-edge labels must be unchanged"
        );

        let reopened = reopen_failpoint_labeled_graph(&mems, default);
        assert_eq!(reopened.vertices().get(source), before_vertex);
        assert_eq!(
            reopened.iter_edges_for_label(source, default).unwrap(),
            before_edges
        );
    }

    #[test]
    fn bypass_promotion_success_after_rejected_bucket_slab_allocation() {
        let default = BucketLabelKey::directed_from_index(1);
        let (graph, mems) = failpoint_labeled_graph(default);
        for target in 1..=4 {
            graph
                .insert_edge(VertexId::from(0), default, TestEdge { target })
                .unwrap();
        }

        let source = fill_bucket_slab_to_next_grow(&graph, &mems, default);
        let before_edges = graph.iter_edges_for_label(source, default).unwrap();

        mems[1].fail_at_grow(mems[1].grow_count().saturating_add(1));
        assert!(
            graph.promote_bypass_to_bucket_mode(source).is_err(),
            "expected first promotion to be rejected"
        );

        mems[1].fail_never();
        graph
            .promote_bypass_to_bucket_mode(source)
            .expect("retry after rejected bucket-slab allocation");

        // After promotion succeeds, add a new non-default label through the public API
        // to show the promoted vertex is fully usable.
        let road = BucketLabelKey::directed_from_index(5001);
        graph
            .insert_edge(source, road, TestEdge { target: 99 })
            .expect("insert after successful promotion");

        let vertex = graph.vertices().get(source);
        assert!(!vertex.is_default_edge_labeled());
        let labels = graph.out_edge_label_ids(source).unwrap();
        assert!(
            labels.contains(&default),
            "default label must still be present"
        );
        assert!(labels.contains(&road), "new label must be present");
        assert_eq!(
            graph.iter_edges_for_label(source, default).unwrap(),
            before_edges,
            "default-label edges must survive retry"
        );
        assert_eq!(
            graph.iter_edges_for_label(source, road).unwrap(),
            vec![TestEdge { target: 99 }]
        );

        let reopened = reopen_failpoint_labeled_graph(&mems, default);
        let mut reopened_labels = reopened.out_edge_label_ids(source).unwrap();
        reopened_labels.sort();
        let mut expected_labels = vec![default, road];
        expected_labels.sort();
        assert_eq!(reopened_labels, expected_labels);
        assert_eq!(
            reopened.iter_edges_for_label(source, road).unwrap(),
            vec![TestEdge { target: 99 }]
        );
    }

    /// Fills the bucket free-span record store (mems[2]) until the next record
    /// allocation would require a memory grow. The first span establishes the
    /// initial page; subsequent spans fill the page until the next record would
    /// force a grow. That final grow is failed deliberately, leaving the store
    /// exactly at the page boundary with no mutation performed.
    fn fill_bucket_free_spans_to_next_records_grow(
        graph: &LabeledLaraGraph<TestEdge, FailpointMemory>,
        mems: &[FailpointMemory; 15],
    ) {
        // Establish the initial free-span record page. This also lets the
        // by-start index acquire the pages it needs for the relatively small
        // number of records we are about to add, so the next grow we hit is
        // the record store itself.
        graph
            .buckets()
            .release_span(10_000_000, 1)
            .expect("first release establishes the record page");

        let mut released: u64 = 1;
        loop {
            let slot = 10_000_000u64 + released.saturating_mul(2);
            let grow_before = mems[2].grow_count();
            mems[2].fail_at_grow(grow_before + 1);
            let result = graph.buckets().release_span(slot, 1);
            mems[2].fail_never();

            match result {
                Ok(()) => {
                    released += 1;
                    assert!(
                        released <= 2_000,
                        "record store did not reach a grow boundary in reasonable releases"
                    );
                }
                Err(_) => {
                    assert_eq!(
                        mems[2].grow_count(),
                        grow_before + 1,
                        "expected the record-store grow to be attempted"
                    );
                    return;
                }
            }
        }
    }

    #[test]
    fn bypass_promotion_failure_atomic_at_free_span_reservation() {
        let default = BucketLabelKey::directed_from_index(1);
        let (graph, mems) = failpoint_labeled_graph(default);
        for target in 1..=4 {
            graph
                .insert_edge(VertexId::from(0), default, TestEdge { target })
                .unwrap();
        }

        // Seed a peer vertex in the same segment that is already in bucket mode.
        // Promoting the source will rewrite the whole segment, so the peer's old
        // bucket span becomes a free-span release that reserve_for_releases must
        // accommodate.
        let road = BucketLabelKey::directed_from_index(2);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(VertexId::from(1), road, TestEdge { target: 100 })
            .unwrap();

        fill_bucket_free_spans_to_next_records_grow(&graph, &mems);

        let source = VertexId::from(0);
        let before_vertex = graph.vertices().get(source);
        assert!(before_vertex.is_default_edge_labeled());
        let before_edges = graph.iter_edges_for_label(source, default).unwrap();
        let before_labels = graph.out_edge_label_ids(source).unwrap();
        let before_segment_count = graph.edges.header().segment_count;
        let before_counts_len = graph.edges.counts_store().len();
        let before_slab_grow_count = mems[1].grow_count();

        mems[2].fail_at_grow(mems[2].grow_count().saturating_add(1));
        let result = graph.promote_bypass_to_bucket_mode(source);
        assert!(
            result.is_err(),
            "expected free-span reservation grow to fail"
        );
        mems[2].fail_never();

        let after_vertex = graph.vertices().get(source);
        assert_eq!(after_vertex, before_vertex, "bypass row must be unchanged");
        assert_eq!(
            graph.iter_edges_for_label(source, default).unwrap(),
            before_edges,
            "default-label edges must be unchanged"
        );
        assert_eq!(
            graph.out_edge_label_ids(source).unwrap(),
            before_labels,
            "out-edge labels must be unchanged"
        );
        assert_eq!(
            graph.edges.header().segment_count,
            before_segment_count,
            "segment count must be unchanged"
        );
        assert_eq!(
            graph.edges.counts_store().len(),
            before_counts_len,
            "counts store length must be unchanged"
        );
        assert_eq!(
            mems[1].grow_count(),
            before_slab_grow_count,
            "bucket-slab allocation must not occur"
        );

        let reopened = reopen_failpoint_labeled_graph(&mems, default);
        assert_eq!(reopened.vertices().get(source), before_vertex);
        assert_eq!(
            reopened.iter_edges_for_label(source, default).unwrap(),
            before_edges
        );
        assert_eq!(reopened.out_edge_label_ids(source).unwrap(), before_labels);
    }

    #[test]
    fn bypass_promotion_success_after_rejected_free_span_reservation() {
        let default = BucketLabelKey::directed_from_index(1);
        let (graph, mems) = failpoint_labeled_graph(default);
        for target in 1..=4 {
            graph
                .insert_edge(VertexId::from(0), default, TestEdge { target })
                .unwrap();
        }

        let road = BucketLabelKey::directed_from_index(2);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(VertexId::from(1), road, TestEdge { target: 100 })
            .unwrap();

        fill_bucket_free_spans_to_next_records_grow(&graph, &mems);

        let source = VertexId::from(0);
        let before_edges = graph.iter_edges_for_label(source, default).unwrap();

        mems[2].fail_at_grow(mems[2].grow_count().saturating_add(1));
        assert!(
            graph.promote_bypass_to_bucket_mode(source).is_err(),
            "expected first promotion to be rejected"
        );
        mems[2].fail_never();

        graph
            .promote_bypass_to_bucket_mode(source)
            .expect("retry after rejected free-span reservation");

        let vertex = graph.vertices().get(source);
        assert!(!vertex.is_default_edge_labeled());
        let labels = graph.out_edge_label_ids(source).unwrap();
        assert!(
            labels.contains(&default),
            "default label must still be present"
        );
        assert_eq!(
            graph.iter_edges_for_label(source, default).unwrap(),
            before_edges,
            "default-label edges must survive retry"
        );
        // The peer label lives on vertex 1, not on the promoted source.
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(1), road).unwrap(),
            vec![TestEdge { target: 100 }],
            "peer label bucket must survive segment rewrite"
        );

        let reopened = reopen_failpoint_labeled_graph(&mems, default);
        assert!(
            reopened
                .out_edge_label_ids(source)
                .unwrap()
                .contains(&default)
        );
        assert_eq!(
            reopened
                .iter_edges_for_label(VertexId::from(1), road)
                .unwrap(),
            vec![TestEdge { target: 100 }]
        );
    }
    #[test]
    fn undirected_homogeneous_bypass_promotion_failure_atomic_at_bucket_slab_allocation() {
        let undirected_default = BucketLabelKey::UNLABELED_UNDIRECTED;
        let (graph, mems) = failpoint_labeled_graph(undirected_default);
        for target in 1..=3 {
            graph
                .insert_edge(VertexId::from(0), undirected_default, TestEdge { target })
                .unwrap();
        }

        let source = fill_bucket_slab_to_next_grow(&graph, &mems, undirected_default);
        let before_vertex = graph.vertices().get(source);
        assert!(before_vertex.is_default_edge_labeled());
        assert!(before_vertex.is_bypass_undirected());
        let before_edges = graph
            .iter_edges_for_label(source, undirected_default)
            .unwrap();

        mems[1].fail_at_grow(mems[1].grow_count().saturating_add(1));
        let result = graph.promote_bypass_to_bucket_mode(source);
        assert!(result.is_err(), "expected bucket-slab grow to fail");

        let after_vertex = graph.vertices().get(source);
        assert_eq!(after_vertex, before_vertex, "bypass row must be unchanged");
        assert!(
            after_vertex.is_bypass_undirected(),
            "undirected bypass flag must be preserved"
        );
        assert_eq!(
            graph
                .iter_edges_for_label(source, undirected_default)
                .unwrap(),
            before_edges,
            "default undirected edges must be unchanged"
        );

        let reopened = reopen_failpoint_labeled_graph(&mems, undirected_default);
        assert_eq!(reopened.vertices().get(source), before_vertex);
    }
}
