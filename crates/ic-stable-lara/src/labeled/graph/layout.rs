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

use super::compact::{bucket_content_region, bucket_content_slots};
use super::{EdgeSlotMove, LabeledLaraGraph};

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
        self.graph.vertex_layout(src).ensure_room_for(len, avoid)
    }

    /// Promotes the bucket to tree mode and returns the view over the descriptor it became.
    ///
    /// The refresh is part of the operation: promotion rewrites the descriptor's residency, so a caller
    /// holding the old `LabelBucket` would publish a slab descriptor over a tree root. Callers that used to
    /// re-find the bucket by label after promoting get the new value from here instead.
    pub(crate) fn promote_to_tree(self, src: VertexId) -> Result<Self, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let label = self.bucket.bucket_label_key();
        super::promote::promote_bucket_to_tree_mode(self.graph, src, label)?;
        let vertex = self.graph.vertices.get(src);
        match self.graph.find_bucket(src, &vertex, label)? {
            super::BucketSearch::Found { bucket, .. } => Ok(Self { bucket, ..self }),
            super::BucketSearch::Missing { .. } => {
                Err(LabeledOperationError::BucketNotFound { vid: src, label })
            }
        }
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
            let end = match bucket_content_region(other) {
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
    /// Repacks the bucket's spill run, dropping tombstoned rows, and reports the rows that moved.
    pub(crate) fn rebuild_run(&self) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let Ok(run) = self.graph.edges.spill_mint(self.bucket.edge_spill_run()) else {
            return Ok(Vec::new());
        };
        let used = self.graph.edges.spill_used_len(run);
        let mut keep = Vec::with_capacity(used as usize);
        for index in 0..used {
            let edge = self
                .graph
                .edges
                .spill_edge_at(run, index)
                .map_err(LabeledOperationError::from)?;
            if !edge.is_tombstone_edge() {
                keep.push(index);
            }
        }
        if keep.len() as u32 == used {
            return Ok(Vec::new());
        }
        let prefix = bucket_content_slots(&self.bucket);
        if keep.is_empty() {
            // Nothing live: drop the run entirely and let the descriptor say so.
            self.graph.publish_bucket_descriptor(
                self.bucket_slot,
                self.bucket.without_edge_spill_run(),
            )?;
            self.graph
                .edges
                .spill_release(run)
                .map_err(LabeledOperationError::from)?;
            return Ok(Vec::new());
        }
        // ADR 0099: the values plane mirrors the edge plane, so the same `keep` repacks the value
        // run at the same indices. Both new ids are published in one descriptor write, and only
        // then are the old runs released - the descriptor never names a freed run, and never names
        // one plane's new run beside the other plane's old one.
        let fresh = self
            .graph
            .edges
            .spill_repack(run, &keep)
            .map_err(LabeledOperationError::from)?;
        let fresh_values = match self.bucket.values_run() {
            None => None,
            Some(old) => Some((
                old,
                self.graph
                    .values
                    .spill_repack(old, &keep)
                    .map_err(LabeledOperationError::from)?,
            )),
        };
        let published = match &fresh_values {
            Some((_, new_values)) => self
                .bucket
                .with_edge_run(Some(fresh))
                .map_err(LabeledOperationError::from)?
                .with_values_run(Some(*new_values))
                .map_err(LabeledOperationError::from)?,
            None => self
                .bucket
                .with_edge_run(Some(fresh))
                .map_err(LabeledOperationError::from)?,
        };
        self.graph
            .publish_bucket_descriptor(self.bucket_slot, published)?;
        self.graph
            .edges
            .spill_release(run)
            .map_err(LabeledOperationError::from)?;
        if let Some((old_values, _)) = fresh_values {
            self.graph
                .values
                .spill_release(old_values)
                .map_err(LabeledOperationError::from)?;
        }
        let mut moves = Vec::new();
        for (destination, source) in keep.iter().copied().enumerate() {
            if destination as u32 != source {
                moves.push(EdgeSlotMove {
                    label_id: self.bucket.bucket_label_key(),
                    old_slot_index: prefix + source,
                    new_slot_index: prefix + destination as u32,
                });
            }
        }
        Ok(moves)
    }

    /// Moves the head of a bucket's edge spill run into its slab span.
    ///
    /// A spilled row is one the leaf could not give the span a slot for, and it stays spilled
    /// because the edge fold was deleted with the shared overflow log (ADR 0097). When the span
    /// has room again - its region's own gap after a left-pack - the lowest spill rows belong back
    /// in the span, where a read finds them without touching the run.
    ///
    /// No logical ordinal moves: logical row `i` is the span slot for `i < stored_slots` and spill
    /// row `i - stored_slots` otherwise, so growing `stored_slots` by `k` re-homes exactly the `k`
    /// rows removed from the front of the run. That is why this needs no move observer and no
    /// rekey, unlike a rebuild.
    ///
    /// Order matters: write the span, drop the run's head, then publish `stored_slots`. A failure
    /// before the publish leaves the old descriptor naming the spill, so the rows stay readable and
    /// the written span bytes are simply not visible yet. `max_rows` is the caller's capacity
    /// bound; the drop is also capped at the run's class floor, because a class change is the
    /// rebuild's job.
    /// Moves up to `max_rows` run rows into the bucket's span, in order, without moving ordinals.
    pub(crate) fn fold_head(
        &self,
        src: VertexId,
        free: super::span_size::SpanSlots,
    ) -> Result<u32, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let run = match self.graph.edges.spill_mint(self.bucket.edge_spill_run()) {
            Ok(run) => run,
            Err(_) => return Ok(0),
        };
        let Some((start, len)) = bucket_content_region(&self.bucket) else {
            return Ok(0);
        };
        // How many rows may move is arithmetic with two bounds (rows waiting in the run, room in the span
        // tail) plus the run's own head-drop capacity; the first two come from the shared rule.
        let prefix = super::span_size::ContentSlots::new(self.bucket.stored_slots());
        let run_rows = self.graph.edges.spill_used_len(run);
        let resident = super::span_size::ContentSlots::new(
            self.bucket.stored_slots().saturating_add(run_rows),
        );
        let fold = u32::try_from(super::span_size::foldable_rows(prefix, resident, free).get())
            .unwrap_or(u32::MAX)
            .min(self.graph.edges.spill_head_drop_capacity(run));
        if fold == 0 {
            return Ok(0);
        }
        let mut rows = vec![0u8; fold as usize * E::BYTES];
        for index in 0..fold {
            let offset = index as usize * E::BYTES;
            let mut row = vec![0u8; E::BYTES];
            self.graph
                .edges
                .spill_read_row(run, index, &mut row)
                .map_err(LabeledOperationError::from)?;
            rows[offset..offset + E::BYTES].copy_from_slice(&row);
        }
        self.graph
            .edges
            .write_slots_contiguous(start.saturating_add(u64::from(len)), &rows)
            .map_err(LabeledOperationError::from)?;
        if let Err(error) = self.graph.edges.spill_drop_head(run, fold) {
            return Err(LabeledOperationError::from(error));
        }
        let grown = len
            .checked_add(fold)
            .ok_or(LaraOperationError::overflow())?;
        let grown_bucket = self.bucket.with_stored_slots(grown);
        // ADR 0099: the values plane mirrors the edge plane, so the rows that just moved into the
        // span bring their value blocks with them. The move *is* the plane's operation (it grows the
        // prefix it fills), so it goes through the view rather than being spelled out here.
        if grown_bucket.inline_property_byte_width() != 0 && self.bucket.values_run().is_some() {
            let published = self
                .graph
                .value_plane(self.bucket_slot, self.bucket)
                .fold_head_into_span(src, len, fold)?
                .bucket();
            self.graph
                .publish_bucket_descriptor(self.bucket_slot, published)?;
            return Ok(fold);
        }
        self.graph
            .publish_bucket_descriptor(self.bucket_slot, grown_bucket)?;
        Ok(fold)
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
