//! Unified PMA region: per-tree-node [`SegmentEdgeCounts`] (`SEC` mini header + packed nodes).

use ic_stable_structures::Memory;

use super::segment_arrays::PMA_REGION_HEADER_SIZE;
use crate::memory_util::{read_i64_le, write_i64_le};

pub const PMA_SEGMENT_EDGE_COUNTS_MAGIC: &[u8; 3] = b"SEC";

/// Packed PMA counts for one segment-tree node (leaf = vertex block, internal = sum of children).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentEdgeCounts {
    pub actual: i64,
    pub total: i64,
    pub tombstone: i64,
}

#[inline]
pub fn pma_edge_counts_array_bytes(segment_count: u32, stride: u64) -> u64 {
    let n = (segment_count as u64).saturating_mul(2);
    n.saturating_mul(stride)
}

#[inline]
pub fn required_segment_edge_counts_bytes(segment_count: u32, stride: u64) -> u64 {
    PMA_REGION_HEADER_SIZE.saturating_add(pma_edge_counts_array_bytes(segment_count, stride))
}

#[inline]
pub fn segment_edge_counts_node_offset(j: usize, stride: u64) -> u64 {
    PMA_REGION_HEADER_SIZE.saturating_add((j as u64).saturating_mul(stride))
}

pub fn write_segment_edge_counts_region_header<M: Memory>(memory: &M) {
    memory.write(0, PMA_SEGMENT_EDGE_COUNTS_MAGIC);
    memory.write(3, &[super::segment_arrays::PMA_REGION_VERSION]);
    memory.write(4, &[0u8; 12]);
}

pub fn segment_edge_counts_region_ok<M: Memory>(memory: &M) -> bool {
    let mut magic = [0u8; 3];
    memory.read(0, &mut magic);
    if &magic != PMA_SEGMENT_EDGE_COUNTS_MAGIC {
        return false;
    }
    let mut ver = [0u8; 1];
    memory.read(3, &mut ver);
    ver[0] == super::segment_arrays::PMA_REGION_VERSION
}

pub fn read_segment_edge_counts<M: Memory>(memory: &M, j: usize, stride: u64) -> SegmentEdgeCounts {
    let off = segment_edge_counts_node_offset(j, stride);
    let actual = read_i64_le(memory, off);
    let total = read_i64_le(memory, off + 8);
    let tombstone = if stride >= 24 {
        read_i64_le(memory, off + 16)
    } else {
        0
    };
    SegmentEdgeCounts {
        actual,
        total,
        tombstone,
    }
}

pub fn write_segment_edge_counts<M: Memory>(
    memory: &M,
    j: usize,
    stride: u64,
    c: SegmentEdgeCounts,
) {
    let off = segment_edge_counts_node_offset(j, stride);
    write_i64_le(memory, off, c.actual);
    write_i64_le(memory, off + 8, c.total);
    if stride >= 24 {
        write_i64_le(memory, off + 16, c.tombstone);
    }
}
