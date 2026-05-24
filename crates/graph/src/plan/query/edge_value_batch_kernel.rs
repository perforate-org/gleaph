//! Width-dispatched edge-value batch kernels.

use gleaph_graph_kernel::entry::EdgeValueEncoding;
use ic_stable_lara::labeled::ValueWidthCode;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedEdgeValueBatchKernel {
    width_code: ValueWidthCode,
    encoding: EdgeValueEncoding,
}

impl PreparedEdgeValueBatchKernel {
    pub(crate) fn new(width_code: ValueWidthCode, encoding: EdgeValueEncoding) -> Self {
        Self {
            width_code,
            encoding,
        }
    }

    pub(crate) fn width_code(&self) -> ValueWidthCode {
        self.width_code
    }

    pub(crate) fn encoding(&self) -> &EdgeValueEncoding {
        &self.encoding
    }

    pub(crate) fn collect_equal_value_indices(
        &self,
        value_bytes: &[u8],
        needle: &[u8],
        out: &mut Vec<usize>,
    ) {
        let width = usize::from(self.width_code.byte_width());
        if width == 0 || needle.len() != width || value_bytes.len() % width != 0 {
            return;
        }
        match self.width_code {
            ValueWidthCode::W1 => collect_equal_w1(value_bytes, needle[0], out),
            ValueWidthCode::W2
            | ValueWidthCode::W4
            | ValueWidthCode::W8
            | ValueWidthCode::W16
            | ValueWidthCode::W32
            | ValueWidthCode::W64 => collect_equal_fixed_width(value_bytes, needle, width, out),
            ValueWidthCode::Zero => {}
        }
    }
}

fn collect_equal_fixed_width(
    value_bytes: &[u8],
    needle: &[u8],
    width: usize,
    out: &mut Vec<usize>,
) {
    for (idx, bytes) in value_bytes.chunks_exact(width).enumerate() {
        if bytes == needle {
            out.push(idx);
        }
    }
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_equal_w1(value_bytes: &[u8], needle: u8, out: &mut Vec<usize>) {
    for (idx, byte) in value_bytes.iter().copied().enumerate() {
        if byte == needle {
            out.push(idx);
        }
    }
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_equal_w1(value_bytes: &[u8], needle: u8, out: &mut Vec<usize>) {
    use core::arch::wasm32::{i8x16_bitmask, i8x16_eq, i8x16_splat, v128_load};

    let needle_v = i8x16_splat(needle as i8);
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            i8x16_bitmask(i8x16_eq(values, needle_v))
        };
        let mut bits = mask as u32;
        while bits != 0 {
            let lane = bits.trailing_zeros() as usize;
            out.push(chunk_idx * 16 + lane);
            bits &= bits - 1;
        }
    }
    let base = value_bytes.len() - chunks.remainder().len();
    for (idx, byte) in chunks.remainder().iter().copied().enumerate() {
        if byte == needle {
            out.push(base + idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w1_equal_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W1, EdgeValueEncoding::RawU8);
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&[1, 2, 1, 3, 1], &[1], &mut out);
        assert_eq!(out, vec![0, 2, 4]);
    }

    #[test]
    fn w4_equal_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W4, EdgeValueEncoding::RawU32);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&bytes, &1u32.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }
}
