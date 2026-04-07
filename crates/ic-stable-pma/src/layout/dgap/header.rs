//! **Edges + log** [`Memory`] (`edges_and_log_segment`): DGAP `Base` scalars as [`DgapEdgeHeaderV1`] at offset 0
//! (C++ `Base` / `graph.h`). CSR slab and log arrays follow after [`super::edges_and_log::EDGE_PAYLOAD_HEADER_SIZE`].
//!
//! # V1 layout (`VCE`, [`EDGE_REGION_VERSION`])
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "VCE"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Reserved                              ↕ 4 bytes
//! --------------------------------------------------
//! elem_capacity (u64 LE)                ↕ 8 bytes
//! --------------------------------------------------
//! segment_count (u32 LE)                ↕ 4 bytes
//! --------------------------------------------------
//! segment_size (u32 LE)                 ↕ 4 bytes
//! --------------------------------------------------
//! tree_height (u32 LE)                  ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 4 bytes
//! --------------------------------------------------
//! num_edges (u64 LE)                    ↕ 8 bytes
//! --------------------------------------------------
//! edge_stride (u32 LE)                  ↕ 4 bytes
//! --------------------------------------------------
//! max_log_entries (u32 LE)              ↕ 4 bytes
//! --------------------------------------------------
//! log_entry_stride (u32 LE)             ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 12 bytes
//! -------------------------------------------------- <- Address 64 ([`EDGE_HEADER_SIZE`])
//! CSR edge slab, log idx[], log pool … (see `edges_and_log`)
//! ```

use ic_stable_structures::Memory;

use crate::memory_util::{read_u32_le, read_u64_le, write_u32_le, write_u64_le};

pub const EDGE_REGION_MAGIC: &[u8; 3] = b"VCE";
pub const EDGE_REGION_VERSION: u8 = 4;
pub const EDGE_HEADER_SIZE: u64 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DgapEdgeHeaderV1 {
    pub elem_capacity: u64,
    pub segment_count: u32,
    pub segment_size: u32,
    pub tree_height: u32,
    pub num_edges: u64,
    pub edge_stride: u32,
    pub max_log_entries: u32,
    pub log_entry_stride: u32,
}

impl DgapEdgeHeaderV1 {
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
