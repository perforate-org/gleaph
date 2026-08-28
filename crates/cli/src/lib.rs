//! The Gleaph command-line interface library.
//!
//! The subcommand modules live here so integration tests (and the PocketIC E2E suite) can drive
//! the exact transports and pure helpers the `gleaph` binary uses; [`main.rs`](../main.rs) owns
//! only argument parsing and dispatch.

pub mod auth;
pub mod config;
pub mod embed;
pub mod grants;
pub mod identity;
pub mod load;
pub mod migration;
pub mod network;
pub mod prepared;
pub mod progress;
pub mod remote;
