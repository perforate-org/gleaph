//! V2 layout for **VCSR + DGAP overflow** edge region (`M_e`): header, PMA arrays, CSR slab,
//! per-leaf `log_segment_idx[]`, then per-leaf `LogEntry` pools.
//!
//! Matches [`gleaph-old/reference/DGAP/dgap/src/graph.h`](../../../../gleaph-old/reference/DGAP/dgap/src/graph.h)
//! segment logs (`MAX_LOG_ENTRIES`, `LogEntry` chain via `vertex.offset` / `prev_offset`).
//!
//! ```text
//! [0..64)                    header (v2)
//! [64..64+pma*2)             segment_edges_actual + segment_edges_total
//! [slab_base..)              CSR edge slab: elem_capacity * edge_stride
//! [log_idx_base..)           log_segment_idx[segment_count]  (i32 LE each)
//! [log_pool_base..)          segment_count * max_log_entries * log_entry_stride
//! ```

use ic_stable_structures::Memory;

use crate::memory_util::{
    read_i32_le, read_i64_le, read_u32_le, read_u64_le, write_i32_le, write_i64_le, write_u32_le,
    write_u64_le,
};

pub const EDGE_REGION_MAGIC: &[u8; 3] = b"VCE";
/// v2: DGAP per-segment log pools after the CSR slab.
pub const EDGE_REGION_VERSION: u8 = 2;
pub const EDGE_HEADER_SIZE: u64 = 64;

/// Default `MAX_LOG_ENTRIES` from DGAP `graph.h`.
pub const DGAP_DEFAULT_MAX_LOG_ENTRIES: u32 = 170;

/// Byte length of PMA arrays for `segment_count` leaves (`2 * segment_count` nodes each).
#[inline]
pub fn pma_array_bytes(segment_count: u32) -> u64 {
    let n = (segment_count as u64).saturating_mul(2);
    n.saturating_mul(8)
}

/// Start of CSR edge slab (after both PMA arrays).
#[inline]
pub fn csr_slab_base_offset(segment_count: u32) -> u64 {
    EDGE_HEADER_SIZE.saturating_add(pma_array_bytes(segment_count).saturating_mul(2))
}

/// First byte of `log_segment_idx[segment_count]` (i32 per leaf segment).
#[inline]
pub fn dgap_log_idx_base_offset(
    segment_count: u32,
    edge_stride: u32,
    elem_capacity: u64,
) -> u64 {
    csr_slab_base_offset(segment_count)
        .saturating_add(elem_capacity.saturating_mul(edge_stride as u64))
}

/// First byte of the packed log pool: `segment_count * max_log_entries * log_entry_stride`.
#[inline]
pub fn dgap_log_pool_base_offset(
    segment_count: u32,
    edge_stride: u32,
    elem_capacity: u64,
) -> u64 {
    dgap_log_idx_base_offset(segment_count, edge_stride, elem_capacity)
        .saturating_add((segment_count as u64).saturating_mul(4))
}

/// Fixed on-disk size for one DGAP log record: `prev` + `src_vid` + `edge` payload, 4-byte aligned.
#[inline]
pub fn dgap_log_entry_stride(edge_stride: u32) -> u32 {
    let raw = 8u32.saturating_add(edge_stride);
    (raw.saturating_add(3)) & !3u32
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VcsrEdgeHeaderV1 {
    pub elem_capacity: u64,
    pub segment_count: u32,
    pub segment_size: u32,
    pub tree_height: u32,
    pub num_edges: u64,
    pub edge_stride: u32,
    pub max_log_entries: u32,
    pub log_entry_stride: u32,
}

impl VcsrEdgeHeaderV1 {
    pub fn read<M: Memory>(memory: &M) -> Option<Self> {
        let mut magic = [0u8; 3];
        memory.read(0, &mut magic);
        if &magic != EDGE_REGION_MAGIC {
            return None;
        }
        let mut ver = [0u8; 1];
        memory.read(3, &mut ver);
        if ver[0] != EDGE_REGION_VERSION {
            return None;
        }
        Some(Self {
            elem_capacity: read_u64_le(memory, 8),
            segment_count: read_u32_le(memory, 16),
            segment_size: read_u32_le(memory, 20),
            tree_height: read_u32_le(memory, 24),
            num_edges: read_u64_le(memory, 32),
            edge_stride: read_u32_le(memory, 40),
            max_log_entries: read_u32_le(memory, 44),
            log_entry_stride: read_u32_le(memory, 48),
        })
    }

    pub fn write<M: Memory>(&self, memory: &M) {
        memory.write(0, EDGE_REGION_MAGIC);
        memory.write(3, &[EDGE_REGION_VERSION]);
        memory.write(4, &[0u8; 4]);
        write_u64_le(memory, 8, self.elem_capacity);
        write_u32_le(memory, 16, self.segment_count);
        write_u32_le(memory, 20, self.segment_size);
        write_u32_le(memory, 24, self.tree_height);
        memory.write(28, &[0u8; 4]);
        write_u64_le(memory, 32, self.num_edges);
        write_u32_le(memory, 40, self.edge_stride);
        write_u32_le(memory, 44, self.max_log_entries);
        write_u32_le(memory, 48, self.log_entry_stride);
        memory.write(52, &[0u8; 12]);
    }
}

#[inline]
pub fn actual_offset(segment_count: u32, j: usize) -> u64 {
    debug_assert!(j < (segment_count as usize) * 2);
    EDGE_HEADER_SIZE.saturating_add((j as u64).saturating_mul(8))
}

#[inline]
pub fn total_offset(segment_count: u32, j: usize) -> u64 {
    EDGE_HEADER_SIZE
        .saturating_add(pma_array_bytes(segment_count))
        .saturating_add((j as u64).saturating_mul(8))
}

pub fn read_actual<M: Memory>(memory: &M, segment_count: u32, j: usize) -> i64 {
    read_i64_le(memory, actual_offset(segment_count, j))
}

pub fn write_actual<M: Memory>(memory: &M, segment_count: u32, j: usize, v: i64) {
    write_i64_le(memory, actual_offset(segment_count, j), v);
}

pub fn read_total<M: Memory>(memory: &M, segment_count: u32, j: usize) -> i64 {
    read_i64_le(memory, total_offset(segment_count, j))
}

pub fn write_total<M: Memory>(memory: &M, segment_count: u32, j: usize, v: i64) {
    write_i64_le(memory, total_offset(segment_count, j), v);
}

#[inline]
pub fn edge_slot_offset(segment_count: u32, edge_stride: u32, slot: u64) -> u64 {
    csr_slab_base_offset(segment_count).saturating_add(slot.saturating_mul(edge_stride as u64))
}

/// PMA tree node index for `segment_edges_actual` / `total` (DGAP `get_segment_id`).
#[inline]
pub fn pma_node_id_for_vertex(vid: usize, segment_size: u32, segment_count: u32) -> usize {
    let leaf = (vid as u64) / (segment_size as u64);
    (leaf as usize).saturating_add(segment_count as usize)
}

/// Leaf segment index in `[0, segment_count)` for log arrays (`get_segment_id(v) - segment_count`).
#[inline]
pub fn dgap_leaf_segment_id(vid: usize, segment_size: u32) -> u32 {
    (vid as u64 / segment_size as u64) as u32
}

#[inline]
pub fn log_segment_idx_offset(
    segment_count: u32,
    edge_stride: u32,
    elem_capacity: u64,
    leaf_seg: u32,
) -> u64 {
    dgap_log_idx_base_offset(segment_count, edge_stride, elem_capacity)
        .saturating_add((leaf_seg as u64).saturating_mul(4))
}

#[inline]
pub fn log_entry_offset(
    segment_count: u32,
    edge_stride: u32,
    elem_capacity: u64,
    log_entry_stride: u32,
    max_log_entries: u32,
    leaf_seg: u32,
    entry_idx: u32,
) -> u64 {
    let pool = dgap_log_pool_base_offset(segment_count, edge_stride, elem_capacity);
    let row = (leaf_seg as u64)
        .saturating_mul(max_log_entries as u64)
        .saturating_add(entry_idx as u64);
    pool.saturating_add(row.saturating_mul(log_entry_stride as u64))
}

pub fn read_log_segment_idx<M: Memory>(
    memory: &M,
    h: &VcsrEdgeHeaderV1,
    leaf_seg: u32,
) -> i32 {
    let off = log_segment_idx_offset(
        h.segment_count,
        h.edge_stride,
        h.elem_capacity,
        leaf_seg,
    );
    read_i32_le(memory, off)
}

pub fn write_log_segment_idx<M: Memory>(
    memory: &M,
    h: &VcsrEdgeHeaderV1,
    leaf_seg: u32,
    v: i32,
) {
    let off = log_segment_idx_offset(
        h.segment_count,
        h.edge_stride,
        h.elem_capacity,
        leaf_seg,
    );
    write_i32_le(memory, off, v);
}

/// Total bytes: header + PMA + slab + log idx + log pool.
pub fn required_byte_len(h: &VcsrEdgeHeaderV1) -> u64 {
    let sc = h.segment_count as u64;
    let pool = sc
        .saturating_mul(h.max_log_entries as u64)
        .saturating_mul(h.log_entry_stride as u64);
    dgap_log_pool_base_offset(h.segment_count, h.edge_stride, h.elem_capacity).saturating_add(pool)
}
