//! Router provisioning outbound boundary (ADR 0035 Slice 5).
//!
//! This module owns the Router -> Provision cross-canister send path and the
//! runtime provision-canister binding. It intentionally does NOT own deployment
//! trust bindings; those live in the Provision canister in this slice.

pub mod config;
pub mod sender;
