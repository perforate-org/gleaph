//! Slot extraction and binding, re-exported from the client SDK.
//!
//! The SDK owns the slot contract (vocabulary matching, span binding and fail-closed errors) so
//! the CLI, services and browser bindings cannot drift; this module only keeps the CLI's module
//! path stable for `crate::slots`.

pub use gleaph_sdk::routing::slots::*;
