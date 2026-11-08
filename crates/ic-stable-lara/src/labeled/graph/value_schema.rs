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
use super::error::ValueMigration;
use super::values::{MaterializeCursor, MaterializeStep};

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

impl<'a, E, M> BucketValueSchema<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Migrates a bucket's rows to `width`, or hands back the work when the stream is too large to do inline.
    ///
    /// The decision belongs here because it is about the whole stream (every live row gets a block, wherever it
    /// lives), while the byte budget is about the reservation. `Deferred` is the same [`ValueMigration`] the error
    /// carries, so a caller that only propagates it and a caller that queues it describe the same job.
    pub(crate) fn migrate(
        &self,
        src: VertexId,
        bucket_slot: u64,
        width: u16,
        fill_byte: u8,
    ) -> Result<Migration, LabeledOperationError> {
        let cursor = self
            .graph
            .materialize_cursor(src, bucket_slot, width, fill_byte)?;
        let stream_bytes = u64::from(cursor.source.degree) * u64::from(width);
        if stream_bytes > <LabeledLaraGraph<E, M>>::INLINE_MATERIALIZE_BYTE_BUDGET {
            return Ok(Migration::Deferred(ValueMigration {
                vid: src,
                bucket_slot,
                width,
                fill_byte,
                degree: cursor.source.degree,
            }));
        }
        self.graph.complete_migration_inline(&cursor)?;
        Ok(Migration::Done)
    }

    /// Drives the deferred migration the queue owns, one step at a time.
    ///
    /// `Err` retains the cursor (the caller keeps the work item); `Cancelled` consumes it, because the source it
    /// named is gone.
    pub(crate) fn migrate_step(
        &self,
        cursor: &mut MaterializeCursor,
    ) -> Result<MaterializeStep, LabeledOperationError> {
        self.graph.materialize_inline_property_stream_step(cursor)
    }
}

/// What a migration did: finished inline, or the work a caller must drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Migration {
    /// The rows are migrated and the new width is published.
    Done,
    /// The stream exceeds the inline byte budget; drive it through the queue.
    Deferred(ValueMigration),
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
mod migration_tests {
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

    fn bucket_slot(graph: &Graph, vid: VertexId) -> (u64, crate::labeled::record::LabelBucket) {
        let vertex = graph.vertices().get(vid);
        match graph.find_bucket(vid, &vertex, label()).expect("find") {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("bucket missing"),
        }
    }

    /// A bucket with a few width-0 rows migrates to a wide schema inline, and the descriptor it publishes names the
    /// width and the span.
    #[test]
    fn a_small_stream_migrates_inline() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 100..103u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let (slot, before) = bucket_slot(&graph, vid);
        assert_eq!(before.inline_property_byte_width(), 0);
        let outcome = graph
            .value_schema()
            .migrate(vid, slot, 4, 0xA5)
            .expect("migrate");
        assert_eq!(outcome, Migration::Done, "3 rows x 4 bytes fits inline");
        let (_, after) = bucket_slot(&graph, vid);
        assert_eq!(after.inline_property_byte_width(), 4);
        assert_eq!(after.inline_property_bytes_slab_slots(), 3);
    }

    /// A stream over the inline budget is not attempted: the view answers with the work, and nothing is published.
    #[test]
    fn an_oversized_stream_hands_back_the_work() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 100..103u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let (slot, _) = bucket_slot(&graph, vid);
        let width = 2048;
        let outcome = graph
            .value_schema()
            .migrate(vid, slot, width, 0xA5)
            .expect("migrate");
        let Migration::Deferred(work) = outcome else {
            panic!("3 rows x 2048 bytes must defer");
        };
        assert_eq!(work.vid, vid);
        assert_eq!(work.bucket_slot, slot);
        assert_eq!(work.width, width);
        assert_eq!(work.degree, 3);
        assert_eq!(work.total_bytes(), 6_144);
        let (_, after) = bucket_slot(&graph, vid);
        assert_eq!(
            after.inline_property_byte_width(),
            0,
            "a deferred migration publishes nothing"
        );
    }

    /// The deferred work is drivable: the queue's step form finishes it and publishes the width.
    #[test]
    fn a_deferred_migration_finishes_through_the_step_driver() {
        use crate::labeled::graph::EdgePlacementPolicy;
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 100..103u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let (slot, _) = bucket_slot(&graph, vid);
        let Migration::Deferred(work) = graph
            .value_schema()
            .migrate(vid, slot, 2048, 0xA5)
            .expect("migrate")
        else {
            panic!("must defer");
        };
        let mut cursor = graph
            .materialize_cursor(work.vid, work.bucket_slot, work.width, work.fill_byte)
            .expect("admit cursor");
        let mut steps = 0;
        loop {
            match graph
                .value_schema()
                .migrate_step(&mut cursor)
                .expect("step")
            {
                MaterializeStep::Finished => break,
                MaterializeStep::Resumed => steps += 1,
                MaterializeStep::Cancelled(reason) => panic!("cancelled: {reason}"),
            }
        }
        assert!(steps > 0, "a deferred stream takes more than one step");
        let (_, after) = bucket_slot(&graph, vid);
        assert_eq!(after.inline_property_byte_width(), 2048);
    }
}

#[cfg(test)]
mod migration_signal_tests {
    use crate::VertexId;
    use crate::labeled::graph::{LabeledOperationError, ValueMigration};

    /// The deferred work is one value: the error carries it, and its byte count is the one the stepped fill uses.
    #[test]
    fn the_deferred_signal_is_the_work_itself() {
        let queued = ValueMigration {
            vid: VertexId::from(7),
            bucket_slot: 42,
            width: 4,
            fill_byte: 0xA5,
            degree: 600,
        };
        assert_eq!(queued.total_bytes(), 2_400);
        let error = LabeledOperationError::InlinePropertyMaterializeDeferredRequired(queued);
        match error {
            LabeledOperationError::InlinePropertyMaterializeDeferredRequired(carried) => {
                assert_eq!(
                    carried, queued,
                    "the error carries the work, not a copy of it"
                )
            }
            other => panic!("wrong variant: {other}"),
        }
    }
}

#[cfg(test)]
mod construction_tests {
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::record::{LabelBucket, LabelBucketFieldError};

    /// A width wider than a value block's payload is refused where the descriptor is built, not where a reader
    /// happens to discover it: no code path can hold a bucket whose schema contradicts its storage.
    #[test]
    fn a_width_wider_than_a_value_block_is_refused_at_construction() {
        let too_wide =
            u16::try_from(crate::labeled::tree_csr::PROPERTY_BLOCK_BYTES + 1).expect("fits u16");
        let refused = LabelBucket::try_from_parts(
            BucketLabelKey::directed_from_index(2),
            0,
            1,
            1,
            crate::labeled::record::InlinePropertyParts {
                byte_width: too_wide,
                offset: 0,
                slab_slots: 0,
                log_head: -1,
                log_len: 0,
            },
        );
        assert!(
            matches!(
                refused,
                Err(LabelBucketFieldError::InlinePropertyBytesWidthOutOfRange)
            ),
            "a schema that cannot be stored must not be constructible"
        );
        // The widest storable width is accepted, so the boundary is a real one rather than a blanket refusal.
        assert!(
            LabelBucket::try_from_parts(
                BucketLabelKey::directed_from_index(2),
                0,
                1,
                1,
                crate::labeled::record::InlinePropertyParts {
                    byte_width: u16::try_from(crate::labeled::tree_csr::PROPERTY_BLOCK_BYTES)
                        .expect("fits"),
                    offset: 0,
                    slab_slots: 0,
                    log_head: -1,
                    log_len: 0,
                }
            )
            .is_ok()
        );
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
