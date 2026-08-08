//! Row identity payload and row-meta layout.
//!
//! A row stores only the 30-bit shard-local vertex id (the shard is shared via the run table),
//! mirroring the graph's own 30-bit `VertexRef::PAYLOAD_MASK`. Bit 30 is reserved and bit 31 is
//! the tombstone bit, so a partition scan can cheaply skip dead rows.

use crate::header::MAX_META_STRIDE;

/// Mask over the 30 usable vertex-id bits (bits 0..30).
pub const VERTEX_PAYLOAD_MASK: u32 = (1 << 30) - 1;
/// Bit 30 is reserved (must stay clear on write).
pub const VERTEX_PAYLOAD_RESERVED_BIT: u32 = 1 << 30;
/// Bit 31 marks a tombstoned (deleted) row.
pub const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;
/// Largest valid vertex id (fits the 30-bit payload).
pub const MAX_VERTEX_ID: u32 = VERTEX_PAYLOAD_MASK;

/// Row identity payload: 30-bit vertex id + reserved bit 30 + tombstone bit 31.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VertexPayload(u32);

impl VertexPayload {
    /// Wraps a raw `u32` without validation (for deserialization and construction from
    /// already-validated values).
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Constructs a live payload for `vertex_id`, failing closed when the id does not fit the
    /// 30-bit contract (the ingest boundary rejects `vertex_id >= 2^30`).
    pub const fn new(vertex_id: u32) -> Option<Self> {
        if vertex_id > VERTEX_PAYLOAD_MASK {
            None
        } else {
            Some(Self(vertex_id))
        }
    }

    /// Returns the 30-bit vertex id.
    pub const fn vertex_id(self) -> u32 {
        self.0 & VERTEX_PAYLOAD_MASK
    }

    /// Returns `true` when the row is tombstoned (deleted).
    pub const fn is_tombstone(self) -> bool {
        self.0 & VERTEX_TOMBSTONE_BIT != 0
    }

    /// Returns the tombstoned form of this payload (id preserved, tombstone bit set).
    pub const fn tombstoned(self) -> Self {
        Self(self.0 | VERTEX_TOMBSTONE_BIT)
    }

    /// Returns the raw `u32` backing word.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Error decoding a [`RowMeta`] from its stored bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    /// `meta_stride` is not one of 4 | 8 | 12.
    InvalidMetaStride(usize),
    /// The byte slice length does not match `meta_stride`.
    BadSliceLength {
        /// Expected byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
    /// The stored vertex id violates the 30-bit contract.
    VertexIdOutOfRange(u32),
}

/// The stored row meta: identity payload plus encoding-dependent aux bytes.
///
/// Stored width is `meta_stride` (4 | 8 | 12); the aux field is only meaningful in its low
/// `meta_stride - 4` bytes. The storage layer keeps aux opaque — interpretation belongs to the
/// canister's `(encoding, metric, pruning_config)` row-aux contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowMeta {
    /// Identity payload (vertex id + tombstone bit).
    pub vertex: VertexPayload,
    /// Aux bytes (0 | 4 | 8 meaningful bytes depending on `meta_stride`).
    pub aux: [u8; 8],
}

impl RowMeta {
    /// Constructs row meta from a payload and aux bytes.
    pub const fn new(vertex: VertexPayload, aux: [u8; 8]) -> Self {
        Self { vertex, aux }
    }

    /// Encodes exactly `meta_stride` bytes: 4-byte payload then `meta_stride - 4` aux bytes.
    pub fn write_into(&self, out: &mut [u8], meta_stride: usize) -> Result<(), PayloadError> {
        if !valid_meta_stride(meta_stride) {
            return Err(PayloadError::InvalidMetaStride(meta_stride));
        }
        if out.len() != meta_stride {
            return Err(PayloadError::BadSliceLength {
                expected: meta_stride,
                actual: out.len(),
            });
        }
        out[..4].copy_from_slice(&self.vertex.raw().to_le_bytes());
        out[4..meta_stride].copy_from_slice(&self.aux[..meta_stride - 4]);
        Ok(())
    }

    /// Decodes row meta from its stored `meta_stride` bytes.
    pub fn from_bytes(bytes: &[u8], meta_stride: usize) -> Result<Self, PayloadError> {
        if !valid_meta_stride(meta_stride) {
            return Err(PayloadError::InvalidMetaStride(meta_stride));
        }
        if bytes.len() != meta_stride {
            return Err(PayloadError::BadSliceLength {
                expected: meta_stride,
                actual: bytes.len(),
            });
        }
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[..4]);
        let vertex = VertexPayload::from_raw(u32::from_le_bytes(raw));
        if vertex.raw() & VERTEX_PAYLOAD_RESERVED_BIT != 0 {
            return Err(PayloadError::VertexIdOutOfRange(vertex.raw()));
        }
        let mut aux = [0u8; 8];
        aux[..meta_stride - 4].copy_from_slice(&bytes[4..meta_stride]);
        Ok(Self { vertex, aux })
    }

    /// Returns the meaningful aux slice for a given `meta_stride`.
    pub fn aux_slice(&self, meta_stride: usize) -> &[u8] {
        debug_assert!(valid_meta_stride(meta_stride));
        &self.aux[..meta_stride - 4]
    }
}

/// Returns `true` when `meta_stride` is one of 4 | 8 | 12.
pub const fn valid_meta_stride(meta_stride: usize) -> bool {
    matches!(meta_stride, 4 | 8 | 12)
}

/// Returns the stored meta stride for an aux width (`0 | 4 | 8 -> 4 | 8 | 12`), or `None`.
pub const fn meta_stride_for_aux(aux_bytes: usize) -> Option<usize> {
    match aux_bytes {
        0 => Some(4),
        4 => Some(8),
        8 => Some(12),
        _ => None,
    }
}

/// Maximum aux bytes a row meta can carry (8; see [`MAX_META_STRIDE`]).
pub const MAX_AUX_BYTES: usize = MAX_META_STRIDE as usize - 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_payload_respects_30_bit_contract() {
        assert_eq!(VertexPayload::new(0).expect("zero").vertex_id(), 0);
        assert_eq!(
            VertexPayload::new(MAX_VERTEX_ID).expect("max").vertex_id(),
            MAX_VERTEX_ID
        );
        assert!(VertexPayload::new(MAX_VERTEX_ID + 1).is_none());
    }

    #[test]
    fn vertex_payload_tombstone_preserves_id() {
        let live = VertexPayload::new(7).expect("id");
        assert!(!live.is_tombstone());
        let dead = live.tombstoned();
        assert!(dead.is_tombstone());
        assert_eq!(dead.vertex_id(), 7);
        assert_eq!(dead.raw(), 7 | VERTEX_TOMBSTONE_BIT);
    }

    #[test]
    fn row_meta_roundtrips_at_each_stride() {
        let meta = RowMeta::new(
            VertexPayload::new(1234).expect("id"),
            [0xAA, 0xBB, 0xCC, 0xDD, 1, 2, 3, 4],
        );
        for (aux_bytes, stride) in [(0, 4), (4, 8), (8, 12)] {
            let mut buf = [0u8; 12];
            meta.write_into(&mut buf[..stride], stride).expect("encode");
            assert_eq!(&buf[stride..], &vec![0u8; 12 - stride][..]);
            let decoded = RowMeta::from_bytes(&buf[..stride], stride).expect("decode");
            assert_eq!(decoded.vertex, meta.vertex);
            // Only the meaningful aux bytes survive the roundtrip.
            assert_eq!(&decoded.aux[..aux_bytes], &meta.aux[..aux_bytes]);
            assert_eq!(&decoded.aux[aux_bytes..], &vec![0u8; 8 - aux_bytes][..]);
        }
    }

    #[test]
    fn row_meta_rejects_invalid_stride_and_length() {
        let meta = RowMeta::new(VertexPayload::new(1).expect("id"), [0; 8]);
        let mut buf = [0u8; 12];
        assert_eq!(
            meta.write_into(&mut buf[..4], 6).expect_err("bad stride"),
            PayloadError::InvalidMetaStride(6)
        );
        assert_eq!(
            meta.write_into(&mut buf[..6], 8).expect_err("bad length"),
            PayloadError::BadSliceLength {
                expected: 8,
                actual: 6,
            }
        );
        assert_eq!(
            RowMeta::from_bytes(&buf[..4], 6).expect_err("bad stride"),
            PayloadError::InvalidMetaStride(6)
        );
    }

    #[test]
    fn row_meta_rejects_reserved_bit_set() {
        let mut buf = [0u8; 4];
        buf[..4].copy_from_slice(&(1u32 << 30).to_le_bytes());
        let err = RowMeta::from_bytes(&buf, 4).expect_err("reserved bit must fail");
        assert_eq!(err, PayloadError::VertexIdOutOfRange(1 << 30));
    }

    #[test]
    fn meta_stride_mapping() {
        assert_eq!(meta_stride_for_aux(0), Some(4));
        assert_eq!(meta_stride_for_aux(4), Some(8));
        assert_eq!(meta_stride_for_aux(8), Some(12));
        assert_eq!(meta_stride_for_aux(2), None);
    }
}
