#![cfg_attr(test, feature(f128))]

#[cfg(feature = "canbench")]
mod bench;
pub mod facade;
pub mod plan;
mod stable;

pub use facade::GraphStore;
