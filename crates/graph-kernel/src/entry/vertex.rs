use ic_stable_lara::traits::{CsrVertex, CsrVertexTombstone};
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;
use std::mem::size_of;
use std::slice;

/// Per-vertex base-neighborhood locator for one CSR vertex row.
///
/// [`Self::edge_index`] is the global edge-slot index of the first slot in this
/// vertex's clean slab prefix (the value surfaced as [`CsrVertex::base_slot_start`]).
/// [`Self::log_offset`] holds packed metadata read and written via
/// [`CsrVertex::log_head`] / [`CsrVertex::with_log_head`] and
/// [`CsrVertexTombstone::is_tombstone`] / [`CsrVertexTombstone::with_tombstone`].
///
/// Invariants:
/// - The base neighborhood is one contiguous interval of slab slots.
/// - [`Self::edge_index`] is the start of that interval in `Edge` slot units.
/// - Overflow neighbors are not counted in [`Self::degree`] for clean scans.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Vertex {
    /// First edge slot in this vertex's clean slab prefix.
    pub edge_index: u64,
    /// Number of live outgoing edges visible through clean scans.
    pub degree: u32,
    /// Number of slab slots owned by this vertex's current span.
    pub capacity: u32,
    /// Packed metadata:
    /// - bit31: vertex tombstone
    /// - bit30: empty overflow sentinel
    /// - low 30 bits: overflow head offset
    pub log_offset: i32,
}

/// Bit 31: vertex tombstone.
const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;
/// Bit 30: overflow-head empty sentinel.
const LOG_EMPTY_BIT: u32 = 1 << 30;
/// Low 30 bits: overflow-head offset.
const LOG_OFFSET_BITS_MASK: u32 = (1 << 30) - 1;

const _: () = assert!(size_of::<Vertex>() == <Vertex as CsrVertex>::BYTES);

impl CsrVertex for Vertex {
    const BYTES: usize = 20;

    fn base_slot_start(&self) -> u64 {
        self.edge_index
    }

    fn degree(&self) -> u32 {
        self.degree
    }

    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.edge_index = start;
        self
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }

    fn log_head(self) -> i32 {
        let raw = self.log_offset as u32;
        if (raw & LOG_EMPTY_BIT) != 0 {
            return -1;
        }
        (raw & LOG_OFFSET_BITS_MASK) as i32
    }

    fn with_log_head(mut self, idx: i32) -> Self {
        let mut raw = self.log_offset as u32;
        raw &= !(LOG_EMPTY_BIT | LOG_OFFSET_BITS_MASK);
        if idx < 0 {
            raw |= LOG_EMPTY_BIT;
        } else {
            raw |= (idx as u32) & LOG_OFFSET_BITS_MASK;
        }
        self.log_offset = raw as i32;
        self
    }

    fn span_capacity(&self) -> u32 {
        self.capacity
    }

    fn with_span_capacity(mut self, capacity: u32) -> Self {
        self.capacity = capacity;
        self
    }
}

impl CsrVertexTombstone for Vertex {
    fn is_tombstone(&self) -> bool {
        (self.log_offset as u32 & VERTEX_TOMBSTONE_BIT) != 0
    }

    fn with_tombstone(mut self, tomb: bool) -> Self {
        let mut raw = self.log_offset as u32;
        if tomb {
            raw |= VERTEX_TOMBSTONE_BIT;
        } else {
            raw &= !VERTEX_TOMBSTONE_BIT;
        }
        self.log_offset = raw as i32;
        self
    }
}

#[cfg(any(test, not(target_endian = "little")))]
fn vertex_row_bytes(v: &Vertex) -> [u8; Vertex::BYTES] {
    let mut b = [0u8; Vertex::BYTES];
    b[0..8].copy_from_slice(&v.edge_index.to_le_bytes());
    b[8..12].copy_from_slice(&v.degree.to_le_bytes());
    b[12..16].copy_from_slice(&v.capacity.to_le_bytes());
    b[16..20].copy_from_slice(&v.log_offset.to_le_bytes());
    b
}

#[cfg(not(target_endian = "little"))]
fn vertex_from_row(chunk: &[u8; Vertex::BYTES]) -> Vertex {
    Vertex {
        edge_index: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
        degree: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
        capacity: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
        log_offset: i32::from_le_bytes(chunk[16..20].try_into().unwrap()),
    }
}

impl Storable for Vertex {
    const BOUND: Bound = Bound::Bounded {
        max_size: 20,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        // On little-endian, `repr(C, packed)` layout matches explicit LE serialization (see
        // `vertex_row_bytes`). ICP canisters are wasm32 (LE); fallback keeps `to_bytes` correct
        // elsewhere without relying on native field endianness.
        #[cfg(target_endian = "little")]
        {
            Cow::Borrowed(unsafe {
                slice::from_raw_parts(
                    (self as *const Self).cast::<u8>(),
                    <Self as CsrVertex>::BYTES,
                )
            })
        }
        #[cfg(not(target_endian = "little"))]
        {
            Cow::Owned(vertex_row_bytes(self).into())
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        #[cfg(target_endian = "little")]
        {
            // SAFETY: `Vertex` is `repr(C, packed)` with size `BYTES`; on LE this is the on-wire layout.
            let bytes: [u8; <Self as CsrVertex>::BYTES] =
                unsafe { std::mem::transmute_copy(&self) };
            Vec::from(bytes)
        }
        #[cfg(not(target_endian = "little"))]
        {
            Vec::from(vertex_row_bytes(&self))
        }
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let chunk: &[u8; Vertex::BYTES] = bytes
            .as_ref()
            .try_into()
            .expect("Vertex::from_bytes expects exactly 20 bytes");
        #[cfg(target_endian = "little")]
        {
            // SAFETY: `Vertex` is `repr(C, packed)` (align 1); on LE, disk/wire layout matches memory.
            // `read_unaligned` matches how we may load from arbitrary stable-memory offsets.
            unsafe { chunk.as_ptr().cast::<Vertex>().read_unaligned() }
        }
        #[cfg(not(target_endian = "little"))]
        {
            vertex_from_row(chunk)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guardrail: `to_bytes` must stay aligned with explicit LE serialization (see `vertex_row_bytes`).
    #[cfg(target_endian = "little")]
    #[test]
    fn to_bytes_borrows_identical_to_explicit_le_row() {
        let v = Vertex {
            edge_index: 0x0102_0304_0506_0708,
            degree: 0x090a_0b0c,
            capacity: 0x0d0e_0f10,
            log_offset: i32::from_le_bytes([0x11, 0x12, 0x13, 0x14]),
        };
        let cow = v.to_bytes();
        assert!(matches!(cow, Cow::Borrowed(_)));
        assert_eq!(cow.as_ref(), vertex_row_bytes(&v).as_slice());
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn from_bytes_recover_explicit_le_row() {
        let v = Vertex {
            edge_index: 0x0102_0304_0506_0708,
            degree: 0x090a_0b0c,
            capacity: 0x0d0e_0f10,
            log_offset: i32::from_le_bytes([0x11, 0x12, 0x13, 0x14]),
        };
        let row = vertex_row_bytes(&v);
        assert_eq!(Vertex::from_bytes(Cow::Borrowed(&row[..])), v);
    }

    #[test]
    fn storable_roundtrip() {
        let v = Vertex {
            edge_index: 1,
            degree: 2,
            capacity: 3,
            log_offset: 0,
        };
        assert_eq!(Vertex::from_bytes(v.to_bytes()), v);
        assert_eq!(Vertex::from_bytes(Cow::Owned(v.into_bytes())), v);
    }
}
