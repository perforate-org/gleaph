//! **Edges + log** memory: after [`EDGE_PAYLOAD_HEADER_SIZE`] bytes ([`DgapEdgeHeaderV1`]), CSR `edges_` slab,
//! `log_segment_idx[]`, then `LogEntry` pool.
//!
//! # V1 payload layout (same `Memory` as [`super::header::DgapEdgeHeaderV1`], after the 64-byte header)
//!
//! Sizes depend on [`DgapEdgeHeaderV1`] fields (`elem_capacity`, `edge_stride`, `segment_count`,
//! `max_log_entries`, `log_entry_stride`). Offsets match [`edge_slab_slot_offset`], [`log_idx_base`],
//! [`log_pool_base`], [`log_entry_offset`].
//!
//! ```text
//! -------------------------------------------------- <- Address 0 (see `header` module for `VCE` header incl. `slab_occupied_tail`)
//! … [`DgapEdgeHeaderV1`] …               ↕ [`EDGE_PAYLOAD_HEADER_SIZE`] bytes
//! -------------------------------------------------- <- CSR slab base
//! Edge slot 0 … slot (elem_capacity - 1) ↕ elem_capacity × edge_stride bytes
//! -------------------------------------------------- <- [`log_idx_base`]
//! log_segment_idx[0 … segment_count-1]   ↕ segment_count × 4 bytes (i32 LE each)
//! -------------------------------------------------- <- [`log_pool_base`]
//! Log rows: segment_count × max_log_entries rows   ↕ each row `log_entry_stride` bytes
//!     (row-major by leaf segment, then entry index; see [`log_entry_offset`])
//! --------------------------------------------------
//! Unallocated space (until [`required_edges_and_log_bytes`])
//! ```

use ic_stable_structures::Memory;

use super::header::{DgapEdgeHeaderV1, EDGE_HEADER_SIZE};
use crate::memory_util::{read_i32_le, write_i32_le};

/// Same as [`EDGE_HEADER_SIZE`]: byte length of [`DgapEdgeHeaderV1`] at the start of this memory.
pub const EDGE_PAYLOAD_HEADER_SIZE: u64 = EDGE_HEADER_SIZE;

/// Default `MAX_LOG_ENTRIES` from DGAP `graph.h`.
pub const DGAP_DEFAULT_MAX_LOG_ENTRIES: u32 = 170;

#[inline]
pub fn dgap_log_entry_stride(edge_stride: u32) -> u32 {
    let raw = 8u32.saturating_add(edge_stride);
    (raw.saturating_add(3)) & !3u32
}

#[inline]
pub fn edge_slab_slot_offset(edge_stride: u32, slot: u64) -> u64 {
    EDGE_PAYLOAD_HEADER_SIZE.saturating_add(slot.saturating_mul(edge_stride as u64))
}

#[inline]
pub fn log_idx_base(elem_capacity: u64, edge_stride: u32) -> u64 {
    EDGE_PAYLOAD_HEADER_SIZE.saturating_add(elem_capacity.saturating_mul(edge_stride as u64))
}

#[inline]
pub fn log_pool_base(segment_count: u32, elem_capacity: u64, edge_stride: u32) -> u64 {
    log_idx_base(elem_capacity, edge_stride)
        .saturating_add((segment_count as u64).saturating_mul(4))
}

#[inline]
pub fn log_segment_idx_offset(elem_capacity: u64, edge_stride: u32, leaf_seg: u32) -> u64 {
    log_idx_base(elem_capacity, edge_stride).saturating_add((leaf_seg as u64).saturating_mul(4))
}

#[inline]
pub fn log_entry_offset(h: &DgapEdgeHeaderV1, leaf_seg: u32, entry_idx: u32) -> u64 {
    let pool = log_pool_base(h.segment_count, h.elem_capacity, h.edge_stride);
    let row = (leaf_seg as u64)
        .saturating_mul(h.max_log_entries as u64)
        .saturating_add(entry_idx as u64);
    pool.saturating_add(row.saturating_mul(h.log_entry_stride as u64))
}

pub fn read_log_segment_idx<M: Memory>(memory: &M, h: &DgapEdgeHeaderV1, leaf_seg: u32) -> i32 {
    let off = log_segment_idx_offset(h.elem_capacity, h.edge_stride, leaf_seg);
    read_i32_le(memory, off)
}

pub fn write_log_segment_idx<M: Memory>(memory: &M, h: &DgapEdgeHeaderV1, leaf_seg: u32, v: i32) {
    let off = log_segment_idx_offset(h.elem_capacity, h.edge_stride, leaf_seg);
    write_i32_le(memory, off, v);
}

pub fn required_edges_and_log_bytes(h: &DgapEdgeHeaderV1) -> u64 {
    let pool = (h.segment_count as u64)
        .saturating_mul(h.max_log_entries as u64)
        .saturating_mul(h.log_entry_stride as u64);
    EDGE_PAYLOAD_HEADER_SIZE.saturating_add(
        h.elem_capacity
            .saturating_mul(h.edge_stride as u64)
            .saturating_add((h.segment_count as u64).saturating_mul(4))
            .saturating_add(pool),
    )
}
