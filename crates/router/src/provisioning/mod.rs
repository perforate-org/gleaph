//! Router provisioning outbound boundary (ADR 0035).
//!
//! - `config`: runtime provision-canister binding.
//! - `sender`: Router -> Provision cross-canister send.

pub mod config;
pub mod graph;
pub mod sender;
