//! Stable-memory read/write/grow helpers, mirroring `ic-stable-vec-deque` (which mirrors
//! `ic-stable-structures`). All offsets are byte offsets into a [`Memory`] region.

use ic_stable_structures::Memory;
use std::error;
use std::fmt::{Display, Formatter};

pub(crate) const WASM_PAGE_SIZE: u64 = 65536;

/// Reads a single 32-bit little-endian integer at `offset`.
pub(crate) fn read_u32<M: Memory>(m: &M, offset: u64) -> u32 {
    let mut buf = [0u8; 4];
    m.read(offset, &mut buf);
    u32::from_le_bytes(buf)
}

/// Reads a single 64-bit little-endian integer at `offset`.
pub(crate) fn read_u64<M: Memory>(m: &M, offset: u64) -> u64 {
    let mut buf = [0u8; 8];
    m.read(offset, &mut buf);
    u64::from_le_bytes(buf)
}

/// Writes a single 32-bit little-endian integer at `offset`.
pub(crate) fn write_u32<M: Memory>(m: &M, offset: u64, val: u32) {
    write(m, offset, &val.to_le_bytes());
}

/// Writes a single 64-bit little-endian integer at `offset`.
pub(crate) fn write_u64<M: Memory>(m: &M, offset: u64, val: u64) {
    write(m, offset, &val.to_le_bytes());
}

/// Stable memory grow failed while writing the hash map.
#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    current_size: u64,
    delta: u64,
}

impl Display for GrowFailed {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to grow memory: current size={}, delta={}",
            self.current_size, self.delta
        )
    }
}

impl error::Error for GrowFailed {}

/// Grows `memory` so its size in bytes is at least `min_bytes`.
pub(crate) fn grow_memory_to_at_least_bytes<M: Memory>(
    memory: &M,
    min_bytes: u64,
) -> Result<(), GrowFailed> {
    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("Address space overflow");
    if size_bytes >= min_bytes {
        return Ok(());
    }
    let diff_bytes = min_bytes - size_bytes;
    let diff_pages = diff_bytes
        .checked_add(WASM_PAGE_SIZE - 1)
        .expect("Address space overflow")
        / WASM_PAGE_SIZE;
    if memory.grow(diff_pages) == -1 {
        return Err(GrowFailed {
            current_size: size_pages,
            delta: diff_pages,
        });
    }
    Ok(())
}

/// Writes the bytes at the specified offset, growing the memory size if needed.
pub(crate) fn safe_write<M: Memory>(
    memory: &M,
    offset: u64,
    bytes: &[u8],
) -> Result<(), GrowFailed> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .expect("Address space overflow");

    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("Address space overflow");

    if size_bytes < last_byte {
        let diff_bytes = last_byte - size_bytes;
        let diff_pages = diff_bytes
            .checked_add(WASM_PAGE_SIZE - 1)
            .expect("Address space overflow")
            / WASM_PAGE_SIZE;
        if memory.grow(diff_pages) == -1 {
            return Err(GrowFailed {
                current_size: size_pages,
                delta: diff_pages,
            });
        }
    }
    memory.write(offset, bytes);
    Ok(())
}

/// Like [`safe_write`], but panics if the memory grow fails.
pub(crate) fn write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) {
    if let Err(GrowFailed {
        current_size,
        delta,
    }) = safe_write(memory, offset, bytes)
    {
        panic!(
            "Failed to grow memory from {} pages to {} pages (delta = {} pages).",
            current_size,
            current_size + delta,
            delta
        );
    }
}
