//! **Stable Clustered Hashing** in Internet Computer stable memory: current layout with magic **`CHM`**, a
//! 64-byte header prefix following `ic-stable-structures`, a flattened chained hash table where items
//! of the same bucket are clustered together (Amble & Knuth 1974, "Ordered Hash Tables").
//!
//! The main type is [`StableClusteredHashMap`].
//!
//! # Operations
//!
//! - **O(1)** amortized [`StableClusteredHashMap::get`], [`StableClusteredHashMap::insert`],
//!   [`StableClusteredHashMap::remove`], [`StableClusteredHashMap::contains_key`] (by key).
//! - Growing when `len >= 3/4 * buckets` doubles a settled table's buckets while preserving its
//!   dynamic collision-tail reserve, then rehashes entries incrementally across subsequent
//!   operations (amortized **O(1)** per op). Relocation pressure grows and clears only the persisted
//!   logical tail; it does not drain an active remap or introduce a third bucket mapping. A new-key
//!   insert at the next threshold continues the bounded remap rather than starting another bucket
//!   generation.
//! - [`StableClusteredHashMap::insert`] and [`StableClusteredHashMap::remove`] return [`InsertError`]
//!   when the requested mutation and bounded remap maintenance encounter stable-memory growth or
//!   capacity failure. The whole public operation is failure-atomic: logical map bytes, headers,
//!   length, capacity, remap boundary, and key set remain unchanged and reopenable. Physical
//!   stable-memory pages grown before a later failure are outside that logical rollback contract.
//!   Active operations retain the direct path when a bounded empty suffix proves that maintenance
//!   plus the request cannot require growth. Other growth-capable operations write once through an
//!   undo transaction that snapshots each overwritten original logical block at most once and
//!   restores those blocks on a returned error. A trap relies on the IC message rollback boundary;
//!   the map does not add a write-ahead journal for process crashes outside that boundary.
//!
//! # Type parameters
//!
//! - `K`, `V`: must be [`ic_stable_structures::Storable`] with a **fixed-size** layout.
//! - `M`: [`ic_stable_structures::Memory`] (e.g. [`DefaultMemoryImpl`](ic_stable_structures::DefaultMemoryImpl)).
//!
//! Hashing uses `rapidhash(data)` (deterministic constant seed) mapped to a bucket via Fibonacci
//! hashing (`lower N bits of hash * 2^64/phi`). The hash is **not stored** (saves 8B/entry).
//!
//! All mutation uses `&self` and [`Memory`](ic_stable_structures::Memory); avoid aliasing the same
//! byte range with another mutating wrapper while an iterator is alive.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]

mod header;
mod iter;
mod map;
mod memory;

#[cfg(feature = "canbench")]
mod bench;

pub use header::InitError;
pub use iter::Iter;
pub use map::{InsertError, StableClusteredHashMap};
