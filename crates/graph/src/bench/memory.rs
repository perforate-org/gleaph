//! Stable-memory (`Memory`) backend for canbench: real IC stable pages on wasm32, in-process Vec on host.

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

/// IC canister stable memory (64KiB pages). Zero-sized: state lives in the system API.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct IcStableMemory;

#[cfg(target_arch = "wasm32")]
impl gleaph_graph_pma::stable::Memory for IcStableMemory {
    fn size(&self) -> u64 {
        ic_cdk::api::stable_size()
    }

    fn grow(&self, pages: u64) -> i64 {
        let r = ic_cdk::api::stable_grow(pages);
        if r == u64::MAX { -1 } else { r as i64 }
    }

    fn read(&self, offset: u64, buf: &mut [u8]) {
        ic_cdk::api::stable_read(offset, buf);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        ic_cdk::api::stable_write(offset, src);
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) type BenchMemory = IcStableMemory;

#[cfg(not(target_arch = "wasm32"))]
pub(super) type BenchMemory = gleaph_graph_pma::GraphPmaVecMemory;
