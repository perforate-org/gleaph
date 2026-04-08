use core::fmt::{Display, Formatter};
use ic_stable_structures::Memory;
use std::error;

pub(crate) const WASM_PAGE_SIZE: u64 = 65536;
pub(crate) const BULK_WORDS: usize = 4096;

pub(crate) fn read_u64<M: Memory>(m: &M, offset: u64) -> u64 {
    let mut buf = [0u8; 8];
    m.read(offset, &mut buf);
    u64::from_le_bytes(buf)
}

pub(crate) fn read_u64_words_into<M: Memory>(
    m: &M,
    offset: u64,
    words: &mut [u64],
    scratch: &mut [u8],
) {
    let chunk_words = (scratch.len() / 8).clamp(1, BULK_WORDS);
    let mut remaining = words;
    let mut base = offset;
    while !remaining.is_empty() {
        let take = remaining.len().min(chunk_words);
        let bytes = &mut scratch[..take * 8];
        m.read(base, bytes);
        for (slot, chunk) in remaining[..take].iter_mut().zip(bytes.chunks_exact(8)) {
            *slot = u64::from_le_bytes(chunk.try_into().unwrap());
        }
        base += (take as u64) * 8;
        remaining = &mut remaining[take..];
    }
}

pub(crate) fn read_u64_words_vec<M: Memory>(m: &M, offset: u64, word_count: u64) -> Vec<u64> {
    let count = word_count as usize;
    let mut words: Vec<u64> = Vec::with_capacity(count);
    let mut scratch = vec![0u8; BULK_WORDS * 8];
    let mut filled = 0usize;
    let mut base = offset;
    let spare = words.spare_capacity_mut();
    while filled < count {
        let take = (count - filled).min(BULK_WORDS);
        let bytes = &mut scratch[..take * 8];
        m.read(base, bytes);
        for (dst, chunk) in spare[filled..filled + take]
            .iter_mut()
            .zip(bytes.chunks_exact(8))
        {
            dst.write(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        base += (take as u64) * 8;
        filled += take;
    }
    unsafe {
        words.set_len(count);
    }
    words
}

pub(crate) fn write_u64<M: Memory>(m: &M, offset: u64, value: u64) {
    write(m, offset, &value.to_le_bytes());
}

pub(crate) fn write_u64_words_into<M: Memory>(
    m: &M,
    offset: u64,
    words: &[u64],
    scratch: &mut [u8],
) {
    let chunk_words = (scratch.len() / 8).clamp(1, BULK_WORDS);
    let mut remaining = words;
    let mut base = offset;
    while !remaining.is_empty() {
        let take = remaining.len().min(chunk_words);
        let bytes = &mut scratch[..take * 8];
        for (i, word) in remaining[..take].iter().enumerate() {
            bytes[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        write(m, base, bytes);
        base += (take as u64) * 8;
        remaining = &remaining[take..];
    }
}

pub(crate) fn write_zero_words<M: Memory>(m: &M, offset: u64, word_count: u64) {
    if word_count == 0 {
        return;
    }
    let mut scratch = vec![0u8; BULK_WORDS * 8];
    let mut remaining = word_count as usize;
    let mut base = offset;
    while remaining > 0 {
        let take = remaining.min(BULK_WORDS);
        scratch[..take * 8].fill(0);
        write(m, base, &scratch[..take * 8]);
        base += (take as u64) * 8;
        remaining -= take;
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

pub(crate) fn write<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) {
    if let Err(e) = safe_write(memory, offset, bytes) {
        panic!(
            "Failed to grow memory from {} pages to {} pages (delta = {} pages).",
            e.current_size,
            e.current_size + e.delta,
            e.delta
        );
    }
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
