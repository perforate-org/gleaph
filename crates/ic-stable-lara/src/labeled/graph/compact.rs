//! Labeled graph `compact` implementation.

use crate::{
    SegmentId, VertexId,
    labeled::{
        access::LabelEdgeSpanAccess,
        record::{LabelBucket, LabeledVertex},
        slot_index::checked_add_slot_index,
    },
    lara::{edge::EdgeStore, operation_error::LaraOperationError},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{DEFAULT_SEGMENT_SIZE, EdgeSlotMove, LabeledLaraGraph, VertexEdgeSpanCompactOneStep};

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;
use std::cell::Cell;

thread_local! {
    static LABELED_LEAF_RELOCATE_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
    static LABELED_REBALANCE_RESOLVE_IN_PROGRESS: Cell<bool> = const { Cell::new(false) };
    static LABELED_REBALANCE_LEAF_RELOCATED: Cell<bool> = const { Cell::new(false) };
}

struct LabeledLeafRelocateGuard;

impl LabeledLeafRelocateGuard {
    fn new() -> Self {
        LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for LabeledLeafRelocateGuard {
    fn drop(&mut self) {
        LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.set(false));
    }
}

struct LabeledRebalanceResolveGuard;

impl LabeledRebalanceResolveGuard {
    fn new() -> Self {
        LABELED_REBALANCE_RESOLVE_IN_PROGRESS.with(|flag| flag.set(true));
        Self
    }
}

impl Drop for LabeledRebalanceResolveGuard {
    fn drop(&mut self) {
        LABELED_REBALANCE_RESOLVE_IN_PROGRESS.with(|flag| flag.set(false));
    }
}

// Test-only leaf/footprint release metrics. These are thread-local rather than process-global so
// the `reset` + delta assertions in parallel tests cannot contaminate one another: every release is
// recorded synchronously on the same thread that runs the graph operation under test, so a release
// triggered by a concurrent test on another thread is never observed here.
#[cfg(test)]
thread_local! {
    static LABELED_LEAF_PHYSICAL_RELEASE_CALLS: Cell<u32> = const { Cell::new(0) };
    static LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS: Cell<u32> = const { Cell::new(0) };
    static REWRITE_VERTEX_EDGE_SPAN_CALLS: Cell<u32> = const { Cell::new(0) };
    // Fault-injection latch: when set, the next `compact_vertex_edge_span_one_step`
    // returns an error before mutating any state, so maintenance-loop tests can assert
    // a failed step is requeued for retry rather than silently marked complete.
    static FORCE_COMPACT_VERTEX_EDGE_SPAN_STEP_ERROR: Cell<bool> = const { Cell::new(false) };
}

/// Arms a one-shot error from the next `compact_vertex_edge_span_one_step` call on
/// this thread. Consumed by the first call. Test-only.
#[cfg(test)]
pub(crate) fn force_next_compact_vertex_edge_span_step_error() {
    FORCE_COMPACT_VERTEX_EDGE_SPAN_STEP_ERROR.with(|c| c.set(true));
}

#[cfg(test)]
fn take_forced_compact_vertex_edge_span_step_error() -> bool {
    FORCE_COMPACT_VERTEX_EDGE_SPAN_STEP_ERROR.with(|c| c.replace(false))
}

#[cfg(test)]
pub(crate) fn labeled_leaf_physical_release_calls() -> u32 {
    LABELED_LEAF_PHYSICAL_RELEASE_CALLS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn labeled_vertex_footprint_release_calls() -> u32 {
    LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS.with(|c| c.get())
}

#[cfg(test)]
pub(crate) fn rewrite_vertex_edge_span_calls() -> u32 {
    REWRITE_VERTEX_EDGE_SPAN_CALLS.with(|c| c.get())
}

#[cfg(test)]
fn record_labeled_leaf_physical_release() {
    LABELED_LEAF_PHYSICAL_RELEASE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
fn record_labeled_vertex_footprint_release() {
    LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
fn record_rewrite_vertex_edge_span() {
    REWRITE_VERTEX_EDGE_SPAN_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn reset_labeled_leaf_release_test_metrics() {
    LABELED_LEAF_PHYSICAL_RELEASE_CALLS.with(|c| c.set(0));
    LABELED_VERTEX_FOOTPRINT_RELEASE_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn reset_rewrite_vertex_edge_span_test_metrics() {
    REWRITE_VERTEX_EDGE_SPAN_CALLS.with(|c| c.set(0));
}

#[cfg(target_family = "wasm")]
fn log_collect_overflow(message: &str) {
    ic_cdk::println!("LARA CollectAllocationOverflow: {}", message);
}

#[cfg(not(target_family = "wasm"))]
fn log_collect_overflow(_message: &str) {}

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

    fn tail_append_labeled_edge_base(&self, new_alloc: u32) -> Result<u64, LabeledOperationError> {
        let start = self.edges.header().elem_capacity;
        let end = crate::slab_index::checked_add_slot_exclusive_end(start, u64::from(new_alloc))
            .ok_or_else(|| {
                log_collect_overflow(&format!(
                    "tail_append_labeled_edge_base: elem_capacity={start} new_alloc={new_alloc} exceeds index space"
                ));
                LaraOperationError::CollectAllocationOverflow
            })?;
        self.edges
            .set_elem_capacity(end)
            .map_err(LabeledOperationError::from)?;
        Ok(start)
    }

    /// Resolves edge-slab base for [`rebalance_vertex_edge_span`]: in-leaf pin, one leaf
    /// relocate, or (unpinned / relocate-internal only) tail append at `elem_capacity`.
    pub(super) fn labeled_edge_base_from_first_bucket(
        &self,
        src: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(src);
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        buckets
            .first()
            .map(|bucket| bucket.edge_start())
            .ok_or_else(|| {
                log_collect_overflow(&format!(
                    "labeled_edge_base_from_first_bucket: src={src:?} has no buckets"
                ));
                LaraOperationError::CollectAllocationOverflow.into()
            })
    }

    pub(super) fn resolve_labeled_edge_base_for_rebalance(
        &self,
        src: VertexId,
        new_alloc: u32,
    ) -> Result<u64, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc) {
            return Ok(base);
        }
        if LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.get())
            || LABELED_REBALANCE_RESOLVE_IN_PROGRESS.with(|flag| flag.get())
        {
            if self.labeled_leaf_physical_range(src).is_some() {
                return self.labeled_edge_base_from_first_bucket(src);
            }
            log_collect_overflow(
                "resolve_labeled_edge_base_for_rebalance: leaf not pinned; pinning before allocating",
            );
            self.relocate_labeled_leaf_physical_block(src)?;
            if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc) {
                return Ok(base);
            }
            return self.labeled_edge_base_from_first_bucket(src);
        }
        let _resolve_guard = LabeledRebalanceResolveGuard::new();
        if self.labeled_leaf_physical_range(src).is_some() {
            self.relocate_labeled_leaf_physical_block(src)?;
            LABELED_REBALANCE_LEAF_RELOCATED.with(|flag| flag.set(true));
            if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc) {
                return Ok(base);
            }
            return self.labeled_edge_base_from_first_bucket(src);
        }
        log_collect_overflow(
            "resolve_labeled_edge_base_for_rebalance: leaf not pinned; pinning before allocating",
        );
        self.relocate_labeled_leaf_physical_block(src)?;
        LABELED_REBALANCE_LEAF_RELOCATED.with(|flag| flag.set(true));
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc) {
            return Ok(base);
        }
        self.labeled_edge_base_from_first_bucket(src)
    }

    /// Resolves edge-slab base for [`rewrite_vertex_edge_span`]: in-leaf pin, leaf relocate
    /// (with growth retries), or (unpinned / relocate-internal only) tail append.
    pub(super) fn resolve_labeled_edge_base_for_growth(
        &self,
        src: VertexId,
        new_alloc: u32,
    ) -> Result<u64, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc) {
            return Ok(base);
        }
        if self.labeled_leaf_physical_range(src).is_some() {
            if LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.get()) {
                return self.tail_append_labeled_edge_base(new_alloc);
            }
            for _ in 0..4 {
                if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc)
                {
                    return Ok(base);
                }
                self.relocate_labeled_leaf_physical_block(src)?;
            }
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        // Leaf is not pinned yet. Pin it with a block-aligned leaf physical block
        // instead of tail-appending, per ADR 0001 new-bucket contract.
        self.relocate_labeled_leaf_physical_block(src)?;
        if let Some(base) = self.try_labeled_vertex_edge_base_in_pinned_leaf(src, new_alloc) {
            return Ok(base);
        }
        Err(LabeledOperationError::from(
            LaraOperationError::CollectAllocationOverflow,
        ))
    }

    pub(super) fn release_labeled_leaf_physical_footprint(
        &self,
        span_start: u64,
        span_len: u64,
    ) -> Result<(), LabeledOperationError> {
        if span_len == 0 {
            return Ok(());
        }
        #[cfg(test)]
        record_labeled_leaf_physical_release();
        self.edges
            .release_span(span_start, span_len)
            .map_err(LabeledOperationError::from)
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
        #[cfg(test)]
        record_labeled_vertex_footprint_release();
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
        in_window_layout: Option<(u64, u32)>,
    ) -> Result<(Vec<LabelBucket>, u32, u64, u32, u32, bool, u64, Vec<u64>), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
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
        if let Some((new_base, new_alloc)) = in_window_layout {
            if new_alloc < min_required {
                return Err(LaraOperationError::CollectAllocationOverflow.into());
            }
            let old_alloc = vertex.stored_slots;
            let old_base = buckets
                .first()
                .map(|bucket| bucket.edge_start())
                .unwrap_or(0);
            let moved = old_alloc != new_alloc || old_base != new_base;
            let preferred = preferred_bucket.map(|index| index as usize);
            let positions = Self::calculate_label_edge_span_positions(
                new_base,
                new_alloc,
                buckets.as_slice(),
                preferred,
                preferred_extra,
            )?;
            return Ok((
                buckets, old_alloc, old_base, total_live, new_alloc, moved, new_base, positions,
            ));
        }
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
            self.resolve_labeled_edge_base_for_growth(src, new_alloc)?
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
        in_window_layout: Option<(u64, u32)>,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        #[cfg(test)]
        record_rewrite_vertex_edge_span();
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
        let planned_vertex = self.vertices.get(src);
        if planned_vertex.is_default_edge_labeled() || planned_vertex.degree() == 0 {
            return Ok(());
        }

        let (buckets, old_alloc, old_base, total_live, new_alloc, moved, new_base, positions) =
            self.rewrite_vertex_edge_span_read_and_plan(
                src,
                &planned_vertex,
                preferred_bucket,
                preferred_extra,
                compact,
                force_slack_grow,
                in_window_layout,
            )?;

        // Edge-span planning (especially resolving a growth base) may relocate the leaf,
        // fold overflow logs, and update bucket metadata. Re-read the edge buckets rather than
        // publishing positions from the stale snapshot taken before planning/relocate.
        let vertex = self.vertices.get(src);
        let current_buckets = self.read_vertex_label_buckets(&vertex)?;
        if vertex != planned_vertex || current_buckets != buckets {
            // Base resolution may relocate the whole pinned leaf. That updates all
            // vertex spans and bucket starts, invalidating this vertex's pre-relocation
            // plan. Restart from the published layout instead of committing stale
            // positions over the relocation result.
            return self.rewrite_vertex_edge_span(
                src,
                preferred_bucket,
                preferred_extra,
                compact,
                force_slack_grow,
                in_window_layout,
            );
        }

        let slab_only_bulk =
            !compact && self.label_buckets_allow_contiguous_slab_copy(&vertex, &current_buckets)?;

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
                    let successor = self.bucket_slab_window_end_exclusive_after_bucket(
                        &vertex,
                        bucket_index,
                        &buckets[index],
                    )?;
                    let acc = LabelEdgeSpanAccess::with_bucket(
                        &self.buckets,
                        slot,
                        buckets[index],
                        successor,
                        src,
                    );
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
                    let successor = self.bucket_slab_window_end_exclusive_after_bucket(
                        &vertex,
                        bucket_index,
                        &buckets[index],
                    )?;
                    let acc = LabelEdgeSpanAccess::with_bucket(
                        &self.buckets,
                        slot,
                        buckets[index],
                        successor,
                        src,
                    );
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
        if moved
            && old_alloc > 0
            && new_base != old_base
            && !self.vertex_edge_span_relocates_within_pinned_leaf(
                src, old_base, old_alloc, new_base, new_alloc,
            )
            && !self.labeled_edge_footprint_in_current_leaf_pin(src, old_base, old_alloc)
        {
            self.release_vertex_edge_span_footprint(old_base, old_alloc, &buckets)?;
        }
        self.vertices.set(src, &vertex.with_stored_slots(new_alloc));

        let d_total = i64::from(new_alloc) - i64::from(old_alloc);
        self.bump_vertex_edge_span_total_delta(src, d_total)?;
        Ok(())
    }

    fn calculate_labeled_leaf_vertex_positions(
        leaf_start: u64,
        slices: &[(u32, u32)],
        gaps: u64,
    ) -> Result<Vec<u64>, LabeledOperationError> {
        let size = slices.len() as u64;
        let mut total_weight = size;
        for (live_edges, _) in slices {
            total_weight = total_weight
                .checked_add(u64::from(*live_edges))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }

        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .checked_div(tw)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
        };

        let mut cursor_fp = u128::from(leaf_start)
            .checked_mul(P)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let mut out = Vec::with_capacity(slices.len());
        for (live_edges, label_buckets) in slices {
            let start = u64::try_from(cursor_fp / P)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            out.push(start);
            let live = u128::from(*live_edges);
            let labels = u128::from(*label_buckets);
            let start_fp = u128::from(start)
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            cursor_fp = start_fp
                .checked_add(
                    live.checked_mul(P)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?,
                )
                .and_then(|cursor| {
                    let weight = live.checked_add(labels)?;
                    cursor.checked_add(step_fp.checked_mul(weight)?)
                })
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(out)
    }

    /// Weighted in-window slide for all labeled vertices sharing one pinned PMA leaf block.
    pub(crate) fn rebalance_labeled_leaf_weighted_slide(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let (leaf_start, leaf_len) = match self.labeled_leaf_physical_range(src) {
            Some(range) => range,
            None => return Ok(()),
        };
        self.rebalance_labeled_leaf_weighted_slide_in_block(src, leaf_start, leaf_len, false, true)
    }

    pub(crate) fn relocate_labeled_leaf_physical_block(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let _relocate_guard = LabeledLeafRelocateGuard::new();
        let pinned_range = self.labeled_leaf_physical_range(src);
        let (old_start, old_len) = pinned_range.unwrap_or((0, 0));
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, header.segment_size);

        let counts = self.leaf_segment_counts_for_vid(src);
        let used = counts.actual.max(0) as u64;
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let active_vertices = (start_vid..end_vid)
            .filter(|vid_u| {
                let vertex = self.vertices.get(VertexId::from(*vid_u));
                !vertex.is_default_edge_labeled() && vertex.degree() > 0
            })
            .count() as u64;
        let mut resident_geometry = 0u64;
        let mut label_bucket_rows = 0u64;
        for vid_u in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vid_u));
            if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
                continue;
            }
            label_bucket_rows = label_bucket_rows.saturating_add(u64::from(vertex.degree()));
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let mut resident_slots = 0u32;
            for bucket in &buckets {
                let log_slots = if bucket.overflow_log_head() >= 0 {
                    self.edges
                        .overflow_log_chain_len(leaf, bucket.overflow_log_head())
                } else {
                    0
                };
                resident_slots = resident_slots
                    .checked_add(
                        bucket
                            .stored_slots
                            .checked_add(log_slots)
                            .ok_or(LaraOperationError::RowDegreeOverflow)?,
                    )
                    .ok_or(LaraOperationError::RowDegreeOverflow)?;
            }
            resident_geometry = resident_geometry.saturating_add(u64::from(resident_slots));
        }
        let interior_slack = label_bucket_rows
            .saturating_add(active_vertices)
            .saturating_add(u64::from(DEFAULT_SEGMENT_SIZE));
        let block_len = super::leaf_pin::labeled_leaf_physical_block_len(seg);
        let raw_len = resident_geometry
            .saturating_add(interior_slack)
            .saturating_add(u64::from(DEFAULT_SEGMENT_SIZE))
            .max(used.saturating_add(active_vertices))
            .max(old_len.saturating_add(1));
        let new_len = raw_len
            .div_ceil(block_len)
            .saturating_mul(block_len)
            .max(block_len);

        let grew_in_place = if pinned_range.is_some() {
            self.try_expand_labeled_leaf_in_place(old_start, old_len, new_len)?
        } else {
            false
        };
        let new_start = if grew_in_place {
            old_start
        } else {
            self.edges
                .allocate_span(new_len)
                .map_err(LabeledOperationError::from)?
        };

        // Pin the new physical block BEFORE folding the leaf log. Log fold may
        // trigger a vertex-edge-span rebalance; if the leaf is still unpinned,
        // that rebalance calls back into relocate and we recurse forever.
        // Publishing the new start early makes the recursive path see a pinned
        // leaf and resolve an in-place base instead.
        if !grew_in_place {
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), new_start)
                .map_err(LabeledOperationError::from)?;
        }

        self.rebalance_labeled_leaf_weighted_slide_in_block(src, new_start, new_len, true, true)?;

        if !grew_in_place && pinned_range.is_some() {
            self.release_labeled_leaf_physical_footprint(old_start, old_len)?;
        }

        self.edges
            .release_log_segment(SegmentId::from(leaf))
            .map_err(LabeledOperationError::from)?;
        // Per-vertex commits update PMA total from each vertex's prior logical
        // allocation. Zero-length anchors mean those allocations do not always
        // sum to the old physical block width, so reconcile to the one physical
        // source of truth instead of applying a second blind block delta.
        let counts_after = self.leaf_segment_counts_for_vid(src);
        let new_total =
            i64::try_from(new_len).map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let correction = new_total
            .checked_sub(counts_after.total)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if correction != 0 {
            self.edges
                .bump_vertex_segment_counts(src, 0, correction)
                .map_err(LabeledOperationError::from)?;
        }
        Ok(())
    }

    pub(crate) fn rebalance_labeled_leaf_weighted_slide_in_block(
        &self,
        src: VertexId,
        leaf_start: u64,
        leaf_len: u64,
        leaf_relocate_commit: bool,
        suppress_vertex_footprint_release: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(src, seg);
        if !leaf_relocate_commit && self.edges.overflow_log_segment_high_water(leaf) > 0 {
            self.rebalance_edge_log_leaf_for_labeled(src, true, false)?;
        }

        // Folding the shared edge log may itself relocate the leaf while this is
        // a normal slide. Continue from the currently published block, never from
        // the stale range captured by the caller before the fold.
        let (leaf_start, leaf_len) = if leaf_relocate_commit {
            (leaf_start, leaf_len)
        } else {
            self.labeled_leaf_physical_range(src)
                .unwrap_or((leaf_start, leaf_len))
        };
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());

        let mut slices: Vec<(VertexId, u32, u32)> = Vec::new();
        let mut total_resident = 0u64;
        for vid_u in start_vid..end_vid {
            let vid = VertexId::from(vid_u);
            let vertex = self.vertices.get(vid);
            if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
                continue;
            }
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let resident = buckets.iter().try_fold(0u32, |acc, bucket| {
                let log_slots = if bucket.overflow_log_head() >= 0 {
                    self.edges
                        .overflow_log_chain_len(leaf, bucket.overflow_log_head())
                } else {
                    0
                };
                acc.checked_add(
                    bucket
                        .stored_slots
                        .checked_add(log_slots)
                        .ok_or(LaraOperationError::RowDegreeOverflow)?,
                )
                .ok_or(LaraOperationError::RowDegreeOverflow)
            })?;
            // A newly created label bucket is an active zero-edge vertex span. Keep
            // it in the slide so the relocation capacity planned for `active_vertices`
            // is actually assigned to it; otherwise an oversized leaf mate consumes
            // the whole new block and the first bucket cannot obtain an anchor.
            total_resident = total_resident.saturating_add(u64::from(resident));
            slices.push((vid, resident, vertex.degree()));
        }
        if slices.is_empty() {
            return Ok(());
        }

        let gaps = leaf_len.saturating_sub(total_resident);
        let position_inputs: Vec<(u32, u32)> = slices
            .iter()
            .map(|(_, live, labels)| (*live, *labels))
            .collect();
        let vertex_starts =
            Self::calculate_labeled_leaf_vertex_positions(leaf_start, &position_inputs, gaps)?;

        let mut plans: Vec<(
            VertexId,
            LabeledVertex,
            Vec<LabelBucket>,
            u64,
            u32,
            Vec<Vec<E>>,
        )> = Vec::with_capacity(slices.len());
        for (i, (vid, _, _)) in slices.iter().enumerate() {
            let vertex = self.vertices.get(*vid);
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let mut per_bucket_edges: Vec<Vec<E>> = Vec::with_capacity(buckets.len());
            for bucket in &buckets {
                let log_len = if bucket.overflow_log_head() >= 0 {
                    self.edges
                        .overflow_log_chain_len(leaf, bucket.overflow_log_head())
                } else {
                    0
                };
                let resident_slots = bucket
                    .stored_slots
                    .checked_add(log_len)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?;
                let mut resident = Vec::with_capacity(resident_slots as usize);
                for slot_index in 0..bucket.stored_slots {
                    let slot = checked_add_slot_index(bucket.edge_start(), u64::from(slot_index))
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    resident.push(self.edges.read_slot(slot));
                }
                if bucket.overflow_log_head() >= 0 {
                    for (log_offset, log_index) in self
                        .edges
                        .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head())
                        .into_iter()
                        .enumerate()
                    {
                        let log_offset = u32::try_from(log_offset)
                            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                        let slot_index = bucket
                            .stored_slots
                            .checked_add(log_offset)
                            .ok_or(LaraOperationError::RowDegreeOverflow)?;
                        let (_, edge) = self.edges.read_overflow_log_entry(leaf, log_index);
                        resident.push(edge.with_slot_index(slot_index));
                    }
                }
                debug_assert_eq!(resident.len(), resident_slots as usize);
                per_bucket_edges.push(resident);
            }
            let v_start = vertex_starts[i];
            let v_end = vertex_starts.get(i + 1).copied().unwrap_or(leaf_end);
            let span_slots = u32::try_from(v_end.saturating_sub(v_start))
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            plans.push((*vid, vertex, buckets, v_start, span_slots, per_bucket_edges));
        }

        plans.sort_by_key(|(_, _, _, v_start, _, _)| std::cmp::Reverse(*v_start));
        for (vid, vertex, buckets, v_start, span_slots, per_bucket_edges) in plans {
            self.commit_vertex_edge_span_layout(
                vid,
                &vertex,
                &buckets,
                &per_bucket_edges,
                v_start,
                span_slots,
                leaf_relocate_commit,
                suppress_vertex_footprint_release,
            )?;
        }
        Ok(())
    }

    fn commit_vertex_edge_span_layout(
        &self,
        src: VertexId,
        vertex: &LabeledVertex,
        buckets: &[LabelBucket],
        per_bucket_edges: &[Vec<E>],
        new_base: u64,
        new_alloc: u32,
        leaf_relocate_commit: bool,
        suppress_vertex_footprint_release: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let old_alloc = vertex.stored_slots;
        let old_base = buckets
            .first()
            .map(|bucket| bucket.edge_start())
            .unwrap_or(0);
        let resident_buckets = buckets
            .iter()
            .zip(per_bucket_edges.iter())
            .map(|(bucket, edges)| {
                let resident_slots = u32::try_from(edges.len())
                    .map_err(|_| LaraOperationError::RowDegreeOverflow)?;
                Ok::<LabelBucket, LabeledOperationError>(
                    bucket
                        .with_stored_slots(resident_slots)
                        .with_overflow_log_head(-1),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let positions = Self::calculate_label_edge_span_positions_by_resident_slots(
            new_base,
            new_alloc,
            &resident_buckets,
            None,
            0,
        )?;
        let max_run = per_bucket_edges.iter().try_fold(0usize, |max_run, edges| {
            Ok::<usize, LabeledOperationError>(max_run.max(Self::edge_bytes_for_len(edges.len())?))
        })?;
        let mut buf = vec![0u8; max_run];
        let mut row_buckets = Vec::with_capacity(buckets.len());
        for (index, bucket) in resident_buckets.iter().enumerate() {
            let row_start = positions[index];
            let edges = &per_bucket_edges[index];
            if !edges.is_empty() {
                let run = Self::edge_bytes_for_len(edges.len())?;
                let mut offset = 0usize;
                for edge in edges {
                    edge.write_to(&mut buf[offset..offset + E::BYTES]);
                    offset += E::BYTES;
                }
                self.edges.write_slots_contiguous(row_start, &buf[..run])?;
            }
            row_buckets.push(
                bucket
                    .with_edge_range(row_start, bucket.stored_slots)
                    .with_overflow_log_head(-1),
            );
        }
        self.buckets
            .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
        if !leaf_relocate_commit
            && !suppress_vertex_footprint_release
            && old_alloc > 0
            && new_base != old_base
            && !self.vertex_edge_span_relocates_within_pinned_leaf(
                src, old_base, old_alloc, new_base, new_alloc,
            )
            && !self.labeled_edge_footprint_in_current_leaf_pin(src, old_base, old_alloc)
        {
            self.release_vertex_edge_span_footprint(old_base, old_alloc, buckets)?;
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

    fn compact_label_bucket_overflow_to_slab(
        &self,
        src: VertexId,
        bucket_index: u32,
        bucket: LabelBucket,
    ) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        if bucket.overflow_log_head() < 0 {
            return Ok(Vec::new());
        }
        let mut moves = Vec::new();
        let leaf = self.payload_log_leaf(src);
        let chain = self
            .edges
            .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
        let mut live_log_edges = Vec::with_capacity(chain.len());
        for (log_offset, log_index) in chain.into_iter().enumerate() {
            let log_offset = u32::try_from(log_offset)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let old_slot_index = bucket
                .stored_slots
                .checked_add(log_offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let (_, edge) = self.edges.read_overflow_log_entry(leaf, log_index);
            if edge.is_tombstone_edge() {
                continue;
            }
            let live_log_offset = u32::try_from(live_log_edges.len())
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let new_slot_index = bucket
                .stored_slots
                .checked_add(live_log_offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if old_slot_index != new_slot_index {
                moves.push(EdgeSlotMove {
                    label_id: bucket.bucket_label_key(),
                    old_slot_index,
                    new_slot_index,
                });
            }
            live_log_edges.push(edge.with_slot_index(new_slot_index));
        }

        let live_log_count = u32::try_from(live_log_edges.len())
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
        let new_stored_slots = bucket
            .stored_slots
            .checked_add(live_log_count)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let vertex = self.vertices.get(src);
        let successor = self.bucket_successor_start(&vertex, bucket_index)?;
        let available = successor.saturating_sub(bucket.edge_start());
        if available < u64::from(new_stored_slots) {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }

        for (log_offset, edge) in live_log_edges.into_iter().enumerate() {
            let log_offset = u64::try_from(log_offset)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let out_slot = checked_add_slot_index(
                bucket.edge_start(),
                u64::from(bucket.stored_slots)
                    .checked_add(log_offset)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?,
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .write_slot(out_slot, edge)
                .map_err(LabeledOperationError::from)?;
        }

        let compacted = bucket
            .with_stored_slots(new_stored_slots)
            .with_overflow_log_head(-1);
        let bucket_slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
        self.buckets
            .write_label_bucket_slot(bucket_slot, compacted)?;
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
        #[cfg(test)]
        if take_forced_compact_vertex_edge_span_step_error() {
            return Err(LaraOperationError::CollectAllocationOverflow.into());
        }
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
        if resume_bucket_index == 0
            && let Some((bucket_index, bucket)) = buckets
                .iter()
                .enumerate()
                .find(|(_, bucket)| bucket.overflow_log_head() >= 0)
        {
            let bucket_index = u32::try_from(bucket_index)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            match self.compact_label_bucket_overflow_to_slab(vid, bucket_index, *bucket) {
                Ok(moves) => {
                    return Ok(VertexEdgeSpanCompactOneStep::OverflowRewrite(moves));
                }
                Err(LabeledOperationError::Store(
                    LaraOperationError::CollectAllocationOverflow,
                )) => {
                    let log_len = self.edges.overflow_log_chain_len(
                        self.payload_log_leaf(vid),
                        bucket.overflow_log_head(),
                    );
                    self.rebalance_vertex_edge_span(vid, Some(bucket_index), log_len, true)?;
                    return self.compact_vertex_edge_span_one_step(vid, 0);
                }
                Err(error) => return Err(error),
            }
        }
        if resume_bucket_index >= vertex.degree() {
            // Per-bucket steps may already pack each label row (`stored_slots == degree`) while
            // the vertex-wide VertexEdgeSpan width (`vertex.stored_slots`) stays oversized.
            if vertex.stored_slots > total_live
                || buckets
                    .iter()
                    .any(|b| b.overflow_log_head() >= 0 || b.stored_slots != b.degree())
            {
                self.rewrite_vertex_edge_span(vid, None, 0, true, false, None)?;
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
                .checked_div(tw)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
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
            .ok_or_else(|| {
                log_collect_overflow(&format!(
                    "calculate_label_edge_span_positions_by_resident_slots: span_slots={span_slots} < effective_live={effective_live}"
                ));
                LaraOperationError::CollectAllocationOverflow
            })?;

        const P: u128 = 100_000_000;
        let gaps_u = u128::from(gaps);
        let tw = total_weight as u128;
        let step_fp = if tw == 0 {
            0u128
        } else {
            gaps_u
                .checked_mul(P)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
                .checked_div(tw)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
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
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
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
        let mut new_alloc = if force_slack_grow && old_alloc >= min_required && old_alloc > 0 {
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
        new_alloc = new_alloc.max(min_required);
        if self.labeled_leaf_physical_range(src).is_some()
            && min_required > self.labeled_vertex_stored_slots_max_in_leaf(src)?
            && !LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.get())
        {
            self.relocate_labeled_leaf_physical_block(src)?;
            let relocated = self.vertices.get(src);
            if relocated.stored_slots >= min_required {
                return Ok(());
            }
            return self.rebalance_vertex_edge_span(src, preferred_bucket, preferred_extra, false);
        }
        let moved = old_alloc == 0 || new_alloc > old_alloc;
        let new_base = if new_alloc == 0 {
            0
        } else if moved {
            self.resolve_labeled_edge_base_for_rebalance(src, new_alloc)?
        } else {
            old_base
        };
        if LABELED_REBALANCE_LEAF_RELOCATED.with(|flag| flag.replace(false))
            && self.labeled_leaf_physical_range(src).is_some()
        {
            // The relocate slide already published new bucket starts and vertex
            // spans. Re-plan from that state; never write the pre-relocation row.
            let relocated = self.vertices.get(src);
            if relocated.stored_slots >= min_required {
                return Ok(());
            }
            return self.rebalance_vertex_edge_span(src, preferred_bucket, preferred_extra, false);
        }
        let preferred = preferred_bucket.map(|index| index as usize);
        let positions = Self::calculate_label_edge_span_positions_by_resident_slots(
            new_base,
            new_alloc,
            &buckets,
            preferred,
            preferred_extra,
        )?;

        // Snapshot every source run before the first write. Repositioned label
        // spans can overlap another bucket's old range, so read-then-write per
        // bucket would let an early destination corrupt a later source.
        let mut source_runs = Vec::with_capacity(buckets.len());
        for bucket in &buckets {
            let run = Self::edge_bytes_for_len(bucket.stored_slots as usize)?;
            let mut bytes = vec![0u8; run];
            if run > 0 {
                self.edges
                    .read_slots_contiguous(bucket.edge_start(), &mut bytes);
            }
            source_runs.push(bytes);
        }
        let mut row_buckets = Vec::with_capacity(buckets.len());
        for (index, bucket) in buckets.iter().enumerate() {
            let row_start = positions[index];
            if !source_runs[index].is_empty() {
                self.edges
                    .write_slots_contiguous(row_start, &source_runs[index])?;
            }
            row_buckets.push(bucket.with_edge_range(row_start, bucket.stored_slots));
        }
        self.buckets
            .write_label_bucket_row_adaptive(vertex.base_slot_start(), &row_buckets)?;
        if moved
            && old_alloc > 0
            && new_base != old_base
            && !self.vertex_edge_span_relocates_within_pinned_leaf(
                src, old_base, old_alloc, new_base, new_alloc,
            )
            && !self.labeled_edge_footprint_in_current_leaf_pin(src, old_base, old_alloc)
        {
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
                    resume = 0;
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
            self.rewrite_vertex_edge_span(vid, None, 0, false, true, None)?;
        }
        Ok(())
    }

    pub(super) fn rebalance_vertex_edge_span_light(
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
        if allow_slack_grow {
            self.rebalance_vertex_edge_span(vid, None, 0, true)
        } else {
            self.rebalance_vertex_edge_span(vid, None, 0, false)
        }
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
        let leaf = self.payload_log_leaf(src);
        let log_len = self
            .edges
            .overflow_log_chain_len(leaf, bucket.overflow_log_head());
        let resident_after_fold = bucket
            .stored_slots
            .checked_add(log_len)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let successor = self.bucket_successor_start(vertex, bucket_index)?;
        let slack = successor.saturating_sub(bucket.edge_start());
        // Capacity is a preflight condition. Never stream log entries into the
        // slab before proving that this bucket's physical window can hold every
        // logical slot, including tombstones. Rebalance and relocation must not
        // change externally observed bucket-local slot indices.
        if slack < u64::from(resident_after_fold) {
            return Err(LabeledOperationError::from(
                LaraOperationError::CollectAllocationOverflow,
            ));
        }
        let live_written = self.stream_fold_label_bucket_overflow_to_slab(src, &bucket)?;
        Ok(bucket
            .with_overflow_log_head(-1)
            .with_stored_slots(live_written)
            .with_degree_field(bucket.degree()))
    }

    /// Relocates one label bucket's overflow log behind its existing slab prefix.
    ///
    /// Every log entry, including tombstones, keeps its bucket-local slot index. This is the
    /// structural fold used by rebalance and relocation paths that cannot notify derived sidecars.
    /// Maintenance performs the separately bounded overflow compaction when slot rewrites are
    /// allowed and observable.
    fn stream_fold_label_bucket_overflow_to_slab(
        &self,
        src: VertexId,
        bucket: &LabelBucket,
    ) -> Result<u32, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let leaf = u32::from(src) / self.edges.header().segment_size.max(1);
        let log_len = self
            .edges
            .overflow_log_chain_len(leaf, bucket.overflow_log_head());
        // `stored_slots` is the on-slab prefix length. For a brand-new bucket that has
        // never been folded it is zero and all live edges live in the log. The old
        // `stored_slots.saturating_sub(log_len)` formula assumed `stored_slots` already
        // included the log suffix, which is no longer true under the zero-length new-bucket
        // contract.
        let slab_prefix_slots = bucket.stored_slots;
        let edge_start = bucket.edge_start();

        let mut log_edges = Vec::with_capacity(log_len as usize);
        if log_len > 0 {
            let chain = self
                .edges
                .overflow_log_chain_asc_indices(leaf, bucket.overflow_log_head());
            for (log_offset, log_idx) in chain.into_iter().enumerate() {
                let (_, edge) = self.edges.read_overflow_log_entry(leaf, log_idx);
                let log_offset = u32::try_from(log_offset)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                let slot_index = slab_prefix_slots
                    .checked_add(log_offset)
                    .ok_or(LaraOperationError::RowDegreeOverflow)?;
                log_edges.push(edge.with_slot_index(slot_index));
            }
        }

        for (log_offset, edge) in log_edges.into_iter().enumerate() {
            let log_offset = u64::try_from(log_offset)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let out_slot = checked_add_slot_index(
                edge_start,
                u64::from(slab_prefix_slots)
                    .checked_add(log_offset)
                    .ok_or(LaraOperationError::CollectAllocationOverflow)?,
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.edges
                .write_slot(out_slot, edge)
                .map_err(LabeledOperationError::from)?;
        }

        slab_prefix_slots
            .checked_add(log_len)
            .ok_or(LaraOperationError::RowDegreeOverflow.into())
    }

    pub(super) fn fold_label_bucket_payload_log_to_slab(
        &self,
        src: VertexId,
        _vertex: &LabeledVertex,
        _bucket_index: u32,
        bucket_slot: u64,
        bucket: LabelBucket,
    ) -> Result<LabelBucket, LabeledOperationError> {
        if bucket.inline_value_log_head() < 0 || !bucket.is_payload_allocated() {
            return Ok(bucket);
        }
        let leaf = self.payload_log_leaf(src);
        let payload_chain = self
            .values
            .payload_log_chain_asc_indices(leaf, bucket.inline_value_log_head());
        let mut saved = Vec::with_capacity(bucket.degree() as usize);
        for ordinal in 0..bucket.degree() {
            saved.push(self.read_bucket_payload_for_slot(src, &bucket, ordinal, None)?);
        }
        let old_payload_slots = self.bucket_resident_payload_slots_for(src, &bucket);
        let mut bucket = bucket
            .try_with_inline_value_log_head(-1)
            .map_err(LabeledOperationError::from)?;
        bucket = self.ensure_bucket_payload_span(src, bucket_slot, bucket, old_payload_slots)?;
        let width = bucket.inline_value_byte_width();
        for (slot_index, bytes) in saved.iter().enumerate() {
            let slot_index = u32::try_from(slot_index)
                .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
            let offset =
                super::super::invariants::inline_value_byte_offset_at_slot(&bucket, slot_index)?;
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
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            let bucket_index = bucket_index as u32;
            let slot = Self::labeled_vertex_bucket_slot(&vertex, bucket_index)?;
            if bucket.overflow_log_head() >= 0 {
                match self.fold_label_bucket_edges_to_slab(
                    vid,
                    &vertex,
                    bucket_index,
                    slot,
                    *bucket,
                ) {
                    Ok(folded) => {
                        self.buckets.write_label_bucket_slot(slot, folded)?;
                    }
                    Err(LabeledOperationError::Store(
                        LaraOperationError::CollectAllocationOverflow,
                    )) => {
                        let log_len = self.edges.overflow_log_chain_len(
                            self.payload_log_leaf(vid),
                            bucket.overflow_log_head(),
                        );
                        self.rebalance_vertex_edge_span(vid, Some(bucket_index), log_len, true)?;
                        return self.reclaim_vertex_overflow_buckets(vid);
                    }
                    Err(error) => return Err(error),
                }
            }
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

    /// Syncs [`LabeledVertex::stored_slots`] before overflow-log fold when bucket metadata
    /// has outgrown the vertex row but slab bytes are already in place inside the pinned leaf.
    /// Avoids `rebalance_vertex_edge_span_light` (forced slack growth + full copy) on every
    /// `SegmentLogFull` when only the vertex metadata field is stale.
    fn prepare_vertex_edge_span_for_overflow_log_fold(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let buckets = self.read_vertex_label_buckets(&vertex)?;
        let old_alloc = vertex.stored_slots;
        let old_base = buckets
            .first()
            .map(|bucket| bucket.edge_start())
            .unwrap_or(0);
        let resident_slots = buckets.iter().try_fold(0u32, |acc, bucket| {
            acc.checked_add(bucket.stored_slots.max(bucket.degree()))
                .ok_or(LaraOperationError::RowDegreeOverflow)
        })?;
        if resident_slots <= old_alloc {
            return Ok(());
        }
        let edge_only = buckets
            .iter()
            .all(|bucket| bucket.inline_value_byte_width() == 0);
        let target_alloc = if edge_only {
            resident_slots
                .max(DEFAULT_SEGMENT_SIZE)
                .max(resident_slots.saturating_mul(2))
                .max(old_alloc.saturating_mul(2))
        } else {
            resident_slots
        };
        if self.try_labeled_vertex_edge_base_in_pinned_leaf(vid, target_alloc) == Some(old_base) {
            self.vertices
                .set(vid, &vertex.with_stored_slots(target_alloc));
            return Ok(());
        }
        if self.labeled_leaf_physical_range(vid).is_some()
            && resident_slots > self.labeled_vertex_stored_slots_max_in_leaf(vid)?
            && !LABELED_LEAF_RELOCATE_IN_PROGRESS.with(|flag| flag.get())
        {
            self.relocate_labeled_leaf_physical_block(vid)?;
            return Ok(());
        }
        let positions = Self::calculate_label_edge_span_positions_by_resident_slots(
            old_base,
            resident_slots,
            &buckets,
            None,
            0,
        )?;
        let layout_unchanged = buckets.iter().enumerate().all(|(index, bucket)| {
            positions
                .get(index)
                .is_some_and(|start| *start == bucket.edge_start())
        });
        if layout_unchanged {
            let need_end = checked_add_slot_index(old_base, u64::from(resident_slots))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let physical_ok =
                if let Some((leaf_start, leaf_len)) = self.labeled_leaf_physical_range(vid) {
                    let leaf_end = checked_add_slot_index(leaf_start, leaf_len)
                        .ok_or(LaraOperationError::CollectAllocationOverflow)?;
                    need_end <= leaf_end
                } else {
                    true
                };
            if physical_ok {
                self.vertices
                    .set(vid, &vertex.with_stored_slots(resident_slots));
                return Ok(());
            }
        }
        self.rebalance_vertex_edge_span(vid, None, 0, false)
    }

    pub(super) fn rebalance_edge_log_vertex_for_labeled(
        &self,
        vid: VertexId,
        grow_vertex_span: bool,
        use_log_fold_prelude: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let vertex = self.vertices.get(vid);
        if vertex.degree() == 0 || vertex.is_default_edge_labeled() {
            return Ok(());
        }
        if grow_vertex_span {
            let single_label_overflow_fold =
                use_log_fold_prelude && self.read_vertex_label_buckets(&vertex)?.len() == 1;
            if single_label_overflow_fold {
                self.prepare_vertex_edge_span_for_overflow_log_fold(vid)?;
            } else {
                self.rebalance_vertex_edge_span_light(vid, true)?;
            }
        }
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
        grow_vertex_span: bool,
        use_log_fold_prelude: bool,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        let seg_size = self.edges.header().segment_size.max(1);
        let leaf = u32::from(src) / seg_size;
        let start_vid = leaf.saturating_mul(seg_size);
        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            self.rebalance_edge_log_vertex_for_labeled(
                VertexId::from(vid_u),
                grow_vertex_span,
                use_log_fold_prelude,
            )?;
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
                if bucket.inline_value_log_head() < 0 {
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
    use super::{
        labeled_leaf_physical_release_calls, labeled_vertex_footprint_release_calls,
        reset_labeled_leaf_release_test_metrics,
    };
    use crate::VertexId;

    /// ADR 0022 Stage 1 regression: a labeled bucket whose edge bytes live past the
    /// edge-slab leaf-0 physical block and which has spilled into the per-leaf overflow
    /// log must be readable in descending order.
    ///
    /// `LabelEdgeSpanAccess` exposes the bucket as a synthetic `v_ord == 0` row, so
    /// `slab_window_exclusive_end` would read leaf-0's physical cap. When the bucket
    /// base is past that cap, `next_base.min(cap)` placed the window end *before* the
    /// base and `on_slab_edges_with_layout` underflowed into `CollectAllocationOverflow`
    /// — the wedge that broke the 256x64 friends-of-friends build during the descending
    /// edge-handle lookup `insert_directed_edge` runs.
    #[test]
    fn overflow_log_bucket_past_leaf0_cap_reads_descending() {
        let default = BucketLabelKey::directed_from_index(1);
        let graph = test_graph_with_default(default);
        let label = BucketLabelKey::directed_from_index(5);

        // Need leaves 0, 1, 2 (segment_size == 32) plus destination vertices.
        const FAR_VID: u32 = 64; // first vertex of leaf 2
        const EDGES: u32 = 48; // > per-vertex leaf quota (32) so the overflow log activates
        for _ in 1..=(FAR_VID + EDGES) {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        // Pin leaf 0 and leaf 1 with labeled edges so their physical blocks sit before
        // leaf 2's block; this pushes the far vertex's edge_start past leaf-0's cap.
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(0), label, TestEdge { target: 1 })
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(VertexId::from(32), label, TestEdge { target: 2 })
            .unwrap();

        // Fill the far vertex past its leaf quota so excess edges land in the per-leaf
        // overflow log (`log_head >= 0`) without relocating the block.
        for i in 0..EDGES {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(FAR_VID),
                    label,
                    TestEdge {
                        target: FAR_VID + i,
                    },
                )
                .unwrap_or_else(|e| panic!("far insert {i}: {e:?}"));
        }

        let mut descending = Vec::new();
        graph
            .for_each_edges_for_label_ordered(
                VertexId::from(FAR_VID),
                label,
                OutEdgeOrder::Descending,
                |edge| descending.push(edge.target),
            )
            .expect("descending read over overflow-log bucket must succeed");
        assert_eq!(
            descending.len(),
            EDGES as usize,
            "descending read must yield every inserted edge"
        );

        let ascending = graph
            .iter_edges_for_label(VertexId::from(FAR_VID), label)
            .unwrap();
        assert_eq!(ascending.len(), EDGES as usize);
    }

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
    fn mixed_label_hub_50_labels_1000_edges_each() {
        build_mixed_label_hub(50, 1000);
    }

    /// Streaming overflow-log fold must not materialize the full slab width (skewed-noise shape).
    #[test]
    fn single_label_parallel_insert_survives_overflow_log_fold() {
        let started = std::time::Instant::now();
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
        let label = BucketLabelKey::from_raw(42_000);
        const EDGE_COUNT: u32 = if cfg!(debug_assertions) { 600 } else { 5_000 };
        for edge_i in 0..EDGE_COUNT {
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    label,
                    TestEdge {
                        target: u32::from(dst),
                    },
                )
                .unwrap_or_else(|e| panic!("edge_i={edge_i}: {e:?}"));
        }
        assert_eq!(
            graph.iter_edges_for_label(hub, label).unwrap().len(),
            EDGE_COUNT as usize
        );
        assert_eq!(graph.asc_out_edges(hub).unwrap().len(), EDGE_COUNT as usize);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "single-label parallel insert took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn mixed_label_hub_20_labels_500_edges_each() {
        build_mixed_label_hub(20, 500);
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

    /// Phase E gate: same workload as the span-release regression completes quickly.
    #[test]
    fn labeled_hub_33_labels_bounded_insert_time() {
        let started = std::time::Instant::now();
        build_mixed_label_hub(33, 50);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "33-label hub insert took {:?}",
            started.elapsed()
        );
    }

    /// Regression: 33rd label on a dense hub used to spend minutes in span release.
    #[test]
    fn mixed_label_hub_33_labels_span_release_regression() {
        build_mixed_label_hub(33, 50);
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
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
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
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
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

        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();

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

    fn live_edge_count_in_leaf(
        graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        vid: VertexId,
    ) -> u32 {
        let header = graph.edges().header();
        let seg = header.segment_size.max(1);
        let leaf = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::leaf_index_for_vid(
            vid,
            header.segment_size,
        );
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(graph.vertices().len());
        let mut total = 0u32;
        for vidx in start_vid..end_vid {
            for (_, targets) in materialized_labeled_edges(graph, VertexId::from(vidx)) {
                total = total.saturating_add(targets.len() as u32);
            }
        }
        total
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
        graph
            .rebalance_vertex_edge_span(VertexId::from(0), None, 0, true)
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
    fn labeled_proportional_slack_by_label_degree_after_slide() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let hot = BucketLabelKey::from_raw(2);
        let cold = BucketLabelKey::from_raw(3);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        for target in 0..64u32 {
            graph
                .insert_edge_skip_leaf_cascade(vid, hot, TestEdge { target })
                .unwrap();
        }
        graph
            .insert_edge_skip_leaf_cascade(vid, cold, TestEdge { target: 900 })
            .unwrap();
        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();

        let vertex = graph.vertices().get(vid);
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
        graph
            .assert_labeled_buckets_within_leaf_physical(vid)
            .unwrap();
    }

    #[test]
    fn labeled_leaf_rebalance_folds_overflow_log() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        for target in 1..=40u32 {
            graph
                .insert_edge_skip_leaf_cascade(vid, road, TestEdge { target })
                .unwrap();
        }
        let vertex = graph.vertices().get(vid);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert!(bucket.overflow_log_head() >= 0);

        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();

        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.overflow_log_head(), -1);
        assert_eq!(graph.iter_edges_for_label(vid, road).unwrap().len(), 40);
        let leaf = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::leaf_index_for_vid(
            vid,
            graph.edges().header().segment_size,
        );
        assert_eq!(graph.edges().overflow_log_segment_high_water(leaf), 0);
    }

    #[test]
    fn labeled_leaf_rebalance_rewrites_all_bucket_starts_in_leaf() {
        let graph = test_graph();
        let hub = VertexId::from(0);
        let neighbor = VertexId::from(1);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label_a = BucketLabelKey::from_raw(10);
        let label_b = BucketLabelKey::from_raw(11);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(hub, label_a, TestEdge { target: 1 })
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(neighbor, label_b, TestEdge { target: 0 })
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(hub, road, TestEdge { target: 2 })
            .unwrap();
        let before_hub = materialized_labeled_edges(&graph, hub);
        let before_neighbor = materialized_labeled_edges(&graph, neighbor);
        graph.rebalance_labeled_leaf_weighted_slide(hub).unwrap();
        assert_eq!(materialized_labeled_edges(&graph, hub), before_hub);
        assert_eq!(
            materialized_labeled_edges(&graph, neighbor),
            before_neighbor
        );
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
        graph
            .assert_labeled_buckets_within_leaf_physical(neighbor)
            .unwrap();
    }

    #[test]
    fn labeled_leaf_weighted_slide_preserves_total_live_edges() {
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        for target in 0..80u32 {
            graph
                .insert_edge_skip_leaf_cascade(vid, road, TestEdge { target })
                .unwrap();
        }
        for target in 0..20u32 {
            graph
                .remove_edge_matching(vid, road, |edge| edge.target == target)
                .unwrap();
        }
        let before = live_edge_count_in_leaf(&graph, vid);
        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();
        let after = live_edge_count_in_leaf(&graph, vid);
        assert_eq!(before, after);
        assert!(after >= 60);
    }

    #[test]
    fn labeled_leaf_rebalance_does_not_release_span() {
        reset_labeled_leaf_release_test_metrics();
        let leaf_releases_before = labeled_leaf_physical_release_calls();
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        for target in 0..96u32 {
            graph
                .insert_edge_skip_leaf_cascade(vid, road, TestEdge { target })
                .unwrap();
        }
        graph.rebalance_labeled_leaf_weighted_slide(vid).unwrap();
        assert_eq!(
            labeled_leaf_physical_release_calls().saturating_sub(leaf_releases_before),
            0
        );
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0
        );
    }

    #[test]
    fn labeled_segment_relocate_releases_single_footprint() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        reset_labeled_leaf_release_test_metrics();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap();
        let leaf_releases_before = labeled_leaf_physical_release_calls();
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        graph.relocate_labeled_leaf_physical_block(vid).unwrap();
        assert_eq!(
            labeled_leaf_physical_release_calls().saturating_sub(leaf_releases_before),
            1
        );
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0
        );
        assert!(
            graph
                .edges()
                .free_span_store()
                .free_span_starting_at(old_start)
                .is_some_and(|span| span.len == old_len)
        );
        let counts = graph.leaf_segment_counts_for_vid(vid);
        assert!(counts.total as u64 > old_len);
    }

    #[test]
    fn labeled_rewrite_within_pinned_leaf_does_not_release_vertex_footprint() {
        reset_labeled_leaf_release_test_metrics();
        let graph = test_graph();
        let src = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        for target in [1u32, 2, 3] {
            graph
                .insert_edge_skip_leaf_cascade(src, road, TestEdge { target })
                .unwrap();
        }
        assert!(graph.labeled_leaf_physical_range(src).is_some());
        graph
            .remove_edge_at_slot(src, road, 0)
            .unwrap()
            .expect("removed");
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        graph
            .rewrite_vertex_edge_span(src, None, 1, false, true, None)
            .unwrap();
        graph.compact_vertex_edge_span(src, 0).unwrap();
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0,
            "pinned-leaf rewrite keeps interior slack inside the leaf block"
        );
        graph.rebalance_labeled_leaf_weighted_slide(src).unwrap();
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0,
            "pinned-leaf slide must not peel per-vertex footprints"
        );
    }

    #[test]
    fn labeled_segment_relocate_does_not_call_vertex_span_release() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        let vertex_releases_before = labeled_vertex_footprint_release_calls();
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        assert_eq!(
            labeled_vertex_footprint_release_calls().saturating_sub(vertex_releases_before),
            0
        );
    }

    #[test]
    fn labeled_relocate_commit_order() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap();
        graph.relocate_labeled_leaf_physical_block(vid).unwrap();
        graph
            .assert_labeled_buckets_within_leaf_physical(vid)
            .unwrap();
        let (new_start, new_len) = graph.labeled_leaf_physical_range(vid).unwrap();
        assert_ne!(new_start, old_start);
        assert!(new_len > old_len);
        assert!(
            graph
                .edges()
                .free_span_store()
                .free_span_starting_at(old_start)
                .is_some_and(|span| span.len == old_len)
        );
        assert_eq!(materialized_labeled_edges(&graph, vid).len(), 2);
    }

    #[test]
    fn labeled_segment_slide_coalesces_adjacent_free() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        use crate::lara::edge::free_span::FreeSpan;

        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap();
        let left_len = 6u64;
        let right_len = 5u64;
        assert!(
            old_start >= left_len,
            "leaf pin must leave room for a left-adjacent free span"
        );
        let left_start = old_start.saturating_sub(left_len);
        let right_start = old_start.saturating_add(old_len);
        graph.edges().release_span(left_start, left_len).unwrap();
        graph.edges().release_span(right_start, right_len).unwrap();

        graph.relocate_labeled_leaf_physical_block(vid).unwrap();

        let merged_len = left_len.saturating_add(old_len).saturating_add(right_len);
        assert_eq!(count_free_spans(&graph), 1);
        assert_eq!(
            graph.edges().free_span_store().spans(),
            vec![FreeSpan {
                start_slot: left_start,
                len: merged_len,
            }]
        );
        let (new_start, _) = graph.labeled_leaf_physical_range(vid).unwrap();
        assert_ne!(new_start, old_start);
    }

    #[test]
    fn labeled_segment_relocate_reuses_free_span() {
        use super::super::leaf_pin::labeled_leaf_physical_block_len;
        let graph = test_graph();
        let vid = VertexId::from(0);
        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                BucketLabelKey::from_raw(99),
                TestEdge { target: 999 },
            )
            .unwrap();
        let (old_start, old_len) = graph.labeled_leaf_physical_range(vid).unwrap();
        let adjacent = old_start.saturating_add(old_len);
        graph.edges().release_span(adjacent, old_len).unwrap();
        let block_len = labeled_leaf_physical_block_len(graph.edges().header().segment_size);
        for target in 0..block_len {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    road,
                    TestEdge {
                        target: target as u32,
                    },
                )
                .unwrap();
        }
        graph.relocate_labeled_leaf_physical_block(vid).unwrap();
        let (new_start, new_len) = graph.labeled_leaf_physical_range(vid).unwrap();
        assert_eq!(new_start, old_start);
        assert!(new_len > old_len);
        assert!(
            graph
                .edges()
                .free_span_store()
                .free_span_starting_at(adjacent)
                .is_none(),
            "in-place leaf grow should consume the adjacent free span"
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
        graph
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
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
            .compact_vertex_edge_span(VertexId::from(0), 0)
            .unwrap();
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
    fn overflow_direct_unlink_reports_bounded_moves_before_tombstone_free_fold() {
        let graph = inline_value_test_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::directed_from_index(2);
        for target in [10, 11, 12] {
            graph
                .insert_edge(hub, road, PayloadTestEdge::with_bytes(target, &[]))
                .unwrap();
        }
        let removal = graph
            .remove_edge_matching_with_move(hub, road, |edge| edge.target == 10)
            .unwrap();
        assert_eq!(
            removal.unwrap().moves,
            vec![
                EdgeSlotMove {
                    label_id: road,
                    old_slot_index: 1,
                    new_slot_index: 0,
                },
                EdgeSlotMove {
                    label_id: road,
                    old_slot_index: 2,
                    new_slot_index: 1,
                },
            ]
        );

        let rewritten = graph.compact_vertex_edge_span_one_step(hub, 0).unwrap();

        assert_eq!(
            rewritten,
            VertexEdgeSpanCompactOneStep::OverflowRewrite(Vec::new())
        );
        assert_eq!(
            graph
                .iter_edges_for_label(hub, road)
                .unwrap()
                .into_iter()
                .map(|edge| (edge.edge_slot_index_raw(), edge.target))
                .collect::<Vec<_>>(),
            vec![(1, 12), (0, 11)]
        );
    }

    #[test]
    fn structural_overflow_fold_sees_tombstone_free_ordered_unlink_chain() {
        let graph = inline_value_test_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::directed_from_index(2);
        for target in [10, 11, 12] {
            graph
                .insert_edge(hub, road, PayloadTestEdge::with_bytes(target, &[]))
                .unwrap();
        }
        let removal = graph
            .remove_edge_matching_with_move(hub, road, |edge| edge.target == 10)
            .unwrap()
            .unwrap();
        assert_eq!(removal.moves.len(), 2);

        graph
            .rebalance_edge_log_leaf_for_labeled(hub, true, true)
            .unwrap();

        let vertex = graph.vertices().get(hub);
        let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        assert_eq!(bucket.stored_slots, 2);
        assert_eq!(bucket.degree(), 2);
        assert_eq!(bucket.overflow_log_head(), -1);
        assert_eq!(
            graph
                .iter_edges_for_label(hub, road)
                .unwrap()
                .into_iter()
                .map(|edge| (edge.target, edge.edge_slot_index_raw()))
                .collect::<Vec<_>>(),
            vec![(12, 1), (11, 0)]
        );
    }

    #[test]
    fn overflow_rewrite_compacts_only_log_suffix_before_slab_tombstones() {
        let graph = inline_value_test_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::directed_from_index(2);
        for target in 0..4 {
            graph
                .insert_edge(hub, road, PayloadTestEdge::with_bytes(target, &[]))
                .unwrap();
        }
        graph
            .rebalance_edge_log_leaf_for_labeled(hub, true, true)
            .unwrap();
        graph
            .remove_edge_matching(hub, road, |edge| edge.target == 0)
            .unwrap();

        let read_bucket = || {
            let vertex = graph.vertices().get(hub);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let mut first_log_target = None;
        for target in 100..300 {
            graph
                .insert_edge_skip_leaf_cascade(hub, road, PayloadTestEdge::with_bytes(target, &[]))
                .unwrap();
            let bucket = read_bucket();
            if bucket.overflow_log_head() >= 0 {
                first_log_target.get_or_insert(target);
                let log_len = graph.edges().overflow_log_chain_len(
                    graph.payload_log_leaf(hub),
                    bucket.overflow_log_head(),
                );
                if log_len >= 3 {
                    break;
                }
            }
        }
        let first_log_target = first_log_target.expect("hybrid overflow suffix");
        let removal = graph
            .remove_edge_matching_with_move(hub, road, |edge| edge.target == first_log_target)
            .unwrap()
            .unwrap();
        assert!(!removal.moves.is_empty());
        let before = read_bucket();
        assert!(before.stored_slots >= 4);
        assert!(before.overflow_log_head() >= 0);

        let rewritten = graph.compact_vertex_edge_span_one_step(hub, 0).unwrap();

        let VertexEdgeSpanCompactOneStep::OverflowRewrite(moves) = rewritten else {
            panic!("expected overflow-only compaction, got {rewritten:?}");
        };
        assert!(moves.is_empty());
        let slab_survivor = graph
            .iter_edges_for_label(hub, road)
            .unwrap()
            .into_iter()
            .find(|edge| edge.target == 1)
            .unwrap();
        assert_eq!(slab_survivor.edge_slot_index_raw(), 1);

        assert_eq!(
            graph.compact_vertex_edge_span_one_step(hub, 0).unwrap(),
            VertexEdgeSpanCompactOneStep::EdgeMoved(EdgeSlotMove {
                label_id: road,
                old_slot_index: 1,
                new_slot_index: 0,
            })
        );
    }

    #[test]
    fn edge_overflow_compaction_does_not_fold_inline_value_log() {
        let graph = inline_value_test_graph_with_capacity(1 << 16);
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(hub, road, 2)
            .unwrap();
        for target in 1..=33u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    road,
                    PayloadTestEdge::with_bytes(target, &(target as u16).to_le_bytes()),
                )
                .unwrap();
        }
        let read_bucket = || {
            let vertex = graph.vertices().get(hub);
            let slot = graph.find_bucket_slot(&vertex, road).unwrap().unwrap();
            graph.buckets().read_label_bucket_slot(slot).unwrap()
        };
        let before = read_bucket();
        assert!(before.overflow_log_head() >= 0);
        assert!(before.inline_value_log_head() >= 0);
        let payload_state = (
            before.inline_value_offset(),
            before.inline_value_slab_slots(),
            before.inline_value_log_head(),
            before.inline_value_log_len(),
        );

        assert!(matches!(
            graph.compact_vertex_edge_span_one_step(hub, 0).unwrap(),
            VertexEdgeSpanCompactOneStep::OverflowRewrite(_)
        ));

        let after = read_bucket();
        assert_eq!(after.overflow_log_head(), -1);
        assert_eq!(
            (
                after.inline_value_offset(),
                after.inline_value_slab_slots(),
                after.inline_value_log_head(),
                after.inline_value_log_len(),
            ),
            payload_state
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
