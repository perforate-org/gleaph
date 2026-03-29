use crate::memory::Memory;
use thiserror::Error;

pub const GRAPH_PMA_MAGIC: [u8; 8] = *b"GLPHGPMA";
pub const GRAPH_PMA_VERSION: u32 = 3;
pub const HEADER_SIZE: usize = 88;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphLayoutHeader {
    pub version: u32,
    pub node_count: u64,
    pub edge_count: u64,
    pub node_bytes_len: u64,
    pub edge_bytes_len: u64,
    pub adjacency_bytes_len: u64,
    pub property_bytes_len: u64,
    pub property_index_bytes_len: u64,
    pub op_log_bytes_len: u64,
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("invalid graph-pma header magic")]
    InvalidMagic,
    #[error("unsupported graph-pma version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid graph-pma payload")]
    InvalidPayload,
    #[error("unexpected end of graph-pma payload")]
    UnexpectedEof,
}

pub type LayoutResult<T> = Result<T, LayoutError>;

impl GraphLayoutHeader {
    pub fn new() -> Self {
        Self {
            version: GRAPH_PMA_VERSION,
            node_count: 0,
            edge_count: 0,
            node_bytes_len: 0,
            edge_bytes_len: 0,
            adjacency_bytes_len: 0,
            property_bytes_len: 0,
            property_index_bytes_len: 0,
            op_log_bytes_len: 0,
        }
    }

    pub fn write_into<M: Memory>(&self, memory: &mut M) {
        if memory.len() < HEADER_SIZE {
            memory.resize(HEADER_SIZE);
        }

        memory.write(0, &GRAPH_PMA_MAGIC);
        memory.write(8, &self.version.to_le_bytes());
        memory.write(16, &self.node_count.to_le_bytes());
        memory.write(24, &self.edge_count.to_le_bytes());
        memory.write(32, &self.node_bytes_len.to_le_bytes());
        memory.write(40, &self.edge_bytes_len.to_le_bytes());
        memory.write(48, &self.adjacency_bytes_len.to_le_bytes());
        memory.write(56, &self.property_bytes_len.to_le_bytes());
        memory.write(64, &self.property_index_bytes_len.to_le_bytes());
        memory.write(72, &self.op_log_bytes_len.to_le_bytes());
    }

    pub fn read_from<M: Memory>(memory: &M) -> LayoutResult<Self> {
        let mut magic = [0u8; 8];
        memory.read(0, &mut magic);
        if magic != GRAPH_PMA_MAGIC {
            return Err(LayoutError::InvalidMagic);
        }

        let version = read_u32(memory, 8);
        if version != GRAPH_PMA_VERSION {
            return Err(LayoutError::UnsupportedVersion(version));
        }

        Ok(Self {
            version,
            node_count: read_u64(memory, 16),
            edge_count: read_u64(memory, 24),
            node_bytes_len: read_u64(memory, 32),
            edge_bytes_len: read_u64(memory, 40),
            adjacency_bytes_len: read_u64(memory, 48),
            property_bytes_len: read_u64(memory, 56),
            property_index_bytes_len: read_u64(memory, 64),
            op_log_bytes_len: read_u64(memory, 72),
        })
    }
}

fn read_u32<M: Memory>(memory: &M, offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    memory.read(offset, &mut bytes);
    u32::from_le_bytes(bytes)
}

fn read_u64<M: Memory>(memory: &M, offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    memory.read(offset, &mut bytes);
    u64::from_le_bytes(bytes)
}
