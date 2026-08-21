//! Per-index encoding metadata — the LARA `label → width` generalization, owned by the vector
//! canister (ADR 0064 §8).
//!
//! One index = one encoding = one dims = one stride. The record derives every width the physical
//! layer needs (stored stride, SIMD-aligned scoring stride, row-meta aux width) and selects the
//! byte-level scoring kernel, so the page store and search path never re-derive encoding rules.

use gleaph_graph_kernel::vector_index::VectorEncoding;
use serde::{Deserialize, Serialize};

/// Byte-level scoring kernel selected by the encoding (the metric/formulation is orthogonal and
/// chosen at search time).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScoringKernel {
    /// Stored bytes are `f32` little-endian; score directly.
    F32Dot,
    /// Stored bytes are `f16`/`bf16`/`i8`/`u8`; upcast to `f32` once per page read.
    UpcastF32Dot,
}

/// Fail-closed encoding record error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodingError {
    /// `dims` is zero.
    ZeroDims,
    /// The minimal stride does not match `component_bytes × dims`.
    StrideMismatch {
        /// Derived minimal stride.
        expected: u32,
        /// Recorded stride.
        actual: u32,
    },
    /// The SIMD scratch stride is not a multiple of 16 or does not match
    /// `align16(component_bytes × dims)`.
    PadStrideMismatch(u32),
    /// `aux_bytes` is not one of 0 | 4 | 8.
    InvalidAuxBytes(u32),
    /// The kernel is inconsistent with the encoding.
    KernelMismatch,
    /// The derived width overflows `u32`.
    WidthOverflow,
}

/// Width computation for one index: everything the physical layer needs from `(encoding, dims)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodingRecord {
    /// The stored encoding (one index = one encoding).
    pub encoding: VectorEncoding,
    /// Number of components.
    pub dims: u16,
    /// Stored stride, minimal per encoding (`component_bytes × dims`).
    pub stride_bytes: u32,
    /// Scoring scratch stride `align16(component_bytes × dims)` (16-byte aligned for SIMD; the
    /// page store's `row_stride`).
    pub pad_stride_bytes: u32,
    /// Row-meta aux width: 0 | 4 | 8 (encoding/metric-dependent; see the design's `RowAux`).
    pub aux_bytes: u32,
    /// Byte-level scoring kernel for the encoding.
    pub kernel: ScoringKernel,
}

impl EncodingRecord {
    /// Builds the record from the index definition inputs, deriving every width and the aux
    /// default for `encoding` (the design's `RowAux` defaults: F32 0, I8 mandatory 4-byte scale —
    /// none depend on the metric; the metric-dependent pruning aux is opt-in and not part of the
    /// definition-time record).
    pub fn from_parts(encoding: VectorEncoding, dims: u16) -> Result<Self, EncodingError> {
        if dims == 0 {
            return Err(EncodingError::ZeroDims);
        }
        let stride_bytes = encoding
            .component_bytes()
            .checked_mul(u32::from(dims))
            .ok_or(EncodingError::WidthOverflow)?;
        // SIMD scratch stride: the stored stride aligned up to a 16-byte boundary. Valid
        // `(component_bytes, dims)` widths cannot overflow here (`4 × u16::MAX << u32::MAX`).
        let pad_stride_bytes = stride_bytes.div_ceil(16) * 16;
        let (aux_bytes, kernel) = match encoding {
            // F32: default formulations need no per-row aux (sub-square + early exit for L2;
            // normalized-dot only for cosine). I8 carries a mandatory 4-byte per-row quantization
            // scale and an upcast/fused scoring kernel.
            VectorEncoding::F32 => (0, ScoringKernel::F32Dot),
            VectorEncoding::I8 => (4, ScoringKernel::UpcastF32Dot),
        };
        let record = Self {
            encoding,
            dims,
            stride_bytes,
            pad_stride_bytes,
            aux_bytes,
            kernel,
        };
        record.validate()?;
        Ok(record)
    }

    /// Validates the record's internal consistency fail-closed.
    pub fn validate(&self) -> Result<(), EncodingError> {
        if self.dims == 0 {
            return Err(EncodingError::ZeroDims);
        }
        let expected_stride = self
            .encoding
            .component_bytes()
            .checked_mul(u32::from(self.dims))
            .ok_or(EncodingError::WidthOverflow)?;
        if self.stride_bytes != expected_stride {
            return Err(EncodingError::StrideMismatch {
                expected: expected_stride,
                actual: self.stride_bytes,
            });
        }
        let expected_pad = expected_stride.div_ceil(16) * 16;
        if self.pad_stride_bytes != expected_pad || !self.pad_stride_bytes.is_multiple_of(16) {
            return Err(EncodingError::PadStrideMismatch(self.pad_stride_bytes));
        }
        if !matches!(self.aux_bytes, 0 | 4 | 8) {
            return Err(EncodingError::InvalidAuxBytes(self.aux_bytes));
        }
        match (self.encoding, self.kernel) {
            (VectorEncoding::F32, ScoringKernel::F32Dot) => {}
            (VectorEncoding::F32, _) => return Err(EncodingError::KernelMismatch),
            (VectorEncoding::I8, ScoringKernel::UpcastF32Dot) => {}
            (VectorEncoding::I8, _) => return Err(EncodingError::KernelMismatch),
        }
        // The default aux widths do not depend on the metric; metric-dependent pruning aux is
        // opt-in and validated at configuration time, not here.
        Ok(())
    }

    /// Stored row-meta stride: `4 + aux_bytes` (4 | 8 | 12), the page store's `meta_stride`.
    pub fn meta_stride(&self) -> u32 {
        self.aux_bytes + 4
    }

    /// The stored stride per component (bytes).
    pub fn component_bytes(&self) -> u32 {
        self.encoding.component_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d1536_f32_widths() {
        // d = 1536: stride = pad = align16(4·1536) = 6144; meta 4.
        let record = EncodingRecord::from_parts(VectorEncoding::F32, 1536).expect("valid");
        assert_eq!(record.stride_bytes, 6144);
        assert_eq!(record.pad_stride_bytes, 6144);
        assert_eq!(record.aux_bytes, 0);
        assert_eq!(record.meta_stride(), 4);
        assert_eq!(record.kernel, ScoringKernel::F32Dot);
    }

    #[test]
    fn d1536_i8_widths() {
        // d = 1536: stride = pad = align16(1·1536) = 1536; meta = 4 + 4 (scale) = 8.
        let record = EncodingRecord::from_parts(VectorEncoding::I8, 1536).expect("valid");
        assert_eq!(record.stride_bytes, 1536);
        assert_eq!(record.pad_stride_bytes, 1536);
        assert_eq!(record.aux_bytes, 4);
        assert_eq!(record.meta_stride(), 8);
        assert_eq!(record.kernel, ScoringKernel::UpcastF32Dot);
    }

    #[test]
    fn d17_i8_widths_pad_to_16_byte_boundary() {
        // d = 17: stride 17; pad = align16(17) = 32 (a non-multiple-of-16 dims still yields a
        // 16-byte-aligned row).
        let record = EncodingRecord::from_parts(VectorEncoding::I8, 17).expect("valid");
        assert_eq!(record.stride_bytes, 17);
        assert_eq!(record.pad_stride_bytes, 32);
        assert!(record.pad_stride_bytes.is_multiple_of(16));
    }

    #[test]
    fn d4_f32_widths() {
        // d = 4: stride = pad = align16(4·4) = 16; meta 4.
        let record = EncodingRecord::from_parts(VectorEncoding::F32, 4).expect("valid");
        assert_eq!(record.stride_bytes, 16);
        assert_eq!(record.pad_stride_bytes, 16);
        assert_eq!(record.meta_stride(), 4);
    }

    #[test]
    fn non_multiple_of_four_dims_pad_up() {
        // d = 17: stride align16 target = 4·17 = 68; pad = align16(68) = 80.
        let record = EncodingRecord::from_parts(VectorEncoding::F32, 17).expect("valid");
        assert_eq!(record.stride_bytes, 68);
        assert_eq!(record.pad_stride_bytes, 80);
        assert!(record.pad_stride_bytes.is_multiple_of(16));
    }

    #[test]
    fn zero_dims_rejected() {
        assert_eq!(
            EncodingRecord::from_parts(VectorEncoding::F32, 0),
            Err(EncodingError::ZeroDims)
        );
    }

    #[test]
    fn validate_rejects_inconsistent_widths() {
        let mut record = EncodingRecord::from_parts(VectorEncoding::F32, 4).expect("valid");
        record.stride_bytes = 17;
        assert_eq!(
            record.validate(),
            Err(EncodingError::StrideMismatch {
                expected: 16,
                actual: 17,
            })
        );
        record.stride_bytes = 16;
        record.pad_stride_bytes = 63;
        assert_eq!(record.validate(), Err(EncodingError::PadStrideMismatch(63)));
        // An aligned pad wider than `align16(stride)` is still rejected.
        record.pad_stride_bytes = 32;
        assert_eq!(record.validate(), Err(EncodingError::PadStrideMismatch(32)));
    }

    #[test]
    fn validate_rejects_invalid_aux() {
        let mut record = EncodingRecord::from_parts(VectorEncoding::F32, 4).expect("valid");
        record.aux_bytes = 6;
        assert_eq!(record.validate(), Err(EncodingError::InvalidAuxBytes(6)));
    }

    #[test]
    fn validate_rejects_wrong_kernel() {
        let mut record = EncodingRecord::from_parts(VectorEncoding::F32, 4).expect("valid");
        record.kernel = ScoringKernel::UpcastF32Dot;
        assert_eq!(record.validate(), Err(EncodingError::KernelMismatch));
    }
}
