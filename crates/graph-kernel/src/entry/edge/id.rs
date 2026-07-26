//! Row-local logical edge position shared with LARA traversal.

/// Edge slot metadata uses the same tombstone-inclusive row-local position as
/// LARA traversal. Orientation is carried separately by the owning occurrence.
pub use ic_stable_lara::traverse::BucketEntryPosition as EdgeSlotIndex;
