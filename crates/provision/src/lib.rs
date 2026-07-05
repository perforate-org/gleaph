//! Gleaph Provision canister — ADR 0035.
//!
//! Owns durable job/receipt state and the deployment trust binding.
//! Does not own graph topology, tenancy, or routing catalogs.

#![cfg_attr(not(test), allow(dead_code))]

pub mod stable;
pub mod types;

/// IC NNS timestamp in nanoseconds.
///
/// Mirrors `crates/router/src/facade/store.rs:121-128`: returns `ic_cdk::api::time()` on
/// `wasm`, `0` on `not(target_family = "wasm")`. Used by the future ingress path (0056); not used
/// by the store or by tests.
#[allow(dead_code)]
pub(crate) fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}
