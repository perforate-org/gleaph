//! Router provisioning outbound boundary and ack callback (ADR 0035 Slices 5 and 6).
//!
//! - `config`: runtime provision-canister binding.
//! - `sender`: Router -> Provision cross-canister send.
//! - `ack_handler`: Provision -> Router `router_ack` callback handler.

pub mod ack_handler;
pub mod config;
pub mod sender;
