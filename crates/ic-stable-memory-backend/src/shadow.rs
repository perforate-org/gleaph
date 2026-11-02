//! Copy-on-write shadow over a [`Memory`].
//!
//! A batch write path wants to run the ordinary insert code, decide a final layout, and only then
//! touch stable memory. This wrapper gives that: reads fall through to the inner memory, writes are
//! absorbed into a contiguous-range overlay, and [`ShadowMemory::commit`] copies the overlay out.
//! Dropping it instead discards the overlay, so a failed attempt costs nothing real.
//!
//! Nothing is copied up front. A read of an untouched range goes straight to the inner memory, and
//! a write copies only the range it touches (plus whatever part of it was already shadowed), so the
//! overlay's size is bounded by the bytes the caller actually wrote - not by the size of the
//! canister's stable memory.
//!
//! Growth is virtual: [`Memory::grow`] moves the overlay's own page count and the inner memory is
//! only asked at commit. That is the one step where a commit can fail on its own, which is why
//! [`ShadowMemory::commit`] returns a result instead of panicking.

use ic_stable_structures::Memory;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// Bytes per stable-memory page, matching [`Memory::grow`]'s unit.
pub const PAGE_BYTES: u64 = 64 * 1024;

/// One attempt's worth of writes over an inner memory.
pub struct ShadowMemory<M: Memory> {
    inner: M,
    /// Dirty ranges keyed by start offset, ascending and non-overlapping. Every byte inside a range
    /// is authoritative; bytes outside every range are read from the inner memory.
    ///
    /// Behind a cell because [`Memory`] takes `&self` for both reads and writes, the way the
    /// canister's own memory does.
    ranges: RefCell<BTreeMap<u64, Vec<u8>>>,
    /// Virtual size in pages, never below `inner.size()`.
    pages: Cell<u64>,
}

impl<M: Memory> ShadowMemory<M> {
    /// Starts an empty overlay over `inner`: no bytes are copied and no page is reserved.
    pub fn new(inner: M) -> Self {
        let pages = inner.size();
        Self {
            inner,
            ranges: RefCell::new(BTreeMap::new()),
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

    /// Bytes the overlay currently holds. This is the cost of the attempt: the rollback payload and
    /// the commit payload are both exactly this.
    pub fn dirty_bytes(&self) -> u64 {
        self.ranges
            .borrow()
            .values()
            .map(|bytes| bytes.len() as u64)
            .sum()
    }

    /// Number of dirty ranges, i.e. how many separate copies a commit performs.
    pub fn dirty_ranges(&self) -> usize {
        self.ranges.borrow().len()
    }

    /// Copies the overlay into the inner memory and returns it.
    ///
    /// Growth happens first: if the inner memory refuses it, nothing has been written and the
    /// caller can still discard the attempt. The writes that follow cannot fail.
    pub fn commit(self) -> Result<M, ShadowCommitError> {
        let Self {
            inner,
            ranges,
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
        for (start, bytes) in ranges.into_inner() {
            // Clip to the live size: a caller may grow, write, and grow again, and the real memory
            // only keeps what the final size covers.
            if start >= limit {
                continue;
            }
            let len = ((start + bytes.len() as u64).min(limit) - start) as usize;
            inner.write(start, &bytes[..len]);
        }
        Ok(inner)
    }

    /// Fills `dst` from the overlay, falling through to the inner memory for uncovered bytes.
    fn read_shadowed(&self, offset: u64, dst: &mut [u8]) {
        let ranges = self.ranges.borrow();
        let virtual_end = self.pages.get() * PAGE_BYTES;
        let mut written = 0usize;
        while written < dst.len() {
            let at = offset + written as u64;
            let owner = ranges.range(..=at).next_back();
            let inside = owner.filter(|(start, bytes)| at < *start + bytes.len() as u64);
            match inside {
                Some((start, bytes)) => {
                    // `at` is inside this range: copy up to its end, then loop for the remainder.
                    let skip = (at - start) as usize;
                    let len = (bytes.len() - skip).min(dst.len() - written);
                    dst[written..written + len].copy_from_slice(&bytes[skip..skip + len]);
                    written += len;
                }
                None => {
                    // A gap: it runs to the next dirty range, or to the virtual end.
                    let next = ranges
                        .range((at + 1)..)
                        .next()
                        .map_or(virtual_end, |(next, _)| *next);
                    let len = ((next.saturating_sub(at)) as usize).min(dst.len() - written);
                    Self::read_inner(&self.inner, at, &mut dst[written..written + len]);
                    written += len;
                }
            }
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

    /// Absorbs `src` at `offset`, merging every dirty range it overlaps into one.
    ///
    /// Overlapping ranges merge rather than stack so that `dirty_bytes` stays the size of the data
    /// the caller wrote and a commit writes each byte at most once. Gaps the merge exposes are
    /// filled from the inner memory, which keeps the invariant simple: inside a dirty range the
    /// overlay's bytes are the truth.
    fn write_shadowed(&self, offset: u64, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        let mut ranges = self.ranges.borrow_mut();
        let new_start = offset;
        let new_end = offset + src.len() as u64;
        let mut overlapped: Vec<(u64, Vec<u8>)> = Vec::new();
        let starts: Vec<u64> = ranges
            .range(..new_end)
            .rev()
            .take_while(|(start, bytes)| **start + bytes.len() as u64 > new_start)
            .map(|(start, _)| *start)
            .collect();
        for start in starts {
            if let Some(bytes) = ranges.remove(&start) {
                overlapped.push((start, bytes));
            }
        }
        let merged_start = overlapped
            .iter()
            .map(|(start, _)| *start)
            .min()
            .map_or(new_start, |oldest| oldest.min(new_start));
        let merged_end = overlapped
            .iter()
            .map(|(start, bytes)| start + bytes.len() as u64)
            .max()
            .map_or(new_end, |latest| latest.max(new_end));
        let mut merged = vec![0u8; (merged_end - merged_start) as usize];
        Self::read_inner(&self.inner, merged_start, &mut merged);
        for (start, bytes) in overlapped {
            let at = (start - merged_start) as usize;
            merged[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        let at = (new_start - merged_start) as usize;
        merged[at..at + src.len()].copy_from_slice(src);
        ranges.insert(merged_start, merged);
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
    use super::{PAGE_BYTES, ShadowCommitError, ShadowMemory};
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

        assert_eq!(shadow.dirty_bytes(), 4, "only the written bytes are copied");
        assert_eq!(shadow.dirty_ranges(), 1);
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
    fn discarding_the_overlay_leaves_the_inner_memory_alone() {
        let inner = grown(1);
        inner.write(0, &[9; 8]);
        {
            let shadow = ShadowMemory::new(inner);
            shadow.write(0, &[1; 8]);
            assert_eq!(read(&shadow, 0, 8), vec![1; 8]);
        }
        // `inner` was moved into the shadow and dropped with it; the original bytes are gone with
        // it, which is exactly the point: an attempt that is abandoned costs nothing real.
        let fresh = grown(1);
        fresh.write(0, &[9; 8]);
        let shadow = ShadowMemory::new(fresh);
        shadow.write(0, &[1; 8]);
        let discarded = shadow.into_inner();
        assert_eq!(
            read(&discarded, 0, 8),
            vec![9; 8],
            "an abandoned overlay never reached the inner memory"
        );
    }

    #[test]
    fn overlapping_writes_merge_into_one_range() {
        let inner = grown(1);
        let shadow = ShadowMemory::new(inner);
        shadow.write(0, &[1; 4]);
        shadow.write(2, &[2; 4]);
        assert_eq!(shadow.dirty_ranges(), 1, "the ranges merge");
        assert_eq!(shadow.dirty_bytes(), 6);
        assert_eq!(read(&shadow, 0, 6), vec![1, 1, 2, 2, 2, 2]);

        shadow.write(11, &[3; 2]);
        assert_eq!(shadow.dirty_ranges(), 2, "a disjoint write stays separate");
        assert_eq!(read(&shadow, 0, 13), {
            let mut expected = vec![1, 1, 2, 2, 2, 2, 0, 0, 0, 0, 0, 3, 3];
            expected[6..11].fill(0);
            expected
        });
    }

    #[test]
    fn a_read_spanning_dirty_and_clean_bytes_fills_both() {
        let inner = grown(1);
        inner.write(100, &[7; 16]);
        let shadow = ShadowMemory::new(inner);
        shadow.write(104, &[1; 4]);

        assert_eq!(
            read(&shadow, 100, 12),
            vec![7, 7, 7, 7, 1, 1, 1, 1, 7, 7, 7, 7]
        );
        // A read that starts before the range and ends inside it.
        assert_eq!(read(&shadow, 98, 4), vec![0, 0, 7, 7]);
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
