//! The Gleaph command-line interface library.
//!
//! The subcommand modules live here so integration tests (and the PocketIC E2E suite) can drive
//! the exact transports and pure helpers the `gleaph` binary uses; [`main.rs`](../main.rs) owns
//! only argument parsing and dispatch.

pub mod ask;
pub mod auth;
pub mod config;
pub mod decision;
pub mod embed;
pub mod embedding;
pub mod gql;
pub mod grants;
pub mod identity;
pub mod load;
pub mod migration;
pub mod network;
pub mod prepared;
pub mod progress;
pub mod remote;
pub mod route;
pub mod routing_log;
pub mod slots;
pub mod text_index;
pub mod vector;
