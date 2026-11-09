//! Verified leaf for absolute-target Stable64 growth.
//!
//! `grow_to(target)` grows stable memory to *at least* `target` 64 KiB pages and
//! returns the resulting page count. Stable64 grow is a delta API, so clients
//! compute `target - size()`; this leaf owns that arithmetic once.
//!
//! Two implementations share the API:
//!
//! - the plain one (default build) calls the [`verus_ic`] bridge and aborts when
//!   the subnet refuses growth, mirroring the verified token's trap;
//! - the verified one (`--features verus`, checked by `cargo verus verify`)
//!   threads the `verus_ic::token` linear token and proves
//!   `final_pages >= target_pages`.
//!
//! Scope: stable memory must have a single owner, as required by verus-ic. This
//! crate is the first adoption leaf, not yet wired into a canister.

pub mod plain;

#[cfg(feature = "verus")]
pub mod verified;

#[cfg(not(feature = "verus"))]
pub use plain::grow_to;

#[cfg(feature = "verus")]
pub use verified::grow_to;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::grow_to;

    #[test]
    fn grow_to_absolute_target() {
        verus_ic::bridge::reset_native_memory(0);
        assert_eq!(grow_to(2), 2);
        assert_eq!(verus_ic::bridge::stable64_size(), 2);
        // Already large enough: no grow, report the current size.
        assert_eq!(grow_to(1), 2);
        assert_eq!(verus_ic::bridge::stable64_size(), 2);
        assert_eq!(grow_to(4), 4);
        assert_eq!(verus_ic::bridge::stable64_size(), 4);
        verus_ic::bridge::reset_native_memory(0);
    }
}
