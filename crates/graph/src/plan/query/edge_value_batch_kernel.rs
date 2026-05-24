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
            ValueWidthCode::W2 => collect_equal_w2(value_bytes, needle, out),
            ValueWidthCode::W4 => collect_equal_w4(value_bytes, needle, out),
            ValueWidthCode::W8 => collect_equal_w8(value_bytes, needle, out),
            ValueWidthCode::W16 => collect_equal_w16(value_bytes, needle, out),
            ValueWidthCode::W32 | ValueWidthCode::W64 => {
                collect_equal_fixed_width(value_bytes, needle, width, out)
            }
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

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_equal_w2(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    collect_equal_fixed_width(value_bytes, needle, 2, out);
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_equal_w2(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    use core::arch::wasm32::{i16x8_bitmask, i16x8_eq, i16x8_splat, v128_load};

    let needle_v = i16x8_splat(i16::from_le_bytes([needle[0], needle[1]]));
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            i16x8_bitmask(i16x8_eq(values, needle_v))
        };
        push_masked_lanes(mask as u32, chunk_idx * 8, out);
    }
    let base = value_bytes.len() - chunks.remainder().len();
    collect_equal_fixed_width_with_base(chunks.remainder(), needle, 2, base / 2, out);
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_equal_w4(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    collect_equal_fixed_width(value_bytes, needle, 4, out);
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_equal_w4(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    use core::arch::wasm32::{i32x4_bitmask, i32x4_eq, i32x4_splat, v128_load};

    let needle_v = i32x4_splat(i32::from_le_bytes([
        needle[0], needle[1], needle[2], needle[3],
    ]));
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            i32x4_bitmask(i32x4_eq(values, needle_v))
        };
        push_masked_lanes(mask as u32, chunk_idx * 4, out);
    }
    let base = value_bytes.len() - chunks.remainder().len();
    collect_equal_fixed_width_with_base(chunks.remainder(), needle, 4, base / 4, out);
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_equal_w8(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    collect_equal_fixed_width(value_bytes, needle, 8, out);
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_equal_w8(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    use core::arch::wasm32::{i64x2_bitmask, i64x2_eq, i64x2_splat, v128_load};

    let needle_v = i64x2_splat(i64::from_le_bytes([
        needle[0], needle[1], needle[2], needle[3], needle[4], needle[5], needle[6], needle[7],
    ]));
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            i64x2_bitmask(i64x2_eq(values, needle_v))
        };
        push_masked_lanes(mask as u32, chunk_idx * 2, out);
    }
    let base = value_bytes.len() - chunks.remainder().len();
    collect_equal_fixed_width_with_base(chunks.remainder(), needle, 8, base / 8, out);
}

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_equal_w16(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    collect_equal_fixed_width(value_bytes, needle, 16, out);
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_equal_w16(value_bytes: &[u8], needle: &[u8], out: &mut Vec<usize>) {
    use core::arch::wasm32::{i8x16_bitmask, i8x16_eq, v128_load};

    let needle_v = unsafe { v128_load(needle.as_ptr().cast()) };
    for (idx, bytes) in value_bytes.chunks_exact(16).enumerate() {
        let mask = unsafe {
            let values = v128_load(bytes.as_ptr().cast());
            i8x16_bitmask(i8x16_eq(values, needle_v))
        };
        if mask == 0xffff {
            out.push(idx);
        }
    }
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_equal_fixed_width_with_base(
    value_bytes: &[u8],
    needle: &[u8],
    width: usize,
    base_idx: usize,
    out: &mut Vec<usize>,
) {
    for (idx, bytes) in value_bytes.chunks_exact(width).enumerate() {
        if bytes == needle {
            out.push(base_idx + idx);
        }
    }
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn push_masked_lanes(mask: u32, base_idx: usize, out: &mut Vec<usize>) {
    let mut bits = mask;
    while bits != 0 {
        let lane = bits.trailing_zeros() as usize;
        out.push(base_idx + lane);
        bits &= bits - 1;
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

    #[test]
    fn w2_equal_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W2, EdgeValueEncoding::RawU16);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7u16.to_le_bytes());
        bytes.extend_from_slice(&9u16.to_le_bytes());
        bytes.extend_from_slice(&7u16.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&bytes, &7u16.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn w8_equal_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W8, EdgeValueEncoding::RawU64);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&11u64.to_le_bytes());
        bytes.extend_from_slice(&12u64.to_le_bytes());
        bytes.extend_from_slice(&11u64.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&bytes, &11u64.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }

    #[test]
    fn w16_equal_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W16, EdgeValueEncoding::RawU128);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&13u128.to_le_bytes());
        bytes.extend_from_slice(&14u128.to_le_bytes());
        bytes.extend_from_slice(&13u128.to_le_bytes());
        let mut out = Vec::new();
        kernel.collect_equal_value_indices(&bytes, &13u128.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 2]);
    }
}
