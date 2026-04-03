#![doc = include_str!("../../../docs/graph-pma-target-design.md")]
//!
//! This crate is the rewrite entrypoint for `graph-pma`.
//! It exposes only the rewrite implementation.

pub(crate) mod bench_profile;
pub(crate) mod canbench_scope;
pub mod facade;
pub mod integration;
pub mod low_level;
pub mod observability;
pub mod property_index;
pub mod property_store;
pub mod stable;
pub(crate) use low_level::{GraphInsertDecision, GraphInsertResult, ResolvedEdgeSlot};
#[cfg(any(test, doctest))]
pub(crate) use property_index::PropertyIndexNodeId;
pub(crate) use property_store::PropertyEntityKind;

// Convenience aliases used by callers and tests.
pub use facade::{RewriteGraphPma, RewriteGraphPmaError, RewriteGraphPmaResult};
pub type GraphPma = facade::RewriteGraphPma;
pub type VecMemory = stable::VecMemory;
pub type RewriteVecMemory = stable::VecMemory;
