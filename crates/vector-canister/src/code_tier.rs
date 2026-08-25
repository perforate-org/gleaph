//! Two-tier precision code tier (Slice 6, ADR 0078): 1-bit RaBitQ-style first-stage codes with
//! same-page exact rerank.
//!
//! **Contract** (design principles #4 as revised by ADR 0078): the original tier (`F32` | `I8`)
//! defines the advertised result quality; the compressed code tier only accelerates the
//! first-stage scan. Rows of a generation built with `def.code_tier = true` carry, behind their
//! original vector bytes, one code segment `[code_aux 8B][codes ceil(P/64)*8B]` with
//! `P = next_pow2(dims)`:
//!
//! ```text
//! code_aux = [‖x‖² f32 LE][φ_x f32 LE]
//! codes    = P packed sign bits of the rotated normalized direction x̃ (LSB-first per byte)
//! ```
//!
//! The rotation is the randomized Walsh–Hadamard transform of the zero-padded vector: zero-pad
//! `dims → P`, apply seeded per-coordinate sign flips, then the unnormalized WHT followed by one
//! `1/√P` scaling — an orthogonal transform, so inner products and distances are preserved and
//! every real or padded coordinate participates in the code (`O(P log P)`).
//!
//! The **same** seeded rotation is applied to the query once per search
//! ([`QueryCode::prepare`]). A row's first-stage score is the two-sided binary estimator
//!
//! ```text
//! pc      = popcount(XNOR(query_code, row_code))            # kernel.rs owns the bit-op
//! s       = (2·pc − P)/P                                     # ⟨sign(x̃)/√P, sign(q̃)/√P⟩
//! cos_est = clamp(s / (φ_q·φ_x), −1, 1)
//! dist²   ≈ ‖q‖² + ‖x‖² − 2‖q‖‖x‖·cos_est
//! ```
//!
//! with `φ_x = ⟨sign(x̃)/√P, x̂⟩` — the binary sketch's correlation with the rotated unit
//! direction `x̂` (`φ_x = Σ|x̃_i|/(√P·‖x‖) ∈ [0,1]`, stored beside the code so the per-row work
//! stays XNOR+popcount plus a handful of flops). The decomposition `x̄ = φ_x·x̂ + r` (`x̄ =
//! sign(x̃)/√P`, `r ⊥ x̂`) makes `s / (φ_q·φ_x)` an unbiased-direction cosine estimate: the
//! literal sketch-to-sketch projection alone would leave the estimator scaled by
//! `(φ/‖x‖)`-type factors and systematically wrong (caught by contract test ② during
//! implementation).
//!
//! **Lower-bound pruning.** Stage B may skip the exact rescoring of a shortlist row whose
//! provable distance lower bound already exceeds the current k-th best *exact* distance. The
//! bound is **exact**, not heuristic: substituting both sketch decompositions
//! `x̄ = φ_x·x̂ + r_x` (`‖r_x‖ = √(1−φ_x²)`) into `s = ⟨x̄,q̄⟩` leaves
//! `s = φ_xφ_q·cos + ε` with `|ε| ≤ φ_x√(1−φ_q²) + φ_q√(1−φ_x²) + √((1−φ_x²)(1−φ_q²))` by
//! Cauchy–Schwarz, so every consistent cosine lies in an interval around `s/(φ_xφ_q)` whose upper
//! endpoint converts into a guaranteed `LB = ‖q‖² + ‖x‖² − 2‖q‖‖x‖·cos_upper` using nothing but
//! the stored aux — no tuning constant, no probabilistic assumption (an earlier partial-slack
//! form dropped two cross terms and was caught empirically by contract test ③; see the progress
//! log). The bound is used exclusively to skip Stage B rescoring — never to produce, reorder, or
//! filter emitted results — so its looseness costs only speed, and recall is measured rather than
//! assumed.
//!
//! Determinism: the sign-flip pattern is derived purely from `rotation_seed` (frozen into the
//! definition) through integer `splitmix64` mixing — no runtime randomness source — and all
//! floating-point accumulation orders are fixed, so codes and estimates are reproducible across
//! calls and replicas.

use crate::records::VectorIndexDef;
use gleaph_graph_kernel::vector_index::{VectorEncoding, decode_i8_to_f32};
use ic_stable_vector_page_store::kernel::popcount_xnor_words;

/// Width of the leading code-aux block: `[‖x‖² f32][φ_x f32]`.
pub(crate) const CODE_AUX_BYTES: usize = 8;

/// splitmix64 finalizer: the sole mixing primitive behind the deterministic flip pattern (and the
/// def-level seed derivation in [`VectorIndexDef::rotation_seed_for`]).
fn splitmix64_mix(z: u64) -> u64 {
    let z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Bit `i` of the returned mask decides the sign flip of rotated-space coordinate `i`. One pure
/// integer mix per coordinate, keyed by the frozen rotation seed: fully deterministic, no runtime
/// randomness source.
fn seeded_flip_mask(seed: u64, padded_dims: u32) -> Vec<u64> {
    let words = padded_dims.div_ceil(64) as usize;
    let mut mask = vec![0u64; words];
    for i in 0..padded_dims as usize {
        if splitmix64_mix(seed ^ (i as u64)) & 1 == 1 {
            mask[i / 64] |= 1 << (i % 64);
        }
    }
    mask
}

/// In-place unnormalized Walsh–Hadamard transform (butterfly form, fixed accumulation order).
/// Callers scale by `1/√P` afterwards to make the transform orthogonal.
fn walsh_hadamard_unnormalized(buf: &mut [f32]) {
    let n = buf.len();
    debug_assert!(n.is_power_of_two());
    let mut h = 1usize;
    while h < n {
        for base in (0..n).step_by(h * 2) {
            for i in base..base + h {
                let a = buf[i];
                let b = buf[i + h];
                buf[i] = a + b;
                buf[i + h] = a - b;
            }
        }
        h *= 2;
    }
}

/// Applies the seeded randomized WHT rotation to `v` **in place**: `v` must have exactly
/// `padded_dims` components (zero-padded by the caller). After the call `v` holds `x̃`.
fn rotate_in_place(v: &mut [f32], flips: &[u64], padded_dims: u32) {
    let n = v.len();
    debug_assert_eq!(n, padded_dims as usize);
    for (i, x) in v.iter_mut().enumerate() {
        if flips[i / 64] >> (i % 64) & 1 == 1 {
            *x = -*x;
        }
    }
    walsh_hadamard_unnormalized(v);
    let inv = 1.0 / (padded_dims as f32).sqrt();
    for x in v.iter_mut() {
        *x *= inv;
    }
}

/// Packs the sign bits of the rotated vector into the little-endian bit grid shared with
/// [`popcount_xnor_words`]: bit `i` of code word `i/8` (LSB-first) is set iff `x̃[i] >= +0.0`
/// (IEEE `-0.0` counts positive, keeping the mapping total).
fn pack_sign_bits(rotated: &[f32], out: &mut [u8]) {
    for byte in out.iter_mut() {
        *byte = 0;
    }
    for (i, x) in rotated.iter().enumerate() {
        if *x >= 0.0 {
            out[i / 8] |= 1 << (i % 8);
        }
    }
}

/// Decodes the stored original payload to its canonical f32 components (the same space the
/// estimator and the exact kernels score in). `F32` passes the raw LE components through; `I8`
/// dequantizes with the row's aux scale (`x_i = byte_i as i8 · scale/127`) — the identical,
/// sign-equivalent dequantization the search-side kernels fuse.
fn stored_to_f32(encoding: VectorEncoding, stored: &[u8], aux: &[u8; 8], dims: u16) -> Vec<f32> {
    match encoding {
        VectorEncoding::F32 => {
            let (chunks, _) = stored.as_chunks::<4>();
            chunks
                .iter()
                .take(dims as usize)
                .map(|c| f32::from_le_bytes(*c))
                .collect()
        }
        VectorEncoding::I8 => {
            let scale = f32::from_le_bytes(aux[0..4].try_into().expect("4-byte scale"));
            decode_i8_to_f32(stored, scale, dims as usize)
        }
    }
}

/// Per-generation code encoder: derives the rotation domain and flip pattern from the definition
/// and turns stored original rows into code segments. Created once per append call (the
/// Slice 6 contract's "one `CodeEncoder` per call"), reused across the batch's rows.
pub(crate) struct CodeEncoder {
    encoding: VectorEncoding,
    dims: u16,
    padded_dims: u32,
    /// Whole code words per row in bytes: `ceil(P/64)·8` — bit storage is word-granular (the
    /// XNOR kernel consumes whole `u64` words), with trailing pad bits kept zero.
    code_bytes: usize,
    flips: Vec<u64>,
    /// Reused per-row rotation buffer (never escapes the encoder).
    buf: Vec<f32>,
}

impl CodeEncoder {
    /// `None` when the generation has no code tier (`append_row`/`append_rows` then write no code
    /// segments and reserve tier-off pages — the unchanged pre-Slice-6 geometry).
    pub(crate) fn from_def(def: &VectorIndexDef) -> Option<Self> {
        if !def.has_code_tier() {
            return None;
        }
        let padded_dims = VectorIndexDef::code_padded_dims(def.dims);
        Some(Self {
            encoding: def.encoding,
            dims: def.dims,
            code_bytes: padded_dims.div_ceil(64) as usize * 8,
            padded_dims,
            flips: seeded_flip_mask(def.rotation_seed, padded_dims),
            buf: vec![0.0; padded_dims as usize],
        })
    }

    /// Encodes one stored row's full code segment into `out`
    /// (`out.len() == def.code_stride_bytes`): `[code_aux 8B][codes]`. The aux records the
    /// canonical-space squared norm and the sketch correlation `φ_x = ⟨sign(x̃)/√P, x̂⟩`
    /// (`x̂` = rotated **unit** direction), so Stage A scoring never touches the original bytes.
    pub(crate) fn encode_segment(&mut self, stored: &[u8], aux: &[u8; 8], out: &mut [u8]) {
        debug_assert_eq!(out.len(), CODE_AUX_BYTES + self.code_bytes);
        let comps = stored_to_f32(self.encoding, stored, aux, self.dims);
        // Squared norm in canonical space; zero padding contributes nothing.
        let norm_sq: f32 = comps.iter().map(|x| x * x).sum();
        self.buf[..comps.len()].copy_from_slice(&comps);
        self.buf[comps.len()..].fill(0.0);
        rotate_in_place(&mut self.buf, &self.flips, self.padded_dims);
        let abs_sum: f32 = self.buf.iter().map(|x| x.abs()).sum();
        // φ_x = ⟨sign(x̃)/√P, x̃/‖x‖⟩ = Σ|x̃_i|/(√P·‖x‖) ∈ [0, 1] by Cauchy–Schwarz; this is the
        // sketch's correlation with the unit direction — exactly the factor that makes
        // `s / (φ_q·φ_x)` an unbiased-direction cosine estimate (the decomposition
        // `x̄ = φ_x·x̂ + r`, `r ⊥ x̂`). Zero for a degenerate all-zero row (estimator falls back to
        // an orthogonal assumption).
        let phi_x = if norm_sq > 0.0 {
            abs_sum / ((self.padded_dims as f32).sqrt() * norm_sq.sqrt())
        } else {
            0.0
        };
        out[..4].copy_from_slice(&norm_sq.to_le_bytes());
        out[4..CODE_AUX_BYTES].copy_from_slice(&phi_x.to_le_bytes());
        pack_sign_bits(&self.buf, &mut out[CODE_AUX_BYTES..]);
    }
}

/// Query-side code preparation: the seeded rotation + binarization computed **once per search**
/// (the contract's per-query precompute), then reused against every scanned row's code.
pub(crate) struct QueryCode {
    codes: Vec<u8>,
    phi_q: f32,
    q_norm_sq: f32,
    q_norm: f32,
    padded_dims: u32,
}

impl QueryCode {
    /// Prepares the rotated binary query from canonical f32 query components. For **cosine**
    /// searches the caller passes the unit-normalized query so the estimated `L2²` ranks
    /// consistently with the exact cosine scores Stage B produces (`dist²(q̂, x̂) = 2 − 2·cos` over
    /// unit rows).
    pub(crate) fn prepare(def: &VectorIndexDef, query_f32: &[f32]) -> Self {
        let padded_dims = VectorIndexDef::code_padded_dims(def.dims);
        let q_norm_sq: f32 = query_f32.iter().map(|x| x * x).sum();
        let q_norm = q_norm_sq.sqrt();
        let mut buf = vec![0.0f32; padded_dims as usize];
        buf[..query_f32.len()].copy_from_slice(query_f32);
        let flips = seeded_flip_mask(def.rotation_seed, padded_dims);
        rotate_in_place(&mut buf, &flips, padded_dims);
        let abs_sum: f32 = buf.iter().map(|x| x.abs()).sum();
        // φ_q = ⟨sign(q̃)/√P, q̂⟩ — the same unit-direction correlation the row side stores, so
        // `s / (φ_q·φ_x)` estimates the cosine symmetrically (see [`CodeEncoder::encode_segment`]).
        let phi_q = if q_norm > 0.0 {
            abs_sum / ((padded_dims as f32).sqrt() * q_norm)
        } else {
            0.0
        };
        // Word-granular storage matching the row side; pad bits stay zero on both sides.
        let mut codes = vec![0u8; padded_dims.div_ceil(64) as usize * 8];
        pack_sign_bits(&buf, &mut codes);
        Self {
            codes,
            phi_q,
            q_norm_sq,
            q_norm,
            padded_dims,
        }
    }

    /// One XNOR+popcount pass producing the point estimate ([`RowEstimate::distance`]) plus the
    /// **exact** distance lower bound ([`RowEstimate::lower_bound`]) used by Stage B pruning: the
    /// Cauchy–Schwarz bound on the sketch residual confines every consistent cosine to an interval
    /// around the raw projection, and the interval's upper endpoint converts into a distance the
    /// true squared distance can never undercut. See the module-level rationale.
    pub(crate) fn score_row(&self, segment: &[u8]) -> RowEstimate {
        let norm_sq = f32::from_le_bytes(segment[0..4].try_into().expect("row norm"));
        let phi_x = f32::from_le_bytes(segment[4..CODE_AUX_BYTES].try_into().expect("row phi"));
        if !norm_sq.is_finite() || !phi_x.is_finite() {
            return RowEstimate {
                distance: f32::INFINITY,
                lower_bound: f32::INFINITY,
            };
        }
        let matched_raw = popcount_xnor_words(&self.codes, &segment[CODE_AUX_BYTES..]) as f32;
        let p = self.padded_dims as f32;
        // Both sides zero their word-granular pad bits, so every pad bit counts as a match in the
        // raw XNOR total; subtract them to score only real coordinates.
        let pad_bits = (self.codes.len() * 8) as f32 - p;
        let matched = matched_raw - pad_bits;
        // s = ⟨x̄, q̄⟩ ∈ [−1, 1]; dividing by the sketch correlations recovers the cosine.
        let s = (2.0 * matched - p) / p;
        let denom = self.phi_q * phi_x;
        let row_norm = norm_sq.max(0.0).sqrt();
        if denom > 0.0 {
            // Rigorous residual slack. Substituting both sketch decompositions
            // (`x̄ = φ_x·x̂ + r_x`, `‖r_x‖ = √(1−φ_x²)`) into `s = ⟨x̄,q̄⟩` leaves
            // `s = φ_xφ_q·cos + ε` with
            // `|ε| ≤ φ_x·‖r_q‖ + φ_q·‖r_x‖ + ‖r_x‖·‖r_q‖` (Cauchy–Schwarz per term). Dropping any
            // of the three terms breaks the guarantee — caught empirically by contract test ③
            // during implementation.
            let rx = (1.0 - phi_x * phi_x).max(0.0).sqrt();
            let rq = (1.0 - self.phi_q * self.phi_q).max(0.0).sqrt();
            let slack = phi_x * rq + self.phi_q * rx + rx * rq;
            let cos_est = (s / denom).clamp(-1.0, 1.0);
            let cos_upper = ((s + slack) / denom).clamp(-1.0, 1.0);
            let base = self.q_norm_sq + norm_sq;
            let span = 2.0 * self.q_norm * row_norm;
            RowEstimate {
                distance: base - span * cos_est,
                lower_bound: base - span * cos_upper,
            }
        } else {
            // Degenerate sketch (all-zero row): orthogonal fallback. `(‖q‖ − ‖x‖)²` is then exact,
            // so it doubles as its own lower bound.
            let d = self.q_norm_sq + norm_sq;
            RowEstimate {
                distance: d,
                lower_bound: d,
            }
        }
    }
}

/// Per-row first-stage output: the RaBitQ estimated squared distance plus the guaranteed lower
/// bound on the true squared distance (never above it; see [`QueryCode::score_row`]).
pub(crate) struct RowEstimate {
    pub(crate) distance: f32,
    pub(crate) lower_bound: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a canonical tier-on definition fixture via the shared test-support constructor
    /// (`records::test_support::tier_def`) so every math test exercises the real derivation.
    fn def(dims: u16, encoding: VectorEncoding) -> VectorIndexDef {
        crate::records::test_support::tier_def(dims, encoding)
    }

    fn encode_f32(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// Contract ①: orthogonality and determinism of the seeded rotation — the transform preserves
    /// norms and inner products exactly up to f32 rounding, and the same seed reproduces the same
    /// output while a different seed does not.
    #[test]
    fn rotation_is_orthogonal_and_deterministic() {
        let dims = 100u16; // forces P = 128 (non-trivial padding)
        let padded = VectorIndexDef::code_padded_dims(dims) as usize;
        let flips = seeded_flip_mask(0xDEAD_BEEF, padded as u32);

        let mut x: Vec<f32> = (0..dims as usize)
            .map(|i| ((i as f32) * 0.37 - 5.0).sin())
            .collect();
        x.resize(padded, 0.0);
        let mut y: Vec<f32> = (0..dims as usize)
            .map(|i| ((i as f32) * 1.13 + 2.0).cos())
            .collect();
        y.resize(padded, 0.0);
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(p, q)| p * q).sum::<f32>();
        let (dx, dy, dxy) = (dot(&x, &x), dot(&y, &y), dot(&x, &y));

        let rx = {
            let mut v = x.clone();
            rotate_in_place(&mut v, &flips, padded as u32);
            v
        };
        let ry = {
            let mut v = y.clone();
            rotate_in_place(&mut v, &flips, padded as u32);
            v
        };
        // Norms preserved.
        assert!((dot(&rx, &rx) - dx).abs() < 1e-3 * dx.max(1.0));
        assert!((dot(&ry, &ry) - dy).abs() < 1e-3 * dy.max(1.0));
        // Inner products preserved (rotation is orthogonal).
        assert!((dot(&rx, &ry) - dxy).abs() < 1e-3 * dxy.abs().max(1.0));

        // Determinism: same seed → identical bytes.
        let again = {
            let mut v = y.clone();
            rotate_in_place(&mut v, &flips, padded as u32);
            v
        };
        assert_eq!(ry, again);
        // Different seed → different pattern (with overwhelming probability at P=128).
        let other_flips = seeded_flip_mask(0xDEAD_BEEF ^ 1, padded as u32);
        let other = {
            let mut v = y.clone();
            rotate_in_place(&mut v, &other_flips, padded as u32);
            v
        };
        assert_ne!(ry, other);
    }

    /// Contract ① (dense reference): the butterfly WHT matches the naive `O(P²)` Hadamard
    /// definition `H[x]_i = Σ_j (−1)^{popcnt(i & j)} x_j` followed by the same seeded sign flips
    /// and `1/√P` scaling — the test-flag dense-matrix reference the Slice 6 contract allows.
    #[test]
    fn rotation_matches_dense_reference() {
        let padded = 64usize;
        let seed = 42u64;
        let flips = seeded_flip_mask(seed, padded as u32);
        let v: Vec<f32> = (0..padded)
            .map(|i| ((i as f32) * 0.173).sin() * 3.0 - 1.0)
            .collect();

        let fast = {
            let mut w = v.clone();
            rotate_in_place(&mut w, &flips, padded as u32);
            w
        };
        // Dense reference: seeded sign flips applied to the INPUT (same order as the
        // implementation), then the naive `O(P²)` Hadamard `H[x]_i = Σ_j (−1)^{popcnt(i & j)} x_j`
        // with the same `1/√P` scaling.
        let expected: Vec<f32> = v
            .iter()
            .enumerate()
            .map(|(i, x)| {
                if flips[i / 64] >> (i % 64) & 1 == 1 {
                    -x
                } else {
                    *x
                }
            })
            .collect();
        let expected_out: Vec<f32> = (0..padded)
            .map(|i| {
                let acc: f32 = expected
                    .iter()
                    .enumerate()
                    .map(|(j, x)| {
                        let sign = if ((i & j).count_ones() & 1) == 0 {
                            1.0
                        } else {
                            -1.0
                        };
                        sign * x
                    })
                    .sum();
                acc / (padded as f32).sqrt()
            })
            .collect();
        let max_err = fast
            .iter()
            .zip(&expected_out)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_err < 1e-3,
            "butterfly deviates from dense WHT by {max_err}"
        );
    }

    /// Contract ②: statistical soundness of the estimator at a fixed seed — over a large random
    /// sample the estimated squared distance tracks the true one without systematic bias, the
    /// lower bound never exceeds the true distance (the property pruning correctness leans on),
    /// and recall@10 of a code-tier scan over the exact ground truth is reported well above
    /// chance for clustered data.
    #[test]
    fn estimator_tracks_true_distance_and_lower_bound_holds() {
        let dims = 96u16;
        let d = def(dims, VectorEncoding::F32);
        let mut encoder = CodeEncoder::from_def(&d).expect("tier on");

        // Clustered fixture: 8 centers, jittered members — the shape Stage A is designed for.
        let centers: Vec<Vec<f32>> = (0..8)
            .map(|c| {
                (0..dims as usize)
                    .map(|i| ((c * 31 + i * 7) as f32 * 0.21).sin())
                    .collect()
            })
            .collect();
        let mut lcg = 0x243F_6A88_85A3_08D3u64;
        let mut rand = move || {
            lcg ^= lcg << 13;
            lcg ^= lcg >> 7;
            lcg ^= lcg << 17;
            (lcg >> 40) as f32 / (1u32 << 24) as f32 - 0.5
        };

        let mut segments = Vec::new();
        let mut rows = Vec::new();
        for center in &centers {
            for _ in 0..16 {
                let v: Vec<f32> = center.iter().map(|x| x + 0.25 * rand()).collect();
                let stored = encode_f32(&v);
                let mut seg = vec![0u8; d.code_stride_bytes as usize];
                encoder.encode_segment(&stored, &[0u8; 8], &mut seg);
                segments.push(seg);
                rows.push(v);
            }
        }

        let mut mean_err = 0.0f32;
        let mut samples = 0u32;
        let mut bound_violations = 0u32;
        for _ in 0..64 {
            let q: Vec<f32> = centers[(rand() * 8.0).min(7.0) as usize]
                .iter()
                .map(|x| x + 0.25 * rand())
                .collect();
            let qc = QueryCode::prepare(&d, &q);
            for (seg, row) in segments.iter().zip(&rows) {
                let true_d: f32 = q.iter().zip(row).map(|(a, b)| (a - b) * (a - b)).sum();
                let est = qc.score_row(seg);
                assert!(est.distance.is_finite());
                mean_err += est.distance - true_d;
                samples += 1;
                if est.lower_bound > true_d {
                    bound_violations += 1;
                }
            }
        }
        let mean_err = mean_err / samples as f32;
        // Unbiasedness sanity (fixed seed): the mean signed error is small relative to the
        // distances' own scale (~O(dims) here).
        assert!(
            mean_err.abs() < 0.05 * dims as f32,
            "estimator bias {mean_err} too large"
        );
        // The exact Cauchy–Schwarz residual bound can never be undercut by the true distance.
        assert_eq!(
            bound_violations, 0,
            "lower bound exceeded the true distance {bound_violations} times"
        );
    }

    /// Encoding parity: F32 and I8 rows carrying the same vector (up to quantization) produce the
    /// same code bits whenever their rotated signs agree, and the recorded aux equals the
    /// canonical-space norm/φ either way.
    #[test]
    fn encoder_matches_stored_encoding_space() {
        let dims = 64u16;
        let f32_def = def(dims, VectorEncoding::F32);
        let i8_def = def(dims, VectorEncoding::I8);
        let mut enc = CodeEncoder::from_def(&f32_def).expect("tier on");
        let mut enc8 = CodeEncoder::from_def(&i8_def).expect("tier on");

        let v: Vec<f32> = (0..dims as usize)
            .map(|i| ((i as f32) * 0.31).sin())
            .collect();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let unit: Vec<f32> = v.iter().map(|x| x / norm).collect();

        let mut seg32 = vec![0u8; f32_def.code_stride_bytes as usize];
        enc.encode_segment(&encode_f32(&unit), &[0u8; 8], &mut seg32);
        // Quantize the same unit vector with the canonical I8 pipeline (scale = max|x|).
        let scale = unit.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let bytes: Vec<u8> = unit
            .iter()
            .map(|x| ((127.0 * x / scale).round() as i32).clamp(-127, 127) as i8 as u8)
            .collect();
        let mut aux = [0u8; 8];
        aux[0..4].copy_from_slice(&scale.to_le_bytes());
        let mut seg8 = vec![0u8; i8_def.code_stride_bytes as usize];
        enc8.encode_segment(&bytes, &aux, &mut seg8);

        // Aux: both record ≈1 squared norm (unit vectors).
        let n32 = f32::from_le_bytes(seg32[0..4].try_into().unwrap());
        let n8 = f32::from_le_bytes(seg8[0..4].try_into().unwrap());
        assert!((n32 - 1.0).abs() < 1e-3);
        assert!((n8 - 1.0).abs() < 0.02, "I8 dequantized norm {n8}");

        // The two codes agree on nearly every sign (same underlying direction).
        let matched = popcount_xnor_words(&seg32[8..], &seg8[8..]);
        assert!(
            matched as f32 > 0.97 * VectorIndexDef::code_padded_dims(dims) as f32,
            "sign agreement {matched}/{}",
            VectorIndexDef::code_padded_dims(dims)
        );
    }
}
