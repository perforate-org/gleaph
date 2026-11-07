//! The one owner of "how big is this thing in the leaf?" - as a view, so the answer cannot be recomputed elsewhere.
//!
//! `design/investigations/2026-11-08-owning-region-accounting.md` found seven functions answering parts of this question from
//! different inputs, and the disagreement between two of them (promotion's `resident` versus
//! `bucket_span_region_len`'s `stored_slots`) is what keeps the edge-side cover defect open. This view is the seam where
//! those answers get one owner: callers ask the view, the view asks the geometry functions, and the *types* it returns
//! ([`BucketLeafSpan`], [`VertexLeafCover`]) carry private fields, so a caller cannot substitute its own arithmetic.
//!
//! What this does not do yet: the seven functions are still crate-visible, so an attacker-in-a-hurry can bypass the view. The
//! next slice makes them private to this module - that is a mechanical, compile-error-driven change, and it belongs in its own
//! patch rather than mixed into the first one that introduces the seam.

use super::compact::{SourceRows, bucket_content_slots, bucket_span_region_len, source_rows};
use super::leaf_cover::{BucketLeafSpan, SlackRule, VertexLeafCover};
use super::{BucketMode, LabeledLaraGraph};
use crate::Address;
use crate::VertexId;
use crate::labeled::record::{LabelBucket, LabeledVertex};
use crate::traits::CsrEdgeTombstone;
use ic_stable_structures::Memory;

/// Borrowed view over one graph's leaf geometry.
///
/// Every entry answers a *derived* question; none of them lets the caller supply a length, a depth or a base. The view holds
/// only the graph, so it cannot go stale against a store it captured before a mutation - callers re-derive through a fresh
/// view, which is what keeps a cached number from outliving the rows it described.
pub(crate) struct LeafGeometry<'a, E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
}

impl<'a, E, M> LeafGeometry<'a, E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// The view for `graph`. The only constructor.
    pub(crate) fn of(graph: &'a LabeledLaraGraph<E, M>) -> Self {
        Self { graph }
    }

    /// What one bucket occupies in the leaf.
    ///
    /// Tiny buckets and run-backed tree roots occupy nothing there, and say so as an empty span rather than as a zero length
    /// someone has to remember to special-case.
    pub(crate) fn bucket_span(&self, bucket: &LabelBucket) -> BucketLeafSpan {
        match BucketMode::from_bucket(bucket) {
            BucketMode::Tiny => BucketLeafSpan::empty(),
            BucketMode::Slab => {
                let initialized = self
                    .graph
                    .edges()
                    .initialized_slab_slots(bucket)
                    .expect("initialized slab slots must be readable");
                BucketLeafSpan::new(bucket.edge_start(), initialized)
            }
            BucketMode::Tree if bucket.edge_run().is_some() => BucketLeafSpan::empty(),
            BucketMode::Tree => {
                BucketLeafSpan::new(bucket.edge_start(), bucket_span_region_len(bucket))
            }
        }
    }

    /// The edge root's leaf region, whether it lives in the leaf or in a run.
    ///
    /// The promotion needs this *before* a descriptor exists for the promoted bucket, so it cannot go through
    /// [`Self::bucket_span`] on the published row; it asks about the template instead.
    pub(crate) fn edge_root_len(&self, bucket: &LabelBucket) -> u32 {
        bucket_span_region_len(bucket)
    }

    /// The physical rows the bucket's source holds, and how many of them are live.
    pub(crate) fn resident_rows(
        &self,
        bucket: &LabelBucket,
    ) -> Result<SourceRows, super::LabeledOperationError> {
        source_rows(self.graph, bucket)
    }

    /// Physical leaf slots the bucket's content occupies (log chains excluded).
    pub(crate) fn content_slots(&self, bucket: &LabelBucket) -> u32 {
        bucket_content_slots(bucket)
    }

    /// The vertex's cover, derived from every bucket it owns.
    ///
    /// This is the entry the promotion case needed and did not have: the cover is not passed in, it is computed from what the
    /// vertex's buckets occupy, so a cover that does not back them cannot be expressed.
    pub(crate) fn vertex_cover(
        &self,
        vid: VertexId,
        slack: SlackRule,
    ) -> Result<VertexLeafCover, super::LabeledOperationError> {
        self.graph.ensure_vertex(vid)?;
        let vertex: LabeledVertex = self.graph.vertices().get(vid);
        if vertex.is_default_edge_labeled() {
            return Ok(VertexLeafCover::empty());
        }
        let buckets = self.graph.read_vertex_label_buckets(&vertex)?;
        Ok(VertexLeafCover::derive(
            buckets.iter().map(|bucket| self.bucket_span(bucket)),
            slack,
        ))
    }

    /// The edge-store address the vertex's cover starts at, or zero when it owns nothing.
    pub(crate) fn cover_anchor(
        &self,
        vid: VertexId,
    ) -> Result<Address, super::LabeledOperationError> {
        Ok(Address::from(
            self.vertex_cover(vid, SlackRule::None)?.start_anchor(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::labeled::BucketLabelKey;
    use crate::labeled::graph::{EdgePlacementPolicy, T_PROMOTE};
    use crate::test_support::vector_memory;

    fn wide_graph()
    -> LabeledLaraGraph<crate::labeled::graph::test_support::InlinePropertyTestEdge, VectorMemory>
    {
        crate::labeled::graph::test_support::inline_property_test_graph()
    }

    /// A slab bucket occupies its initialized rows, not its slack.
    #[test]
    fn bucket_span_reports_initialized_rows_for_a_slab_bucket() {
        let graph = wide_graph();
        let vid = graph
            .push_vertex(LabeledVertex::default())
            .expect("push vertex");
        let label = BucketLabelKey::directed_from_index(2);
        // More rows than a tiny bucket can hold, so the fixture reaches slab mode.
        for target in 0..8 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target,
                        &[],
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        let span = LeafGeometry::of(&graph).bucket_span(&bucket);
        assert!(!span.is_empty());
        assert_eq!(span.start(), bucket.edge_start());
        assert!(
            span.len() <= bucket.stored_slots(),
            "a span never exceeds the bucket's allocated width"
        );
    }

    /// A tiny bucket occupies nothing in the leaf: its payload is inline.
    #[test]
    fn bucket_span_is_empty_for_a_tiny_bucket() {
        let graph = wide_graph();
        let vid = graph
            .push_vertex(LabeledVertex::default())
            .expect("push vertex");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(7, &[]),
                EdgePlacementPolicy::Insertion,
            )
            .expect("insert");
        let vertex = graph.vertices().get(vid);
        let slot = graph
            .find_bucket_slot(&vertex, label)
            .expect("find")
            .expect("bucket");
        let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert!(bucket.is_tiny_mode(), "the fixture must stay tiny");
        assert!(LeafGeometry::of(&graph).bucket_span(&bucket).is_empty());
    }

    /// The derived cover backs every bucket the vertex owns - the property the promotion case violated.
    #[test]
    fn vertex_cover_backs_every_bucket_span() {
        let graph = wide_graph();
        let vid = graph
            .push_vertex(LabeledVertex::default())
            .expect("push vertex");
        let label = BucketLabelKey::directed_from_index(2);
        for target in 0..(T_PROMOTE + 2) {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target,
                        &[],
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        let buckets = graph.read_vertex_label_buckets(&vertex).expect("buckets");
        let geometry = LeafGeometry::of(&graph);
        let cover = geometry.vertex_cover(vid, SlackRule::None).expect("cover");
        for bucket in &buckets {
            cover
                .backs(geometry.bucket_span(bucket))
                .unwrap_or_else(|miss| panic!("{}: {miss}", bucket.bucket_label_key().raw()));
        }
    }

    /// The view is a fresh read: it answers from the stores it borrows, so a second call sees a later mutation.
    #[test]
    fn cover_follows_the_rows() {
        let graph = wide_graph();
        let vid = graph
            .push_vertex(LabeledVertex::default())
            .expect("push vertex");
        let label = BucketLabelKey::directed_from_index(2);
        let geometry = LeafGeometry::of(&graph);
        assert_eq!(
            geometry
                .vertex_cover(vid, SlackRule::None)
                .expect("cover")
                .len(),
            0
        );
        for target in 0..8 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    crate::labeled::graph::test_support::InlinePropertyTestEdge::with_bytes(
                        target,
                        &[],
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        assert!(
            !geometry
                .vertex_cover(vid, SlackRule::None)
                .expect("cover")
                .is_empty()
        );
    }

    fn _unused_memory() -> VectorMemory {
        vector_memory()
    }
}
