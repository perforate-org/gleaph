//! Dense variable-length byte store backing the swapped regions (plan 0295).
//!
//! `ic_stable_structures::vec::Vec` reserves a full `max_size` slot per element, so
//! variable-size blobs cannot live in a stable vector directly without capping their
//! length or wasting space on every slot. This module owns the one canonical dense
//! alternative: a fixed-size chunk arena ([`BlobArena`] over `Vec<BlobChunk>`) addressed
//! by [`BlobRef`] records `{chunk_start, chunk_count, byte_len}` that themselves live in
//! plain dense vectors indexed by `term_id` / deque slot.
//!
//! Rewrite economics: writing the same chunk count as an existing ref occupies rewrites
//! those chunks **in place** (no garbage); any other size appends a fresh run at the
//! arena frontier and orphans the old one. Orphaned bytes are inert until a future
//! compaction slice — the same documented lag class as stale tombstone bits before pass
//! completion and the decode-all/re-encode append path (v0 scale: fixture storage is
//! 141–193 KiB logical, so churn stays trivial).

use ic_stable_structures::Memory;
use ic_stable_structures::storable::{Bound as SBound, Storable};
use ic_stable_structures::vec::Vec as StableVec;
use std::borrow::Cow;

/// Bytes per arena chunk. Large enough that typical posting lists fit one chunk
/// (~1.3 k single-byte-delta postings), small enough to keep tail waste low.
pub(crate) const ARENA_CHUNK_BYTES: usize = 4096;

/// One fixed-size arena chunk.
#[derive(Clone)]
pub(crate) struct BlobChunk([u8; ARENA_CHUNK_BYTES]);

impl Storable for BlobChunk {
    const BOUND: SBound = SBound::Bounded {
        max_size: ARENA_CHUNK_BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        assert_eq!(
            bytes.len(),
            ARENA_CHUNK_BYTES,
            "corrupt blob arena chunk length"
        );
        Self(bytes.as_ref().try_into().expect("checked length"))
    }
}

/// Locator for one byte string inside a [`BlobArena`]. Fixed-size record; `chunk_count 0`
/// encodes the empty blob regardless of the other fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlobRef {
    pub(crate) chunk_start: u64,
    pub(crate) chunk_count: u32,
    pub(crate) byte_len: u32,
}

impl BlobRef {
    /// The empty-blob locator.
    pub(crate) const EMPTY: Self = Self {
        chunk_start: 0,
        chunk_count: 0,
        byte_len: 0,
    };

    /// True when the ref addresses no bytes.
    pub(crate) fn is_empty(self) -> bool {
        self.chunk_count == 0
    }

    fn chunk_count_for(byte_len: usize) -> u32 {
        byte_len.div_ceil(ARENA_CHUNK_BYTES) as u32
    }
}

/// Fixed-size `Storable` carrier for [`BlobRef`] (16 bytes: u64 start + u32 count +
/// u32 len, little-endian). The raw bytes are shared with [`crate::state`] record packing.
#[derive(Clone, Copy)]
pub(crate) struct BlobRefSlot(pub(crate) [u8; 16]);

impl From<BlobRef> for BlobRefSlot {
    fn from(r: BlobRef) -> Self {
        let mut out = [0u8; 16];
        out[0..8].copy_from_slice(&r.chunk_start.to_le_bytes());
        out[8..12].copy_from_slice(&r.byte_len.to_le_bytes());
        out[12..16].copy_from_slice(&r.chunk_count.to_le_bytes());
        Self(out)
    }
}

impl From<BlobRefSlot> for BlobRef {
    fn from(slot: BlobRefSlot) -> Self {
        let b = &slot.0;
        Self {
            chunk_start: u64::from_le_bytes(b[0..8].try_into().expect("fixed width")),
            byte_len: u32::from_le_bytes(b[8..12].try_into().expect("fixed width")),
            chunk_count: u32::from_le_bytes(b[12..16].try_into().expect("fixed width")),
        }
    }
}

impl Storable for BlobRefSlot {
    const BOUND: SBound = SBound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.as_ref().try_into().expect("corrupt blob ref width"))
    }
}

/// Where `StableVec` (ic-stable-structures 0.7) puts element 0: elements of a fixed-size type
/// follow its header back to back, one `max_size` slot each. Reads and in-place rewrites go to
/// the memory at these offsets, so they move only a blob's bytes instead of every whole 4 KiB
/// chunk; `raw_offsets_match_the_stable_vec_layout` pins the layout against `StableVec` itself.
const VEC_DATA_OFFSET: u64 = 64;

/// The shared chunk arena over one stable region.
pub(crate) struct BlobArena<M: Memory> {
    chunks: StableVec<BlobChunk, M>,
    /// The region `chunks` lives in, read and rewritten directly at a blob's byte span.
    memory: M,
}

impl<M: Memory + Clone> BlobArena<M> {
    /// Load-or-create an empty arena over `memory`.
    pub(crate) fn init(memory: M) -> Self {
        Self {
            chunks: StableVec::init(memory.clone()),
            memory,
        }
    }

    /// Byte offset of chunk `index` in the region.
    fn chunk_offset(index: u64) -> u64 {
        VEC_DATA_OFFSET + index * ARENA_CHUNK_BYTES as u64
    }

    /// Asserts `r` lies inside the arena's chunks.
    fn check_extent(&self, r: BlobRef) {
        assert!(
            r.chunk_start + u64::from(r.chunk_count) <= self.chunks.len(),
            "blob ref exceeds arena extent"
        );
    }

    /// Stores `bytes` as a fresh run at the arena frontier and returns its locator.
    pub(crate) fn put(&mut self, bytes: &[u8]) -> BlobRef {
        if bytes.is_empty() {
            return BlobRef::EMPTY;
        }
        let chunk_start = self.chunks.len();
        for chunk in chunks_of(bytes) {
            self.chunks.push(&BlobChunk(chunk));
        }
        BlobRef {
            chunk_start,
            chunk_count: BlobRef::chunk_count_for(bytes.len()),
            byte_len: bytes.len() as u32,
        }
    }

    /// Stores `bytes`, reusing `old`'s run when the chunk count is unchanged; otherwise
    /// allocates a fresh run and orphans the old one (see module docs).
    pub(crate) fn write_over(&mut self, old: BlobRef, bytes: &[u8]) -> BlobRef {
        let needed = BlobRef::chunk_count_for(bytes.len());
        if needed > 0
            && needed == old.chunk_count
            && old.chunk_start + u64::from(needed) <= self.chunks.len()
        {
            // The run's chunks are contiguous, and readers stop at `byte_len`: only the new
            // bytes need writing, not the zero padding of whole chunks.
            self.memory
                .write(Self::chunk_offset(old.chunk_start), bytes);
            return BlobRef {
                chunk_start: old.chunk_start,
                chunk_count: needed,
                byte_len: bytes.len() as u32,
            };
        }
        self.put(bytes)
    }

    /// Reads back the bytes addressed by `r` (empty slice for [`BlobRef::EMPTY`]).
    pub(crate) fn read(&self, r: BlobRef) -> Vec<u8> {
        if r.is_empty() {
            return Vec::new();
        }
        self.check_extent(r);
        let mut out = vec![0u8; r.byte_len as usize];
        self.memory
            .read(Self::chunk_offset(r.chunk_start), &mut out);
        out
    }

    /// Whether `r` holds exactly `bytes`, reading only when the lengths agree.
    pub(crate) fn matches(&self, r: BlobRef, bytes: &[u8]) -> bool {
        if r.byte_len as usize != bytes.len() {
            return false;
        }
        if r.is_empty() {
            return true;
        }
        self.check_extent(r);
        let mut stack = [0u8; 64];
        let mut heap = Vec::new();
        let buf = match stack.get_mut(..bytes.len()) {
            Some(buf) => buf,
            None => {
                heap.resize(bytes.len(), 0);
                &mut heap[..]
            }
        };
        self.memory.read(Self::chunk_offset(r.chunk_start), buf);
        buf == bytes
    }
}

/// Splits `bytes` into chunk-sized pieces; the trailing partial chunk is zero-padded in
/// memory only (its real length rides on the ref).
fn chunks_of(bytes: &[u8]) -> impl Iterator<Item = [u8; ARENA_CHUNK_BYTES]> + '_ {
    bytes.chunks(ARENA_CHUNK_BYTES).map(|chunk| {
        let mut out = [0u8; ARENA_CHUNK_BYTES];
        out[..chunk.len()].copy_from_slice(chunk);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn arena() -> BlobArena<VectorMemory> {
        BlobArena::init(VectorMemory::default())
    }

    #[test]
    fn raw_offsets_match_the_stable_vec_layout() {
        let mut a = arena();
        let first = a.put(b"lead");
        let multi: Vec<u8> = (0..ARENA_CHUNK_BYTES * 2 + 17).map(|i| i as u8).collect();
        let r = a.put(&multi);
        // Reassemble through StableVec's own reads, chunk by chunk.
        let via_vec: Vec<u8> = (r.chunk_start..r.chunk_start + u64::from(r.chunk_count))
            .flat_map(|index| a.chunks.get(index).unwrap().0)
            .take(multi.len())
            .collect();
        assert_eq!(via_vec, multi);
        assert_eq!(a.read(r), multi);
        assert_eq!(a.read(first), b"lead");
        // An in-place rewrite lands where StableVec reads it.
        let rewritten: Vec<u8> = multi.iter().map(|b| b ^ 0x5A).collect();
        assert_eq!(a.write_over(r, &rewritten), r);
        assert_eq!(
            a.chunks.get(r.chunk_start + 1).unwrap().0[..],
            rewritten[ARENA_CHUNK_BYTES..ARENA_CHUNK_BYTES * 2]
        );
        assert_eq!(a.read(r), rewritten);
        assert!(a.matches(first, b"lead"));
        assert!(!a.matches(first, b"leaf"));
        assert!(!a.matches(first, b"leads"));
        assert!(a.matches(r, &rewritten));
    }

    #[test]
    fn round_trips_empty_single_and_multi_chunk_blobs() {
        let mut a = arena();
        let empty_ref = a.put(b"");
        assert_eq!(a.read(empty_ref), Vec::<u8>::new());

        let small = b"hello postings".to_vec();
        let small_ref = a.put(&small);
        assert_eq!(small_ref.chunk_count, 1);
        assert_eq!(a.read(small_ref), small);

        // 2.5 chunks of deterministic pseudo-random bytes.
        let big: Vec<u8> = (0..ARENA_CHUNK_BYTES * 5 / 2)
            .map(|i| (i % 251) as u8)
            .collect();
        let big_ref = a.put(&big);
        assert_eq!(big_ref.chunk_count, 3);
        assert_eq!(big_ref.byte_len as usize, big.len());
        assert_eq!(a.read(big_ref), big);
    }

    #[test]
    fn same_chunk_count_rewrites_in_place_and_other_sizes_relocate() {
        let mut a = arena();
        let first = a.put(&vec![7u8; ARENA_CHUNK_BYTES]);

        // Same-size overwrite must stay in place and take effect.
        let second = a.write_over(first, &vec![9u8; ARENA_CHUNK_BYTES]);
        assert_eq!(second.chunk_start, first.chunk_start, "in place");
        assert_eq!(second.chunk_count, 1);
        assert_eq!(a.read(second), vec![9u8; ARENA_CHUNK_BYTES]);

        // Growth allocates a fresh run at the frontier and spans two chunks.
        let third = a.write_over(second, &vec![1u8; ARENA_CHUNK_BYTES + 1]);
        assert!(third.chunk_start > first.chunk_start, "relocated");
        assert_eq!(third.chunk_count, 2);
        assert_eq!(a.read(third), vec![1u8; ARENA_CHUNK_BYTES + 1]);

        // Shrink-to-empty detaches entirely.
        let fourth = a.write_over(third, b"");
        assert!(fourth.is_empty());
        assert_eq!(a.read(fourth), Vec::<u8>::new());
    }
}
