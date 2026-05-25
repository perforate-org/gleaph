//! Width-dispatched edge-value batch kernels.

use std::cmp::Ordering;

use gleaph_gql::ast::CmpOp;
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

    pub(crate) fn collect_matching_value_indices(
        &self,
        value_bytes: &[u8],
        op: CmpOp,
        needle: &[u8],
        out: &mut Vec<usize>,
    ) {
        if op == CmpOp::Eq {
            self.collect_equal_value_indices(value_bytes, needle, out);
            return;
        }
        let width = usize::from(self.width_code.byte_width());
        if width == 0 || needle.len() != width || value_bytes.len() % width != 0 {
            return;
        }
        if self.is_unsigned_integer_encoding() {
            match self.width_code {
                ValueWidthCode::W1 => collect_cmp_u8(value_bytes, op, needle[0], out),
                ValueWidthCode::W2 => collect_cmp_u16(value_bytes, op, needle, out),
                ValueWidthCode::W4 => collect_cmp_u32(value_bytes, op, needle, out),
                ValueWidthCode::W8 => collect_cmp_unsigned_scalar(value_bytes, op, needle, 8, out),
                _ => collect_cmp_fixed_width_bytes(value_bytes, op, needle, width, out),
            }
            return;
        }
        collect_cmp_fixed_width_bytes(value_bytes, op, needle, width, out);
    }

    fn is_unsigned_integer_encoding(&self) -> bool {
        matches!(
            self.encoding,
            EdgeValueEncoding::RawU8
                | EdgeValueEncoding::RawU16
                | EdgeValueEncoding::RawU32
                | EdgeValueEncoding::RawU64
                | EdgeValueEncoding::RawU128
                | EdgeValueEncoding::WeightRawU16
        )
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

fn collect_cmp_fixed_width_bytes(
    value_bytes: &[u8],
    op: CmpOp,
    needle: &[u8],
    width: usize,
    out: &mut Vec<usize>,
) {
    for (idx, bytes) in value_bytes.chunks_exact(width).enumerate() {
        let matches = match op {
            CmpOp::Eq => bytes == needle,
            CmpOp::Ne => bytes != needle,
            _ => false,
        };
        if matches {
            out.push(idx);
        }
    }
}

fn collect_cmp_unsigned_scalar(
    value_bytes: &[u8],
    op: CmpOp,
    needle: &[u8],
    width: usize,
    out: &mut Vec<usize>,
) {
    for (idx, bytes) in value_bytes.chunks_exact(width).enumerate() {
        let ord = bytes.iter().rev().cmp(needle.iter().rev());
        let matches = match op {
            CmpOp::Eq => ord.is_eq(),
            CmpOp::Ne => !ord.is_eq(),
            CmpOp::Lt => ord.is_lt(),
            CmpOp::Le => !ord.is_gt(),
            CmpOp::Gt => ord.is_gt(),
            CmpOp::Ge => !ord.is_lt(),
        };
        if matches {
            out.push(idx);
        }
    }
}

fn cmp_ord(ord: Ordering, op: CmpOp) -> bool {
    match op {
        CmpOp::Eq => ord.is_eq(),
        CmpOp::Ne => !ord.is_eq(),
        CmpOp::Lt => ord.is_lt(),
        CmpOp::Le => !ord.is_gt(),
        CmpOp::Gt => ord.is_gt(),
        CmpOp::Ge => !ord.is_lt(),
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

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_cmp_u8(value_bytes: &[u8], op: CmpOp, needle: u8, out: &mut Vec<usize>) {
    for (idx, value) in value_bytes.iter().copied().enumerate() {
        if cmp_ord(value.cmp(&needle), op) {
            out.push(idx);
        }
    }
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_cmp_u8(value_bytes: &[u8], op: CmpOp, needle: u8, out: &mut Vec<usize>) {
    use core::arch::wasm32::{
        i8x16_bitmask, i8x16_ne, i8x16_splat, u8x16_ge, u8x16_gt, u8x16_le, u8x16_lt, v128_load,
    };

    if op == CmpOp::Eq {
        collect_equal_w1(value_bytes, needle, out);
        return;
    }
    let needle_v = i8x16_splat(needle as i8);
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            let matched = match op {
                CmpOp::Eq => unreachable!(),
                CmpOp::Ne => i8x16_ne(values, needle_v),
                CmpOp::Lt => u8x16_lt(values, needle_v),
                CmpOp::Le => u8x16_le(values, needle_v),
                CmpOp::Gt => u8x16_gt(values, needle_v),
                CmpOp::Ge => u8x16_ge(values, needle_v),
            };
            i8x16_bitmask(matched)
        };
        push_masked_lanes(mask as u32, chunk_idx * 16, out);
    }
    let base = value_bytes.len() - chunks.remainder().len();
    for (idx, value) in chunks.remainder().iter().copied().enumerate() {
        if cmp_ord(value.cmp(&needle), op) {
            out.push(base + idx);
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

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_cmp_u16(value_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = u16::from_le_bytes([needle[0], needle[1]]);
    for (idx, bytes) in value_bytes.chunks_exact(2).enumerate() {
        let value = u16::from_le_bytes([bytes[0], bytes[1]]);
        if cmp_ord(value.cmp(&needle), op) {
            out.push(idx);
        }
    }
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_cmp_u16(value_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    use core::arch::wasm32::{
        i16x8_bitmask, i16x8_ne, i16x8_splat, u16x8_ge, u16x8_gt, u16x8_le, u16x8_lt, v128_load,
    };

    if op == CmpOp::Eq {
        collect_equal_w2(value_bytes, needle, out);
        return;
    }
    let needle_scalar = u16::from_le_bytes([needle[0], needle[1]]);
    let needle_v = i16x8_splat(needle_scalar as i16);
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            let matched = match op {
                CmpOp::Eq => unreachable!(),
                CmpOp::Ne => i16x8_ne(values, needle_v),
                CmpOp::Lt => u16x8_lt(values, needle_v),
                CmpOp::Le => u16x8_le(values, needle_v),
                CmpOp::Gt => u16x8_gt(values, needle_v),
                CmpOp::Ge => u16x8_ge(values, needle_v),
            };
            i16x8_bitmask(matched)
        };
        push_masked_lanes(mask as u32, chunk_idx * 8, out);
    }
    let base = (value_bytes.len() - chunks.remainder().len()) / 2;
    for (idx, bytes) in chunks.remainder().chunks_exact(2).enumerate() {
        let value = u16::from_le_bytes([bytes[0], bytes[1]]);
        if cmp_ord(value.cmp(&needle_scalar), op) {
            out.push(base + idx);
        }
    }
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

#[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
fn collect_cmp_u32(value_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    let needle = u32::from_le_bytes([needle[0], needle[1], needle[2], needle[3]]);
    for (idx, bytes) in value_bytes.chunks_exact(4).enumerate() {
        let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if cmp_ord(value.cmp(&needle), op) {
            out.push(idx);
        }
    }
}

#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
fn collect_cmp_u32(value_bytes: &[u8], op: CmpOp, needle: &[u8], out: &mut Vec<usize>) {
    use core::arch::wasm32::{
        i32x4_bitmask, i32x4_ne, i32x4_splat, u32x4_ge, u32x4_gt, u32x4_le, u32x4_lt, v128_load,
    };

    if op == CmpOp::Eq {
        collect_equal_w4(value_bytes, needle, out);
        return;
    }
    let needle_scalar = u32::from_le_bytes([needle[0], needle[1], needle[2], needle[3]]);
    let needle_v = i32x4_splat(needle_scalar as i32);
    let mut chunks = value_bytes.chunks_exact(16);
    for (chunk_idx, chunk) in chunks.by_ref().enumerate() {
        let mask = unsafe {
            let values = v128_load(chunk.as_ptr().cast());
            let matched = match op {
                CmpOp::Eq => unreachable!(),
                CmpOp::Ne => i32x4_ne(values, needle_v),
                CmpOp::Lt => u32x4_lt(values, needle_v),
                CmpOp::Le => u32x4_le(values, needle_v),
                CmpOp::Gt => u32x4_gt(values, needle_v),
                CmpOp::Ge => u32x4_ge(values, needle_v),
            };
            i32x4_bitmask(matched)
        };
        push_masked_lanes(mask as u32, chunk_idx * 4, out);
    }
    let base = (value_bytes.len() - chunks.remainder().len()) / 4;
    for (idx, bytes) in chunks.remainder().chunks_exact(4).enumerate() {
        let value = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if cmp_ord(value.cmp(&needle_scalar), op) {
            out.push(base + idx);
        }
    }
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
    collect_equal_fixed_width(value_bytes, needle, 16, out);
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

#[cfg(all(feature = "canbench", target_family = "wasm"))]
mod bench {
    use super::*;

    use canbench_rs::{bench, bench_fn};
    use gleaph_graph_kernel::entry::EdgeValueEncoding;
    use std::hint::black_box;

    const LANES: usize = 4096;

    fn w1_values() -> Vec<u8> {
        (0..LANES)
            .map(|i| {
                if i % 4 == 0 {
                    7u8
                } else {
                    100u8 + (i % 101) as u8
                }
            })
            .collect()
    }

    fn w2_values() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LANES * 2);
        for i in 0..LANES {
            let value = if i % 4 == 0 {
                7u16
            } else {
                1000u16 + (i % 251) as u16
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn w4_values() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LANES * 4);
        for i in 0..LANES {
            let value = if i % 4 == 0 {
                7u32
            } else {
                1000u32 + (i % 251) as u32
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn w8_values() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LANES * 8);
        for i in 0..LANES {
            let value = if i % 4 == 0 {
                7u64
            } else {
                1000u64 + (i % 251) as u64
            };
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn w16_values() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LANES * 16);
        let needle = 13u128.to_le_bytes();
        for i in 0..LANES {
            if i % 4 == 0 {
                bytes.extend_from_slice(&needle);
            } else {
                bytes.extend_from_slice(&((i as u128) << 64 | 0xa5a5_5a5a_u128).to_le_bytes());
            }
        }
        bytes
    }

    fn collect_equal_scalar(value_bytes: &[u8], needle: &[u8], width: usize, out: &mut Vec<usize>) {
        out.clear();
        for (idx, bytes) in value_bytes.chunks_exact(width).enumerate() {
            if bytes == needle {
                out.push(idx);
            }
        }
    }

    fn collect_equal_prepared(
        kernel: &PreparedEdgeValueBatchKernel,
        value_bytes: &[u8],
        needle: &[u8],
        out: &mut Vec<usize>,
    ) {
        out.clear();
        kernel.collect_equal_value_indices(value_bytes, needle, out);
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w1_scalar_4096_25pct() -> canbench_rs::BenchResult {
        let values = w1_values();
        let needle = [7u8];
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_scalar(black_box(&values), black_box(&needle), 1, &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w1_dispatch_4096_25pct() -> canbench_rs::BenchResult {
        let values = w1_values();
        let needle = [7u8];
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W1, EdgeValueEncoding::RawU8);
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_prepared(&kernel, black_box(&values), black_box(&needle), &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w2_scalar_4096_25pct() -> canbench_rs::BenchResult {
        let values = w2_values();
        let needle = 7u16.to_le_bytes();
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_scalar(black_box(&values), black_box(&needle), 2, &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w2_dispatch_4096_25pct() -> canbench_rs::BenchResult {
        let values = w2_values();
        let needle = 7u16.to_le_bytes();
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W2, EdgeValueEncoding::RawU16);
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_prepared(&kernel, black_box(&values), black_box(&needle), &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w4_scalar_4096_25pct() -> canbench_rs::BenchResult {
        let values = w4_values();
        let needle = 7u32.to_le_bytes();
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_scalar(black_box(&values), black_box(&needle), 4, &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w4_dispatch_4096_25pct() -> canbench_rs::BenchResult {
        let values = w4_values();
        let needle = 7u32.to_le_bytes();
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W4, EdgeValueEncoding::RawU32);
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_prepared(&kernel, black_box(&values), black_box(&needle), &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w8_scalar_4096_25pct() -> canbench_rs::BenchResult {
        let values = w8_values();
        let needle = 7u64.to_le_bytes();
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_scalar(black_box(&values), black_box(&needle), 8, &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w8_dispatch_4096_25pct() -> canbench_rs::BenchResult {
        let values = w8_values();
        let needle = 7u64.to_le_bytes();
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W8, EdgeValueEncoding::RawU64);
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_prepared(&kernel, black_box(&values), black_box(&needle), &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w16_scalar_4096_25pct() -> canbench_rs::BenchResult {
        let values = w16_values();
        let needle = 13u128.to_le_bytes();
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_scalar(black_box(&values), black_box(&needle), 16, &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
    }

    #[bench(raw)]
    fn bench_edge_value_batch_equal_w16_dispatch_4096_25pct() -> canbench_rs::BenchResult {
        let values = w16_values();
        let needle = 13u128.to_le_bytes();
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W16, EdgeValueEncoding::RawU128);
        let mut out = Vec::with_capacity(LANES / 4);

        bench_fn(|| {
            collect_equal_prepared(&kernel, black_box(&values), black_box(&needle), &mut out);
            assert_eq!(out.len(), LANES / 4);
            black_box(out.len())
        })
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
    fn w2_gt_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W2, EdgeValueEncoding::RawU16);
        let mut bytes = Vec::new();
        for value in [3u16, 7, 8, 11] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&bytes, CmpOp::Gt, &7u16.to_le_bytes(), &mut out);
        assert_eq!(out, vec![2, 3]);
    }

    #[test]
    fn w4_le_collects_matching_indices() {
        let kernel =
            PreparedEdgeValueBatchKernel::new(ValueWidthCode::W4, EdgeValueEncoding::RawU32);
        let mut bytes = Vec::new();
        for value in [3u32, 7, 8, 11] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mut out = Vec::new();
        kernel.collect_matching_value_indices(&bytes, CmpOp::Le, &7u32.to_le_bytes(), &mut out);
        assert_eq!(out, vec![0, 1]);
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
