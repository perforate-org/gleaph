//! Zero stable memory before each benchmark so [`MemoryManager::init`](ic_stable_structures::memory_manager::MemoryManager::init) sees a clean backing.

#[cfg(target_arch = "wasm32")]
const STABLE_COPY_CHUNK: usize = 65_536;

#[cfg(target_arch = "wasm32")]
pub(crate) fn wipe_stable_memory() {
    use ic_cdk::api::{stable_size, stable_write};
    let pages = stable_size();
    if pages == 0 {
        return;
    }
    let len = pages.saturating_mul(65_536);
    let mut off = 0u64;
    let zero = [0u8; STABLE_COPY_CHUNK];
    while off < len {
        let take = ((len - off) as usize).min(STABLE_COPY_CHUNK);
        stable_write(off, &zero[..take]);
        off += take as u64;
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn wipe_stable_memory() {}

#[cfg(target_arch = "wasm32")]
pub(crate) fn snapshot_stable_memory() -> Vec<u8> {
    use ic_cdk::api::{stable_read, stable_size};
    let pages = stable_size();
    let len = pages.saturating_mul(65_536);
    let mut bytes = vec![0u8; len as usize];
    let mut off = 0u64;
    while off < len {
        let take = ((len - off) as usize).min(STABLE_COPY_CHUNK);
        stable_read(off, &mut bytes[off as usize..off as usize + take]);
        off += take as u64;
    }
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn snapshot_stable_memory() -> Vec<u8> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn restore_stable_memory(bytes: &[u8]) {
    use ic_cdk::api::{stable_grow, stable_size, stable_write};
    wipe_stable_memory();
    let need_pages = (bytes.len() as u64).div_ceil(65_536);
    let cur_pages = stable_size();
    if need_pages > cur_pages {
        let grow_by = need_pages - cur_pages;
        let prev = stable_grow(grow_by);
        assert_ne!(prev, u64::MAX, "stable_grow for fixture restore failed");
    }
    let mut off = 0usize;
    while off < bytes.len() {
        let end = (off + STABLE_COPY_CHUNK).min(bytes.len());
        stable_write(off as u64, &bytes[off..end]);
        off = end;
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn restore_stable_memory(_bytes: &[u8]) {}
