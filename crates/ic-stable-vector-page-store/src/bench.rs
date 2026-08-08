//! Isolated kernel and page-geometry benchmarks at the `d = 1536` design target.
//!
//! Run from `crates/ic-stable-vector-page-store`: `canbench` (see `canbench.yml`). These
//! benchmarks establish the raw per-row instruction cost of the scoring formulations before the
//! canister's search path composes them; the ADR 0064 measurement step compares formulations
//! against these baselines.

use std::hint::black_box;

use canbench_rs::bench;

use crate::header::PageHeader;
use crate::kernel::{
    bits01_and_popcount, dot_f32, l2_squared_f32, l2_squared_f32_early_exit, popcount_bytes,
    signs_xor_popcount,
};
use crate::layout::PageLayout;

/// Design-target dimensions.
const D: usize = 1536;
/// F32 pad stride at `d = 1536`: `ceil(1536 / 4) * 16`.
const D1536_PAD_STRIDE: u32 = 6144;
/// Binary row bytes at `d = 1536`: `ceil(1536 / 8)`.
const D1536_BINARY_BYTES: usize = 192;

/// xorshift64 — deterministic pseudo-random fill (content is irrelevant, only cost matters).
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn f32_row_bytes(dims: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15;
    (0..dims)
        .flat_map(|_| {
            let bits = next_rand(&mut state) as u32;
            f32::from_bits(bits | 0x3F80_0000).to_le_bytes() // in [1.0, 2.0)
        })
        .collect()
}

fn query_f32(dims: usize) -> Vec<f32> {
    let mut state = 0x4D59_5DF4_D0F3_3173;
    (0..dims)
        .map(|_| {
            let bits = next_rand(&mut state) as u32;
            f32::from_bits(bits | 0x3F80_0000) // in [1.0, 2.0)
        })
        .collect()
}

fn binary_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x0F0F_0F0F_0F0F_0F0F;
    (0..len).map(|_| next_rand(&mut state) as u8).collect()
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

/// Binary popcount primitives at `d = 1536` (192 row bytes).
#[bench(raw)]
fn bench_binary_popcount_d1536() -> canbench_rs::BenchResult {
    let q = black_box(binary_bytes(D1536_BINARY_BYTES));
    let v = black_box(binary_bytes(D1536_BINARY_BYTES));
    canbench_rs::bench_fn(|| {
        black_box(bits01_and_popcount(&q, &v));
        black_box(signs_xor_popcount(&q, &v));
        black_box(popcount_bytes(&q));
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
