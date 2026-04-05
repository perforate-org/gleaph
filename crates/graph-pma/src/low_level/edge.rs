//! Hot adjacency-entry types and directional surface descriptors.

use gleaph_graph_kernel::LabelId;

use super::ids::VertexRef;
use super::region::RegionRef;
use super::vertex::TOMBSTONE_MASK;

/// Bit 14: when set, [`EdgeMeta`] holds a shard-canister slot in the payload, not a local label id.
pub const EDGE_SHARD_CANISTER_MASK: u16 = 1 << 14;

/// Low 14 bits: local [`LabelId`] (cross-shard flag clear) or [`ShardCanisterSlot`](super::shard_canister::ShardCanisterSlot).
pub const EDGE_META_PAYLOAD_MASK: u16 = (1 << 14) - 1;

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

/// Logical position of one edge inside a directional surface.
///
/// This does not depend on packed-memory physical slot assignment. It
/// identifies one edge by `(surface, vertex_ref, logical_index)` plus whether
/// the edge currently lives in base or overflow storage.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct LogicalEdgeLocator {
    pub surface: u8,
    pub slot_kind: u8,
    pub reserved: [u8; 2],
    pub vertex_ref: VertexRef,
    pub logical_index: u32,
}

impl LogicalEdgeLocator {
    const BASE_SLOT_KIND: u8 = 0;
    const OVERFLOW_SLOT_KIND: u8 = 1;

    /// Creates one logical base-edge locator.
    pub fn base(
        surface: SurfaceKind,
        vertex_ref: impl Into<VertexRef>,
        logical_index: u32,
    ) -> Self {
        Self {
            surface: surface as u8,
            slot_kind: Self::BASE_SLOT_KIND,
            reserved: [0; 2],
            vertex_ref: vertex_ref.into(),
            logical_index,
        }
    }

    /// Creates one logical overflow-edge locator.
    pub fn overflow(
        surface: SurfaceKind,
        vertex_ref: impl Into<VertexRef>,
        logical_index: u32,
    ) -> Self {
        Self {
            surface: surface as u8,
            slot_kind: Self::OVERFLOW_SLOT_KIND,
            reserved: [0; 2],
            vertex_ref: vertex_ref.into(),
            logical_index,
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

    /// Returns whether this locator points into overflow storage.
    pub const fn is_overflow(self) -> bool {
        self.slot_kind == Self::OVERFLOW_SLOT_KIND
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
/// - bit 15: tombstone
/// - bit 14: shard-canister flag (`1` = payload is a shard slot id; `0` = payload is a local label id)
/// - bits 0–13: payload (14 bits)
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeMeta(u16);

impl EdgeMeta {
    /// Packed edge metadata with no label, no tombstone, and no cross-shard flag.
    pub const UNLABELED: Self = Self(0);

    /// Packs a local label id and tombstone bit (same-canister target).
    pub fn new(label_id: LabelId, tombstone: bool) -> Self {
        assert!(
            label_id <= EDGE_META_PAYLOAD_MASK,
            "label id exceeds 14-bit edge meta layout"
        );
        let tombstone_bits = if tombstone { TOMBSTONE_MASK } else { 0 };
        Self(tombstone_bits | label_id)
    }

    /// Packs a remote shard slot id and tombstone bit (`target` names a vertex in that canister).
    pub fn new_shard_canister(shard_slot: u16, tombstone: bool) -> Self {
        assert!(
            shard_slot <= EDGE_META_PAYLOAD_MASK,
            "shard canister slot exceeds 14-bit edge meta layout"
        );
        let tombstone_bits = if tombstone { TOMBSTONE_MASK } else { 0 };
        Self(tombstone_bits | EDGE_SHARD_CANISTER_MASK | shard_slot)
    }

    /// Wraps a raw packed 16-bit value.
    #[inline]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// Returns the raw packed representation.
    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Low 14 bits (local label id or shard slot id).
    #[inline]
    pub const fn payload(self) -> u16 {
        self.0 & EDGE_META_PAYLOAD_MASK
    }

    /// Returns whether the tombstone bit is set.
    #[inline]
    pub const fn is_tombstone(self) -> bool {
        (self.0 & TOMBSTONE_MASK) != 0
    }

    /// Returns whether the neighbor lives in another canister (payload is a shard slot id).
    #[inline]
    pub const fn is_shard_canister(self) -> bool {
        (self.0 & EDGE_SHARD_CANISTER_MASK) != 0
    }

    /// Local label id when this edge targets a vertex in the same canister.
    #[inline]
    pub const fn local_label_id(self) -> Option<LabelId> {
        if self.is_shard_canister() {
            None
        } else {
            Some(self.payload())
        }
    }

    /// Shard slot id when this edge targets another canister.
    #[inline]
    pub const fn shard_canister_slot(self) -> Option<u16> {
        if self.is_shard_canister() {
            Some(self.payload())
        } else {
            None
        }
    }

    /// Returns the stored label id for **local** edges; for cross-shard edges returns the payload bits
    /// (a slot id, not a graph label). Prefer [`Self::local_label_id`] when filtering by label.
    #[inline]
    pub const fn label_id(self) -> LabelId {
        self.payload()
    }

    /// Returns a copy with only the tombstone bit changed.
    pub fn with_tombstone(self, tombstone: bool) -> Self {
        Self((self.0 & !TOMBSTONE_MASK) | if tombstone { TOMBSTONE_MASK } else { 0 })
    }

    /// Returns a copy with a local label id (clears the cross-shard flag).
    pub fn with_label_id(self, label_id: LabelId) -> Self {
        Self::new(label_id, self.is_tombstone())
    }

    /// Returns a copy with a shard slot (sets the cross-shard flag).
    pub fn with_shard_canister_slot(self, shard_slot: u16) -> Self {
        Self::new_shard_canister(shard_slot, self.is_tombstone())
    }
}

/// Fixed-size hot adjacency entry.
///
/// This is the VCSR/DGAP-style base entry stored in a surface edge region.
/// It intentionally contains only the neighbor vertex ref and edge-local hot
/// metadata.
///
/// Invariant:
/// - one `EdgeEntry` is always exactly 8 bytes
/// - `target` is a packed low-level `VertexRef`
/// - semantic edge identity is stored elsewhere, not here
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeEntry {
    pub target: VertexRef,
    pub meta: EdgeMeta,
}

impl EdgeEntry {
    /// Creates one fixed-size hot adjacency entry.
    pub fn new(target: impl Into<VertexRef>, meta: EdgeMeta) -> Self {
        Self {
            target: target.into(),
            meta,
        }
    }
}

const _: [(); 64] = [(); core::mem::size_of::<SurfaceRegions>()];
const _: [(); 8] = [(); core::mem::size_of::<EdgeEntry>()];
const _: [(); 2] = [(); core::mem::size_of::<EdgeMeta>()];

#[cfg(test)]
mod tests {
    use super::{EdgeEntry, EdgeMeta, LogicalEdgeLocator, SurfaceKind, SurfaceRegions};
    use crate::low_level::VertexRef;
    use crate::low_level::{RegionKind, RegionRef, RegionStorageKind};

    #[test]
    fn edge_entry_has_expected_abi() {
        assert_eq!(core::mem::size_of::<EdgeEntry>(), 8);
        assert_eq!(core::mem::size_of::<VertexRef>(), 6);
        assert_eq!(core::mem::size_of::<EdgeMeta>(), 2);
    }

    #[test]
    fn edge_meta_packs_label_and_tombstone() {
        let meta = EdgeMeta::new(42, true);
        assert_eq!(meta.local_label_id(), Some(42));
        assert!(meta.is_tombstone());
        assert!(!meta.is_shard_canister());
    }

    #[test]
    fn edge_meta_shard_canister_roundtrip() {
        let meta = EdgeMeta::new_shard_canister(99, false);
        assert_eq!(meta.shard_canister_slot(), Some(99));
        assert_eq!(meta.local_label_id(), None);
        assert!(!meta.is_tombstone());
        assert!(meta.is_shard_canister());
    }

    #[test]
    fn edge_entry_uses_packed_kernel_node_id() {
        let target = VertexRef::from(7u8);
        let entry = EdgeEntry::new(target, EdgeMeta::new(3, false));
        assert_eq!(u64::from(entry.target), 7);
        assert_eq!(entry.meta.local_label_id(), Some(3));
        assert!(!entry.meta.is_tombstone());
    }

    #[test]
    fn logical_edge_locator_carries_surface_and_vertex_local_slot() {
        let locator = LogicalEdgeLocator::base(SurfaceKind::Reverse, VertexRef::from(9u8), 17);
        assert_eq!(locator.surface_kind(), SurfaceKind::Reverse);
        assert_eq!(u64::from(locator.vertex_ref), 9);
        assert_eq!(locator.logical_index, 17);
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
