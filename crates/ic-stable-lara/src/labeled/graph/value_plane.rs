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
use crate::traits::{CsrEdge, CsrEdgeTombstone};
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

    /// Releases the values run and clears the field that names it.
    ///
    /// Best-effort like every other release on the delete path: a release that fails leaks a class
    /// rather than failing a successful delete, and the descriptor is left naming the run it could not
    /// give back. The field and the store therefore never disagree.
    pub(crate) fn release_run(mut self) -> Self {
        if let Ok(run) = self.graph.values.spill_mint(self.bucket.values_spill_run())
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
