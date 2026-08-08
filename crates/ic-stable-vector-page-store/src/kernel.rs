//! Distance kernels over stored byte spans.
//!
//! Stored rows are little-endian byte spans; the f32 kernels interpret the first
//! `query.len() * 4` bytes as `f32` values. Binary kernels operate on bit-packed bytes
//! (`dims` bits → `ceil(dims / 8)` bytes).
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
    f32x4_add, f32x4_extract_lane, f32x4_mul, f32x4_splat, f32x4_sub, v128_load,
};
#[cfg(all(
    target_family = "wasm",
    target_arch = "wasm64",
    target_feature = "simd128"
))]
use core::arch::wasm64::{
    f32x4_add, f32x4_extract_lane, f32x4_mul, f32x4_splat, f32x4_sub, v128_load,
};

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

/// Number of set bits in `bytes` (compiles to `i64.popcnt` on wasm).
pub fn popcount_bytes(bytes: &[u8]) -> u32 {
    let (chunks, tail) = bytes.as_chunks::<8>();
    let mut count = chunks
        .iter()
        .map(|c| u64::from_le_bytes(*c).count_ones())
        .sum::<u32>();
    for b in tail {
        count += b.count_ones();
    }
    count
}

/// `n11 = Σ popcnt(q ∧ v)` for the Bits01 convention (bit 1 = 0/1 entries).
pub fn bits01_and_popcount(q: &[u8], v: &[u8]) -> u32 {
    let len = q.len().min(v.len());
    let mut count = 0;
    let (qchunks, qtail) = q[..len].as_chunks::<8>();
    let (vchunks, vtail) = v[..len].as_chunks::<8>();
    count += qchunks
        .iter()
        .zip(vchunks.iter())
        .map(|(qc, vc)| (u64::from_le_bytes(*qc) & u64::from_le_bytes(*vc)).count_ones())
        .sum::<u32>();
    count += qtail
        .iter()
        .zip(vtail.iter())
        .map(|(qb, vb)| (qb & vb).count_ones())
        .sum::<u32>();
    count
}

/// `H = Σ popcnt(q ⊕ v)` for the Signs convention (bit 1 = −1, bit 0 = +1); cosine ≡ Hamming
/// order, so no square root is needed.
pub fn signs_xor_popcount(q: &[u8], v: &[u8]) -> u32 {
    let len = q.len().min(v.len());
    let mut count = 0;
    let (qchunks, qtail) = q[..len].as_chunks::<8>();
    let (vchunks, vtail) = v[..len].as_chunks::<8>();
    count += qchunks
        .iter()
        .zip(vchunks.iter())
        .map(|(qc, vc)| (u64::from_le_bytes(*qc) ^ u64::from_le_bytes(*vc)).count_ones())
        .sum::<u32>();
    count += qtail
        .iter()
        .zip(vtail.iter())
        .map(|(qb, vb)| (qb ^ vb).count_ones())
        .sum::<u32>();
    count
}

/// Bits01 cosine score `n11 · rq · rv`, where `rv = 1/√popcnt(v)` is derived from the row.
///
/// Returns `None` for a zero-popcount row (the cosine is undefined); the caller skips such rows
/// during search. `rq` is the precomputed query factor `1/√popcnt(q)`.
pub fn bits01_score(q: &[u8], v: &[u8], rq: f32) -> Option<f32> {
    let n11 = bits01_and_popcount(q, v);
    let v_count = popcount_bytes(v);
    if v_count == 0 {
        return None;
    }
    let rv = 1.0 / (v_count as f32).sqrt();
    Some(n11 as f32 * rq * rv)
}

/// Signs cosine score `1 − 2H/d` (`dims` = number of bits; cosine ≡ Hamming order).
pub fn signs_score(q: &[u8], v: &[u8], dims: u32) -> f32 {
    let h = signs_xor_popcount(q, v);
    1.0 - 2.0 * h as f32 / dims as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32_row(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
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
    fn dot_matches_naive() {
        let q: Vec<f32> = vec![1.0, -2.0, 3.0];
        let v: Vec<f32> = vec![4.0, 5.0, 6.0];
        assert_eq!(dot_f32(&f32_row(&v), &q), 4.0 - 10.0 + 18.0);
    }

    #[test]
    fn dot_and_norm2_matches_naive() {
        let q: Vec<f32> = vec![1.0, 2.0];
        let v: Vec<f32> = vec![3.0, 4.0];
        assert_eq!(dot_and_norm2_f32(&f32_row(&v), &q), (11.0, 25.0));
    }

    #[test]
    fn popcount_bytes_matches_naive() {
        // 0xFF → 8; 0x0F → 4; 0x03 → 2.
        let bytes = [0xFFu8, 0x0F, 0x03, 0x00];
        assert_eq!(popcount_bytes(&bytes), 14);
        // Longer input exercises the u64 chunk path plus tail.
        let long: Vec<u8> = (0..17).map(|i| if i % 2 == 0 { 0xFF } else { 0 }).collect();
        assert_eq!(popcount_bytes(&long), 9 * 8);
    }

    #[test]
    fn bits01_and_popcount_counts_set_bits_of_intersection() {
        let q = [0b1010_1010u8, 0b1111_0000];
        let v = [0b1100_1100u8, 0b0000_1111];
        // AND = 0b1000_1000, 0b0000_0000 → 2 set bits.
        assert_eq!(bits01_and_popcount(&q, &v), 2);
    }

    #[test]
    fn signs_xor_popcount_counts_hamming_distance() {
        let q = [0b0000_0000u8, 0b0000_0000];
        let v = [0b1111_0000u8, 0b0000_1111];
        // XOR = 0b1111_0000, 0b0000_1111 → 8 set bits.
        assert_eq!(signs_xor_popcount(&q, &v), 8);
    }

    #[test]
    fn bits01_score_matches_definition() {
        // q = 0b11 (2 bits), v = 0b11 (2 bits): n11 = 2, rq = 1/√2, rv = 1/√2 → 2 · 1/2 = 1.
        let q = [0b0000_0011u8];
        let v = [0b0000_0011u8];
        let rq = 1.0 / (2.0f32).sqrt();
        let score = bits01_score(&q, &v, rq).expect("nonzero row");
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bits01_score_rejects_zero_row() {
        let q = [0xFFu8];
        let v = [0x00u8];
        assert!(bits01_score(&q, &v, 1.0).is_none());
    }

    #[test]
    fn signs_score_matches_definition() {
        // q = 0, v = 0b1111_0000 (dims 8): H = 4 → 1 − 2·4/8 = 0.
        let q = [0x00u8];
        let v = [0b1111_0000u8];
        assert_eq!(signs_score(&q, &v, 8), 0.0);
        // Identical vectors: H = 0 → 1.
        assert_eq!(signs_score(&q, &q, 8), 1.0);
    }
}
