//! Vertex-table entries and surface-local label-sidecar records.

use gleaph_graph_kernel::LabelId;

/// High bit reserved for the tombstone flag inside `EdgeMeta`.
pub const TOMBSTONE_MASK: u16 = 1 << 15;
/// Low 15 bits reserved for the packed label id inside `EdgeMeta`.
pub const LABEL_ID_MASK: u16 = !TOMBSTONE_MASK;
/// Sentinel log offset meaning "no overflow chain".
pub const EMPTY_LOG_OFFSET: i32 = -1;

/// Surface-local index into an `EdgeEntry` region.
///
/// This is expressed in units of `EdgeEntry`, not bytes.
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
    pub const fn checked_add(self, delta: u32) -> Option<Self> {
        match self.raw.checked_add(delta as u64) {
            Some(raw) => Some(Self { raw }),
            None => None,
        }
    }
}

/// Per-vertex base-neighborhood locator.
///
/// `edge_index .. edge_index + degree` names the contiguous base interval for
/// one vertex inside a directional surface. `log_offset` points to overflow
/// state outside the base interval.
///
/// Invariant:
/// - the base neighborhood is represented as one contiguous interval
/// - `edge_index` is expressed in `EdgeEntry` units, not bytes
/// - overflow entries are never folded into `degree`
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct VertexEntry {
    pub edge_index: EdgeIndex,
    pub degree: u32,
    pub log_offset: i32,
}

impl VertexEntry {
    /// Creates one vertex-table entry for a contiguous base interval.
    pub const fn new(edge_index: EdgeIndex, degree: u32, log_offset: i32) -> Self {
        Self {
            edge_index,
            degree,
            log_offset,
        }
    }

    /// Returns whether this vertex currently points at an overflow chain.
    pub const fn has_overflow(self) -> bool {
        self.log_offset != EMPTY_LOG_OFFSET
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
    use super::{EdgeIndex, VertexEntry, EMPTY_LOG_OFFSET};

    #[test]
    fn vertex_entry_has_expected_abi() {
        let entry = VertexEntry::new(EdgeIndex::new(42), 3, EMPTY_LOG_OFFSET);

        assert_eq!(core::mem::size_of::<VertexEntry>(), 16);
        assert_eq!(EMPTY_LOG_OFFSET, -1);
        assert_eq!(entry.edge_index.raw, 42);
    }
}
