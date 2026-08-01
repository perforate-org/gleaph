//! Public Candid surface index (ADR 0056 §1).
//!
//! The three layers are siblings; they do not call each other. Cross-domain orchestration goes
//! through `facade` / `gql` / `prepared` / `provisioning`. This module is a pure index with no
//! shared state.

pub mod client;
pub mod control;
pub mod federation;
