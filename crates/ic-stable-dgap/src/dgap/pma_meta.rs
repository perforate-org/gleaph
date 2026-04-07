//! PMA layer for DGAP (segment tree density, `calculate_positions_v1`, `rebalance_weighted`); mirrors reference `graph.h`.

use crate::csr::vertex_column::CsrVertexColumn;
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

/// Leaf PMA tree index for `vertex_id` (C++ `get_segment_id`).
#[inline]
pub fn pma_tree_index(vertex_id: u32, segment_size: u32, segment_count: u32) -> usize {
    (vertex_id / segment_size.max(1)) as usize + segment_count as usize
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
    let (delta_up, _) = density_deltas(tree_height);
    let mut height = 0i32;
    let mut window = pma_tree_index(src_vertex, segment_size, segment_count);
    if window >= len {
        return RebalanceDecision::Noop;
    }
    if total[window] <= 0 {
        return RebalanceDecision::Noop;
    }
    let mut density = actual[window] as f64 / total[window] as f64;
    let mut up_height = UP_0 - (height as f64) * delta_up;

    while window > 0 && density >= up_height {
        window /= 2;
        height += 1;
        up_height = UP_0 - (height as f64) * delta_up;
        if window >= len || total[window] <= 0 {
            break;
        }
        density = actual[window] as f64 / total[window] as f64;
    }

    if height == 0 {
        return RebalanceDecision::Noop;
    }

    if density < up_height {
        let window_size =
            (segment_size as usize).saturating_mul(1usize << (height as u32).max(0));
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

/// In-memory DGAP `rebalance_weighted` on a **vertex window** `vertices_win` (global indices `[left, right)`).
///
/// `next_base_after_window` is the first edge slot **after** the window (or `elem_capacity_slots` if `right == num_vertices`).
pub fn rebalance_weighted_window<V: CsrVertex, E: CsrEdge>(
    vertices_win: &mut [V],
    next_base_after_window: u64,
    edges: &mut [E],
    _elem_capacity_slots: u64,
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
    debug_assert!(
        new_index[w - 1].saturating_add(local_deg[w - 1] as u64) <= index_boundary
    );

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
            let mut read_index =
                vertices_win[j].base_slot_start() + vertices_win[j].degree() as u64 - 1;
            let last_read = vertices_win[j].base_slot_start();
            let mut write_index = new_index[j] + vertices_win[j].degree() as u64 - 1;
            while read_index >= last_read {
                edges[write_index as usize] = edges[read_index as usize];
                write_index -= 1;
                read_index -= 1;
            }
            vertices_win[j] = vertices_win[j].with_base_slot_start(new_index[j]);
            jj -= 1;
        }
        curr_li = next_to_start;
    }
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
        elem_capacity_slots,
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
    let sc = segment_count as usize;
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
        let mut j = i + sc;
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
        let leaf = i + sc;
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

/// [`recount_segment_total`] reading vertices through [`CsrVertexColumn`] (no `&[V]` snapshot).
pub fn recount_segment_total_column<V, C>(
    col: &C,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    elem_capacity_slots: u64,
    total: &mut [i64],
) where
    V: CsrVertex,
    C: CsrVertexColumn<V>,
{
    let nv = num_vertices as usize;
    total.fill(0);
    let sc = segment_count as usize;
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
                col.col_get(ni as u64)
                    .expect("vertex column get")
                    .base_slot_start()
            }
        };
        let v0_row = col
            .col_get(v0 as u64)
            .expect("vertex column get v0");
        let segment_total_p = next_starter as i64 - v0_row.base_slot_start() as i64;
        let mut j = i + sc;
        while j > 0 && j < total.len() {
            total[j] += segment_total_p;
            j /= 2;
        }
    }
}

/// [`recount_segment_actual_from_degrees`] via [`CsrVertexColumn`].
pub fn recount_segment_actual_column<V, C>(
    col: &C,
    num_vertices: u64,
    segment_count: u32,
    segment_size: u32,
    actual: &mut [i64],
) where
    V: CsrVertex,
    C: CsrVertexColumn<V>,
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
            sum += col
                .col_get(v as u64)
                .expect("vertex column get")
                .degree() as i64;
        }
        let leaf = i + sc;
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
