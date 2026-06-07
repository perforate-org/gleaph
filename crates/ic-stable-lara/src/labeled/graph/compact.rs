//! Labeled graph `compact` implementation.

use crate::{
    SegmentId, VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::{
        edge::{DeleteTarget, EdgeStore, LogEntryKind, decode_log_entry_kind},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
#[cfg(feature = "canbench")]
use canbench_rs::bench_scope;
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{DEFAULT_SEGMENT_SIZE, EdgeSlotMove, LabeledLaraGraph, VertexEdgeSpanCompactOneStep};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn vertex_label_edge_span_end_exclusive(
        vertex: &LabeledVertex,
        first_bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        checked_add_slot_index(first_bucket.edge_start(), u64::from(vertex.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn release_vertex_edge_span_slab(
        &self,
        base: u64,
        len: u64,
    ) -> Result<(), LabeledOperationError> {
        let mut cur_base = base;
        let mut cur_len = len;
        while cur_len > 0 {
            if self.edges.release_span(cur_base, cur_len).is_ok() {
                return Ok(());
            }
            if let Some(free_at_base) = self.edges.free_span_store().free_span_starting_at(cur_base)
            {
                let skip = free_at_base.len.min(cur_len);
                cur_base = cur_base
                    .checked_add(skip)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                cur_len = cur_len
                    .checked_sub(skip)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                continue;
            }
            let mut lo = 1u64;
            let mut hi = cur_len;
            let mut best = 0u64;
            while lo <= hi {
                let mid = lo + (hi - lo) / 2;
                if self.edges.release_span(cur_base, mid).is_ok() {
                    best = mid;
                    lo = mid
                        .checked_add(1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                } else if mid == 0 {
                    break;
                } else {
                    hi = mid
                        .checked_sub(1)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                }
            }
            if best > 0 {
                cur_base = cur_base
                    .checked_add(best)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                cur_len = cur_len
                    .checked_sub(best)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            } else {
                cur_base = cur_base
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                cur_len = cur_len
                    .checked_sub(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
        }
        Ok(())
    }

    /// Returns contiguous physical intervals whose union is the retired VertexEdgeSpan
    /// `[span_start, span_start + span_len)`, including proportional interior slack.
    pub(super) fn vertex_edge_span_retire_intervals(
        span_start: u64,
        span_len: u32,
        buckets: &[LabelBucket],
    ) -> Result<Vec<(u64, u64)>, LabeledOperationError> {
        if span_len == 0 {
            return Ok(Vec::new());
        }
        let span_end = checked_add_slot_index(span_start, u64::from(span_len))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        let mut bucket_intervals: Vec<(u64, u64)> = buckets
            .iter()
            .filter(|bucket| bucket.stored_slots > 0)
            .map(|bucket| (bucket.edge_start(), u64::from(bucket.stored_slots)))
            .collect();
        bucket_intervals.sort_by_key(|(start, _)| *start);

        let mut merged_buckets: Vec<(u64, u64)> = Vec::with_capacity(bucket_intervals.len());
        for (start, len) in bucket_intervals {
            let end = checked_add_slot_index(start, len)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if let Some((last_start, last_len)) = merged_buckets.last_mut() {
                let last_end = checked_add_slot_index(*last_start, *last_len)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if start <= last_end {
                    if end > last_end {
                        *last_len = end
                            .checked_sub(*last_start)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    }
                    continue;
                }
            }
            merged_buckets.push((start, len));
        }

        let mut intervals: Vec<(u64, u64)> = Vec::new();
        let mut cursor = span_start;
        for (bucket_start, bucket_len) in merged_buckets {
            if cursor < bucket_start {
                let gap_len = bucket_start
                    .checked_sub(cursor)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                intervals.push((cursor, gap_len));
            }
            intervals.push((bucket_start, bucket_len));
            cursor = checked_add_slot_index(bucket_start, bucket_len)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .max(cursor);
        }
        if cursor < span_end {
            let tail_len = span_end
                .checked_sub(cursor)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            intervals.push((cursor, tail_len));
        }

        Ok(intervals)
    }

    /// Releases a relocated VertexEdgeSpan footprint back to the edge free-span store.
    ///
    /// Prefer one monolithic `release_span` for the whole `[span_start, span_len)` reservation.
    /// When that fails (typically due to overlap with partial free-span entries), release the
    /// same footprint as bucket ranges plus interior proportional slack intervals.
    pub(super) fn release_vertex_edge_span_footprint(
        &self,
        span_start: u64,
        span_len: u32,
        buckets: &[LabelBucket],
    ) -> Result<(), LabeledOperationError> {
        if span_len == 0 {
            return Ok(());
        }
        let len = u64::from(span_len);
        if self.edges.release_span(span_start, len).is_ok() {
            return Ok(());
        }

        for (start, interval_len) in
            Self::vertex_edge_span_retire_intervals(span_start, span_len, buckets)?
        {
            if interval_len == 0 {
                continue;
            }
            if self.edges.release_span(start, interval_len).is_ok() {
                continue;
            }
            self.release_vertex_edge_span_slab(start, interval_len)?;
        }
        Ok(())
    }

    pub(super) fn rewrite_vertex_edge_span_read_and_plan(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        compact: bool,
        force_slack_grow: bool,
    ) -> Result<(Vec<LabelBucket>, u32, u64, u32, u32, bool, u64, Vec<u64>), LabeledOperationError>
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_rewrite_read_and_plan");
        let buckets = self.read_vertex_label_buckets(vertex)?;
        let old_alloc = vertex.stored_slots;
        let old_base = buckets
            .first()
            .map(|bucket| bucket.edge_start())
            .unwrap_or(0);
        let mut total_live = 0u32;
        for bucket in &buckets {
            total_live = total_live
                .checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
        }

        let min_required = total_live
            .checked_add(preferred_extra)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let mut new_alloc = if compact {
            total_live
        } else if force_slack_grow && old_alloc >= min_required && old_alloc > 0 {
            let doubled_old_alloc = old_alloc
                .checked_mul(2)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let old_plus_segment = old_alloc
                .checked_add(DEFAULT_SEGMENT_SIZE)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            old_alloc
                .max(doubled_old_alloc)
                .max(old_plus_segment)
                .max(min_required)
        } else if old_alloc >= min_required && old_alloc > 0 {
            old_alloc
        } else {
            let doubled_old_alloc = old_alloc
                .checked_mul(2)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            min_required
                .max(DEFAULT_SEGMENT_SIZE)
                .max(doubled_old_alloc)
        };

        if force_slack_grow && !compact && total_live > 0 && vertex.degree() <= 8 {
            let headroom_alloc = ((f64::from(total_live) / 0.85).ceil() as u32).max(min_required);
            new_alloc = new_alloc.max(headroom_alloc);
        }

        let moved = old_alloc == 0 || new_alloc > old_alloc || compact;
        let new_base = if new_alloc == 0 {
            0
        } else if moved {
            if let Some(in_leaf) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)
            {
                in_leaf
            } else {
                // Always append when relocating outside the pinned leaf block.
                let start = self.edges.header().elem_capacity;
                let end =
                    crate::slab_index::checked_add_slot_exclusive_end(start, u64::from(new_alloc))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                self.edges
                    .set_elem_capacity(end)
                    .map_err(LabeledOperationError::from)?;
                start
            }
        } else {
            old_base
        };

        let preferred = preferred_bucket.map(|index| index as usize);
        let positions = Self::calculate_label_edge_span_positions(
            new_base,
            new_alloc,
            buckets.as_slice(),
            preferred,
            preferred_extra,
        )?;
        Ok((
            buckets, old_alloc, old_base, total_live, new_alloc, moved, new_base, positions,
        ))
    }

    pub(super) fn edge_bytes_for_len(edge_count: usize) -> Result<usize, LabeledOperationError> {
        edge_count
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn rewrite_vertex_edge_span(
        &self,
        src: VertexId,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        compact: bool,
        force_slack_grow: bool,
    ) -> Result<(), LabeledOperationError> {
        // Rebuild the VertexEdgeSpan from LabelEdgeSpan live prefixes.
        //
        // The bucket layer is exact-fit, so a new label can appear anywhere in
        // BucketLabelKey order without reserving space in the bucket descriptor. Edge
        // slack is instead distributed inside this VertexEdgeSpan. As in the
        // regular LARA PMA rebalance, row gaps are weighted by `degree + 1`, so a
        // high-degree label receives more spare room than a cold label. The
        // preferred bucket receives `preferred_extra` before proportional gap
        // placement because the caller is about to insert there.
        //
        // Copy strategy: when relocating to a fresh slab span (`moved` with a prior
        // allocation), source and destination are disjoint. If every bucket is
        // slab-only (`overflow_log_head < 0`) and each label's live edges sit in a
        // contiguous slab prefix, use cheap `read_slots_contiguous` / `write_slots_contiguous`.
        // Otherwise collect through `EdgeStore` (slab + overflow log) before writing.
        // In-place layout changes can overlap; the same fast vs full split applies.
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }

        let (buckets, old_alloc, old_base, total_live, new_alloc, moved, new_base, positions) =
            self.rewrite_vertex_edge_span_read_and_plan(
                src,
                &vertex,
                preferred_bucket,
                preferred_extra,
                compact,
                force_slack_grow,
            )?;

        let slab_only_bulk =
            !compact && self.label_buckets_allow_contiguous_slab_copy(&vertex, &buckets)?;

        let needs_value_sync =
            Self::vertex_payload_spans_need_sync_after_rewrite(&buckets, moved, old_alloc, compact);
        let saved_values = if needs_value_sync {
            Some(self.collect_bucket_payloads_before_edge_rewrite(src, &vertex, &buckets)?)
        } else {
            None
        };

        let disjoint_copy = moved && old_alloc > 0 && new_base != old_base;
        if disjoint_copy {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_rewrite_copy_disjoint");
            if slab_only_bulk {
                let max_run = buckets.iter().try_fold(0usize, |max_run, bucket| {
                    Ok::<usize, LabeledOperationError>(
                        max_run.max(Self::edge_bytes_for_len(bucket.degree() as usize)?),
                    )
                })?;
                let mut buf = vec![0u8; max_run];
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = Self::edge_bytes_for_len(bucket.degree() as usize)?;
                    if run > 0 {
                        self.edges
                            .read_slots_contiguous(bucket.edge_start(), &mut buf[..run]);
                        self.edges.write_slots_contiguous(row_start, &buf[..run])?;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, bucket.degree())
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            } else {
                let mut per_bucket: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
                for (index, _) in buckets.iter().enumerate() {
                    let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
                    let bucket_index = index as u32;
                    let successor = self.bucket_successor_start(&vertex, bucket_index)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    per_bucket.push(
                        self.edges
                            .asc_out_edges(&acc, VertexId::from(0))
                            .map_err(LabeledOperationError::from)?,
                    );
                }
                let max_run = per_bucket.iter().try_fold(0usize, |max_run, edges| {
                    Ok::<usize, LabeledOperationError>(
                        max_run.max(Self::edge_bytes_for_len(edges.len())?),
                    )
                })?;
                let mut buf = vec![0u8; max_run];
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let edges = &per_bucket[index];
                    let el = edges.len() as u32;
                    if !edges.is_empty() {
                        let run = Self::edge_bytes_for_len(edges.len())?;
                        debug_assert!(run <= buf.len());
                        let mut o = 0usize;
                        for e in edges {
                            e.write_to(&mut buf[o..o + E::BYTES]);
                            o += E::BYTES;
                        }
                        self.edges.write_slots_contiguous(row_start, &buf[..run])?;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, el)
                            .with_degree_field(el)
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            }
        } else if total_live > 0 {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_rewrite_copy_inplace_vec");
            if slab_only_bulk {
                let run_total = Self::edge_bytes_for_len(
                    usize::try_from(total_live)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                )?;
                let mut raw = vec![0u8; run_total];
                let mut off = 0usize;
                for bucket in &buckets {
                    let run = Self::edge_bytes_for_len(bucket.degree() as usize)?;
                    if run > 0 {
                        let end = off
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .read_slots_contiguous(bucket.edge_start(), &mut raw[off..end]);
                        off = end;
                    }
                }
                off = 0;
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let run = Self::edge_bytes_for_len(bucket.degree() as usize)?;
                    if run > 0 {
                        let end = off
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .write_slots_contiguous(row_start, &raw[off..end])?;
                        off = end;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, bucket.degree())
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            } else {
                let mut per_bucket: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
                for (index, _) in buckets.iter().enumerate() {
                    let slot = Self::labeled_vertex_bucket_slot(&vertex, index as u32)?;
                    let bucket_index = index as u32;
                    let successor = self.bucket_successor_start(&vertex, bucket_index)?;
                    let acc = LabelEdgeSpanAccess::new(&self.buckets, slot, successor, src);
                    per_bucket.push(
                        self.edges
                            .asc_out_edges(&acc, VertexId::from(0))
                            .map_err(LabeledOperationError::from)?,
                    );
                }
                let run_total: usize = per_bucket.iter().try_fold(0usize, |total, edges| {
                    let run = Self::edge_bytes_for_len(edges.len())?;
                    total.checked_add(run).ok_or_else(|| {
                        LabeledOperationError::from(LaraOperationError::CollectAllocationOverflow)
                    })
                })?;
                let mut raw = vec![0u8; run_total];
                let mut pack = 0usize;
                for edges in &per_bucket {
                    for e in edges {
                        e.write_to(&mut raw[pack..pack + E::BYTES]);
                        pack += E::BYTES;
                    }
                }
                pack = 0;
                let mut row_buckets = Vec::with_capacity(buckets.len());
                for (index, bucket) in buckets.iter().enumerate() {
                    let row_start = positions[index];
                    let edges = &per_bucket[index];
                    let run = Self::edge_bytes_for_len(edges.len())?;
                    if run > 0 {
                        let end = pack
                            .checked_add(run)
                            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                        self.edges
                            .write_slots_contiguous(row_start, &raw[pack..end])?;
                        pack = end;
                    }
                    row_buckets.push(
                        bucket
                            .with_edge_range(row_start, edges.len() as u32)
                            .with_degree_field(edges.len() as u32)
                            .with_overflow_log_head(-1),
                    );
                }
                self.buckets
                    .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
            }
        } else {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _bench_scope = bench_scope("labeled_rewrite_metadata_only");
            let mut row_buckets = Vec::with_capacity(buckets.len());
            for (index, bucket) in buckets.iter().enumerate() {
                let row_start = positions[index];
                row_buckets.push(
                    bucket
                        .with_edge_range(row_start, bucket.degree())
                        .with_overflow_log_head(-1),
                );
            }
            self.buckets
                .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
        }

        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _bench_scope = bench_scope("labeled_rewrite_finalize");
        let new_buckets = self.read_vertex_label_buckets(&vertex)?;
        if let Some(saved) = saved_values {
            self.sync_vertex_payload_spans_after_edge_rewrite(src, &buckets, &new_buckets, &saved)?;
        }
        if moved && old_alloc > 0 && new_base != old_base {
            self.release_vertex_edge_span_footprint(old_base, old_alloc, &buckets)?;
        }
        self.vertices.set(src, &vertex.with_stored_slots(new_alloc));

        let d_total = i64::from(new_alloc) - i64::from(old_alloc);
        self.bump_vertex_edge_span_total_delta(src, d_total)?;
        Ok(())
    }

    pub(super) fn vertex_edge_span_slot_moves(
        &self,
        buckets: &[LabelBucket],
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let mut moves = Vec::new();
        for bucket in buckets {
            if bucket.degree() == 0 {
                continue;
            }
            if bucket.overflow_log_head() >= 0 {
                continue;
            }
            let mut next_live = 0u32;
            for old_slot_index in 0..bucket.stored_slots {
                let edge_slot =
                    checked_add_slot_index(bucket.edge_start(), u64::from(old_slot_index))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                if self.edges.read_slot(edge_slot).is_tombstone_edge() {
                    continue;
                }
                if old_slot_index != next_live {
                    moves.push(EdgeSlotMove {
                        label_id: bucket.bucket_label_key(),
                        old_slot_index,
                        new_slot_index: next_live,
                    });
                }
                next_live = next_live
                    .checked_add(1)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            }
            debug_assert_eq!(next_live, bucket.degree());
        }
        Ok(moves)
    }

    pub(super) fn first_edge_slot_move_in_bucket(
        bucket: &LabelBucket,
        edges: &EdgeStore<E, M>,
    ) -> Result<Option<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.degree() == 0 || bucket.overflow_log_head() >= 0 {
            return Ok(None);
        }
        let mut next_live = 0u32;
        for old_slot_index in 0..bucket.stored_slots {
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(old_slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if edges.read_slot(edge_slot).is_tombstone_edge() {
                continue;
            }
            if old_slot_index != next_live {
                return Ok(Some(EdgeSlotMove {
                    label_id: bucket.bucket_label_key(),
                    old_slot_index,
                    new_slot_index: next_live,
                }));
            }
            next_live = next_live
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(None)
    }

    pub(super) fn apply_edge_slot_move_in_bucket(
        &self,
        bucket: &LabelBucket,
        moved: EdgeSlotMove,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let from = checked_add_slot_index(bucket.edge_start(), u64::from(moved.old_slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let to = checked_add_slot_index(bucket.edge_start(), u64::from(moved.new_slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let edge = self.edges.read_slot(from);
        self.edges
            .write_slot(to, edge)
            .map_err(LabeledOperationError::from)?;
        self.edges
            .write_slot(from, E::tombstone_edge())
            .map_err(LabeledOperationError::from)?;
        let width = bucket.payload_byte_width();
        if bucket.is_payload_allocated() {
            let from_off = bucket
                .payload_offset()
                .checked_add(u64::from(moved.old_slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let to_off = bucket
                .payload_offset()
                .checked_add(u64::from(moved.new_slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut buf = vec![0u8; usize::from(width)];
            self.values.read_bytes(from_off, &mut buf);
            self.values
                .write_payload_slot(to_off, width, &buf)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(())
    }

    pub(super) fn finalize_bucket_slab_metadata(bucket: LabelBucket) -> LabelBucket {
        bucket
            .with_stored_slots(bucket.degree())
            .with_overflow_log_head(-1)
    }

    pub(crate) fn compact_vertex_edge_span_one_step(
        &self,
        vid: VertexId,
        resume_bucket_index: u32,
    ) -> Result<VertexEdgeSpanCompactOneStep, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.ensure_vertex(vid)?;
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(VertexEdgeSpanCompactOneStep::Finished);
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let mut total_live = 0u32;
        for bucket in &buckets {
            total_live = total_live
                .checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
        }
        if resume_bucket_index == 0 && buckets.iter().any(|b| b.overflow_log_head() >= 0) {
            self.reclaim_vertex_overflow_buckets(vid)?;
            return self.compact_vertex_edge_span_one_step(vid, 0);
        }
        if resume_bucket_index >= vertex.degree() {
            // Per-bucket steps may already pack each label row (`stored_slots == degree`) while
            // the vertex-wide VertexEdgeSpan width (`vertex.stored_slots`) stays oversized.
            if vertex.stored_slots > total_live
                || buckets
                    .iter()
                    .any(|b| b.overflow_log_head() >= 0 || b.stored_slots != b.degree())
            {
                self.rewrite_vertex_edge_span(vid, None, 0, true, false)?;
            }
            return Ok(VertexEdgeSpanCompactOneStep::Finished);
        }
        let bucket_index = resume_bucket_index;
        let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        let bucket = self
            .buckets
            .read_label_bucket_slot(slot)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if let Some(moved) = Self::first_edge_slot_move_in_bucket(&bucket, &self.edges)? {
            self.apply_edge_slot_move_in_bucket(&bucket, moved)?;
            return Ok(VertexEdgeSpanCompactOneStep::EdgeMoved(moved));
        }
        let finalized = Self::finalize_bucket_slab_metadata(bucket);
        if finalized != bucket {
            self.buckets.write_label_bucket_slot(slot, finalized)?;
        }
        Ok(VertexEdgeSpanCompactOneStep::AdvanceBucket(
            bucket_index.saturating_add(1),
        ))
    }

    pub(super) fn calculate_label_edge_span_positions(
        start_slot: u64,
        span_slots: u32,
        buckets: &[LabelBucket],
        preferred: Option<usize>,
        preferred_extra: u32,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        let mut out = Vec::with_capacity(buckets.len());
        if buckets.is_empty() {
            return Ok(out);
        }

        let mut effective_live = 0u64;
        let mut total_weight = buckets.len() as u64;
        for (index, bucket) in buckets.iter().enumerate() {
            let extra = if preferred == Some(index) {
                preferred_extra
            } else {
                0
            };
            let degree = u64::from(
                bucket
                    .degree()
                    .checked_add(extra)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?,
            );
            effective_live = effective_live
                .checked_add(degree)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            total_weight = total_weight
                .checked_add(degree)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        let gaps = u64::from(span_slots)
            .checked_sub(effective_live)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        // Same layout as the historical `f64` implementation: one fixed-point step
        // `floor((gaps/total_weight) * 1e8) / 1e8` per bucket weight `(deg+1)`.
        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                / tw
        };

        let mut cursor_fp = u128::from(start_slot)
            .checked_mul(P)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        for (index, bucket) in buckets.iter().enumerate() {
            let start = u64::try_from(cursor_fp / P)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            out.push(start);
            let extra = if preferred == Some(index) {
                preferred_extra
            } else {
                0
            };
            let deg = u128::from(
                bucket
                    .degree()
                    .checked_add(extra)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?,
            );
            let start_fp = u128::from(start)
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            cursor_fp = start_fp
                .checked_add(
                    deg.checked_mul(P)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                )
                .and_then(|cursor| {
                    let weight = deg.checked_add(1)?;
                    let gap = step_fp.checked_mul(weight)?;
                    cursor.checked_add(gap)
                })
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(out)
    }

    pub(super) fn calculate_label_edge_span_positions_by_resident_slots(
        start_slot: u64,
        span_slots: u32,
        buckets: &[LabelBucket],
        preferred: Option<usize>,
        preferred_extra: u32,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        let mut out = Vec::with_capacity(buckets.len());
        if buckets.is_empty() {
            return Ok(out);
        }

        let mut effective_live = 0u64;
        let mut total_weight = buckets.len() as u64;
        for (index, bucket) in buckets.iter().enumerate() {
            let extra = if preferred == Some(index) {
                preferred_extra
            } else {
                0
            };
            let resident = bucket
                .stored_slots
                .max(bucket.degree())
                .checked_add(extra)
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
            effective_live = effective_live
                .checked_add(u64::from(resident))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            total_weight = total_weight
                .checked_add(u64::from(bucket.degree()).saturating_add(1))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        let gaps = u64::from(span_slots)
            .checked_sub(effective_live)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                / tw
        };

        let mut cursor_fp = u128::from(start_slot)
            .checked_mul(P)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        for (index, bucket) in buckets.iter().enumerate() {
            let start = u64::try_from(cursor_fp / P)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            out.push(start);
            let extra = if preferred == Some(index) {
                preferred_extra
            } else {
                0
            };
            let resident = u128::from(
                bucket
                    .stored_slots
                    .max(bucket.degree())
                    .checked_add(extra)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?,
            );
            let start_fp = u128::from(start)
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            cursor_fp = start_fp
                .checked_add(
                    resident
                        .checked_mul(P)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                )
                .and_then(|cursor| {
                    let weight = u128::from(bucket.degree()).checked_add(1)?;
                    let gap = step_fp.checked_mul(weight)?;
                    cursor.checked_add(gap)
                })
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(out)
    }

    pub(super) fn rebalance_vertex_edge_span(
        &self,
        src: VertexId,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        force_slack_grow: bool,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let old_alloc = vertex.stored_slots;
        let old_base = buckets
            .first()
            .map(|bucket| bucket.edge_start())
            .unwrap_or(0);
        let mut resident_slots = 0u32;
        for bucket in &buckets {
            resident_slots = resident_slots
                .checked_add(bucket.stored_slots.max(bucket.degree()))
                .ok_or(LaraOperationError::RowDegreeOverflow)?;
        }
        let min_required = resident_slots
            .checked_add(preferred_extra)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let new_alloc = if force_slack_grow && old_alloc >= min_required && old_alloc > 0 {
            let base = old_alloc.max(min_required);
            let gap = DEFAULT_SEGMENT_SIZE.max(base / 8);
            base.saturating_add(gap)
        } else if old_alloc >= min_required && old_alloc > 0 {
            old_alloc
        } else {
            let base = min_required.max(DEFAULT_SEGMENT_SIZE);
            let gap = DEFAULT_SEGMENT_SIZE.max(base / 8);
            base.saturating_add(gap)
        };
        let moved = old_alloc == 0 || new_alloc > old_alloc;
        let new_base = if new_alloc == 0 {
            0
        } else if moved {
            if let Some(in_leaf) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)
            {
                in_leaf
            } else {
                let start = self.edges.header().elem_capacity;
                let end = match checked_add_slot_index(start, u64::from(new_alloc)) {
                    Some(end) => end,
                    None => return Err(LaraOperationError::CollectAllocationOverflow.into()),
                };
                self.edges
                    .set_elem_capacity(end)
                    .map_err(LabeledOperationError::from)?;
                start
            }
        } else {
            old_base
        };
        let preferred = preferred_bucket.map(|index| index as usize);
        let positions = Self::calculate_label_edge_span_positions_by_resident_slots(
            new_base,
            new_alloc,
            &buckets,
            preferred,
            preferred_extra,
        )?;

        let max_run = buckets.iter().try_fold(0usize, |max_run, bucket| {
            Ok::<usize, LabeledOperationError>(
                max_run.max(Self::edge_bytes_for_len(bucket.stored_slots as usize)?),
            )
        })?;
        let mut buf = vec![0u8; max_run];
        let mut row_buckets = Vec::with_capacity(buckets.len());
        for (index, bucket) in buckets.iter().enumerate() {
            let row_start = positions[index];
            let run = Self::edge_bytes_for_len(bucket.stored_slots as usize)?;
            if run > 0 {
                self.edges
                    .read_slots_contiguous(bucket.edge_start(), &mut buf[..run]);
                self.edges.write_slots_contiguous(row_start, &buf[..run])?;
            }
            row_buckets.push(bucket.with_edge_range(row_start, bucket.stored_slots));
        }
        self.buckets
            .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
        if moved && old_alloc > 0 && new_base != old_base {
            self.release_vertex_edge_span_footprint(old_base, old_alloc, &buckets)?;
        }
        self.vertices.set(src, &vertex.with_stored_slots(new_alloc));

        let d_total = i64::from(new_alloc) - i64::from(old_alloc);
        self.bump_vertex_edge_span_total_delta(src, d_total)?;
        Ok(())
    }

    /// Compacts all edge buckets for `vid` into slab-backed spans.
    pub fn compact_vertex_edge_span(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.compact_vertex_edge_span_with_moves(vid, bucket_index)
            .map(|_| ())
    }

    /// Compacts all edge buckets for `vid` and returns the slab-slot moves performed.
    pub fn compact_vertex_edge_span_with_moves(
        &self,
        vid: VertexId,
        bucket_index: u32,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let _ = bucket_index;
        let mut moves = Vec::new();
        let mut resume = 0u32;
        loop {
            match self.compact_vertex_edge_span_one_step(vid, resume)? {
                VertexEdgeSpanCompactOneStep::EdgeMoved(moved) => {
                    moves.push(moved);
                }
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => {
                    resume = next;
                }
                VertexEdgeSpanCompactOneStep::OverflowRewrite(batch) => {
                    moves.extend(batch);
                    break;
                }
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        Ok(moves)
    }

    /// `true` when any label bucket on `vid` has enough slab tombstones to benefit from
    /// deferred edge-span compaction.
    pub(crate) fn vertex_has_slab_tombstone_slack_pressure(
        &self,
        vid: VertexId,
    ) -> Result<bool, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(false);
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        Ok(buckets.iter().any(|bucket| {
            bucket.stored_slots.saturating_sub(bucket.degree()) >= DEFAULT_SEGMENT_SIZE
        }))
    }

    pub(super) fn maintain_vertex_edge_span_light(
        &self,
        vid: VertexId,
        allow_slack_grow: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        self.compact_vertex_edge_span(vid, 0)?;
        if !allow_slack_grow {
            return Ok(());
        }
        let vertex = self.vertices.get(vid);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let total_live = buckets.iter().try_fold(0u32, |acc, bucket| {
            acc.checked_add(bucket.degree())
                .ok_or(LaraOperationError::RowDegreeOverflow)
        })?;
        if vertex.stored_slots > total_live.saturating_add(1) {
            self.rewrite_vertex_edge_span(vid, None, 0, false, true)?;
        }
        Ok(())
    }

    pub(super) fn rebalance_vertex_edge_span_light(
        &self,
        vid: VertexId,
        allow_slack_grow: bool,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        if allow_slack_grow {
            self.rebalance_vertex_edge_span(vid, None, 0, true)
        } else {
            self.rebalance_vertex_edge_span(vid, None, 0, false)
        }
    }

    pub(super) fn fold_label_bucket_to_slab(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.overflow_log_head() < 0 {
            return Ok(bucket);
        }
        let degree = bucket.degree();
        if degree == 0 {
            return Ok(bucket.with_overflow_log_head(-1));
        }
        let successor = self.bucket_successor_start(vertex, bucket_index)?;
        let slack = successor.saturating_sub(bucket.edge_start());
        if slack < u64::from(degree) {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let acc = LabelEdgeSpanAccess::new(&self.buckets, bucket_slot, successor, src);
        let edges = self
            .edges
            .asc_out_edges(&acc, VertexId::from(0))
            .map_err(LabeledOperationError::from)?;
        let run = Self::edge_bytes_for_len(edges.len())?;
        if run > 0 {
            let mut buf = vec![0u8; run];
            let mut offset = 0usize;
            for edge in &edges {
                edge.write_to(&mut buf[offset..offset + E::BYTES]);
                offset += E::BYTES;
            }
            self.edges
                .write_slots_contiguous(bucket.edge_start(), &buf[..run])?;
        }
        let had_payload_log = bucket.payload_log_head() >= 0;
        if had_payload_log {
            let leaf = self.payload_log_leaf(src);
            let (_, payload_chain) = self.bucket_log_chains(src, &bucket);
            self.values.sweep_payload_log_chain(leaf, &payload_chain);
        }
        let saved = if bucket.is_payload_allocated() && bucket.payload_byte_width() > 0 {
            if had_payload_log {
                Some(self.collect_bucket_payloads_asc_order(src, vertex, bucket_index, &bucket)?)
            } else {
                self.read_bucket_payloads_slab_dense(&bucket)
            }
        } else {
            None
        };
        let mut bucket = bucket
            .with_overflow_log_head(-1)
            .with_stored_slots(degree)
            .with_degree_field(degree);
        if had_payload_log {
            bucket = bucket
                .try_with_payload_log_head(-1)
                .map_err(LabeledOperationError::from)?;
        }
        if let Some(saved) = saved {
            let width = bucket.payload_byte_width();
            let flat_len = saved
                .len()
                .checked_mul(usize::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut flat = Vec::with_capacity(flat_len);
            for bytes in &saved {
                flat.extend_from_slice(bytes);
            }
            self.values
                .write_bytes(bucket.payload_offset(), &flat)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(bucket)
    }

    pub(super) fn fold_label_bucket_edges_to_slab(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        _bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.overflow_log_head() < 0 {
            return Ok(bucket);
        }
        let degree = bucket.degree();
        if degree == 0 {
            return Ok(bucket.with_overflow_log_head(-1));
        }
        let edges = self.materialize_label_bucket_edges_for_log_release(src, &bucket)?;
        let successor = self.bucket_successor_start(vertex, bucket_index)?;
        let slack = successor.saturating_sub(bucket.edge_start());
        if slack < edges.len() as u64 {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let run = Self::edge_bytes_for_len(edges.len())?;
        if run > 0 {
            let mut buf = vec![0u8; run];
            let mut offset = 0usize;
            for edge in &edges {
                edge.write_to(&mut buf[offset..offset + E::BYTES]);
                offset += E::BYTES;
            }
            self.edges
                .write_slots_contiguous(bucket.edge_start(), &buf[..run])?;
        }
        let live = u32::try_from(
            edges
                .iter()
                .filter(|edge| !edge.is_tombstone_edge())
                .count(),
        )
        .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
        Ok(bucket
            .with_overflow_log_head(-1)
            .with_stored_slots(
                u32::try_from(edges.len()).map_err(|_| LaraOperationError::RowDegreeOverflow)?,
            )
            .with_degree_field(live))
    }

    fn materialize_label_bucket_edges_for_log_release(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Result<Vec<E>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let leaf = u32::from(src) / self.edges.header().segment_size.max(1);
        let mut out: Vec<(DeleteTarget, E)> = Vec::new();
        let slab_prefix_slots = self.bucket_slab_prefix_slots(src, bucket);
        for slot_index in 0..slab_prefix_slots {
            let edge_slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.edges.read_slot(edge_slot);
            out.push((DeleteTarget::Slab(slot_index), edge));
        }
        let slab_live = out
            .iter()
            .filter(|(_, edge)| !edge.is_deleted_slot() && !edge.is_tombstone_edge())
            .count();
        let expected_live = usize::try_from(bucket.degree()).unwrap_or(usize::MAX);
        if slab_live == expected_live {
            return Ok(out.into_iter().map(|(_, edge)| edge).collect());
        }
        if slab_live > expected_live {
            return Err(LabeledOperationError::from(
                LaraOperationError::LogChainShort,
            ));
        }
        let chain = self
            .edges
            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
        for log_idx in chain {
            let (_, src_tag, edge) = self.edges.read_overflow_log_entry(leaf, log_idx);
            match decode_log_entry_kind(src_tag) {
                LogEntryKind::Dead => {
                    out.push((DeleteTarget::Log(log_idx), E::tombstone_edge()));
                }
                LogEntryKind::Delete(target) => {
                    if let Some(index) = out.iter().position(|(candidate, _)| *candidate == target)
                    {
                        out[index].1 = E::tombstone_edge();
                    }
                }
                LogEntryKind::Live => out.push((DeleteTarget::Log(log_idx), edge)),
            }
        }
        let live = out
            .iter()
            .filter(|(_, edge)| !edge.is_tombstone_edge())
            .count();
        if live != usize::try_from(bucket.degree()).unwrap_or(usize::MAX) {
            return Err(LabeledOperationError::from(
                LaraOperationError::LogChainShort,
            ));
        }
        Ok(out.into_iter().map(|(_, edge)| edge).collect())
    }

    pub(super) fn fold_label_bucket_payload_log_to_slab(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.payload_log_head() < 0 || !bucket.is_payload_allocated() {
            return Ok(bucket);
        }
        let leaf = self.payload_log_leaf(src);
        let (_, payload_chain) = self.bucket_log_chains(src, &bucket);
        let saved =
            self.collect_bucket_payload_slots_asc_order(src, vertex, bucket_index, &bucket)?;
        let old_payload_slots = self.bucket_resident_payload_slots_for(src, &bucket);
        let mut bucket = bucket
            .try_with_payload_log_head(-1)
            .map_err(LabeledOperationError::from)?;
        bucket = self.ensure_bucket_payload_span(src, bucket_slot, bucket, old_payload_slots)?;
        let width = bucket.payload_byte_width();
        for (slot_index, bytes) in &saved {
            let offset = bucket
                .payload_offset()
                .checked_add(u64::from(*slot_index) * u64::from(width))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.values
                .write_payload_slot(offset, width, bytes)
                .map_err(LabeledOperationError::from)?;
        }
        self.values.sweep_payload_log_chain(leaf, &payload_chain);
        self.buckets.write_label_bucket_slot(bucket_slot, bucket)?;
        Ok(bucket)
    }

    pub(super) fn reclaim_vertex_overflow_buckets(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let mut needs_rewrite = false;
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            let bucket_index = bucket_index as u32;
            let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            if bucket.overflow_log_head() >= 0 {
                match self.fold_label_bucket_to_slab(vid, &vertex, bucket_index, slot, *bucket) {
                    Ok(folded) => {
                        self.buckets.write_label_bucket_slot(slot, folded)?;
                    }
                    Err(_) => {
                        needs_rewrite = true;
                        break;
                    }
                }
            } else if bucket.payload_log_head() >= 0 {
                self.fold_label_bucket_payload_log_to_slab(
                    vid,
                    &vertex,
                    bucket_index,
                    slot,
                    *bucket,
                )?;
            }
        }
        if needs_rewrite {
            self.rewrite_vertex_edge_span(vid, None, 0, true, false)?;
        }
        Ok(())
    }

    pub(super) fn reclaim_edge_log_leaf_for_labeled(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let seg_size = self.edges.header().segment_size.max(1);
        let leaf = u32::from(src) / seg_size;
        let start_vid = leaf.saturating_mul(seg_size);
        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            self.reclaim_vertex_overflow_buckets(VertexId::from(vid_u))?;
        }
        self.edges
            .release_log_segment(SegmentId::from(leaf))
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }

    pub(super) fn rebalance_edge_log_vertex_for_labeled(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.degree() == 0 || vertex.is_default_edge_labeled() {
            return Ok(());
        }
        self.rebalance_vertex_edge_span_light(vid, true)?;
        let vertex = self.vertices.get(vid);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            if bucket.overflow_log_head() < 0 {
                continue;
            }
            let bucket_index = bucket_index as u32;
            let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            let folded =
                self.fold_label_bucket_edges_to_slab(vid, &vertex, bucket_index, slot, *bucket)?;
            self.buckets.write_label_bucket_slot(slot, folded)?;
        }
        Ok(())
    }

    pub(super) fn rebalance_edge_log_leaf_for_labeled(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let seg_size = self.edges.header().segment_size.max(1);
        let leaf = u32::from(src) / seg_size;
        let start_vid = leaf.saturating_mul(seg_size);
        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            self.rebalance_edge_log_vertex_for_labeled(VertexId::from(vid_u))?;
        }
        self.edges
            .release_log_segment(SegmentId::from(leaf))
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }

    pub(super) fn rebalance_payload_log_leaf_for_labeled(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let seg_size = self.edges.header().segment_size.max(1);
        let leaf = u32::from(src) / seg_size;
        let start_vid = leaf.saturating_mul(seg_size);
        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            let vertex = self.vertices.get(vid);
            if vertex.degree() == 0 || vertex.is_default_edge_labeled() {
                continue;
            }
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            for (bucket_index, bucket) in buckets.iter().enumerate() {
                if bucket.payload_log_head() < 0 {
                    continue;
                }
                let bucket_index = bucket_index as u32;
                let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
                self.fold_label_bucket_payload_log_to_slab(
                    vid,
                    &vertex,
                    bucket_index,
                    slot,
                    *bucket,
                )?;
            }
        }
        self.values
            .release_payload_log_segment(leaf)
            .map_err(LabeledOperationError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use crate::VertexId;

    #[test]
    fn homogeneous_bypass_append_rejects_degree_overflow() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_degree(u32::MAX),
        );

        let err = graph
            .insert_edge(hub, default, TestEdge { target: 1 })
            .expect_err("max degree must be rejected");
        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
        assert_eq!(graph.vertices().get(hub).degree(), u32::MAX);
    }

    #[test]
    fn homogeneous_bypass_appends_stay_slab_only_without_overflow_log() {
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
            256,
            default,
        )
        .unwrap();
        let hub = graph
            .push_vertex(LabeledVertex::default().with_homogeneous_bypass_label(default))
            .unwrap();

        for target in 0..8u32 {
            graph
                .insert_edge(hub, default, TestEdge { target })
                .unwrap();
        }

        let vertex = graph.vertices().get(hub);
        assert_eq!(vertex.bypass_overflow_log_head(), -1);
        assert_eq!(vertex.degree(), 8);
        assert_eq!(graph.iter_edges_for_label(hub, default).unwrap().len(), 8);
    }

    #[test]
    fn push_vertex_rejects_normal_row_with_label_bucket_overflow() {
        let graph = test_graph();
        let vertex = LabeledVertex::default().with_degree(MAX_VERTEX_LABEL_BUCKETS + 1);
        let err = graph
            .push_vertex(vertex)
            .expect_err("push must reject normal rows over MAX_VERTEX_LABEL_BUCKETS");
        assert!(matches!(
            err,
            LabeledOperationError::InvalidVertexRow(
                LabeledVertexFieldError::LabelBucketCountOverflow
            )
        ));
    }

    #[test]
    fn homogeneous_bypass_region_end_rejects_slot_overflow() {
        let default = BucketLabelKey::from_raw(7);
        let graph = test_graph_with_default(default);
        let hub = VertexId::from(0);
        graph.vertices().set(
            hub,
            &LabeledVertex::default()
                .with_homogeneous_bypass_label(default)
                .with_base_slot_start(crate::labeled::slot_index::SLOT_INDEX_MASK)
                .with_degree(1)
                .with_stored_slots(1),
        );

        let err = graph
            .bypass_region_end(hub)
            .expect_err("bypass end overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn bucket_vertex_prefix_end_rejects_slot_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(
                    BucketLabelKey::from_raw(42),
                    crate::labeled::slot_index::SLOT_INDEX_MASK,
                    1,
                    1,
                    -1,
                ),
            )
            .unwrap();
        graph
            .vertices()
            .set(hub, &graph.vertices().get(hub).with_stored_slots(1));

        let err = graph
            .vertex_prefix_end(hub)
            .expect_err("bucket prefix end overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn last_bucket_successor_rejects_span_end_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        let label = BucketLabelKey::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(
                    label,
                    crate::labeled::slot_index::SLOT_INDEX_MASK,
                    0,
                    0,
                    -1,
                ),
            )
            .unwrap();
        graph
            .vertices()
            .set(hub, &graph.vertices().get(hub).with_stored_slots(1));

        let vertex = graph.vertices().get(hub);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        let err = graph
            .bucket_successor_start_after_bucket(&vertex, 0, &bucket)
            .expect_err("last bucket successor overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn contiguous_tiled_out_edges_rejects_span_end_overflow() {
        let vertex = LabeledVertex::default()
            .with_bucket_row(0, 1)
            .with_stored_slots(1);
        let buckets = [LabelBucket::from_parts(
            BucketLabelKey::from_raw(42),
            crate::labeled::slot_index::SLOT_INDEX_MASK,
            0,
            0,
            -1,
        )];

        assert_eq!(
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::try_contiguous_tiled_labeled_out_edges(
                &vertex,
                &buckets,
            ),
            None
        );
    }

    #[test]
    fn normal_label_bucket_insert_rejects_edge_len_overflow() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let hub = VertexId::from(0);
        let label = BucketLabelKey::from_raw(42);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(label, 0, u32::MAX, u32::MAX, -1),
            )
            .unwrap();

        let err = graph
            .insert_edge(hub, label, TestEdge { target: 1 })
            .expect_err("max bucket edge_len must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
        let vertex = graph.vertices().get(hub);
        let bucket = graph
            .buckets()
            .read_label_bucket_slot(vertex.base_slot_start())
            .unwrap();
        assert_eq!(bucket.stored_slots, u32::MAX);
    }

    #[test]
    fn vertex_edge_span_rewrite_rejects_total_live_overflow() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(BucketLabelKey::from_raw(10), 0, u32::MAX, u32::MAX, -1),
            )
            .unwrap();
        graph
            .buckets()
            .insert_label_bucket(
                graph.vertices(),
                hub,
                LabelBucket::from_parts(BucketLabelKey::from_raw(20), 0, 1, 1, -1),
            )
            .unwrap();

        let err = graph
            .compact_vertex_edge_span(hub, 0)
            .expect_err("bucket edge_len sum overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::RowDegreeOverflow)
        ));
    }

    #[test]
    fn rewrite_copy_byte_len_rejects_usize_overflow() {
        let oversized = usize::MAX / TestEdge::BYTES + 1;
        let err = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::edge_bytes_for_len(oversized)
            .expect_err("edge byte length overflow must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }

    #[test]
    fn two_label_hub_500_then_173_parallel_edges() {
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
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let a = BucketLabelKey::from_raw(10_000);
        let b = BucketLabelKey::from_raw(10_001);
        for edge_i in 0..500u32 {
            graph
                .insert_edge(
                    hub,
                    a,
                    TestEdge {
                        target: u32::from(dst),
                    },
                )
                .unwrap_or_else(|e| panic!("label_a edge_i={edge_i}: {e:?}"));
        }
        for edge_i in 0..174u32 {
            graph
                .insert_edge(
                    hub,
                    b,
                    TestEdge {
                        target: u32::from(dst),
                    },
                )
                .unwrap_or_else(|e| panic!("label_b edge_i={edge_i}: {e:?}"));
        }
    }

    #[test]
    fn mixed_label_hub_20_labels_500_edges_each() {
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
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..20u16 {
            let label = BucketLabelKey::from_raw(10_000 + label_idx);
            for edge_i in 0..500u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
    }

    #[test]
    fn vertex_edge_span_retire_intervals_cover_interior_gaps_and_tail() {
        use crate::labeled::record::LabelBucket;
        let buckets = [
            LabelBucket::default()
                .with_bucket_label_key(BucketLabelKey::from_raw(1))
                .with_edge_range(100, 10),
            LabelBucket::default()
                .with_bucket_label_key(BucketLabelKey::from_raw(2))
                .with_edge_range(150, 20),
        ];
        let intervals =
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::vertex_edge_span_retire_intervals(
                100, 100, &buckets,
            )
            .unwrap();
        let total: u64 = intervals.iter().map(|(_, len)| *len).sum();
        assert_eq!(total, 100);
        assert!(intervals.contains(&(110, 40)));
        assert!(intervals.contains(&(170, 30)));
    }

    /// Regression: 33rd label on a dense hub used to spend minutes in span release.
    #[test]
    fn mixed_label_hub_33_labels_span_release_regression() {
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
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..33u16 {
            let label = BucketLabelKey::from_raw(10_000 + label_idx);
            for edge_i in 0..50u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
    }

    #[test]
    fn mixed_label_hub_parallel_edges_do_not_corrupt_overflow_log() {
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
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..10u16 {
            let label = BucketLabelKey::from_raw(1000 + label_idx);
            for edge_i in 0..24u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(dst),
                        },
                    )
                    .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
            }
        }
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn find_bucket_resolves_non_default_label_after_parallel_inserts() {
        let graph = test_graph();
        let label = BucketLabelKey::from_raw(42);
        for target in 0..8u32 {
            graph
                .insert_edge(VertexId::from(0), label, TestEdge { target })
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert!(graph.find_bucket_slot(&vertex, label).unwrap().is_some());
    }

    #[test]
    fn parallel_catalog_edges_on_high_index_vertex_stay_on_slab() {
        let graph = test_graph();
        for _ in 0..64 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(42);
        for target in 0..240u32 {
            graph.insert_edge(hub, label, TestEdge { target }).unwrap();
        }
        let vertex = graph.vertices().get(hub);
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(graph.iter_edges_for_label(hub, label).unwrap().len(), 240);
    }

    #[test]
    fn catalog_label_parallel_inserts_use_single_bucket_row() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(42);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 0 })
            .unwrap();
        let bucket_only = graph.vertices().get(VertexId::from(0));
        assert!(!bucket_only.is_default_edge_labeled());
        for target in 1..24u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            24
        );
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            vec![road]
        );
    }

    #[test]
    fn incremental_vertex_edge_span_compact_clears_many_tombstones() {
        use super::VertexEdgeSpanCompactOneStep;

        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        for target in 1..=60u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in 1..=55u32 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket_before = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(
            bucket_before
                .stored_slots
                .saturating_sub(bucket_before.degree),
            55
        );
        assert_eq!(bucket_before.degree(), 5);

        let mut resume = 0u32;
        let mut edge_moves = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume)
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => edge_moves += 1,
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => break,
                VertexEdgeSpanCompactOneStep::Finished => break,
            }
        }
        let bucket_after = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(
            edge_moves > 0 || bucket_after.stored_slots == bucket_after.degree,
            "expected in-bucket compaction progress"
        );
        assert_eq!(bucket_after.stored_slots, bucket_after.degree);
        assert_eq!(bucket_after.degree, 5);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            (56..=60)
                .rev()
                .map(|target| TestEdge { target })
                .collect::<Vec<_>>()
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn incremental_vertex_edge_span_compact_preserves_other_label_bucket() {
        use super::VertexEdgeSpanCompactOneStep;

        let graph = test_graph();
        let anchor = BucketLabelKey::from_raw(99);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), anchor, TestEdge { target: 999 })
            .unwrap();
        for target in 1..=20u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in 1..=17u32 {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap();
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let anchor_slot = graph.find_bucket_slot(&vertex, anchor).unwrap().unwrap();
        let road_slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        assert_eq!(
            road_bucket.stored_slots.saturating_sub(road_bucket.degree),
            17
        );

        let mut resume = 0u32;
        loop {
            match graph
                .compact_vertex_edge_span_one_step(VertexId::from(0), resume)
                .unwrap()
            {
                VertexEdgeSpanCompactOneStep::EdgeMoved(_) => {}
                VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
                VertexEdgeSpanCompactOneStep::OverflowRewrite(_)
                | VertexEdgeSpanCompactOneStep::Finished => {
                    break;
                }
            }
        }

        let anchor_bucket = graph.buckets().read_label_bucket_slot(anchor_slot).unwrap();
        assert_eq!(anchor_bucket.stored_slots, anchor_bucket.degree);
        assert_eq!(anchor_bucket.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), anchor)
                .unwrap(),
            vec![TestEdge { target: 999 }]
        );

        let road_bucket = graph.buckets().read_label_bucket_slot(road_slot).unwrap();
        assert_eq!(road_bucket.stored_slots, road_bucket.degree);
        assert_eq!(road_bucket.degree, 3);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![
                TestEdge { target: 20 },
                TestEdge { target: 19 },
                TestEdge { target: 18 },
            ]
        );
    }

    #[test]
    fn label_bucket_accumulates_many_slab_tombstones_before_compact() {
        let graph = test_graph();
        let road = BucketLabelKey::from_raw(2);
        let total = 202u32;
        for target in 1..=total {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }

        for target in 1..=200 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots.saturating_sub(bucket.degree), 200);
        assert_eq!(bucket.degree(), 2);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, bucket.degree);
        assert_eq!(bucket.degree, 2);
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 202 }, TestEdge { target: 201 }]
        );
    }

    #[test]
    fn label_bucket_store_vertex_segment_compacts_across_thirty_two_vertices() {
        let graph = test_graph();
        for _ in 1..32 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 10 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(31),
                BucketLabelKey::from_raw(3),
                TestEdge { target: 20 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(4),
                TestEdge { target: 30 },
            )
            .unwrap();

        let first = graph.vertices().get(VertexId::from(0));
        let last = graph.vertices().get(VertexId::from(31));
        assert_eq!(first.degree(), 2);
        assert_eq!(last.degree(), 1);
        assert!(
            last.base_slot_start()
                >= first.base_slot_start().saturating_add(u64::from(
                    first
                        .label_bucket_descriptor_span()
                        .expect("hub vertex is in bucket mode"),
                ))
        );
        assert_eq!(
            graph.out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge { target: 30 }, TestEdge { target: 10 }]
        );
        assert_eq!(
            graph.out_edges(VertexId::from(31)).unwrap(),
            vec![TestEdge { target: 20 }]
        );
    }

    #[test]
    fn vertex_edge_span_rewrite_weights_slack_by_label_degree() {
        let graph = test_graph();
        let hot = BucketLabelKey::from_raw(2);
        let cold = BucketLabelKey::from_raw(3);
        for target in 0..64u32 {
            graph
                .insert_edge(VertexId::from(0), hot, TestEdge { target })
                .unwrap();
        }
        graph
            .insert_edge(VertexId::from(0), cold, TestEdge { target: 900 })
            .unwrap();

        let vertex = graph.vertices().get(VertexId::from(0));
        let hot_slot = graph.find_bucket_slot(&vertex, hot).unwrap().unwrap();
        let cold_slot = graph.find_bucket_slot(&vertex, cold).unwrap().unwrap();
        let hot_index = hot_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let cold_index = cold_slot.saturating_sub(vertex.base_slot_start()) as u32;
        let hot_bucket = graph.buckets().read_label_bucket_slot(hot_slot).unwrap();
        let cold_bucket = graph.buckets().read_label_bucket_slot(cold_slot).unwrap();
        let hot_capacity = graph
            .bucket_successor_start(&vertex, hot_index)
            .unwrap()
            .saturating_sub(hot_bucket.edge_start());
        let cold_capacity = graph
            .bucket_successor_start(&vertex, cold_index)
            .unwrap()
            .saturating_sub(cold_bucket.edge_start());

        assert!(hot_capacity > cold_capacity);
        assert!(hot_capacity > u64::from(hot_bucket.stored_slots));
        assert!(cold_capacity >= u64::from(cold_bucket.stored_slots));
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn compact_vertex_edge_span_shrinks_vertex_edge_span() {
        let graph = test_graph();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let road = BucketLabelKey::from_raw(2);
        for target in 0..80u32 {
            graph
                .insert_edge(VertexId::from(0), road, TestEdge { target })
                .unwrap();
        }
        for target in 0..72u32 {
            graph
                .remove_edge_matching(VertexId::from(0), road, |edge| edge.target == target)
                .unwrap();
        }

        let before = graph.vertices().get(VertexId::from(0));
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            8
        );
        assert!(before.stored_slots > 8);

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

        let after = graph.vertices().get(VertexId::from(0));
        assert_eq!(after.stored_slots, 9);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), road)
                .unwrap()
                .len(),
            8
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }

    #[test]
    fn compact_vertex_edge_span_uses_edge_tombstone_contract() {
        let graph = flag_tombstone_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(2);
        for target in [10, 11, 12] {
            graph
                .insert_edge(VertexId::from(0), road, FlagTombstoneEdge::live(target))
                .unwrap();
        }
        graph
            .remove_edge_matching(VertexId::from(0), road, |edge| {
                *edge == FlagTombstoneEdge::live(10)
            })
            .unwrap();

        let moved = graph
            .compact_vertex_edge_span_one_step(VertexId::from(0), 0)
            .unwrap();

        assert_eq!(
            moved,
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: road,
                old_slot_index: 1,
                new_slot_index: 0,
            })
        );
    }

    #[test]
    fn compact_label_bucket_vertex_segment_preserves_rows() {
        let graph = test_graph();
        for label in 1..=6u16 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    BucketLabelKey::from_raw(label),
                    TestEdge {
                        target: label as u32,
                    },
                )
                .unwrap();
        }
        let before = graph.vertices().get(VertexId::from(0));
        graph
            .compact_label_bucket_vertex_segment(VertexId::from(0))
            .unwrap();
        let after = graph.vertices().get(VertexId::from(0));
        assert_eq!(after.degree(), before.degree());
        assert_eq!(graph.out_edges(VertexId::from(0)).unwrap().len(), 6);
    }
}
