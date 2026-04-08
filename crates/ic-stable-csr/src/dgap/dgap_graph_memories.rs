//! Two [`ic_stable_structures::Memory`] handles for DGAP-aligned edge state (`M_e`).
//!
//! Typically these are two [`VirtualMemory`](ic_stable_structures::memory_manager::VirtualMemory) values
//! from one [`MemoryManager`](ic_stable_structures::memory_manager::MemoryManager) (distinct [`MemoryId`] per
//! region), or two [`VectorMemory`](ic_stable_structures::vec_mem::VectorMemory) instances in tests.
//!
//! ```text
//! [`DgapGraphMemories::segment_edge_counts`] — Memory #1
//! --------------------------------------------------
//! | `SEC` V1 mini header + packed [`SegmentEdgeCounts`] PMA tree (`layout::dgap`)     |
//! --------------------------------------------------
//!
//! [`DgapGraphMemories::edges_and_log_segment`] — Memory #2
//! --------------------------------------------------
//! | `VCE` [`crate::layout::dgap::DgapEdgeHeaderV1`] + CSR slab + log idx + log pool |
//! --------------------------------------------------
//! ```

use ic_stable_structures::Memory;

use crate::layout::dgap::{
    DgapEdgeHeaderV1, SegmentEdgeCounts, edge_slab_slot_offset, log_entry_offset,
    read_log_segment_idx, read_segment_edge_counts, required_segment_edge_counts_bytes,
    write_log_segment_idx, write_segment_edge_counts, write_segment_edge_counts_region_header,
};
use crate::memory_util::{
    GrowFailed, read_i32_le, read_u64_le, safe_write, write_i32_le, write_u64_le,
};

#[derive(Clone, Debug)]
pub struct DgapGraphMemories<M1, M2> {
    pub segment_edge_counts: M1,
    pub edges_and_log_segment: M2,
}

impl<M1: Memory, M2: Memory> DgapGraphMemories<M1, M2> {
    pub fn new(segment_edge_counts: M1, edges_and_log_segment: M2) -> Self {
        Self {
            segment_edge_counts,
            edges_and_log_segment,
        }
    }

    pub fn read_header(&self) -> Option<DgapEdgeHeaderV1> {
        DgapEdgeHeaderV1::read(&self.edges_and_log_segment)
    }

    pub fn write_header(&self, h: &DgapEdgeHeaderV1) {
        h.write(&self.edges_and_log_segment);
    }

    pub fn read_segment_edge_counts(&self, j: usize, stride: u64) -> SegmentEdgeCounts {
        read_segment_edge_counts(&self.segment_edge_counts, j, stride)
    }

    pub fn write_segment_edge_counts(&self, j: usize, stride: u64, c: SegmentEdgeCounts) {
        write_segment_edge_counts(&self.segment_edge_counts, j, stride, c);
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

    /// Writes `n` contiguous CSR slab slots starting at `start_slot` in a single [`safe_write`].
    ///
    /// **Contract:** `payload.len() == n * edge_stride` for some `n` (if `payload` is empty, this is a no-op).
    /// The caller must ensure `start_slot + n <= elem_capacity` and preserve PMA / vertex-boundary invariants;
    /// this method only writes bytes. Vertex `degree` and [`Self::set_num_edges`] are the caller's duty.
    pub fn write_edge_slab_span(
        &self,
        edge_stride: u32,
        start_slot: u64,
        payload: &[u8],
    ) -> Result<(), GrowFailed> {
        let st = edge_stride as usize;
        if st == 0 || payload.is_empty() {
            return Ok(());
        }
        assert!(
            payload.len().is_multiple_of(st),
            "write_edge_slab_span: payload length must be a multiple of edge_stride"
        );
        let off = edge_slab_slot_offset(edge_stride, start_slot);
        safe_write(&self.edges_and_log_segment, off, payload)
    }

    pub fn read_log_idx(&self, h: &DgapEdgeHeaderV1, leaf_seg: u32) -> i32 {
        read_log_segment_idx(&self.edges_and_log_segment, h, leaf_seg)
    }

    pub fn write_log_idx(&self, h: &DgapEdgeHeaderV1, leaf_seg: u32, v: i32) {
        write_log_segment_idx(&self.edges_and_log_segment, h, leaf_seg, v)
    }

    /// Reads `prev`, `src`, and the edge payload into `edge_out` (length must equal `edge_bytes`).
    pub fn read_log_entry_raw_into(
        &self,
        h: &DgapEdgeHeaderV1,
        leaf_seg: u32,
        entry_idx: u32,
        edge_out: &mut [u8],
    ) -> (i32, i32) {
        let off = log_entry_offset(h, leaf_seg, entry_idx);
        let prev = read_i32_le(&self.edges_and_log_segment, off);
        let src = read_i32_le(&self.edges_and_log_segment, off + 4);
        self.edges_and_log_segment.read(off + 8, edge_out);
        (prev, src)
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

    /// Patch only [`DgapEdgeHeaderV1::slab_occupied_tail`] (offset 52); other header fields unchanged.
    pub fn set_slab_occupied_tail(&self, tail: u64) {
        write_u64_le(&self.edges_and_log_segment, 52, tail);
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

    pub fn grow_all_regions_for_header(
        &self,
        h: &DgapEdgeHeaderV1,
        pma_stride: u64,
    ) -> Result<(), GrowFailed> {
        use crate::layout::dgap::required_edges_and_log_bytes;
        grow_memory_to(
            &self.segment_edge_counts,
            required_segment_edge_counts_bytes(h.segment_count, pma_stride),
        )?;
        grow_memory_to(&self.edges_and_log_segment, required_edges_and_log_bytes(h))?;
        write_segment_edge_counts_region_header(&self.segment_edge_counts);
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
