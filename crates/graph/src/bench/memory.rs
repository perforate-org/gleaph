//! Stable-memory (`Memory`) backend for canbench: real IC stable pages on wasm32, in-process Vec on host.

#[cfg(target_arch = "wasm32")]
use ic_stable_structures::Ic0StableMemory;

/// Clears allocated stable pages (wasm32). Used between iterations so each `bootstrap_empty` starts clean.
pub(super) fn wipe_for_bench_iteration() {
    wipe_stable_bytes();
}

#[cfg(target_arch = "wasm32")]
fn wipe_stable_bytes() {
    use ic_cdk::api::{stable_size, stable_write};
    let pages = stable_size();
    if pages == 0 {
        return;
    }
    let len = pages.saturating_mul(65_536);
    const CHUNK: usize = 8192;
    let mut off = 0u64;
    let zero = [0u8; CHUNK];
    while off < len {
        let take = ((len - off) as usize).min(CHUNK);
        stable_write(off, &zero[..take]);
        off += take as u64;
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn wipe_stable_bytes() {}

#[cfg(target_arch = "wasm32")]
pub(super) type BenchMemory = Ic0StableMemory;

#[cfg(not(target_arch = "wasm32"))]
pub(super) type BenchMemory = ic_stable_structures::VectorMemory;
