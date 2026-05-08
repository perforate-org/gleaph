use super::label::LabelId;
use ic_stable_lara::traits::{CsrVertex, CsrVertexTombstone};
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;
use std::mem::size_of;
use std::slice;

/// Per-vertex base-neighborhood locator for one CSR vertex row.
///
/// [`Self::base_slot_start`] is the global edge-slot index of the first slot in
/// this vertex's clean slab prefix (the value surfaced as [`CsrVertex::base_slot_start`]).
/// [`Self::metadata`] holds packed metadata read and written via
/// [`CsrVertex::log_head`] / [`CsrVertex::with_log_head`] and
/// [`CsrVertexTombstone::is_tombstone`] / [`CsrVertexTombstone::with_tombstone`].
///
/// Invariants:
/// - The base neighborhood is one contiguous interval of slab slots.
/// - [`Self::base_slot_start`] is the start of that interval in `Edge` slot units.
/// - Overflow neighbors are not counted in [`Self::live_edge_count`] for clean scans.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Vertex {
    /// First edge slot in this vertex's clean slab prefix.
    pub base_slot_start: u64,
    /// Number of live outgoing edges visible through clean scans.
    pub live_edge_count: u32,
    /// Number of slab slots owned by this vertex's current span.
    pub span_capacity: u32,
    /// Packed metadata:
    /// - bits 0..=7: overflow head plus one (`0` means no overflow)
    /// - bits 8..=23: primary inline label id (`0` means no inline label)
    /// - bit 24: additional label sidecar exists
    /// - bits 25..=30: reserved for property sidecars and future metadata
    /// - bit 31: vertex tombstone
    pub metadata: i32,
}

/// Bits 0..=7: overflow-head index plus one.
const LOG_HEAD_PLUS_ONE_MASK: u32 = 0x0000_00ff;
/// Bits 8..=23: primary inline label id.
const PRIMARY_LABEL_ID_MASK: u32 = 0x00ff_ff00;
const PRIMARY_LABEL_ID_SHIFT: u32 = 8;
/// Bit 24: vertex has additional labels in the sidecar label store.
const LABEL_SIDECAR_BIT: u32 = 1 << 24;
/// Bit 31: vertex tombstone.
const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;

const _: () = assert!(size_of::<Vertex>() == <Vertex as CsrVertex>::BYTES);

impl Vertex {
    #[inline]
    fn metadata_word(self) -> u32 {
        self.metadata as u32
    }

    #[inline]
    fn with_metadata_word(mut self, raw: u32) -> Self {
        self.metadata = raw as i32;
        self
    }

    #[inline]
    pub fn primary_label_id(self) -> Option<LabelId> {
        let raw = (self.metadata_word() & PRIMARY_LABEL_ID_MASK) >> PRIMARY_LABEL_ID_SHIFT;
        if raw == 0 {
            None
        } else {
            Some(LabelId::from_raw(raw as u16))
        }
    }

    #[inline]
    pub fn with_primary_label_id(self, label_id: Option<LabelId>) -> Self {
        let mut raw = self.metadata_word() & !PRIMARY_LABEL_ID_MASK;
        if let Some(label_id) = label_id {
            raw |= (u32::from(label_id.raw()) << PRIMARY_LABEL_ID_SHIFT) & PRIMARY_LABEL_ID_MASK;
        }
        self.with_metadata_word(raw)
    }

    #[inline]
    pub fn has_label_sidecar(self) -> bool {
        (self.metadata_word() & LABEL_SIDECAR_BIT) != 0
    }

    #[inline]
    pub fn with_label_sidecar(self, has_sidecar: bool) -> Self {
        let mut raw = self.metadata_word();
        if has_sidecar {
            raw |= LABEL_SIDECAR_BIT;
        } else {
            raw &= !LABEL_SIDECAR_BIT;
        }
        self.with_metadata_word(raw)
    }
}

impl CsrVertex for Vertex {
    const BYTES: usize = 20;

    fn base_slot_start(&self) -> u64 {
        self.base_slot_start
    }

    fn degree(&self) -> u32 {
        self.live_edge_count
    }

    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.base_slot_start = start;
        self
    }

    fn with_degree(mut self, degree: u32) -> Self {
        self.live_edge_count = degree;
        self
    }

    fn log_head(self) -> i32 {
        let encoded = self.metadata_word() & LOG_HEAD_PLUS_ONE_MASK;
        if encoded == 0 {
            -1
        } else {
            encoded as i32 - 1
        }
    }

    fn with_log_head(mut self, idx: i32) -> Self {
        let mut raw = self.metadata_word() & !LOG_HEAD_PLUS_ONE_MASK;
        if idx >= 0 {
            assert!(
                idx < u8::MAX as i32,
                "vertex log head does not fit in 8 bits"
            );
            raw |= (idx as u32 + 1) & LOG_HEAD_PLUS_ONE_MASK;
        }
        self.metadata = raw as i32;
        self
    }

    fn span_capacity(&self) -> u32 {
        self.span_capacity
    }

    fn with_span_capacity(mut self, capacity: u32) -> Self {
        self.span_capacity = capacity;
        self
    }
}

impl CsrVertexTombstone for Vertex {
    fn is_tombstone(&self) -> bool {
        ((*self).metadata_word() & VERTEX_TOMBSTONE_BIT) != 0
    }

    fn with_tombstone(self, tomb: bool) -> Self {
        let mut raw = self.metadata_word();
        if tomb {
            raw |= VERTEX_TOMBSTONE_BIT;
        } else {
            raw &= !VERTEX_TOMBSTONE_BIT;
        }
        self.with_metadata_word(raw)
    }
}

#[cfg(any(test, not(target_endian = "little")))]
fn vertex_row_bytes(v: &Vertex) -> [u8; Vertex::BYTES] {
    let mut b = [0u8; Vertex::BYTES];
    b[0..8].copy_from_slice(&v.base_slot_start.to_le_bytes());
    b[8..12].copy_from_slice(&v.live_edge_count.to_le_bytes());
    b[12..16].copy_from_slice(&v.span_capacity.to_le_bytes());
    b[16..20].copy_from_slice(&v.metadata.to_le_bytes());
    b
}

#[cfg(not(target_endian = "little"))]
fn vertex_from_row(chunk: &[u8; Vertex::BYTES]) -> Vertex {
    Vertex {
        base_slot_start: u64::from_le_bytes(chunk[0..8].try_into().unwrap()),
        live_edge_count: u32::from_le_bytes(chunk[8..12].try_into().unwrap()),
        span_capacity: u32::from_le_bytes(chunk[12..16].try_into().unwrap()),
        metadata: i32::from_le_bytes(chunk[16..20].try_into().unwrap()),
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
            base_slot_start: 0x0102_0304_0506_0708,
            live_edge_count: 0x090a_0b0c,
            span_capacity: 0x0d0e_0f10,
            metadata: i32::from_le_bytes([0x11, 0x12, 0x13, 0x14]),
        };
        let cow = v.to_bytes();
        assert!(matches!(cow, Cow::Borrowed(_)));
        assert_eq!(cow.as_ref(), vertex_row_bytes(&v).as_slice());
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn from_bytes_recover_explicit_le_row() {
        let v = Vertex {
            base_slot_start: 0x0102_0304_0506_0708,
            live_edge_count: 0x090a_0b0c,
            span_capacity: 0x0d0e_0f10,
            metadata: i32::from_le_bytes([0x11, 0x12, 0x13, 0x14]),
        };
        let row = vertex_row_bytes(&v);
        assert_eq!(Vertex::from_bytes(Cow::Borrowed(&row[..])), v);
    }

    #[test]
    fn storable_roundtrip() {
        let v = Vertex {
            base_slot_start: 1,
            live_edge_count: 2,
            span_capacity: 3,
            metadata: 0,
        };
        assert_eq!(Vertex::from_bytes(v.to_bytes()), v);
        assert_eq!(Vertex::from_bytes(Cow::Owned(v.into_bytes())), v);
    }

    #[test]
    fn log_head_uses_low_byte_and_preserves_label_bits() {
        let labelled = Vertex {
            base_slot_start: 1,
            live_edge_count: 2,
            span_capacity: 3,
            metadata: 0,
        }
        .with_primary_label_id(Some(LabelId::from_raw(42)))
        .with_label_sidecar(true);

        let with_log = labelled.with_log_head(169);
        assert_eq!(with_log.log_head(), 169);
        assert_eq!(with_log.primary_label_id(), Some(LabelId::from_raw(42)));
        assert!(with_log.has_label_sidecar());

        let cleared = with_log.with_log_head(-1);
        assert_eq!(cleared.log_head(), -1);
        assert_eq!(cleared.primary_label_id(), Some(LabelId::from_raw(42)));
        assert!(cleared.has_label_sidecar());
    }

    #[test]
    fn tombstone_bit_preserves_label_and_log_bits() {
        let v = Vertex {
            base_slot_start: 1,
            live_edge_count: 2,
            span_capacity: 3,
            metadata: 0,
        }
        .with_log_head(7)
        .with_primary_label_id(Some(LabelId::from_raw(9)))
        .with_tombstone(true);

        assert!(v.is_tombstone());
        assert_eq!(v.log_head(), 7);
        assert_eq!(v.primary_label_id(), Some(LabelId::from_raw(9)));

        let live = v.with_tombstone(false);
        assert!(!live.is_tombstone());
        assert_eq!(live.log_head(), 7);
        assert_eq!(live.primary_label_id(), Some(LabelId::from_raw(9)));
    }

    #[test]
    #[should_panic(expected = "vertex log head does not fit in 8 bits")]
    fn oversized_log_head_panics() {
        let v = Vertex {
            base_slot_start: 1,
            live_edge_count: 2,
            span_capacity: 3,
            metadata: 0,
        };

        let _ = v.with_log_head(255);
    }
}
