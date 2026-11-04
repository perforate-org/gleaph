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
        self.bucket = if self.graph.values.spill_used_len(run) == 0 {
            sized.without_values_spill_run()
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
        if self.bucket.inline_property_bytes_slab_slots_allocated() > 0
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
        if bucket.edge_spill_run() != crate::traits::NO_SPILL_RUN {
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
