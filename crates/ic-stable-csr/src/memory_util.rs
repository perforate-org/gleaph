//! Grow-safe reads/writes for [`ic_stable_structures::Memory`].

use ic_stable_structures::Memory;
use std::error::Error;
use std::fmt::{Display, Formatter};

/// WebAssembly / IC stable memory page size (64 KiB).
pub const WASM_PAGE_SIZE: u64 = 65536;

#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    pub current_size_pages: u64,
    pub delta_pages: u64,
}

impl Display for GrowFailed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "memory grow failed: current_pages={}, delta_pages={}",
            self.current_size_pages, self.delta_pages
        )
    }
}

impl Error for GrowFailed {}

/// Writes `bytes` at `offset`, growing memory if necessary.
pub fn safe_write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
    let last = offset
        .checked_add(bytes.len() as u64)
        .expect("address overflow");
    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("address overflow");
    if size_bytes < last {
        let diff = last - size_bytes;
        let pages = diff.div_ceil(WASM_PAGE_SIZE);
        if memory.grow(pages) == -1 {
            return Err(GrowFailed {
                current_size_pages: size_pages,
                delta_pages: pages,
            });
        }
    }
    memory.write(offset, bytes);
    Ok(())
}

pub fn read_u64_le<M: Memory>(memory: &M, offset: u64) -> u64 {
    let mut b = [0u8; 8];
    memory.read(offset, &mut b);
    u64::from_le_bytes(b)
}

pub fn write_u64_le<M: Memory>(memory: &M, offset: u64, v: u64) {
    memory.write(offset, &v.to_le_bytes());
}

pub fn read_i64_le<M: Memory>(memory: &M, offset: u64) -> i64 {
    let mut b = [0u8; 8];
    memory.read(offset, &mut b);
    i64::from_le_bytes(b)
}

pub fn write_i64_le<M: Memory>(memory: &M, offset: u64, v: i64) {
    memory.write(offset, &v.to_le_bytes());
}

pub fn read_u32_le<M: Memory>(memory: &M, offset: u64) -> u32 {
    let mut b = [0u8; 4];
    memory.read(offset, &mut b);
    u32::from_le_bytes(b)
}

pub fn write_u32_le<M: Memory>(memory: &M, offset: u64, v: u32) {
    memory.write(offset, &v.to_le_bytes());
}

pub fn read_i32_le<M: Memory>(memory: &M, offset: u64) -> i32 {
    let mut b = [0u8; 4];
    memory.read(offset, &mut b);
    i32::from_le_bytes(b)
}

pub fn write_i32_le<M: Memory>(memory: &M, offset: u64, v: i32) {
    memory.write(offset, &v.to_le_bytes());
}

pub fn memory_byte_len<M: Memory>(memory: &M) -> u64 {
    memory.size().saturating_mul(WASM_PAGE_SIZE)
}

impl From<ic_stable_vec_deque::GrowFailed> for GrowFailed {
    fn from(e: ic_stable_vec_deque::GrowFailed) -> Self {
        Self {
            current_size_pages: e.current_size_pages(),
            delta_pages: e.delta_pages(),
        }
    }
}
