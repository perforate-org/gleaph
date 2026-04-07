use crate::types::Address;
use core::fmt::{Display, Formatter};
use ic_stable_structures::Memory;
use std::error;

pub(crate) const WASM_PAGE_SIZE: u64 = 65536;

#[inline]
pub(crate) fn read_to_vec<M: Memory>(
    m: &M,
    addr: Address,
    dst: &mut std::vec::Vec<u8>,
    count: usize,
) {
    dst.clear();
    dst.reserve_exact(count);
    unsafe {
        m.read_unsafe(addr.get(), dst.as_mut_ptr(), count);
        dst.set_len(count);
    }
}

pub(crate) fn read_u32<M: Memory>(m: &M, addr: Address) -> u32 {
    let mut buf: [u8; 4] = [0; 4];
    m.read(addr.get(), &mut buf);
    u32::from_le_bytes(buf)
}

pub(crate) fn read_u64<M: Memory>(m: &M, addr: Address) -> u64 {
    let mut buf: [u8; 8] = [0; 8];
    m.read(addr.get(), &mut buf);
    u64::from_le_bytes(buf)
}

pub(crate) fn write_u32<M: Memory>(m: &M, addr: Address, val: u32) {
    write(m, addr.get(), &val.to_le_bytes());
}

pub(crate) fn write_u64<M: Memory>(m: &M, addr: Address, val: u64) {
    write(m, addr.get(), &val.to_le_bytes());
}

/// Stable memory could not be grown to fit a write (used by [`crate::SlotMap::new`],
/// [`crate::SlotMap::insert`], and low-level helpers).
///
/// Page counts refer to Wasm pages (65536 bytes each), matching [`ic_stable_structures::Memory`].
#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    current_size: u64,
    delta: u64,
}

impl GrowFailed {
    /// Size of the memory region in **pages** when the failed grow was attempted.
    pub fn current_size_pages(&self) -> u64 {
        self.current_size
    }
    /// Number of additional pages the grow tried to allocate.
    pub fn delta_pages(&self) -> u64 {
        self.delta
    }

    pub(crate) fn with_pages(current_size: u64, delta: u64) -> Self {
        Self {
            current_size,
            delta,
        }
    }
}

impl Display for GrowFailed {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            fmt,
            "Failed to grow memory: current size={}, delta={}",
            self.current_size, self.delta
        )
    }
}

impl error::Error for GrowFailed {}

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
