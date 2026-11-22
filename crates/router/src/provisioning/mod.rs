//! Router provisioning outbound boundary (ADR 0035).
//!
//! - `config`: runtime provision-canister binding.
//! - `resource`: deletion of one issued add-on canister (ADR 0037).
//! - `sender`: Router -> Provision cross-canister send.

pub mod graph;
pub mod resource;
pub mod sender;
