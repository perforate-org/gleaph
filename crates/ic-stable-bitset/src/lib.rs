//! Stable-memory bitset with a heap mirror and a durable mutation journal.
//!
//! [`BitSet`] is the primary type, and [`StableBitSet`] is a convenience alias for the same
//! implementation. The type stores the authoritative bits in a heap mirror for fast reads, while
//! stable memory holds a compact header, a packed `u64` bitset snapshot, and an append-only
//! journal of packed mutation records.
//!
//! # Design
//!
//! - **Reads** always consult the heap mirror.
//! - `set` and `truncate` append journal records and update the heap mirror.
//! - `clear` is a convenience alias for `set(index, false)`.
//! - `remove` is journaled so reopen can replay suffix shifts deterministically.
//! - When the journal reaches capacity, the current heap state is checkpointed back into the stable
//!   snapshot and the journal is cleared.
//!
//! # Layout
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
//! Mutation record 0           ↕ 8 bytes
//! ----------------------------------------
//! Mutation record 1           ↕ 8 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! Mutation record N-1         ↕ 8 bytes
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
//! the other `ic-stable-*` crates. The journal stores packed 8-byte mutation records, and the
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
//! - [`BitSet::set`] and [`BitSet::clear`] are **O(1)** amortized, with journal append plus a heap
//!   update.
//! - [`BitSet::remove`] is **O(number of live words after the removed index)** because it shifts
//!   the suffix left by one word at a time after recording the mutation.
//! - [`BitSet::truncate`] and [`BitSet::ensure_len`] are **O(number of live words)** because they
//!   clear or preserve suffix bits directly and checkpoint when the journal is full.
//!
//! # Concurrency
//!
//! `BitSet` uses interior mutability for the heap mirror and is intended for single-writer use.
//! The stable memory region should not be mutated through another wrapper while a bitset instance
//! is in use.
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

/// Maximum payload value that can be stored in a packed journal record.
pub const JOURNAL_PAYLOAD_MAX: u64 = (1u64 << 61) - 1;

pub use bitset::{BitSet, BitSet as StableBitSet, InitError};
pub use memory::GrowFailed;
