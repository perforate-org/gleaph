//! Copy-on-write shadow over a [`Memory`].
//!
//! A write path can run its ordinary code against this overlay, decide a final layout, and only
//! then publish it. Reads fall through to the inner memory, writes land in fixed-size blocks, and
//! [`ShadowMemory::commit`] copies the dirty blocks out. Dropping it discards the attempt.
//!
//! **Blocks, not ranges.** An earlier range-based overlay merged overlapping writes into one buffer,
//! which cost a `BTreeMap` search on every read, a fresh allocation per write, and a re-read of the
//! whole merged span from the inner memory - on a path whose writes are mostly *distinct* bytes.
//! Measured on 128 inserts into one bucket, that overlay ran the same inserts 3.1x slower than
//! writing directly. Fixed blocks make an access one hash lookup plus a copy, with no merging and no
//! re-read: a block is copied in the first time it is touched, and every later write patches it in
//! place.
//!
//! The trade is memory: a block is copied whole, so a single 12-byte write costs [`BLOCK_BYTES`].
//! That is the right trade here because the alternative spent work on bookkeeping for every access
//! instead of on the data.

use ic_stable_structures::Memory;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Bytes per stable-memory page, matching [`Memory::grow`]'s unit.
pub const PAGE_BYTES: u64 = 64 * 1024;

/// Copy-on-write granularity: one hash lookup per block touched, and the smallest unit an attempt
/// pays for.
pub const BLOCK_BYTES: u64 = 1024;

/// One attempt's worth of writes over an inner memory.
pub struct ShadowMemory<M: Memory> {
    inner: M,
    /// Dirty blocks keyed by block index. A present block is authoritative for its whole extent; an
    /// absent block is read from the inner memory.
    ///
    /// Behind a cell because [`Memory`] takes `&self` for both reads and writes, the way the
    /// canister's own memory does.
    blocks: RefCell<HashMap<u64, Box<[u8]>>>,
    /// Virtual size in pages, never below `inner.size()`.
    pages: Cell<u64>,
}

impl<M: Memory> ShadowMemory<M> {
    /// Starts an empty overlay over `inner`: no bytes are copied and no page is reserved.
    pub fn new(inner: M) -> Self {
        let pages = inner.size();
        Self {
            inner,
            blocks: RefCell::new(HashMap::new()),
            pages: Cell::new(pages),
        }
    }

    /// The inner memory, for a caller that decides to discard the overlay.
    pub fn into_inner(self) -> M {
        self.inner
    }

    /// The inner memory, for assertions about what has not been published yet.
    #[cfg(test)]
    fn inner_for_test(&self) -> M
    where
        M: Clone,
    {
        self.inner.clone()
    }

    /// The overlay's virtual size in pages, including pages only the overlay knows about.
    pub fn virtual_pages(&self) -> u64 {
        self.pages.get()
    }

    /// Bytes the commit will write back. This is the cost of the attempt and of its rollback.
    pub fn dirty_bytes(&self) -> u64 {
        self.blocks.borrow().len() as u64 * BLOCK_BYTES
    }

    /// Blocks the attempt has touched, i.e. how many copies the commit performs.
    pub fn dirty_ranges(&self) -> usize {
        self.blocks.borrow().len()
    }

    /// Copies the overlay into the inner memory and returns it.
    ///
    /// Growth happens first: if the inner memory refuses it, nothing has been written and the caller
    /// can still discard the attempt. The writes that follow cannot fail.
    pub fn commit(self) -> Result<M, ShadowCommitError> {
        let Self {
            inner,
            blocks,
            pages,
        } = self;
        let pages = pages.get();
        let current = inner.size();
        if pages > current {
            let requested = pages - current;
            if inner.grow(requested) < 0 {
                return Err(ShadowCommitError::GrowRefused {
                    current_pages: current,
                    requested_pages: requested,
                });
            }
        }
        let limit = pages * PAGE_BYTES;
        for (block, bytes) in blocks.into_inner() {
            let start = block * BLOCK_BYTES;
            if start >= limit {
                continue;
            }
            // Clip to the live size: a caller may grow, write, and grow again.
            let len = usize::try_from((start + BLOCK_BYTES).min(limit) - start).unwrap_or(0);
            inner.write(start, &bytes[..len.min(bytes.len())]);
        }
        Ok(inner)
    }

    /// Fills `dst` from the overlay, reading uncovered blocks straight from the inner memory.
    fn read_shadowed(&self, offset: u64, dst: &mut [u8]) {
        // The fast path: an attempt that has not written yet must not pay for bookkeeping.
        if self.blocks.borrow().is_empty() {
            Self::read_inner(&self.inner, offset, dst);
            return;
        }
        let blocks = self.blocks.borrow();
        let mut filled = 0usize;
        while filled < dst.len() {
            let at = offset + filled as u64;
            let block = at / BLOCK_BYTES;
            let within = (at % BLOCK_BYTES) as usize;
            let len = (BLOCK_BYTES as usize - within).min(dst.len() - filled);
            match blocks.get(&block) {
                Some(bytes) => {
                    dst[filled..filled + len].copy_from_slice(&bytes[within..within + len])
                }
                None => Self::read_inner(&self.inner, at, &mut dst[filled..filled + len]),
            }
            filled += len;
        }
    }

    /// Absorbs `src` at `offset`, copying each touched block in on first write.
    fn write_shadowed(&self, offset: u64, src: &[u8]) {
        let mut written = 0usize;
        while written < src.len() {
            let at = offset + written as u64;
            let block = at / BLOCK_BYTES;
            let within = (at % BLOCK_BYTES) as usize;
            let len = (BLOCK_BYTES as usize - within).min(src.len() - written);
            let mut blocks = self.blocks.borrow_mut();
            let bytes = blocks.entry(block).or_insert_with(|| {
                let mut fresh = vec![0u8; BLOCK_BYTES as usize].into_boxed_slice();
                Self::read_inner(&self.inner, block * BLOCK_BYTES, &mut fresh);
                fresh
            });
            bytes[within..within + len].copy_from_slice(&src[written..written + len]);
            written += len;
        }
    }

    /// Fills `dst` from the inner memory, zeroing the part past its live size.
    fn read_inner(inner: &M, offset: u64, dst: &mut [u8]) {
        let live = inner.size() * PAGE_BYTES;
        if offset >= live {
            dst.fill(0);
            return;
        }
        let available = usize::try_from(live - offset).unwrap_or(usize::MAX);
        let covered = available.min(dst.len());
        inner.read(offset, &mut dst[..covered]);
        dst[covered..].fill(0);
    }
}

impl<M: Memory> Memory for ShadowMemory<M> {
    fn size(&self) -> u64 {
        self.pages.get()
    }

    fn grow(&self, pages: u64) -> i64 {
        // Virtual growth never fails: the inner memory is only asked at commit. `Memory::grow`
        // returns the previous page count.
        let previous = self.pages.get();
        self.pages.set(previous.saturating_add(pages));
        i64::try_from(previous).unwrap_or(-1)
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        self.read_shadowed(offset, dst);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        self.write_shadowed(offset, src);
    }
}

/// Why [`ShadowMemory::commit`] could not publish the overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowCommitError {
    /// The inner memory refused the pages the overlay had grown into. Nothing was written.
    GrowRefused {
        /// Pages the inner memory had before the commit.
        current_pages: u64,
        /// Pages the commit asked for.
        requested_pages: u64,
    },
}

impl std::fmt::Display for ShadowCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GrowRefused {
                current_pages,
                requested_pages,
            } => write!(
                f,
                "shadow commit could not grow stable memory: current={current_pages} pages, requested={requested_pages}"
            ),
        }
    }
}

impl std::error::Error for ShadowCommitError {}

#[cfg(test)]
mod tests {
    use super::{BLOCK_BYTES, PAGE_BYTES, ShadowCommitError, ShadowMemory};
    use ic_stable_structures::{Memory, VectorMemory};

    fn grown(pages: u64) -> VectorMemory {
        let memory = VectorMemory::default();
        assert_eq!(memory.grow(pages), 0);
        memory
    }

    fn read(memory: &impl Memory, offset: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        memory.read(offset, &mut bytes);
        bytes
    }

    #[test]
    fn writes_stay_in_the_overlay_until_commit() {
        let inner = grown(1);
        inner.write(0, &[9; 8]);
        let shadow = ShadowMemory::new(inner);
        shadow.write(4, &[1, 2, 3, 4]);

        assert_eq!(
            shadow.dirty_ranges(),
            1,
            "one block, not one range per byte"
        );
        assert_eq!(shadow.dirty_bytes(), BLOCK_BYTES);
        assert_eq!(read(&shadow, 4, 4), vec![1, 2, 3, 4]);
        assert_eq!(
            read(&shadow, 0, 2),
            vec![9, 9],
            "untouched bytes fall through"
        );
        assert_eq!(
            read(&shadow.inner_for_test(), 4, 4),
            vec![9, 9, 9, 9],
            "the inner memory is untouched before commit"
        );

        let inner = shadow.commit().expect("commit");
        assert_eq!(read(&inner, 4, 4), vec![1, 2, 3, 4]);
        assert_eq!(read(&inner, 0, 4), vec![9, 9, 9, 9]);
    }

    #[test]
    fn an_untouched_overlay_reads_straight_through() {
        let inner = grown(1);
        inner.write(100, &[7; 16]);
        let shadow = ShadowMemory::new(inner);
        assert_eq!(shadow.dirty_ranges(), 0);
        assert_eq!(read(&shadow, 100, 16), vec![7; 16]);
        assert_eq!(shadow.dirty_ranges(), 0, "reads never copy a block in");
    }

    #[test]
    fn repeated_writes_to_one_block_patch_it_in_place() {
        let inner = grown(1);
        let shadow = ShadowMemory::new(inner);
        for byte in 0..64u8 {
            shadow.write(u64::from(byte), &[byte]);
        }
        assert_eq!(
            shadow.dirty_ranges(),
            1,
            "many small writes to one block stay one block"
        );
        assert_eq!(read(&shadow, 0, 8), vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_read_spanning_dirty_and_clean_bytes_fills_both() {
        let inner = grown(1);
        inner.write(0, &[7; 16]);
        let shadow = ShadowMemory::new(inner);
        shadow.write(4, &[1; 4]);
        assert_eq!(
            read(&shadow, 0, 12),
            vec![7, 7, 7, 7, 1, 1, 1, 1, 7, 7, 7, 7]
        );
    }

    #[test]
    fn a_write_across_a_block_boundary_copies_both_blocks() {
        let inner = grown(1);
        let shadow = ShadowMemory::new(inner);
        shadow.write(BLOCK_BYTES - 2, &[5; 4]);
        assert_eq!(shadow.dirty_ranges(), 2, "the write straddled a boundary");
        assert_eq!(read(&shadow, BLOCK_BYTES - 3, 6), vec![0, 5, 5, 5, 5, 0]);
    }

    #[test]
    fn growth_is_virtual_until_commit() {
        let inner = grown(1);
        let shadow = ShadowMemory::new(inner);
        assert_eq!(shadow.size(), 1);
        assert_eq!(shadow.grow(1), 1, "grow reports the previous page count");
        assert_eq!(shadow.size(), 2);
        assert_eq!(
            shadow.inner_for_test().size(),
            1,
            "the inner memory has not grown yet"
        );

        shadow.write(PAGE_BYTES + 8, &[5; 4]);
        let inner = shadow.commit().expect("commit");
        assert_eq!(inner.size(), 2, "commit performs the growth");
        assert_eq!(read(&inner, PAGE_BYTES + 8, 4), vec![5; 4]);
    }

    #[test]
    fn discarding_the_overlay_leaves_the_inner_memory_alone() {
        let inner = grown(1);
        inner.write(0, &[9; 8]);
        let shadow = ShadowMemory::new(inner);
        shadow.write(0, &[1; 8]);
        let discarded = shadow.into_inner();
        assert_eq!(
            read(&discarded, 0, 8),
            vec![9; 8],
            "an abandoned overlay never reached the inner memory"
        );
    }

    #[test]
    fn a_refused_commit_reports_growth_and_writes_nothing() {
        #[derive(Debug)]
        struct RefusesGrow;
        impl Memory for RefusesGrow {
            fn size(&self) -> u64 {
                0
            }
            fn grow(&self, _pages: u64) -> i64 {
                -1
            }
            fn read(&self, _offset: u64, dst: &mut [u8]) {
                dst.fill(0);
            }
            fn write(&self, _offset: u64, _src: &[u8]) {
                panic!("a refused commit must not write");
            }
        }

        let shadow = ShadowMemory::new(RefusesGrow);
        // Growth is where a commit can fail on its own, so the attempt has to ask for a page.
        assert_eq!(shadow.grow(1), 0);
        shadow.write(0, &[1; 4]);
        assert_eq!(
            shadow.commit().expect_err("growth is refused"),
            ShadowCommitError::GrowRefused {
                current_pages: 0,
                requested_pages: 1,
            }
        );
    }
}
