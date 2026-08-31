//! ADR 0088 §1 LTB block store, raw stable-memory backend.
//!
//! Replaces the [`super::ltb_reopen_prototype`] scaffold (and the StableBTreeMap
//! scaffold in `tree_csr_prototype`) with direct `Memory::read` / `Memory::write`
//! access against fixed-stride blocks (16-byte header + 4096-byte payload =
//! 4112 bytes per block). **Evidence-only**: the LTB store is wired into the
//! prototype here, not into the production [`crate::labeled::LabeledLaraGraph`]
//! (that lands in a later implementation slice).
#![allow(
    dead_code,
    reason = "Wired into the prototype in Plan 0315 Step 2; crate-root users come later."
)]
//!
//! # Header layout (64 bytes, ADR 0088 §1)
//!
//! | Offset | Size | Field                                                                 |
//! |-------:|-----:|-----------------------------------------------------------------------|
//! |      0 |    3 | magic `"LTB"`                                                          |
//! |      3 |    1 | layout version = 1                                                    |
//! |      4 |    4 | payload bytes per block = 4096 (wire truth; fail-closed otherwise)     |
//! |      8 |    4 | root fan-out cap R_max = 1024 (wire truth; fail-closed otherwise)      |
//! |     12 |    8 | `block_capacity` (allocated block slots in pages)                      |
//! |     20 |    8 | `tail_next` (first never-minted id)                                    |
//! |     28 |    4 | `free_head` (intrusive list head; `NULL_BLOCK` = `u32::MAX` when none) |
//! |     32 |    4 | `free_count`                                                          |
//! |     36 |   28 | reserved (must be zero)                                                |
//!
//! # Per-block header (16 bytes)
//!
//! | Offset | Size | Field                                                                  |
//! |-------:|-----:|------------------------------------------------------------------------|
//! |      0 |    1 | kind (0 Free, 1 Edge, 2 InlineProperty, 3 EdgeInterior, 4 InlinePropertyInterior) |
//! |      1 |    1 | reserved (zero)                                                        |
//! |      2 |    2 | bucket label key wire (low 16 bits of the wire [`crate::labeled::BucketLabelKey`]) |
//! |      4 |    4 | owner vertex id when `kind != Free`; next-free id when `kind == Free`  |
//! |      8 |    4 | stream ordinal                                                         |
//! |     12 |    1 | level (depth-1 leaf = 0; reserved metadata byte)                       |
//! |     13 |    3 | reserved (zero)                                                        |
//!
//! The 4096-byte payload follows immediately after the 16-byte block header
//! for a stride of 4112 bytes per block. Blocks are dense: block id `i` lives
//! at `HEADER_SIZE + i * BLOCK_STRIDE`.

use ic_stable_structures::Memory;

use crate::GrowFailed;

/// Header size in bytes (ADR 0088 §1).
pub(crate) const HEADER_SIZE: u64 = 64;
/// Per-block payload capacity in bytes (ADR 0088 §1 wire truth).
pub(crate) const BLOCK_PAYLOAD_BYTES: usize = 4096;
/// Per-block header size in bytes (ADR 0088 §1).
pub(crate) const BLOCK_HEADER_BYTES: usize = 16;
/// Per-block stride = header + payload (4112 bytes).
pub(crate) const BLOCK_STRIDE: u64 = BLOCK_HEADER_BYTES as u64 + BLOCK_PAYLOAD_BYTES as u64;
/// Root fan-out cap R_max (ADR 0088 §1 wire truth).
pub(crate) const R_MAX: u32 = 1024;
/// Sentinel value for `free_head == none`.
const NULL_BLOCK: u32 = u32::MAX;
/// Reserved bytes in the LTB header (between `free_count` and the end of header).
const HEADER_RESERVED_OFFSET: u64 = 36;
const HEADER_RESERVED_BYTES: usize = 28;
/// Header field offsets (kept aligned with the table above).
const OFFSET_MAGIC: u64 = 0;
const OFFSET_VERSION: u64 = 3;
const OFFSET_PAYLOAD_BYTES: u64 = 4;
const OFFSET_R_MAX: u64 = 8;
const OFFSET_BLOCK_CAPACITY: u64 = 12;
const OFFSET_TAIL_NEXT: u64 = 20;
const OFFSET_FREE_HEAD: u64 = 28;
const OFFSET_FREE_COUNT: u64 = 32;

/// Magic bytes that identify LTB store metadata.
const MAGIC: [u8; 3] = *b"LTB";
/// Layout version byte stored immediately after [`MAGIC`].
const LAYOUT_VERSION: u8 = 1;

/// Block kind (1 byte). See module docs for the full table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
#[allow(
    dead_code,
    reason = "Wired into the prototype in Plan 0315 Step 2; not yet used at the crate root."
)]
pub(crate) enum BlockKind {
    Free = 0,
    Edge = 1,
    InlineProperty = 2,
    EdgeInterior = 3,
    InlinePropertyInterior = 4,
}

impl BlockKind {
    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Free,
            1 => Self::Edge,
            2 => Self::InlineProperty,
            3 => Self::EdgeInterior,
            4 => Self::InlinePropertyInterior,
            // ADR 0088 §8 fail-closed: any unknown kind is an access-time
            // invariant violation; the read_block_header path panics so the
            // bug surfaces at the call site rather than silently propagating
            // a forged kind through the data path.
            _ => panic!("LtbRawBlockStore: unknown block kind {byte}"),
        }
    }
}

/// Errors raised by LTB block access (read/write outside the minted envelope).
#[derive(Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Wired into the prototype in Plan 0315 Step 2; not yet used at the crate root."
)]
pub(crate) enum BlockError {
    /// Block id is past `tail_next` (never minted or already released).
    NotMinted { id: u32 },
    /// Block id is past the allocated memory envelope (unbacked page).
    OutOfRange { id: u32 },
    /// `offset + len` exceeds the 4096-byte payload envelope (or
    /// `offset.checked_add(len)` overflowed). Raised by
    /// [`LtbRawBlockStore::read_payload_partial`] and
    /// [`LtbRawBlockStore::write_payload_partial`] for callers that
    /// exceed the block's payload capacity.
    OutOfBounds { id: u32, offset: usize, len: usize },
    /// Reopen header failed magic / reserved / wire-truth / counter checks.
    Init(InitError),
}

/// Errors raised by reopen-time header validation.
#[derive(Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Wired into the prototype in Plan 0315 Step 2; not yet used at the crate root."
)]
pub enum InitError {
    /// Stored magic does not match `b"LTB"`.
    BadMagic { actual: [u8; 3] },
    /// Layout version is not the supported version.
    IncompatibleVersion(u8),
    /// Header reserved bytes (28 bytes) are not zero.
    NonZeroReserved,
    /// `payload_bytes` field is not 4096.
    BadPayloadBytes(u32),
    /// `R_max` field is not 1024.
    BadRMax(u32),
    /// Counter consistency: `free_count > tail_next`, `tail_next > block_capacity`,
    /// or any other cross-field invariant.
    CounterMismatch {
        free_count: u32,
        tail_next: u32,
        block_capacity: u32,
    },
    /// Memory backing does not contain the LTB header page.
    TruncatedHeader,
    /// Free-list walk exceeded `min(free_count, declared envelope)` or hit a
    /// non-Free kind, an out-of-range id, or a cycle.
    FreeListCorrupt,
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(
                    f,
                    "ltb header magic mismatch: got {actual:?}, expected b\"LTB\""
                )
            }
            Self::IncompatibleVersion(v) => {
                write!(f, "ltb header version {v} is not the supported version 1")
            }
            Self::NonZeroReserved => write!(f, "ltb header reserved bytes are not zero"),
            Self::BadPayloadBytes(p) => {
                write!(f, "ltb header payload_bytes {p} is not the supported 4096")
            }
            Self::BadRMax(r) => {
                write!(f, "ltb header R_max {r} is not the supported 1024")
            }
            Self::CounterMismatch {
                free_count,
                tail_next,
                block_capacity,
            } => write!(
                f,
                "ltb counter inconsistency: free_count={free_count}, tail_next={tail_next}, block_capacity={block_capacity}"
            ),
            Self::TruncatedHeader => write!(f, "ltb memory is smaller than the header page"),
            Self::FreeListCorrupt => write!(
                f,
                "ltb free-list walk exceeded declared envelope or hit a non-Free kind / out-of-range id / cycle"
            ),
        }
    }
}

impl std::error::Error for InitError {}

/// Raw-block LTB store, generic over [`Memory`].
///
/// Holds the LTB header fields in cached form so the read/write path skips
/// stable memory on the hot scan path; mutations invalidate the cache by
/// rewriting the canonical header bytes through [`Self::write_header`].
#[allow(
    dead_code,
    reason = "All methods are exercised by benches and tests; allow until wired."
)]
pub(crate) struct LtbRawBlockStore<M: Memory> {
    memory: M,
    payload_bytes: u32,
    r_max: u32,
    block_capacity: u32,
    tail_next: u32,
    free_head: u32,
    free_count: u32,
}

impl<M: Memory> LtbRawBlockStore<M> {
    // ----- header I/O ------------------------------------------------------

    fn write_header(&mut self) -> Result<(), GrowFailed> {
        crate::safe_write(&self.memory, OFFSET_MAGIC, &MAGIC)?;
        crate::safe_write(&self.memory, OFFSET_VERSION, &[LAYOUT_VERSION])?;
        crate::safe_write(
            &self.memory,
            OFFSET_PAYLOAD_BYTES,
            &self.payload_bytes.to_le_bytes(),
        )?;
        crate::safe_write(&self.memory, OFFSET_R_MAX, &self.r_max.to_le_bytes())?;
        crate::write_u64(
            &self.memory,
            crate::types::Address::from(OFFSET_BLOCK_CAPACITY),
            u64::from(self.block_capacity),
        );
        crate::write_u64(
            &self.memory,
            crate::types::Address::from(OFFSET_TAIL_NEXT),
            u64::from(self.tail_next),
        );
        crate::safe_write(
            &self.memory,
            OFFSET_FREE_HEAD,
            &self.free_head.to_le_bytes(),
        )?;
        crate::safe_write(
            &self.memory,
            OFFSET_FREE_COUNT,
            &self.free_count.to_le_bytes(),
        )?;
        // Zero the reserved region explicitly. On real stable memory a
        // freshly-grown page reads as zero; on the VectorMemory test backend
        // a zero-fill ensures reopen validation never trips on stale bytes.
        let zero_reserved = [0u8; HEADER_RESERVED_BYTES];
        crate::safe_write(&self.memory, HEADER_RESERVED_OFFSET, &zero_reserved)?;
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(OFFSET_MAGIC, &mut magic);
        memory.read(OFFSET_VERSION, &mut version);
        let mut payload = [0u8; 4];
        let mut r_max = [0u8; 4];
        memory.read(OFFSET_PAYLOAD_BYTES, &mut payload);
        memory.read(OFFSET_R_MAX, &mut r_max);
        HeaderV1 {
            magic,
            version: version[0],
            payload_bytes: u32::from_le_bytes(payload),
            r_max: u32::from_le_bytes(r_max),
            block_capacity: crate::read_u64(
                memory,
                crate::types::Address::from(OFFSET_BLOCK_CAPACITY),
            ) as u32,
            tail_next: crate::read_u64(memory, crate::types::Address::from(OFFSET_TAIL_NEXT))
                as u32,
            free_head: {
                let mut buf = [0u8; 4];
                memory.read(OFFSET_FREE_HEAD, &mut buf);
                u32::from_le_bytes(buf)
            },
            free_count: {
                let mut buf = [0u8; 4];
                memory.read(OFFSET_FREE_COUNT, &mut buf);
                u32::from_le_bytes(buf)
            },
        }
    }

    fn validate_header(memory: &M, h: &HeaderV1) -> Result<(), InitError> {
        if h.magic != MAGIC {
            return Err(InitError::BadMagic { actual: h.magic });
        }
        if h.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(h.version));
        }
        if !Self::region_is_zero(memory, HEADER_RESERVED_OFFSET, HEADER_RESERVED_BYTES) {
            return Err(InitError::NonZeroReserved);
        }
        if h.payload_bytes != BLOCK_PAYLOAD_BYTES as u32 {
            return Err(InitError::BadPayloadBytes(h.payload_bytes));
        }
        if h.r_max != R_MAX {
            return Err(InitError::BadRMax(h.r_max));
        }
        if h.free_count > h.tail_next
            || h.tail_next > h.block_capacity
            || (h.free_head == NULL_BLOCK) != (h.free_count == 0)
        {
            return Err(InitError::CounterMismatch {
                free_count: h.free_count,
                tail_next: h.tail_next,
                block_capacity: h.block_capacity,
            });
        }
        Ok(())
    }

    /// Walks the free list up to `min(free_count, envelope)` ids, checking
    /// bounds, kind (must be [`BlockKind::Free`]), and cycle.
    fn validate_free_list(memory: &M, header: &HeaderV1) -> Result<(), InitError> {
        let limit = header.free_count.min(header.tail_next);
        let mut current = header.free_head;
        let mut steps = 0u32;
        let mut visited = 0u32;
        while current != NULL_BLOCK {
            if visited >= limit {
                return Err(InitError::FreeListCorrupt);
            }
            if current >= header.tail_next {
                return Err(InitError::FreeListCorrupt);
            }
            if steps > header.free_count {
                return Err(InitError::FreeListCorrupt);
            }
            let offset = Self::block_offset(current);
            let mut kind_byte = [0u8; 1];
            memory.read(offset, &mut kind_byte);
            if BlockKind::from_byte(kind_byte[0]) != BlockKind::Free {
                return Err(InitError::FreeListCorrupt);
            }
            let mut next_buf = [0u8; 4];
            memory.read(offset + 4, &mut next_buf);
            current = u32::from_le_bytes(next_buf);
            visited += 1;
            steps += 1;
        }
        if visited != header.free_count {
            return Err(InitError::FreeListCorrupt);
        }
        Ok(())
    }

    /// Returns `true` when every byte in `len` bytes at `offset` reads as zero.
    fn region_is_zero(memory: &M, offset: u64, len: usize) -> bool {
        let mut buf = [0u8; 64];
        memory.read(offset, &mut buf[..len]);
        buf[..len].iter().all(|&byte| byte == 0)
    }

    /// Compute the byte offset of block id `id`'s per-block header.
    #[inline]
    pub(crate) fn block_offset(id: u32) -> u64 {
        HEADER_SIZE + u64::from(id) * BLOCK_STRIDE
    }

    // ----- constructors ----------------------------------------------------

    /// Create a fresh empty LTB store. Memory is grown to fit at least the
    /// 64-byte header (no block pages are minted up front).
    pub(crate) fn new(memory: M) -> Result<Self, GrowFailed> {
        let mut store = Self {
            memory,
            payload_bytes: BLOCK_PAYLOAD_BYTES as u32,
            r_max: R_MAX,
            block_capacity: 0,
            tail_next: 0,
            free_head: NULL_BLOCK,
            free_count: 0,
        };
        store.write_header()?;
        Ok(store)
    }

    /// Reopen a previously-populated LTB store. Validates magic, version,
    /// reserved bytes, wire-truth payload_bytes and R_max, counter
    /// consistency, and walks the free list up to its declared envelope with
    /// bounds/kind/cycle checks.
    pub(crate) fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::TruncatedHeader);
        }
        let header = Self::read_header(&memory);
        Self::validate_header(&memory, &header)?;
        Self::validate_free_list(&memory, &header)?;
        Ok(Self {
            memory,
            payload_bytes: header.payload_bytes,
            r_max: header.r_max,
            block_capacity: header.block_capacity,
            tail_next: header.tail_next,
            free_head: header.free_head,
            free_count: header.free_count,
        })
    }

    // ----- accessors --------------------------------------------------------

    pub(crate) fn payload_bytes(&self) -> usize {
        self.payload_bytes as usize
    }

    pub(crate) fn r_max(&self) -> u32 {
        self.r_max
    }

    pub(crate) fn block_capacity(&self) -> u32 {
        self.block_capacity
    }

    pub(crate) fn tail_next(&self) -> u32 {
        self.tail_next
    }

    pub(crate) fn free_count(&self) -> u32 {
        self.free_count
    }

    pub(crate) fn free_head(&self) -> u32 {
        self.free_head
    }

    /// Consumes the store and returns the backing [`Memory`]. Used by reopen
    /// tests to construct a fresh `LtbRawBlockStore::init` against the same
    /// bytes.
    #[cfg(test)]
    pub(crate) fn into_memory(self) -> M {
        self.memory
    }

    // ----- per-block I/O ---------------------------------------------------

    /// Reads the per-block header at id `id`. Panics if `id >= tail_next`
    /// (ADR 0088 §8 access-time invariant: never read unbacked ids).
    pub(crate) fn read_block_header(&self, id: u32) -> BlockHeader {
        assert!(
            id < self.tail_next,
            "LtbRawBlockStore::read_block_header({id}) past tail_next={}",
            self.tail_next
        );
        let mut buf = [0u8; BLOCK_HEADER_BYTES];
        self.memory.read(Self::block_offset(id), &mut buf);
        BlockHeader::from_bytes(buf)
    }

    /// Writes the per-block header at id `id`. Panics if `id >= tail_next`.
    pub(crate) fn write_block_header(&mut self, id: u32, header: &BlockHeader) {
        assert!(
            id < self.tail_next,
            "LtbRawBlockStore::write_block_header({id}) past tail_next={}",
            self.tail_next
        );
        let bytes = header.to_bytes();
        self.memory.write(Self::block_offset(id), &bytes);
    }

    /// Reads the 4096-byte payload at id `id` into `dst`.
    pub(crate) fn read_payload(
        &self,
        id: u32,
        dst: &mut [u8; BLOCK_PAYLOAD_BYTES],
    ) -> Result<(), BlockError> {
        if id >= self.tail_next {
            return Err(BlockError::NotMinted { id });
        }
        let offset = Self::block_offset(id) + BLOCK_HEADER_BYTES as u64;
        self.memory.read(offset, dst);
        Ok(())
    }

    /// Reads `dst.len()` bytes starting at `offset` within the payload of
    /// block `id` into `dst`. Use this when only a small portion of a block
    /// is needed (e.g. `random_ordinal_access` reads 4 bytes at
    /// `slot * 4`) to avoid materializing a 4 KiB stack buffer per call.
    /// Bounds check: `offset + dst.len() <= BLOCK_PAYLOAD_BYTES`. The
    /// kind/owner/ordinal/level fields in the 16-byte block header are not
    /// touched.
    pub(crate) fn read_payload_partial(
        &self,
        id: u32,
        offset: usize,
        dst: &mut [u8],
    ) -> Result<(), BlockError> {
        if id >= self.tail_next {
            return Err(BlockError::NotMinted { id });
        }
        let end = offset
            .checked_add(dst.len())
            .ok_or(BlockError::OutOfBounds {
                id,
                offset,
                len: dst.len(),
            })?;
        if end > BLOCK_PAYLOAD_BYTES {
            return Err(BlockError::OutOfBounds {
                id,
                offset,
                len: dst.len(),
            });
        }
        let base = Self::block_offset(id) + BLOCK_HEADER_BYTES as u64 + offset as u64;
        self.memory.read(base, dst);
        Ok(())
    }

    /// Writes the 4096-byte payload at id `id` from `src`.
    pub(crate) fn write_payload(
        &mut self,
        id: u32,
        src: &[u8; BLOCK_PAYLOAD_BYTES],
    ) -> Result<(), BlockError> {
        if id >= self.tail_next {
            return Err(BlockError::NotMinted { id });
        }
        let offset = Self::block_offset(id) + BLOCK_HEADER_BYTES as u64;
        self.memory.write(offset, src);
        Ok(())
    }

    /// Writes `src.len()` bytes starting at `offset` within the payload of
    /// block `id`. Use this when only a small portion of a block has changed
    /// (e.g. one slot within a 4 KiB block) to avoid the read-modify-write
    /// cost of [`Self::write_payload`]. Bounds check: `offset + src.len()
    /// <= BLOCK_PAYLOAD_BYTES`. The kind/owner/ordinal/level fields in the
    /// 16-byte block header are not touched.
    pub(crate) fn write_payload_partial(
        &mut self,
        id: u32,
        offset: usize,
        src: &[u8],
    ) -> Result<(), BlockError> {
        if id >= self.tail_next {
            return Err(BlockError::NotMinted { id });
        }
        let end = offset
            .checked_add(src.len())
            .ok_or(BlockError::OutOfBounds {
                id,
                offset,
                len: src.len(),
            })?;
        if end > BLOCK_PAYLOAD_BYTES {
            return Err(BlockError::OutOfBounds {
                id,
                offset,
                len: src.len(),
            });
        }
        let base = Self::block_offset(id) + BLOCK_HEADER_BYTES as u64 + offset as u64;
        self.memory.write(base, src);
        Ok(())
    }

    // ----- mint / release --------------------------------------------------

    /// Mint a new block id. Pops the free list if non-empty; otherwise mints
    /// at `tail_next` and grows memory by [`BLOCK_STRIDE`] (rounded up to the
    /// nearest WASM page). The returned id has `kind = Free` until the caller
    /// writes a header via [`Self::write_block_header`].
    pub(crate) fn mint(&mut self) -> Result<u32, GrowFailed> {
        if self.free_head != NULL_BLOCK {
            let id = self.free_head;
            // Pop the free list: read the next-free id from the popped block's
            // header (offset 4 within the 16-byte block header), advance
            // free_head, decrement free_count. The popped block's kind is
            // rewritten to a sensible non-Free default so a caller that
            // immediately re-releases it does not trip the pop-time guard.
            let mut next_buf = [0u8; 4];
            self.memory.read(Self::block_offset(id) + 4, &mut next_buf);
            self.free_head = u32::from_le_bytes(next_buf);
            self.free_count = self
                .free_count
                .checked_sub(1)
                .expect("LtbRawBlockStore: free_count underflow");
            // Default-populated header: caller may rewrite kind via
            // write_block_header once the canonical body is committed.
            let default_header = BlockHeader {
                kind: BlockKind::Edge,
                bucket_label_key_wire: 0,
                owner_or_next_free: 0,
                ordinal: 0,
                level: 0,
                reserved: [0; 3],
            };
            self.write_block_header(id, &default_header);
            // Header counter fields change; persist them.
            self.write_header()?;
            Ok(id)
        } else {
            let id = self.tail_next;
            let new_tail = self
                .tail_next
                .checked_add(1)
                .expect("LtbRawBlockStore: tail_next overflow");
            // Grow memory so the new block's stride fits.
            let need_bytes = Self::block_offset(new_tail);
            let size_bytes = self.memory.size().saturating_mul(crate::WASM_PAGE_SIZE);
            if size_bytes < need_bytes {
                let diff_bytes = need_bytes - size_bytes;
                let diff_pages = diff_bytes
                    .checked_add(crate::WASM_PAGE_SIZE - 1)
                    .expect("LtbRawBlockStore: page count overflow")
                    / crate::WASM_PAGE_SIZE;
                if self.memory.grow(diff_pages) == -1 {
                    return Err(GrowFailed {
                        current_size: self.memory.size(),
                        delta: diff_pages,
                    });
                }
            }
            // Advance counters before writing the default header so the
            // write_block_header precondition (`id < tail_next`) holds.
            self.tail_next = new_tail;
            self.block_capacity = self
                .block_capacity
                .checked_add(1)
                .expect("LtbRawBlockStore: block_capacity overflow");
            // Initialize the per-block header so the block is immediately
            // releasable: a fresh mint defaults to `Edge` (the most common
            // kind in the prototype) with the rest of the metadata zeroed.
            // The caller rewrites kind via `write_block_header` once the
            // canonical body is committed.
            let default_header = BlockHeader {
                kind: BlockKind::Edge,
                bucket_label_key_wire: 0,
                owner_or_next_free: 0,
                ordinal: 0,
                level: 0,
                reserved: [0; 3],
            };
            self.write_block_header(id, &default_header);
            self.write_header()?;
            Ok(id)
        }
    }

    /// Release a previously-minted block id back to the free list.
    /// Pop-time guard (ADR 0088 §8): the current block kind must not already
    /// be [`BlockKind::Free`]. Rewrites the block header to `kind = Free,
    /// owner = previous free_head, rest = 0` and pushes the id onto the
    /// intrusive list head.
    pub(crate) fn release(&mut self, id: u32) -> Result<(), BlockError> {
        if id >= self.tail_next {
            return Err(BlockError::NotMinted { id });
        }
        let current = self.read_block_header(id);
        if current.kind == BlockKind::Free {
            // Pop-time guard: refuse to re-release a Free block. A correct
            // production caller never asks to release a block that's already
            // Free; this is a programming-error surface, so panic to make
            // the bug visible.
            panic!("LtbRawBlockStore::release({id}): block is already Free (double release)");
        }
        let new_header = BlockHeader {
            kind: BlockKind::Free,
            bucket_label_key_wire: 0,
            owner_or_next_free: self.free_head,
            ordinal: 0,
            level: 0,
            reserved: [0; 3],
        };
        self.write_block_header(id, &new_header);
        self.free_head = id;
        self.free_count = self
            .free_count
            .checked_add(1)
            .expect("LtbRawBlockStore: free_count overflow");
        self.write_header()
            .expect("LtbRawBlockStore: header write after release failed");
        Ok(())
    }

    // ----- free-list walk --------------------------------------------------

    /// Walks the intrusive free list from `free_head` for at most
    /// `min(free_count, envelope)` ids. Returns the visited ids in order.
    /// Used by reopen-time validation and the Gate 4 reopen walk.
    pub(crate) fn walk_free_list(&self, envelope: u32) -> Vec<u32> {
        let limit = self.free_count.min(envelope);
        let mut visited = Vec::with_capacity(limit as usize);
        let mut current = self.free_head;
        let mut steps = 0u32;
        while current != NULL_BLOCK && visited.len() < limit as usize {
            if steps > self.free_count {
                break;
            }
            let header = self.read_block_header(current);
            assert_eq!(
                header.kind,
                BlockKind::Free,
                "LtbRawBlockStore::walk_free_list: id {current} has kind {:?}",
                header.kind
            );
            visited.push(current);
            current = header.owner_or_next_free;
            steps += 1;
        }
        visited
    }
}

/// Header read at reopen time.
#[allow(
    dead_code,
    reason = "Wired into the prototype in Plan 0315 Step 2; not yet used at the crate root."
)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    payload_bytes: u32,
    r_max: u32,
    block_capacity: u32,
    tail_next: u32,
    free_head: u32,
    free_count: u32,
}

/// Per-block header (16 bytes) as a Rust-side value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Wired into the prototype in Plan 0315 Step 2; not yet used at the crate root."
)]
pub(crate) struct BlockHeader {
    pub(crate) kind: BlockKind,
    pub(crate) bucket_label_key_wire: u16,
    /// Owner vertex id when `kind != Free`; next-free id when `kind == Free`.
    pub(crate) owner_or_next_free: u32,
    /// Stream ordinal (used for bucket/accounting metadata).
    pub(crate) ordinal: u32,
    /// Level (0 = depth-1 leaf; reserved metadata byte).
    pub(crate) level: u8,
    pub(crate) reserved: [u8; 3],
}

impl BlockHeader {
    fn to_bytes(self) -> [u8; BLOCK_HEADER_BYTES] {
        let mut buf = [0u8; BLOCK_HEADER_BYTES];
        buf[0] = self.kind as u8;
        buf[1] = 0; // reserved byte (already 0)
        buf[2..4].copy_from_slice(&self.bucket_label_key_wire.to_le_bytes());
        buf[4..8].copy_from_slice(&self.owner_or_next_free.to_le_bytes());
        buf[8..12].copy_from_slice(&self.ordinal.to_le_bytes());
        buf[12] = self.level;
        buf[13..16].copy_from_slice(&self.reserved);
        buf
    }

    fn from_bytes(buf: [u8; BLOCK_HEADER_BYTES]) -> Self {
        Self {
            kind: BlockKind::from_byte(buf[0]),
            bucket_label_key_wire: u16::from_le_bytes([buf[2], buf[3]]),
            owner_or_next_free: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ordinal: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            level: buf[12],
            reserved: [buf[13], buf[14], buf[15]],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    fn block_offset_for_test(id: u32) -> u64 {
        LtbRawBlockStore::<crate::VectorMemory>::block_offset(id)
    }

    fn fresh() -> LtbRawBlockStore<crate::VectorMemory> {
        LtbRawBlockStore::new(vector_memory()).expect("fresh ltb")
    }

    fn round_trip_header() -> BlockHeader {
        BlockHeader {
            kind: BlockKind::Edge,
            bucket_label_key_wire: 0x1234,
            owner_or_next_free: 0xDEAD_BEEF,
            ordinal: 42,
            level: 0,
            reserved: [0; 3],
        }
    }

    #[test]
    fn header_round_trip_preserves_kind_owner_ordinal() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let header = round_trip_header();
        ltb.write_block_header(id, &header);
        let read_back = ltb.read_block_header(id);
        assert_eq!(read_back, header);
    }

    #[test]
    fn reopen_validates_magic_version_payload_bytes_r_max_and_reserved() {
        let mut ltb = fresh();
        // Mintage a block to make the page-backed byte interesting.
        let id = ltb.mint().expect("mint");
        ltb.write_block_header(id, &round_trip_header());
        let memory = ltb.into_memory();
        // Fresh header: reopen succeeds.
        let reopened = LtbRawBlockStore::init(memory.clone()).expect("reopen");
        assert_eq!(reopened.tail_next(), 1);
        assert_eq!(reopened.payload_bytes(), BLOCK_PAYLOAD_BYTES);
        assert_eq!(reopened.r_max(), R_MAX);
        // Helper to clone and tamper one byte / range of bytes.
        fn tamper(memory: &crate::VectorMemory, offset: u64, value: &[u8]) -> crate::VectorMemory {
            let mut new_bytes = memory.borrow().clone();
            new_bytes[offset as usize..offset as usize + value.len()].copy_from_slice(value);
            std::rc::Rc::new(std::cell::RefCell::new(new_bytes))
        }
        // Tamper with the magic byte: reopen rejects.
        let bad_magic = tamper(&memory, OFFSET_MAGIC, b"XTB");
        let err = LtbRawBlockStore::init(bad_magic).err().unwrap();
        assert!(matches!(err, InitError::BadMagic { .. }), "got {err:?}");
        // Tamper with the reserved region: reopen rejects.
        let bad_reserved = tamper(&memory, HEADER_RESERVED_OFFSET, &[1]);
        let err = LtbRawBlockStore::init(bad_reserved).err().unwrap();
        assert!(matches!(err, InitError::NonZeroReserved), "got {err:?}");
        // Tamper with payload_bytes: reopen rejects.
        let bad_payload = tamper(
            &memory,
            OFFSET_PAYLOAD_BYTES,
            &((BLOCK_PAYLOAD_BYTES as u32 + 1).to_le_bytes()),
        );
        let err = LtbRawBlockStore::init(bad_payload).err().unwrap();
        assert!(matches!(err, InitError::BadPayloadBytes(_)), "got {err:?}");
        // Tamper with R_max: reopen rejects.
        let bad_r_max = tamper(&memory, OFFSET_R_MAX, &((R_MAX + 1).to_le_bytes()));
        let err = LtbRawBlockStore::init(bad_r_max).err().unwrap();
        assert!(matches!(err, InitError::BadRMax(_)), "got {err:?}");
        // Tamper with layout version: reopen rejects.
        let bad_version = tamper(&memory, OFFSET_VERSION, &[99]);
        let err = LtbRawBlockStore::init(bad_version).err().unwrap();
        assert!(
            matches!(err, InitError::IncompatibleVersion(99)),
            "got {err:?}"
        );
    }

    #[test]
    fn reopen_counter_consistency_free_count_must_not_exceed_tail_next() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        ltb.write_block_header(id, &round_trip_header());
        let memory = ltb.into_memory();
        let new_bytes = {
            let mut v = memory.borrow().clone();
            let bad: [u8; 4] = (1u32 + 1).to_le_bytes();
            v[OFFSET_FREE_COUNT as usize..OFFSET_FREE_COUNT as usize + 4].copy_from_slice(&bad);
            std::rc::Rc::new(std::cell::RefCell::new(v))
        };
        let err = LtbRawBlockStore::init(new_bytes).err().unwrap();
        assert!(
            matches!(err, InitError::CounterMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn mint_release_pop_round_trip_returns_lifo_ids() {
        let mut ltb = fresh();
        let a = ltb.mint().expect("mint a");
        let b = ltb.mint().expect("mint b");
        let c = ltb.mint().expect("mint c");
        assert_eq!(ltb.tail_next(), 3);
        assert_eq!(ltb.free_count(), 0);
        ltb.release(a).expect("release a");
        ltb.release(b).expect("release b");
        ltb.release(c).expect("release c");
        assert_eq!(ltb.free_count(), 3);
        let popped1 = ltb.mint().expect("pop 1");
        let popped2 = ltb.mint().expect("pop 2");
        let popped3 = ltb.mint().expect("pop 3");
        assert_eq!(popped1, c);
        assert_eq!(popped2, b);
        assert_eq!(popped3, a);
        assert_eq!(ltb.free_count(), 0);
        assert_eq!(ltb.tail_next(), 3);
    }

    #[test]
    fn reopen_free_list_walk_validates_bounds_kind_and_cycle() {
        let mut ltb = fresh();
        // Build a free list of 4 entries.
        for _ in 0..4u32 {
            let id = ltb.mint().expect("mint");
            ltb.write_block_header(id, &round_trip_header());
            ltb.release(id).expect("release");
        }
        let memory = ltb.into_memory();
        // Clean reopen succeeds.
        let _ = LtbRawBlockStore::init(memory.clone()).expect("clean reopen");
        // Tamper: corrupt the kind byte of the free-list head so the walker
        // sees a non-Free kind. Reopen rejects with FreeListCorrupt.
        let head = {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(
                &memory.borrow()[OFFSET_FREE_HEAD as usize..OFFSET_FREE_HEAD as usize + 4],
            );
            u32::from_le_bytes(buf)
        };
        let head_offset = block_offset_for_test(head);
        let tampered = {
            let mut v = memory.borrow().clone();
            v[head_offset as usize] = BlockKind::Edge as u8;
            std::rc::Rc::new(std::cell::RefCell::new(v))
        };
        let err = LtbRawBlockStore::init(tampered).err().unwrap();
        assert!(matches!(err, InitError::FreeListCorrupt), "got {err:?}");
    }

    #[test]
    fn walk_free_list_returns_ids_in_lifo_order() {
        let mut ltb = fresh();
        let ids: Vec<u32> = (0..5).map(|_| ltb.mint().expect("mint")).collect();
        for id in &ids {
            ltb.release(*id).expect("release");
        }
        // Walk the full list and assert the order matches what was released.
        let visited = ltb.walk_free_list(u32::MAX);
        // LIFO: the last-released id is at the head.
        let expected_head = *ids.last().expect("non-empty");
        assert_eq!(visited[0], expected_head);
        assert_eq!(visited.len(), ids.len());
    }

    #[test]
    fn walk_free_list_respects_envelope() {
        let mut ltb = fresh();
        // Mint 10, then release all 10 without re-minting, so the free list
        // holds 10 entries and the walk is bounded by the envelope parameter.
        let mut ids = Vec::with_capacity(10);
        for _ in 0..10u32 {
            ids.push(ltb.mint().expect("mint"));
        }
        for id in &ids {
            ltb.release(*id).expect("release");
        }
        assert_eq!(ltb.free_count(), 10);
        let visited = ltb.walk_free_list(3);
        assert_eq!(visited.len(), 3, "envelope caps walk length");
    }

    #[test]
    fn release_double_release_panics() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        ltb.write_block_header(id, &round_trip_header());
        ltb.release(id).expect("release once");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = ltb.release(id);
        }));
        assert!(
            result.is_err(),
            "second release must panic (pop-time guard)"
        );
    }

    #[test]
    fn payload_round_trip_4096_bytes() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let src: [u8; BLOCK_PAYLOAD_BYTES] = std::array::from_fn(|i| (i % 251) as u8);
        ltb.write_payload(id, &src).expect("write payload");
        let mut dst = [0u8; BLOCK_PAYLOAD_BYTES];
        ltb.read_payload(id, &mut dst).expect("read payload");
        assert_eq!(dst, src);
    }

    #[test]
    fn reopen_init_rejects_truncated_header() {
        let memory: crate::VectorMemory = vector_memory();
        let err = LtbRawBlockStore::init(memory).err().unwrap();
        assert!(matches!(err, InitError::TruncatedHeader), "got {err:?}");
    }

    #[test]
    fn write_payload_partial_writes_only_offset_range() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        // Pre-fill the block with a known pattern via write_payload.
        let baseline: [u8; BLOCK_PAYLOAD_BYTES] = std::array::from_fn(|i| (i % 251) as u8);
        ltb.write_payload(id, &baseline).expect("write payload");
        // Overwrite 4 bytes at offset 100..104.
        let patch: [u8; 4] = [0xDE, 0xAD, 0xBE, 0xEF];
        ltb.write_payload_partial(id, 100, &patch)
            .expect("write_payload_partial");
        let mut dst = [0u8; BLOCK_PAYLOAD_BYTES];
        ltb.read_payload(id, &mut dst).expect("read payload");
        assert_eq!(dst[100..104], patch);
        // Surrounding bytes unchanged.
        assert_eq!(&dst[..100], &baseline[..100]);
        assert_eq!(&dst[104..], &baseline[104..]);
    }

    #[test]
    fn write_payload_partial_rejects_out_of_bounds() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let src = [0u8; 4];
        // offset == BLOCK_PAYLOAD_BYTES → end == BLOCK_PAYLOAD_BYTES
        // is *not* out of bounds (an empty write at the very end is valid),
        // but a 4-byte write at the very end is.
        let err = ltb
            .write_payload_partial(id, BLOCK_PAYLOAD_BYTES - 2, &src)
            .err()
            .unwrap();
        assert!(
            matches!(err, BlockError::OutOfBounds { id: _, offset, len } if offset == BLOCK_PAYLOAD_BYTES - 2 && len == 4),
            "expected OutOfBounds, got {err:?}"
        );
        // offset + len overflow guard: huge offset should be rejected.
        let err = ltb
            .write_payload_partial(id, usize::MAX, &src)
            .err()
            .unwrap();
        assert!(
            matches!(err, BlockError::OutOfBounds { .. }),
            "expected OutOfBounds on overflow, got {err:?}"
        );
    }

    // ----- Plan 0322: read_payload_partial ---------------------------------

    #[test]
    fn read_payload_partial_reads_only_offset_range() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let baseline: [u8; BLOCK_PAYLOAD_BYTES] = std::array::from_fn(|i| (i % 251) as u8);
        ltb.write_payload(id, &baseline).expect("write payload");
        // Read 4 bytes at offset 100..104.
        let mut dst = [0u8; 4];
        ltb.read_payload_partial(id, 100, &mut dst)
            .expect("read_payload_partial");
        assert_eq!(dst, baseline[100..104]);
    }

    #[test]
    fn read_payload_partial_zero_length_is_noop() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let baseline: [u8; BLOCK_PAYLOAD_BYTES] = [0xAB; BLOCK_PAYLOAD_BYTES];
        ltb.write_payload(id, &baseline).expect("write payload");
        // Zero-length read at any offset within the payload is a no-op.
        let mut dst: [u8; 0] = [];
        ltb.read_payload_partial(id, 0, &mut dst)
            .expect("read_payload_partial at start");
        ltb.read_payload_partial(id, 200, &mut dst)
            .expect("read_payload_partial at middle");
        ltb.read_payload_partial(id, BLOCK_PAYLOAD_BYTES, &mut dst)
            .expect("read_payload_partial at exact end (empty is valid)");
    }

    #[test]
    fn read_payload_partial_rejects_offset_out_of_bounds() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let mut dst = [0u8; 4];
        let err = ltb
            .read_payload_partial(id, BLOCK_PAYLOAD_BYTES, &mut dst)
            .err()
            .unwrap();
        assert!(
            matches!(err, BlockError::OutOfBounds { .. }),
            "expected OutOfBounds, got {err:?}"
        );
    }

    #[test]
    fn read_payload_partial_rejects_length_overflow() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        let mut dst = [0u8; 4];
        // offset in-range, but offset + len > BLOCK_PAYLOAD_BYTES.
        let err = ltb
            .read_payload_partial(id, BLOCK_PAYLOAD_BYTES - 2, &mut dst)
            .err()
            .unwrap();
        assert!(
            matches!(err, BlockError::OutOfBounds { id: _, offset, len } if offset == BLOCK_PAYLOAD_BYTES - 2 && len == 4),
            "len overflow should reject, got {err:?}"
        );
    }

    #[test]
    fn read_payload_partial_rejects_past_tail_next() {
        let ltb = fresh();
        let mut dst = [0u8; 4];
        let err = ltb.read_payload_partial(0, 0, &mut dst).err().unwrap();
        assert!(
            matches!(err, BlockError::NotMinted { id: 0 }),
            "expected NotMinted, got {err:?}"
        );
    }

    /// Plan 0315 / Step 3 (carried over from `ltb_reopen_prototype`):
    /// `fresh_free_list_is_empty` confirms an empty free list and that a
    /// `walk_free_list` returns nothing.
    #[test]
    fn fresh_free_list_is_empty() {
        let ltb = fresh();
        assert_eq!(ltb.free_count(), 0);
        let visited = ltb.walk_free_list(u32::MAX);
        assert!(visited.is_empty());
    }

    /// Plan 0315 / Step 3 (carried over from `ltb_reopen_prototype`):
    /// `pop_remint_repop_yields_same_id` confirms the pop-time guard lets the
    /// same id be re-acquired after a release/pop round trip.
    #[test]
    fn pop_remint_repop_yields_same_id() {
        let mut ltb = fresh();
        let id = ltb.mint().expect("mint");
        ltb.release(id).expect("release");
        // Pop should yield the same id (LIFO on the free list head).
        let popped = ltb.mint().expect("pop");
        assert_eq!(popped, id);
        // Releasing and popping again yields the same id, confirming the
        // pop-time guard does not hand out a different block.
        ltb.release(popped).expect("release");
        let second = ltb.mint().expect("pop 2");
        assert_eq!(second, id);
    }
}
