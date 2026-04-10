//! Stable-memory roaring bitmap with a heap mirror and a durable mutation journal.
//!
//! [`bitmap::RoaringBitMap`] is the primary type, and [`StableRoaringBitMap`] is a convenience
//! alias for the same implementation. The type stores the authoritative set bits in a heap-mirrored
//! [`roaring::RoaringTreemap`], while stable memory holds a compact header, an append-only journal
//! of packed mutation records, and a serialized snapshot of the roaring structure.
//!
//! # Design
//!
//! - **Reads** always consult the heap mirror.
//! - `set`, `insert`, `clear`, `ensure_len`, and `truncate` append journal records and update the
//!   heap mirror.
//! - `remove` is intentionally not part of the API.
//! - When the journal reaches capacity, the current heap state is checkpointed back into the
//!   stable snapshot and the journal is cleared.
//!
//! # Layout
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic `RSB`                 ↕ 3 bytes
//! ----------------------------------------
//! Layout version              ↕ 1 byte
//! ----------------------------------------
//! Length (`len_bits`)         ↕ 8 bytes
//! ----------------------------------------
//! Journal capacity            ↕ 8 bytes
//! ----------------------------------------
//! Snapshot length             ↕ 8 bytes
//! ----------------------------------------
//! Reserved space              ↕ 36 bytes
//! ---------------------------------------- <- Address 64
//! Mutation record 0           ↕ 8 bytes
//! ----------------------------------------
//! Mutation record 1           ↕ 8 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! Mutation record N-1         ↕ 8 bytes
//! ---------------------------------------- <- Address 64 + journal_cap * 8
//! Serialized Roaring snapshot bytes
//! ----------------------------------------
//! ```
//!
//! The snapshot is the canonical `RoaringTreemap` serialization. The journal stores fixed-width
//! packed `u64` records so reopen can replay pending mutations before the next checkpoint.
//!
//! # Type parameters
//!
//! - `M`: an [`ic_stable_structures::Memory`] implementation. The bitmap reads and writes the
//!   provided stable memory directly.
//!
//! # Complexity
//!
//! - [`bitmap::RoaringBitMap::contains`] is **O(1)**.
//! - [`bitmap::RoaringBitMap::set`] and [`bitmap::RoaringBitMap::clear`] are **O(1)** amortized, with journal append plus a heap
//!   update.
//! - [`bitmap::RoaringBitMap::truncate`] and [`bitmap::RoaringBitMap::ensure_len`] are **O(number of roaring containers
//!   touched)**.
//!
//! # Concurrency
//!
//! `RoaringBitMap` uses interior mutability for the heap mirror and is intended for single-writer use.
//! The stable memory region should not be mutated through another wrapper while a bitmap instance
//! is in use.
//!
//! # Example
//!
//! ```rust
//! # use ic_stable_roaring::StableRoaringBitMap;
//! # use ic_stable_structures::DefaultMemoryImpl;
//! let memory = DefaultMemoryImpl::default();
//! let bitset = StableRoaringBitMap::new(memory).unwrap();
//! bitset.insert(7).unwrap();
//! assert!(bitset.contains(7));
//! ```

pub mod bitmap;
mod memory;

/// Maximum payload value that can be stored in a packed journal record.
pub const JOURNAL_PAYLOAD_MAX: u64 = (1u64 << 61) - 1;

pub use bitmap::{ContainsView, InitError, RoaringBitMap};
pub use bitmap::RoaringBitMap as StableRoaringBitMap;
pub use memory::GrowFailed;
