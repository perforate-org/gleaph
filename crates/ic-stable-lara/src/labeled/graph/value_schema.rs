//! One bucket's inline value schema, borrowed for one operation.
//!
//! ADR 0100's last family is the inline value schema: declaring a width, migrating a bucket from width 0 to that
//! width, and scheduling the migration when it is too large to finish inline. Measured before the cut, the migration
//! turned out to be one operation with two drivers already - the same admission (`materialize_source`), the same
//! persisted cursor, the same compaction and publication steps - so the duplication is not in the copy. It is at the
//! **declaration**: nine sites write `with_inline_property_byte_width` directly while declaration has its own entry
//! with 132 callers.
//!
//! This view is that entry's owner, so "a bucket's width changed" has one place that performs it and, eventually, one
//! place that justifies the sites which write a width as part of a descriptor rewrite (ADR 0101's value-root split is
//! the real case).

use crate::VertexId;
use crate::labeled::bucket_label_key::BucketLabelKey;
use crate::labeled::graph::error::LabeledOperationError;
use crate::traits::{CsrEdge, CsrEdgeTombstone};
use ic_stable_structures::Memory;

use super::LabeledLaraGraph;

/// See the module documentation.
pub(crate) struct BucketValueSchema<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
}

impl<'a, E, M> BucketValueSchema<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) const fn new(graph: &'a LabeledLaraGraph<E, M>) -> Self {
        Self { graph }
    }

    /// Declares `(src, label)`'s value width, giving the bucket the span that width needs.
    ///
    /// A bucket that does not exist yet is created first, because a width without a bucket has nowhere to live. The
    /// descriptor is published once, at the end, so a failure leaves the previous schema in place.
    pub(crate) fn declare(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        inline_property_byte_width: u16,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.ensure_vertex(src)?;
        let vertex = self.graph.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let (bucket_slot, bucket) = self.graph.find_or_create_bucket(src, &vertex, label_id)?;
        let bucket = self
            .graph
            .ensure_bucket_inline_property_byte_width_on_slot(
                src,
                bucket_slot,
                bucket,
                inline_property_byte_width,
            )?;
        self.graph.publish_bucket_descriptor(bucket_slot, bucket)?;
        Ok(())
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The inline value schema of one graph (ADR 0100: the schema family's only door).
    pub(crate) fn value_schema(&self) -> BucketValueSchema<'_, E, M> {
        BucketValueSchema::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMemory;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::test_support::{InlinePropertyTestEdge, inline_property_test_graph};
    use crate::labeled::record::LabeledVertex;

    type Graph = LabeledLaraGraph<InlinePropertyTestEdge, VectorMemory>;

    fn graph() -> Graph {
        inline_property_test_graph()
    }

    fn label() -> BucketLabelKey {
        BucketLabelKey::directed_from_index(2)
    }

    fn bucket(graph: &Graph, vid: VertexId) -> crate::labeled::record::LabelBucket {
        let vertex = graph.vertices().get(vid);
        match graph.find_bucket(vid, &vertex, label()).expect("find") {
            super::super::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing"),
        }
    }

    /// Declaring a width before the rows exist is the flow the insert path expects: the schema arrives first, then
    /// each insert writes a value row of that width, and the span the declaration made backs them.
    #[test]
    fn declare_backs_the_rows_the_schema_promises() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        graph
            .value_schema()
            .declare(vid, label(), 4)
            .expect("declare");
        let declared = bucket(&graph, vid);
        assert_eq!(declared.inline_property_byte_width(), 4);
        for target in 100..104u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &target.to_le_bytes()),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let bucket = bucket(&graph, vid);
        assert_eq!(bucket.inline_property_byte_width(), 4, "the schema held");
        // The row's value is addressable whatever backs it - the declaration's span while it fits, a run beyond it.
        let mut rows = Vec::new();
        let _ = graph
            .visit_edges_with_inline_property(
                vid,
                label(),
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |_, item| {
                    rows.push(u32::from_le_bytes(
                        item.inline_property.bytes().try_into().expect("width 4"),
                    ));
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .expect("read the rows");
        assert_eq!(
            rows,
            (100..104u32).collect::<Vec<_>>(),
            "each row reads back the value its insert carried"
        );
    }

    /// Declaring the same width twice is idempotent: the schema is a statement of what the bucket holds, not a
    /// counter of how often it was said.
    #[test]
    fn declaring_twice_leaves_the_same_schema() {
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        graph
            .value_schema()
            .declare(vid, label(), 4)
            .expect("first");
        let once = bucket(&graph, vid);
        graph
            .value_schema()
            .declare(vid, label(), 4)
            .expect("second");
        let twice = bucket(&graph, vid);
        assert_eq!(
            (
                twice.inline_property_byte_width(),
                twice.inline_property_bytes_slab_slots()
            ),
            (
                once.inline_property_byte_width(),
                once.inline_property_bytes_slab_slots()
            )
        );
    }

    /// A width on a bucket whose rows already carry a different one is a migration, not a declaration: the
    /// declaration fails closed rather than rewriting the schema under live rows.
    #[test]
    fn declaring_over_live_rows_fails_closed() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 100..102u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        assert!(
            graph.value_schema().declare(vid, label(), 4).is_err(),
            "a width over width-0 rows is a migration, and this entry does not pretend to be one"
        );
    }
}
