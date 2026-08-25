//! Distance kernels over stored byte spans.
//!
//! Stored rows are little-endian byte spans; the f32 kernels interpret the first
//! `query.len() * 4` bytes as `f32` values.
//!
//! Kernels are SIMD-accelerated on wasm32/wasm64 with `simd128` and fall back to scalar
//! otherwise. The 16-byte row alignment contract (`PageHeader::row_stride` multiple of 16,
//! `vector_bytes` offset aligned to 16) makes `v128` loads safe.

#[cfg(all(
    target_family = "wasm",
    target_arch = "wasm32",
    target_feature = "simd128"
))]
use core::arch::wasm32::{
    f32x4_add, f32x4_convert_i32x4, f32x4_extract_lane, f32x4_mul, f32x4_splat, f32x4_sub,
    i16x8_extend_high_i8x16, i16x8_extend_low_i8x16, i32x4_extend_high_i16x8,
    i32x4_extend_low_i16x8, v128, v128_load,
};
#[cfg(all(
    target_family = "wasm",
    target_arch = "wasm64",
    target_feature = "simd128"
))]
use core::arch::wasm64::{
    f32x4_add, f32x4_convert_i32x4, f32x4_extract_lane, f32x4_mul, f32x4_splat, f32x4_sub,
    i16x8_extend_high_i8x16, i16x8_extend_low_i8x16, i32x4_extend_high_i16x8,
    i32x4_extend_low_i16x8, v128, v128_load,
};

/// Counts matching bits between two code-word byte spans (`XNOR` + `popcount` over whole
/// little-endian `u64` words). Both spans must be the same non-zero, multiple-of-8 length (the
/// stored per-row code width); a mismatch is a caller bug and panics fail-closed. This is the
/// physical bit-op of the two-tier precision first-stage estimator — the estimator math that
/// turns the count into a distance stays in the domain layer.
///
/// Returns the number of matching bits in `[0, 8 × bytes.len()]`.
pub fn popcount_xnor_words(a: &[u8], b: &[u8]) -> u32 {
    debug_assert_eq!(a.len(), b.len(), "code spans must be equal width");
    debug_assert!(a.len().is_multiple_of(8), "code spans must be word-granular");
    let mut matched = 0u32;
    for (wa, wb) in a.as_chunks::<8>().0.iter().zip(b.as_chunks::<8>().0) {
        let x = u64::from_le_bytes(*wa);
        let y = u64::from_le_bytes(*wb);
        matched += (!(x ^ y)).count_ones();
    }
    matched
}

/// Sums the four f32 lanes of `v`.
#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
#[inline(always)]
fn f32x4_sum4(v: v128) -> f32 {
    f32x4_extract_lane::<0>(v)
        + f32x4_extract_lane::<1>(v)
        + f32x4_extract_lane::<2>(v)
        + f32x4_extract_lane::<3>(v)
}

/// Widens the 16 signed i8 bytes at `bytes[block*16..]` to four f32 lanes, scaled by `k`.
#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
#[inline(always)]
fn i8_block_scaled(bytes: &[u8], block: usize, ksplat: v128) -> (v128, v128, v128, v128) {
    let v = unsafe { v128_load(bytes[block * 16..].as_ptr().cast()) };
    let lo16 = i16x8_extend_low_i8x16(v);
    let hi16 = i16x8_extend_high_i8x16(v);
    let a = i32x4_extend_low_i16x8(lo16);
    let b = i32x4_extend_high_i16x8(lo16);
    let c = i32x4_extend_low_i16x8(hi16);
    let d = i32x4_extend_high_i16x8(hi16);
    (
        f32x4_mul(f32x4_convert_i32x4(a), ksplat),
        f32x4_mul(f32x4_convert_i32x4(b), ksplat),
        f32x4_mul(f32x4_convert_i32x4(c), ksplat),
        f32x4_mul(f32x4_convert_i32x4(d), ksplat),
    )
}

/// L2-squared contribution of the 16 components in `bytes[block*16..]` against the query f32 block.
#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
#[inline(always)]
fn i8_l2_block(bytes: &[u8], query: &[f32], block: usize, ksplat: v128) -> v128 {
    let (va, vb, vc, vd) = i8_block_scaled(bytes, block, ksplat);
    let qa = unsafe { v128_load(query[block * 16..].as_ptr().cast()) };
    let qb = unsafe { v128_load(query[block * 16 + 4..].as_ptr().cast()) };
    let qc = unsafe { v128_load(query[block * 16 + 8..].as_ptr().cast()) };
    let qd = unsafe { v128_load(query[block * 16 + 12..].as_ptr().cast()) };
    let da = f32x4_sub(qa, va);
    let db = f32x4_sub(qb, vb);
    let dc = f32x4_sub(qc, vc);
    let dd = f32x4_sub(qd, vd);
    f32x4_add(
        f32x4_mul(da, da),
        f32x4_add(
            f32x4_mul(db, db),
            f32x4_add(f32x4_mul(dc, dc), f32x4_mul(dd, dd)),
        ),
    )
}

/// Dot-product contribution of the 16 components in `bytes[block*16..]` against the query f32 block.
#[cfg(all(target_family = "wasm", target_feature = "simd128"))]
#[inline(always)]
fn i8_dot_block(bytes: &[u8], query: &[f32], block: usize, ksplat: v128) -> v128 {
    let (va, vb, vc, vd) = i8_block_scaled(bytes, block, ksplat);
    let qa = unsafe { v128_load(query[block * 16..].as_ptr().cast()) };
    let qb = unsafe { v128_load(query[block * 16 + 4..].as_ptr().cast()) };
    let qc = unsafe { v128_load(query[block * 16 + 8..].as_ptr().cast()) };
    let qd = unsafe { v128_load(query[block * 16 + 12..].as_ptr().cast()) };
    f32x4_add(
        f32x4_mul(qa, va),
        f32x4_add(
            f32x4_mul(qb, vb),
            f32x4_add(f32x4_mul(qc, vc), f32x4_mul(qd, vd)),
        ),
    )
}

/// Squared L2 distance `Σ(q − v)²` over the first `query.len()` dims of `bytes`.
///
/// Requires `bytes.len() >= query.len() * 4`; extra bytes are ignored (the caller slices the
/// stored row span to `pad_stride_bytes`).
pub fn l2_squared_f32(bytes: &[u8], query: &[f32]) -> f32 {
    debug_assert!(bytes.len() >= query.len() * 4);
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        // Two accumulators to hide SIMD latency; one trailing block handled by `acc0`.
        let mut acc0 = f32x4_splat(0.0);
        let mut acc1 = f32x4_splat(0.0);
        let chunks = query.len() / 4;
        let mut i = 0;
        while i + 1 < chunks {
            let v0 = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q0 = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            let d0 = f32x4_sub(v0, q0);
            acc0 = f32x4_add(acc0, f32x4_mul(d0, d0));
            let v1 = unsafe { v128_load(bytes[(i + 1) * 16..].as_ptr().cast()) };
            let q1 = unsafe { v128_load(query[(i + 1) * 4..].as_ptr().cast()) };
            let d1 = f32x4_sub(v1, q1);
            acc1 = f32x4_add(acc1, f32x4_mul(d1, d1));
            i += 2;
        }
        if i < chunks {
            let v = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            let d = f32x4_sub(v, q);
            acc0 = f32x4_add(acc0, f32x4_mul(d, d));
        }
        let mut sum = f32x4_extract_lane::<0>(acc0)
            + f32x4_extract_lane::<1>(acc0)
            + f32x4_extract_lane::<2>(acc0)
            + f32x4_extract_lane::<3>(acc0)
            + f32x4_extract_lane::<0>(acc1)
            + f32x4_extract_lane::<1>(acc1)
            + f32x4_extract_lane::<2>(acc1)
            + f32x4_extract_lane::<3>(acc1);
        for (chunk, q) in bytes[chunks * 16..]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query[chunks * 4..].iter().copied())
        {
            let d = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) - q;
            sum += d * d;
        }
        sum
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query.iter().copied())
            .map(|(chunk, q)| {
                let d = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) - q;
                d * d
            })
            .sum()
    }
}

/// Squared L2 with dimension-blocked early exit: returns `None` as soon as the partial distance
/// strictly exceeds `threshold` (the row cannot beat the running k-th best), `Some(total)` when it
/// never does.
///
/// The query may be pre-arranged by the caller into blocks ordered by descending block norm
/// (HARMONY-style) so that pruning triggers earlier; monotone partial sums make the early exit
/// exact regardless of order. A non-finite `threshold` never triggers the exit (full sum returned).
pub fn l2_squared_f32_early_exit(bytes: &[u8], query: &[f32], threshold: f32) -> Option<f32> {
    debug_assert!(bytes.len() >= query.len() * 4);
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let mut sum = 0.0;
        let chunks = query.len() / 4;
        for i in 0..chunks {
            let v = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            let d = f32x4_sub(v, q);
            let squared = f32x4_mul(d, d);
            let block_sum = f32x4_extract_lane::<0>(squared)
                + f32x4_extract_lane::<1>(squared)
                + f32x4_extract_lane::<2>(squared)
                + f32x4_extract_lane::<3>(squared);
            sum += block_sum;
            if sum > threshold {
                return None;
            }
        }
        for (chunk, q) in bytes[chunks * 16..]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query[chunks * 4..].iter().copied())
        {
            let d = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) - q;
            sum += d * d;
            if sum > threshold {
                return None;
            }
        }
        // A non-finite component makes the accumulated sum NaN (or overflow to inf); a NaN sum never
        // triggers the strict `> threshold` early exit, so it reaches here and is skipped as a
        // non-finite row (fusing the caller's finiteness guard into this single pass).
        if !sum.is_finite() {
            return None;
        }
        Some(sum)
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        let mut sum = 0.0;
        for (chunk, q) in bytes.as_chunks::<4>().0.iter().zip(query.iter().copied()) {
            let d = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) - q;
            sum += d * d;
            if sum > threshold {
                return None;
            }
        }
        if !sum.is_finite() {
            return None;
        }
        Some(sum)
    }
}

/// Squared L2 distance `Σ(q − v)²` where `v` is an `I8`-quantized row with per-row `scale`:
/// `v_i = bytes[i] as i8 as f32 * scale / 127`. Reads `query.len()` i8 bytes (the stored payload;
/// any trailing pad is ignored). A `scale == 0` row (zero vector) dequantizes to all zeros.
pub fn l2_squared_i8_f32(bytes: &[u8], scale: f32, query: &[f32]) -> f32 {
    let k = scale / 127.0;
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let ksplat = f32x4_splat(k);
        let mut acc0 = f32x4_splat(0.0);
        let mut acc1 = f32x4_splat(0.0);
        let blocks = query.len() / 16;
        let mut i = 0;
        while i + 1 < blocks {
            acc0 = f32x4_add(acc0, i8_l2_block(bytes, query, i, ksplat));
            acc1 = f32x4_add(acc1, i8_l2_block(bytes, query, i + 1, ksplat));
            i += 2;
        }
        if i < blocks {
            acc0 = f32x4_add(acc0, i8_l2_block(bytes, query, i, ksplat));
        }
        let mut sum = f32x4_sum4(acc0) + f32x4_sum4(acc1);
        for (b, q) in bytes[blocks * 16..]
            .iter()
            .take(query.len() - blocks * 16)
            .zip(query[blocks * 16..].iter().copied())
        {
            let v = (*b as i8 as f32) * k;
            let d = v - q;
            sum += d * d;
        }
        sum
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        bytes
            .iter()
            .take(query.len())
            .zip(query.iter().copied())
            .map(|(b, q)| {
                let v = (*b as i8 as f32) * k;
                let d = v - q;
                d * d
            })
            .sum()
    }
}

/// Squared L2 with dimension-blocked early exit for an `I8` row (see [`l2_squared_f32_early_exit`]).
/// Monotone partial sums make the exit exact (checked at 16-block granularity in the SIMD path); a
/// non-finite `threshold` never triggers it.
pub fn l2_squared_i8_f32_early_exit(
    bytes: &[u8],
    scale: f32,
    query: &[f32],
    threshold: f32,
) -> Option<f32> {
    let k = scale / 127.0;
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let ksplat = f32x4_splat(k);
        let mut sum = 0.0;
        let blocks = query.len() / 16;
        for i in 0..blocks {
            sum += f32x4_sum4(i8_l2_block(bytes, query, i, ksplat));
            if sum > threshold {
                return None;
            }
        }
        for (b, q) in bytes[blocks * 16..]
            .iter()
            .take(query.len() - blocks * 16)
            .zip(query[blocks * 16..].iter().copied())
        {
            let v = (*b as i8 as f32) * k;
            let d = v - q;
            sum += d * d;
            if sum > threshold {
                return None;
            }
        }
        Some(sum)
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        let mut sum = 0.0;
        for (b, q) in bytes.iter().take(query.len()).zip(query.iter().copied()) {
            let v = (*b as i8 as f32) * k;
            let d = v - q;
            sum += d * d;
            if sum > threshold {
                return None;
            }
        }
        Some(sum)
    }
}

/// Dot product `Σ q·v` for an `I8` row with per-row `scale` (`v_i = bytes[i] as i8 as f32 * scale / 127`).
pub fn dot_i8_f32(bytes: &[u8], scale: f32, query: &[f32]) -> f32 {
    let k = scale / 127.0;
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let ksplat = f32x4_splat(k);
        let mut acc0 = f32x4_splat(0.0);
        let mut acc1 = f32x4_splat(0.0);
        let blocks = query.len() / 16;
        let mut i = 0;
        while i + 1 < blocks {
            acc0 = f32x4_add(acc0, i8_dot_block(bytes, query, i, ksplat));
            acc1 = f32x4_add(acc1, i8_dot_block(bytes, query, i + 1, ksplat));
            i += 2;
        }
        if i < blocks {
            acc0 = f32x4_add(acc0, i8_dot_block(bytes, query, i, ksplat));
        }
        let mut sum = f32x4_sum4(acc0) + f32x4_sum4(acc1);
        for (b, q) in bytes[blocks * 16..]
            .iter()
            .take(query.len() - blocks * 16)
            .zip(query[blocks * 16..].iter().copied())
        {
            sum += (*b as i8 as f32) * k * q;
        }
        sum
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        bytes
            .iter()
            .take(query.len())
            .zip(query.iter().copied())
            .map(|(b, q)| (*b as i8 as f32) * k * q)
            .sum()
    }
}

/// Dot product with an exact Cauchy-Schwarz early exit for cosine scoring of an `I8` row. The
/// dequantized row is only approximately unit-normalized (I8 quantization error), so the bound uses a
/// conservative `max_norm` upper bound on the row norm: the remaining contribution
/// `Σ_{i>=j} q_i·v_i ≤ suffix_norm[j] * norm(v[j..]) ≤ suffix_norm[j] * max_norm`. Returns `None` as
/// soon as `partial + suffix_norm[j] * max_norm` is below `dot_threshold` (the row cannot beat the
/// running k-th best), `Some(dot)` when it can. A non-finite `dot_threshold` never triggers the exit;
/// a non-finite component makes the partial dot NaN, which never triggers the strict `<` exit and is
/// caught by the fused finiteness check at the end.
pub fn dot_i8_f32_early_exit(
    bytes: &[u8],
    scale: f32,
    query: &[f32],
    suffix_norm: &[f32],
    max_norm: f32,
    dot_threshold: f32,
) -> Option<f32> {
    let k = scale / 127.0;
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let ksplat = f32x4_splat(k);
        let mut acc0 = f32x4_splat(0.0);
        let mut acc1 = f32x4_splat(0.0);
        let blocks = query.len() / 16;
        let early_exit_blocks = (blocks / 8).max(1) * 2;
        let mut i = 0;
        while i + 1 < blocks {
            acc0 = f32x4_add(acc0, i8_dot_block(bytes, query, i, ksplat));
            acc1 = f32x4_add(acc1, i8_dot_block(bytes, query, i + 1, ksplat));
            i += 2;
            if i % early_exit_blocks == 0 {
                let partial = f32x4_sum4(acc0) + f32x4_sum4(acc1);
                if partial + suffix_norm[i * 16] * max_norm < dot_threshold {
                    return None;
                }
            }
        }
        if i < blocks {
            acc0 = f32x4_add(acc0, i8_dot_block(bytes, query, i, ksplat));
        }
        let mut sum = f32x4_sum4(acc0) + f32x4_sum4(acc1);
        for (b, q) in bytes[blocks * 16..]
            .iter()
            .take(query.len() - blocks * 16)
            .zip(query[blocks * 16..].iter().copied())
        {
            sum += (*b as i8 as f32) * k * q;
        }
        if !sum.is_finite() {
            return None;
        }
        Some(sum)
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        let mut sum = 0.0;
        for (j, (b, q)) in bytes
            .iter()
            .take(query.len())
            .zip(query.iter().copied())
            .enumerate()
        {
            sum += (*b as i8 as f32) * k * q;
            if sum + suffix_norm[j + 1] * max_norm < dot_threshold {
                return None;
            }
        }
        if !sum.is_finite() {
            return None;
        }
        Some(sum)
    }
}

/// Dot product `Σ q·v` over the first `query.len()` dims of `bytes`.
pub fn dot_f32(bytes: &[u8], query: &[f32]) -> f32 {
    debug_assert!(bytes.len() >= query.len() * 4);
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let mut acc0 = f32x4_splat(0.0);
        let mut acc1 = f32x4_splat(0.0);
        let chunks = query.len() / 4;
        let mut i = 0;
        while i + 1 < chunks {
            let v0 = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q0 = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            acc0 = f32x4_add(acc0, f32x4_mul(v0, q0));
            let v1 = unsafe { v128_load(bytes[(i + 1) * 16..].as_ptr().cast()) };
            let q1 = unsafe { v128_load(query[(i + 1) * 4..].as_ptr().cast()) };
            acc1 = f32x4_add(acc1, f32x4_mul(v1, q1));
            i += 2;
        }
        if i < chunks {
            let v = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            acc0 = f32x4_add(acc0, f32x4_mul(v, q));
        }
        let mut sum = f32x4_extract_lane::<0>(acc0)
            + f32x4_extract_lane::<1>(acc0)
            + f32x4_extract_lane::<2>(acc0)
            + f32x4_extract_lane::<3>(acc0)
            + f32x4_extract_lane::<0>(acc1)
            + f32x4_extract_lane::<1>(acc1)
            + f32x4_extract_lane::<2>(acc1)
            + f32x4_extract_lane::<3>(acc1);
        for (chunk, q) in bytes[chunks * 16..]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query[chunks * 4..].iter().copied())
        {
            sum += f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q;
        }
        sum
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query.iter().copied())
            .map(|(chunk, q)| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q)
            .sum()
    }
}

/// Dot product with an exact Cauchy-Schwarz early exit for cosine scoring of **unit-normalized**
/// rows. Returns `None` as soon as the partial dot plus the query's remaining suffix norm cannot
/// reach `dot_threshold` (the row cannot beat the running k-th best), `Some(dot)` when it can.
///
/// `suffix_norm[j] = sqrt(Σ_{i>=j} q_i²)` is the norm of the query's remaining components (length
/// `query.len() + 1`, with `suffix_norm[query.len()] = 0`). Because the stored row is unit-normalized,
/// the remaining contribution `Σ_{i>=j} q_i·v_i` is bounded by `suffix_norm[j]` (Cauchy-Schwarz), so
/// `partial + suffix_norm[j]` is an upper bound on the final dot. If that bound is strictly below
/// `dot_threshold`, the final dot is too, and the row cannot be in the top-k. A non-finite
/// `dot_threshold` never triggers the exit (full dot returned). A non-finite component makes the
/// partial dot NaN, which never triggers the strict `<` exit and is caught by the fused finiteness
/// check at the end.
pub fn dot_f32_early_exit(
    bytes: &[u8],
    query: &[f32],
    suffix_norm: &[f32],
    dot_threshold: f32,
) -> Option<f32> {
    debug_assert!(bytes.len() >= query.len() * 4);
    debug_assert!(suffix_norm.len() > query.len());
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        // Two accumulators to hide SIMD latency (same structure as `dot_f32`), with the Cauchy-
        // Schwarz bound checked at an adaptive granularity (~1/8 of the blocks, min 2, a multiple of
        // 2 since the loop advances 2 blocks per iteration) so the full-dot path stays close to
        // `dot_f32` when the exit never triggers and small dims still get an early exit.
        // `suffix_norm[i*4]` is the norm of the query's remaining components after `i` blocks.
        let chunks = query.len() / 4;
        let early_exit_blocks = (chunks / 8).max(1) * 2;
        let mut acc0 = f32x4_splat(0.0);
        let mut acc1 = f32x4_splat(0.0);
        let mut i = 0;
        while i + 1 < chunks {
            let v0 = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q0 = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            acc0 = f32x4_add(acc0, f32x4_mul(v0, q0));
            let v1 = unsafe { v128_load(bytes[(i + 1) * 16..].as_ptr().cast()) };
            let q1 = unsafe { v128_load(query[(i + 1) * 4..].as_ptr().cast()) };
            acc1 = f32x4_add(acc1, f32x4_mul(v1, q1));
            i += 2;
            if i % early_exit_blocks == 0 {
                let partial = f32x4_sum4(acc0) + f32x4_sum4(acc1);
                if partial + suffix_norm[i * 4] < dot_threshold {
                    return None;
                }
            }
        }
        if i < chunks {
            let v = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            acc0 = f32x4_add(acc0, f32x4_mul(v, q));
        }
        let mut sum = f32x4_sum4(acc0) + f32x4_sum4(acc1);
        for (chunk, q) in bytes[chunks * 16..]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query[chunks * 4..].iter().copied())
        {
            sum += f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q;
        }
        if !sum.is_finite() {
            return None;
        }
        Some(sum)
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        let mut sum = 0.0;
        for (j, (chunk, q)) in bytes
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query.iter().copied())
            .enumerate()
        {
            sum += f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) * q;
            if sum + suffix_norm[j + 1] < dot_threshold {
                return None;
            }
        }
        if !sum.is_finite() {
            return None;
        }
        Some(sum)
    }
}

/// Dot product plus row norm² in one pass (`Σ q·v`, `Σ v²`) for cosine scoring of unnormalized
/// rows.
pub fn dot_and_norm2_f32(bytes: &[u8], query: &[f32]) -> (f32, f32) {
    debug_assert!(bytes.len() >= query.len() * 4);
    #[cfg(all(target_family = "wasm", target_feature = "simd128"))]
    {
        let mut dot_acc = f32x4_splat(0.0);
        let mut norm_acc = f32x4_splat(0.0);
        let chunks = query.len() / 4;
        for i in 0..chunks {
            let v = unsafe { v128_load(bytes[i * 16..].as_ptr().cast()) };
            let q = unsafe { v128_load(query[i * 4..].as_ptr().cast()) };
            dot_acc = f32x4_add(dot_acc, f32x4_mul(v, q));
            norm_acc = f32x4_add(norm_acc, f32x4_mul(v, v));
        }
        let mut dot = f32x4_extract_lane::<0>(dot_acc)
            + f32x4_extract_lane::<1>(dot_acc)
            + f32x4_extract_lane::<2>(dot_acc)
            + f32x4_extract_lane::<3>(dot_acc);
        let mut norm2 = f32x4_extract_lane::<0>(norm_acc)
            + f32x4_extract_lane::<1>(norm_acc)
            + f32x4_extract_lane::<2>(norm_acc)
            + f32x4_extract_lane::<3>(norm_acc);
        for (chunk, q) in bytes[chunks * 16..]
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query[chunks * 4..].iter().copied())
        {
            let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            dot += v * q;
            norm2 += v * v;
        }
        (dot, norm2)
    }
    #[cfg(not(all(target_family = "wasm", target_feature = "simd128")))]
    {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .zip(query.iter().copied())
            .fold((0.0, 0.0), |(dot, norm2), (chunk, q)| {
                let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                (dot + v * q, norm2 + v * v)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_row(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn popcount_xnor_counts_matching_bits() {
        // Identical words: every bit matches.
        let a = 0x0123_4567_89ab_cdefu64.to_le_bytes();
        assert_eq!(popcount_xnor_words(&a, &a), 64);
        // Complement words: no bit matches.
        let b = (!0x0123_4567_89ab_cdefu64).to_le_bytes();
        assert_eq!(popcount_xnor_words(&a, &b), 0);
        // Two words, mixed: count is additive.
        let two_a = [a.as_slice(), b.as_slice()].concat();
        let two_b = [a.as_slice(), a.as_slice()].concat();
        assert_eq!(popcount_xnor_words(&two_a, &two_b), 64);
        // All-zero spans match fully.
        let zeros = [0u8; 16];
        assert_eq!(popcount_xnor_words(&zeros, &zeros), 128);
    }

    #[test]
    #[should_panic(expected = "equal width")]
    fn popcount_xnor_rejects_width_mismatch() {
        let _ = popcount_xnor_words(&[0u8; 8], &[0u8; 16]);
    }

    /// Encodes an f32 vector to `I8` with the kernel's convention: `s = max|x|`, `i8_i = round(127*x/s)`
    /// clamped to [-127, 127], bytes stored as i8 bit patterns.
    fn i8_row(values: &[f32]) -> (Vec<u8>, f32) {
        let s = values.iter().copied().map(f32::abs).fold(0.0f32, f32::max);
        let bytes = if s == 0.0 {
            vec![0u8; values.len()]
        } else {
            values
                .iter()
                .map(|x| (((127.0 * x / s).round() as i32).clamp(-127, 127) as i8) as u8)
                .collect()
        };
        (bytes, s)
    }

    #[test]
    fn i8_kernels_match_naive_dequantized_f32() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let v: Vec<f32> = vec![2.0, 0.0, 3.0, 1.0, -1.0];
        let (bytes, scale) = i8_row(&v);
        let dec: Vec<f32> = bytes
            .iter()
            .map(|b| (*b as i8 as f32) * scale / 127.0)
            .collect();
        let expected_l2: f32 = q.iter().zip(&dec).map(|(q, v)| (q - v) * (q - v)).sum();
        let expected_dot: f32 = q.iter().zip(&dec).map(|(q, v)| q * v).sum();
        assert_eq!(l2_squared_i8_f32(&bytes, scale, &q), expected_l2);
        assert_eq!(dot_i8_f32(&bytes, scale, &q), expected_dot);
        // Early exit agrees with the full L2 when under the threshold.
        assert_eq!(
            l2_squared_i8_f32_early_exit(&bytes, scale, &q, f32::INFINITY),
            Some(expected_l2)
        );
        // Early exit stops when the threshold is beaten.
        assert_eq!(l2_squared_i8_f32_early_exit(&bytes, scale, &q, 0.0), None);
    }

    #[test]
    fn i8_kernel_zero_scale_and_padding_are_ignored() {
        // Zero vector: scale 0, all-zero i8 -> distance is the query's own norm.
        let q: Vec<f32> = vec![1.0, 2.0, 3.0];
        let zero = i8_row(&[0.0, 0.0, 0.0]);
        let expected: f32 = q.iter().map(|x| x * x).sum();
        assert_eq!(l2_squared_i8_f32(&zero.0, zero.1, &q), expected);
        // A padded row (payload then trailing garbage beyond `dims`) is scored over `dims` only.
        let v = i8_row(&[1.0, 2.0]);
        let mut padded = v.0.clone();
        padded.extend_from_slice(&[99u8; 16]); // padding that must be ignored
        assert_eq!(l2_squared_i8_f32(&padded, v.1, &q[..2]), {
            let dec: Vec<f32> =
                v.0.iter()
                    .map(|b| (*b as i8 as f32) * v.1 / 127.0)
                    .collect();
            q[..2]
                .iter()
                .zip(&dec)
                .map(|(q, v)| (q - v) * (q - v))
                .sum()
        });
    }

    #[test]
    fn i8_kernel_scores_exact_over_zero_padded_on_slab_row() {
        // On-slab I8 row shape for d = 17: a 17-byte payload zero-padded to the 32-byte aligned row
        // stride. A partial final block (one full 16-byte block plus one scalar tail byte) scores
        // exactly over the payload; the zero-filled pad lanes never contribute.
        let values: Vec<f32> = (0..17).map(|i| i as f32 - 8.0).collect();
        let q: Vec<f32> = (0..17).map(|i| 1.5 - 0.25 * i as f32).collect();
        let (payload, scale) = i8_row(&values);
        assert_eq!(payload.len(), 17);
        let mut padded = payload.clone();
        padded.resize(32, 0);
        let dec: Vec<f32> = payload
            .iter()
            .map(|b| (*b as i8 as f32) * scale / 127.0)
            .collect();
        let expected_l2: f32 = q.iter().zip(&dec).map(|(q, v)| (q - v) * (q - v)).sum();
        let expected_dot: f32 = q.iter().zip(&dec).map(|(q, v)| q * v).sum();
        assert_eq!(l2_squared_i8_f32(&padded, scale, &q), expected_l2);
        assert_eq!(dot_i8_f32(&padded, scale, &q), expected_dot);
        assert_eq!(
            l2_squared_i8_f32_early_exit(&padded, scale, &q, f32::INFINITY),
            Some(expected_l2)
        );
    }

    #[test]
    fn l2_squared_matches_naive() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let v: Vec<f32> = vec![2.0, 0.0, 3.0, 1.0, -1.0];
        let expected: f32 = q.iter().zip(v.iter()).map(|(q, v)| (q - v) * (q - v)).sum();
        assert_eq!(l2_squared_f32(&f32_row(&v), &q), expected);
        // Tail dims (not a multiple of 4) are handled.
        let q6: Vec<f32> = vec![0.0; 6];
        let v6: Vec<f32> = vec![1.0; 6];
        assert_eq!(l2_squared_f32(&f32_row(&v6), &q6), 6.0);
    }

    #[test]
    fn l2_early_exit_agrees_with_full_when_under_threshold() {
        let q = vec![0.0f32; 8];
        let v = vec![1.0f32; 8];
        assert_eq!(
            l2_squared_f32_early_exit(&f32_row(&v), &q, 100.0),
            Some(8.0)
        );
    }

    #[test]
    fn l2_early_exit_stops_when_threshold_beaten() {
        let q = vec![0.0f32; 8];
        let v = vec![1.0f32; 8];
        // Partial distance exceeds 1.0 after the first dimension block.
        assert_eq!(l2_squared_f32_early_exit(&f32_row(&v), &q, 1.0), None);
        // Exact tie does not trigger the strict-exceeds exit.
        assert_eq!(l2_squared_f32_early_exit(&f32_row(&v), &q, 8.0), Some(8.0));
    }

    #[test]
    fn l2_early_exit_nan_threshold_never_triggers() {
        let q = vec![0.0f32; 8];
        let v = vec![1.0f32; 8];
        assert_eq!(
            l2_squared_f32_early_exit(&f32_row(&v), &q, f32::NAN),
            Some(8.0)
        );
    }

    #[test]
    fn l2_early_exit_skips_non_finite_row() {
        let q = vec![0.0f32; 8];
        // A NaN component poisons the partial sum; even with an infinite threshold it must be
        // skipped (fused finiteness), never returned as a NaN distance.
        let v_nan = vec![f32::NAN, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        assert_eq!(
            l2_squared_f32_early_exit(&f32_row(&v_nan), &q, f32::INFINITY),
            None
        );
        // +inf component is likewise non-finite and skipped.
        let v_inf = vec![f32::INFINITY, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
        assert_eq!(
            l2_squared_f32_early_exit(&f32_row(&v_inf), &q, f32::INFINITY),
            None
        );
    }

    #[test]
    fn dot_matches_naive() {
        let q: Vec<f32> = vec![1.0, -2.0, 3.0];
        let v: Vec<f32> = vec![4.0, 5.0, 6.0];
        assert_eq!(dot_f32(&f32_row(&v), &q), 4.0 - 10.0 + 18.0);
    }

    fn suffix_norms(q: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; q.len() + 1];
        let mut acc = 0.0;
        for j in (0..q.len()).rev() {
            acc += q[j] * q[j];
            out[j] = acc.sqrt();
        }
        out
    }

    #[test]
    fn dot_early_exit_agrees_with_full_when_under_threshold() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let v: Vec<f32> = vec![0.5, 0.5, 0.5, 0.5];
        let sn = suffix_norms(&q);
        // dot = 5.0; a threshold below it never triggers the exit -> full dot returned.
        assert_eq!(dot_f32_early_exit(&f32_row(&v), &q, &sn, 4.0), Some(5.0));
        // `-INFINITY` (the production value when the heap is not full) likewise never triggers.
        assert_eq!(
            dot_f32_early_exit(&f32_row(&v), &q, &sn, f32::NEG_INFINITY),
            Some(5.0)
        );
    }

    #[test]
    fn dot_early_exit_stops_when_threshold_beaten() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        // Anti-correlated row: the max possible dot (partial + suffix norm) drops below 10.0 early.
        let v: Vec<f32> = vec![-1.0, -1.0, -1.0, -1.0];
        let sn = suffix_norms(&q);
        assert_eq!(dot_f32_early_exit(&f32_row(&v), &q, &sn, 10.0), None);
    }

    #[test]
    fn dot_early_exit_nan_threshold_never_triggers() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let v: Vec<f32> = vec![0.5, 0.5, 0.5, 0.5];
        let sn = suffix_norms(&q);
        assert_eq!(
            dot_f32_early_exit(&f32_row(&v), &q, &sn, f32::NAN),
            Some(5.0)
        );
    }

    #[test]
    fn dot_early_exit_skips_non_finite_row() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let sn = suffix_norms(&q);
        let v_nan = vec![f32::NAN, 1.0, 1.0, 1.0];
        assert_eq!(
            dot_f32_early_exit(&f32_row(&v_nan), &q, &sn, f32::INFINITY),
            None
        );
        let v_inf = vec![f32::INFINITY, 1.0, 1.0, 1.0];
        assert_eq!(
            dot_f32_early_exit(&f32_row(&v_inf), &q, &sn, f32::INFINITY),
            None
        );
    }

    #[test]
    fn dot_i8_early_exit_agrees_with_full_when_under_threshold() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let (bytes, scale) = i8_row(&[0.5, 0.5, 0.5, 0.5]);
        let sn = suffix_norms(&q);
        let full = dot_i8_f32(&bytes, scale, &q);
        // A threshold below the dot never triggers the exit -> full dot returned.
        assert_eq!(
            dot_i8_f32_early_exit(&bytes, scale, &q, &sn, 1.0, full - 1.0),
            Some(full)
        );
        // `-INFINITY` (heap not full) likewise never triggers.
        assert_eq!(
            dot_i8_f32_early_exit(&bytes, scale, &q, &sn, 1.0, f32::NEG_INFINITY),
            Some(full)
        );
    }

    #[test]
    fn dot_i8_early_exit_stops_when_threshold_beaten() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let (bytes, scale) = i8_row(&[-1.0, -1.0, -1.0, -1.0]);
        let sn = suffix_norms(&q);
        // Anti-correlated row: the max possible dot drops below 10.0 early.
        assert_eq!(
            dot_i8_f32_early_exit(&bytes, scale, &q, &sn, 1.0, 10.0),
            None
        );
    }

    #[test]
    fn dot_i8_early_exit_nan_threshold_never_triggers() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let (bytes, scale) = i8_row(&[0.5, 0.5, 0.5, 0.5]);
        let sn = suffix_norms(&q);
        let full = dot_i8_f32(&bytes, scale, &q);
        assert_eq!(
            dot_i8_f32_early_exit(&bytes, scale, &q, &sn, 1.0, f32::NAN),
            Some(full)
        );
    }

    #[test]
    fn dot_i8_early_exit_skips_non_finite_row() {
        let q: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let sn = suffix_norms(&q);
        // A NaN component poisons the dot; even with an infinite threshold it must be skipped.
        let (bytes_nan, scale_nan) = i8_row(&[f32::NAN, 1.0, 1.0, 1.0]);
        assert_eq!(
            dot_i8_f32_early_exit(&bytes_nan, scale_nan, &q, &sn, 1.0, f32::INFINITY),
            None
        );
        let (bytes_inf, scale_inf) = i8_row(&[f32::INFINITY, 1.0, 1.0, 1.0]);
        assert_eq!(
            dot_i8_f32_early_exit(&bytes_inf, scale_inf, &q, &sn, 1.0, f32::INFINITY),
            None
        );
    }

    #[test]
    fn dot_and_norm2_matches_naive() {
        let q: Vec<f32> = vec![1.0, 2.0];
        let v: Vec<f32> = vec![3.0, 4.0];
        assert_eq!(dot_and_norm2_f32(&f32_row(&v), &q), (11.0, 25.0));
    }
}
