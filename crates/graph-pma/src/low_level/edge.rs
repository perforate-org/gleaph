//! Hot adjacency-entry types and directional surface descriptors.

use gleaph_graph_kernel::{LabelId, NodeId};

use super::region::RegionRef;
use super::vertex::{LABEL_ID_MASK, TOMBSTONE_MASK};

/// Directional adjacency surface.
///
/// Forward is source-major and reverse is destination-major. The two surfaces
/// are logically separate even when they share the same stable-memory arena.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceKind {
    Forward = 0,
    Reverse = 1,
}

/// Physical position of one edge inside a directional surface.
///
/// This is not semantic edge identity. It is the storage-level locator that
/// says "which surface, which vertex-local neighborhood, which ordinal slot".
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeLocator {
    pub surface: u8,
    pub reserved: [u8; 3],
    pub vertex: NodeId,
    pub ordinal: u32,
}

impl EdgeLocator {
    /// Creates one physical locator inside a directional surface.
    pub const fn new(surface: SurfaceKind, vertex: NodeId, ordinal: u32) -> Self {
        Self {
            surface: surface as u8,
            reserved: [0; 3],
            vertex,
            ordinal,
        }
    }

    /// Returns the typed surface represented by this locator.
    pub fn surface_kind(self) -> SurfaceKind {
        match self.surface {
            0 => SurfaceKind::Forward,
            1 => SurfaceKind::Reverse,
            _ => panic!("invalid surface kind"),
        }
    }
}

/// Region bundle that defines one directional adjacency surface.
///
/// A surface is composed from a vertex table, an edge-entry region, a label
/// sidecar, and a segment-log region.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct SurfaceRegions {
    pub vertex_table: RegionRef,
    pub edge_entries: RegionRef,
    pub label_index: RegionRef,
    pub segment_log: RegionRef,
}

impl SurfaceRegions {
    /// Bundles the four regions that make up one directional surface.
    pub const fn new(
        vertex_table: RegionRef,
        edge_entries: RegionRef,
        label_index: RegionRef,
        segment_log: RegionRef,
    ) -> Self {
        Self {
            vertex_table,
            edge_entries,
            label_index,
            segment_log,
        }
    }
}

/// Packed hot metadata for an [`EdgeEntry`].
///
/// Layout:
/// - high bit: tombstone
/// - low 15 bits: label id
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeMeta(u16);

impl EdgeMeta {
    /// Packed edge metadata with no label and no tombstone bit set.
    pub const UNLABELED: Self = Self(0);

    /// Packs a label id and tombstone bit into the fixed 16-bit edge-meta layout.
    pub fn new(label_id: LabelId, tombstone: bool) -> Self {
        assert!(
            label_id <= LABEL_ID_MASK,
            "label id exceeds 15-bit edge meta layout"
        );
        let tombstone_bits = if tombstone { TOMBSTONE_MASK } else { 0 };
        Self(tombstone_bits | label_id)
    }

    /// Wraps a raw packed 16-bit value.
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Returns the raw packed representation.
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Returns the stored label id.
    pub const fn label_id(self) -> LabelId {
        self.0 & LABEL_ID_MASK
    }

    /// Returns whether the tombstone bit is set.
    pub const fn is_tombstone(self) -> bool {
        (self.0 & TOMBSTONE_MASK) != 0
    }

    /// Returns a copy with only the tombstone bit changed.
    pub fn with_tombstone(self, tombstone: bool) -> Self {
        Self::new(self.label_id(), tombstone)
    }

    /// Returns a copy with only the label id changed.
    pub fn with_label_id(self, label_id: LabelId) -> Self {
        Self::new(label_id, self.is_tombstone())
    }
}

/// Fixed-size hot adjacency entry.
///
/// This is the VCSR/DGAP-style base entry stored in a surface edge region.
/// It intentionally contains only the neighbor node id and edge-local hot
/// metadata.
///
/// Invariant:
/// - one `EdgeEntry` is always exactly 8 bytes
/// - `target` is a packed kernel `NodeId`
/// - semantic edge identity is stored elsewhere, not here
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeEntry {
    pub target: NodeId,
    pub meta: EdgeMeta,
}

impl EdgeEntry {
    /// Creates one fixed-size hot adjacency entry.
    pub const fn new(target: NodeId, meta: EdgeMeta) -> Self {
        Self { target, meta }
    }
}

const _: [(); 16] = [(); core::mem::size_of::<EdgeLocator>()];
const _: [(); 64] = [(); core::mem::size_of::<SurfaceRegions>()];
const _: [(); 8] = [(); core::mem::size_of::<EdgeEntry>()];
const _: [(); 2] = [(); core::mem::size_of::<EdgeMeta>()];

#[cfg(test)]
mod tests {
    use super::{EdgeEntry, EdgeLocator, EdgeMeta, SurfaceKind, SurfaceRegions};
    use crate::low_level::{RegionKind, RegionRef, RegionStorageKind};
    use gleaph_graph_kernel::NodeId;

    #[test]
    fn edge_entry_has_expected_abi() {
        assert_eq!(core::mem::size_of::<EdgeEntry>(), 8);
        assert_eq!(core::mem::size_of::<NodeId>(), 6);
        assert_eq!(core::mem::size_of::<EdgeMeta>(), 2);
    }

    #[test]
    fn edge_meta_packs_label_and_tombstone() {
        let meta = EdgeMeta::new(42, true);
        assert_eq!(meta.label_id(), 42);
        assert!(meta.is_tombstone());
    }

    #[test]
    fn edge_entry_uses_packed_kernel_node_id() {
        let target = NodeId::from(7u8);
        let entry = EdgeEntry::new(target, EdgeMeta::new(3, false));
        assert_eq!(u64::from(entry.target), 7);
        assert_eq!(entry.meta.label_id(), 3);
        assert!(!entry.meta.is_tombstone());
    }

    #[test]
    fn edge_locator_carries_surface_and_vertex_local_slot() {
        let locator = EdgeLocator::new(SurfaceKind::Reverse, NodeId::from(9u8), 17);
        assert_eq!(locator.surface_kind(), SurfaceKind::Reverse);
        assert_eq!(u64::from(locator.vertex), 9);
        assert_eq!(locator.ordinal, 17);
    }

    #[test]
    fn surface_regions_group_directional_region_refs() {
        let forward = SurfaceRegions::new(
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardVertexTable,
                1,
                128,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardEdgeEntries,
                2,
                4096,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardLabelIndex,
                3,
                256,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardSegmentLog,
                4,
                1024,
            ),
        );

        assert_eq!(
            forward.vertex_table.region_kind(),
            RegionKind::ForwardVertexTable
        );
        assert_eq!(
            forward.edge_entries.region_kind(),
            RegionKind::ForwardEdgeEntries
        );
    }
}
