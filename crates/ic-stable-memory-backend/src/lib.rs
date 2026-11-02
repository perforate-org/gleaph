//! Stable-memory backend selection for Internet Computer canisters.
//!
//! `ic-stable-structures` 0.7 selects real canister stable memory only for
//! `wasm32`; its fallback on `wasm64` is heap-backed `VectorMemory`. Gleaph
//! canisters build for `wasm64-unknown-unknown`, so they use this adapter to
//! reach the IC stable64 API. `wasm32-unknown-unknown` retains the upstream IC
//! implementation, while native and WASI builds use the upstream vector memory.
//!
//! The handle is a newtype rather than a re-export so that [`shadow`] can route every read and write
//! through a copy-on-write overlay while a window is installed. That keeps one `Memory`
//! implementation for the whole workspace: no caller is generic over "shadowed or not".

#![deny(unsafe_code)]

pub mod shadow;

pub use shadow::{PAGE_BYTES, ShadowCommitError, ShadowMemory};

use ic_stable_structures::Memory;
use std::cell::RefCell;

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm32"))]
type PlatformMemory = ic_stable_structures::Ic0StableMemory;

#[cfg(not(any(
    all(target_family = "wasm", target_os = "unknown", target_arch = "wasm32"),
    all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64")
)))]
type PlatformMemory = ic_stable_structures::VectorMemory;

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64"))]
type PlatformMemory = Stable64Memory;

/// The canister's stable memory.
#[derive(Clone, Default)]
pub struct DefaultMemoryImpl {
    platform: PlatformMemory,
}

impl std::fmt::Debug for DefaultMemoryImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The platform handle is a wasm import on `wasm32` and a heap buffer elsewhere; only the
        // fact that a handle exists is meaningful to a caller.
        f.write_str("DefaultMemoryImpl")
    }
}

impl DefaultMemoryImpl {
    /// A handle to the backend memory for this target.
    pub fn new() -> Self {
        Self {
            platform: PlatformMemory::default(),
        }
    }
}

thread_local! {
    /// The memory every handle created inside the current shadow window shares, so that those
    /// handles address the same bytes the overlay was built over.
    static WINDOW_MEMORY: RefCell<Option<PlatformMemory>> = const { RefCell::new(None) };
    /// The installed overlay, if any.
    static OVERLAY: RefCell<Option<ShadowMemory<PlatformMemory>>> = const { RefCell::new(None) };
}

/// Creates the default stable-memory implementation for the compilation target.
///
/// Inside a [`enter_shadow`] window this returns a handle to the window's memory, so a caller
/// created during the window sees the overlay rather than the committed bytes.
pub fn default_memory_impl() -> DefaultMemoryImpl {
    if let Some(platform) = WINDOW_MEMORY.with(|slot| slot.borrow().clone()) {
        return DefaultMemoryImpl { platform };
    }
    DefaultMemoryImpl::new()
}

impl Memory for DefaultMemoryImpl {
    fn size(&self) -> u64 {
        let shadowed = OVERLAY.with(|slot| slot.borrow().as_ref().map(|shadow| shadow.size()));
        shadowed.unwrap_or_else(|| self.platform.size())
    }

    fn grow(&self, pages: u64) -> i64 {
        let shadowed = OVERLAY.with(|slot| slot.borrow().as_ref().map(|shadow| shadow.grow(pages)));
        shadowed.unwrap_or_else(|| self.platform.grow(pages))
    }

    fn read(&self, offset: u64, dst: &mut [u8]) {
        let shadowed = OVERLAY.with(|slot| {
            let borrowed = slot.borrow();
            borrowed
                .as_ref()
                .map(|shadow| shadow.read(offset, dst))
                .is_some()
        });
        if !shadowed {
            self.platform.read(offset, dst);
        }
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let shadowed = OVERLAY.with(|slot| {
            let borrowed = slot.borrow();
            borrowed
                .as_ref()
                .map(|shadow| shadow.write(offset, src))
                .is_some()
        });
        if !shadowed {
            self.platform.write(offset, src);
        }
    }
}

/// An installed shadow window: every memory access until this guard ends goes to a copy-on-write
/// overlay instead of the backend memory.
///
/// Installing one is only sound when the writes it absorbs are decided inside the window and the
/// window ends in a commit or a trap - the overlay is the only record of the attempt. Two rules
/// follow, and both belong to the caller:
///
/// - **No `await` inside the window.** An inter-canister call would let another message observe a
///   half-written overlay; the write paths that use this are synchronous.
/// - **Commit immediately after the work.** Until the commit writes the overlay out, the backend
///   memory still holds the pre-attempt bytes while the caller's caches describe the new layout.
#[must_use = "dropping the guard discards the attempt"]
pub struct ShadowGuard {
    /// The overlay is thread-local, so the guard must not travel between threads.
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ShadowGuard {
    /// Bytes the attempt has written, i.e. the cost of the commit and the rollback payload.
    pub fn dirty_bytes(&self) -> u64 {
        OVERLAY.with(|slot| slot.borrow().as_ref().map_or(0, ShadowMemory::dirty_bytes))
    }

    /// Ranges the attempt has written, i.e. how many separate copies the commit performs.
    pub fn dirty_ranges(&self) -> usize {
        OVERLAY.with(|slot| slot.borrow().as_ref().map_or(0, ShadowMemory::dirty_ranges))
    }

    /// Publishes the attempt and ends the window. Dropping the guard instead discards it.
    pub fn commit(self) -> Result<(), ShadowCommitError> {
        let overlay = OVERLAY.with(|slot| slot.borrow_mut().take());
        WINDOW_MEMORY.with(|slot| *slot.borrow_mut() = None);
        match overlay {
            Some(shadow) => shadow.commit().map(|_| ()),
            None => Ok(()),
        }
    }
}

impl Drop for ShadowGuard {
    fn drop(&mut self) {
        OVERLAY.with(|slot| *slot.borrow_mut() = None);
        WINDOW_MEMORY.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Starts a shadow window over a fresh backend memory.
///
/// While the window is installed, [`default_memory_impl`] hands out handles to the window's memory,
/// so structures created inside it address the overlay.
///
/// # Panics
///
/// Panics if a window is already installed: nesting would make "which overlay saw this write"
/// unanswerable, and no caller needs it.
pub fn enter_shadow() -> ShadowGuard {
    let platform = PlatformMemory::default();
    WINDOW_MEMORY.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "a shadow window is already installed");
        *slot = Some(platform.clone());
    });
    OVERLAY.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(slot.is_none(), "a shadow window is already installed");
        *slot = Some(ShadowMemory::new(platform));
    });
    ShadowGuard {
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64"))]
#[derive(Clone, Copy, Debug, Default)]
struct Stable64Memory;

#[cfg(all(target_family = "wasm", target_os = "unknown", target_arch = "wasm64"))]
#[allow(unsafe_code)]
mod ic0 {
    use super::Stable64Memory;
    use ic_stable_structures::Memory;

    #[link(wasm_import_module = "ic0")]
    unsafe extern "C" {
        fn stable64_size() -> u64;
        fn stable64_grow(additional_pages: u64) -> i64;
        fn stable64_read(dst: u64, offset: u64, size: u64);
        fn stable64_write(offset: u64, src: u64, size: u64);
    }

    impl Memory for Stable64Memory {
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
            // SAFETY: `dst` is writable for `dst.len()` bytes, and the IC traps when the
            // stable-memory range is out of bounds.
            unsafe { stable64_read(dst.as_mut_ptr() as u64, offset, dst.len() as u64) }
        }

        #[inline]
        fn write(&self, offset: u64, src: &[u8]) {
            // SAFETY: `src` is readable for `src.len()` bytes, and the IC traps when the
            // stable-memory range is out of bounds.
            unsafe { stable64_write(offset, src.as_ptr() as u64, src.len() as u64) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{default_memory_impl, enter_shadow};
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

    #[test]
    fn a_shadow_window_absorbs_writes_and_commits_them() {
        let guard = enter_shadow();
        let memory = default_memory_impl();
        memory.write(0, &[7; 4]);
        assert_eq!(guard.dirty_ranges(), 1, "the write stayed in the overlay");
        guard.commit().expect("commit");

        // The window's memory kept the committed bytes, and a fresh handle outside any window is
        // the backend's pre-existing per-call memory.
        let reopened = enter_shadow();
        assert_eq!(reopened.dirty_bytes(), 0);
    }
}
