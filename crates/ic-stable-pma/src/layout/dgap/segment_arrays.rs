//! `segment_edges_actual` / `segment_edges_total` memories: **mini header** + `i64` PMA array.
//! Scalars such as `segment_count` live only on the edges+log memory ([`super::header::DgapEdgeHeaderV1`]).
//!
//! # V1 mini header (`VCA` / `VCT`, [`PMA_REGION_VERSION`])
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "VCA" or "VCT"                  ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Reserved                              ↕ 12 bytes
//! -------------------------------------------------- <- Address 16 ([`PMA_REGION_HEADER_SIZE`])
//! PMA tree node 0 (i64 LE)              ↕ 8 bytes
//! --------------------------------------------------
//! PMA tree node 1 (i64 LE)              ↕ 8 bytes
//! --------------------------------------------------
//! … (2 * segment_count nodes, segment tree layout) …
//! --------------------------------------------------
//! Unallocated space
//! ```

use ic_stable_structures::Memory;

use crate::memory_util::{read_i64_le, write_i64_le};

pub const PMA_SEGMENT_EDGES_ACTUAL_MAGIC: &[u8; 3] = b"VCA";
pub const PMA_SEGMENT_EDGES_TOTAL_MAGIC: &[u8; 3] = b"VCT";
pub const PMA_REGION_VERSION: u8 = 1;
/// Mini header: magic (3) + version (1) + reserved (12) — no duplicate of graph scalars.
pub const PMA_REGION_HEADER_SIZE: u64 = 16;

#[inline]
pub fn pma_array_bytes(segment_count: u32) -> u64 {
    let n = (segment_count as u64).saturating_mul(2);
    n.saturating_mul(8)
}

#[inline]
pub fn required_segment_edges_actual_bytes(segment_count: u32) -> u64 {
    PMA_REGION_HEADER_SIZE.saturating_add(pma_array_bytes(segment_count))
}

#[inline]
pub fn required_segment_edges_total_bytes(segment_count: u32) -> u64 {
    PMA_REGION_HEADER_SIZE.saturating_add(pma_array_bytes(segment_count))
}

#[inline]
pub fn segment_array_data_offset(j: usize) -> u64 {
    PMA_REGION_HEADER_SIZE.saturating_add((j as u64).saturating_mul(8))
}

pub fn write_segment_edges_actual_region_header<M: Memory>(memory: &M) {
    memory.write(0, PMA_SEGMENT_EDGES_ACTUAL_MAGIC);
    memory.write(3, &[PMA_REGION_VERSION]);
    memory.write(4, &[0u8; 12]);
}

pub fn write_segment_edges_total_region_header<M: Memory>(memory: &M) {
    memory.write(0, PMA_SEGMENT_EDGES_TOTAL_MAGIC);
    memory.write(3, &[PMA_REGION_VERSION]);
    memory.write(4, &[0u8; 12]);
}

pub fn segment_edges_actual_region_ok<M: Memory>(memory: &M) -> bool {
    let mut magic = [0u8; 3];
    memory.read(0, &mut magic);
    if &magic != PMA_SEGMENT_EDGES_ACTUAL_MAGIC {
        return false;
    }
    let mut ver = [0u8; 1];
    memory.read(3, &mut ver);
    ver[0] == PMA_REGION_VERSION
}

pub fn segment_edges_total_region_ok<M: Memory>(memory: &M) -> bool {
    let mut magic = [0u8; 3];
    memory.read(0, &mut magic);
    if &magic != PMA_SEGMENT_EDGES_TOTAL_MAGIC {
        return false;
    }
    let mut ver = [0u8; 1];
    memory.read(3, &mut ver);
    ver[0] == PMA_REGION_VERSION
}

pub fn read_actual<M: Memory>(memory: &M, j: usize) -> i64 {
    read_i64_le(memory, segment_array_data_offset(j))
}

pub fn write_actual<M: Memory>(memory: &M, j: usize, v: i64) {
    write_i64_le(memory, segment_array_data_offset(j), v);
}

pub fn read_total<M: Memory>(memory: &M, j: usize) -> i64 {
    read_i64_le(memory, segment_array_data_offset(j))
}

pub fn write_total<M: Memory>(memory: &M, j: usize, v: i64) {
    write_i64_le(memory, segment_array_data_offset(j), v);
}

#[inline]
pub fn pma_node_id_for_vertex(vid: usize, segment_size: u32, segment_count: u32) -> usize {
    let leaf = (vid as u64) / (segment_size as u64);
    (leaf as usize).saturating_add(segment_count as usize)
}

#[inline]
pub fn dgap_leaf_segment_id(vid: usize, segment_size: u32) -> u32 {
    (vid as u64 / segment_size as u64) as u32
}
