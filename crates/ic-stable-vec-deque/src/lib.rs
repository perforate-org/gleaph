//! **Stable deque** in Internet Computer stable memory: **V1 segmented block-ring** layout with
//! magic **`SVD`** and a 128-byte header. Elements live in fixed-size blocks routed through an
//! on-structure directory: logical index `i` sits at virtual position `(headOff + i) % virtCap`,
//! i.e. block `k = r / blockSlots`, slot `k' = r % blockSlots`, at byte address
//! `dir[k] + k' · SLOT_SIZE`. Drained top-most blocks are recycled through an intrusive free list.
//!
//! The main type is [`VecDeque`], also re-exported as [`StableVecDeque`].
//!
//! # Operations
//!
//! - **O(1)** [`VecDeque::push_front`], [`VecDeque::push_back`], [`VecDeque::pop_front`],
//!   [`VecDeque::pop_back`], [`VecDeque::get`], [`VecDeque::set`] (by logical index): each performs
//!   at most one element encode/decode plus O(64 bytes) of header writes.
//! - Pushing into a full deque additionally appends one block of `blockSlots · SLOT_SIZE` bytes
//!   page-aligned at the end of memory (reused from the free list when one is available), rotates
//!   and at most once doubles the directory (`8 · dirSlots ≤ 8 · (len/blockSlots + 1)` metadata
//!   bytes), and migrates at most one block of boundary slots into the new block. All envelopes
//!   are constants for a fixed element type at a fixed moment in time; growth never relocates
//!   elements in bulk and no operation's cost grows with `len`.
//!
//! # Type parameters
//!
//! - `T`: must be [`ic_stable_structures::Storable`] with a **bounded** layout.
//! - `M`: [`ic_stable_structures::Memory`] (e.g. [`DefaultMemoryImpl`](ic_stable_structures::DefaultMemoryImpl)).
//!
//! All mutation uses `&self` and [`Memory`](ic_stable_structures::Memory); avoid aliasing the same
//! byte range with another mutating wrapper while an iterator is alive.

mod memory;

pub use memory::GrowFailed;
mod slot;
mod storable;
mod types;
mod vec_deque;

pub use vec_deque::Iter;
pub use vec_deque::{HeaderV1, InitError};
pub use {vec_deque::VecDeque as StableVecDeque, vec_deque::VecDeque};
