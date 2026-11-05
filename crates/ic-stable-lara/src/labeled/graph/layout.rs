//! A bucket's physical span, borrowed for one operation.
//!
//! ADR 0100 unit two. A bucket's span has two rules that every caller used to restate: it is admitted
//! before a row is written, and it is released exactly once when the bucket stops holding rows. The view
//! owns both, so an operation says "release this bucket's span" rather than computing what is releasable,
//! in which order, and what the vertex cover becomes afterwards.
//!
//! The layers below stay where they were: `LeafArena` places inside a leaf, `FreeSpanStore` keeps the
//! ledger, and the store owns the slot arithmetic. The view composes them; it does not reimplement them.

use crate::VertexId;
use crate::labeled::graph::LabeledOperationError;
use crate::labeled::record::LabelBucket;
use crate::lara::operation_error::LaraOperationError;
use crate::traits::{CsrEdge, CsrEdgeTombstone};
use ic_stable_structures::Memory;

use super::LabeledLaraGraph;

/// See the module documentation.
pub(crate) struct BucketLayout<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
    bucket_slot: u64,
    bucket: LabelBucket,
}

impl<'a, E, M> BucketLayout<'a, E, M>
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

    /// Makes room for `len` more slots in the bucket's span, growing the extent or moving the leaf.
    ///
    /// Placement itself stays with [`super::leaf_arena::LeafArena`]; what this owns is the answer to
    /// "where does the room come from", so a caller does not have to know about the leaf at all.
    pub(crate) fn ensure_room_for(
        &self,
        src: VertexId,
        len: u64,
        avoid: (u64, u64),
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        super::leaf_arena::LeafArena::ensure_room(
            self.graph,
            src,
            len,
            super::leaf_arena::SourceRegion::new(avoid.0, avoid.1),
        )
    }

    pub(crate) fn release_span(self, src: VertexId) -> Result<Self, LabeledOperationError> {
        debug_assert!(
            self.bucket.degree == 0 && self.bucket.edge_run().is_none(),
            "F1 release requires an emptied run-free bucket"
        );
        if self.bucket.is_tiny_mode() {
            return Ok(self);
        }
        if self.bucket.is_tree_mode() {
            // Tree root regions are LTB-addressed, not slab spans; releasing
            // them as edge slots would corrupt the free store.
            return Ok(self);
        }
        // The value *span* is this function's to release; the values *run* is not. The degree-zero
        // block in `commit_bucket_edge_delete` owns the runs because releasing one is meaningless
        // without clearing the descriptor that names it - and releasing it here as well handed the
        // same run to the free list twice, which is how a descriptor ended up naming an empty run
        // (`MissingAscLogIndex` in the expand benchmark).
        if self.bucket.inline_property_bytes_slab_slots_allocated() > 0 {
            let _ = self
                .graph
                .release_bucket_inline_property_bytes_span(src, &self.bucket);
        }
        let span_len = self.bucket.stored_slots_raw();
        if span_len == 0 {
            return Ok(self);
        }
        // GAP-2026-09-20-002: inside a delete-level batch the emptied range is
        // recorded and released merged at the flush; otherwise it is released
        // immediately (F1's single-delete behaviour). Either way the release is
        // best-effort, so a failed release never fails a successful delete.
        let batched = match self.graph.span_release_batch.borrow_mut().as_mut() {
            Some(ranges) => {
                ranges.push((self.bucket.edge_start(), u64::from(span_len)));
                true
            }
            None => false,
        };
        if !batched {
            let _ = self
                .graph
                .edges
                .release_span(self.bucket.edge_start(), u64::from(span_len));
        }
        // Recompute the cover over survivors (the emptied span contributes 0),
        // skipping the emptied bucket by INDEX (two buckets can share an
        // anchor; address matching would skip a live survivor).
        let vertex = self.graph.vertices.get(src);
        let bucket_index =
            LabeledLaraGraph::<E, M>::labeled_bucket_descriptor_index(&vertex, self.bucket_slot)?;
        let buckets = self.graph.read_vertex_label_buckets(&vertex)?;
        let mut cover_base: Option<u64> = None;
        let mut survivor_end: Option<u64> = None;
        for (index, other) in buckets.iter().enumerate() {
            if other.is_tiny_mode() {
                continue;
            }
            if u32::try_from(index).map_err(|_| LaraOperationError::overflow())? == bucket_index {
                continue;
            }
            if cover_base.is_none() {
                cover_base = Some(other.edge_start());
            }
            let end = match super::compact::bucket_content_region(other) {
                Some((start, len)) => start
                    .checked_add(u64::from(len))
                    .ok_or(LaraOperationError::overflow())?,
                None => continue,
            };
            survivor_end = Some(survivor_end.map_or(end, |prev| prev.max(end)));
        }
        if let (Some(base), Some(end)) = (cover_base, survivor_end) {
            let new_stored = end
                .checked_sub(base)
                .ok_or(LaraOperationError::overflow())?;
            let new_stored =
                u32::try_from(new_stored).map_err(|_| LaraOperationError::overflow())?;
            self.graph
                .vertices
                .set(src, &vertex.with_stored_slots(new_stored));
        } else {
            // No slab survivors: vertex span returns to zero.
            self.graph.vertices.set(src, &vertex.with_stored_slots(0));
        }
        Ok(self)
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The layout of one bucket, borrowed for one operation.
    ///
    /// ADR 0100 rule 2: this constructor is the only way in, so a caller cannot take half of the span
    /// invariant - admit without releasing, or release twice.
    pub(crate) fn layout(&self, bucket_slot: u64, bucket: LabelBucket) -> BucketLayout<'_, E, M> {
        BucketLayout::new(self, bucket_slot, bucket)
    }
}
