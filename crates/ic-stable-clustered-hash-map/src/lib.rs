//! **Stable Clustered Hashing** in Internet Computer stable memory: V1 layout with magic **`CHM`**, a
//! 64-byte header prefix following `ic-stable-structures`, a flattened chained hash table where items
//! of the same bucket are clustered together (Amble & Knuth 1974, "Ordered Hash Tables").
//!
//! The main type is [`StableClusteredHashMap`].
//!
//! # Operations
//!
//! - **O(1)** amortized [`StableClusteredHashMap::get`], [`StableClusteredHashMap::insert`],
//!   [`StableClusteredHashMap::remove`], [`StableClusteredHashMap::contains_key`] (by key).
//! - Growing when `len >= 3/4 * buckets` rehashes all entries into a `2^(N+1) + (N+1)` table:
//!   **O(len)** work plus stable memory growth. (An in-place incremental resize is a later slice.)
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
