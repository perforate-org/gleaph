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
//! bits 1–27 encode `(spill_run + 1)`, `0` = no spill run) — then `live_edges` and `slab_slots`.
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

#[cfg(any(test, feature = "canbench"))]
use super::operation_error::VertexAccess;
use crate::{
    GrowFailed, VertexId,
    lara::{
        operation_error::{LaraOperationError, VertexAccessError},
        reserved::{region_is_zero, write_zeroes},
    },
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
/// Declared reserved header bytes between the stride field and [`DATA_OFFSET`].
const RESERVED_OFFSET: u64 = 12;
const RESERVED_SIZE: usize = 52;
/// Stack buffer width for [`VertexStore::get`] when `V::BYTES` is small enough.
const INLINE_VERTEX_ROW_BYTES: usize = 64;

#[derive(Clone, Copy, Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u32,
    stride: u32,
}

/// Prepared existing-row data. Stable rows contain their encoded bytes and checked address;
/// explicit value-only accessors copy a validated value without a byte serializer.
pub(crate) struct PreparedVertexWrite<'a, V: CsrVertex>(VertexWriteTarget<'a, V>);

enum VertexWriteTarget<'a, V: CsrVertex> {
    Memory {
        memory: &'a dyn Memory,
        address: u64,
        bytes: Vec<u8>,
        row: std::marker::PhantomData<V>,
    },
    #[cfg(any(test, feature = "canbench"))]
    Value {
        owner: &'a dyn VertexAccess<V>,
        id: VertexId,
        item: V,
    },
}

impl<'a, V: CsrVertex> PreparedVertexWrite<'a, V> {
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn value(
        owner: &'a dyn VertexAccess<V>,
        id: VertexId,
        item: V,
    ) -> Result<Self, LaraOperationError> {
        owner.get_in_range(id)?;
        Ok(Self(VertexWriteTarget::Value { owner, id, item }))
    }

    pub(crate) fn write(&self) {
        match &self.0 {
            VertexWriteTarget::Memory {
                memory,
                address,
                bytes,
                ..
            } => memory.write(*address, bytes),
            #[cfg(any(test, feature = "canbench"))]
            VertexWriteTarget::Value { owner, id, item } => owner.set(*id, item),
        }
    }
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
    /// A declared header reserved byte is nonzero (foreign or corrupt layout).
    ReservedRegionNonZero,
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
            Self::ReservedRegionNonZero => {
                write!(f, "vertex header reserved region must be zero")
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
    /// Spill run id does not fit in the packed tail28 encoding.
    SpillRunOverflow,
}

impl fmt::Display for VertexFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WireLengthMismatch => write!(f, "vertex wire row must be exactly 16 bytes"),
            Self::SlotIndexOverflow => {
                write!(f, "vertex base_slot_start exceeds 36-bit slot index")
            }
            Self::SpillRunOverflow => {
                write!(f, "vertex spill run id does not fit in packed tail28")
            }
        }
    }
}

impl std::error::Error for VertexFieldError {}

/// Default fixed-width LARA vertex row (16 bytes on wire).
///
/// `live_edges` is the logical out-degree (clean scans, rebalance packing).
/// `slab_slots` is the stored backing width (prefix plus used spill rows). Tombstones retain
/// their slots until compaction; `CsrVertex::initialized_slab_slots` projects the prefix. The locator's tail28 carries the bucket's
/// spill run id (ADR 0097 §3), or `-1` when nothing has spilled.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vertex {
    locator: u64,
    /// Live edges counted by graph APIs ([`CsrVertex::degree`]).
    pub live_edges: u32,
    /// Stored backing width, including used spill rows ([`CsrVertex::stored_degree`]).
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
    ///
    /// `spill_run` is the bucket's spill run id (ADR 0097 §3), or `-1` when nothing has spilled.
    #[inline]
    pub fn from_parts(
        base_slot_start: u64,
        live_edges: u32,
        slab_slots: u32,
        spill_run: i32,
        deleted: bool,
    ) -> Self {
        Self::try_from_parts(base_slot_start, live_edges, slab_slots, spill_run, deleted)
            .expect("Vertex::from_parts: invalid fields")
    }

    /// Fallible constructor with release-safe range checks.
    #[inline]
    pub fn try_from_parts(
        base_slot_start: u64,
        live_edges: u32,
        slab_slots: u32,
        spill_run: i32,
        deleted: bool,
    ) -> Result<Self, VertexFieldError> {
        if !slot_index_fits(base_slot_start) {
            return Err(VertexFieldError::SlotIndexOverflow);
        }
        let tail =
            try_pack_vertex_tail28(spill_run, deleted).ok_or(VertexFieldError::SpillRunOverflow)?;
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

    /// This bucket's spill run id (ADR 0097 §3), or `-1` when nothing has spilled.
    ///
    /// This row's spill run id (ADR 0097 §3), or [`crate::traits::NO_SPILL_RUN`].
    #[inline]
    pub fn spill_run(self) -> u32 {
        match unpack_vertex_tail28(decode_meta28(self.locator)).0 {
            run if run < 0 => crate::traits::NO_SPILL_RUN,
            run => run as u32,
        }
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

    /// Returns a copy with a new spill run id (rejects ids that do not fit the 27-bit tail).
    #[inline]
    pub fn try_with_spill_run(self, run: u32) -> Result<Self, VertexFieldError> {
        let encoded = if run == crate::traits::NO_SPILL_RUN {
            -1
        } else {
            let run = i32::try_from(run).map_err(|_| VertexFieldError::SpillRunOverflow)?;
            if run > (1 << 27) - 2 {
                return Err(VertexFieldError::SpillRunOverflow);
            }
            run
        };
        let tail = try_pack_vertex_tail28(encoded, self.deleted())
            .ok_or(VertexFieldError::SpillRunOverflow)?;
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
    fn ensure_valid_wire(self) -> Result<Self, VertexFieldError> {
        if !slot_index_fits(self.base_slot_start()) {
            return Err(VertexFieldError::SlotIndexOverflow);
        }
        // The tail is a fixed-width 27-bit run id, so every decoded value is in range by
        // construction: there is no separate range to validate here (ADR 0097 §3).
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

    fn try_grow_packed_slab_by_one(mut self) -> Result<Self, ()> {
        self.live_edges = self.live_edges.checked_add(1).ok_or(())?;
        self.slab_slots = self.slab_slots.checked_add(1).ok_or(())?;
        Ok(self)
    }

    fn grow_packed_slab_by_one(self) -> Self {
        self.try_grow_packed_slab_by_one()
            .expect("Vertex::grow_packed_slab_by_one: row width overflow")
    }

    fn after_slab_insert_reuse_tail_tombstone(mut self) -> Self {
        debug_assert!(self.live_edges < self.slab_slots);
        self.live_edges = self.live_edges.saturating_add(1);
        self
    }

    fn spill_run(self) -> u32 {
        Vertex::spill_run(self)
    }

    fn try_with_spill_run(self, run: u32) -> Result<Self, ()> {
        Vertex::try_with_spill_run(self, run).map_err(|_| ())
    }
}

impl CsrVertexTombstone for Vertex {
    fn is_tombstone(&self) -> bool {
        self.deleted()
    }

    fn with_tombstone(self, tomb: bool) -> Self {
        // Only the tombstone bit changes: the tail's run field is re-packed from its own encoding
        // (`unpack_vertex_tail28(..).0`, where a negative value means "no run"), so no run id is
        // re-derived, cast or truncated.
        let run_encoding = unpack_vertex_tail28(decode_meta28(self.locator)).0;
        self.with_tail28(pack_vertex_tail28(run_encoding, tomb))
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
        // Reserved-region guard: nonzero reserved bytes mean a foreign or corrupt
        // layout; fail closed with the dedicated variant.
        if !region_is_zero(&memory, RESERVED_OFFSET, RESERVED_SIZE) {
            return Err(InitError::ReservedRegionNonZero);
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
    #[inline]
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
    #[inline]
    pub(crate) fn set(&self, id: VertexId, item: &V) {
        let index = u64::from(id);
        assert!(index < u64::from(self.len()));
        crate::write(
            &self.memory,
            self.entry_offset(index),
            &item.to_bytes_checked(),
        );
    }

    /// Prepare bytes and backing for an existing row, without changing it.
    pub(crate) fn prepare_set(
        &self,
        id: VertexId,
        item: &V,
    ) -> Result<PreparedVertexWrite<'_, V>, LaraOperationError> {
        let index = u64::from(id);
        if index >= u64::from(self.len()) {
            return Err(VertexAccessError::OutOfRange.into());
        }
        let address = self.entry_offset(index);
        let bytes = item.to_bytes_checked().into_owned();
        let end = address
            .checked_add(bytes.len() as u64)
            .ok_or(LaraOperationError::overflow())?;
        let backed = self
            .memory
            .size()
            .checked_mul(crate::WASM_PAGE_SIZE)
            .ok_or(LaraOperationError::overflow())?;
        if end > backed {
            return Err(LaraOperationError::CollectAllocationOverflow);
        }
        Ok(PreparedVertexWrite(VertexWriteTarget::Memory {
            memory: &self.memory,
            address,
            bytes,
            row: std::marker::PhantomData,
        }))
    }

    /// Replaces a contiguous run of existing vertex rows with one stable-memory write.
    pub(crate) fn set_many(&self, start: VertexId, items: impl IntoIterator<Item = V>) {
        let items: Vec<V> = items.into_iter().collect();
        let start_index = u64::from(start);
        let end_index = start_index
            .checked_add(items.len() as u64)
            .expect("vertex row range overflow");
        assert!(end_index <= u64::from(self.len()));
        if items.is_empty() {
            return;
        }
        let byte_len = items
            .len()
            .checked_mul(V::BYTES)
            .expect("vertex row byte length overflow");
        let mut bytes = Vec::with_capacity(byte_len);
        for item in items {
            bytes.extend_from_slice(&item.to_bytes_checked());
        }
        crate::write(&self.memory, self.entry_offset(start_index), &bytes);
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

    /// Append several fixed-width rows with one contiguous stable-memory write and one length
    /// header update. The caller remains responsible for any paired orientation bookkeeping.
    pub(crate) fn push_many(&self, items: impl IntoIterator<Item = V>) -> Result<u32, GrowFailed> {
        let items: Vec<V> = items.into_iter().collect();
        let old_len = self.len();
        let added = u32::try_from(items.len()).expect("vertex batch length exceeds u32::MAX");
        let new_len = old_len
            .checked_add(added)
            .expect("vertex store length exceeds u32::MAX");
        if added == 0 {
            return Ok(old_len);
        }

        let byte_len = items
            .len()
            .checked_mul(V::BYTES)
            .expect("vertex batch byte length overflow");
        let mut bytes = Vec::with_capacity(byte_len);
        for item in items {
            bytes.extend_from_slice(&item.to_bytes_checked());
        }
        safe_write(&self.memory, self.entry_offset(u64::from(old_len)), &bytes)?;
        write_u32(&self.memory, Address::from(LEN_OFFSET), new_len);
        let mut hdr = self.header_mirror.get();
        hdr.len = new_len;
        self.header_mirror.set(hdr);
        Ok(old_len)
    }

    fn entry_offset(&self, index: u64) -> u64 {
        DATA_OFFSET + V::BYTES as u64 * index
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version]);
        write_u32(memory, Address::from(LEN_OFFSET), header.len);
        write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        write_zeroes(memory, RESERVED_OFFSET, RESERVED_SIZE)?;
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
    use super::{InitError, RESERVED_OFFSET, RESERVED_SIZE, Vertex, VertexFieldError, VertexStore};
    use crate::VectorMemory;
    use crate::{
        slab_index::{
            SLOT_INDEX_MASK, VERTEX_TAIL_RUN_MASK, encode_locator_word, pack_vertex_tail28,
        },
        traits::CsrVertexTombstone,
    };
    use ic_stable_structures::{Memory, Storable};
    use std::borrow::Cow;
    use std::mem::size_of;

    #[test]
    fn vertex_row_is_16_bytes() {
        assert_eq!(size_of::<Vertex>(), 16);
        assert_eq!(Vertex::BYTES, 16);
    }

    #[test]
    fn set_many_replaces_contiguous_rows() {
        let store = VertexStore::new(VectorMemory::default()).expect("vertex store");
        for index in 0..4 {
            store
                .push(Vertex::from_parts(index, 0, 0, -1, false))
                .expect("vertex row");
        }

        let replacement = [
            Vertex::from_parts(10, 1, 2, -1, false),
            Vertex::from_parts(11, 3, 4, 7, true),
        ];
        store.set_many(1.into(), replacement);

        assert_eq!(store.get(0.into()).base_slot_start(), 0);
        assert_eq!(store.get(1.into()), replacement[0]);
        assert_eq!(store.get(2.into()), replacement[1]);
        assert_eq!(store.get(3.into()).base_slot_start(), 3);
    }

    #[test]
    fn vertex_default_has_no_spill_run() {
        assert_eq!(Vertex::default().spill_run(), crate::traits::NO_SPILL_RUN);
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
    fn tombstone_bit_preserves_spill_run() {
        let v = Vertex::from_parts(1, 2, 2, 42, false).with_tombstone(true);
        assert!(v.is_tombstone());
        assert_eq!(v.spill_run(), 42);
        let back = Vertex::from_bytes(v.to_bytes());
        assert!(back.deleted());
        assert_eq!(back.spill_run(), 42);
    }

    #[test]
    fn try_from_parts_rejects_slot_overflow() {
        assert_eq!(
            Vertex::try_from_parts(SLOT_INDEX_MASK + 1, 0, 0, -1, false),
            Err(VertexFieldError::SlotIndexOverflow)
        );
    }

    #[test]
    fn try_from_parts_rejects_spill_run_overflow() {
        // The tail28 field is a full 27-bit run id (ADR 0097 §3), so the old 170-entry log cap is
        // gone: 170 is a valid run id now, and only a value that cannot be encoded is rejected.
        // The tail stores `run + 1`, so the largest encodable id is one below the all-ones mask.
        let max_run = VERTEX_TAIL_RUN_MASK as i32 - 1;
        assert!(Vertex::try_from_parts(0, 0, 0, 170, false).is_ok());
        assert!(Vertex::try_from_parts(0, 0, 0, max_run, false).is_ok());
        assert_eq!(
            Vertex::try_from_parts(0, 0, 0, max_run + 1, false),
            Err(VertexFieldError::SpillRunOverflow)
        );
    }

    #[test]
    fn oversized_spill_run_is_rejected() {
        // Re-derived with the typed setter: the retired i32 log-head setter panicked on an
        // unrepresentable id, and the spill API's `try_with_spill_run` returns `Err` instead -
        // a typed rejection rather than a panic (ADR 0097 §3).
        let v = Vertex::default();
        assert!(v.try_with_spill_run(VERTEX_TAIL_RUN_MASK).is_err());
    }

    #[test]
    fn try_read_from_rejects_wire_length_mismatch() {
        assert_eq!(
            Vertex::try_read_from(&[0u8; 15]),
            Err(VertexFieldError::WireLengthMismatch)
        );
    }

    #[test]
    fn try_read_from_accepts_the_full_spill_run_range() {
        // A 27-bit field cannot hold an out-of-range id, so the wire check that remains is the
        // round trip: the retired 170-entry log cap no longer rejects a wire row, and the largest
        // encodable run id (`mask - 1`, because the tail stores `run + 1`) survives encode/decode.
        for run in [170u32, VERTEX_TAIL_RUN_MASK - 1] {
            let tail = pack_vertex_tail28(run as i32, false);
            let locator = encode_locator_word(0, tail);
            let mut bytes = [0u8; Vertex::BYTES];
            bytes[0..8].copy_from_slice(&locator.to_le_bytes());
            assert_eq!(
                Vertex::try_read_from(&bytes).expect("wire row").spill_run(),
                run
            );
        }
    }

    #[test]
    fn header_reserved_region_is_zeroed_and_reopens() {
        let memory = VectorMemory::default();
        let store = VertexStore::new(memory.clone()).expect("vertex store");
        store
            .push(Vertex::from_parts(7, 1, 1, -1, false))
            .expect("vertex row");
        drop(store);

        let mut reserved = [0u8; RESERVED_SIZE];
        memory.read(RESERVED_OFFSET, &mut reserved);
        assert!(reserved.iter().all(|&byte| byte == 0));

        let reopened = VertexStore::<Vertex, VectorMemory>::init(memory).expect("reopen");
        assert_eq!(reopened.len(), 1);
        assert_eq!(
            reopened.get(0.into()),
            Vertex::from_parts(7, 1, 1, -1, false)
        );
    }

    #[test]
    fn init_rejects_nonzero_reserved_byte() {
        let memory = VectorMemory::default();
        let store = VertexStore::<Vertex, VectorMemory>::new(memory.clone()).expect("vertex store");
        drop(store);

        memory.write(RESERVED_OFFSET, &[1]);
        assert!(matches!(
            VertexStore::<Vertex, VectorMemory>::init(memory),
            Err(InitError::ReservedRegionNonZero)
        ));
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
    fn bench_r_vx_p_1024() -> canbench_rs::BenchResult {
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
    fn bench_r_vx_gs_1024() -> canbench_rs::BenchResult {
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
