//! A bucket's value plane, borrowed for one operation.
//!
//! A values run and the descriptor field that names it have to move together. Releasing the run without
//! clearing the field leaves a row set whose blocks can never be read (`MissingAscLogIndex`), and two
//! call sites releasing the same run hand it to the free list twice. Both failures happened here, so the
//! pair has an owner instead of a convention: a caller asks the view, and the view cannot do half of it.
//!
//! The view holds no state of its own - the descriptor is a `LabelBucket` value and the store belongs to
//! the graph - so it is a borrowing facade over the plane, not a second source of truth for it.

use super::LabeledLaraGraph;
use crate::VertexId;
use crate::labeled::graph::LabeledOperationError;
use crate::labeled::record::LabelBucket;
use crate::traits::{CsrEdge, CsrEdgeTombstone, CsrVertex};
use ic_stable_structures::Memory;

/// See the module documentation.
pub(crate) struct BucketValuePlane<'a, E: CsrEdge, M: Memory> {
    graph: &'a LabeledLaraGraph<E, M>,
    bucket_slot: u64,
    bucket: LabelBucket,
}

impl<'a, E, M> BucketValuePlane<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) const fn new(
        graph: &'a LabeledLaraGraph<E, M>,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Self {
        Self {
            graph,
            bucket_slot,
            bucket,
        }
    }

    /// The descriptor as it stands.
    pub(crate) const fn bucket(&self) -> LabelBucket {
        self.bucket
    }

    /// Grows the values run until it holds as many rows as the edge run, filling the new blocks with
    /// `fill_byte`.
    ///
    /// The mirror is what makes a run row addressable at all, so this is the value plane's own
    /// operation: no caller should be appending blocks to keep step with the edge plane by hand.
    /// A bucket with no edge run has no rows beyond its prefix and needs nothing from the run, and a
    /// run that never needed a row is never published - the id only changes when a block is appended.
    pub(crate) fn fill_to_edge_run_len(
        mut self,
        fill_byte: u8,
    ) -> Result<Self, LabeledOperationError> {
        let Ok(edge_run) = self.graph.edges.spill_mint(self.bucket.edge_spill_run()) else {
            return Ok(self);
        };
        let target = self.graph.edges.spill_used_len(edge_run);
        let width = self.bucket.inline_property_byte_width();
        if width == 0 {
            return Ok(self);
        }
        let mut run = self
            .bucket
            .values_run()
            .and_then(|run| self.graph.values.spill_mint(run.get()).ok());
        let fill = vec![fill_byte; usize::from(width)];
        loop {
            let used = run.map_or(0, |run| self.graph.values.spill_used_len(run));
            if used >= target {
                break;
            }
            let (next, _row) = self
                .graph
                .values
                .spill_append(run, width, &fill)
                .map_err(LabeledOperationError::from)?;
            run = Some(next);
        }
        if let Some(run) = run
            && self.bucket.values_run().map(|current| current.get()) != Some(run.get())
        {
            self.bucket = self
                .bucket
                .with_values_run(Some(run))
                .map_err(LabeledOperationError::from)?;
        }
        Ok(self)
    }

    /// Writes the value of a row that was **just appended**, in whichever plane it landed.
    ///
    /// The insert path's counterpart of [`Self::write_block_at`]: a row inside the prefix gets a slab
    /// block, a row beyond it appends one to the run. The mirror means a run row always gets a block,
    /// which the schema admission guarantees by rejecting a width-mismatched edge before any write.
    pub(crate) fn write_after_insert(
        mut self,
        src: VertexId,
        slot_index: u32,
        edge: &E,
    ) -> Result<Self, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.bucket = self.graph.write_edge_inline_property_after_insert(
            src,
            self.bucket_slot,
            self.bucket,
            slot_index,
            edge,
        )?;
        Ok(self)
    }

    /// Moves the run's first `fold` blocks into the span at `[first_slot, first_slot + fold)` and
    /// drops them from the run.
    ///
    /// This is the mirror in the other direction: rows that leave the edge run for the prefix take
    /// their blocks with them. The prefix grows by `fold` here because that growth *is* the operation -
    /// a caller that grew it separately could publish a prefix whose blocks were never written.
    pub(crate) fn fold_head_into_span(
        mut self,
        src: VertexId,
        first_slot: u32,
        fold: u32,
    ) -> Result<Self, LabeledOperationError> {
        let width = self.bucket.inline_property_byte_width();
        let grown = self
            .bucket
            .with_stored_slots(self.bucket.stored_slots().saturating_add(fold));
        let sized =
            self.graph
                .ensure_bucket_inline_property_bytes_span(src, self.bucket_slot, grown)?;
        let run = self
            .graph
            .values
            .spill_mint(self.bucket.values_spill_run())
            .map_err(LabeledOperationError::from)?;
        let mut values = vec![0u8; fold as usize * usize::from(width)];
        self.graph
            .values
            .spill_read_range(run, 0..fold, width, &mut values)
            .map_err(LabeledOperationError::from)?;
        let offset = crate::labeled::invariants::inline_property_bytes_byte_offset_at_slot(
            &sized, first_slot,
        )?;
        self.graph
            .values
            .write_bytes(offset, &values)
            .map_err(LabeledOperationError::from)?;
        self.graph
            .values
            .spill_drop_head(run, fold)
            .map_err(LabeledOperationError::from)?;
        // An emptied run is **released**, not just unnamed: clearing the field alone returns the class to
        // nobody, so the arena grows forever while every descriptor says it spilled nothing (the fold-cost
        // bench caught exactly that - `GAP-LARA-FOLD-BENCH-RUN-PROBE`). On a failed release the field stays,
        // which is the pairing `release_run` already states for the rest of this plane.
        self.bucket = if self.graph.values.spill_used_len(run) == 0 {
            match self.graph.values.spill_release(run) {
                Ok(()) => sized.without_values_spill_run(),
                Err(_) => sized,
            }
        } else {
            sized
        };
        Ok(self)
    }

    /// Releases the values run and clears the field that names it.
    ///
    /// Best-effort like every other release on the delete path: a release that fails leaks a class
    /// rather than failing a successful delete, and the descriptor is left naming the run it could not
    /// give back. The field and the store therefore never disagree.
    pub(crate) fn release_run(mut self) -> Self {
        if let Some(run) = self.bucket.values_run()
            && let Ok(run) = self.graph.values.spill_mint(run.get())
            && self.graph.values.spill_release(run).is_ok()
        {
            self.bucket = self.bucket.without_values_spill_run();
        }
        self
    }

    /// Releases the value span and takes it off the owning vertex's charge.
    ///
    /// Best-effort for the same reason as [`Self::release_run`].
    pub(crate) fn release_span(mut self, src: VertexId) -> Self {
        // Asks the derivation, not the slab-slot field: a tree holds a value root span with zero slab slots.
        if super::super::invariants::bucket_resident_inline_property_bytes(&self.bucket) > 0
            && self
                .graph
                .release_bucket_inline_property_bytes_span(src, &self.bucket)
                .is_ok()
        {
            self.bucket = self
                .bucket
                .with_inline_property_bytes_offset(0)
                .with_inline_property_bytes_slab_slots(0);
        }
        self
    }

    /// Appends one block to the values run, minting or growing it, and publishes the run id.
    pub(crate) fn append_block(mut self, bytes: &[u8]) -> Result<Self, LabeledOperationError> {
        self.bucket = self
            .graph
            .write_inline_property_bytes_to_log(&self.bucket, bytes)?;
        Ok(self)
    }

    /// Publishes the descriptor.
    pub(crate) fn publish(self) -> Result<(), LabeledOperationError> {
        self.graph
            .buckets
            .write_label_bucket_slot(self.bucket_slot, self.bucket)
            .map_err(LabeledOperationError::from)
    }
}

impl<'a, E, M> BucketValuePlane<'a, E, M>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    /// Writes an existing row's block, in whichever plane the row lives.
    pub(crate) fn write_block_at(
        mut self,
        src: VertexId,
        slot_index: u32,
        edge: &E,
    ) -> Result<Self, LabeledOperationError> {
        self.bucket = self.graph.write_edge_inline_property_at_existing_row(
            src,
            self.bucket_slot,
            self.bucket,
            slot_index,
            edge,
        )?;
        Ok(self)
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Publishes one bucket descriptor after checking that the runs it names are real.
    ///
    /// The store's `write_label_bucket_slot` is the byte-level publish; this is the *contract-level*
    /// one. A descriptor that names a values or edge run which the arena does not hold - because a
    /// later operation released it while a caller kept an older bucket value - cannot be published
    /// through here. That failure mode reads as `MissingAscLogIndex` much later, so catching it at the
    /// publish is the difference between a precise trap and a mystery.
    pub(crate) fn publish_bucket_descriptor(
        &self,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<(), LabeledOperationError> {
        // A run the bucket names must exist. An *owned but empty* run is legitimate (a freshly
        // allocated class, or a bucket whose rows all sit in its prefix), so emptiness only breaks the
        // descriptor when the bucket has rows the run is supposed to hold.
        let rows_beyond_prefix = bucket.degree() > bucket.inline_property_bytes_slab_slots();
        if let Some(run) = bucket.values_run() {
            let run = self
                .values
                .spill_mint(run.get())
                .map_err(LabeledOperationError::from)?;
            assert!(
                !rows_beyond_prefix || self.values.spill_used_len(run) > 0,
                "publishing label {:?} at slot {bucket_slot} with rows beyond its prefix but values run \
                 {} has none",
                bucket.bucket_label_key(),
                bucket.values_spill_run()
            );
        }
        if bucket.edge_run().is_some() {
            let run = self
                .edges
                .spill_mint(bucket.edge_spill_run())
                .map_err(LabeledOperationError::from)?;
            assert!(
                bucket.degree() <= bucket.stored_slots() || self.edges.spill_used_len(run) > 0,
                "publishing label {:?} at slot {bucket_slot} with rows beyond its prefix but edge run \
                 {} has none",
                bucket.bucket_label_key(),
                bucket.edge_spill_run()
            );
        }
        // The byte-level publish this method exists to guard.
        self.buckets
            .write_label_bucket_slot(bucket_slot, bucket)
            .map_err(LabeledOperationError::from)
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

    /// A slab bucket carrying values at width 2, with `rows` edges in it.
    fn bucket_with_values(graph: &Graph, rows: u32, width: u16) -> (VertexId, u64, LabelBucket) {
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label(), width)
            .expect("declare the width");
        for target in 7..7 + rows {
            let bytes = u16::try_from(target).expect("fits").to_le_bytes();
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &bytes),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        match graph.find_bucket(vid, &vertex, label()).expect("find") {
            super::super::BucketSearch::Found { slot, bucket } => (vid, slot, bucket),
            _ => panic!("bucket missing"),
        }
    }

    /// The plane answers the descriptor it was opened on: a caller publishes what an operation produced, and a
    /// plane that has produced nothing is a no-op rather than a silent write.
    #[test]
    fn the_plane_answers_the_descriptor_it_was_opened_on() {
        let graph = graph();
        let (_vid, slot, bucket) = bucket_with_values(&graph, 8, 2);
        let plane = graph.value_plane(slot, bucket);
        assert_eq!(plane.bucket(), bucket);
        // Publishing an untouched plane writes back exactly the descriptor it holds.
        graph.value_plane(slot, bucket).publish().expect("publish");
        let after = graph.buckets().read_label_bucket_slot(slot).expect("read");
        assert_eq!(after, bucket, "a no-op publish changes nothing");
    }

    /// `release_run` clears the descriptor field that named the run, so the plane cannot leave a bucket
    /// pointing at a run it no longer owns.
    #[test]
    fn release_run_clears_the_descriptor_field_that_named_it() {
        let graph = graph();
        let (_vid, slot, bucket) = bucket_with_values(&graph, 8, 2);
        assert!(
            bucket.values_run().is_some(),
            "the fixture's values live in a run"
        );
        // Releasing is best-effort by contract: it answers the descriptor it produced.
        let released = graph.value_plane(slot, bucket).release_run();
        assert_eq!(
            released.bucket().values_run(),
            None,
            "a released run leaves no run id behind"
        );
    }

    /// A bucket with no values has nothing to fill: the mirror is a no-op rather than an error, because "no run
    /// to keep step with" is the normal state of a bucket without inline property bytes.
    #[test]
    fn filling_an_edge_run_is_a_no_op_without_values() {
        let graph = graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        for target in 7..12u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        let vertex = graph.vertices().get(vid);
        let (slot, bucket) = match graph.find_bucket(vid, &vertex, label()).expect("find") {
            super::super::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("bucket missing"),
        };
        assert_eq!(bucket.inline_property_byte_width(), 0);
        let filled = graph
            .value_plane(slot, bucket)
            .fill_to_edge_run_len(0)
            .expect("a width-0 bucket needs no mirror");
        assert_eq!(filled.bucket(), bucket, "and nothing is changed");
    }
}
