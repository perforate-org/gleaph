use core::fmt::{Display, Formatter};
use ic_stable_structures::Memory;
use std::error;
use std::io::{self, Read, Write};
use std::sync::OnceLock;

pub(crate) const WASM_PAGE_SIZE: u64 = 65_536;
const BULK_BYTES: usize = 32 * 1024;

pub(crate) fn read_u64<M: Memory>(m: &M, offset: u64) -> u64 {
    let mut buf = [0u8; 8];
    m.read(offset, &mut buf);
    u64::from_le_bytes(buf)
}

pub(crate) fn read_u64_words_into<M: Memory>(m: &M, offset: u64, words: &mut [u64]) {
    let mut base = offset;
    let mut buf = [0u8; 8];
    for word in words {
        m.read(base, &mut buf);
        *word = u64::from_le_bytes(buf);
        base += 8;
    }
}

pub(crate) fn safe_write<M: Memory>(
    memory: &M,
    offset: u64,
    bytes: &[u8],
) -> Result<(), GrowFailed> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .expect("address overflow");
    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("address overflow");
    if size_bytes < last_byte {
        let diff_bytes = last_byte - size_bytes;
        let diff_pages = diff_bytes
            .checked_add(WASM_PAGE_SIZE - 1)
            .expect("address overflow")
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

pub(crate) fn write_u64<M: Memory>(m: &M, offset: u64, value: u64) -> Result<(), GrowFailed> {
    safe_write(m, offset, &value.to_le_bytes())
}

pub(crate) fn write_zero_bytes<M: Memory>(
    m: &M,
    offset: u64,
    byte_len: u64,
) -> Result<(), GrowFailed> {
    if byte_len == 0 {
        return Ok(());
    }
    static ZERO_BYTES: OnceLock<Box<[u8]>> = OnceLock::new();
    let zero_bytes = ZERO_BYTES.get_or_init(|| vec![0u8; BULK_BYTES].into_boxed_slice());
    let mut remaining = byte_len as usize;
    let mut base = offset;
    while remaining > 0 {
        let take = remaining.min(BULK_BYTES);
        safe_write(m, base, &zero_bytes[..take])?;
        base += take as u64;
        remaining -= take;
    }
    Ok(())
}

pub(crate) fn grow_memory_to_at_least_bytes<M: Memory>(
    memory: &M,
    min_bytes: u64,
) -> Result<(), GrowFailed> {
    let size_pages = memory.size();
    let size_bytes = size_pages
        .checked_mul(WASM_PAGE_SIZE)
        .expect("address overflow");
    if size_bytes >= min_bytes {
        return Ok(());
    }
    let diff_bytes = min_bytes - size_bytes;
    let diff_pages = diff_bytes
        .checked_add(WASM_PAGE_SIZE - 1)
        .expect("address overflow")
        / WASM_PAGE_SIZE;
    if memory.grow(diff_pages) == -1 {
        return Err(GrowFailed {
            current_size: size_pages,
            delta: diff_pages,
        });
    }
    Ok(())
}

pub(crate) struct MemoryReader<'a, M: Memory> {
    memory: &'a M,
    offset: u64,
    end: u64,
}

impl<'a, M: Memory> MemoryReader<'a, M> {
    pub(crate) fn new(memory: &'a M, offset: u64, len: u64) -> Self {
        let end = offset
            .checked_add(len)
            .expect("address overflow while creating memory reader");
        Self {
            memory,
            offset,
            end,
        }
    }
}

impl<M: Memory> Read for MemoryReader<'_, M> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.offset >= self.end {
            return Ok(0);
        }
        let remaining = (self.end - self.offset) as usize;
        let take = remaining.min(buf.len());
        self.memory.read(self.offset, &mut buf[..take]);
        self.offset += take as u64;
        Ok(take)
    }
}

pub(crate) struct MemoryWriter<'a, M: Memory> {
    memory: &'a M,
    offset: u64,
}

impl<'a, M: Memory> MemoryWriter<'a, M> {
    pub(crate) fn new(memory: &'a M, offset: u64) -> Self {
        Self { memory, offset }
    }
}

impl<M: Memory> Write for MemoryWriter<'_, M> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let end = self
            .offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "address overflow"))?;
        grow_memory_to_at_least_bytes(self.memory, end).map_err(io::Error::other)?;
        self.memory.write(self.offset, buf);
        self.offset = end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct GrowFailed {
    current_size: u64,
    delta: u64,
}

impl GrowFailed {
    pub fn current_size_pages(&self) -> u64 {
        self.current_size
    }

    pub fn delta_pages(&self) -> u64 {
        self.delta
    }
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
