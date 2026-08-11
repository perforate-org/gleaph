//! Stable-memory backend selection for Internet Computer canisters.
//!
//! `ic-stable-structures` 0.7 selects real canister stable memory only for
//! `wasm32`; its fallback on `wasm64` is heap-backed `VectorMemory`. Gleaph
//! canisters build for `wasm64`, so they use this adapter to reach the IC
//! stable64 API while native tests retain the upstream in-memory fallback.

#![forbid(unsafe_code)]

#[cfg(not(all(target_family = "wasm", target_arch = "wasm64")))]
pub use ic_stable_structures::DefaultMemoryImpl as StableMemoryBackend;

/// Creates the stable-memory backend appropriate for the compilation target.
#[cfg(not(all(target_family = "wasm", target_arch = "wasm64")))]
pub fn stable_memory_backend() -> StableMemoryBackend {
    StableMemoryBackend::default()
}

#[cfg(all(target_family = "wasm", target_arch = "wasm64"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct StableMemoryBackend;

/// Creates the stable-memory backend appropriate for the compilation target.
#[cfg(all(target_family = "wasm", target_arch = "wasm64"))]
pub fn stable_memory_backend() -> StableMemoryBackend {
    StableMemoryBackend
}

#[cfg(all(target_family = "wasm", target_arch = "wasm64"))]
impl ic_stable_structures::Memory for StableMemoryBackend {
    fn size(&self) -> u64 {
        ic_cdk::stable::stable_size()
    }

    fn grow(&self, pages: u64) -> i64 {
        ic_cdk::stable::stable_grow(pages)
            .ok()
            .and_then(|previous_pages| i64::try_from(previous_pages).ok())
            .unwrap_or(-1)
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        ic_cdk::stable::stable_read(offset, dst);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        ic_cdk::stable::stable_write(offset, src);
    }
}

#[cfg(test)]
mod tests {
    use super::stable_memory_backend;
    use ic_stable_structures::Memory;

    #[test]
    fn native_backend_preserves_the_memory_contract() {
        let memory = stable_memory_backend();
        assert_eq!(memory.size(), 0);
        assert_eq!(memory.grow(1), 0);

        memory.write(17, &[1, 2, 3, 4]);
        let mut bytes = [0; 4];
        memory.read(17, &mut bytes);
        assert_eq!(bytes, [1, 2, 3, 4]);
    }
}
