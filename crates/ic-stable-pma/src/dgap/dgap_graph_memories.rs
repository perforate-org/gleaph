//! Three [`ic_stable_structures::Memory`] handles for DGAP-aligned edge state (`M_e`).
//!
//! Typically these are three [`VirtualMemory`](ic_stable_structures::memory_manager::VirtualMemory) values
//! from one [`MemoryManager`](ic_stable_structures::memory_manager::MemoryManager) (distinct [`MemoryId`] per
//! region), or three [`VectorMemory`](ic_stable_structures::vec_mem::VectorMemory) instances in tests.
//!
//! ```text
//! [`DgapGraphMemories::segment_edges_actual`]  — Memory #1
//! --------------------------------------------------
//! | `VCA` V1 mini header + `segment_edges_actual` PMA `i64` array (`layout::dgap`)    |
//! --------------------------------------------------
//!
//! [`DgapGraphMemories::segment_edges_total`]   — Memory #2
//! --------------------------------------------------
//! | `VCT` V1 mini header + `segment_edges_total` PMA `i64` array (`layout::dgap`)     |
//! --------------------------------------------------
//!
//! [`DgapGraphMemories::edges_and_log_segment`] — Memory #3
//! --------------------------------------------------
//! | `VCE` [`crate::layout::dgap::DgapEdgeHeaderV1`] + CSR slab + log idx + log pool |
//! --------------------------------------------------
//! ```

use ic_stable_structures::Memory;

use crate::layout::dgap::{
    edge_slab_slot_offset, log_entry_offset, read_actual as read_actual_arr, read_log_segment_idx,
    read_total as read_total_arr, write_actual as write_actual_arr, write_log_segment_idx,
    write_segment_edges_actual_region_header, write_segment_edges_total_region_header,
    write_total as write_total_arr, DgapEdgeHeaderV1,
};
use crate::memory_util::{read_i32_le, read_u64_le, safe_write, write_i32_le, write_u64_le, GrowFailed};

#[derive(Clone, Debug)]
pub struct DgapGraphMemories<M1, M2, M3> {
    pub segment_edges_actual: M1,
    pub segment_edges_total: M2,
    pub edges_and_log_segment: M3,
}

impl<M1: Memory, M2: Memory, M3: Memory> DgapGraphMemories<M1, M2, M3> {
    pub fn new(segment_edges_actual: M1, segment_edges_total: M2, edges_and_log_segment: M3) -> Self {
        Self {
            segment_edges_actual,
            segment_edges_total,
            edges_and_log_segment,
        }
    }

    pub fn read_header(&self) -> Option<DgapEdgeHeaderV1> {
        DgapEdgeHeaderV1::read(&self.edges_and_log_segment)
    }

    pub fn write_header(&self, h: &DgapEdgeHeaderV1) {
        h.write(&self.edges_and_log_segment);
    }

    pub fn read_actual(&self, j: usize) -> i64 {
        read_actual_arr(&self.segment_edges_actual, j)
    }

    pub fn write_actual(&self, j: usize, v: i64) {
        write_actual_arr(&self.segment_edges_actual, j, v);
    }

    pub fn read_total(&self, j: usize) -> i64 {
        read_total_arr(&self.segment_edges_total, j)
    }

    pub fn write_total(&self, j: usize, v: i64) {
        write_total_arr(&self.segment_edges_total, j, v);
    }

    pub fn read_edge_slab(&self, edge_stride: u32, slot: u64, out: &mut [u8]) {
        let off = edge_slab_slot_offset(edge_stride, slot);
        self.edges_and_log_segment.read(off, out);
    }

    pub fn write_edge_slab(
        &self,
        edge_stride: u32,
        slot: u64,
        bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        let off = edge_slab_slot_offset(edge_stride, slot);
        safe_write(&self.edges_and_log_segment, off, bytes)
    }

    pub fn read_log_idx(&self, h: &DgapEdgeHeaderV1, leaf_seg: u32) -> i32 {
        read_log_segment_idx(&self.edges_and_log_segment, h, leaf_seg)
    }

    pub fn write_log_idx(&self, h: &DgapEdgeHeaderV1, leaf_seg: u32, v: i32) {
        write_log_segment_idx(&self.edges_and_log_segment, h, leaf_seg, v);
    }

    pub fn read_log_entry_raw(
        &self,
        h: &DgapEdgeHeaderV1,
        leaf_seg: u32,
        entry_idx: u32,
        edge_bytes: usize,
    ) -> (i32, i32, Vec<u8>) {
        let off = log_entry_offset(h, leaf_seg, entry_idx);
        let prev = read_i32_le(&self.edges_and_log_segment, off);
        let src = read_i32_le(&self.edges_and_log_segment, off + 4);
        let mut eb = vec![0u8; edge_bytes];
        self.edges_and_log_segment.read(off + 8, &mut eb);
        (prev, src, eb)
    }

    pub fn write_log_entry_raw(
        &self,
        h: &DgapEdgeHeaderV1,
        leaf_seg: u32,
        entry_idx: u32,
        prev: i32,
        src_vid: i32,
        edge_payload: &[u8],
    ) -> Result<(), GrowFailed> {
        let off = log_entry_offset(h, leaf_seg, entry_idx);
        write_i32_le(&self.edges_and_log_segment, off, prev);
        write_i32_le(&self.edges_and_log_segment, off + 4, src_vid);
        let mut z = vec![0u8; h.log_entry_stride as usize];
        z[..edge_payload.len()].copy_from_slice(edge_payload);
        safe_write(&self.edges_and_log_segment, off + 8, &z)
    }

    pub fn zero_log_entry_slot(
        &self,
        h: &DgapEdgeHeaderV1,
        leaf_seg: u32,
        entry_idx: u32,
    ) -> Result<(), GrowFailed> {
        let off = log_entry_offset(h, leaf_seg, entry_idx);
        let z = vec![0u8; h.log_entry_stride as usize];
        safe_write(&self.edges_and_log_segment, off, &z)
    }

    pub fn set_num_edges(&self, n: u64) {
        write_u64_le(&self.edges_and_log_segment, 32, n);
    }

    pub fn read_num_edges(&self) -> u64 {
        read_u64_le(&self.edges_and_log_segment, 32)
    }

    pub fn grow_edges_and_log_to(&self, need: u64) -> Result<(), GrowFailed> {
        let m = &self.edges_and_log_segment;
        let cur = crate::memory_util::memory_byte_len(m);
        if need > cur {
            safe_write(m, cur, &vec![0u8; (need - cur) as usize])?;
        }
        Ok(())
    }

    pub fn grow_all_regions_for_header(&self, h: &DgapEdgeHeaderV1) -> Result<(), GrowFailed> {
        use crate::layout::dgap::{
            required_edges_and_log_bytes, required_segment_edges_actual_bytes,
            required_segment_edges_total_bytes,
        };
        grow_memory_to(
            &self.segment_edges_actual,
            required_segment_edges_actual_bytes(h.segment_count),
        )?;
        grow_memory_to(
            &self.segment_edges_total,
            required_segment_edges_total_bytes(h.segment_count),
        )?;
        grow_memory_to(&self.edges_and_log_segment, required_edges_and_log_bytes(h))?;
        write_segment_edges_actual_region_header(&self.segment_edges_actual);
        write_segment_edges_total_region_header(&self.segment_edges_total);
        Ok(())
    }

    pub fn zero_log_partition(&self, h: &DgapEdgeHeaderV1) -> Result<(), GrowFailed> {
        use crate::layout::dgap::{log_idx_base, log_pool_base, required_edges_and_log_bytes};
        let m = &self.edges_and_log_segment;
        let idx_base = log_idx_base(h.elem_capacity, h.edge_stride);
        let idx_bytes = (h.segment_count as u64).saturating_mul(4);
        safe_write(m, idx_base, &vec![0u8; idx_bytes as usize])?;
        let pool_base = log_pool_base(h.segment_count, h.elem_capacity, h.edge_stride);
        let end = required_edges_and_log_bytes(h);
        let pool_bytes = end.saturating_sub(pool_base);
        safe_write(m, pool_base, &vec![0u8; pool_bytes as usize])?;
        Ok(())
    }
}

fn grow_memory_to<M: Memory>(memory: &M, need: u64) -> Result<(), GrowFailed> {
    let cur = crate::memory_util::memory_byte_len(memory);
    if need > cur {
        safe_write(memory, cur, &vec![0u8; (need - cur) as usize])?;
    }
    Ok(())
}
