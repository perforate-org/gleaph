//! PMA layer for DGAP (segment tree density, `calculate_positions_v1`, `rebalance_weighted`); mirrors reference `graph.h`.

use ic_stable_slot_map::SlotMap;
use ic_stable_structures::Memory;

use crate::layout::dgap::{SegmentEdgeCounts, read_segment_edge_counts, write_segment_edge_counts};
use crate::traits::{CsrEdge, CsrVertex};

/// Upper density at root / leaves (C++ `up_h` / `up_0`).
pub const UP_H: f64 = 0.75;
pub const UP_0: f64 = 1.0;
pub const LOW_H: f64 = 0.50;
pub const LOW_0: f64 = 0.25;

#[inline]
pub fn floor_log2_u32(mut n: u32) -> u32 {
    if n <= 1 {
        return 0;
    }
    let mut r = 0u32;
    n >>= 1;
    while n > 0 {
        r += 1;
        n >>= 1;
    }
    r
}

/// PMA tree index of the leaf for `segment_id` (`0..segment_count`), matching
/// [`recount_segment_edge_counts_column`] (`leaf = segment_id + segment_count`).
#[inline]
pub fn leaf_pma_tree_index(segment_id: usize, segment_count: u32) -> usize {
    segment_id + segment_count as usize
}

/// Leaf PMA tree index for `vertex_id` (C++ `get_segment_id`).
#[inline]
pub fn pma_tree_index(vertex_id: u32, segment_size: u32, segment_count: u32) -> usize {
    let sid = (vertex_id / segment_size.max(1)) as usize;
    leaf_pma_tree_index(sid, segment_count)
}

#[inline]
fn merge_segment_edge_counts_children(
    l: SegmentEdgeCounts,
    r: SegmentEdgeCounts,
) -> SegmentEdgeCounts {
    SegmentEdgeCounts {
        actual: l.actual.saturating_add(r.actual),
        total: l.total.saturating_add(r.total),
        tombstone: l.tombstone.saturating_add(r.tombstone),
    }
}

/// Re-aggregate internal PMA nodes from `leaf_j` up to the root (indices match
/// [`recount_segment_edge_counts_column`]). Mirrors `out[0] = out[1]` when `segment_count > 1`.
pub fn reaggregate_segment_edge_counts_ancestors<M: Memory>(
    sec_mem: &M,
    stride: u64,
    segment_count: u32,
    leaf_j: usize,
) {
    let sc = segment_count as usize;
    let n = sc.saturating_mul(2);
    if sc == 0 || n == 0 {
        return;
    }
    let mut j = leaf_j;
    while j >= 2 {
        let p = j / 2;
        let l = p * 2;
        let r = l + 1;
        if r >= n {
            break;
        }
        let cl = read_segment_edge_counts(sec_mem, l, stride);
        let cr = read_segment_edge_counts(sec_mem, r, stride);
        let merged = merge_segment_edge_counts_children(cl, cr);
        write_segment_edge_counts(sec_mem, p, stride, merged);
        j = p;
    }
    if sc > 1 {
        let root = read_segment_edge_counts(sec_mem, 1, stride);
        write_segment_edge_counts(sec_mem, 0, stride, root);
    }
}

/// Apply integer deltas to one leaf PMA node (`segment_id`), then propagate to ancestors.
///
/// For stride **16** nodes ([`crate::layout::dgap::write_segment_edge_counts`]), `d_tombstone` is ignored
/// (no tombstone field on disk); internal aggregation still sums stored tombstone fields (always 0).
pub fn propagate_segment_edge_counts_leaf_delta<M: Memory>(
    sec_mem: &M,
    stride: u64,
    segment_count: u32,
    segment_id: usize,
    d_actual: i64,
    d_total: i64,
    d_tombstone: i64,
) -> Result<(), &'static str> {
    let sc = segment_count as usize;
    let n = sc.saturating_mul(2);
    if sc == 0 {
        return Err("segment_count is 0");
    }
    let leaf_j = segment_id.saturating_add(sc);
    if leaf_j >= n {
        return Err("leaf index oob");
    }
    let mut c = read_segment_edge_counts(sec_mem, leaf_j, stride);
    c.actual = c.actual.saturating_add(d_actual);
    c.total = c.total.saturating_add(d_total);
    if stride >= 24 {
        c.tombstone = c.tombstone.saturating_add(d_tombstone);
    }
    write_segment_edge_counts(sec_mem, leaf_j, stride, c);
    reaggregate_segment_edge_counts_ancestors(sec_mem, stride, segment_count, leaf_j);
    Ok(())
}

/// `delta_up` / `delta_low` from `tree_height`.
pub fn density_deltas(tree_height: u32) -> (f64, f64) {
    let th = tree_height.max(1) as f64;
    let delta_up = (UP_0 - UP_H) / th;
    let delta_low = (LOW_H - LOW_0) / th;
    (delta_up, delta_low)
}

/// DGAP / `graph.h` `calculate_positions_V1`: new base slot index per vertex in `[start_vertex, end_vertex)`.
pub fn calculate_positions_v1(
    start_vertex: usize,
    end_vertex: usize,
    base_slot: &[u64],
    degree: &[u32],
    gaps: i64,
    segment_edges_actual: i64,
) -> Vec<u64> {
    assert!(end_vertex > start_vertex);
    assert!(base_slot.len() >= end_vertex);
    assert!(degree.len() >= end_vertex);
    let size = end_vertex - start_vertex;
    let mut out = vec![0u64; size];
    let total_degree = segment_edges_actual.saturating_add(size as i64).max(1);
    let step = (gaps as f64) / (total_degree as f64);
    let mut index_d = base_slot[start_vertex] as f64;
    for i in start_vertex..end_vertex {
        let li = i - start_vertex;
        out[li] = index_d as u64;
        if i > start_vertex {
            let prev = out[li - 1];
            let need = prev.saturating_add(degree[i - 1] as u64);
            debug_assert!(
                out[li] >= need,
                "non-overlap: prev_end={need} new_start={}",
                out[li]
            );
        }
        let d = degree[i] as f64;
        index_d += d + step * (d + 1.0);
    }
    out
}

/// Same as [`calculate_positions_v1`] but for a contiguous window using **local** `base_slot[0..w]` / `degree[0..w]`
/// (entry `k` is global vertex `start_vertex + k`).
pub fn calculate_positions_v1_window(
    w: usize,
    local_base: &[u64],
    local_degree: &[u32],
    gaps: i64,
    segment_edges_actual: i64,
) -> Vec<u64> {
    assert!(w > 0);
    assert!(local_base.len() >= w && local_degree.len() >= w);
    let mut out = vec![0u64; w];
    let total_degree = segment_edges_actual.saturating_add(w as i64).max(1);
    let step = (gaps as f64) / (total_degree as f64);
    let mut index_d = local_base[0] as f64;
    for li in 0..w {
        out[li] = index_d as u64;
        if li > 0 {
            let prev = out[li - 1];
            let need = prev.saturating_add(local_degree[li - 1] as u64);
            debug_assert!(
                out[li] >= need,
                "non-overlap: prev_end={need} new_start={}",
                out[li]
            );
        }
        let d = local_degree[li] as f64;
        index_d += d + step * (d + 1.0);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebalanceDecision {
    Noop,
    RebalanceWindow {
        left_vertex: usize,
        right_vertex: usize,
        pma_idx: usize,
    },
    ResizeNeeded,
}

/// C++ `rebalance_wrapper` control flow (density too high → walk up tree).
///
/// Only reads PMA indices on the leaf-to-root path (`window`, `window/2`, …), not the full `segment_count * 2` array.
pub fn rebalance_decision_with_reader(
    src_vertex: u32,
    segment_size: u32,
    segment_count: u32,
    num_vertices: usize,
    tree_height: u32,
    mut read_at: impl FnMut(usize) -> (i64, i64),
) -> RebalanceDecision {
    let sc = segment_count as usize;
    let len = sc * 2;
    if sc == 0 {
        return RebalanceDecision::Noop;
    }
    let (delta_up, _) = density_deltas(tree_height);
    let mut height = 0i32;
    let mut window = pma_tree_index(src_vertex, segment_size, segment_count);
    if window >= len {
        return RebalanceDecision::Noop;
    }
    let (a0, t0) = read_at(window);
    if t0 <= 0 {
        return RebalanceDecision::Noop;
    }
    let mut density = a0 as f64 / t0 as f64;
    let mut up_height = UP_0 - (height as f64) * delta_up;

    while window > 0 && density >= up_height {
        window /= 2;
        height += 1;
        up_height = UP_0 - (height as f64) * delta_up;
        if window >= len {
            break;
        }
        let (a, t) = read_at(window);
        if t <= 0 {
            break;
        }
        density = a as f64 / t as f64;
    }

    if height == 0 {
        return RebalanceDecision::Noop;
    }

    if density < up_height {
        let window_size = (segment_size as usize).saturating_mul(1usize << (height as u32));
        let left = (src_vertex as usize / window_size) * window_size;
        let right = (left + window_size).min(num_vertices);
        RebalanceDecision::RebalanceWindow {
            left_vertex: left,
            right_vertex: right,
            pma_idx: window,
        }
    } else {
        RebalanceDecision::ResizeNeeded
    }
}

/// Same as [`rebalance_decision_with_reader`] using preloaded `actual` / `total` slices (length `segment_count * 2`).
pub fn rebalance_decision(
    src_vertex: u32,
    segment_size: u32,
    segment_count: u32,
    num_vertices: usize,
    tree_height: u32,
    actual: &[i64],
    total: &[i64],
) -> RebalanceDecision {
    let sc = segment_count as usize;
    let len = sc * 2;
    if actual.len() < len || total.len() < len || sc == 0 {
        return RebalanceDecision::Noop;
    }
    rebalance_decision_with_reader(
        src_vertex,
        segment_size,
        segment_count,
        num_vertices,
        tree_height,
        |i| (actual[i], total[i]),
    )
}

/// In-memory DGAP `rebalance_weighted` on a **vertex window** `vertices_win` (global indices `[left, right)`).
///
/// `next_base_after_window` is the first edge slot **after** the window (or `elem_capacity_slots` if `right == num_vertices`).
pub fn rebalance_weighted_window<V: CsrVertex, E: CsrEdge>(
    vertices_win: &mut [V],
    next_base_after_window: u64,
    edges: &mut [E],
    segment_edges_total_at_idx: i64,
    segment_edges_actual_at_idx: i64,
) {
    let w = vertices_win.len();
    assert!(w > 0);
    let mut local_base = vec![0u64; w];
    let mut local_deg = vec![0u32; w];
    for li in 0..w {
        local_base[li] = vertices_win[li].base_slot_start();
        local_deg[li] = vertices_win[li].degree();
    }

    let from = local_base[0];
    let to = next_base_after_window;
    assert!(to > from, "invalid rebalance range");
    let capacity = to - from;
    assert!(
        segment_edges_total_at_idx == capacity as i64,
        "total mismatch"
    );
    let gaps = segment_edges_total_at_idx - segment_edges_actual_at_idx;

    let new_index = calculate_positions_v1_window(
        w,
        &local_base,
        &local_deg,
        gaps,
        segment_edges_actual_at_idx,
    );

    let index_boundary = next_base_after_window;
    debug_assert!(new_index[w - 1].saturating_add(local_deg[w - 1] as u64) <= index_boundary);

    let mut curr_li = 1usize;
    while curr_li < w {
        let mut ii = curr_li;
        while ii < w {
            if new_index[ii] <= vertices_win[ii].base_slot_start() {
                break;
            }
            ii += 1;
        }
        if ii == w {
            ii -= 1;
        }
        let next_to_start = ii + 1;
        if new_index[ii] <= vertices_win[ii].base_slot_start() {
            let jj = ii;
            let mut read_index = vertices_win[jj].base_slot_start();
            let last_read = read_index + vertices_win[jj].degree() as u64;
            let mut write_index = new_index[jj];
            while read_index < last_read {
                edges[write_index as usize] = edges[read_index as usize];
                write_index += 1;
                read_index += 1;
            }
            vertices_win[jj] = vertices_win[jj].with_base_slot_start(new_index[jj]);
            ii -= 1;
        }

        let mut jj = ii as isize;
        while jj >= curr_li as isize {
            let j = jj as usize;
            let deg = vertices_win[j].degree() as u64;
            if deg > 0 {
                let b = vertices_win[j].base_slot_start();
                let nb = new_index[j];
                for off in (0..deg).rev() {
                    let read_index = b + off;
                    let write_index = nb + off;
                    edges[write_index as usize] = edges[read_index as usize];
                }
            }
            vertices_win[j] = vertices_win[j].with_base_slot_start(new_index[j]);
            jj -= 1;
        }
        curr_li = next_to_start;
    }
}

/// Like [`rebalance_weighted_window`], but `edges_local` covers only `[range_from, next_base_after_window)`;
/// global slot `g` maps to `edges_local[(g - range_from) as usize]`.
///
/// Requires `vertices_win[0].base_slot_start() == range_from`.
pub fn rebalance_weighted_window_rel<V: CsrVertex, E: CsrEdge>(
    vertices_win: &mut [V],
    range_from: u64,
    next_base_after_window: u64,
    edges_local: &mut [E],
    segment_edges_total_at_idx: i64,
    segment_edges_actual_at_idx: i64,
) {
    let w = vertices_win.len();
    assert!(w > 0);
    let mut local_base = vec![0u64; w];
    let mut local_deg = vec![0u32; w];
    for li in 0..w {
        local_base[li] = vertices_win[li].base_slot_start();
        local_deg[li] = vertices_win[li].degree();
    }

    let from = range_from;
    let to = next_base_after_window;
    assert_eq!(
        local_base[0], from,
        "rebalance_weighted_window_rel: first vertex base must equal range_from"
    );
    assert!(to > from, "invalid rebalance range");
    let span = (to - from) as usize;
    assert_eq!(
        edges_local.len(),
        span,
        "edges_local must span window slab range"
    );
    let capacity = to - from;
    assert!(
        segment_edges_total_at_idx == capacity as i64,
        "total mismatch"
    );
    let gaps = segment_edges_total_at_idx - segment_edges_actual_at_idx;

    let new_index = calculate_positions_v1_window(
        w,
        &local_base,
        &local_deg,
        gaps,
        segment_edges_actual_at_idx,
    );

    let index_boundary = next_base_after_window;
    debug_assert!(new_index[w - 1].saturating_add(local_deg[w - 1] as u64) <= index_boundary);

    #[inline]
    fn li(off: u64, g: u64) -> usize {
        (g - off) as usize
    }

    let mut curr_li = 1usize;
    while curr_li < w {
        let mut ii = curr_li;
        while ii < w {
            if new_index[ii] <= vertices_win[ii].base_slot_start() {
                break;
            }
            ii += 1;
        }
        if ii == w {
            ii -= 1;
        }
        let next_to_start = ii + 1;
        if new_index[ii] <= vertices_win[ii].base_slot_start() {
            let jj = ii;
            let mut read_index = vertices_win[jj].base_slot_start();
            let last_read = read_index + vertices_win[jj].degree() as u64;
            let mut write_index = new_index[jj];
            while read_index < last_read {
                edges_local[li(from, write_index)] = edges_local[li(from, read_index)];
                write_index += 1;
                read_index += 1;
            }
            vertices_win[jj] = vertices_win[jj].with_base_slot_start(new_index[jj]);
            ii -= 1;
        }

        let mut jj = ii as isize;
        while jj >= curr_li as isize {
            let j = jj as usize;
            let deg = vertices_win[j].degree() as u64;
            if deg > 0 {
                let b = vertices_win[j].base_slot_start();
                let nb = new_index[j];
                for off in (0..deg).rev() {
                    let read_index = b + off;
                    let write_index = nb + off;
                    edges_local[li(from, write_index)] = edges_local[li(from, read_index)];
                }
            }
            vertices_win[j] = vertices_win[j].with_base_slot_start(new_index[j]);
            jj -= 1;
        }
        curr_li = next_to_start;
    }
}

/// Tunable thresholds for [`segment_maintenance_decision`].
///
/// **PMA density:** structural work is driven by [`rebalance_decision`] (`actual` / `total` vs
/// implicit tree thresholds), not by these ratios alone.
///
/// **Physical tombstones:** modeled after LSM-style *tombstone-density* triggers, but the fixed
/// dead-slot count gate is replaced with a size-corrected score (`tombstone_ratio * log2(total+1)`).
/// This keeps small leaves from enqueueing too early while still surfacing large leaves with
/// meaningful waste.
#[derive(Clone, Copy, Debug)]
pub struct SegmentMaintainThresholds {
    /// Treat a leaf as tombstone-backed work when `tombstone / total` is at least this (`total > 0`).
    pub soft_tombstone_ratio: f64,
    /// Prefer inline maintenance when `tombstone / total` is at least this.
    pub strict_tombstone_ratio: f64,
    /// Treat a leaf as garbage-heavy when `tombstone_ratio * log2(total + 1)` is at least this.
    pub soft_tombstone_score_threshold: f64,
    /// Prefer inline maintenance when `tombstone_ratio * log2(total + 1)` is at least this.
    pub strict_tombstone_score_threshold: f64,
    /// When the work queue is at least this long and the leaf has **significant** tombstone garbage
    /// (same rule as enqueue: soft ratio or score), prefer inline.
    pub queue_depth_inline_pressure: u64,
}

impl Default for SegmentMaintainThresholds {
    fn default() -> Self {
        Self {
            soft_tombstone_ratio: 0.05,
            strict_tombstone_ratio: 0.25,
            soft_tombstone_score_threshold: 0.20,
            strict_tombstone_score_threshold: 0.60,
            queue_depth_inline_pressure: 64,
        }
    }
}

/// What to do after a logical delete (or periodic tick) for one PMA leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentMaintainAction {
    Noop,
    Enqueue,
    InlineNow,
}

/// Combine PMA rebalance hints, tombstone pressure, and queue depth.
///
/// - **Density:** [`RebalanceDecision::ResizeNeeded`] or non-`Noop` rebalance window schedules structural work.
/// - **Tombstones:** physical garbage uses a ratio gate plus a size-corrected score gate, not a fixed
///   dead-slot count.
/// - **Backlog:** long queue + significant tombstone garbage biases toward [`SegmentMaintainAction::InlineNow`].
pub fn segment_maintenance_decision(
    leaf: SegmentEdgeCounts,
    rebalance: RebalanceDecision,
    queue_len: u64,
    thr: &SegmentMaintainThresholds,
) -> SegmentMaintainAction {
    if matches!(rebalance, RebalanceDecision::ResizeNeeded) {
        return SegmentMaintainAction::InlineNow;
    }

    let tomb_garbage_strict = tombstone_garbage_strict(leaf, thr);
    if tomb_garbage_strict {
        return SegmentMaintainAction::InlineNow;
    }

    let tomb_garbage = tombstone_garbage_significant(leaf, thr);

    let queue_pressure = {
        let _s = crate::canbench_scope::scope("dgap_segment_maintain_queue_pressure");
        queue_len >= thr.queue_depth_inline_pressure && tomb_garbage
    };
    if queue_pressure {
        return SegmentMaintainAction::InlineNow;
    }

    let density_work = !matches!(rebalance, RebalanceDecision::Noop);

    if density_work || tomb_garbage {
        SegmentMaintainAction::Enqueue
    } else {
        SegmentMaintainAction::Noop
    }
}

#[inline]
fn tombstone_ratio(leaf: SegmentEdgeCounts) -> f64 {
    let _s = crate::canbench_scope::scope("dgap_segment_maintain_tombstone_ratio");
    if leaf.total <= 0 || leaf.tombstone <= 0 {
        return 0.0;
    }
    leaf.tombstone as f64 / leaf.total as f64
}

#[inline]
fn tombstone_score(leaf: SegmentEdgeCounts) -> f64 {
    let _s = crate::canbench_scope::scope("dgap_segment_maintain_tombstone_score");
    let tomb_r = tombstone_ratio(leaf);
    if tomb_r <= 0.0 {
        return 0.0;
    }
    tomb_r * ((leaf.total.max(0) as f64) + 1.0).log2()
}

#[inline]
fn tombstone_garbage_significant(leaf: SegmentEdgeCounts, thr: &SegmentMaintainThresholds) -> bool {
    let _s = crate::canbench_scope::scope("dgap_segment_maintain_soft_garbage");
    if leaf.tombstone <= 0 || leaf.total <= 0 {
        return false;
    }
    let tomb_r = tombstone_ratio(leaf);
    tomb_r >= thr.soft_tombstone_ratio || tombstone_score(leaf) >= thr.soft_tombstone_score_threshold
}

#[inline]
fn tombstone_garbage_strict(leaf: SegmentEdgeCounts, thr: &SegmentMaintainThresholds) -> bool {
    let _s = crate::canbench_scope::scope("dgap_segment_maintain_strict_garbage");
    if leaf.tombstone <= 0 || leaf.total <= 0 {
        return false;
    }
    let tomb_r = tombstone_ratio(leaf);
    tomb_r >= thr.strict_tombstone_ratio || tombstone_score(leaf) >= thr.strict_tombstone_score_threshold
}

/// In-memory DGAP `rebalance_weighted` on global edge slot indices (full `vertices` slice).
pub fn rebalance_weighted<V: CsrVertex, E: CsrEdge>(
    vertices: &mut [V],
    edges: &mut [E],
    start_vertex: usize,
    end_vertex: usize,
    elem_capacity_slots: u64,
    segment_edges_total_at_idx: i64,
    segment_edges_actual_at_idx: i64,
) {
    assert!(end_vertex > start_vertex);
    let next_base = if end_vertex >= vertices.len() {
        elem_capacity_slots
    } else {
        vertices[end_vertex].base_slot_start()
    };
    rebalance_weighted_window(
        &mut vertices[start_vertex..end_vertex],
        next_base,
        edges,
        segment_edges_total_at_idx,
        segment_edges_actual_at_idx,
    );
}

/// C++ `recount_segment_total`: zero `total`, then add each leaf span walking `j /= 2` while `j > 0`.
pub fn recount_segment_total<V: CsrVertex>(
    vertices: &[V],
    segment_count: u32,
    segment_size: u32,
    elem_capacity_slots: u64,
    total: &mut [i64],
) {
    total.fill(0);
    for i in 0..segment_count as usize {
        let v0 = i * segment_size as usize;
        if v0 >= vertices.len() {
            break;
        }
        let next_starter = if i + 1 == segment_count as usize {
            elem_capacity_slots
        } else {
            let ni = (i + 1) * segment_size as usize;
            if ni >= vertices.len() {
                elem_capacity_slots
            } else {
                vertices[ni].base_slot_start()
            }
        };
        let segment_total_p = next_starter as i64 - vertices[v0].base_slot_start() as i64;
        let mut j = leaf_pma_tree_index(i, segment_count);
        while j > 0 && j < total.len() {
            total[j] += segment_total_p;
            j /= 2;
        }
    }
}

/// Set leaf `actual[i+sc]` to sum of degrees in vertex range, then aggregate parents (C++ insert path).
pub fn recount_segment_actual_from_degrees<V: CsrVertex>(
    vertices: &[V],
    segment_count: u32,
    segment_size: u32,
    actual: &mut [i64],
) {
    let sc = segment_count as usize;
    let n = sc * 2;
    if actual.len() < n {
        return;
    }
    actual.fill(0);
    for i in 0..segment_count as usize {
        let v0 = i * segment_size as usize;
        let vend = ((i + 1) * segment_size as usize).min(vertices.len());
        let mut sum = 0i64;
        for v in v0..vend {
            sum += vertices[v].degree() as i64;
        }
        let leaf = leaf_pma_tree_index(i, segment_count);
        if leaf < actual.len() {
            actual[leaf] = sum;
        }
    }
    for p in (1..sc).rev() {
        let l = p * 2;
        let r = l + 1;
        if r < n {
            actual[p] = actual[l] + actual[r];
        }
    }
    if sc > 1 {
        actual[0] = actual[1];
    }
}

/// [`recount_segment_total`] reading vertices through a dense [`SlotMap`] (no `&[V]` snapshot).
pub fn recount_segment_total_column<V, M>(
    col: &SlotMap<V, M>,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    elem_capacity_slots: u64,
    total: &mut [i64],
) where
    V: CsrVertex,
    M: Memory,
{
    let nv = num_vertices as usize;
    total.fill(0);
    for i in 0..segment_count as usize {
        let v0 = i * segment_size as usize;
        if v0 >= nv {
            break;
        }
        let next_starter = if i + 1 == segment_count as usize {
            elem_capacity_slots
        } else {
            let ni = (i + 1) * segment_size as usize;
            if ni >= nv {
                elem_capacity_slots
            } else {
                col.get_dense(ni as u32)
                    .expect("vertex column get")
                    .base_slot_start()
            }
        };
        let v0_row = col.get_dense(v0 as u32).expect("vertex column get v0");
        let segment_total_p = next_starter as i64 - v0_row.base_slot_start() as i64;
        let mut j = leaf_pma_tree_index(i, segment_count);
        while j > 0 && j < total.len() {
            total[j] += segment_total_p;
            j /= 2;
        }
    }
}

/// [`recount_segment_actual_from_degrees`] via a dense [`SlotMap`] vertex table.
pub fn recount_segment_actual_column<V, M>(
    col: &SlotMap<V, M>,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    actual: &mut [i64],
) where
    V: CsrVertex,
    M: Memory,
{
    let nv = num_vertices as usize;
    let sc = segment_count as usize;
    let n = sc * 2;
    if actual.len() < n {
        return;
    }
    actual.fill(0);
    for i in 0..segment_count as usize {
        let v0 = i * segment_size as usize;
        let vend = ((i + 1) * segment_size as usize).min(nv);
        let mut sum = 0i64;
        for v in v0..vend {
            sum += col.get_dense(v as u32).expect("vertex column get").degree() as i64;
        }
        let leaf = leaf_pma_tree_index(i, segment_count);
        if leaf < actual.len() {
            actual[leaf] = sum;
        }
    }
    for p in (1..sc).rev() {
        let l = p * 2;
        let r = l + 1;
        if r < n {
            actual[p] = actual[l] + actual[r];
        }
    }
    if sc > 1 {
        actual[0] = actual[1];
    }
}

/// DGAP segment indices whose PMA leaves must be recomputed after mutating only vertices in `[left, right)`.
///
/// `right` is exclusive. When `left >= right`, returns an empty list (no-op for callers).
///
/// If `left` lies on a segment boundary (`left == seg_lo * segment_size`) and `left > 0`, segment `seg_lo - 1`
/// is included: its `total` depends on the first vertex base of segment `seg_lo`, which may change inside the window.
pub fn segments_for_vertex_range(
    left: usize,
    right: usize,
    segment_size: u32,
    segment_count: u32,
) -> Vec<usize> {
    if left >= right {
        return Vec::new();
    }
    let ss = segment_size.max(1) as usize;
    let sc = segment_count as usize;
    if sc == 0 {
        return Vec::new();
    }
    let seg_lo = left / ss;
    let seg_hi = (right - 1) / ss;
    let mut range_start = seg_lo;
    if left > 0 && seg_lo > 0 && left == seg_lo.saturating_mul(ss) {
        range_start = seg_lo - 1;
    }
    let range_end = seg_hi;
    let last_valid = sc - 1;
    let lo = range_start;
    let hi = range_end.min(last_valid);
    if lo > hi {
        return Vec::new();
    }
    (lo..=hi).collect()
}

/// One PMA leaf's [`SegmentEdgeCounts`] for DGAP segment index `segment_idx`, same definition as
/// [`recount_segment_edge_counts_column`] for that leaf.
///
/// If the segment has no vertices (`segment_idx * segment_size >= num_vertices`) or `segment_idx >= segment_count`,
/// returns zeros.
pub fn segment_edge_counts_leaf_from_column<V, M, F>(
    col: &SlotMap<V, M>,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    elem_capacity_slots: u64,
    segment_idx: usize,
    slot_is_tombstone: &mut F,
) -> SegmentEdgeCounts
where
    V: CsrVertex,
    M: Memory,
    F: FnMut(u64) -> bool,
{
    let nv = num_vertices as usize;
    let i = segment_idx;
    if i >= segment_count as usize {
        return SegmentEdgeCounts {
            actual: 0,
            total: 0,
            tombstone: 0,
        };
    }
    let v0 = i * segment_size as usize;
    if v0 >= nv {
        return SegmentEdgeCounts {
            actual: 0,
            total: 0,
            tombstone: 0,
        };
    }
    let vend = ((i + 1) * segment_size as usize).min(nv);
    let mut actual_sum = 0i64;
    let mut tombstone_sum = 0i64;
    for v in v0..vend {
        actual_sum += col.get_dense(v as u32).expect("vertex column get").degree() as i64;
        let row = col.get_dense(v as u32).expect("vertex column get");
        let b = row.base_slot_start();
        let end_exclusive = if v + 1 < nv {
            col.get_dense((v + 1) as u32)
                .expect("vertex column get")
                .base_slot_start()
        } else {
            elem_capacity_slots
        };
        let mut s = b;
        while s < end_exclusive {
            if slot_is_tombstone(s) {
                tombstone_sum += 1;
            }
            s = s.saturating_add(1);
        }
    }
    let next_starter = if i + 1 == segment_count as usize {
        elem_capacity_slots
    } else {
        let ni = (i + 1) * segment_size as usize;
        if ni >= nv {
            elem_capacity_slots
        } else {
            col.get_dense(ni as u32)
                .expect("vertex column get")
                .base_slot_start()
        }
    };
    let v0_row = col.get_dense(v0 as u32).expect("vertex column get v0");
    let segment_total_p = next_starter as i64 - v0_row.base_slot_start() as i64;
    SegmentEdgeCounts {
        actual: actual_sum,
        total: segment_total_p,
        tombstone: tombstone_sum,
    }
}

/// Recompute selected PMA leaves from the vertex column, write `SEC`, and re-aggregate ancestors for each leaf (ascending `segment_idx`).
pub fn refresh_segment_edge_counts_leaves<V, M, F, SecM: Memory>(
    col: &SlotMap<V, M>,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    elem_capacity_slots: u64,
    sec_mem: &SecM,
    stride: u64,
    segment_indices: &[usize],
    mut slot_is_tombstone: F,
) -> Result<(), &'static str>
where
    V: CsrVertex,
    M: Memory,
    F: FnMut(u64) -> bool,
{
    let sc = segment_count as usize;
    let mut sorted: Vec<usize> = segment_indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    for &seg in &sorted {
        if seg >= sc {
            return Err("segment index oob for segment_count");
        }
        let c = segment_edge_counts_leaf_from_column(
            col,
            num_vertices,
            segment_count,
            segment_size,
            elem_capacity_slots,
            seg,
            &mut slot_is_tombstone,
        );
        let leaf_j = leaf_pma_tree_index(seg, segment_count);
        write_segment_edge_counts(sec_mem, leaf_j, stride, c);
        reaggregate_segment_edge_counts_ancestors(sec_mem, stride, segment_count, leaf_j);
    }
    Ok(())
}

/// Recompute PMA leaves from the vertex column, then aggregate `actual` / `total` / `tombstone` for internal nodes.
///
/// `slot_is_tombstone` is invoked for every slab slot in each vertex span `[base, next_vertex_base)` inside
/// the leaf (including slots not covered by `degree`, e.g. orphaned tombstones after logical delete).
pub fn recount_segment_edge_counts_column<V, M, F>(
    col: &SlotMap<V, M>,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    elem_capacity_slots: u64,
    mut slot_is_tombstone: F,
    out: &mut [SegmentEdgeCounts],
) where
    V: CsrVertex,
    M: Memory,
    F: FnMut(u64) -> bool,
{
    let nv = num_vertices as usize;
    let sc = segment_count as usize;
    let n = sc * 2;
    if out.len() < n {
        return;
    }
    out.fill(SegmentEdgeCounts {
        actual: 0,
        total: 0,
        tombstone: 0,
    });
    for i in 0..segment_count as usize {
        let v0 = i * segment_size as usize;
        if v0 >= nv {
            continue;
        }
        let leaf = leaf_pma_tree_index(i, segment_count);
        if leaf < out.len() {
            out[leaf] = segment_edge_counts_leaf_from_column(
                col,
                num_vertices,
                segment_count,
                segment_size,
                elem_capacity_slots,
                i,
                &mut slot_is_tombstone,
            );
        }
    }
    for p in (1..sc).rev() {
        let l = p * 2;
        let r = l + 1;
        if r < n {
            out[p] = SegmentEdgeCounts {
                actual: out[l].actual.saturating_add(out[r].actual),
                total: out[l].total.saturating_add(out[r].total),
                tombstone: out[l].tombstone.saturating_add(out[r].tombstone),
            };
        }
    }
    if sc > 1 {
        out[0] = out[1];
    }
}

#[cfg(test)]
mod segment_maintain_tests {
    use super::{
        RebalanceDecision, SegmentMaintainAction, SegmentMaintainThresholds,
        segment_maintenance_decision,
    };
    use crate::layout::dgap::SegmentEdgeCounts;

    fn thr() -> SegmentMaintainThresholds {
        SegmentMaintainThresholds {
            soft_tombstone_ratio: 0.05,
            strict_tombstone_ratio: 0.25,
            soft_tombstone_score_threshold: 0.20,
            strict_tombstone_score_threshold: 0.60,
            queue_depth_inline_pressure: 64,
        }
    }

    #[test]
    fn noop_when_no_density_and_one_tombstone_below_soft_ratio() {
        let leaf = SegmentEdgeCounts {
            actual: 10,
            total: 100,
            tombstone: 1,
        };
        let a = segment_maintenance_decision(leaf, RebalanceDecision::Noop, 0, &thr());
        assert_eq!(a, SegmentMaintainAction::Noop);
    }

    #[test]
    fn enqueue_when_same_ratio_large_segment_crosses_score_threshold() {
        let leaf_small = SegmentEdgeCounts {
            actual: 29,
            total: 30,
            tombstone: 1,
        };
        assert_eq!(
            segment_maintenance_decision(leaf_small, RebalanceDecision::Noop, 0, &thr()),
            SegmentMaintainAction::Noop
        );

        let leaf_large = SegmentEdgeCounts {
            actual: 2900,
            total: 3000,
            tombstone: 100,
        };
        assert_eq!(
            segment_maintenance_decision(leaf_large, RebalanceDecision::Noop, 0, &thr()),
            SegmentMaintainAction::Enqueue
        );
    }

    #[test]
    fn enqueue_when_score_meets_soft_threshold() {
        let leaf = SegmentEdgeCounts {
            actual: 960,
            total: 1000,
            tombstone: 40,
        };
        let a = segment_maintenance_decision(leaf, RebalanceDecision::Noop, 0, &thr());
        assert_eq!(a, SegmentMaintainAction::Enqueue);
    }

    #[test]
    fn inline_when_score_meets_strict_threshold() {
        let leaf = SegmentEdgeCounts {
            actual: 900,
            total: 1000,
            tombstone: 100,
        };
        let a = segment_maintenance_decision(leaf, RebalanceDecision::Noop, 0, &thr());
        assert_eq!(a, SegmentMaintainAction::InlineNow);
    }

    #[test]
    fn inline_when_strict_ratio() {
        let leaf = SegmentEdgeCounts {
            actual: 7,
            total: 10,
            tombstone: 3,
        };
        let a = segment_maintenance_decision(leaf, RebalanceDecision::Noop, 0, &thr());
        assert_eq!(a, SegmentMaintainAction::InlineNow);
    }

    #[test]
    fn resize_always_inline() {
        let leaf = SegmentEdgeCounts {
            actual: 0,
            total: 1,
            tombstone: 0,
        };
        let a = segment_maintenance_decision(leaf, RebalanceDecision::ResizeNeeded, 0, &thr());
        assert_eq!(a, SegmentMaintainAction::InlineNow);
    }

    #[test]
    fn queue_pressure_inline_only_with_significant_garbage() {
        let leaf_low = SegmentEdgeCounts {
            actual: 999,
            total: 1000,
            tombstone: 1,
        };
        assert_eq!(
            segment_maintenance_decision(leaf_low, RebalanceDecision::Noop, 100, &thr()),
            SegmentMaintainAction::Noop
        );

        let leaf_ok = SegmentEdgeCounts {
            actual: 960,
            total: 1000,
            tombstone: 40,
        };
        assert_eq!(
            segment_maintenance_decision(leaf_ok, RebalanceDecision::Noop, 100, &thr()),
            SegmentMaintainAction::InlineNow
        );
    }

    #[test]
    fn rebalance_window_enqueues_without_tombstones() {
        let leaf = SegmentEdgeCounts {
            actual: 8,
            total: 10,
            tombstone: 0,
        };
        let reb = RebalanceDecision::RebalanceWindow {
            left_vertex: 0,
            right_vertex: 8,
            pma_idx: 1,
        };
        let a = segment_maintenance_decision(leaf, reb, 0, &thr());
        assert_eq!(a, SegmentMaintainAction::Enqueue);
    }

    #[test]
    fn total_zero_is_safe_and_noop() {
        let leaf = SegmentEdgeCounts {
            actual: 0,
            total: 0,
            tombstone: 1,
        };
        assert_eq!(super::tombstone_ratio(leaf), 0.0);
        assert_eq!(super::tombstone_score(leaf), 0.0);
        assert_eq!(
            segment_maintenance_decision(leaf, RebalanceDecision::Noop, 0, &thr()),
            SegmentMaintainAction::Noop
        );
    }
}

#[cfg(test)]
mod propagate_leaf_delta_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ic_stable_structures::vec_mem::VectorMemory;

    use crate::layout::dgap::{
        SegmentEdgeCounts, read_segment_edge_counts, required_segment_edge_counts_bytes,
        write_segment_edge_counts, write_segment_edge_counts_region_header,
    };

    use super::{leaf_pma_tree_index, propagate_segment_edge_counts_leaf_delta};

    fn make_sec_memory(sc: u32, stride: u64) -> VectorMemory {
        let n = required_segment_edge_counts_bytes(sc, stride).max(64) as usize;
        Rc::new(RefCell::new(vec![0u8; n]))
    }

    #[test]
    fn leaf_pma_tree_index_examples() {
        assert_eq!(leaf_pma_tree_index(0, 4), 4);
        assert_eq!(leaf_pma_tree_index(3, 4), 7);
    }

    #[test]
    fn stride_16_ignores_tombstone_delta() {
        let sc = 2u32;
        let stride = 16u64;
        let mem = make_sec_memory(sc, stride);
        write_segment_edge_counts_region_header(&mem);
        for j in 0..(sc as usize * 2) {
            write_segment_edge_counts(
                &mem,
                j,
                stride,
                SegmentEdgeCounts {
                    actual: 0,
                    total: 0,
                    tombstone: 0,
                },
            );
        }
        propagate_segment_edge_counts_leaf_delta(&mem, stride, sc, 0, 0, 0, 99).unwrap();
        let leaf = read_segment_edge_counts(&mem, leaf_pma_tree_index(0, sc), stride);
        assert_eq!(leaf.tombstone, 0);
    }

    #[test]
    fn actual_total_delta_propagates_to_root_sc4_stride24() {
        let sc = 4u32;
        let stride = 24u64;
        let mem = make_sec_memory(sc, stride);
        write_segment_edge_counts_region_header(&mem);
        let n = sc as usize * 2;
        for j in 0..n {
            write_segment_edge_counts(
                &mem,
                j,
                stride,
                SegmentEdgeCounts {
                    actual: 0,
                    total: 0,
                    tombstone: 0,
                },
            );
        }
        for sid in 0..4 {
            propagate_segment_edge_counts_leaf_delta(
                &mem,
                stride,
                sc,
                sid,
                (sid + 1) as i64,
                10,
                0,
            )
            .unwrap();
        }
        let root = read_segment_edge_counts(&mem, 1, stride);
        assert_eq!(root.actual, 10);
        assert_eq!(root.total, 40);
    }
}

#[cfg(test)]
mod rebalance_reader_parity_tests {
    use super::{floor_log2_u32, rebalance_decision, rebalance_decision_with_reader};

    #[test]
    fn reader_matches_slice_over_small_grids() {
        for sc in 2u32..=8 {
            let len = sc as usize * 2;
            let th = floor_log2_u32(sc.max(1));
            let ss = 4u32;
            let nv = (sc * ss) as usize;
            for pattern in 0..8u32 {
                let mut actual = vec![0i64; len];
                let mut total = vec![1i64; len];
                for j in 0..len {
                    actual[j] = ((j + pattern as usize) % 17) as i64;
                    total[j] = actual[j].max(1) + ((j * 7 + pattern as usize) % 20) as i64;
                }
                for vtx in 0..nv.min(32) {
                    let d1 = rebalance_decision(vtx as u32, ss, sc, nv, th, &actual, &total);
                    let d2 = rebalance_decision_with_reader(vtx as u32, ss, sc, nv, th, |i| {
                        (actual[i], total[i])
                    });
                    assert_eq!(d1, d2, "sc={sc} vtx={vtx} pattern={pattern}");
                }
            }
        }
    }
}

#[cfg(test)]
mod segments_for_vertex_range_tests {
    use super::segments_for_vertex_range;

    #[test]
    fn empty_range_yields_no_segments() {
        assert!(segments_for_vertex_range(3, 3, 4, 8).is_empty());
        assert!(segments_for_vertex_range(5, 2, 4, 8).is_empty());
    }

    #[test]
    fn single_vertex_mid_segment() {
        let s = segments_for_vertex_range(9, 10, 4, 8);
        assert_eq!(s, vec![2]);
    }

    #[test]
    fn segment_boundary_includes_predecessor() {
        let s = segments_for_vertex_range(8, 9, 4, 8);
        assert_eq!(s, vec![1, 2]);
    }

    #[test]
    fn span_two_segments() {
        let s = segments_for_vertex_range(6, 10, 4, 8);
        assert_eq!(s, vec![1, 2]);
    }

    #[test]
    fn first_vertex_no_predecessor() {
        let s = segments_for_vertex_range(0, 1, 4, 8);
        assert_eq!(s, vec![0]);
    }

    #[test]
    fn caps_at_segment_count() {
        let s = segments_for_vertex_range(0, 100, 4, 2);
        assert_eq!(s, vec![0, 1]);
    }
}

/// Path B prototype: vertex row `idx` covers `[bases[idx], next(idx))` with `next(i)=bases[i+1]` or
/// `elem_cap` for the last row. Overlap with slide `[slide_lo, slide_hi)` is
/// `bases[idx] < slide_hi && next(idx) > slide_lo`. On non-decreasing `bases`, the set of such `idx`
/// is an index interval `[start, k)` (`k` = count of `bases[i] < slide_hi`, `start` = first `i` with
/// `next(i) > slide_lo`), provably matching the linear scan (see `linear_matches_range_stub_on_random_monotone`).
#[cfg(test)]
mod remove_slab_dirty_overlap_tests {
    use std::collections::BTreeSet;

    fn next_base(bases: &[u64], i: usize, elem_cap: u64) -> u64 {
        if i + 1 < bases.len() {
            bases[i + 1]
        } else {
            elem_cap
        }
    }

    fn dirty_overlap_linear(
        bases: &[u64],
        elem_cap: u64,
        slide_lo: u64,
        slide_hi: u64,
    ) -> BTreeSet<usize> {
        let n = bases.len();
        let mut out = BTreeSet::new();
        for idx in 0..n {
            let b = bases[idx];
            let nx = next_base(bases, idx, elem_cap);
            if b < slide_hi && nx > slide_lo {
                out.insert(idx);
            }
        }
        out
    }

    fn dirty_overlap_range_stub(
        bases: &[u64],
        elem_cap: u64,
        slide_lo: u64,
        slide_hi: u64,
    ) -> BTreeSet<usize> {
        let n = bases.len();
        if n == 0 {
            return BTreeSet::new();
        }
        let next_b: Vec<u64> = (0..n).map(|i| next_base(bases, i, elem_cap)).collect();
        let k = bases.partition_point(|&b| b < slide_hi);
        let start = next_b.partition_point(|&nb| nb <= slide_lo);
        (start..k).collect()
    }

    #[test]
    fn linear_matches_range_stub_on_random_monotone() {
        let mut rng = 1u64;
        let mut next = 0u64;
        for _ in 0..200 {
            let n = (rng % 30 + 1) as usize;
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut bases = Vec::with_capacity(n);
            for _ in 0..n {
                let delta = (rng % 5) as u64;
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                next = next.saturating_add(delta);
                bases.push(next);
            }
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let span = next.saturating_add(20);
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let slide_lo = (rng % span.max(1)) as u64;
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut slide_hi = (rng % span.max(1)) as u64;
            if slide_hi <= slide_lo {
                slide_hi = slide_lo.saturating_add(1);
            }
            let elem_cap = span.saturating_mul(2).max(next.saturating_add(1));

            let a = dirty_overlap_linear(&bases, elem_cap, slide_lo, slide_hi);
            let b = dirty_overlap_range_stub(&bases, elem_cap, slide_lo, slide_hi);
            assert_eq!(
                a, b,
                "bases={bases:?} cap={elem_cap} lo={slide_lo} hi={slide_hi}"
            );
        }
    }
}
