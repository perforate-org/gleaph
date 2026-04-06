#![doc = include_str!("../../../docs/graph-pma-target-design.md")]
//!
//! Primary graph persistence and adjacency implementation for `graph-pma`.

pub(crate) mod bench_profile;
pub(crate) mod byte_compare;
pub(crate) mod canbench_scope;
pub mod facade;
pub mod integration;
pub mod low_level;
pub mod observability;
pub mod property_index;
pub mod property_store;
pub(crate) use low_level::{GraphInsertDecision, GraphInsertResult, ResolvedEdgeSlot};
#[cfg(any(test, doctest))]
pub(crate) use property_index::PropertyIndexNodeId;
pub(crate) use property_store::PropertyEntityKind;

// Convenience aliases used by callers and tests.
pub use facade::{GraphPma, GraphPmaError, GraphPmaResult};
pub type VecMemory = ic_stable_structures::VectorMemory;
pub type GraphPmaVecMemory = ic_stable_structures::VectorMemory;
