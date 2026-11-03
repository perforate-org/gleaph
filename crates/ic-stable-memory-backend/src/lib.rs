//! Stable-memory backend selection for Internet Computer canisters.
//!
//! `ic-stable-structures` 0.7 selects real canister stable memory only for
//! `wasm32`; its fallback on `wasm64` is heap-backed `VectorMemory`. Gleaph
//! canisters build for `wasm64-unknown-unknown`, so they use this adapter to
//! reach the IC stable64 API. `wasm32-unknown-unknown` retains the upstream IC
//! implementation, while native and WASI builds use the upstream vector memory.

#![deny(unsafe_code)]

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm32"))]
pub use ic_stable_structures::DefaultMemoryImpl;

#[cfg(not(any(
    all(target_family = "wasm", target_os = "unknown", target_arch = "wasm32"),
    all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64")
)))]
pub use ic_stable_structures::VectorMemory as DefaultMemoryImpl;

/// Creates the default stable-memory implementation for the compilation target.
#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm32"))]
pub fn default_memory_impl() -> DefaultMemoryImpl {
    ic_stable_structures::Ic0StableMemory
}

/// Creates the default stable-memory implementation for the compilation target.
#[cfg(not(any(
    all(target_family = "wasm", target_os = "unknown", target_arch = "wasm32"),
    all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64")
)))]
pub fn default_memory_impl() -> DefaultMemoryImpl {
    DefaultMemoryImpl::default()
}

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultMemoryImpl;

/// Creates the default stable-memory implementation for the compilation target.
#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64"))]
pub fn default_memory_impl() -> DefaultMemoryImpl {
    DefaultMemoryImpl
}

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64"))]
#[allow(unsafe_code)]
mod ic0 {
    use super::DefaultMemoryImpl;
    use ic_stable_structures::Memory;

    #[link(wasm_import_module = "ic0")]
    unsafe extern "C" {
        fn stable64_size() -> u64;
        fn stable64_grow(additional_pages: u64) -> i64;
        fn stable64_read(dst: u64, offset: u64, size: u64);
        fn stable64_write(offset: u64, src: u64, size: u64);
    }

    impl Memory for DefaultMemoryImpl {
        #[inline]
        fn size(&self) -> u64 {
            // SAFETY: The IC provides this import for canister Wasm modules.
            unsafe { stable64_size() }
        }

        #[inline]
        fn grow(&self, pages: u64) -> i64 {
            // SAFETY: The IC provides this import for canister Wasm modules.
            unsafe { stable64_grow(pages) }
        }

        #[inline]
        fn read(&self, offset: u64, dst: &mut [u8]) {
            // SAFETY: `dst` is writable for `dst.len()` bytes, and the IC traps
            // when the stable-memory range is out of bounds.
            unsafe { stable64_read(dst.as_mut_ptr() as u64, offset, dst.len() as u64) }
        }

        #[inline]
        unsafe fn read_unsafe(&self, offset: u64, dst: *mut u8, count: usize) {
            // SAFETY: The caller upholds `Memory::read_unsafe`'s destination
            // requirements, and the IC traps for an out-of-bounds source range.
            unsafe { stable64_read(dst as u64, offset, count as u64) }
        }

        #[inline]
        fn write(&self, offset: u64, src: &[u8]) {
            // SAFETY: `src` is readable for `src.len()` bytes, and the IC traps
            // when the stable-memory range is out of bounds.
            unsafe { stable64_write(offset, src.as_ptr() as u64, src.len() as u64) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::default_memory_impl;
    use ic_stable_structures::Memory;

    #[test]
    fn native_backend_preserves_the_memory_contract() {
        let memory = default_memory_impl();
        assert_eq!(memory.size(), 0);
        assert_eq!(memory.grow(1), 0);

        memory.write(17, &[1, 2, 3, 4]);
        let mut bytes = [0; 4];
        memory.read(17, &mut bytes);
        assert_eq!(bytes, [1, 2, 3, 4]);
    }
}
