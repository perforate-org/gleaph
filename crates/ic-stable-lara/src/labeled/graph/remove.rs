//! Labeled graph `remove` implementation.

use crate::{
    VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        bucket_label_key::{BucketDirectedness, BucketLabelKey},
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::{edge::OutEdgeSlabIter, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;
use std::ops::ControlFlow;

use super::error::LabeledOperationError;
use super::{
    BucketMode, BucketSearch, EdgeRemoval, EdgeSlotMove, LabeledLaraGraph, OutEdgeOrder, T_DEMOTE,
};

enum BucketEdgeDeleteLocation {
    Slab {
        physical_slot: u64,
    },
    OverflowLog {
        leaf: u32,
        entry_idx: u32,
        newer_entry_idx: Option<u32>,
        moves: Vec<EdgeSlotMove>,
    },
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn overflow_chain_slot_moves_after_delete(
        &self,
        leaf: u32,
        chain: &[u32],
        removed_ordinal: usize,
        slab_prefix_slots: u32,
        label_id: BucketLabelKey,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError> {
        let mut moves = Vec::with_capacity(chain.len().saturating_sub(removed_ordinal + 1));
        for (ordinal, entry_idx) in chain.iter().enumerate().skip(removed_ordinal + 1) {
            let (_, edge) = self.edges.read_overflow_log_entry(leaf, *entry_idx);
            if edge.is_deleted_slot() {
                continue;
            }
            let old_slot_index = slab_prefix_slots
                .checked_add(
                    u32::try_from(ordinal)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                )
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            moves.push(EdgeSlotMove {
                label_id,
                old_slot_index,
                new_slot_index: old_slot_index - 1,
            });
        }
        Ok(moves)
    }

    fn decrement_edge_counts_after_remove(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let hdr = self.edges.header();
        let next_global = hdr
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_global);
        self.edges
            .bump_vertex_segment_counts(src, -1, 0)
            .map_err(LabeledOperationError::from)
    }

    fn default_bypass_edge_log_slots(&self, src: VertexId, vertex: &LabeledVertex) -> u32 {
        let head = vertex.bypass_overflow_log_head();
        if head < 0 {
            return 0;
        }
        let leaf = self.inline_property_bytes_log_leaf(src);
        self.edges.overflow_log_chain_asc_indices(leaf, head).len() as u32
    }

    fn default_bypass_slab_prefix_slots(&self, src: VertexId, vertex: &LabeledVertex) -> u32 {
        vertex
            .stored_degree()
            .saturating_sub(self.default_bypass_edge_log_slots(src, vertex))
    }

    /// Read-only existence check for a label-row slot.  Mutation callers use this
    /// before invalidating a published mate leaf so a missing edge remains a true
    /// no-op.
    pub(crate) fn edge_exists_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<bool, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex)
                || slot_index >= vertex.stored_degree()
            {
                return Ok(false);
            }
            let prefix = self.default_bypass_slab_prefix_slots(src, &vertex);
            if slot_index < prefix {
                let physical =
                    checked_add_slot_index(vertex.base_slot_start(), u64::from(slot_index))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                let edge = self.edges.read_slot(physical);
                return Ok(!edge.is_deleted_slot() && !edge.is_tombstone_edge());
            }
            let leaf = self.inline_property_bytes_log_leaf(src);
            let ordinal = slot_index
                .checked_sub(prefix)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let chain = self
                .edges
                .overflow_log_chain_asc_indices(leaf, vertex.bypass_overflow_log_head());
            let Some(&entry_idx) = chain.get(ordinal as usize) else {
                return Ok(false);
            };
            let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
            return Ok(!edge.is_tombstone_edge());
        }
        let BucketSearch::Found { bucket, .. } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(false);
        };
        Ok(self
            .locate_bucket_edge_for_delete(src, &bucket, slot_index)?
            .is_some())
    }

    fn remove_default_bypass_edge_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: LabeledVertex,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if label_id != self.bypass_storage_label_for(&vertex) {
            return Ok(None);
        }
        if slot_index >= vertex.stored_degree() {
            return Ok(None);
        }
        let slab_prefix_slots = self.default_bypass_slab_prefix_slots(src, &vertex);
        if slot_index >= slab_prefix_slots {
            let leaf = self.inline_property_bytes_log_leaf(src);
            let log_ordinal = slot_index
                .checked_sub(slab_prefix_slots)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let chain = self
                .edges
                .overflow_log_chain_asc_indices(leaf, vertex.bypass_overflow_log_head());
            let Some(&entry_idx) = chain.get(log_ordinal as usize) else {
                return Ok(None);
            };
            let newer_entry_idx = chain.get(log_ordinal as usize + 1).copied();
            let (_, removed) = self.edges.read_overflow_log_entry(leaf, entry_idx);
            if removed.is_tombstone_edge() {
                return Ok(None);
            }
            let moves = self.overflow_chain_slot_moves_after_delete(
                leaf,
                &chain,
                log_ordinal as usize,
                slab_prefix_slots,
                label_id,
            )?;
            let new_head = self
                .edges
                .unlink_overflow_log_entry(
                    leaf,
                    vertex.bypass_overflow_log_head(),
                    entry_idx,
                    newer_entry_idx,
                )
                .unwrap_or_else(|_| panic!("preflighted edge-log unlink failed"));
            self.vertices.set(
                src,
                &vertex.with_log_head(new_head).after_slab_tombstone_delete(),
            );
            self.decrement_edge_counts_after_remove(src)?;
            return Ok(Some(EdgeRemoval {
                removed: removed
                    .with_slot_index(slot_index)
                    .with_label_id(label_id.raw()),
                moves,
            }));
        }

        let rm_slot = checked_add_slot_index(vertex.base_slot_start(), u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.edges.read_slot(rm_slot);
        if removed.is_deleted_slot() || removed.is_tombstone_edge() {
            return Ok(None);
        }
        self.edges
            .write_slot(rm_slot, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        self.vertices
            .set(src, &vertex.after_slab_tombstone_delete());
        self.decrement_edge_counts_after_remove(src)?;
        Ok(Some(EdgeRemoval {
            removed: removed
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw()),
            moves: Vec::new(),
        }))
    }

    fn remove_default_bypass_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        vertex: LabeledVertex,
        mut matches: F,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        if label_id != self.bypass_storage_label_for(&vertex) {
            return Ok(None);
        }
        if vertex.degree() == 0 {
            return Ok(None);
        }

        let slab_prefix_slots = self.default_bypass_slab_prefix_slots(src, &vertex);
        for slot_index in 0..slab_prefix_slots {
            let edge_slot = checked_add_slot_index(vertex.base_slot_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.edges.read_slot(edge_slot);
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                continue;
            }
            let edge = edge
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if matches(&edge) {
                return self.remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
            }
        }

        if vertex.bypass_overflow_log_head() < 0 {
            return Ok(None);
        }
        let leaf = self.inline_property_bytes_log_leaf(src);
        let chain = self
            .edges
            .overflow_log_chain_asc_indices(leaf, vertex.bypass_overflow_log_head());
        for (ordinal, entry_idx) in chain.into_iter().enumerate() {
            let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
            if edge.is_tombstone_edge() {
                continue;
            }
            let slot_index = slab_prefix_slots
                .checked_add(
                    u32::try_from(ordinal)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                )
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = edge
                .with_slot_index(slot_index)
                .with_label_id(label_id.raw());
            if matches(&edge) {
                return self.remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
            }
        }
        Ok(None)
    }

    /// Removes the edge stored at `slot_index` in the bucket identified by `label_id`.
    pub(crate) fn remove_edge_at_slot(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        Ok(self
            .remove_edge_at_slot_with_move(src, label_id, slot_index)?
            .map(|removal| removal.removed))
    }

    #[allow(clippy::needless_return)]
    /// Removes one edge and reports the bounded survivor slot shifts produced by overflow unlink.
    pub(crate) fn remove_edge_at_slot_with_move(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return self.remove_default_bypass_edge_at_slot(src, label_id, vertex, slot_index);
        }
        let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? else {
            return Ok(None);
        };
        // ADR 0096 §7: match-first dispatch — mode decides before any
        // storage-class read, so a future mode cannot silently inherit a path.
        match BucketMode::from_bucket(&bucket) {
            // ADR 0096 §5: tiny buckets graduate to slab before deleting
            // (promote-then-tombstone): deletes never move survivors inline in
            // any mode or policy — positional stability across deletes is
            // load-bearing (slot-keyed counterpart occurrences; moves-dropping
            // remove APIs). Tiny has no tombstone representation, so the
            // bucket takes the normal slab tombstone path. Moves are always
            // empty (nothing moved), matching slab-delete reporting.
            BucketMode::Tiny => self.remove_tiny_edge_at_slot(src, slot, &bucket, slot_index),
            // Plan 0318 §Step 6 single dispatch point: tree-mode buckets
            // take the tombstone-rewrite path; slab buckets keep the
            // existing path. No other module under `graph/` branches on
            // `bucket.is_tree_mode()`.
            BucketMode::Tree => {
                let removed = super::tree_write::tree_mode_remove_edge_at_slot(
                    self, slot, &bucket, slot_index,
                )?;
                // Plan 0319 §Step 2: after a successful tree-mode removal,
                // check the degree-hysteresis trigger. If the updated
                // `degree <= T_DEMOTE`, rebuild the bucket as a fresh
                // slab. The trigger is best-effort: a successful removal
                // must not be turned into an error, so a mid-demote
                // failure is contained (`let _ =`). The next removal
                // retries the trigger; until then the bucket stays in
                // tree mode with the same `degree`.
                if let Some(ref _removed_edge) = removed {
                    let updated = self
                        .buckets()
                        .read_label_bucket_slot(slot)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    if updated.is_tree_mode() && updated.degree <= T_DEMOTE {
                        let _ = super::tree_write::tree_mode_demote_to_slab::<E, M>(
                            self, src, slot, label_id, &updated,
                        );
                    }
                }
                Ok(removed.map(|removed| EdgeRemoval {
                    removed: removed.with_slot_index(slot_index),
                    moves: Vec::new(),
                }))
            }
            BucketMode::Slab => {
                self.remove_bucket_edge_at_slot(src, &vertex, slot, bucket, slot_index)
            }
        }
    }

    /// Removes one edge from a tiny-mode bucket by promoting first (ADR 0096
    /// §5): deletes never move survivors inline in any mode or policy —
    /// positional stability across deletes is load-bearing (sidecars, postings
    /// and occurrence ranks key on slots; movement happens only in maintenance
    /// with reported moves). Tiny has no tombstone representation, so the
    /// bucket graduates to slab and takes the normal tombstone path. Moves are
    /// always empty (nothing moved), matching slab-delete reporting.
    /// Removes one edge from a tiny-mode bucket inline (delete redesign):
    /// writes the tombstone sentinel at `slot_index`, decrements degree,
    /// publishes the descriptor. No promotion, no slab/log writes, no moves
    /// (survivors keep their slots — positional stability holds trivially).
    /// Empty (`degree` reaches 0) resets to clean-empty tiny (`stored` 0, all
    /// targets zeroed — matches birth shape, so re-inserts start dense).
    /// Tombstone slots are skipped by every scan path and excluded from
    /// promotion transcription; the prefix compacts only via re-insert
    /// hole-fill or full-empty reset.
    fn encode_tiny_tombstone_edge() -> u32
    where
        E: CsrEdgeTombstone,
    {
        let tomb = E::tombstone_edge();
        let mut bytes = [0u8; 4];
        tomb.write_to(&mut bytes[..E::BYTES.min(4)]);
        u32::from_le_bytes(bytes)
    }

    fn remove_tiny_edge_at_slot(
        &self,
        src: VertexId,
        slot: u64,
        bucket: &LabelBucket,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        debug_assert!(
            bucket.is_tiny_mode(),
            "tiny remove requires a tiny-mode bucket"
        );
        let _ = src;
        if slot_index >= bucket.stored_slots_raw() {
            return Ok(None);
        }
        // Layout-native liveness (same predicate as slab/tree read paths):
        // decode the slot and ask the edge layout. Already-dead is an
        // idempotent no-op (mirrors slab double-delete None).
        let probe = E::read_from(&bucket.tiny_target(slot_index).to_le_bytes());
        if probe.is_deleted_slot() {
            return Ok(None);
        }
        let edge = probe
            .with_slot_index(slot_index)
            .with_label_id(bucket.bucket_label_key().raw());
        let new_degree = bucket.degree().saturating_sub(1);
        let updated = if new_degree == 0 {
            // Clean-empty reset: zero width, zeroed payload (birth shape).
            let mut fresh = *bucket;
            for i in 0..LabelBucket::TINY_MAX_DEGREE {
                fresh = fresh.with_tiny_target(i, 0);
            }
            fresh.with_degree_field(0).with_stored_slots(0)
        } else {
            bucket
                .with_tiny_target(slot_index, Self::encode_tiny_tombstone_edge())
                .with_degree_field(new_degree)
        };
        self.buckets.write_label_bucket_slot(slot, updated)?;
        let hdr = self.edges.header();
        let next_num_edges = hdr
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.edges.set_num_edges(next_num_edges);
        // No leaf `actual` bump: tiny edges occupy no leaf slots (mirrors
        // tiny insert, which bumps only the global census).
        Ok(Some(EdgeRemoval {
            removed: edge,
            moves: Vec::new(),
        }))
    }

    /// Releases an emptied bucket's slab span and syncs the vertex cover (F1).
    ///
    /// Called when a delete empties a bucket (`degree == 0`, no log) that still
    /// holds a slab span (`stored_slots > 0`). Releases the span to the edge
    /// free store (best-effort, see below) and recomputes `vertex.stored_slots_raw()`
    /// over survivors so the cover stays exact (no phantom occupancy).
    /// Tiny buckets never hold a releasable span here (empty tiny resets to
    /// stored 0, so the hook guard never fires for them); tree buckets
    /// keep their root until maintenance demotion (unchanged behavior).
    /// PMA total is untouched (block-pin floor owns it; slide/relocate
    /// reconcile on next maintenance — same rule as promotion's floor-covered
    /// spans).
    fn release_bucket_edge_span_on_empty(
        &self,
        src: VertexId,
        slot: u64,
        bucket: &LabelBucket,
    ) -> Result<(), LabeledOperationError> {
        debug_assert!(
            bucket.degree() == 0 && bucket.overflow_log_head() < 0,
            "F1 release requires an emptied log-free bucket"
        );
        if bucket.is_tiny_mode() {
            return Ok(());
        }
        if bucket.is_tree_mode() {
            // Tree root regions are LTB-addressed, not slab spans; releasing
            // them as edge slots would corrupt the free store.
            return Ok(());
        }
        let span_len = bucket.stored_slots_raw();
        if span_len == 0 {
            return Ok(());
        }
        // GAP-2026-09-20-002: inside a delete-level batch the emptied range is
        // recorded and released merged at the flush; otherwise it is released
        // immediately (F1's single-delete behaviour). Either way the release is
        // best-effort, so a failed release never fails a successful delete.
        let batched = match self.span_release_batch.borrow_mut().as_mut() {
            Some(ranges) => {
                ranges.push((bucket.edge_start(), u64::from(span_len)));
                true
            }
            None => false,
        };
        if !batched {
            let _ = self
                .edges
                .release_span(bucket.edge_start(), u64::from(span_len));
        }
        // Recompute the cover over survivors (the emptied span contributes 0),
        // skipping the emptied bucket by INDEX (two buckets can share an
        // anchor; address matching would skip a live survivor).
        let vertex = self.vertices.get(src);
        let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let mut cover_base: Option<u64> = None;
        let mut survivor_end: Option<u64> = None;
        for (index, other) in buckets.iter().enumerate() {
            if other.is_tiny_mode() {
                continue;
            }
            if u32::try_from(index).map_err(|_| LaraOperationError::CollectAllocationOverflow)?
                == bucket_index
            {
                continue;
            }
            if cover_base.is_none() {
                cover_base = Some(other.edge_start());
            }
            let end = other
                .edge_start()
                .checked_add(u64::from(other.stored_slots_raw()))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            survivor_end = Some(survivor_end.map_or(end, |prev| prev.max(end)));
        }
        if let (Some(base), Some(end)) = (cover_base, survivor_end) {
            let new_stored = end
                .checked_sub(base)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let new_stored = u32::try_from(new_stored)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            self.vertices
                .set(src, &vertex.with_stored_slots(new_stored));
        } else {
            // No slab survivors: vertex span returns to zero.
            self.vertices.set(src, &vertex.with_stored_slots(0));
        }
        Ok(())
    }

    /// Starts collecting the spans of the buckets a delete operation empties.
    ///
    /// While a batch is active, `release_bucket_edge_span_on_empty` records the
    /// emptied range (the per-delete cover sync still runs) instead of releasing
    /// it, and [`Self::flush_span_release_batch`] releases the collected ranges as
    /// pre-merged runs. A drain empties one bucket per neighbour and each
    /// per-bucket release costs ~32 K instructions in free-span-store writes
    /// (GAP-2026-09-20-002), so merging first removes whole releases.
    ///
    /// A batch never nests: the delete entry points own the begin/flush pair.
    pub(crate) fn begin_span_release_batch(&self) {
        debug_assert!(
            self.span_release_batch.borrow().is_none(),
            "span release batches do not nest"
        );
        *self.span_release_batch.borrow_mut() = Some(Vec::new());
    }

    /// Releases every span collected since [`Self::begin_span_release_batch`] as
    /// merged runs. Best-effort like the single-delete release, and it must run
    /// before the delete operation returns: F1's reuse regression asserts a freed
    /// span is available to the allocator once the delete completes.
    pub(crate) fn flush_span_release_batch(&self) {
        let Some(ranges) = self.span_release_batch.borrow_mut().take() else {
            return;
        };
        let mut sorted = ranges;
        sorted.sort_unstable();
        let mut start = 0u64;
        let mut end = 0u64;
        let mut pending = false;
        for (range_start, range_len) in sorted {
            let range_end = range_start.saturating_add(range_len);
            if pending && range_start <= end {
                end = end.max(range_end);
                continue;
            }
            if pending {
                #[cfg(test)]
                record_span_release_batch_flush_run();
                let _ = self.release_vertex_edge_span_slab(start, end - start);
            }
            start = range_start;
            end = range_end;
            pending = true;
        }
        if pending {
            #[cfg(test)]
            record_span_release_batch_flush_run();
            let _ = self.release_vertex_edge_span_slab(start, end - start);
        }
    }

    fn remove_bucket_edge_at_slot(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        slot: u64,
        bucket: LabelBucket,
        slot_index: u32,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let Some((location, removed)) =
            self.locate_bucket_edge_for_delete(src, &bucket, slot_index)?
        else {
            return Ok(None);
        };
        let moves =
            self.remove_bucket_edge_at_location(src, vertex, slot, bucket, slot_index, location)?;
        Ok(Some(EdgeRemoval {
            removed: removed.with_slot_index(slot_index),
            moves,
        }))
    }

    fn locate_bucket_edge_for_delete(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
        slot_index: u32,
    ) -> Result<Option<(BucketEdgeDeleteLocation, E)>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if slot_index >= self.bucket_reserved_edge_slots(src, bucket) {
            return Ok(None);
        }
        let slab_prefix_slots = self.bucket_slab_prefix_slots(src, bucket);
        if slot_index < slab_prefix_slots {
            let physical_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.edges.read_slot(physical_slot);
            return Ok((!edge.is_tombstone_edge())
                .then_some((BucketEdgeDeleteLocation::Slab { physical_slot }, edge)));
        }

        let leaf = self.inline_property_bytes_log_leaf(src);
        let log_ordinal = slot_index
            .checked_sub(slab_prefix_slots)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let chain = self
            .edges
            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
        let Some(&entry_idx) = chain.get(log_ordinal as usize) else {
            return Ok(None);
        };
        let newer_entry_idx = chain.get(log_ordinal as usize + 1).copied();
        let moves = self.overflow_chain_slot_moves_after_delete(
            leaf,
            &chain,
            log_ordinal as usize,
            slab_prefix_slots,
            bucket.bucket_label_key(),
        )?;
        let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
        Ok((!edge.is_tombstone_edge()).then_some((
            BucketEdgeDeleteLocation::OverflowLog {
                leaf,
                entry_idx,
                newer_entry_idx,
                moves,
            },
            edge,
        )))
    }

    fn remove_bucket_edge_at_location(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        slot: u64,
        mut bucket: LabelBucket,
        slot_index: u32,
        location: BucketEdgeDeleteLocation,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.inline_property_byte_width() == 0 {
            return self.commit_bucket_edge_delete(src, slot, bucket, slot_index, location);
        }

        let bucket_index = Self::labeled_bucket_descriptor_index(vertex, slot)?;
        let Some(inline_property_bytes_ordinal) = self.bucket_live_ordinal_at_edge_slot(
            src,
            vertex,
            bucket_index,
            slot,
            &bucket,
            slot_index,
        )?
        else {
            return Err(LaraOperationError::LogChainShort.into());
        };
        if bucket.inline_property_bytes_log_head() >= 0 {
            self.rebalance_inline_property_bytes_log_leaf_for_labeled(src)?;
            bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        let inline_property_bytes_delete = self.plan_bucket_inline_property_bytes_delete(
            src,
            bucket,
            inline_property_bytes_ordinal,
        )?;
        if let Some(plan) = inline_property_bytes_delete {
            bucket = self.apply_bucket_inline_property_bytes_delete(src, plan);
        }
        self.commit_bucket_edge_delete(src, slot, bucket, slot_index, location)
    }

    #[inline]
    fn commit_bucket_edge_delete(
        &self,
        src: VertexId,
        slot: u64,
        bucket: LabelBucket,
        _removed_slot_index: u32,
        location: BucketEdgeDeleteLocation,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let (removed_from_log, new_log_head, moves) = match location {
            BucketEdgeDeleteLocation::Slab { physical_slot } => {
                self.edges
                    .write_slot(physical_slot, E::tombstone_edge())
                    .unwrap_or_else(|_| panic!("preflighted edge tombstone write failed"));
                (false, bucket.overflow_log_head(), Vec::new())
            }
            BucketEdgeDeleteLocation::OverflowLog {
                leaf,
                entry_idx,
                newer_entry_idx,
                moves,
            } => {
                let new_head = self
                    .edges
                    .unlink_overflow_log_entry(
                        leaf,
                        bucket.overflow_log_head(),
                        entry_idx,
                        newer_entry_idx,
                    )
                    .unwrap_or_else(|_| panic!("preflighted edge-log unlink failed"));
                (true, new_head, moves)
            }
        };
        let updated = bucket
            .with_overflow_log_head(new_log_head)
            .after_slab_tombstone_delete();
        // F1: an emptied bucket releases its slab span back to the free store
        // and syncs the vertex cover (stale width/anchor otherwise linger as
        // phantom occupancy until slide heals them; block release reclaims the
        // leak only at relocate). The release runs before the descriptor
        // publish so a release failure leaves canonical state untouched.
        // Best-effort release (overlapping promote spans can double-release;
        // pre-existing tiling incoherence, silent before F1 since nothing
        // released these spans): a failed release keeps the old phantom
        // behavior (slide heals), never fails a successful delete (Plan 0319
        // §Step 2 precedent). The cover sync below always runs (it only
        // shrinks to survivor ends, never below live content).
        let emptied = updated.degree() == 0 && updated.overflow_log_head() < 0;
        if emptied && updated.stored_slots() > 0 {
            self.release_bucket_edge_span_on_empty(src, slot, &updated)?;
        }
        let updated = if emptied {
            updated
                .with_inline_property_bytes_log_head(-1)
                .with_inline_property_bytes_slab_slots(0)
                .with_edge_range(updated.edge_start(), 0)
        } else {
            updated
        };
        if bucket.inline_property_byte_width() == 0
            && !removed_from_log
            && !(updated.degree() == 0 && updated.overflow_log_head() < 0)
        {
            self.buckets
                .write_label_bucket_degree(slot, updated.degree())?;
        } else {
            self.buckets.write_label_bucket_slot(slot, updated)?;
        }
        self.decrement_edge_counts_after_remove(src)?;
        if removed_from_log {
            self.invalidate_bucket_lookup_for_label(src, bucket.bucket_label_key());
        }
        Ok(moves)
    }

    pub(super) fn for_each_out_edges_by_directedness_impl<B, Visit>(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        directedness: BucketDirectedness,
        ascending: bool,
        visit: &mut Visit,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        Visit: FnMut(E) -> ControlFlow<B>,
    {
        if vertex.is_default_edge_labeled() {
            if self.bypass_storage_label_for(vertex).directedness() != directedness {
                return Ok(ControlFlow::Continue(()));
            }
            match ascending {
                false => {
                    let slab_iter = OutEdgeSlabIter::try_new(
                        &self.edges,
                        vertex.base_slot_start(),
                        vertex.stored_degree(),
                        vertex.degree(),
                    )?;
                    let label = self.bypass_storage_label_for(vertex).raw();
                    for edge in slab_iter {
                        if let ControlFlow::Break(value) = visit(edge.with_label_id(label)) {
                            return Ok(ControlFlow::Break(value));
                        }
                    }
                }
                true => {
                    let label = self.bypass_storage_label_for(vertex).raw();
                    for edge in self
                        .edges
                        .collect_out_edges_slot_order(&self.vertices, src)?
                    {
                        if let ControlFlow::Break(value) = visit(edge.with_label_id(label)) {
                            return Ok(ControlFlow::Break(value));
                        }
                    }
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        let deg = vertex.degree();
        let strategy = Self::directedness_partition_strategy(directedness, ascending);
        let (lo, hi) = self.buckets.directedness_bucket_index_range(
            vertex.base_slot_start(),
            deg,
            directedness,
            strategy,
        )?;
        if lo >= hi {
            return Ok(ControlFlow::Continue(()));
        }
        let first_global = self
            .buckets
            .read_label_bucket_slot(vertex.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_end_exclusive = Self::vertex_label_edge_span_end_exclusive(vertex, &first_global)?;
        let buckets = self.read_vertex_label_buckets_range(vertex, lo, hi)?;
        if let Some(run) =
            Self::try_contiguous_tiled_labeled_out_edges_slice(&buckets, span_end_exclusive)
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_out_edges_by_directedness_tiled");
            if run.total_edges() > 0 {
                let nbytes = run.byte_len::<E>()?;
                let mut raw = vec![0u8; nbytes];
                self.edges.read_slots_contiguous(run.base(), &mut raw);
                match ascending {
                    false => {
                        let mut bucket_rev_idx = buckets.len() as isize - 1;
                        let mut slot_rev: Option<u32> = None;
                        while bucket_rev_idx >= 0 {
                            let bidx = bucket_rev_idx as usize;
                            let bucket = &buckets[bidx];
                            if bucket.degree() == 0 {
                                bucket_rev_idx -= 1;
                                slot_rev = None;
                                continue;
                            }
                            let bucket_index = lo + bidx as u32;
                            let log_chains =
                                self.bucket_inline_property_bytes_log_chain_opt(src, bucket);
                            let slot = slot_rev.unwrap_or(bucket.degree().saturating_sub(1));
                            let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                            if let ControlFlow::Break(value) = visit(
                                self.attach_edge_inline_property(
                                    src,
                                    vertex,
                                    bucket_index,
                                    *bucket,
                                    slot,
                                    E::read_from(chunk)
                                        .with_slot_index(slot)
                                        .with_label_id(bucket.bucket_label_key().raw()),
                                    log_chains.as_ref(),
                                )?,
                            ) {
                                return Ok(ControlFlow::Break(value));
                            }
                            if slot == 0 {
                                bucket_rev_idx -= 1;
                                slot_rev = None;
                            } else {
                                slot_rev = Some(slot - 1);
                            }
                        }
                    }
                    true => {
                        for (local, bucket) in buckets.iter().enumerate() {
                            if bucket.degree() == 0 {
                                continue;
                            }
                            let bucket_index = lo + local as u32;
                            let log_chains =
                                self.bucket_inline_property_bytes_log_chain_opt(src, bucket);
                            for slot in 0..bucket.degree() {
                                let chunk = run.edge_chunk::<E>(&raw, bucket, slot)?;
                                if let ControlFlow::Break(value) = visit(
                                    self.attach_edge_inline_property(
                                        src,
                                        vertex,
                                        bucket_index,
                                        *bucket,
                                        slot,
                                        E::read_from(chunk)
                                            .with_slot_index(slot)
                                            .with_label_id(bucket.bucket_label_key().raw()),
                                        log_chains.as_ref(),
                                    )?,
                                ) {
                                    return Ok(ControlFlow::Break(value));
                                }
                            }
                        }
                    }
                }
            }
            return Ok(ControlFlow::Continue(()));
        }
        match ascending {
            false => {
                for local_rev in (0..buckets.len()).rev() {
                    let bucket_index = lo + local_rev as u32;
                    let bucket = &buckets[local_rev];
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let log_chains = self.bucket_inline_property_bytes_log_chain_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor = self.bucket_slab_window_end_exclusive_after_bucket(
                        vertex,
                        bucket_index,
                        bucket,
                    )?;
                    let acc = LabelEdgeSpanAccess::with_bucket(
                        &self.buckets,
                        slot,
                        *bucket,
                        successor,
                        src,
                    );
                    if bucket.overflow_log_head() < 0 {
                        let it = OutEdgeSlabIter::try_new(
                            &self.edges,
                            bucket.edge_start(),
                            bucket.stored_slots(),
                            bucket.degree(),
                        )?;
                        for edge in it {
                            let slot_index = edge.edge_slot_index_raw();
                            if let ControlFlow::Break(value) =
                                visit(self.attach_edge_inline_property(
                                    src,
                                    vertex,
                                    bucket_index,
                                    *bucket,
                                    slot_index,
                                    edge.with_label_id(bucket.bucket_label_key().raw()),
                                    log_chains.as_ref(),
                                )?)
                            {
                                return Ok(ControlFlow::Break(value));
                            }
                        }
                    } else {
                        for edge in self.edges.out_edges_iter(&acc, VertexId::from(0))? {
                            let slot_index = edge.edge_slot_index_raw();
                            if let ControlFlow::Break(value) =
                                visit(self.attach_edge_inline_property(
                                    src,
                                    vertex,
                                    bucket_index,
                                    *bucket,
                                    slot_index,
                                    edge.with_label_id(bucket.bucket_label_key().raw()),
                                    log_chains.as_ref(),
                                )?)
                            {
                                return Ok(ControlFlow::Break(value));
                            }
                        }
                    }
                }
            }
            true => {
                for (local, bucket) in buckets.iter().enumerate() {
                    let bucket_index = lo + local as u32;
                    if bucket.degree() == 0 {
                        continue;
                    }
                    let log_chains = self.bucket_inline_property_bytes_log_chain_opt(src, bucket);
                    let slot = Self::labeled_vertex_bucket_slot(vertex, bucket_index)?;
                    let successor = self.bucket_slab_window_end_exclusive_after_bucket(
                        vertex,
                        bucket_index,
                        bucket,
                    )?;
                    let acc = LabelEdgeSpanAccess::with_bucket(
                        &self.buckets,
                        slot,
                        *bucket,
                        successor,
                        src,
                    );
                    for edge in self
                        .edges
                        .collect_out_edges_slot_order(&acc, VertexId::from(0))?
                    {
                        let slot_index = edge.edge_slot_index_raw();
                        if let ControlFlow::Break(value) = visit(self.attach_edge_inline_property(
                            src,
                            vertex,
                            bucket_index,
                            *bucket,
                            slot_index,
                            edge.with_label_id(bucket.bucket_label_key().raw()),
                            log_chains.as_ref(),
                        )?) {
                            return Ok(ControlFlow::Break(value));
                        }
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Visits outgoing edges whose bucket directedness matches `directedness`.
    pub fn for_each_out_edges_by_directedness<B, Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        Visit: FnMut(E) -> ControlFlow<B>,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        self.for_each_out_edges_by_directedness_impl(
            src,
            &vertex,
            directedness,
            order.ascending(),
            &mut visit,
        )
    }

    /// Visits outgoing edges by directedness without checking that `src` is in range.
    pub fn for_each_out_edges_by_directedness_unchecked<B, Visit>(
        &self,
        src: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        mut visit: Visit,
    ) -> Result<ControlFlow<B>, LabeledOperationError>
    where
        Visit: FnMut(E) -> ControlFlow<B>,
    {
        debug_assert!(u32::from(src) < self.vertices.len());
        let vertex = self.vertices.get(src);
        self.for_each_out_edges_by_directedness_impl(
            src,
            &vertex,
            directedness,
            order.ascending(),
            &mut visit,
        )
    }

    /// Finds the label bucket containing `needle` on `src`.
    pub fn find_edge_label(
        &self,
        src: VertexId,
        needle: &E,
    ) -> Result<Option<BucketLabelKey>, LabeledOperationError>
    where
        E: PartialEq + CsrEdgeTombstone,
    {
        self.find_nth_edge_with_inline_property_matching(
            src,
            super::traverse::EdgeFindScope::AllLabels,
            OutEdgeOrder::Descending,
            0,
            |edge| Self::edge_matches_label_lookup(edge, needle),
        )
        .map(|found| found.map(|f| f.label))
    }

    /// Returns the labels that currently have outgoing edges for `src`.
    pub fn out_edge_label_ids(
        &self,
        src: VertexId,
    ) -> Result<Vec<BucketLabelKey>, LabeledOperationError> {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            if vertex.degree() == 0 {
                return Ok(Vec::new());
            }
            return Ok(vec![self.bypass_storage_label_for(&vertex)]);
        }
        let deg = vertex.degree();
        if deg == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(deg as usize);
        for offset in 0..deg {
            let slot = Self::labeled_vertex_bucket_slot(&vertex, offset)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            out.push(bucket.bucket_label_key());
        }
        Ok(out)
    }

    /// Removes every live out-edge for `label_id` at `src`, visiting each removed edge.
    ///
    /// Addresses slots by **descending reserved index** and delegates each removal to
    /// [`Self::remove_edge_at_slot`]. This issues exactly the same per-edge removals (and
    /// therefore the same edge-count/span bookkeeping) as draining the bucket one edge at a
    /// time with [`Self::remove_edge_matching`], but without the O(degree) predicate scan or
    /// the leading-tombstone re-scan that a "find first live, remove, repeat" loop incurs
    /// (tombstones are not trimmed from `stored_slots`; see
    /// [`crate::traits::CsrVertex::after_slab_tombstone_delete`]). Cost is O(reserved slots)
    /// for the slab prefix; overflow-log slots remain O(chain) per removal.
    pub(crate) fn drain_out_edges_for_label<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut visit: F,
    ) -> Result<u32, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(E),
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        let reserved = if vertex.is_default_edge_labeled() {
            if label_id != self.bypass_storage_label_for(&vertex) {
                return Ok(0);
            }
            vertex.stored_degree()
        } else {
            match self.find_bucket(src, &vertex, label_id)? {
                BucketSearch::Found { bucket, .. } => self.bucket_reserved_edge_slots(src, &bucket),
                BucketSearch::Missing { .. } => return Ok(0),
            }
        };
        let mut removed = 0u32;
        for slot_index in (0..reserved).rev() {
            if let Some(edge) = self.remove_edge_at_slot(src, label_id, slot_index)? {
                visit(edge);
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
    }

    /// Removes one live out-edge from `src` (the highest-index live slot of its
    /// first non-empty label bucket / bypass region) and returns it with `slot_index`
    /// and `label_id` set, plus the bucket label. Returns `None` when `src` has no
    /// live out-edge.
    ///
    /// This is the single-edge counterpart of [`Self::drain_out_edges_for_label`],
    /// used by the resumable vertex-delete step (one edge per maintenance step).
    /// When the caller front-packs the live edges first (the delete drain compacts
    /// the row once before draining), each call is O(1): the top slab slot is
    /// `live - 1`. If tombstones interleave the reserved span, it falls back to a
    /// descending scan of that bucket.
    pub(crate) fn remove_top_out_edge(
        &self,
        src: VertexId,
    ) -> Result<Option<(E, BucketLabelKey)>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            let live = vertex.degree();
            if live == 0 {
                return Ok(None);
            }
            let label = self.bypass_storage_label_for(&vertex);
            let reserved = vertex.stored_degree();
            return Ok(self
                .remove_highest_live_edge_in_bucket(src, label, reserved, live)?
                .map(|edge| (edge, label)));
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        for bucket in buckets {
            let live = bucket.degree();
            if live == 0 {
                continue;
            }
            let label = bucket.bucket_label_key();
            let reserved = self.bucket_reserved_edge_slots(src, &bucket);
            if let Some(edge) =
                self.remove_highest_live_edge_in_bucket(src, label, reserved, live)?
            {
                return Ok(Some((edge, label)));
            }
        }
        Ok(None)
    }

    /// Removes the highest-index live edge in `src`'s `label_id` bucket. Tries the
    /// front-packed top slot (`live - 1`) first, then scans the reserved span
    /// descending for any surviving edge. The returned edge carries its slot index
    /// and label for sidecar cleanup by the delete observer.
    fn remove_highest_live_edge_in_bucket(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        reserved: u32,
        live: u32,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if live == 0 || reserved == 0 {
            return Ok(None);
        }
        let tag = |edge: E, slot: u32| edge.with_slot_index(slot).with_label_id(label_id.raw());
        if live <= reserved {
            let top = live - 1;
            if let Some(edge) = self.remove_edge_at_slot(src, label_id, top)? {
                return Ok(Some(tag(edge, top)));
            }
        }
        for slot in (0..reserved).rev() {
            if live <= reserved && slot == live - 1 {
                continue;
            }
            if let Some(edge) = self.remove_edge_at_slot(src, label_id, slot)? {
                return Ok(Some(tag(edge, slot)));
            }
        }
        Ok(None)
    }

    /// Removes the first edge in `label_id` for `src` that satisfies `matches`.
    pub(crate) fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        Ok(self
            .remove_edge_matching_with_move(src, label_id, matches)?
            .map(|removal| removal.removed))
    }

    /// Removes the first matching edge and reports the bounded survivor slot shifts produced by
    /// overflow unlink.
    pub(crate) fn remove_edge_matching_with_move<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.remove_edge_matching_skip_leaf_cascade_with_move(src, label_id, matches)
    }

    pub(crate) fn remove_edge_matching_skip_leaf_cascade<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        matches: F,
    ) -> Result<Option<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        Ok(self
            .remove_edge_matching_skip_leaf_cascade_with_move(src, label_id, matches)?
            .map(|removal| removal.removed))
    }

    #[allow(clippy::needless_return)]
    pub(crate) fn remove_edge_matching_skip_leaf_cascade_with_move<F>(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        mut matches: F,
    ) -> Result<Option<EdgeRemoval<E>>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        self.ensure_vertex(src)?;
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() {
            return self.remove_default_bypass_edge_matching(src, label_id, vertex, matches);
        }
        if let BucketSearch::Found { slot, bucket } = self.find_bucket(src, &vertex, label_id)? {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_remove_edge_skip_leaf");
            // ADR 0096 §7: match-first dispatch — mode decides before any
            // storage-class read, so a future mode cannot silently inherit a path.
            match BucketMode::from_bucket(&bucket) {
                // ADR 0096 §5: tiny buckets match inline (live prefix, holes
                // skipped via the layout predicate; no slab/log reads). Predicate
                // input mirrors the slab path (label attached,
                // no values — tiny width is identically zero).
                BucketMode::Tiny => {
                    debug_assert!(
                        E::BYTES == 4,
                        "tiny buckets require 4-byte edges (birth gate)"
                    );
                    for ordinal in 0..bucket.degree() {
                        let edge = E::read_from(&bucket.tiny_target(ordinal).to_le_bytes())
                            .with_slot_index(ordinal)
                            .with_label_id(label_id.raw());
                        if matches(&edge) {
                            return self.remove_tiny_edge_at_slot(src, slot, &bucket, ordinal);
                        }
                    }
                    return Ok(None);
                }
                // Tree and slab share matching-predicate paths (log chains +
                // slab prefix generic over both; tree widths are real). One
                // combined arm keeps them explicitly joint — a future mode
                // must still choose.
                BucketMode::Tree | BucketMode::Slab => {
                    let bucket_index = Self::labeled_bucket_descriptor_index(&vertex, slot)?;
                    if bucket.degree() == 0 {
                        return Ok(None);
                    }
                    if bucket.overflow_log_head() >= 0 {
                        let log_chains =
                            self.bucket_inline_property_bytes_log_chain_opt(src, &bucket);
                        let slab_prefix_slots = self.bucket_slab_prefix_slots(src, &bucket);
                        for slot_index in 0..slab_prefix_slots {
                            let edge_slot =
                                checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let edge = self.edges.read_slot(edge_slot);
                            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                                continue;
                            }
                            let edge_with_value = self.attach_edge_inline_property(
                                src,
                                &vertex,
                                bucket_index,
                                bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            )?;
                            if matches(&edge_with_value) {
                                let moves = self.remove_bucket_edge_at_location(
                                    src,
                                    &vertex,
                                    slot,
                                    bucket,
                                    slot_index,
                                    BucketEdgeDeleteLocation::Slab {
                                        physical_slot: edge_slot,
                                    },
                                )?;
                                return Ok(Some(EdgeRemoval {
                                    removed: edge_with_value,
                                    moves,
                                }));
                            }
                        }
                        let leaf = self.inline_property_bytes_log_leaf(src);
                        let chain = self
                            .edges
                            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
                        for (ordinal, entry_idx) in chain.iter().copied().enumerate() {
                            let (_, edge) = self.edges.read_overflow_log_entry(leaf, entry_idx);
                            if edge.is_tombstone_edge() {
                                continue;
                            }
                            let slot_index =
                                bucket
                                    .stored_slots_raw()
                                    .checked_add(u32::try_from(ordinal).map_err(|_| {
                                        LaraOperationError::CollectAllocationOverflow
                                    })?)
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let edge_with_value = self.attach_edge_inline_property(
                                src,
                                &vertex,
                                bucket_index,
                                bucket,
                                slot_index,
                                edge.with_label_id(bucket.bucket_label_key().raw()),
                                log_chains.as_ref(),
                            )?;
                            if matches(&edge_with_value) {
                                let moves = self.overflow_chain_slot_moves_after_delete(
                                    leaf,
                                    &chain,
                                    ordinal,
                                    bucket.stored_slots_raw(),
                                    bucket.bucket_label_key(),
                                )?;
                                let newer_entry_idx = chain.get(ordinal + 1).copied();
                                let committed_moves = self.remove_bucket_edge_at_location(
                                    src,
                                    &vertex,
                                    slot,
                                    bucket,
                                    slot_index,
                                    BucketEdgeDeleteLocation::OverflowLog {
                                        leaf,
                                        entry_idx,
                                        newer_entry_idx,
                                        moves,
                                    },
                                )?;
                                return Ok(Some(EdgeRemoval {
                                    removed: edge_with_value,
                                    moves: committed_moves,
                                }));
                            }
                        }
                        return Ok(None);
                    }
                    let stored = bucket.stored_slots_raw();
                    let mut found = None;
                    if bucket.is_inline_property_bytes_allocated() {
                        let log_chains =
                            self.bucket_inline_property_bytes_log_chain_opt(src, &bucket);
                        for offset in 0..stored {
                            let edge_slot =
                                checked_add_slot_index(bucket.edge_start(), u64::from(offset))
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let edge = self.edges.read_slot(edge_slot);
                            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                                continue;
                            }
                            let edge_with_value = self.attach_edge_inline_property(
                                src,
                                &vertex,
                                bucket_index,
                                bucket,
                                offset,
                                edge,
                                log_chains.as_ref(),
                            )?;
                            if matches(&edge_with_value) {
                                found = Some((
                                    offset,
                                    edge_with_value,
                                    BucketEdgeDeleteLocation::Slab {
                                        physical_slot: edge_slot,
                                    },
                                ));
                                break;
                            }
                        }
                    } else {
                        for offset in 0..stored {
                            let edge_slot =
                                checked_add_slot_index(bucket.edge_start(), u64::from(offset))
                                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                            let edge = self.edges.read_slot(edge_slot);
                            if edge.is_tombstone_edge() {
                                continue;
                            }
                            if matches(&edge) {
                                found = Some((
                                    offset,
                                    edge,
                                    BucketEdgeDeleteLocation::Slab {
                                        physical_slot: edge_slot,
                                    },
                                ));
                                break;
                            }
                        }
                    }
                    let Some((local_index, removed, location)) = found else {
                        return Ok(None);
                    };
                    let moves = self.remove_bucket_edge_at_location(
                        src,
                        &vertex,
                        slot,
                        bucket,
                        local_index,
                        location,
                    )?;
                    return Ok(Some(EdgeRemoval { removed, moves }));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only count of store releases a span-release batch flush performs (one
    /// per merged run). Pins the batching contract of
    /// `flush_span_release_batch`: a drain must flush merged runs, not one span
    /// per emptied bucket (GAP-2026-09-20-002).
    static SPAN_RELEASE_BATCH_FLUSH_RUNS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_span_release_batch_flush_runs() {
    SPAN_RELEASE_BATCH_FLUSH_RUNS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn span_release_batch_flush_runs() -> u64 {
    SPAN_RELEASE_BATCH_FLUSH_RUNS.with(|c| c.get())
}

#[cfg(test)]
fn record_span_release_batch_flush_run() {
    SPAN_RELEASE_BATCH_FLUSH_RUNS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use crate::{
        VertexId,
        traits::{CsrEdge, CsrEdgeTombstone},
    };
    use std::ops::ControlFlow;

    #[test]
    fn tombstone_bit_edges_satisfy_csr_liveness_contract() {
        let tombstone = FlagTombstoneEdge::tombstone_edge();
        assert!(tombstone.is_tombstone_edge());
        assert!(
            tombstone.is_deleted_slot(),
            "storage read paths with only a CsrEdge bound rely on is_deleted_slot"
        );
    }

    #[test]
    fn remove_edge_at_slot_uses_edge_tombstone_contract() {
        let graph = flag_tombstone_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(
                VertexId::from(0),
                road,
                FlagTombstoneEdge::live(10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let removed = graph
            .remove_edge_matching(VertexId::from(0), road, |edge| {
                edge.neighbor_vid() == VertexId::from(10)
            })
            .unwrap();
        assert_eq!(removed, Some(FlagTombstoneEdge::live(10)));

        let removed_again = graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap();
        assert_eq!(removed_again, None);
    }

    #[test]
    fn emptied_bucket_releases_span_and_zeroes_vertex_cover() {
        // F1: deleting the last edge of a slab bucket frees its span and
        // syncs the vertex cover (no phantom occupancy between maintenance).
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        for target in [10u32, 11, 12, 13] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(!bucket.is_tiny_mode(), "4 seeds promote past tiny");
        let span_start = bucket.edge_start();
        let span_len = u64::from(bucket.stored_slots_raw());
        assert!(span_len > 0);
        for target in [10u32, 11, 12, 13] {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap()
                .expect("edge must delete");
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.degree(), 0);
        assert_eq!(
            bucket.stored_slots_raw(),
            0,
            "emptied span publishes zero width"
        );
        assert_eq!(
            vertex.stored_slots, 0,
            "vertex cover syncs to zero with no slab survivors"
        );
        // The freed span is reusable: it must be covered by free spans.
        let free = graph.edges().free_span_store().spans();
        assert!(
            free.iter().any(|span| span.start_slot <= span_start
                && span.start_slot.saturating_add(span.len) >= span_start + span_len),
            "freed span must be covered by free spans"
        );
        // Graph stays fully operational and audit-clean after F1 work.
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn remove_edge_leaves_slab_tombstone_until_rebalance() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        // Four seeds: the 4th promotes tiny->slab, so the delete below takes
        // the slab tombstone path (the test's intent; three seeds would stay
        // tiny and take the inline-tombstone path instead).
        for target in [10u32, 11, 12, 13] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 11)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 13 },
                TestEdge { target: 12 },
                TestEdge { target: 10 }
            ]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 10 },
                TestEdge { target: 12 },
                TestEdge { target: 13 }
            ]
        );
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots(), 4);
        assert_eq!(bucket.stored_slots().saturating_sub(bucket.degree), 1);
        assert_eq!(bucket.degree(), 3);

        graph
            .insert_edge(
                VertexId::from(0),
                road,
                TestEdge { target: 14 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 14 },
                TestEdge { target: 13 },
                TestEdge { target: 12 },
                TestEdge { target: 10 },
            ]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![
                TestEdge { target: 10 },
                TestEdge { target: 12 },
                TestEdge { target: 13 },
                TestEdge { target: 14 },
            ]
        );
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        // The span is full (tombstone occupies the 4th slot), so the re-insert
        // spills to the overflow log; the tombstone stays in place until
        // rebalance (positional stability).
        assert_eq!(bucket.stored_slots(), 4);
        assert!(bucket.overflow_log_head() >= 0);
        assert_eq!(bucket.degree(), 4);
    }

    #[test]
    fn removing_log_edge_unlinks_without_tombstone_and_preserves_scan_order() {
        let graph = inline_property_test_graph_with_capacity(1 << 20);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        for target in 1..=35u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(0),
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let slot_of = |target| {
            let mut slot = None;
            graph
                .visit_edges(
                    VertexId::from(0),
                    road,
                    OutEdgeOrder::Descending,
                    |s, edge| {
                        if edge.target == target {
                            slot = Some(s.raw());
                        }
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .unwrap();
            slot.expect("target edge exists")
        };
        let later_slot_before = slot_of(34);
        let removed_slot = slot_of(33);
        let head_slot_before = slot_of(35);

        let removal = graph
            .remove_edge_matching_with_move(VertexId::from(0), road, |edge| edge.target == 33)
            .unwrap()
            .expect("log edge removed");
        assert_eq!(removal.removed.target, 33);
        assert_eq!(
            removal.moves,
            vec![
                EdgeSlotMove {
                    label_id: road,
                    old_slot_index: later_slot_before,
                    new_slot_index: removed_slot,
                },
                EdgeSlotMove {
                    label_id: road,
                    old_slot_index: head_slot_before,
                    new_slot_index: head_slot_before - 1,
                },
            ]
        );
        assert_eq!(slot_of(34), later_slot_before - 1);
        assert_eq!(slot_of(35), head_slot_before - 1);
        assert!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .iter()
                .all(|edge| edge.target != 33)
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        let slab_prefix = graph.bucket_slab_prefix_slots(VertexId::from(0), &bucket);
        let leaf = graph.inline_property_bytes_log_leaf(VertexId::from(0));
        let chain = graph
            .edges()
            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
        assert_eq!(chain.len() as u32, bucket.degree() - slab_prefix);
        assert!(chain.into_iter().all(|entry_idx| {
            let (_, edge) = graph.edges().read_overflow_log_entry(leaf, entry_idx);
            !edge.is_deleted_slot()
        }));
        let moved = graph
            .iter_edges_for_label(VertexId::from(0), road)
            .unwrap()
            .into_iter()
            .find(|edge| edge.target == 35)
            .expect("newest edge survives at the newest slot");
        assert_eq!(moved.value, 35u16.to_le_bytes());
    }

    #[test]
    fn direct_unlink_preserves_legacy_tombstone_head_compatibility() {
        let graph = inline_property_test_graph();
        let src = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        // ADR 0096 §4: tiny birth holds the first three edges inline (no log).
        // Seed past promotion (4th) into log spill (5th) so the bucket owns a
        // real overflow-log head for the direct-unlink step below.
        for target in [10, 11, 12, 13, 14] {
            graph
                .insert_edge(
                    src,
                    road,
                    InlinePropertyTestEdge::with_bytes(target, &[]),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        let vertex = graph.vertices().get(src);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);
        let leaf = graph.inline_property_bytes_log_leaf(src);
        graph
            .edges()
            .rewrite_overflow_log_entry_tombstone(leaf, bucket.overflow_log_head() as u32)
            .unwrap();
        graph
            .buckets()
            .write_label_bucket_degree(bucket_slot, 4)
            .unwrap();

        let removal = graph
            .remove_edge_matching_with_move(src, road, |edge| edge.target == 10)
            .unwrap()
            .unwrap();
        assert_eq!(removal.removed.target, 10);
        // ADR 0096 §4: post-promotion the removed edge is slab-resident, so
        // the delete tombstones in place (no log-ordinal renumbering moves).
        // The legacy-compat intent (direct log-head unlink + consistent
        // follow-up remove) holds: moves are empty and adjacency is exact.
        assert_eq!(removal.moves, vec![]);
        assert_eq!(
            graph
                .iter_edges_for_label(src, road)
                .unwrap()
                .into_iter()
                .map(|edge| (edge.slot_index, edge.target))
                .collect::<Vec<_>>(),
            vec![(3, 13), (2, 12), (1, 11)]
        );
    }

    #[test]
    fn remove_edge_from_one_label_keeps_next_label_isolated() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let walk = BucketLabelKey::from_raw(3);
        for target in [10, 11] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    road,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for target in [20, 21] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    walk,
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 10)
            .unwrap();

        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), walk).unwrap(),
            vec![TestEdge { target: 21 }, TestEdge { target: 20 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn default_bypass_conversion_clears_vertex_edge_span_allocation() {
        let graph = test_graph();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                VertexId::from(0),
                LabelBucket::default().with_bucket_label_key(graph.default_label()),
            )
            .unwrap();
        for target in [7u32, 8] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    graph.default_label(),
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let before = graph.vertices().get(VertexId::from(0));
        assert!(!before.is_default_edge_labeled());
        assert_eq!(before.degree(), 1);

        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert!(after.is_default_edge_labeled());
        assert_eq!(after.degree(), 2);
        assert_eq!(after.stored_slots, 2);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 8 }, TestEdge { target: 7 }]
        );
    }

    #[test]
    fn releasing_one_bucket_inline_property_bytes_span_keeps_other_bucket_inline_property_bytes_log()
     {
        let graph = inline_property_test_graph_with_capacity(1 << 20);
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let road = BucketLabelKey::from_raw(2);
        let rail = BucketLabelKey::from_raw(3);
        let other = BucketLabelKey::from_raw(4);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), other, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                other,
                InlinePropertyTestEdge::with_bytes(400, &400u16.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            graph
                .ensure_label_bucket_inline_property_byte_width(src, label, 2u16)
                .unwrap();
        }
        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            for target in 1..=33u32 {
                let weight = if label == road {
                    target
                } else {
                    target.saturating_mul(10)
                };
                graph
                    .insert_edge_skip_leaf_cascade(
                        src,
                        label,
                        InlinePropertyTestEdge::with_bytes(
                            target,
                            &u16::try_from(weight).unwrap().to_le_bytes(),
                        ),
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }

        let tail_blocker = BucketLabelKey::from_raw(5);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), tail_blocker, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                tail_blocker,
                InlinePropertyTestEdge::with_bytes(500, &500u16.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        for (src, label) in [(VertexId::from(0), road), (VertexId::from(1), rail)] {
            let value = if label == road { 33 } else { 330 };
            graph
                .insert_edge_skip_leaf_cascade(
                    src,
                    label,
                    InlinePropertyTestEdge::with_bytes(
                        33,
                        &u16::try_from(value).unwrap().to_le_bytes(),
                    ),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let road_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        let rail_vertex = graph.vertices().get(VertexId::from(1));
        let rail_slot = graph.find_bucket_slot(&rail_vertex, rail).unwrap().unwrap();
        let rail_bucket = graph.buckets().read_label_bucket_slot(rail_slot).unwrap();
        assert!(road_bucket.inline_property_bytes_log_head() >= 0);
        assert!(rail_bucket.inline_property_bytes_log_head() >= 0);
        let leaf = graph.inline_property_bytes_log_leaf(VertexId::from(1));
        let rail_head = u32::try_from(rail_bucket.inline_property_bytes_log_head()).unwrap();
        let mut before = [0u8; 2];
        graph
            .values()
            .read_inline_property_bytes_log_entry(
                leaf,
                rail_head,
                rail_bucket.inline_property_byte_width(),
                &mut before,
            )
            .expect("read before");

        let road_bucket_log_only = road_bucket.with_degree_field(0).with_stored_slots(0);
        graph
            .release_bucket_inline_property_bytes_span(VertexId::from(0), &road_bucket_log_only)
            .unwrap();

        let mut after = [0u8; 2];
        graph
            .values()
            .read_inline_property_bytes_log_entry(
                leaf,
                rail_head,
                rail_bucket.inline_property_byte_width(),
                &mut after,
            )
            .expect("read after");
        assert_ne!(before, [0, 0]);
        assert_eq!(after, before);
    }

    #[test]
    fn removing_last_inline_property_bytes_edge_clears_vertex_inline_property_bytes_allocation() {
        let graph = inline_property_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                InlinePropertyTestEdge::with_bytes(1, &42u16.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .inline_property_bytes_allocated_bytes(),
            2
        );

        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == 1)
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.inline_property_bytes_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_inline_property_bytes_allocated());
        assert_eq!(bucket.inline_property_bytes_log_head(), -1);
    }

    #[test]
    fn removing_last_inline_property_bytes_edge_by_slot_clears_vertex_inline_property_bytes_allocation()
     {
        let graph = inline_property_test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), road, 2u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                VertexId::from(0),
                road,
                InlinePropertyTestEdge::with_bytes(1, &42u16.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        assert_eq!(
            graph
                .vertices()
                .get(VertexId::from(0))
                .inline_property_bytes_allocated_bytes(),
            2
        );

        let removed = graph
            .remove_edge_at_slot(VertexId::from(0), road, 0)
            .unwrap()
            .expect("removed edge");
        assert_eq!(removed.target, 1);

        let vertex = graph.vertices().get(VertexId::from(0));
        assert_eq!(vertex.inline_property_bytes_allocated_bytes(), 0);
        let bucket_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
        assert!(!bucket.is_inline_property_bytes_allocated());
        assert_eq!(bucket.inline_property_bytes_log_head(), -1);
    }

    // Delete redesign: tiny deletes tombstone inline (no promotion).
    // Survivors keep their slots (positional stability holds trivially —
    // nothing moves); the hole is reported with empty moves like slab deletes.
    #[test]
    fn tiny_remove_tombstones_inline() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        crate::labeled::graph::test_support::force_tiny_bucket(&graph, vid, label, &[10, 11, 12]);
        let actual_before = graph.leaf_segment_counts_for_vid(vid).actual;
        let num_before = graph.edges().header().num_edges;
        let removed = graph
            .remove_edge_at_slot(vid, label, 1)
            .unwrap()
            .expect("removed edge");
        assert_eq!(removed.target, 11);
        let vertex = graph.vertices().get(vid);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(graph.find_bucket_slot(&vertex, label).unwrap().unwrap())
            .unwrap();
        assert!(bucket.is_tiny_mode(), "delete stays tiny (no promotion)");
        // Hole at slot 1 (sentinel), live prefix width 3, live count 2.
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (2, 3));
        // Layout-native tombstone: the hole decodes as deleted through the
        // edge layout's own predicate (same as slab/tree read paths).
        assert!(TestEdge::read_from(&bucket.tiny_target(1).to_le_bytes()).is_deleted_slot());
        // Tombstone-inclusive positions preserved (slab contract).
        let mut seen = Vec::new();
        graph
            .visit_edges(
                vid,
                label,
                crate::labeled::OutEdgeOrder::Ascending,
                |pos, edge| {
                    seen.push((pos.raw(), u32::from(edge.neighbor_vid())));
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        assert_eq!(seen, vec![(0, 10), (2, 12)]);
        // Inline delete touches no leaf counts (tiny edges occupy no slots);
        // only the global live-edge census decrements (-1).
        assert_eq!(graph.leaf_segment_counts_for_vid(vid).actual, actual_before);
        assert_eq!(graph.edges().header().num_edges, num_before - 1);
        // Survivor moves are always empty (nothing moved — tombstoned in place).
        let removal = graph
            .remove_edge_at_slot_with_move(vid, label, 0)
            .unwrap()
            .expect("removed edge");
        assert_eq!(removal.removed.target, 10);
        assert!(removal.moves.is_empty());
    }

    #[test]
    fn tiny_remove_out_of_range_is_noop() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        crate::labeled::graph::test_support::force_tiny_bucket(&graph, vid, label, &[10]);
        let num_before = graph.edges().header().num_edges;
        assert!(graph.remove_edge_at_slot(vid, label, 5).unwrap().is_none());
        let vertex = graph.vertices().get(vid);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(graph.find_bucket_slot(&vertex, label).unwrap().unwrap())
            .unwrap();
        // Out-of-range delete neither promotes nor mutates.
        assert!(bucket.is_tiny_mode());
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (1, 1));
        assert_eq!(graph.edges().header().num_edges, num_before);
    }

    #[test]
    fn tiny_hole_uses_layout_native_tombstone_predicate() {
        // Delete redesign contract: the hole representation is layout-native
        // (`E::tombstone_edge()` encoded, `E::is_deleted_slot()` for liveness —
        // the same predicate as slab/tree read paths), not a tiny-specific
        // sentinel. Exercises with the high-bit layout (FlagTombstoneEdge):
        // a bit-31 hole must decode as deleted while survivors scan intact.
        use crate::labeled::graph::test_support::FlagTombstoneEdge;
        let graph = crate::labeled::graph::test_support::flag_tombstone_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        for target in [10u32, 11, 12] {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    FlagTombstoneEdge::live(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .remove_edge_at_slot(VertexId::from(0), label, 1)
            .unwrap()
            .expect("delete");
        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.is_tiny_mode());
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (2, 3));
        // The hole decodes as deleted through the layout's own predicate...
        assert!(
            FlagTombstoneEdge::read_from(&bucket.tiny_target(1).to_le_bytes()).is_deleted_slot()
        );
        // ...while survivors scan intact with tombstone-inclusive positions.
        let mut seen = Vec::new();
        graph
            .visit_edges(
                VertexId::from(0),
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |pos, edge| {
                    seen.push((pos.raw(), edge.neighbor_vid()));
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        assert_eq!(
            seen,
            vec![(0, VertexId::from(10)), (2, VertexId::from(12)),]
        );
    }

    #[test]
    fn tiny_remove_matching_finds_inline_target() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        crate::labeled::graph::test_support::force_tiny_bucket(&graph, vid, label, &[10, 11, 12]);
        // Unmatched delete is a no-op without promoting.
        assert!(
            graph
                .remove_edge_matching(vid, label, |edge| edge.neighbor_vid() == VertexId::from(99))
                .unwrap()
                .is_none()
        );
        let vertex = graph.vertices().get(vid);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(graph.find_bucket_slot(&vertex, label).unwrap().unwrap())
            .unwrap();
        assert!(bucket.is_tiny_mode());
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (3, 3));
        let removed = graph
            .remove_edge_matching(vid, label, |edge| edge.neighbor_vid() == VertexId::from(11))
            .unwrap()
            .expect("matched edge");
        assert_eq!(u32::from(removed.neighbor_vid()), 11);
        // Matched delete tombstones inline (no promotion); survivors keep
        // their slots with a hole at the removed ordinal.
        let vertex = graph.vertices().get(vid);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(graph.find_bucket_slot(&vertex, label).unwrap().unwrap())
            .unwrap();
        assert!(bucket.is_tiny_mode());
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (2, 3));
        // Layout-native tombstone: the hole decodes as deleted through the
        // edge layout's own predicate (same as slab/tree read paths).
        assert!(TestEdge::read_from(&bucket.tiny_target(1).to_le_bytes()).is_deleted_slot());
    }
}

#[cfg(test)]
mod g6_zero_read_tests {
    use super::super::test_support::*;
    use super::*;

    /// G6 delete arm: an inline tiny tombstone performs zero edge-slab /
    /// edge-log / inline-property / span reads or writes beyond the descriptor
    /// row. Same harness as the insert/scan proofs.
    ///
    /// Wrong-implementation probe: promote-then-tombstone (the pre-redesign
    /// behavior) reserves a slab span + transcribes + releases — thousands of
    /// bytes — and fails the bound by orders of magnitude.
    #[test]
    fn g6_tiny_delete_reads_no_edge_bytes() {
        let (graph, reads, writes) = counting_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        force_tiny_bucket(&graph, vid, label, &[10, 11, 12]);
        reads.set(0);
        writes.set(0);
        graph
            .remove_edge_at_slot(vid, label, 1)
            .unwrap()
            .expect("removed");
        let r = reads.get();
        let w = writes.get();
        // Exact shape: find reads (descriptor + vertex rows) + one descriptor
        // publish (29B row + census). Matches the scan reads (98) with the
        // insert's publish writes (37) — delete is scan + publish, nothing more.
        // The pre-redesign promote-then-tombstone path would move ~thousands
        // (span reserve + transcribe + release); no room for it here.
        assert_eq!((r, w), (98, 37), "tiny delete byte shape changed");
        // Sanity: hole tombstoned inline, still tiny.
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            BucketSearch::Found { bucket, .. } => bucket,
            BucketSearch::Missing { .. } => panic!("bucket missing"),
        };
        assert!(bucket.is_tiny_mode());
        assert_eq!((bucket.degree(), bucket.stored_slots_raw()), (2, 3));
    }
}
