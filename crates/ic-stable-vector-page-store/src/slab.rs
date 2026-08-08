//! The raw row slab: a fixed header plus pages appended at the tail of one stable-memory region.
//!
//! The slab owns no page directory — the vector canister's `VECTOR_PAGE_META` region is the
//! directory and knows each page's slab offset. The slab only guarantees that pages are appended
//! at `occupied_tail` and that the tail is persisted last (a crash between appending a page and
//! rewriting the header leaves a valid, smaller slab whose tail points before the unwritten page).

use ic_stable_structures::Memory;

use crate::header::{SLAB_HEADER_SIZE, SlabHeader};

const WASM_PAGE_SIZE: u64 = 65536;

/// Slab open/append errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlabError {
    /// The slab header failed fail-closed validation.
    InvalidHeader(String),
    /// Memory could not be grown to hold the write.
    GrowFailed {
        /// Current memory size in WebAssembly pages.
        current_pages: u64,
        /// Additional pages requested.
        delta_pages: u64,
    },
    /// The append would place the page end past the maximum addressable offset.
    TailOverflow,
}

/// A raw row slab behind a stable-memory region.
pub struct Slab<'a, M: Memory> {
    memory: &'a M,
    occupied_tail: u64,
}

impl<'a, M: Memory> Slab<'a, M> {
    /// Opens an existing slab or initializes a fresh one.
    ///
    /// A region whose header bytes are all zero is treated as fresh (the memory manager may have
    /// pre-grown the region without writing); any other invalid header fails closed.
    pub fn open_or_init(memory: &'a M) -> Result<Self, SlabError> {
        if memory.size() == 0 || slab_header_bytes_are_zero(memory) {
            let header = SlabHeader::new(SLAB_HEADER_SIZE as u64, 0);
            write_at(memory, 0, &header.to_bytes())?;
            return Ok(Self {
                memory,
                occupied_tail: SLAB_HEADER_SIZE as u64,
            });
        }
        let mut buf = [0u8; SLAB_HEADER_SIZE];
        memory.read(0, &mut buf);
        let header =
            SlabHeader::from_bytes(&buf).map_err(|e| SlabError::InvalidHeader(format!("{e:?}")))?;
        if header.occupied_tail < SLAB_HEADER_SIZE as u64
            || header.occupied_tail > memory.size() * WASM_PAGE_SIZE
        {
            return Err(SlabError::InvalidHeader(format!(
                "occupied_tail {} out of bounds for {} bytes",
                header.occupied_tail,
                memory.size() * WASM_PAGE_SIZE
            )));
        }
        Ok(Self {
            memory,
            occupied_tail: header.occupied_tail,
        })
    }

    /// Returns the byte offset of the first unused byte (end of the last appended page).
    pub fn occupied_tail(&self) -> u64 {
        self.occupied_tail
    }

    /// Returns the current slab header (the tail is always `occupied_tail`).
    pub fn header(&self) -> SlabHeader {
        SlabHeader::new(self.occupied_tail, 0)
    }

    /// Appends one page's bytes at the tail and returns its offset.
    ///
    /// Page bytes are written first; the header's `occupied_tail` is persisted last, and the
    /// in-memory tail is advanced only after both writes succeed.
    pub fn append_page(&mut self, page: &[u8]) -> Result<u64, SlabError> {
        let offset = self.occupied_tail;
        let end = offset
            .checked_add(page.len() as u64)
            .ok_or(SlabError::TailOverflow)?;
        write_at(self.memory, offset, page)?;
        write_at(self.memory, 0, &SlabHeader::new(end, 0).to_bytes())?;
        self.occupied_tail = end;
        Ok(offset)
    }
}

/// Returns `true` when the first 32 bytes of the region are all zero (a pre-grown but never
/// written region).
fn slab_header_bytes_are_zero<M: Memory>(memory: &M) -> bool {
    let mut buf = [0u8; SLAB_HEADER_SIZE];
    memory.read(0, &mut buf);
    buf.iter().all(|b| *b == 0)
}

/// Writes bytes at `offset`, growing the memory when needed.
fn write_at<M: Memory>(memory: &M, offset: u64, bytes: &[u8]) -> Result<(), SlabError> {
    let last_byte = offset
        .checked_add(bytes.len() as u64)
        .ok_or(SlabError::TailOverflow)?;
    let size_bytes = memory.size() * WASM_PAGE_SIZE;
    if size_bytes < last_byte {
        let diff_bytes = last_byte - size_bytes;
        let diff_pages = diff_bytes
            .checked_add(WASM_PAGE_SIZE - 1)
            .ok_or(SlabError::TailOverflow)?
            / WASM_PAGE_SIZE;
        if memory.grow(diff_pages) == -1 {
            return Err(SlabError::GrowFailed {
                current_pages: memory.size(),
                delta_pages: diff_pages,
            });
        }
    }
    memory.write(offset, bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::vec_mem::VectorMemory;

    #[test]
    fn fresh_slab_initializes_header_and_reopens() {
        let memory = VectorMemory::default();
        let slab = Slab::open_or_init(&memory).expect("fresh init");
        assert_eq!(slab.occupied_tail(), SLAB_HEADER_SIZE as u64);

        let reopened = Slab::open_or_init(&memory).expect("reopen");
        assert_eq!(reopened.occupied_tail(), SLAB_HEADER_SIZE as u64);
    }

    #[test]
    fn pre_grown_zero_region_initializes_fresh() {
        // The memory manager may allocate pages without writing; all-zero headers are fresh.
        let memory = VectorMemory::default();
        memory.grow(1);
        let slab = Slab::open_or_init(&memory).expect("zero region is fresh");
        assert_eq!(slab.occupied_tail(), SLAB_HEADER_SIZE as u64);
    }

    #[test]
    fn append_bumps_tail_and_is_visible_after_reopen() {
        let memory = VectorMemory::default();
        let mut slab = Slab::open_or_init(&memory).expect("init");
        let page = vec![7u8; 4096];
        let offset = slab.append_page(&page).expect("append");
        assert_eq!(offset, SLAB_HEADER_SIZE as u64);
        assert_eq!(slab.occupied_tail(), offset + 4096);

        let reopened = Slab::open_or_init(&memory).expect("reopen");
        assert_eq!(reopened.occupied_tail(), offset + 4096);
        let mut buf = vec![0u8; 4096];
        memory.read(offset, &mut buf);
        assert_eq!(buf, page);
    }

    #[test]
    fn corrupted_header_fails_closed() {
        let memory = VectorMemory::default();
        Slab::open_or_init(&memory).expect("init");
        memory.write(0, b"XXX".as_slice());
        let result = Slab::open_or_init(&memory);
        assert!(matches!(result, Err(SlabError::InvalidHeader(_))));
    }

    #[test]
    fn out_of_bounds_tail_fails_closed() {
        let memory = VectorMemory::default();
        Slab::open_or_init(&memory).expect("init");
        // Rewrite the tail past the memory size.
        let header = SlabHeader::new(1 << 40, 0).to_bytes();
        memory.write(0, &header);
        let result = Slab::open_or_init(&memory);
        assert!(matches!(result, Err(SlabError::InvalidHeader(_))));
    }
}
