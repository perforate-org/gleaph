//! Stable LARA CSR edge slab (`edges_`) plus graph-wide edge metadata.
//!
//! # V1 layout
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "LEG"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Element capacity                      ↕ 8 bytes
//! --------------------------------------------------
//! Number of leaf segments               ↕ 4 bytes
//! --------------------------------------------------
//! Segment size in vertices              ↕ 4 bytes
//! --------------------------------------------------
//! PMA tree height                       ↕ 4 bytes
//! --------------------------------------------------
//! Number of logical edges               ↕ 8 bytes
//! --------------------------------------------------
//! Edge slot stride                      ↕ 4 bytes
//! --------------------------------------------------
//! Slab occupied tail                    ↕ 8 bytes
//! --------------------------------------------------
//! Initial vertex edge slots             ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 16 bytes
//! -------------------------------------------------- <- Address 64
//! E_0                                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! E_1                                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! E_(elem_capacity-1)                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! Unallocated space
//! ```

use crate::{
    GrowFailed, VertexCount, lara::reserved::region_is_zero, safe_write, traits::CsrEdge,
    types::Address, write_u64,
};
use ic_stable_structures::Memory;
use std::{cell::RefCell, fmt, marker::PhantomData};

/// Magic bytes that identify a LARA edge slab memory.
pub const MAGIC: [u8; 3] = *b"LEG";
/// Current edge slab layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Size of the persisted edge slab header in bytes.
pub const HEADER_SIZE: u64 = 64;

const ELEM_CAPACITY_OFFSET: u64 = 4;
const SEGMENT_COUNT_OFFSET: u64 = 12;
const SEGMENT_SIZE_OFFSET: u64 = 16;
const TREE_HEIGHT_OFFSET: u64 = 20;
const NUM_EDGES_OFFSET: u64 = 24;
const EDGE_STRIDE_OFFSET: u64 = 32;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 36;
const INITIAL_VERTEX_EDGE_SLOTS_OFFSET: u64 = 44;
const RESERVED_OFFSET: u64 = 48;
const RESERVED_SIZE: usize = 16;
/// Extra physical pages reserved when the edge slab crosses a stable-memory page boundary.
/// Logical `elem_capacity` remains exact; the reserve only amortizes repeated `Memory::grow`
/// calls during relocation-heavy workloads.
const EDGE_SLAB_GROW_RESERVE_PAGES: u64 = 4;

/// Persisted V1 edge slab header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    /// Magic bytes, always `LEG` for this layout.
    pub magic: [u8; 3],
    /// Layout version for this header.
    pub version: u8,
    /// Number of edge slots allocated in the slab.
    pub elem_capacity: u64,
    /// Number of leaf segments in the PMA segment tree.
    pub segment_count: u32,
    /// Number of vertices covered by one leaf segment.
    pub segment_size: u32,
    /// Height of the PMA segment tree.
    pub tree_height: u32,
    /// Number of logical edges stored by the graph.
    pub num_edges: u64,
    /// Encoded byte width of one edge record.
    pub stride: u32,
    /// Highest occupied slab slot boundary used by tail allocation.
    pub slab_occupied_tail: u64,
    /// When non-zero, new vertex batches in a leaf may allocate `n_L * this`
    /// slab slots eagerly. Zero preserves legacy implicit packing.
    pub initial_vertex_edge_slots: u32,
}

impl HeaderV1 {
    /// Builds a fresh V1 header for a new edge slab.
    pub fn new(
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
        stride: u32,
        initial_vertex_edge_slots: u32,
    ) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            elem_capacity,
            segment_count,
            segment_size,
            tree_height: tree_height_for_segment_count(segment_count),
            num_edges: 0,
            stride,
            slab_occupied_tail: 0,
            initial_vertex_edge_slots,
        }
    }

    fn encode(self) -> [u8; HEADER_SIZE as usize] {
        let mut bytes = [0; HEADER_SIZE as usize];
        bytes[..3].copy_from_slice(&self.magic);
        bytes[3] = self.version;
        for (offset, value) in [
            (ELEM_CAPACITY_OFFSET, self.elem_capacity),
            (NUM_EDGES_OFFSET, self.num_edges),
            (SLAB_OCCUPIED_TAIL_OFFSET, self.slab_occupied_tail),
        ] {
            bytes[offset as usize..offset as usize + 8].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (SEGMENT_COUNT_OFFSET, self.segment_count),
            (SEGMENT_SIZE_OFFSET, self.segment_size),
            (TREE_HEIGHT_OFFSET, self.tree_height),
            (EDGE_STRIDE_OFFSET, self.stride),
            (
                INITIAL_VERTEX_EDGE_SLOTS_OFFSET,
                self.initial_vertex_edge_slots,
            ),
        ] {
            bytes[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

/// Errors returned when reopening a persisted edge slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The memory header does not contain the LARA edge magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The memory is empty or does not contain a valid slab header.
    InvalidLayout,
    /// The store could not allocate its metadata.
    OutOfMemory,
    /// The persisted edge width does not match the edge type `E`.
    StrideMismatch {
        /// Expected edge record width.
        expected: u32,
        /// Edge record width read from stable memory.
        actual: u32,
    },
    /// Persisted [`HeaderV1::elem_capacity`] exceeds the 36-bit slab index space.
    ElemCapacityOverflow,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad edge magic {actual:?}, expected {MAGIC:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported edge layout version {v}"),
            Self::InvalidLayout => write!(f, "invalid edge slab layout"),
            Self::OutOfMemory => write!(f, "failed to allocate edge slab metadata"),
            Self::StrideMismatch { expected, actual } => {
                write!(f, "edge stride mismatch: expected {expected}, got {actual}")
            }
            Self::ElemCapacityOverflow => {
                write!(f, "edge slab elem_capacity exceeds 36-bit slot index space")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable storage for raw fixed-width edge slots.
#[derive(Clone, Debug)]
pub struct EdgeSlabStore<E: CsrEdge, M: Memory> {
    memory: M,
    _marker: PhantomData<E>,
}

/// Resident bytes or a proved-disjoint raw source. Addresses are slab slots.
pub(crate) enum PhysicalSlabPayload {
    Buffered {
        source: Option<u64>,
        destination: u64,
        bytes: Vec<u8>,
    },
    Disjoint {
        source: u64,
        destination: u64,
        slots: u64,
    },
}

struct BackedSlabRange {
    destination: u64,
    source: Option<u64>,
    bytes: Vec<u8>,
    streamed_bytes: u64,
}

/// Checked, encoded slab image; writing it cannot grow or encode.
pub(crate) struct BackedSlabImage<'a, E: CsrEdge, M: Memory> {
    owner: &'a EdgeSlabStore<E, M>,
    header: [u8; HEADER_SIZE as usize],
    ranges: Vec<BackedSlabRange>,
    scratch: RefCell<Vec<u8>>,
}

impl<E: CsrEdge, M: Memory> BackedSlabImage<'_, E, M> {
    pub(crate) fn validate_source(&self) -> bool {
        let mut scratch = [0u8; 128];
        for range in &self.ranges {
            let Some(source) = range.source else { continue };
            // Streamed sources remain outside every destination; their row/header
            // witnesses are revalidated by the enclosing physical preparation.
            for (index, bytes) in range.bytes.chunks(scratch.len()).enumerate() {
                let offset = source + (index * scratch.len()) as u64;
                self.owner.memory.read(offset, &mut scratch[..bytes.len()]);
                if scratch[..bytes.len()] != *bytes {
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn write_payloads(&self) {
        let mut scratch = self.scratch.borrow_mut();
        for range in &self.ranges {
            if range.streamed_bytes == 0 {
                if !range.bytes.is_empty() {
                    self.owner.memory.write(range.destination, &range.bytes);
                }
                continue;
            }
            let source = range.source.expect("admitted streamed source");
            let mut copied = 0u64;
            while copied < range.streamed_bytes {
                let n = (range.streamed_bytes - copied).min(scratch.len() as u64) as usize;
                self.owner.memory.read(source + copied, &mut scratch[..n]);
                self.owner
                    .memory
                    .write(range.destination + copied, &scratch[..n]);
                copied += n as u64;
            }
        }
    }

    pub(crate) fn write(&self) {
        self.write_payloads();
        self.owner.memory.write(0, &self.header);
    }

    #[cfg(test)]
    pub(crate) fn copy_buffer_metrics(&self) -> (usize, usize) {
        (self.retained_source_bytes(), self.scratch.borrow().len())
    }

    #[cfg(test)]
    pub(crate) fn retained_source_bytes(&self) -> usize {
        self.ranges
            .iter()
            .filter(|range| range.source.is_some())
            .map(|range| range.bytes.len())
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn transcribed_bytes(&self) -> usize {
        self.ranges
            .iter()
            .filter(|range| range.source.is_none())
            .map(|range| range.bytes.len())
            .sum()
    }
}

impl<E: CsrEdge, M: Memory> EdgeSlabStore<E, M> {
    /// Creates a fresh edge slab with `header`.
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        store.grow_for_header(&header)?;
        store.write_header(&header);
        Ok(store)
    }

    /// Reopens an existing edge slab from stable memory.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::InvalidLayout);
        }
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        let header = store.read_header()?;
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if header.stride as usize != E::BYTES {
            return Err(InitError::StrideMismatch {
                expected: E::BYTES as u32,
                actual: header.stride,
            });
        }
        // Reserved-region guard: nonzero reserved bytes mean a foreign or corrupt
        // slab layout that must not open.
        if !region_is_zero(&store.memory, RESERVED_OFFSET, RESERVED_SIZE) {
            return Err(InitError::InvalidLayout);
        }
        if !crate::slab_index::validate_elem_capacity(header.elem_capacity).is_ok() {
            return Err(InitError::ElemCapacityOverflow);
        }
        Ok(store)
    }

    /// Consumes the store and returns the underlying memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Reads the current persisted slab header.
    pub fn header(&self) -> Result<HeaderV1, InitError> {
        self.read_header()
    }

    /// Writes the full slab header, including zero reserved bytes, to backed memory.
    pub(crate) fn write_header(&self, h: &HeaderV1) {
        self.memory.write(0, &h.encode());
    }

    /// Admit every buffered destination and one header before publishing any range.
    pub(crate) fn prepare_physical_image(
        &self,
        header: HeaderV1,
        ranges: Vec<(u64, Vec<u8>)>,
    ) -> Result<BackedSlabImage<'_, E, M>, GrowFailed> {
        let mut payloads = Vec::new();
        payloads
            .try_reserve_exact(ranges.len())
            .map_err(|_| self.image_error())?;
        for (destination, bytes) in ranges {
            payloads.push(PhysicalSlabPayload::Buffered {
                source: None,
                destination,
                bytes,
            });
        }
        self.prepare_physical_copy_image(header, payloads)
    }

    fn image_error(&self) -> GrowFailed {
        GrowFailed {
            current_size: self.memory.size(),
            delta: 0,
        }
    }

    fn image_extent(&self, header: HeaderV1) -> Result<u64, GrowFailed> {
        if E::BYTES == 0
            || header.stride as usize != E::BYTES
            || header.magic != MAGIC
            || header.version != LAYOUT_VERSION
            || !crate::slab_index::slot_exclusive_end_fits(header.elem_capacity)
        {
            return Err(self.image_error());
        }
        header
            .elem_capacity
            .checked_mul(E::BYTES as u64)
            .and_then(|n| n.checked_add(HEADER_SIZE))
            .ok_or_else(|| self.image_error())
    }

    fn image_range(
        &self,
        header: HeaderV1,
        start: u64,
        slots: u64,
    ) -> Result<(u64, u64), GrowFailed> {
        let end = crate::slab_index::checked_add_slot_exclusive_end(start, slots)
            .filter(|end| *end <= header.elem_capacity)
            .ok_or_else(|| self.image_error())?;
        let offset = start
            .checked_mul(E::BYTES as u64)
            .and_then(|n| n.checked_add(HEADER_SIZE))
            .ok_or_else(|| self.image_error())?;
        let end = end
            .checked_mul(E::BYTES as u64)
            .and_then(|n| n.checked_add(HEADER_SIZE))
            .ok_or_else(|| self.image_error())?;
        Ok((offset, end - offset))
    }

    /// Source and destination offsets, backing and maximum-chunk scratch have one owner.
    pub(crate) fn prepare_physical_copy_image(
        &self,
        header: HeaderV1,
        payloads: Vec<PhysicalSlabPayload>,
    ) -> Result<BackedSlabImage<'_, E, M>, GrowFailed> {
        let extent = self.image_extent(header)?;
        let source_header = self.header().map_err(|_| self.image_error())?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(payloads.len())
            .map_err(|_| self.image_error())?;
        let mut max_stream = 0u64;
        for payload in payloads {
            let (destination, source, bytes, streamed_bytes) = match payload {
                PhysicalSlabPayload::Buffered {
                    source,
                    destination,
                    bytes,
                } => {
                    if !bytes.len().is_multiple_of(E::BYTES) {
                        return Err(self.image_error());
                    }
                    let slots =
                        u64::try_from(bytes.len() / E::BYTES).map_err(|_| self.image_error())?;
                    let (destination, _) = self.image_range(header, destination, slots)?;
                    let source = source
                        .map(|source| {
                            self.image_range(source_header, source, slots)
                                .map(|(offset, _)| offset)
                        })
                        .transpose()?;
                    (destination, source, bytes, 0)
                }
                PhysicalSlabPayload::Disjoint {
                    source,
                    destination,
                    slots,
                } => {
                    let (source, len) = self.image_range(source_header, source, slots)?;
                    let (destination, _) = self.image_range(header, destination, slots)?;
                    if len > 0 && source < destination + len && destination < source + len {
                        return Err(self.image_error());
                    }
                    max_stream = max_stream.max(len);
                    (destination, Some(source), Vec::new(), len)
                }
            };
            ranges.push(BackedSlabRange {
                destination,
                source,
                bytes,
                streamed_bytes,
            });
        }
        // Disjointness is global, not merely each range's own source/destination pair.
        Self::validate_stream_disjointness(&ranges).map_err(|_| self.image_error())?;
        let chunk = max_stream.min(
            (super::OUT_EDGE_SLAB_CHUNK_SLOTS as usize)
                .checked_mul(E::BYTES)
                .ok_or_else(|| self.image_error())? as u64,
        );
        let chunk = usize::try_from(chunk).map_err(|_| self.image_error())?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(chunk)
            .map_err(|_| self.image_error())?;
        scratch.resize(chunk, 0);
        self.grow_for_header(&header)?;
        let backed = self
            .memory
            .size()
            .checked_mul(crate::WASM_PAGE_SIZE)
            .ok_or_else(|| self.image_error())?;
        if extent > backed {
            return Err(self.image_error());
        }
        Ok(BackedSlabImage {
            owner: self,
            header: header.encode(),
            ranges,
            scratch: RefCell::new(scratch),
        })
    }

    fn validate_stream_disjointness(ranges: &[BackedSlabRange]) -> Result<(), ()> {
        for source in ranges.iter().filter(|range| range.streamed_bytes > 0) {
            let start = source.source.ok_or(())?;
            let end = start.checked_add(source.streamed_bytes).ok_or(())?;
            for destination in ranges {
                let len = if destination.streamed_bytes > 0 {
                    destination.streamed_bytes
                } else {
                    destination.bytes.len() as u64
                };
                if len > 0
                    && start < destination.destination.checked_add(len).ok_or(())?
                    && destination.destination < end
                {
                    return Err(());
                }
            }
        }
        Ok(())
    }

    /// Combine already-admitted components without retaining a header per participant.
    pub(crate) fn combine_physical_images<'a>(
        &'a self,
        header: HeaderV1,
        images: Vec<BackedSlabImage<'a, E, M>>,
    ) -> Result<BackedSlabImage<'a, E, M>, GrowFailed> {
        let extent = self.image_extent(header)?;
        let encoded = header.encode();
        let count = images.iter().try_fold(0usize, |n, image| {
            if !std::ptr::eq(image.owner, self) || image.header != encoded {
                return Err(self.image_error());
            }
            n.checked_add(image.ranges.len())
                .ok_or_else(|| self.image_error())
        })?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(count)
            .map_err(|_| self.image_error())?;
        let mut scratch = Vec::new();
        for image in images {
            ranges.extend(image.ranges);
            let candidate = image.scratch.into_inner();
            if candidate.len() > scratch.len() {
                scratch = candidate;
            }
        }
        Self::validate_stream_disjointness(&ranges).map_err(|_| self.image_error())?;
        self.grow_for_header(&header)?;
        if self
            .memory
            .size()
            .checked_mul(crate::WASM_PAGE_SIZE)
            .is_none_or(|backed| extent > backed)
        {
            return Err(self.image_error());
        }
        Ok(BackedSlabImage {
            owner: self,
            header: encoded,
            ranges,
            scratch: RefCell::new(scratch),
        })
    }

    /// Updates the logical edge count field in the header.
    pub(crate) fn set_num_edges(&self, n: u64) {
        write_u64(&self.memory, Address::from(NUM_EDGES_OFFSET), n);
    }

    /// Updates the slab capacity and grows stable memory if needed.
    pub(crate) fn set_elem_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        if !crate::slab_index::slot_exclusive_end_fits(n) {
            return Err(GrowFailed {
                current_size: self.memory.size(),
                delta: 0,
            });
        }
        let mut h = self.header().map_err(|_| GrowFailed {
            current_size: self.memory.size(),
            delta: 0,
        })?;
        h.elem_capacity = n;
        self.grow_for_header(&h)?;
        write_u64(&self.memory, Address::from(ELEM_CAPACITY_OFFSET), n);
        Ok(())
    }

    /// Reads the raw bytes for `slot` into `out`.
    #[inline]
    pub fn read_slot(&self, slot: u64, out: &mut [u8]) {
        self.memory.read(slot_offset::<E>(slot), out);
    }

    /// Reads `out.len()` bytes starting `byte_offset` bytes past the start of `slot`.
    ///
    /// The tree-mode root regions pack `u32` block ids at 4 bytes whatever the edge
    /// record width is, so a packed id can straddle an edge-record boundary when
    /// `E::BYTES` is not a multiple of 4. This accessor keeps the address anchored to
    /// a slot - callers never handle absolute memory offsets.
    #[inline]
    pub(crate) fn read_from_slot_bytes(&self, slot: u64, byte_offset: u64, out: &mut [u8]) {
        self.memory
            .read(slot_offset::<E>(slot).saturating_add(byte_offset), out);
    }

    /// Writes `src` starting `byte_offset` bytes past the start of `slot`.
    /// See [`Self::read_from_slot_bytes`] for why the address is slot-anchored.
    #[inline]
    pub(crate) fn write_from_slot_bytes(
        &self,
        slot: u64,
        byte_offset: u64,
        src: &[u8],
    ) -> Result<(), GrowFailed> {
        safe_write(
            &self.memory,
            slot_offset::<E>(slot).saturating_add(byte_offset),
            src,
        )
    }

    /// Reads `count` contiguous slots starting at `start_slot` into `out`.
    ///
    /// `out.len()` must equal `count * E::BYTES` for some `count >= 0`.
    #[inline]
    pub(crate) fn read_slots_contiguous(&self, start_slot: u64, out: &mut [u8]) {
        debug_assert_eq!(out.len() % E::BYTES, 0);
        if out.is_empty() {
            return;
        }
        self.memory.read(slot_offset::<E>(start_slot), out);
    }

    /// Writes raw bytes for contiguous slots starting at `start_slot`.
    ///
    /// `bytes.len()` must be a multiple of `E::BYTES`.
    pub(crate) fn write_slots_contiguous(
        &self,
        start_slot: u64,
        bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        debug_assert_eq!(bytes.len() % E::BYTES, 0);
        if bytes.is_empty() {
            return Ok(());
        }
        safe_write(&self.memory, slot_offset::<E>(start_slot), bytes)
    }

    /// Writes raw encoded edge bytes to `slot`.
    #[inline]
    pub(crate) fn write_slot(&self, slot: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        debug_assert_eq!(bytes.len(), E::BYTES);
        safe_write(&self.memory, slot_offset::<E>(slot), bytes)
    }

    /// Writes a byte range within one existing slot without re-encoding the full row.
    #[inline]
    pub(crate) fn write_slot_range(
        &self,
        slot: u64,
        byte_offset: usize,
        bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        debug_assert!(byte_offset.saturating_add(bytes.len()) <= E::BYTES);
        safe_write(
            &self.memory,
            slot_offset::<E>(slot).saturating_add(byte_offset as u64),
            bytes,
        )
    }

    fn read_header(&self) -> Result<HeaderV1, InitError> {
        // A constructed store owns at least one page. Read the same current
        // metadata in one bounded operation; validation and reserved checks
        // retain their existing owners and error order.
        let mut bytes = [0u8; RESERVED_OFFSET as usize];
        self.memory.read(0, &mut bytes);
        let magic = bytes[..3].try_into().expect("fixed magic width");
        if magic != MAGIC {
            return Err(InitError::BadMagic { actual: magic });
        }
        let version = bytes[3];
        if version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(version));
        }
        let u32_at = |offset: u64| {
            let start = offset as usize;
            u32::from_le_bytes(
                bytes[start..start + 4]
                    .try_into()
                    .expect("fixed header field"),
            )
        };
        let u64_at = |offset: u64| {
            let start = offset as usize;
            u64::from_le_bytes(
                bytes[start..start + 8]
                    .try_into()
                    .expect("fixed header field"),
            )
        };
        Ok(HeaderV1 {
            magic,
            version,
            elem_capacity: u64_at(ELEM_CAPACITY_OFFSET),
            segment_count: u32_at(SEGMENT_COUNT_OFFSET),
            segment_size: u32_at(SEGMENT_SIZE_OFFSET),
            tree_height: u32_at(TREE_HEIGHT_OFFSET),
            num_edges: u64_at(NUM_EDGES_OFFSET),
            stride: u32_at(EDGE_STRIDE_OFFSET),
            slab_occupied_tail: u64_at(SLAB_OCCUPIED_TAIL_OFFSET),
            initial_vertex_edge_slots: u32_at(INITIAL_VERTEX_EDGE_SLOTS_OFFSET),
        })
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = HEADER_SIZE + h.elem_capacity.saturating_mul(E::BYTES as u64);
        if need == 0 {
            return Ok(());
        }
        let required_pages = need.div_ceil(crate::WASM_PAGE_SIZE);
        if self.memory.size() < required_pages {
            let reserved_pages = required_pages.saturating_add(EDGE_SLAB_GROW_RESERVE_PAGES);
            let reserved_last_byte = reserved_pages
                .saturating_mul(crate::WASM_PAGE_SIZE)
                .saturating_sub(1);
            safe_write(&self.memory, reserved_last_byte, &[0])
        } else {
            Ok(())
        }
    }
}

/// Returns the byte offset of `slot` in an edge slab for edge type `E`.
#[inline]
pub fn slot_offset<E: CsrEdge>(slot: u64) -> u64 {
    HEADER_SIZE + slot.saturating_mul(E::BYTES as u64)
}

#[inline]
fn floor_log2(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// PMA [`HeaderV1::tree_height`] for a given leaf segment count (at least one leaf).
#[inline]
pub(crate) fn tree_height_for_segment_count(segment_count: u32) -> u32 {
    floor_log2(segment_count.max(1))
}

/// Minimum power-of-two leaf count for a vertex column (`0` vertices ⇒ one leaf).
///
/// Saturates to `u32::MAX` if the logical leaf need does not fit in a power of two
/// (extreme inputs only). `vertex_len` is the vertex column row count (for example
/// from [`VertexCount`] or [`crate::lara::vertex::VertexStore::len`]).
#[inline]
pub fn segment_tree_leaf_count(vertex_len: VertexCount, segment_size: u32) -> u32 {
    let vertex_len = u32::from(vertex_len);
    let sz = segment_size.max(1);
    let need = if vertex_len == 0 {
        1u32
    } else {
        vertex_len.div_ceil(sz)
    };
    need.max(1).checked_next_power_of_two().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{EdgeSlabStore, HeaderV1, InitError, RESERVED_OFFSET, RESERVED_SIZE};
    use crate::{
        slab_index::MAX_SLOT_EXCLUSIVE_END,
        test_support::{TestEdge, vector_memory},
        traits::CsrEdge,
        types::Address,
        write_u64,
    };
    use ic_stable_structures::Memory;

    #[test]
    fn edge_slab_header_reads_current_fields_and_preserves_error_precedence() {
        let memory = vector_memory();
        let mut header = HeaderV1::new(64, 8, 16, TestEdge::BYTES as u32, 3);
        header.num_edges = 17;
        header.slab_occupied_tail = 23;
        let store = EdgeSlabStore::<TestEdge, _>::new(memory.clone(), header).unwrap();
        assert_eq!(store.header().unwrap(), header);
        header.elem_capacity = 63;
        header.segment_count = 4;
        header.segment_size = 32;
        header.tree_height = 2;
        header.num_edges = 19;
        header.slab_occupied_tail = 29;
        header.initial_vertex_edge_slots = 7;
        store.write_header(&header);
        assert_eq!(
            store.header().unwrap(),
            header,
            "must read current metadata, not cache it"
        );
        memory.write(0, b"BAD");
        memory.write(3, &[255]);
        assert_eq!(store.header(), Err(InitError::BadMagic { actual: *b"BAD" }));
        memory.write(0, &super::MAGIC);
        assert_eq!(store.header(), Err(InitError::IncompatibleVersion(255)));
    }

    #[test]
    fn edge_slab_init_rejects_elem_capacity_above_index_space() {
        let memory = vector_memory();
        let header = HeaderV1::new(64, 1, 4, TestEdge::BYTES as u32, 0);
        EdgeSlabStore::<TestEdge, _>::new(memory.clone(), header).expect("seed slab");
        write_u64(
            &memory,
            Address::from(4),
            MAX_SLOT_EXCLUSIVE_END.saturating_add(1),
        );
        assert!(matches!(
            EdgeSlabStore::<TestEdge, _>::init(memory),
            Err(InitError::ElemCapacityOverflow)
        ));
    }

    #[test]
    fn edge_slab_header_reserved_region_is_zeroed_and_reopens() {
        let memory = vector_memory();
        let store = EdgeSlabStore::<TestEdge, _>::new(
            memory.clone(),
            HeaderV1::new(8, 1, 4, TestEdge::BYTES as u32, 0),
        )
        .expect("seed slab");
        drop(store);

        let mut reserved = [0u8; RESERVED_SIZE];
        memory.read(RESERVED_OFFSET, &mut reserved);
        assert!(reserved.iter().all(|&byte| byte == 0));

        let reopened = EdgeSlabStore::<TestEdge, _>::init(memory).expect("reopen");
        assert_eq!(reopened.header().expect("header").elem_capacity, 8);
    }

    #[test]
    fn edge_slab_init_rejects_nonzero_reserved_byte() {
        let memory = vector_memory();
        let store = EdgeSlabStore::<TestEdge, _>::new(
            memory.clone(),
            HeaderV1::new(8, 1, 4, TestEdge::BYTES as u32, 0),
        )
        .expect("seed slab");
        drop(store);

        memory.write(RESERVED_OFFSET, &[1]);
        assert!(matches!(
            EdgeSlabStore::<TestEdge, _>::init(memory),
            Err(InitError::InvalidLayout)
        ));
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{EdgeSlabStore, HeaderV1};
    use crate::{bench as helper, test_support::TestEdge, traits::CsrEdge};

    /// Measures raw slab slot writes followed by raw slot reads. This is the
    /// lowest-level edge payload I/O baseline, below `EdgeStore` log and count
    /// bookkeeping.
    #[bench(raw)]
    fn bench_r_ed_swr_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = EdgeSlabStore::<TestEdge, _>::new(
            memories.memory(),
            HeaderV1::new(helper::MEDIUM_N, 16, 16, TestEdge::BYTES as u32, 0),
        )
        .expect("edge slab");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_edge_slab_write_read");
            let mut payload = [0u8; TestEdge::BYTES];
            for i in 0..helper::MEDIUM_N {
                helper::test_edge(i).write_to(&mut payload);
                store.write_slot(i, &payload).expect("write slot");
            }
            let mut sum = 0u32;
            for i in 0..helper::MEDIUM_N {
                store.read_slot(i, &mut payload);
                sum ^= TestEdge::read_from(&payload).0;
            }
            black_box(sum);
        })
    }
}
