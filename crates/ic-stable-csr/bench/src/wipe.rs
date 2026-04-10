//! Zero stable memory before each benchmark so [`MemoryManager::init`](ic_stable_structures::memory_manager::MemoryManager::init) sees a clean backing.

#[cfg(target_arch = "wasm32")]
const STABLE_COPY_CHUNK: usize = 65_536;

#[cfg(target_arch = "wasm32")]
fn stable_len_bytes() -> u64 {
    ic_cdk::api::stable_size().saturating_mul(65_536)
}

#[cfg(target_arch = "wasm32")]
fn write_stable_in_chunks(offset: u64, bytes: &[u8]) {
    use ic_cdk::api::stable_write;
    let mut off = 0usize;
    while off < bytes.len() {
        let end = (off + STABLE_COPY_CHUNK).min(bytes.len());
        stable_write(offset + off as u64, &bytes[off..end]);
        off = end;
    }
}

#[cfg(target_arch = "wasm32")]
fn read_stable_in_chunks(offset: u64, out: &mut [u8]) {
    use ic_cdk::api::stable_read;
    let mut off = 0usize;
    while off < out.len() {
        let end = (off + STABLE_COPY_CHUNK).min(out.len());
        stable_read(offset + off as u64, &mut out[off..end]);
        off = end;
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn wipe_stable_memory() {
    let len = stable_len_bytes();
    if len == 0 {
        return;
    }
    let mut off = 0u64;
    let zero = [0u8; STABLE_COPY_CHUNK];
    while off < len {
        let take = ((len - off) as usize).min(STABLE_COPY_CHUNK);
        write_stable_in_chunks(off, &zero[..take]);
        off += take as u64;
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn wipe_stable_memory() {}

#[cfg(target_arch = "wasm32")]
pub(crate) fn snapshot_stable_memory() -> Vec<u8> {
    let len = stable_len_bytes();
    let mut bytes = vec![0u8; len as usize];
    read_stable_in_chunks(0, &mut bytes);
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn snapshot_stable_memory() -> Vec<u8> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn restore_stable_memory(bytes: &[u8]) {
    use ic_cdk::api::{stable_grow, stable_size};
    wipe_stable_memory();
    let need_pages = (bytes.len() as u64).div_ceil(65_536);
    let cur_pages = stable_size();
    if need_pages > cur_pages {
        let grow_by = need_pages - cur_pages;
        let prev = stable_grow(grow_by);
        assert_ne!(prev, u64::MAX, "stable_grow for fixture restore failed");
    }
    write_stable_in_chunks(0, bytes);
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn restore_stable_memory(_bytes: &[u8]) {}
