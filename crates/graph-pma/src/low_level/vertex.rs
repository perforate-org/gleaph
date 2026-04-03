//! Vertex-table entries and surface-local label-sidecar records.

use gleaph_graph_kernel::LabelId;

use super::ids::EdgeRef;

/// High bit reserved for the tombstone flag inside `EdgeMeta`.
pub const TOMBSTONE_MASK: u16 = 1 << 15;
/// Low 15 bits reserved for the packed label id inside `EdgeMeta`.
pub const LABEL_ID_MASK: u16 = !TOMBSTONE_MASK;
/// Bit 31: vertex tombstone.
pub const VERTEX_TOMBSTONE_BIT: u32 = 1 << 31;
/// Bit 30: overflow-head empty sentinel.
pub const LOG_EMPTY_BIT: u32 = 1 << 30;
/// Low 30 bits: overflow-head offset.
pub const LOG_OFFSET_BITS_MASK: u32 = (1 << 30) - 1;
/// Packed raw value meaning "no overflow chain" and "not tombstoned".
pub const EMPTY_LOG_OFFSET: i32 = LOG_EMPTY_BIT as i32;

/// Compatibility wrapper for the legacy single-extent edge-index model.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeIndex {
    pub raw: u64,
}

impl EdgeIndex {
    /// Creates a surface-local base-entry index expressed in `EdgeEntry` units.
    pub const fn new(raw: u64) -> Self {
        Self { raw }
    }

    /// Returns a new edge index advanced by `delta` `EdgeEntry` slots.
    pub fn checked_add(self, delta: u32) -> Option<Self> {
        let edge_ref = self.as_edge_ref();
        match edge_ref.start_slot().checked_add(delta as u64) {
            Some(start_slot) => Some(Self {
                raw: edge_ref.with_start_slot(start_slot).raw(),
            }),
            None => None,
        }
    }

    /// Returns the packed edge-ref interpretation of this index.
    pub const fn as_edge_ref(self) -> EdgeRef {
        EdgeRef::from_raw(self.raw)
    }

    /// Returns the decoded segment id.
    pub const fn segment_id(self) -> u32 {
        self.as_edge_ref().segment_id()
    }

    /// Returns the decoded start slot.
    pub const fn start_slot(self) -> u64 {
        self.as_edge_ref().start_slot()
    }
}

impl From<EdgeIndex> for EdgeRef {
    fn from(value: EdgeIndex) -> Self {
        EdgeRef::new(0, value.raw)
    }
}

impl From<EdgeRef> for EdgeIndex {
    fn from(value: EdgeRef) -> Self {
        Self { raw: value.raw() }
    }
}

/// Per-vertex base-neighborhood locator.
///
/// `edge_ref` names the base-neighborhood start for one vertex inside a
/// directional surface. `log_offset` points to overflow state outside the base
/// interval.
///
/// Invariant:
/// - the base neighborhood is represented as one contiguous interval
/// - `edge_ref` encodes a segment id and start slot in `EdgeEntry` units
/// - overflow entries are never folded into `degree`
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexEntry {
    pub edge_index: EdgeIndex,
    pub degree: u32,
    /// Packed metadata:
    /// - bit31: vertex tombstone
    /// - bit30: empty overflow sentinel
    /// - low 30 bits: overflow head offset
    pub log_offset: i32,
}

impl VertexEntry {
    /// Creates one vertex-table entry for a contiguous base interval.
    pub const fn new(edge_index: EdgeIndex, degree: u32, log_offset: i32) -> Self {
        let normalized = if log_offset == -1 {
            EMPTY_LOG_OFFSET
        } else {
            log_offset
        };
        Self {
            edge_index,
            degree,
            log_offset: normalized,
        }
    }

    /// Returns the encoded base-neighborhood reference.
    pub const fn edge_ref(self) -> EdgeRef {
        self.edge_index.as_edge_ref()
    }

    /// Returns the surface-local start slot for this vertex.
    pub const fn start_slot(self) -> u64 {
        self.edge_index.start_slot()
    }

    /// Returns the segment id backing this vertex's base neighborhood.
    pub const fn segment_id(self) -> u32 {
        self.edge_index.segment_id()
    }

    /// Returns whether `other` starts in the same edge segment.
    pub const fn is_in_same_segment_as(self, other: Self) -> bool {
        self.segment_id() == other.segment_id()
    }

    /// Derives the reserved base-span length for this vertex.
    ///
    /// When the next ordinal starts in the same segment, its `start_slot`
    /// closes the current reservation. Otherwise the reservation extends to the
    /// end of the current segment.
    pub fn reserved_span_len(
        self,
        next_ordinal_entry: Option<Self>,
        segment_slot_capacity: u64,
    ) -> Option<u64> {
        let start = self.start_slot();
        let end = match next_ordinal_entry {
            Some(next) if self.is_in_same_segment_as(next) => next.start_slot(),
            _ => segment_slot_capacity,
        };
        end.checked_sub(start)
    }

    /// Returns whether this vertex currently points at an overflow chain.
    pub const fn has_overflow(self) -> bool {
        (self.log_offset as u32 & LOG_EMPTY_BIT) == 0
    }

    /// Returns whether this vertex is tombstoned.
    pub const fn is_tombstone(self) -> bool {
        (self.log_offset as u32 & VERTEX_TOMBSTONE_BIT) != 0
    }

    /// Returns the decoded overflow-head offset when present.
    pub const fn overflow_head(self) -> Option<u32> {
        if !self.has_overflow() {
            return None;
        }
        Some(self.log_offset as u32 & LOG_OFFSET_BITS_MASK)
    }

    /// Sets the vertex tombstone bit while preserving overflow metadata.
    pub fn with_tombstone(self, tombstone: bool) -> Self {
        let mut raw = self.log_offset as u32;
        if tombstone {
            raw |= VERTEX_TOMBSTONE_BIT;
        } else {
            raw &= !VERTEX_TOMBSTONE_BIT;
        }
        Self {
            edge_index: self.edge_index,
            degree: self.degree,
            log_offset: raw as i32,
        }
    }

    /// Replaces the overflow-head pointer.
    pub fn with_overflow_head(self, head: Option<u32>) -> Self {
        let mut raw = self.log_offset as u32;
        raw &= !(LOG_EMPTY_BIT | LOG_OFFSET_BITS_MASK);
        match head {
            Some(offset) => {
                raw |= offset & LOG_OFFSET_BITS_MASK;
            }
            None => {
                raw |= LOG_EMPTY_BIT;
            }
        }
        Self {
            edge_index: self.edge_index,
            degree: self.degree,
            log_offset: raw as i32,
        }
    }
}

/// Per-vertex pointer into the label-range sidecar.
///
/// This says where the list of label subranges for one vertex starts and how
/// many entries it contains.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexLabelIndexEntry {
    pub start: u32,
    pub len: u32,
}

impl VertexLabelIndexEntry {
    /// Creates one pointer into the label-range sidecar for a vertex.
    pub const fn new(start: u32, len: u32) -> Self {
        Self { start, len }
    }
}

/// One exact-label subrange inside a vertex-local base neighborhood.
///
/// `start` and `len` are expressed relative to the surface edge-entry region in
/// `EdgeEntry` units.
///
/// Invariant:
/// - each label range refers to a contiguous subrange of the base neighborhood
/// - overflow/log state is not described here
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexLabelRange {
    pub label_id: LabelId,
    pub start: u32,
    pub len: u32,
}

impl VertexLabelRange {
    /// Creates one contiguous same-label run inside the base interval.
    pub const fn new(label_id: LabelId, start: u32, len: u32) -> Self {
        Self {
            label_id,
            start,
            len,
        }
    }
}

const _: [(); 8] = [(); core::mem::size_of::<EdgeIndex>()];
const _: [(); 16] = [(); core::mem::size_of::<VertexEntry>()];
const _: [(); 8] = [(); core::mem::size_of::<VertexLabelIndexEntry>()];

#[cfg(test)]
mod tests {
    use super::{EMPTY_LOG_OFFSET, EdgeIndex, LOG_EMPTY_BIT, VertexEntry};

    #[test]
    fn vertex_entry_has_expected_abi() {
        let entry = VertexEntry::new(EdgeIndex::new(42), 3, EMPTY_LOG_OFFSET);

        assert_eq!(core::mem::size_of::<VertexEntry>(), 16);
        assert_eq!(EMPTY_LOG_OFFSET as u32, LOG_EMPTY_BIT);
        assert_eq!(entry.edge_ref().raw(), 42);
    }

    #[test]
    fn reserved_span_len_uses_next_vertex_when_segment_matches() {
        let current = VertexEntry::new(EdgeIndex::new((3_u64 << 40) | 10), 2, EMPTY_LOG_OFFSET);
        let next = VertexEntry::new(EdgeIndex::new((3_u64 << 40) | 18), 1, EMPTY_LOG_OFFSET);

        assert_eq!(current.reserved_span_len(Some(next), 64), Some(8));
    }

    #[test]
    fn reserved_span_len_falls_back_to_segment_end_when_segment_changes() {
        let current = VertexEntry::new(EdgeIndex::new((3_u64 << 40) | 18), 2, EMPTY_LOG_OFFSET);
        let next = VertexEntry::new(EdgeIndex::new((4_u64 << 40) | 2), 1, EMPTY_LOG_OFFSET);

        assert_eq!(current.reserved_span_len(Some(next), 64), Some(46));
    }

    #[test]
    fn reserved_span_len_returns_none_for_invalid_descending_span() {
        let current = VertexEntry::new(EdgeIndex::new((3_u64 << 40) | 18), 2, EMPTY_LOG_OFFSET);
        let next = VertexEntry::new(EdgeIndex::new((3_u64 << 40) | 12), 1, EMPTY_LOG_OFFSET);

        assert_eq!(current.reserved_span_len(Some(next), 64), None);
    }
}
