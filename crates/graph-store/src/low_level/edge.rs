//! Hot adjacency-entry types and directional surface descriptors.

use bitflags::bitflags;
use gleaph_graph_kernel::LabelId;

use super::ids::VertexRef;
use super::region::RegionRef;

/// Low 16 bits of on-wire edge metadata.
pub const EDGE_META_PAYLOAD_MASK: u16 = u16::MAX;

/// Packed edge-kind nibble values.
pub const EDGE_KIND_PLAIN_LOCAL: u8 = 0;
pub const EDGE_KIND_PLAIN_REMOTE: u8 = 1;
pub const EDGE_KIND_WEIGHTED: u8 = 2;
pub const EDGE_KIND_TEMPORAL_BUCKET: u8 = 3;
pub const EDGE_KIND_VISIBILITY: u8 = 4;
pub const EDGE_KIND_SIDECAR_A: u8 = 5;
pub const EDGE_KIND_SIDECAR_B: u8 = 6;
pub const EDGE_KIND_RESERVED_START: u8 = 7;

pub const EDGE_KIND_MASK: u8 = 0x0F;
pub const EDGE_INLINE_MASK: u8 = 0x0F;
pub const EDGE_KIND_SHIFT: u32 = 24;
pub const EDGE_INLINE_SHIFT: u32 = 28;
pub const EDGE_FLAGS_SHIFT: u32 = 16;
pub const EDGE_AUX_MEANING_MASK: u32 = 0x00FF_0000;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct EdgeFlags: u8 {
        const TOMBSTONE = 1 << 0;
        const HAS_SIDECAR = 1 << 1;
        const IS_REMOTE = 1 << 2;
        const UNDIRECTED = 1 << 3;
        const PAYLOAD_EXTENDED = 1 << 4;
        const RESERVED5 = 1 << 5;
        const RESERVED6 = 1 << 6;
        const RESERVED7 = 1 << 7;
    }
}

pub const EDGE_RESERVED_FLAGS: EdgeFlags =
    EdgeFlags::RESERVED5.union(EdgeFlags::RESERVED6).union(EdgeFlags::RESERVED7);

pub const TOMBSTONE_MASK: u32 = (EdgeFlags::TOMBSTONE.bits() as u32) << EDGE_FLAGS_SHIFT;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeMetaMode {
    LocalInline,
    RemoteInline,
    LocalSidecar,
    RemoteSidecar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EdgeMetaError {
    ReservedFlags(EdgeFlags),
    ReservedKind(u8),
    InvalidRemoteKind(u8),
    InvalidLocalKind(u8),
    InvalidInlineForPlainKind(u8),
    PayloadExtendedWithoutSidecar,
}

/// Packed hot metadata for an [`EdgeEntry`] (32 significant bits, little-endian on wire).
///
/// Layout (LSB = bit 0):
/// - bits 0–15: payload (local [`LabelId`] / shard slot / sidecar handle low16)
/// - bits 16–23: [`EdgeFlags`]
/// - bits 24–27: edge `kind`
/// - bits 28–31: inline payload nibble / sidecar decode hint
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeMeta(u32);

impl EdgeMeta {
    /// Packed edge metadata with no label and no flags.
    pub const UNLABELED: Self = Self(0);

    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }

    pub const fn new_local(label_id: LabelId) -> Self {
        Self::from_components(label_id, EdgeFlags::empty(), EDGE_KIND_PLAIN_LOCAL, 0)
    }

    pub const fn new_remote(shard_slot: u16) -> Self {
        Self::from_components(
            shard_slot,
            EdgeFlags::IS_REMOTE,
            EDGE_KIND_PLAIN_REMOTE,
            0,
        )
    }

    pub fn new(label_id: LabelId, tombstone: bool) -> Self {
        Self::new_local(label_id).with_tombstone(tombstone)
    }

    pub fn new_shard_canister(shard_slot: u16, tombstone: bool) -> Self {
        Self::new_remote(shard_slot).with_tombstone(tombstone)
    }

    pub const fn from_components(payload: u16, flags: EdgeFlags, kind: u8, inline: u8) -> Self {
        Self(
            (payload as u32)
                | ((flags.bits() as u32) << EDGE_FLAGS_SHIFT)
                | (((kind & EDGE_KIND_MASK) as u32) << EDGE_KIND_SHIFT)
                | (((inline & EDGE_INLINE_MASK) as u32) << EDGE_INLINE_SHIFT),
        )
    }

    #[inline]
    pub const fn payload(self) -> u16 {
        (self.0 & EDGE_META_PAYLOAD_MASK as u32) as u16
    }

    #[inline]
    pub fn flags(self) -> EdgeFlags {
        EdgeFlags::from_bits_retain(((self.0 >> EDGE_FLAGS_SHIFT) & 0xFF) as u8)
    }

    #[inline]
    pub const fn kind(self) -> u8 {
        ((self.0 >> EDGE_KIND_SHIFT) & EDGE_KIND_MASK as u32) as u8
    }

    #[inline]
    pub const fn inline(self) -> u8 {
        ((self.0 >> EDGE_INLINE_SHIFT) & EDGE_INLINE_MASK as u32) as u8
    }

    #[inline]
    pub fn local_id(self) -> Option<LabelId> {
        if self.is_shard_canister() || self.has_sidecar() {
            None
        } else {
            Some(self.payload())
        }
    }

    /// Shard slot id when this edge targets another canister.
    #[inline]
    pub fn shard_canister_slot(self) -> Option<u16> {
        if self.is_shard_canister() && !self.has_sidecar() {
            Some(self.payload())
        } else {
            None
        }
    }

    #[inline]
    pub fn mode(self) -> EdgeMetaMode {
        match (
            self.flags().contains(EdgeFlags::IS_REMOTE),
            self.flags().contains(EdgeFlags::HAS_SIDECAR),
        ) {
            (false, false) => EdgeMetaMode::LocalInline,
            (true, false) => EdgeMetaMode::RemoteInline,
            (false, true) => EdgeMetaMode::LocalSidecar,
            (true, true) => EdgeMetaMode::RemoteSidecar,
        }
    }

    #[inline]
    pub fn validate(self) -> Result<(), EdgeMetaError> {
        let flags = self.flags();
        let kind = self.kind();
        if !(flags & EDGE_RESERVED_FLAGS).is_empty() {
            return Err(EdgeMetaError::ReservedFlags(flags & EDGE_RESERVED_FLAGS));
        }
        if kind >= EDGE_KIND_RESERVED_START && !flags.contains(EdgeFlags::HAS_SIDECAR) {
            return Err(EdgeMetaError::ReservedKind(kind));
        }
        if flags.contains(EdgeFlags::IS_REMOTE) && kind == EDGE_KIND_PLAIN_LOCAL {
            return Err(EdgeMetaError::InvalidRemoteKind(kind));
        }
        if !flags.contains(EdgeFlags::IS_REMOTE) && kind == EDGE_KIND_PLAIN_REMOTE {
            return Err(EdgeMetaError::InvalidLocalKind(kind));
        }
        if matches!(kind, EDGE_KIND_PLAIN_LOCAL | EDGE_KIND_PLAIN_REMOTE) && self.inline() != 0 {
            return Err(EdgeMetaError::InvalidInlineForPlainKind(kind));
        }
        if flags.contains(EdgeFlags::PAYLOAD_EXTENDED) && !flags.contains(EdgeFlags::HAS_SIDECAR) {
            return Err(EdgeMetaError::PayloadExtendedWithoutSidecar);
        }
        Ok(())
    }

    #[inline]
    pub fn is_tombstone(self) -> bool {
        self.flags().contains(EdgeFlags::TOMBSTONE)
    }

    #[inline]
    pub fn has_sidecar(self) -> bool {
        self.flags().contains(EdgeFlags::HAS_SIDECAR)
    }

    #[inline]
    pub fn is_shard_canister(self) -> bool {
        self.flags().contains(EdgeFlags::IS_REMOTE)
    }

    #[inline]
    pub fn is_undirected(self) -> bool {
        self.flags().contains(EdgeFlags::UNDIRECTED)
    }

    pub fn with_flags(self, flags: EdgeFlags) -> Self {
        Self::from_components(self.payload(), flags, self.kind(), self.inline())
    }

    pub fn with_tombstone(self, tombstone: bool) -> Self {
        let mut flags = self.flags();
        if tombstone {
            flags.insert(EdgeFlags::TOMBSTONE);
        } else {
            flags.remove(EdgeFlags::TOMBSTONE);
        }
        self.with_flags(flags)
    }

    pub fn with_label_id(self, label_id: LabelId) -> Self {
        let mut flags = self.flags();
        flags.remove(EdgeFlags::IS_REMOTE);
        Self::from_components(label_id, flags, EDGE_KIND_PLAIN_LOCAL, 0)
    }

    pub fn with_shard_canister_slot(self, shard_slot: u16) -> Self {
        let mut flags = self.flags();
        flags.insert(EdgeFlags::IS_REMOTE);
        Self::from_components(shard_slot, flags, EDGE_KIND_PLAIN_REMOTE, 0)
    }

    pub fn with_undirected(self, undirected: bool) -> Self {
        let mut flags = self.flags();
        if undirected {
            flags.insert(EdgeFlags::UNDIRECTED);
        } else {
            flags.remove(EdgeFlags::UNDIRECTED);
        }
        self.with_flags(flags)
    }

    pub fn with_kind_inline(self, kind: u8, inline: u8) -> Self {
        Self::from_components(self.payload(), self.flags(), kind, inline)
    }

    pub fn with_sidecar(self, handle_low16: u16, kind: u8, inline_hint: u8) -> Self {
        let mut flags = self.flags();
        flags.insert(EdgeFlags::HAS_SIDECAR);
        Self::from_components(handle_low16, flags, kind, inline_hint)
    }

    pub fn with_extended_payload(self, extended: bool) -> Self {
        let mut flags = self.flags();
        if extended {
            flags.insert(EdgeFlags::PAYLOAD_EXTENDED);
        } else {
            flags.remove(EdgeFlags::PAYLOAD_EXTENDED);
        }
        self.with_flags(flags)
    }
}

/// Fixed-size hot adjacency entry.
///
/// This is the DGAP-style base entry stored in a surface edge region (CSR slab slot).
/// It intentionally contains only the neighbor vertex ref and edge-local hot
/// metadata.
///
/// Invariant:
/// - one `EdgeEntry` is always exactly 8 bytes (4-byte BE `VertexRef` + 4-byte LE `EdgeMeta`)
/// - semantic edge identity is stored elsewhere, not here
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EdgeEntry {
    pub target: VertexRef,
    pub meta: EdgeMeta,
}

impl Default for EdgeEntry {
    fn default() -> Self {
        Self {
            target: VertexRef::default(),
            meta: EdgeMeta::UNLABELED,
        }
    }
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
const _: [(); 4] = [(); core::mem::size_of::<EdgeMeta>()];

#[cfg(test)]
mod tests {
    use super::{
        EdgeEntry, EdgeFlags, EdgeMeta, EdgeMetaMode, LogicalEdgeLocator, SurfaceKind,
        SurfaceRegions,
    };
    use crate::low_level::VertexRef;
    use crate::low_level::{RegionKind, RegionRef, RegionStorageKind};

    #[test]
    fn edge_entry_has_expected_abi() {
        assert_eq!(core::mem::size_of::<EdgeEntry>(), 8);
        assert_eq!(core::mem::size_of::<VertexRef>(), 4);
        assert_eq!(core::mem::size_of::<EdgeMeta>(), 4);
    }

    #[test]
    fn edge_meta_packs_label_and_tombstone() {
        let meta = EdgeMeta::new(42, true);
        assert_eq!(meta.local_id(), Some(42));
        assert!(meta.is_tombstone());
        assert!(!meta.is_shard_canister());
        assert_eq!(meta.kind(), super::EDGE_KIND_PLAIN_LOCAL);
    }

    #[test]
    fn edge_meta_shard_canister_roundtrip() {
        let meta = EdgeMeta::new_shard_canister(99, false);
        assert_eq!(meta.shard_canister_slot(), Some(99));
        assert_eq!(meta.local_id(), None);
        assert!(!meta.is_tombstone());
        assert!(meta.is_shard_canister());
        assert_eq!(meta.kind(), super::EDGE_KIND_PLAIN_REMOTE);
    }

    #[test]
    fn edge_entry_uses_packed_vertex_ref() {
        let target = VertexRef::from(7u8);
        let entry = EdgeEntry::new(target, EdgeMeta::new(3, false));
        assert_eq!(u64::from(entry.target), 7);
        assert_eq!(entry.meta.local_id(), Some(3));
        assert!(!entry.meta.is_tombstone());
    }

    #[test]
    fn edge_meta_sidecar_roundtrip() {
        let meta = EdgeMeta::new_local(7)
            .with_sidecar(42, super::EDGE_KIND_SIDECAR_A, 3)
            .with_extended_payload(true);
        assert!(meta.has_sidecar());
        assert_eq!(meta.payload(), 42);
        assert_eq!(meta.kind(), super::EDGE_KIND_SIDECAR_A);
        assert_eq!(meta.inline(), 3);
        assert_eq!(meta.mode(), EdgeMetaMode::LocalSidecar);
        assert!(meta.flags().contains(EdgeFlags::PAYLOAD_EXTENDED));
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
