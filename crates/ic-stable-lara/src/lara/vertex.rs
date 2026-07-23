//! Stable LARA vertex column.
//!
//! The default row stores a packed locator (36-bit slab base + 28-bit tail), live edge count,
//! and slab slot count. Clean scans use [`Vertex::base_slot_start`] plus logical degree
//! ([`Vertex::live_edges`] / [`CsrVertex::degree`]); slab iteration may span
//! [`Vertex::slab_slots`] until maintenance compacts tombstoned edge slots away.
//!
//! Owned slab spans for inserts and relocation use CSR geometry:
//! `[base_slot_start, slab_window_exclusive_end)` derives from the next vertex's
//! `base_slot_start`, PMA leaf totals, or `elem_capacity`.
//!
//! # V1 layout
//!
//! For [`Vertex`], each row is 16 bytes: bytes `0..8` are the locator word (LE)
//! — low 36 bits are `base_slot_start`, high 28 bits are tail metadata (bit 0 tombstone;
//! bits 1–27 encode `(log_head + 1)`, `0` = no overflow log) — then `live_edges` and `slab_slots`.
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "LVX"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Number of vertices                    ↕ 4 bytes
//! --------------------------------------------------
//! Vertex row stride                     ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 52 bytes
//! -------------------------------------------------- <- Address 64
//! V_0                                   ↕ V::BYTES bytes
//! --------------------------------------------------
//! V_1                                   ↕ V::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! V_(len-1)                             ↕ V::BYTES bytes
//! --------------------------------------------------
//! ```

use crate::{
    GrowFailed, VertexId,
    lara::edge::DEFAULT_MAX_LOG_ENTRIES,
    read_u32, safe_write,
    slab_index::{
        decode_meta28, decode_slot_index, encode_locator_word, pack_vertex_tail28, slot_index_fits,
        try_encode_locator_word, try_pack_vertex_tail28, unpack_vertex_tail28,
    },
    traits::{CsrVertex, CsrVertexTombstone},
    types::Address,
    write_u32,
};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{borrow::Cow, cell::Cell, fmt};

/// Magic bytes that identify a LARA vertex-column memory.
pub const MAGIC: [u8; 3] = *b"LVX";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 64;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 8;
/// Stack buffer width for [`VertexStore::get`] when `V::BYTES` is small enough.
const INLINE_VERTEX_ROW_BYTES: usize = 64;

#[derive(Clone, Copy, Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u32,
    stride: u32,
}

/// Errors returned when reopening a persisted [`VertexStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The memory header does not contain the LARA vertex magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The persisted row width does not match the vertex type `V`.
    StrideMismatch {
        /// Expected row width for `V`.
        expected: u32,
        /// Row width read from stable memory.
        actual: u32,
    },
    /// The vertex type does not use a fixed-width [`Storable`] encoding.
    VariableWidthVertex,
    /// The store could not allocate its header while initializing empty memory.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad vertex magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(v) => write!(f, "unsupported vertex layout version {v}"),
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "vertex stride mismatch: expected {expected}, got {actual}"
                )
            }
            Self::VariableWidthVertex => {
                write!(f, "LARA vertices must use fixed-width Storable encoding")
            }
            Self::OutOfMemory => write!(f, "failed to allocate vertex metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Field validation errors for [`Vertex::try_from_parts`] and [`Vertex::try_read_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexFieldError {
    /// Wire slice length is not exactly [`Vertex::BYTES`].
    WireLengthMismatch,
    /// `base_slot_start` does not fit in 36 bits.
    SlotIndexOverflow,
    /// Overflow log head does not fit in the packed tail28 encoding.
    LogHeadOverflow,
    /// Overflow log head is not in `0..`[`DEFAULT_MAX_LOG_ENTRIES`].
    OverflowLogHeadOutOfRange,
}

impl fmt::Display for VertexFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WireLengthMismatch => write!(f, "vertex wire row must be exactly 16 bytes"),
            Self::SlotIndexOverflow => {
                write!(f, "vertex base_slot_start exceeds 36-bit slot index")
            }
            Self::LogHeadOverflow => write!(f, "vertex log head does not fit in packed tail28"),
            Self::OverflowLogHeadOutOfRange => write!(
                f,
                "vertex overflow log head must be < {DEFAULT_MAX_LOG_ENTRIES}"
            ),
        }
    }
}

impl std::error::Error for VertexFieldError {}

/// Default fixed-width LARA vertex row (16 bytes on wire).
///
/// `live_edges` is the logical out-degree (clean scans, rebalance packing).
/// `slab_slots` is the physical slab prefix width; it may be larger while tombstoned cells from
/// logical deletes await compaction on leaf rebalance.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vertex {
    locator: u64,
    /// Live edges counted by graph APIs ([`CsrVertex::degree`]).
    pub live_edges: u32,
    /// Physical slab cells reserved for this row ([`CsrVertex::stored_degree`]).
    pub slab_slots: u32,
}

impl Default for Vertex {
    fn default() -> Self {
        Self::from_parts(0, 0, 0, -1, false)
    }
}

impl Vertex {
    /// Fixed byte width of one encoded vertex row.
    pub const BYTES: usize = 16;

    /// Builds a row from logical fields (panics on invalid input in debug).
    #[inline]
    pub fn from_parts(
        base_slot_start: u64,
        live_edges: u32,
        slab_slots: u32,
        log_head: i32,
        deleted: bool,
    ) -> Self {
        Self::try_from_parts(base_slot_start, live_edges, slab_slots, log_head, deleted)
            .expect("Vertex::from_parts: invalid fields")
    }

    /// Fallible constructor with release-safe range checks.
    #[inline]
    pub fn try_from_parts(
        base_slot_start: u64,
        live_edges: u32,
        slab_slots: u32,
        log_head: i32,
        deleted: bool,
    ) -> Result<Self, VertexFieldError> {
        if !slot_index_fits(base_slot_start) {
            return Err(VertexFieldError::SlotIndexOverflow);
        }
        Self::validate_log_head(log_head)?;
        let tail =
            try_pack_vertex_tail28(log_head, deleted).ok_or(VertexFieldError::LogHeadOverflow)?;
        let locator = try_encode_locator_word(base_slot_start, tail)
            .ok_or(VertexFieldError::SlotIndexOverflow)?;
        Ok(Self {
            locator,
            live_edges,
            slab_slots,
        })
    }

    /// Global edge-slot index where this vertex's slab prefix starts.
    #[inline]
    pub fn base_slot_start(self) -> u64 {
        decode_slot_index(self.locator)
    }

    /// Head entry in the per-segment overflow log, or `-1` when no log is present.
    #[inline]
    pub fn log_head(self) -> i32 {
        unpack_vertex_tail28(decode_meta28(self.locator)).0
    }

    /// Logical deletion marker (tombstone bit in the locator tail).
    #[inline]
    pub fn deleted(self) -> bool {
        unpack_vertex_tail28(decode_meta28(self.locator)).1
    }

    #[inline]
    fn with_locator(mut self, locator: u64) -> Self {
        self.locator = locator;
        self
    }

    #[inline]
    fn with_tail28(self, tail: u32) -> Self {
        self.with_locator(encode_locator_word(self.base_slot_start(), tail))
    }

    /// Returns a copy with a new slab base (rejects out-of-range indices).
    #[inline]
    pub fn try_with_base_slot_start(self, start: u64) -> Result<Self, VertexFieldError> {
        if !slot_index_fits(start) {
            return Err(VertexFieldError::SlotIndexOverflow);
        }
        Ok(self.with_locator(encode_locator_word(start, decode_meta28(self.locator))))
    }

    /// Returns a copy with a new overflow log head (rejects out-of-range indices).
    #[inline]
    pub fn try_with_log_head(self, idx: i32) -> Result<Self, VertexFieldError> {
        Self::validate_log_head(idx)?;
        let tail =
            try_pack_vertex_tail28(idx, self.deleted()).ok_or(VertexFieldError::LogHeadOverflow)?;
        Ok(self.with_tail28(tail))
    }

    /// Encodes this vertex row into exactly [`Self::BYTES`] bytes.
    pub fn write_to(self, bytes: &mut [u8]) {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        bytes.copy_from_slice(&vertex_row_bytes(&self));
    }

    /// Decodes a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn read_from(bytes: &[u8]) -> Self {
        Self::try_read_from(bytes).expect("invalid Vertex wire bytes")
    }

    /// Decodes and validates a vertex row from exactly [`Self::BYTES`] bytes.
    pub fn try_read_from(bytes: &[u8]) -> Result<Self, VertexFieldError> {
        let chunk: [u8; Self::BYTES] = bytes
            .try_into()
            .map_err(|_| VertexFieldError::WireLengthMismatch)?;
        let vertex = Self {
            locator: u64::from_le_bytes(chunk[0..8].try_into().expect("locator")),
            live_edges: u32::from_le_bytes(chunk[8..12].try_into().expect("live")),
            slab_slots: u32::from_le_bytes(chunk[12..16].try_into().expect("slab")),
        };
        vertex.ensure_valid_wire()
    }

    #[inline]
    fn validate_log_head(log_head: i32) -> Result<(), VertexFieldError> {
        if log_head >= 0 && log_head >= DEFAULT_MAX_LOG_ENTRIES as i32 {
            return Err(VertexFieldError::OverflowLogHeadOutOfRange);
        }
        Ok(())
    }

    #[inline]
    fn ensure_valid_wire(self) -> Result<Self, VertexFieldError> {
        if !slot_index_fits(self.base_slot_start()) {
            return Err(VertexFieldError::SlotIndexOverflow);
        }
        let (log_head, _) = unpack_vertex_tail28(decode_meta28(self.locator));
        Self::validate_log_head(log_head)?;
        Ok(self)
    }
}

impl CsrVertex for Vertex {
    const BYTES: usize = 16;

    fn base_slot_start(&self) -> u64 {
        decode_slot_index(self.locator)
    }

    fn degree(&self) -> u32 {
        self.live_edges
    }

    fn stored_degree(&self) -> u32 {
        self.slab_slots
    }

    fn with_base_slot_start(self, start: u64) -> Self {
        self.try_with_base_slot_start(start)
            .expect("Vertex::with_base_slot_start: slot index overflow")
    }

    /// Sets both live and slab to `n` (packed row after rebalance or fresh materialization).
    fn with_degree(mut self, n: u32) -> Self {
        self.live_edges = n;
        self.slab_slots = n;
        self
    }

    fn after_slab_tombstone_delete(mut self) -> Self {
        self.live_edges = self.live_edges.saturating_sub(1);
        self
    }

    fn grow_packed_slab_by_one(mut self) -> Self {
        self.live_edges = self.live_edges.saturating_add(1);
        self.slab_slots = self.slab_slots.saturating_add(1);
        self
    }

    fn after_slab_insert_reuse_tail_tombstone(mut self) -> Self {
        debug_assert!(self.live_edges < self.slab_slots);
        self.live_edges = self.live_edges.saturating_add(1);
        self
    }

    fn log_head(self) -> i32 {
        unpack_vertex_tail28(decode_meta28(self.locator)).0
    }

    fn with_log_head(self, idx: i32) -> Self {
        self.try_with_log_head(idx)
            .expect("vertex overflow log head is invalid for packed LARA row")
    }
}

impl CsrVertexTombstone for Vertex {
    fn is_tombstone(&self) -> bool {
        self.deleted()
    }

    fn with_tombstone(self, tomb: bool) -> Self {
        self.with_tail28(pack_vertex_tail28(self.log_head(), tomb))
    }
}

fn vertex_row_bytes(v: &Vertex) -> [u8; Vertex::BYTES] {
    let mut b = [0u8; Vertex::BYTES];
    b[0..8].copy_from_slice(&v.locator.to_le_bytes());
    b[8..12].copy_from_slice(&v.live_edges.to_le_bytes());
    b[12..16].copy_from_slice(&v.slab_slots.to_le_bytes());
    b
}

impl Storable for Vertex {
    const BOUND: Bound = Bound::Bounded {
        max_size: Vertex::BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(vertex_row_bytes(self)))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(vertex_row_bytes(&self))
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        Self::try_read_from(bytes.as_ref()).expect("Vertex::from_bytes: invalid wire row")
    }
}

/// Stable vector storing fixed-width LARA vertex rows.
#[derive(Clone, Debug)]
pub struct VertexStore<V: CsrVertex, M: Memory> {
    memory: M,
    /// Mirrors the persisted header; [`Self::len`] hot path reads this instead of stable memory.
    header_mirror: Cell<HeaderV1>,
    _marker: std::marker::PhantomData<V>,
}

impl<V: CsrVertex, M: Memory> VertexStore<V, M> {
    /// Creates a fresh vertex store, overwriting any existing contents of `memory`.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        verify_vertex_width::<V>().expect("LARA vertices must be fixed-width");
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
            stride: V::BYTES as u32,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
            header_mirror: Cell::new(header),
            _marker: std::marker::PhantomData,
        })
    }

    /// Reopens an existing vertex store, or creates one if `memory` is empty.
    pub fn init(memory: M) -> Result<Self, InitError> {
        verify_vertex_width::<V>()?;
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let header = Self::read_header(&memory);
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        let expected_stride = V::BYTES as u32;
        if header.stride != expected_stride {
            return Err(InitError::StrideMismatch {
                expected: expected_stride,
                actual: header.stride,
            });
        }
        Ok(Self {
            memory,
            header_mirror: Cell::new(header),
            _marker: std::marker::PhantomData,
        })
    }

    /// Returns the number of vertex rows in the store.
    #[inline]
    pub fn len(&self) -> u32 {
        self.header_mirror.get().len
    }

    /// Returns `true` when the store contains no vertex rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Consumes the store and returns the underlying stable memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Reads the vertex row for `id`.
    ///
    /// Panics if `id >= self.len()`.
    pub fn get(&self, id: VertexId) -> V {
        let index = u64::from(id);
        assert!(index < u64::from(self.len()));
        if V::BYTES <= INLINE_VERTEX_ROW_BYTES {
            let mut buf = [0u8; INLINE_VERTEX_ROW_BYTES];
            self.memory
                .read(self.entry_offset(index), &mut buf[..V::BYTES]);
            V::from_bytes(Cow::Borrowed(&buf[..V::BYTES]))
        } else {
            let mut buf = vec![0u8; V::BYTES];
            self.memory.read(self.entry_offset(index), &mut buf);
            V::from_bytes(Cow::Owned(buf))
        }
    }

    /// Replaces the vertex row for `id`.
    ///
    /// Panics if `id >= self.len()`.
    pub(crate) fn set(&self, id: VertexId, item: &V) {
        let index = u64::from(id);
        assert!(index < u64::from(self.len()));
        crate::write(
            &self.memory,
            self.entry_offset(index),
            &item.to_bytes_checked(),
        );
    }

    /// Appends a vertex row and grows stable memory if necessary.
    pub(crate) fn push(&self, item: V) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len
            .checked_add(1)
            .expect("vertex store length exceeds u32::MAX");
        safe_write(
            &self.memory,
            self.entry_offset(u64::from(len)),
            &item.to_bytes_checked(),
        )?;
        write_u32(&self.memory, Address::from(LEN_OFFSET), new_len);
        let mut hdr = self.header_mirror.get();
        hdr.len = new_len;
        self.header_mirror.set(hdr);
        Ok(())
    }

    fn entry_offset(&self, index: u64) -> u64 {
        DATA_OFFSET + V::BYTES as u64 * index
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version]);
        write_u32(memory, Address::from(LEN_OFFSET), header.len);
        write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        debug_assert!(memory.size() > 0);

        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        let len = read_u32(memory, Address::from(LEN_OFFSET));
        let stride = read_u32(memory, Address::from(STRIDE_OFFSET));

        HeaderV1 {
            magic,
            version: version[0],
            len,
            stride,
        }
    }
}

fn verify_vertex_width<V: CsrVertex>() -> Result<(), InitError> {
    match V::BOUND {
        Bound::Bounded {
            max_size,
            is_fixed_size: true,
        } if max_size as usize == V::BYTES => Ok(()),
        _ => Err(InitError::VariableWidthVertex),
    }
}

#[cfg(test)]
mod tests {
    use super::{Vertex, VertexFieldError};
    use crate::{
        lara::edge::DEFAULT_MAX_LOG_ENTRIES,
        slab_index::{SLOT_INDEX_MASK, encode_locator_word, pack_vertex_tail28},
        traits::{CsrVertex, CsrVertexTombstone},
    };
    use ic_stable_structures::Storable;
    use std::borrow::Cow;
    use std::mem::size_of;

    #[test]
    fn vertex_row_is_16_bytes() {
        assert_eq!(size_of::<Vertex>(), 16);
        assert_eq!(Vertex::BYTES, 16);
    }

    #[test]
    fn vertex_default_has_no_overflow_log() {
        assert_eq!(Vertex::default().log_head(), -1);
    }

    #[test]
    fn vertex_storable_roundtrip_default_tail() {
        let v = Vertex::from_parts(
            0x0102_0304_0506_0708 & SLOT_INDEX_MASK,
            0x090a_0b0c,
            0x090a_0b0c,
            -1,
            false,
        );
        assert_eq!(Vertex::from_bytes(v.to_bytes()), v);
        assert_eq!(Vertex::from_bytes(Cow::Owned(v.into_bytes())), v);
    }

    #[test]
    fn vertex_golden_wire_bytes() {
        let v = Vertex::from_parts(0x0123_4567_89ab_cdef & SLOT_INDEX_MASK, 3, 5, 7, false);
        let bytes = v.into_bytes();
        assert_eq!(bytes.len(), 16);
        let tail = pack_vertex_tail28(7, false);
        let locator = (u64::from(tail) << 36) | (v.base_slot_start() & SLOT_INDEX_MASK);
        assert_eq!(u64::from_le_bytes(bytes[0..8].try_into().unwrap()), locator);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 5);
    }

    #[test]
    fn tombstone_bit_preserves_log_head() {
        let v = Vertex::from_parts(1, 2, 2, 42, false).with_tombstone(true);
        assert!(v.is_tombstone());
        assert_eq!(v.log_head(), 42);
        let back = Vertex::from_bytes(v.to_bytes());
        assert!(back.deleted());
        assert_eq!(back.log_head(), 42);
    }

    #[test]
    fn try_from_parts_rejects_slot_overflow() {
        assert_eq!(
            Vertex::try_from_parts(SLOT_INDEX_MASK + 1, 0, 0, -1, false),
            Err(VertexFieldError::SlotIndexOverflow)
        );
    }

    #[test]
    fn try_from_parts_rejects_overflow_log_head() {
        assert_eq!(
            Vertex::try_from_parts(0, 0, 0, DEFAULT_MAX_LOG_ENTRIES as i32, false),
            Err(VertexFieldError::OverflowLogHeadOutOfRange)
        );
    }

    #[test]
    #[should_panic(expected = "vertex overflow log head is invalid for packed LARA row")]
    fn oversized_log_head_panics() {
        let v = Vertex::default();
        let _ = v.with_log_head(DEFAULT_MAX_LOG_ENTRIES as i32);
    }

    #[test]
    fn try_read_from_rejects_wire_length_mismatch() {
        assert_eq!(
            Vertex::try_read_from(&[0u8; 15]),
            Err(VertexFieldError::WireLengthMismatch)
        );
    }

    #[test]
    fn try_read_from_rejects_overflow_log_on_wire() {
        let tail = pack_vertex_tail28(DEFAULT_MAX_LOG_ENTRIES as i32, false);
        let locator = encode_locator_word(0, tail);
        let mut bytes = [0u8; Vertex::BYTES];
        bytes[0..8].copy_from_slice(&locator.to_le_bytes());
        assert_eq!(
            Vertex::try_read_from(&bytes),
            Err(VertexFieldError::OverflowLogHeadOutOfRange)
        );
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{Vertex, VertexStore};
    use crate::{VertexId, bench as helper, traits::CsrVertex};

    fn populate_store(n: u64) -> VertexStore<Vertex, helper::BenchMemory> {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = VertexStore::new(memories.memory()).expect("vertex store");
        for i in 0..n {
            store
                .push(Vertex::from_parts(
                    i * 4,
                    (i % 8) as u32,
                    (i % 8) as u32,
                    -1,
                    false,
                ))
                .expect("push vertex");
        }
        store
    }

    /// Measures appending vertex rows to the stable vertex column. This guards
    /// the fixed-width row write path and length-header update cost.
    #[bench(raw)]
    fn bench_lara_vertex_push_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = VertexStore::new(memories.memory()).expect("vertex store");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_vertex_push");
            for i in 0..helper::MEDIUM_N {
                store
                    .push(Vertex::from_parts(black_box(i * 4), 0, 0, -1, false))
                    .expect("push vertex");
            }
        })
    }

    /// Measures random-ish vertex row reads followed by in-place updates. The
    /// intent is to catch regressions in row offset calculation and stable
    /// memory read/write overhead for update-side metadata.
    #[bench(raw)]
    fn bench_lara_vertex_get_set_1024() -> canbench_rs::BenchResult {
        let store = populate_store(helper::MEDIUM_N);
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_vertex_get_set");
            for i in 0..helper::MEDIUM_N {
                let idx = helper::splitmix64(i) % helper::MEDIUM_N;
                let id = VertexId::from(idx as u32);
                let v = store.get(id);
                store.set(id, &v.with_degree(black_box(v.live_edges.wrapping_add(1))));
            }
        })
    }
}
