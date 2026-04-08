//! Stable-memory bitset with a heap mirror and a small durable journal.
//!
//! [`BitSet`] is the primary type, and [`StableBitSet`] is a convenience alias for the same
//! implementation. The type stores the authoritative bits in a heap mirror for fast reads, while
//! stable memory holds a compact header, a packed `u64` bitset snapshot, and an append-only
//! journal for pending updates.
//!
//! # Design
//!
//! - **Reads** always consult the heap mirror.
//! - **Writes** append a durable journal record first, then update the heap mirror.
//! - When the journal reaches capacity, the current heap state is checkpointed back into the stable
//!   snapshot and the journal is cleared.
//!
//! # V1 layout
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic `SBS`                 ↕ 3 bytes
//! ----------------------------------------
//! Layout version              ↕ 1 byte
//! ----------------------------------------
//! Length (`len_bits`)         ↕ 8 bytes
//! ----------------------------------------
//! Word capacity               ↕ 8 bytes
//! ----------------------------------------
//! Journal capacity            ↕ 8 bytes
//! ----------------------------------------
//! Reserved space              ↕ 36 bytes
//! ---------------------------------------- <- Address 64
//! Journal record 0            ↕ 8 bytes
//! ----------------------------------------
//! Journal record 1            ↕ 8 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! Journal record N-1          ↕ 8 bytes
//! ---------------------------------------- <- Address 64 + journal_cap * 8
//! Packed snapshot word 0      ↕ 8 bytes
//! ----------------------------------------
//! Packed snapshot word 1      ↕ 8 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! ```
//!
//! The header occupies a fixed 64-byte prefix, matching the stable-memory layout style used by
//! the other `ic-stable-*` crates. The journal stores pending `set`/`truncate` records as packed
//! 8-byte entries. Each record carries a payload that fits in `2 ^ 61` values, so the largest
//! representable bit index or logical length is [`JOURNAL_PAYLOAD_MAX`] = `2 ^ 61 - 1`. The
//! snapshot stores the current packed `u64` words used to rebuild the heap mirror on upgrade.
//!
//! # Type parameters
//!
//! - `M`: an [`ic_stable_structures::Memory`] implementation. The bitset reads and writes the
//!   provided stable memory directly.
//!
//! # Complexity
//!
//! - [`BitSet::contains`] is **O(1)**.
//! - [`BitSet::set`] is **O(1)** amortized, with checkpointing proportional to the number of words.
//! - [`BitSet::truncate`] is **O(number of live words)** because it clears suffix bits before the
//!   next checkpoint.
//!
//! # Concurrency
//!
//! `BitSet` uses interior mutability for the heap mirror and is intended for single-writer use. The
//! stable memory region should not be mutated through another wrapper while a bitset instance is in
//! use.
//!
//! # Constants
//!
//! - [`JOURNAL_PAYLOAD_BITS`] is `61` (`2 ^ 61` distinct payload values).
//! - [`JOURNAL_PAYLOAD_MAX`] is `(1 << 61) - 1` and is the maximum bit index or logical length
//!   value that a packed journal record can carry.
//!
//! # Example
//!
//! ```rust
//! # use ic_stable_bitset::BitSet;
//! # use ic_stable_structures::DefaultMemoryImpl;
//! let memory = DefaultMemoryImpl::default();
//! let bitset = BitSet::new(memory).unwrap();
//! bitset.insert(7).unwrap();
//! assert!(bitset.contains(7));
//! ```

mod bitset;
mod memory;

/// Number of payload bits stored in each packed journal record.
pub const JOURNAL_PAYLOAD_BITS: u32 = 61;

/// Maximum bit index or logical length representable by a packed journal record.
pub const JOURNAL_PAYLOAD_MAX: u64 = (1u64 << JOURNAL_PAYLOAD_BITS) - 1;

pub use bitset::{BitSet, BitSet as StableBitSet, InitError};
pub use memory::GrowFailed;
