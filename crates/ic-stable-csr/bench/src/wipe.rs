//! Zero stable memory before each benchmark so [`MemoryManager::init`](ic_stable_structures::memory_manager::MemoryManager::init) sees a clean backing.

#[cfg(target_arch = "wasm32")]
pub(crate) fn wipe_stable_memory() {
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
pub(crate) fn wipe_stable_memory() {}
