//! **Stable hash map** in Internet Computer stable memory: V1 layout with magic **`SHM`**, a 64-byte
//! header prefix following `ic-stable-structures`, open addressing with linear probing, eager removes
//! (backward shift, no tombstones), and resize at a 3/4 load factor.
//!
//! The main type is [`StableHashMap`], also re-exported as [`StableHashMap`].
//!
//! # Operations
//!
//! - **O(1)** amortized [`StableHashMap::get`], [`StableHashMap::insert`], [`StableHashMap::remove`],
//!   [`StableHashMap::contains_key`] (by key).
//! - Growing when `len == 3/4 * capacity` rehashes all entries into a `2*cap - 1` table: **O(len)**
//!   work plus stable memory growth.
//!
//! # Type parameters
//!
//! - `K`, `V`: must be [`ic_stable_structures::Storable`] with a **fixed-size** layout.
//! - `M`: [`ic_stable_structures::Memory`] (e.g. [`DefaultMemoryImpl`](ic_stable_structures::DefaultMemoryImpl)).
//!
//! Hashing uses `rapidhash(data)` (deterministic constant seed, stable across canister upgrades).
//!
//! All mutation uses `&self` and [`Memory`](ic_stable_structures::Memory); avoid aliasing the same
//! byte range with another mutating wrapper while an iterator is alive.

mod hash_map;
mod header;
mod memory;

#[cfg(feature = "canbench")]
mod bench;

pub use hash_map::{InsertError, StableHashMap};
pub use header::InitError;
