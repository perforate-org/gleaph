//! Read-side adjacency-surface views and directional layout descriptors.

use super::edge::{SurfaceKind, SurfaceRegions};
use super::overflow::OverflowChain;
use super::region::{RegionKind, RegionRef};
use super::vertex::{EdgeIndex, VertexEntry, VertexLabelRange};
use gleaph_graph_kernel::LabelId;

/// Read-side view of one vertex's contiguous base neighborhood.
///
/// This is the base interval only. DGAP overflow entries live outside this
/// interval and are merged later through `log_offset`.
///
/// Invariant:
/// - this always describes exactly one contiguous base interval
/// - `degree == 0` means the interval is empty
/// - overflow/log entries are intentionally excluded
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BaseNeighborhood {
    pub surface: SurfaceKind,
    pub start: EdgeIndex,
    pub degree: u32,
}

/// Read-side view of one vertex's exact-label contiguous base subrange.
///
/// This is derived from the surface-local label sidecar and always points into
/// the canonical base adjacency interval for one vertex.
///
/// Invariant:
/// - this always describes a contiguous subrange of one vertex's base interval
/// - `label_id` identifies the exact label represented by this subrange
/// - overflow/log entries are intentionally excluded
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LabelNeighborhood {
    pub surface: SurfaceKind,
    pub label_id: LabelId,
    pub start: EdgeIndex,
    pub degree: u32,
}

/// Read-side neighborhood view that combines the contiguous base interval with
/// any DGAP overflow chain for the same vertex-local neighborhood.
///
/// Invariant:
/// - `base` always describes the canonical contiguous interval
/// - `overflow` is additive and may be empty
/// - the presence of overflow does not change the meaning of `base.start` or
///   `base.degree`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MergedNeighborhoodView {
    pub base: BaseNeighborhood,
    pub overflow: OverflowChain,
}

impl MergedNeighborhoodView {
    /// Creates one merged read-side view from a base interval and overflow chain.
    pub const fn new(base: BaseNeighborhood, overflow: OverflowChain) -> Self {
        Self { base, overflow }
    }

    /// Returns whether this merged view includes any overflow entries.
    pub const fn has_overflow(self) -> bool {
        !self.overflow.is_empty()
    }
}

impl BaseNeighborhood {
    /// Creates one contiguous base-neighborhood view.
    pub const fn new(surface: SurfaceKind, start: EdgeIndex, degree: u32) -> Self {
        Self {
            surface,
            start,
            degree,
        }
    }

    /// Returns whether this base interval is empty.
    pub const fn is_empty(self) -> bool {
        self.degree == 0
    }

    /// Returns the exclusive end of the base interval.
    pub const fn end_exclusive(self) -> EdgeIndex {
        EdgeIndex::new(self.start.raw + self.degree as u64)
    }

    /// Returns whether the given edge index falls inside the base interval.
    pub const fn contains(self, index: EdgeIndex) -> bool {
        index.raw >= self.start.raw && index.raw < self.end_exclusive().raw
    }
}

impl LabelNeighborhood {
    /// Creates one exact-label subrange view inside a base interval.
    pub const fn new(
        surface: SurfaceKind,
        label_id: LabelId,
        start: EdgeIndex,
        degree: u32,
    ) -> Self {
        Self {
            surface,
            label_id,
            start,
            degree,
        }
    }

    /// Returns whether this label subrange is empty.
    pub const fn is_empty(self) -> bool {
        self.degree == 0
    }

    /// Returns the exclusive end of the label subrange.
    pub const fn end_exclusive(self) -> EdgeIndex {
        EdgeIndex::new(self.start.raw + self.degree as u64)
    }

    /// Returns whether the given edge index falls inside the label subrange.
    pub const fn contains(self, index: EdgeIndex) -> bool {
        index.raw >= self.start.raw && index.raw < self.end_exclusive().raw
    }
}

/// One directional adjacency surface assembled from concrete regions.
///
/// Invariant:
/// - all regions belong to the same directional surface
/// - region-manager placement may change, but region kinds for the surface do
///   not
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceLayout {
    pub kind: SurfaceKind,
    pub regions: SurfaceRegions,
}

impl SurfaceLayout {
    /// Creates one directional surface layout from its region bundle.
    pub const fn new(kind: SurfaceKind, regions: SurfaceRegions) -> Self {
        Self { kind, regions }
    }

    /// Verifies that the bundled regions match the declared surface kind.
    pub fn validate(self) -> bool {
        let expected = self.expected_region_kinds();
        self.regions.vertex_table.region_kind() == expected.vertex_table
            && self.regions.edge_entries.region_kind() == expected.edge_entries
            && self.regions.label_index.region_kind() == expected.label_index
            && self.regions.segment_log.region_kind() == expected.segment_log
    }

    /// Returns the region kinds expected for this surface direction.
    pub const fn expected_region_kinds(self) -> SurfaceRegionKinds {
        match self.kind {
            SurfaceKind::Forward => SurfaceRegionKinds {
                vertex_table: RegionKind::ForwardVertexTable,
                edge_entries: RegionKind::ForwardEdgeEntries,
                label_index: RegionKind::ForwardLabelIndex,
                segment_log: RegionKind::ForwardSegmentLog,
            },
            SurfaceKind::Reverse => SurfaceRegionKinds {
                vertex_table: RegionKind::ReverseVertexTable,
                edge_entries: RegionKind::ReverseEdgeEntries,
                label_index: RegionKind::ReverseLabelIndex,
                segment_log: RegionKind::ReverseSegmentLog,
            },
        }
    }

    /// Returns the region that stores hot edge entries for this surface.
    pub const fn edge_entries_region(self) -> RegionRef {
        self.regions.edge_entries
    }

    /// Returns the region that stores the vertex table for this surface.
    pub const fn vertex_table_region(self) -> RegionRef {
        self.regions.vertex_table
    }

    /// Returns the region that stores the surface-local label sidecar.
    pub const fn label_index_region(self) -> RegionRef {
        self.regions.label_index
    }

    /// Returns the region that stores DGAP-style overflow entries.
    pub const fn segment_log_region(self) -> RegionRef {
        self.regions.segment_log
    }

    /// Builds the canonical base-neighborhood view for one vertex entry.
    pub const fn base_neighborhood(self, vertex: VertexEntry) -> BaseNeighborhood {
        BaseNeighborhood::new(self.kind, vertex.edge_index, vertex.degree)
    }

    /// Builds the merged read-side view for one vertex entry and overflow chain.
    pub const fn merged_neighborhood(
        self,
        vertex: VertexEntry,
        overflow: OverflowChain,
    ) -> MergedNeighborhoodView {
        MergedNeighborhoodView::new(self.base_neighborhood(vertex), overflow)
    }

    /// Converts one label-range record into a typed exact-label view.
    pub const fn label_neighborhood(self, range: VertexLabelRange) -> LabelNeighborhood {
        LabelNeighborhood::new(
            self.kind,
            range.label_id,
            EdgeIndex::new(range.start as u64),
            range.len,
        )
    }
}

/// Expected region kinds for one directional surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceRegionKinds {
    pub vertex_table: RegionKind,
    pub edge_entries: RegionKind,
    pub label_index: RegionKind,
    pub segment_log: RegionKind,
}

/// Strongly-typed wrapper for a forward surface layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ForwardSurface(pub SurfaceLayout);

impl ForwardSurface {
    /// Creates a typed forward surface wrapper.
    pub const fn new(regions: SurfaceRegions) -> Self {
        Self(SurfaceLayout::new(SurfaceKind::Forward, regions))
    }

    /// Returns the underlying generic surface layout.
    pub const fn layout(self) -> SurfaceLayout {
        self.0
    }
}

/// Strongly-typed wrapper for a reverse surface layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReverseSurface(pub SurfaceLayout);

impl ReverseSurface {
    /// Creates a typed reverse surface wrapper.
    pub const fn new(regions: SurfaceRegions) -> Self {
        Self(SurfaceLayout::new(SurfaceKind::Reverse, regions))
    }

    /// Returns the underlying generic surface layout.
    pub const fn layout(self) -> SurfaceLayout {
        self.0
    }
}

const _: [(); 16] = [(); core::mem::size_of::<BaseNeighborhood>()];
const _: [(); 16] = [(); core::mem::size_of::<LabelNeighborhood>()];
const _: [(); 32] = [(); core::mem::size_of::<MergedNeighborhoodView>()];

#[cfg(test)]
mod tests {
    use super::{ForwardSurface, ReverseSurface, SurfaceKind, SurfaceLayout};
    use crate::low_level::{
        EMPTY_LOG_OFFSET, EdgeIndex, LogOffset, OverflowChain, RegionKind, RegionRef,
        RegionStorageKind, SurfaceRegions, VertexEntry, VertexLabelRange, VertexRef,
    };

    fn forward_regions() -> SurfaceRegions {
        SurfaceRegions::new(
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
        )
    }

    #[test]
    fn forward_surface_exposes_expected_region_kinds() {
        let surface = ForwardSurface::new(forward_regions());
        let expected = surface.layout().expected_region_kinds();

        assert_eq!(surface.layout().kind, SurfaceKind::Forward);
        assert_eq!(expected.vertex_table, RegionKind::ForwardVertexTable);
        assert_eq!(expected.edge_entries, RegionKind::ForwardEdgeEntries);
        assert!(surface.layout().validate());
    }

    #[test]
    fn reverse_surface_validation_rejects_forward_region_set() {
        let surface = ReverseSurface::new(forward_regions());
        assert!(!surface.layout().validate());
    }

    #[test]
    fn surface_layout_returns_surface_local_regions() {
        let layout = SurfaceLayout::new(SurfaceKind::Forward, forward_regions());
        assert_eq!(
            layout.vertex_table_region().region_kind(),
            RegionKind::ForwardVertexTable
        );
        assert_eq!(
            layout.edge_entries_region().region_kind(),
            RegionKind::ForwardEdgeEntries
        );
        assert_eq!(
            layout.segment_log_region().region_kind(),
            RegionKind::ForwardSegmentLog
        );
    }

    #[test]
    fn base_neighborhood_uses_vertex_entry_interval_in_edge_entry_units() {
        let layout = SurfaceLayout::new(SurfaceKind::Forward, forward_regions());
        let vertex = VertexEntry::new(EdgeIndex::new(12), 5, EMPTY_LOG_OFFSET);
        let neighborhood = layout.base_neighborhood(vertex);

        assert_eq!(neighborhood.surface, SurfaceKind::Forward);
        assert_eq!(neighborhood.start, EdgeIndex::new(12));
        assert_eq!(neighborhood.end_exclusive(), EdgeIndex::new(17));
        assert!(neighborhood.contains(EdgeIndex::new(12)));
        assert!(neighborhood.contains(EdgeIndex::new(16)));
        assert!(!neighborhood.contains(EdgeIndex::new(17)));
    }

    #[test]
    fn empty_base_neighborhood_is_supported() {
        let layout = SurfaceLayout::new(SurfaceKind::Reverse, forward_regions());
        let vertex = VertexEntry::new(EdgeIndex::new(8), 0, EMPTY_LOG_OFFSET);
        let neighborhood = layout.base_neighborhood(vertex);

        assert!(neighborhood.is_empty());
        assert_eq!(neighborhood.start, neighborhood.end_exclusive());
    }

    #[test]
    fn merged_neighborhood_keeps_base_and_overflow_separate() {
        let layout = SurfaceLayout::new(SurfaceKind::Forward, forward_regions());
        let vertex = VertexEntry::new(EdgeIndex::new(12), 5, 3);
        let overflow = OverflowChain::new(
            SurfaceKind::Forward,
            VertexRef::from(1u8),
            LogOffset::new(3),
        );
        let merged = layout.merged_neighborhood(vertex, overflow);

        assert_eq!(merged.base.start, EdgeIndex::new(12));
        assert_eq!(merged.base.degree, 5);
        assert!(merged.has_overflow());
        assert_eq!(merged.overflow.head, LogOffset::new(3));
    }

    #[test]
    fn label_neighborhood_builds_exact_label_subrange_view() {
        let layout = SurfaceLayout::new(SurfaceKind::Forward, forward_regions());
        let label = layout.label_neighborhood(VertexLabelRange {
            label_id: 7,
            start: 12,
            len: 3,
        });

        assert_eq!(label.surface, SurfaceKind::Forward);
        assert_eq!(label.label_id, 7);
        assert_eq!(label.start, EdgeIndex::new(12));
        assert_eq!(label.end_exclusive(), EdgeIndex::new(15));
        assert!(label.contains(EdgeIndex::new(12)));
        assert!(label.contains(EdgeIndex::new(14)));
        assert!(!label.contains(EdgeIndex::new(15)));
    }
}
