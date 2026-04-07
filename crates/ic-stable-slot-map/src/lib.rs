//! **Stable slot map** (V1 magic `SSM`): generational keys and a freelist over a growable
//! slot array in Internet Computer stable memory.
//!
//! The main type is [`SlotMap`] (also exported as [`StableSlotMap`]); see its inherent methods
//! for allocation, lookup, update, removal, and [`SlotMap::iter_occupied`]. Supporting types
//! include [`SlotKey`], [`InitError`], [`SlotMapError`], and [`GrowFailed`].
//!
//! # Logical layout (`Memory` bytes)
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! | Magic `SSM` (3) + version (1) + live_count (8)   |
//! | max_cell_size (4) + is_fixed_cell (1) + reserved |
//! | slot_capacity (8) + free_head u32 (4) + reserved |
//! -------------------------------------------------- <- offset 64
//! | SlotCell<T>_0  (fixed width per T)               |
//! | SlotCell<T>_1                                    |
//! | ...                                              |
//! --------------------------------------------------
//! ```
//!
//! Each slot cell is either **occupied** (`generation` + `T`) or **vacant**
//! (`generation` for the next allocation at this index + `next_free: u32`). The cell tail is
//! `max(encoded_size(T), 4)` bytes so small `T` still fit a freelist link. [`SlotKey`] pairs
//! `(index, generation)`; after [`SlotMap::remove`], the generation at that index is bumped so
//! old keys become stale.
//!
//! # Limits
//!
//! - Slot indices are `u32`; `slot_capacity` must not exceed `u32::MAX` (enforced on grow).
//! - Generations are `u32` and wrap on overflow (extremely unlikely in practice).
//!
//! # Complexity
//!
//! Insert, remove, get, and set are amortized **O(1)**; growing capacity copies all existing slots
//! (**O(slot_capacity)**). [`SlotMap::iter_occupied`] scans all physical slots in
//! **O(slot_capacity)** (one byte minimum per vacant cell; full decode for occupied cells).
//!
//! Iterating while mutating the same `ic_stable_structures::Memory` region through other handles can yield inconsistent
//! results; treat the map as exclusively borrowed for the iterator lifetime.

mod memory;
mod slot;
mod slot_cell;
mod slot_map;
mod storable;
mod types;

pub use memory::GrowFailed;
pub use slot_map::SlotMap as StableSlotMap;
pub use slot_map::{FREELIST_EMPTY, InitError, IterOccupied, SlotKey, SlotMap, SlotMapError};
