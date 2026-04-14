//! Stable-memory bitset with a heap mirror and a durable mutation journal.
//!
//! [`Bitset`] is the primary type, and [`StableBitset`] is a convenience alias for the same
//! implementation. The type stores the authoritative bits in a heap mirror for fast reads, while
//! stable memory holds a compact header, a packed `u64` bitset snapshot, and an append-only
//! journal of **5-byte** packed mutation records.
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
//! Journal slots (fixed)       ↕ 8 bytes (`JOURNAL_CAP_SLOTS` as `u64`)
//! ----------------------------------------
//! Reserved space              ↕ 36 bytes
//! ---------------------------------------- <- Address 64
//! Mutation record 0           ↕ 5 bytes
//! ----------------------------------------
//! Mutation record 1           ↕ 5 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! Mutation record N-1         ↕ 5 bytes
//! ---------------------------------------- <- 64 + JOURNAL_CAP_SLOTS * 5 (not always 8-aligned)
//! Zero padding                ↕ 0..7 bytes
//! ---------------------------------------- <- snapshot_base = align_up(64 + N*5, 8)
//! Packed snapshot word 0      ↕ 8 bytes
//! ----------------------------------------
//! Packed snapshot word 1      ↕ 8 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! ```
//!
//! The header occupies a fixed 64-byte prefix, matching the stable-memory layout style used by
//! the other `ic-stable-*` crates. The journal stores **5-byte** records ([`JOURNAL_RECORD_RAW_MASK`]);
//! the snapshot stores the current packed `u64` words starting at an **8-byte aligned** offset.
//!
//! # Type parameters
//!
//! - `M`: an [`ic_stable_structures::Memory`] implementation. The bitset reads and writes the
//!   provided stable memory directly.
//!
//! # Complexity
//!
//! - [`Bitset::contains`] is **O(1)**.
//! - [`Bitset::set`] and [`Bitset::clear`] are **O(1)** amortized, with journal append plus a heap
//!   update.
//! - [`Bitset::remove`] is **O(number of live words after the removed index)** because it shifts
//!   the suffix left by one word at a time after recording the mutation.
//! - [`Bitset::truncate`] and [`Bitset::ensure_len`] are **O(number of live words)** because they
//!   clear or preserve suffix bits directly and checkpoint when the journal is full.
//!
//! # Concurrency
//!
//! `Bitset` uses interior mutability for the heap mirror and is intended for single-writer use.
//! The stable memory region should not be mutated through another wrapper while a bitset instance
//! is in use.
//!
//! # Example
//!
//! ```rust
//! # use ic_stable_bitset::Bitset;
//! # use ic_stable_structures::DefaultMemoryImpl;
//! let memory = DefaultMemoryImpl::default();
//! let bitset = Bitset::new(memory).unwrap();
//! bitset.insert(7).unwrap();
//! assert!(bitset.contains(7));
//! ```

mod bitset;
mod memory;

/// Number of journal slots on stable memory (compile-time constant). Must match the `u64` at
/// header offset 20 on disk.
pub const JOURNAL_CAP_SLOTS: usize = 4096;

/// Byte length of the on-disk journal region (`JOURNAL_CAP_SLOTS` records × 5 bytes each).
pub const JOURNAL_REGION_BYTES: usize = JOURNAL_CAP_SLOTS * 5;

/// `Memory::read` chunk size during journal replay (must divide `JOURNAL_REGION_BYTES` and 5).
pub const JOURNAL_READ_CHUNK_BYTES: usize = 5120;

const _: () = assert!(JOURNAL_REGION_BYTES % JOURNAL_READ_CHUNK_BYTES == 0);
const _: () = assert!(JOURNAL_READ_CHUNK_BYTES % 5 == 0);

/// Bit mask for one on-disk journal record: **40 low bits** of a little-endian 5-byte encoding.
pub const JOURNAL_RECORD_RAW_MASK: u64 = (1u64 << 40) - 1;

/// Maximum exclusive logical length (`len_bits`) and maximum `SetLen` value supported by the API.
///
/// Bit indices are `u32`; the exclusive logical length may be `u32::MAX + 1`.
pub const JOURNAL_LEN_MAX: u64 = (u32::MAX as u64) + 1;

pub use bitset::{Bitset, Bitset as StableBitset, ContainsView, InitError};
pub use memory::GrowFailed;
