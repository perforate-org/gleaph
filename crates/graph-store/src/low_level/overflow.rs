//! DGAP-style overflow-log descriptors.

use gleaph_graph_kernel::EdgeId;

use super::edge::{EdgeEntry, SurfaceKind};
use super::ids::VertexRef;
use super::vertex::{EMPTY_LOG_OFFSET, LOG_EMPTY_BIT, LOG_OFFSET_BITS_MASK};

/// Offset into a surface-local overflow log.
///
/// Invariant:
/// - `EMPTY_LOG_OFFSET` means "no overflow"
/// - non-empty offsets are surface-local log positions, not stable-memory
///   addresses
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LogOffset {
    pub raw: i32,
}

impl LogOffset {
    pub const EMPTY: Self = Self {
        raw: EMPTY_LOG_OFFSET,
    };

    /// Creates a surface-local overflow-log offset from its raw slot index.
    pub const fn new(raw: i32) -> Self {
        if raw == -1 {
            return Self::EMPTY;
        }
        Self { raw }
    }

    /// Returns whether this offset is the empty-chain sentinel.
    pub const fn is_empty(self) -> bool {
        (self.raw as u32 & LOG_EMPTY_BIT) != 0
    }

    /// Returns the decoded overflow-log slot index when not empty.
    pub const fn index(self) -> Option<usize> {
        if self.is_empty() {
            return None;
        }
        Some((self.raw as u32 & LOG_OFFSET_BITS_MASK) as usize)
    }
}

/// One overflow entry stored outside the base contiguous neighborhood.
///
/// This is the DGAP-side mutation carrier: it keeps the hot edge entry together
/// with semantic identity and a link to the next overflow entry for the same
/// vertex-local view.
///
/// Invariant:
/// - overflow entries never change the meaning of the base interval itself
/// - `next` links entries within one surface-local overflow chain
/// - `edge_id` is semantic identity and is not part of `EdgeEntry`
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct OverflowEntry {
    pub edge_id: EdgeId,
    pub entry: EdgeEntry,
    pub next: LogOffset,
}

impl OverflowEntry {
    /// Creates one overflow-log entry linked to the next entry in the chain.
    pub const fn new(edge_id: EdgeId, entry: EdgeEntry, next: LogOffset) -> Self {
        Self {
            edge_id,
            entry,
            next,
        }
    }
}

/// Read-side descriptor for one vertex-local overflow chain.
///
/// This does not expose allocator placement. It only says which surface owns
/// the chain, which vertex-local neighborhood it belongs to, and where the
/// first overflow entry lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OverflowChain {
    pub surface: SurfaceKind,
    pub vertex_ref: VertexRef,
    pub head: LogOffset,
}

impl OverflowChain {
    /// Creates one vertex-local overflow-chain descriptor.
    pub fn new(surface: SurfaceKind, vertex_ref: impl Into<VertexRef>, head: LogOffset) -> Self {
        Self {
            surface,
            vertex_ref: vertex_ref.into(),
            head,
        }
    }

    /// Returns whether this chain contains no overflow entries.
    pub const fn is_empty(self) -> bool {
        self.head.is_empty()
    }
}

const _: [(); 4] = [(); core::mem::size_of::<LogOffset>()];
const _: [(); 24] = [(); core::mem::size_of::<OverflowEntry>()];
const _: [(); 12] = [(); core::mem::size_of::<OverflowChain>()];

#[cfg(test)]
mod tests {
    use super::{LogOffset, OverflowChain, OverflowEntry};
    use crate::low_level::{EdgeEntry, EdgeMeta, SurfaceKind, VertexRef};

    #[test]
    fn log_offset_uses_empty_sentinel() {
        assert!(LogOffset::EMPTY.is_empty());
        assert!(!LogOffset::new(0).is_empty());
    }

    #[test]
    fn overflow_entry_keeps_semantic_id_outside_edge_entry() {
        let entry = OverflowEntry::new(
            42,
            EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
            LogOffset::EMPTY,
        );

        assert_eq!(entry.edge_id, 42);
        assert_eq!(u64::from(entry.entry.target), 9);
        assert_eq!(entry.entry.meta.local_id(), Some(3));
        assert!(entry.next.is_empty());
    }

    #[test]
    fn overflow_chain_is_surface_local_and_vertex_local() {
        let chain = OverflowChain::new(
            SurfaceKind::Reverse,
            VertexRef::from(7u8),
            LogOffset::new(11),
        );

        assert_eq!(chain.surface, SurfaceKind::Reverse);
        assert_eq!(u64::from(chain.vertex_ref), 7);
        assert_eq!(chain.head.raw, 11);
        assert!(!chain.is_empty());
    }
}
