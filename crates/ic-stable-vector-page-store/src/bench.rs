//! Isolated kernel and page-geometry benchmarks at the `d = 1536` design target.
//!
//! Run from `crates/ic-stable-vector-page-store`: `canbench` (see `canbench.yml`). These
//! benchmarks establish the raw per-row instruction cost of the scoring formulations before the
//! canister's search path composes them; the ADR 0064 measurement step compares formulations
//! against these baselines.

use std::hint::black_box;

use canbench_rs::bench;

use crate::header::PageHeader;
use crate::kernel::{dot_f32, l2_squared_f32, l2_squared_f32_early_exit};
use crate::layout::PageLayout;

/// Design-target dimensions.
const D: usize = 1536;
/// F32 pad stride at `d = 1536`: `ceil(1536 / 4) * 16`.
const D1536_PAD_STRIDE: u32 = 6144;

/// xorshift64 — deterministic pseudo-random fill (content is irrelevant, only cost matters).
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Masks a random `u32` down to the f32 mantissa and forces sign 0 / exponent 0x7F, so every
/// generated value is finite in `[1.0, 2.0)` (no NaN/Inf; NaN would defeat the early-exit
/// comparison in the L2 benches).
fn unitish(bits: u32) -> f32 {
    f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000)
}

fn f32_row_bytes(dims: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    (0..dims)
        .flat_map(|_| unitish(next_rand(&mut state) as u32).to_le_bytes())
        .collect()
}

fn query_f32(dims: usize) -> Vec<f32> {
    let mut state = 0x4D59_5DF4_D0F3_3173;
    (0..dims)
        .map(|_| unitish(next_rand(&mut state) as u32))
        .collect()
}

/// Full sub-square L2 on one `d = 1536` row (the default formulation).
#[bench(raw)]
fn bench_l2_full_d1536() -> canbench_rs::BenchResult {
    let q = black_box(query_f32(D));
    let v = black_box(f32_row_bytes(D));
    canbench_rs::bench_fn(|| {
        black_box(l2_squared_f32(&v, &q));
    })
}

/// Dimension-blocked early exit with a threshold the row beats early (measures the pruned path).
#[bench(raw)]
fn bench_l2_early_exit_d1536() -> canbench_rs::BenchResult {
    let q = black_box(query_f32(D));
    let v = black_box(f32_row_bytes(D));
    let threshold = black_box(0.5);
    canbench_rs::bench_fn(|| {
        black_box(l2_squared_f32_early_exit(&v, &q, threshold));
    })
}

/// Dot product on one `d = 1536` row (the opt-in `dot + norms` alternative).
#[bench(raw)]
fn bench_dot_d1536() -> canbench_rs::BenchResult {
    let q = black_box(query_f32(D));
    let v = black_box(f32_row_bytes(D));
    canbench_rs::bench_fn(|| {
        black_box(dot_f32(&v, &q));
    })
}

/// Page geometry + fail-closed header validation for a `d = 1536` page (per-page open cost).
#[bench(raw)]
fn bench_page_geometry_d1536() -> canbench_rs::BenchResult {
    let header =
        black_box(PageHeader::new(1024, D1536_PAD_STRIDE, 4, 8).expect("valid page header"));
    canbench_rs::bench_fn(|| {
        let layout = PageLayout::new(&header).expect("valid layout");
        black_box(layout.page_len());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_values_are_finite_in_unit_range() {
        // NaN/Inf would defeat the early-exit comparison in the L2 benches (NaN > threshold is
        // always false), so the generators must produce finite [1.0, 2.0) values only.
        for dims in [1, 4, 1536] {
            for v in query_f32(dims) {
                assert!(v.is_finite() && (1.0..2.0).contains(&v));
            }
            for chunk in f32_row_bytes(dims).as_chunks::<4>().0 {
                let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                assert!(v.is_finite() && (1.0..2.0).contains(&v));
            }
        }
    }
}
