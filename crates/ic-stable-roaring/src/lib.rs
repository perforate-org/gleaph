//! Stable-memory roaring bitmap with a heap mirror and a durable mutation journal.
//!
//! [`bitmap::RoaringBitmap`] is the primary type, and [`StableRoaringBitmap`] is a convenience
//! alias for the same implementation. The type stores the authoritative set bits in a heap-mirrored
//! [`roaring::RoaringBitmap`] (indices are `u32`), while stable memory holds a compact header, an
//! append-only journal of packed mutation records, and a serialized snapshot of the roaring structure.
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
//! Journal slots (fixed)       ↕ 8 bytes (`JOURNAL_CAP_SLOTS` as `u64`)
//! ----------------------------------------
//! Snapshot length             ↕ 8 bytes
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
//! Serialized Roaring snapshot bytes
//! ----------------------------------------
//! ```
//!
//! The snapshot is the canonical `RoaringBitmap` serialization (not wire-compatible with older
//! `RoaringTreemap` snapshots at the same layout version). The journal stores **5-byte** packed
//! records (40 low bits; see module-level [`JOURNAL_RECORD_RAW_MASK`]); logical lengths are
//! bounded by [`JOURNAL_LEN_MAX`]. Replay stops at the first all-zero record.
//!
//! # Type parameters
//!
//! - `M`: an [`ic_stable_structures::Memory`] implementation. The bitmap reads and writes the
//!   provided stable memory directly.
//!
//! # Complexity
//!
//! - [`bitmap::RoaringBitmap::contains`] is **O(1)**.
//! - [`bitmap::RoaringBitmap::set`] and [`bitmap::RoaringBitmap::clear`] are **O(1)** amortized, with journal append plus a heap
//!   update.
//! - [`bitmap::RoaringBitmap::truncate`] and [`bitmap::RoaringBitmap::ensure_len`] are **O(number of roaring containers
//!   touched)**.
//!
//! # Concurrency
//!
//! `RoaringBitmap` uses interior mutability for the heap mirror and is intended for single-writer use.
//! The stable memory region should not be mutated through another wrapper while a bitmap instance
//! is in use.
//!
//! # Example
//!
//! ```rust
//! # use ic_stable_roaring::StableRoaringBitmap;
//! # use ic_stable_structures::DefaultMemoryImpl;
//! let memory = DefaultMemoryImpl::default();
//! let bitset = StableRoaringBitmap::new(memory).unwrap();
//! bitset.insert(7).unwrap();
//! assert!(bitset.contains(7));
//! ```

pub mod bitmap;
mod memory;

/// Number of journal slots on stable memory (compile-time constant). Must match the `u64` at
/// header offset 12 on disk.
pub const JOURNAL_CAP_SLOTS: usize = 4096;

/// Byte length of the on-disk journal region (`JOURNAL_CAP_SLOTS` records × 5 bytes each).
pub const JOURNAL_REGION_BYTES: usize = JOURNAL_CAP_SLOTS * 5;

/// `Memory::read` chunk size during journal replay (must divide `JOURNAL_REGION_BYTES` and 5).
pub const JOURNAL_READ_CHUNK_BYTES: usize = 5120;

const _: () = assert!(JOURNAL_REGION_BYTES.is_multiple_of(JOURNAL_READ_CHUNK_BYTES));
const _: () = assert!(JOURNAL_READ_CHUNK_BYTES.is_multiple_of(5));

/// Bit mask for one on-disk journal record: **40 low bits** of a little-endian 5-byte encoding.
pub const JOURNAL_RECORD_RAW_MASK: u64 = (1u64 << 40) - 1;

/// Maximum exclusive logical length (`len_bits`) and maximum `SetLen` value supported by the API.
///
/// Bit indices are `u32`; the exclusive logical length may be `u32::MAX + 1`.
pub const JOURNAL_LEN_MAX: u64 = (u32::MAX as u64) + 1;

pub use bitmap::RoaringBitmap as StableRoaringBitmap;
pub use bitmap::{ContainsView, InitError, RoaringBitmap};
pub use memory::GrowFailed;
