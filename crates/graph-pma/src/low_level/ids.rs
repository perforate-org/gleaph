//! Allocator-level address types.

/// Physical byte address inside the stable-memory address space.
///
/// This is allocator-level metadata. Adjacency code should prefer surface-local
/// indexes such as [`EdgeIndex`](crate::low_level::EdgeIndex) over raw
/// addresses.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct StableAddr(pub u64);
