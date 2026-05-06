//! **Stable deque** in Internet Computer stable memory: V1 layout with magic **`SVD`**, same
//! 64-byte header prefix as [`ic_stable_structures::vec::Vec`] (`SVC`), plus ring-buffer `head` and
//! `capacity`. Elements are stored from byte offset **64**; logical index `i` maps to physical slot
//! `(head + i) % capacity`.
//!
//! The main type is [`VecDeque`], also re-exported as [`StableVecDeque`].
//!
//! # Operations
//!
//! - **O(1)** amortized [`VecDeque::push_front`], [`VecDeque::push_back`], [`VecDeque::pop_front`],
//!   [`VecDeque::pop_back`], [`VecDeque::get`], [`VecDeque::set`] (by logical index).
//! - Growing when `len == capacity` linearizes the ring into slots `0..len` and may double capacity:
//!   **O(len)** work plus stable memory growth.
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
