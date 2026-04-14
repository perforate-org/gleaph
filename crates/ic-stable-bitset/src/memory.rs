use core::fmt::{Display, Formatter};
use ic_stable_structures::Memory;
use core::slice;
use std::error;
use std::sync::OnceLock;

pub(crate) const WASM_PAGE_SIZE: u64 = 65536;
pub(crate) const BULK_WORDS: usize = 4096;

pub(crate) fn read_u64<M: Memory>(m: &M, offset: u64) -> u64 {
    let mut buf = [0u8; 8];
    m.read(offset, &mut buf);
    u64::from_le_bytes(buf)
}

pub(crate) fn read_5_bytes<M: Memory>(m: &M, offset: u64, dst: &mut [u8; 5]) {
    m.read(offset, dst.as_mut_slice());
}

pub(crate) fn write_5_bytes<M: Memory>(
    memory: &M,
    offset: u64,
    bytes: &[u8; 5],
) -> Result<(), GrowFailed> {
    safe_write(memory, offset, bytes.as_slice())
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
    let zero_bytes = ZERO_BYTES.get_or_init(|| vec![0u8; BULK_WORDS * 8].into_boxed_slice());
    let mut remaining = byte_len as usize;
    let mut base = offset;
    while remaining > 0 {
        let take = remaining.min(BULK_WORDS * 8);
        safe_write(m, base, &zero_bytes[..take])?;
        base += take as u64;
        remaining -= take;
    }
    Ok(())
}

pub(crate) fn read_u64_words_vec<M: Memory>(m: &M, offset: u64, word_count: u64) -> Vec<u64> {
    let count = word_count as usize;
    let mut words: Vec<u64> = Vec::with_capacity(count);
    let mut filled = 0usize;
    let mut base = offset;
    let spare = words.spare_capacity_mut();
    while filled < count {
        let take = (count - filled).min(BULK_WORDS);
        #[cfg(target_endian = "little")]
        {
            let bytes = unsafe {
                slice::from_raw_parts_mut(spare[filled..filled + take].as_mut_ptr() as *mut u8, take * 8)
            };
            m.read(base, bytes);
        }
        #[cfg(not(target_endian = "little"))]
        {
            let mut scratch = vec![0u8; take * 8];
            m.read(base, &mut scratch);
            for (dst, chunk) in spare[filled..filled + take]
                .iter_mut()
                .zip(scratch.chunks_exact(8))
            {
                dst.write(u64::from_le_bytes(chunk.try_into().unwrap()));
            }
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

pub(crate) fn write_u64_words_direct<M: Memory>(m: &M, offset: u64, words: &[u64]) {
    let mut base = offset;
    let mut remaining = words;
    while !remaining.is_empty() {
        let take = remaining.len().min(BULK_WORDS);
        #[cfg(target_endian = "little")]
        {
            let bytes = unsafe {
                slice::from_raw_parts(remaining[..take].as_ptr() as *const u8, take * 8)
            };
            write(m, base, bytes);
        }
        #[cfg(not(target_endian = "little"))]
        {
            let mut scratch = vec![0u8; take * 8];
            for (i, word) in remaining[..take].iter().enumerate() {
                scratch[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
            }
            write(m, base, &scratch);
        }
        base += (take as u64) * 8;
        remaining = &remaining[take..];
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
