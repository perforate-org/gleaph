//! In-memory surface runtimes built from low-level adjacency regions.

use std::collections::BTreeSet;

use gleaph_graph_kernel::{EdgeId, LabelId, NodeId};

use super::edge::{EdgeEntry, EdgeLocator};
use super::locator::EdgeLocatorSidecar;
use super::overflow::{LogOffset, OverflowChain, OverflowEntry};
use super::surface::{
    BaseNeighborhood, ForwardSurface, LabelNeighborhood, MergedNeighborhoodView, ReverseSurface,
    SurfaceLayout,
};
use super::vertex::{
    EdgeIndex, VertexEntry, VertexLabelIndexEntry, VertexLabelRange, EMPTY_LOG_OFFSET,
};

/// Minimal read-side runtime for one directional surface.
///
/// This is intentionally not a stable-memory IO layer yet. It only bundles the
/// surface layout, the vertex table, and enough accessors to produce
/// `BaseNeighborhood` and `MergedNeighborhoodView`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceRuntime {
    pub layout: SurfaceLayout,
    pub vertices: Vec<VertexEntry>,
    pub base_entries: Vec<EdgeEntry>,
    pub overflow_entries: Vec<OverflowEntry>,
    pub label_index_entries: Vec<VertexLabelIndexEntry>,
    pub label_ranges: Vec<VertexLabelRange>,
    pub dirty_regions: SurfaceDirtyRegions,
    pub dirty_vertices: BTreeSet<usize>,
}

/// Region-level dirty tracking for one surface runtime.
///
/// This is the bridge between in-memory mutation and stable-memory writeback.
/// Vertex-local dirtiness drives label-sidecar maintenance, while these flags
/// say which concrete regions need serialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceDirtyRegions {
    pub vertex_table: bool,
    pub edge_entries: bool,
    pub label_index: bool,
    pub segment_log: bool,
}

/// Resolved physical slot kind for one locator inside a surface runtime.
///
/// This lets higher layers decide whether a semantic edge currently lives in
/// the canonical base interval or in the DGAP overflow chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedEdgeSlot {
    /// One entry inside the contiguous base neighborhood.
    Base { logical_index: usize },
    /// One entry inside the overflow log chain.
    Overflow {
        overflow_index: usize,
        offset: LogOffset,
    },
}

/// Chosen insertion path for one logical edge inside a directional surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeInsertPath {
    /// Append into the canonical base interval.
    BaseAppend { logical_index: usize },
    /// Reuse a tombstoned tail slot inside the canonical base interval.
    BaseReuseTombstone { logical_index: usize },
    /// Append into the DGAP overflow chain.
    Overflow,
}

/// Chosen base-insert path for one directional surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseInsertDecision {
    /// Append one entry at the end of the canonical base interval.
    Append { logical_index: usize },
    /// Reuse the final tombstoned slot of the canonical base interval.
    ReuseTombstone { logical_index: usize },
}

/// Pure surface-local rebalance result for one vertex window.
///
/// This does not mutate the runtime yet. It says what the canonical base slice
/// would look like after compacting live base entries together with live
/// overflow entries for the selected window.
///
/// Invariant:
/// - `rewritten_vertices.len() == end_ordinal_exclusive - start_ordinal`
/// - each rewritten vertex has `log_offset == EMPTY_LOG_OFFSET`
/// - `compacted_base_entries` is the concatenation of all rewritten vertex
///   neighborhoods in ordinal order
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceLocalRebalanceDelta {
    pub start_ordinal: usize,
    pub end_ordinal_exclusive: usize,
    pub base_start: EdgeIndex,
    pub compacted_base_entries: Vec<EdgeEntry>,
    pub rewritten_vertices: Vec<VertexEntry>,
    pub reserved_base_len: u32,
}

impl SurfaceLocalRebalanceDelta {
    /// Returns the exclusive end of the rewritten base-capacity span.
    pub fn end_exclusive(&self) -> Option<EdgeIndex> {
        self.base_start.checked_add(self.reserved_base_len)
    }

    /// Returns the rewritten base-capacity span length.
    pub const fn capacity_span_len(&self) -> u32 {
        self.reserved_base_len
    }

    /// Returns how many `EdgeEntry` slots this delta would shift following
    /// base neighborhoods by, relative to `current_span_len`.
    pub fn displacement_against_current_span(&self, current_span_len: u32) -> i64 {
        i64::from(self.reserved_base_len) - i64::from(current_span_len)
    }
}

/// Summary produced after applying one surface-local rebalance delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceAppliedRebalanceSummary {
    pub start_ordinal: usize,
    pub end_ordinal_exclusive: usize,
    pub old_span_len: u32,
    pub new_span_len: u32,
    pub displacement: i64,
}

/// Pure weighted placement metadata for one rebalance window.
///
/// This is the runtime-side analogue of VCSR's position calculation: it says
/// where each rewritten neighborhood starts and how many base-capacity slots it
/// receives after redistributing reserved slack within the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceWeightedWindowLayout {
    pub anchor_ordinal: usize,
    pub start_ordinal: usize,
    pub end_ordinal_exclusive: usize,
    pub base_start: EdgeIndex,
    pub live_degrees: Vec<u32>,
    pub reserved_lengths: Vec<u32>,
    pub positions: Vec<EdgeIndex>,
    pub reserved_base_len: u32,
}

impl SurfaceWeightedWindowLayout {
    /// Returns the expected exclusive end of the rewritten base-capacity span.
    pub fn end_exclusive(&self) -> Option<EdgeIndex> {
        self.base_start.checked_add(self.reserved_base_len)
    }

    /// Returns the expected total base-capacity span of the rewritten window.
    pub const fn capacity_span_len(&self) -> u32 {
        self.reserved_base_len
    }

    /// Returns how many `EdgeEntry` slots the rewritten window would shift
    /// following base neighborhoods by, relative to `current_span_len`.
    pub fn displacement_against_current_span(&self, current_span_len: u32) -> i64 {
        i64::from(self.reserved_base_len) - i64::from(current_span_len)
    }
}

/// Pure summary of reclaimable slack inside one surface-local rebalance window.
///
/// This is used by graph-level insert policy to decide whether it is worth
/// triggering local rebalance before falling back to the overflow log.
///
/// Invariant:
/// - `start_ordinal < end_ordinal_exclusive`
/// - `reclaimable_tombstones <= total_base_slots`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceWindowSlackSummary {
    /// First vertex ordinal included in the summarized window.
    pub start_ordinal: usize,
    /// One-past-the-end vertex ordinal of the summarized window.
    pub end_ordinal_exclusive: usize,
    /// Total number of base-capacity slots currently covered by the window.
    pub total_base_slots: usize,
    /// Number of tombstoned base slots that local compaction could reclaim.
    pub reclaimable_tombstones: usize,
    /// Number of live overflow entries currently attached to the window.
    pub overflow_entries_in_window: usize,
}

impl SurfaceWindowSlackSummary {
    /// Returns whether the window contains at least one reclaimable tombstoned slot.
    pub const fn has_reclaimable_slack(self) -> bool {
        self.reclaimable_tombstones > 0
    }

    /// Returns whether local compaction can absorb `additional_live_entries`
    /// into base without requiring extra base capacity beyond the current window.
    pub const fn can_absorb_additional_live_entries(self, additional_live_entries: usize) -> bool {
        self.reclaimable_tombstones
            >= self
                .overflow_entries_in_window
                .saturating_add(additional_live_entries)
    }
}

impl SurfaceDirtyRegions {
    /// Returns whether no region in this surface is currently dirty.
    pub const fn is_clean(self) -> bool {
        !self.vertex_table && !self.edge_entries && !self.label_index && !self.segment_log
    }

    /// Returns whether at least one region in this surface is currently dirty.
    pub const fn is_dirty(self) -> bool {
        !self.is_clean()
    }
}

impl SurfaceRuntime {
    fn calculate_weighted_positions(
        base_start: EdgeIndex,
        live_degrees: &[u32],
        reserved_base_len: u32,
    ) -> Option<(Vec<EdgeIndex>, Vec<u32>)> {
        if live_degrees.is_empty() {
            return None;
        }
        let total_live_entries: u32 = live_degrees
            .iter()
            .copied()
            .try_fold(0u32, |acc, degree| acc.checked_add(degree))?;
        if reserved_base_len < total_live_entries {
            return None;
        }

        let gaps = reserved_base_len.checked_sub(total_live_entries)?;
        let total_weight: u64 = live_degrees
            .iter()
            .copied()
            .map(|degree| u64::from(degree) + 1)
            .sum();
        let step = if total_weight == 0 {
            0.0
        } else {
            f64::from(gaps) / total_weight as f64
        };

        let mut positions = Vec::with_capacity(live_degrees.len());
        let mut index_d = base_start.raw as f64;
        let mut previous_end = base_start.raw;
        for (offset, &degree) in live_degrees.iter().enumerate() {
            let raw_pos = if offset == 0 {
                base_start.raw
            } else {
                (index_d as u64).max(previous_end)
            };
            positions.push(EdgeIndex::new(raw_pos));
            previous_end = raw_pos + u64::from(degree);
            index_d += f64::from(degree) + (step * f64::from(degree + 1));
        }

        let mut reserved_lengths = Vec::with_capacity(live_degrees.len());
        for offset in 0..live_degrees.len() {
            let start = positions[offset].raw;
            let next_start = positions
                .get(offset + 1)
                .map(|pos| pos.raw)
                .unwrap_or(base_start.raw + u64::from(reserved_base_len));
            let reserved_len = u32::try_from(next_start.checked_sub(start)?).ok()?;
            if reserved_len < live_degrees[offset] {
                return None;
            }
            reserved_lengths.push(reserved_len);
        }

        Some((positions, reserved_lengths))
    }

    /// Computes weighted base positions for one rebalance window before any
    /// concrete entries are copied.
    pub fn build_weighted_window_layout(
        &self,
        anchor_ordinal: usize,
        start_ordinal: usize,
        end_ordinal_exclusive: usize,
        reserved_base_len: u32,
    ) -> Option<SurfaceWeightedWindowLayout> {
        if start_ordinal >= end_ordinal_exclusive
            || end_ordinal_exclusive > self.vertices.len()
            || anchor_ordinal < start_ordinal
            || anchor_ordinal >= end_ordinal_exclusive
        {
            return None;
        }

        let base_start = self.vertex_entry(start_ordinal)?.edge_index;
        let live_degrees: Vec<u32> = (start_ordinal..end_ordinal_exclusive)
            .map(|ordinal| {
                self.merged_live_entries_for_ordinal(ordinal)
                    .and_then(|entries| u32::try_from(entries.len()).ok())
            })
            .collect::<Option<Vec<_>>>()?;
        let total_live_entries: u32 = live_degrees
            .iter()
            .copied()
            .try_fold(0u32, |acc, degree| acc.checked_add(degree))?;
        if reserved_base_len < total_live_entries {
            return None;
        }

        let _ = anchor_ordinal.checked_sub(start_ordinal)?;
        let (positions, reserved_lengths) =
            Self::calculate_weighted_positions(base_start, &live_degrees, reserved_base_len)?;

        Some(SurfaceWeightedWindowLayout {
            anchor_ordinal,
            start_ordinal,
            end_ordinal_exclusive,
            base_start,
            live_degrees,
            reserved_lengths,
            positions,
            reserved_base_len,
        })
    }

    /// Creates one in-memory surface runtime from its layout and decoded region payloads.
    pub fn new(
        layout: SurfaceLayout,
        vertices: Vec<VertexEntry>,
        base_entries: Vec<EdgeEntry>,
        overflow_entries: Vec<OverflowEntry>,
        label_index_entries: Vec<VertexLabelIndexEntry>,
        label_ranges: Vec<VertexLabelRange>,
    ) -> Self {
        Self {
            layout,
            vertices,
            base_entries,
            overflow_entries,
            label_index_entries,
            label_ranges,
            dirty_regions: SurfaceDirtyRegions::default(),
            dirty_vertices: BTreeSet::new(),
        }
    }

    /// Creates a surface runtime with no base entries, overflow entries, or label sidecar.
    pub fn without_overflow(layout: SurfaceLayout, vertices: Vec<VertexEntry>) -> Self {
        Self::new(
            layout,
            vertices,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Returns the vertex-table entry for one vertex ordinal.
    pub fn vertex_entry(&self, ordinal: usize) -> Option<VertexEntry> {
        self.vertices.get(ordinal).copied()
    }

    /// Appends one empty vertex slot at the end of the surface-local vertex table.
    ///
    /// The new vertex starts with an empty base interval and no overflow chain.
    /// Its base start is the current end of the base-entry region.
    pub fn append_empty_vertex(&mut self) -> Option<usize> {
        let ordinal = self.vertices.len();
        let edge_index = EdgeIndex::new(u64::try_from(self.base_entries.len()).ok()?);
        self.vertices
            .push(VertexEntry::new(edge_index, 0, EMPTY_LOG_OFFSET));
        self.label_index_entries.push(VertexLabelIndexEntry::new(
            u32::try_from(self.label_ranges.len()).ok()?,
            0,
        ));
        self.dirty_regions.vertex_table = true;
        self.dirty_regions.label_index = true;
        Some(ordinal)
    }

    /// Iterates over currently dirty vertex ordinals.
    pub fn dirty_vertices(&self) -> impl Iterator<Item = usize> + '_ {
        self.dirty_vertices.iter().copied()
    }

    /// Returns the current region-level dirty flags for this surface.
    pub const fn dirty_regions(&self) -> SurfaceDirtyRegions {
        self.dirty_regions
    }

    /// Returns whether any region in this surface still needs writeback.
    pub const fn has_dirty_regions(&self) -> bool {
        self.dirty_regions.is_dirty()
    }

    /// Marks one vertex ordinal as needing label-sidecar refresh.
    pub fn mark_vertex_dirty(&mut self, ordinal: usize) -> Option<()> {
        if ordinal >= self.vertices.len() {
            return None;
        }
        self.dirty_vertices.insert(ordinal);
        Some(())
    }

    /// Clears all vertex-local dirty markers.
    pub fn clear_dirty_vertices(&mut self) {
        self.dirty_vertices.clear();
    }

    /// Clears all region-level dirty flags.
    pub fn clear_dirty_regions(&mut self) {
        self.dirty_regions = SurfaceDirtyRegions::default();
    }

    /// Resolves one overflow entry by its log offset.
    pub fn overflow_entry(&self, offset: LogOffset) -> Option<OverflowEntry> {
        if offset.is_empty() || offset.raw < 0 {
            return None;
        }
        self.overflow_entries.get(offset.raw as usize).copied()
    }

    /// Builds the canonical base-neighborhood view for one vertex ordinal.
    pub fn base_neighborhood(&self, ordinal: usize) -> Option<BaseNeighborhood> {
        let vertex = self.vertex_entry(ordinal)?;
        Some(self.layout.base_neighborhood(vertex))
    }

    /// Materializes base entries for one vertex ordinal.
    pub fn base_entries_for(&self, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        let base = self.base_neighborhood(ordinal)?;
        let start = usize::try_from(base.start.raw).ok()?;
        let end = usize::try_from(base.end_exclusive().raw).ok()?;
        Some(self.base_entries.get(start..end)?.to_vec())
    }

    /// Returns the overflow-chain descriptor for one vertex-local neighborhood.
    pub fn overflow_chain(&self, vertex: NodeId, ordinal: usize) -> Option<OverflowChain> {
        let entry = self.vertex_entry(ordinal)?;
        let head = if entry.log_offset == EMPTY_LOG_OFFSET {
            LogOffset::EMPTY
        } else {
            LogOffset::new(entry.log_offset)
        };
        Some(OverflowChain::new(self.layout.kind, vertex, head))
    }

    /// Follows and materializes the overflow chain for one vertex-local neighborhood.
    pub fn overflow_entries_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
    ) -> Option<Vec<OverflowEntry>> {
        let _ = vertex;
        self.overflow_entries_for_ordinal(ordinal)
    }

    /// Follows and materializes the overflow chain for one vertex ordinal.
    ///
    /// Unlike `overflow_entries_for`, this helper does not require the caller
    /// to provide the semantic vertex id because chain traversal depends only
    /// on the stored `log_offset`.
    pub fn overflow_entries_for_ordinal(&self, ordinal: usize) -> Option<Vec<OverflowEntry>> {
        let entry = self.vertex_entry(ordinal)?;
        let mut next = if entry.log_offset == EMPTY_LOG_OFFSET {
            LogOffset::EMPTY
        } else {
            LogOffset::new(entry.log_offset)
        };
        if next.is_empty() {
            return Some(Vec::new());
        }

        let mut entries = Vec::new();
        while let Some(entry) = self.overflow_entry(next) {
            entries.push(entry);
            next = entry.next;
            if next.is_empty() {
                break;
            }
        }
        Some(entries)
    }

    /// Materializes merged live entries for one vertex ordinal.
    ///
    /// Tombstoned base or overflow entries are filtered out because local
    /// rebalance compacts only currently live adjacency entries back into the
    /// canonical base interval.
    pub fn merged_live_entries_for_ordinal(&self, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        let mut entries = self.base_entries_for(ordinal)?;
        entries.retain(|entry| !entry.meta.is_tombstone());
        let overflow = self.overflow_entries_for_ordinal(ordinal)?;
        entries.extend(
            overflow
                .into_iter()
                .filter(|entry| !entry.entry.meta.is_tombstone())
                .map(|entry| entry.entry),
        );
        Some(entries)
    }

    /// Builds the merged read-side view for one vertex-local neighborhood.
    pub fn merged_neighborhood(
        &self,
        vertex: NodeId,
        ordinal: usize,
    ) -> Option<MergedNeighborhoodView> {
        let entry = self.vertex_entry(ordinal)?;
        let overflow = self.overflow_chain(vertex, ordinal)?;
        Some(self.layout.merged_neighborhood(entry, overflow))
    }

    /// Materializes merged read order as base entries followed by overflow entries.
    pub fn merged_entries_for(&self, vertex: NodeId, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        let mut entries = self.base_entries_for(ordinal)?;
        let overflow = self.overflow_entries_for(vertex, ordinal)?;
        entries.extend(overflow.into_iter().map(|entry| entry.entry));
        Some(entries)
    }

    /// Returns the label-index pointer entry for one vertex ordinal.
    pub fn label_index_entry(&self, ordinal: usize) -> Option<VertexLabelIndexEntry> {
        self.label_index_entries.get(ordinal).copied()
    }

    /// Materializes all label ranges referenced by one vertex ordinal.
    pub fn label_ranges_for(&self, ordinal: usize) -> Option<Vec<VertexLabelRange>> {
        let index = self.label_index_entry(ordinal)?;
        let start = usize::try_from(index.start).ok()?;
        let end = start.checked_add(usize::try_from(index.len).ok()?)?;
        Some(self.label_ranges.get(start..end)?.to_vec())
    }

    /// Finds the exact-label base subrange for one vertex ordinal.
    pub fn label_range_for(&self, ordinal: usize, label_id: LabelId) -> Option<VertexLabelRange> {
        self.label_ranges_for(ordinal)?
            .into_iter()
            .find(|range| range.label_id == label_id)
    }

    /// Reconstructs one vertex-local label-range list from the canonical base interval.
    pub fn materialize_label_ranges_for(&self, ordinal: usize) -> Option<Vec<VertexLabelRange>> {
        let base = self.base_neighborhood(ordinal)?;
        let start = usize::try_from(base.start.raw).ok()?;
        let end = usize::try_from(base.end_exclusive().raw).ok()?;
        let slice = self.base_entries.get(start..end)?;

        let mut ranges = Vec::new();
        let mut current_label = None;
        let mut current_start = 0u32;
        let mut current_len = 0u32;

        for (local_index, entry) in slice.iter().copied().enumerate() {
            if entry.meta.is_tombstone() {
                if let Some(label_id) = current_label.take() {
                    ranges.push(VertexLabelRange::new(label_id, current_start, current_len));
                }
                current_len = 0;
                continue;
            }

            let label_id = entry.meta.label_id();
            let global_start = u32::try_from(base.start.raw + local_index as u64).ok()?;

            match current_label {
                Some(active) if active == label_id => {
                    current_len += 1;
                }
                Some(active) => {
                    ranges.push(VertexLabelRange::new(active, current_start, current_len));
                    current_label = Some(label_id);
                    current_start = global_start;
                    current_len = 1;
                }
                None => {
                    current_label = Some(label_id);
                    current_start = global_start;
                    current_len = 1;
                }
            }
        }

        if let Some(label_id) = current_label {
            ranges.push(VertexLabelRange::new(label_id, current_start, current_len));
        }

        Some(ranges)
    }

    /// Reconstructs the full surface-local label sidecar from all base intervals.
    pub fn materialize_label_sidecar(
        &self,
    ) -> Option<(Vec<VertexLabelIndexEntry>, Vec<VertexLabelRange>)> {
        let mut index_entries = Vec::with_capacity(self.vertices.len());
        let mut ranges = Vec::new();

        for ordinal in 0..self.vertices.len() {
            let range_start = u32::try_from(ranges.len()).ok()?;
            ranges.extend(self.materialize_label_ranges_for(ordinal)?);
            let range_len = u32::try_from(ranges.len()).ok()? - range_start;
            index_entries.push(VertexLabelIndexEntry::new(range_start, range_len));
        }

        Some((index_entries, ranges))
    }

    /// Rebuilds the full surface-local label sidecar in place.
    pub fn rebuild_label_sidecar(&mut self) -> Option<()> {
        let (index_entries, ranges) = self.materialize_label_sidecar()?;
        self.label_index_entries = index_entries;
        self.label_ranges = ranges;
        self.dirty_regions.label_index = true;
        Some(())
    }

    /// Rebuilds the label-sidecar contribution for a single vertex ordinal.
    pub fn rebuild_label_sidecar_for_vertex(&mut self, ordinal: usize) -> Option<()> {
        if ordinal >= self.vertices.len() {
            return None;
        }

        if self.label_index_entries.len() != self.vertices.len() {
            return self.rebuild_label_sidecar();
        }

        let replacement = self.materialize_label_ranges_for(ordinal)?;
        let current = self.label_index_entry(ordinal)?;
        let old_start = usize::try_from(current.start).ok()?;
        let old_len = usize::try_from(current.len).ok()?;
        let old_end = old_start.checked_add(old_len)?;
        let new_len_u32 = u32::try_from(replacement.len()).ok()?;

        self.label_ranges.splice(old_start..old_end, replacement);
        self.label_index_entries[ordinal] = VertexLabelIndexEntry::new(current.start, new_len_u32);

        let delta = i64::from(new_len_u32) - i64::from(current.len);
        if delta != 0 {
            for entry in self.label_index_entries.iter_mut().skip(ordinal + 1) {
                let shifted = i64::from(entry.start) + delta;
                if shifted < 0 || shifted > i64::from(u32::MAX) {
                    return None;
                }
                entry.start = shifted as u32;
            }
        }

        self.dirty_regions.label_index = true;
        Some(())
    }

    /// Refreshes the label sidecar only for vertices currently marked dirty.
    pub fn refresh_label_sidecar_for_dirty_vertices(&mut self) -> Option<Vec<usize>> {
        let dirty: Vec<_> = self.dirty_vertices().collect();
        for ordinal in dirty.iter().copied() {
            self.rebuild_label_sidecar_for_vertex(ordinal)?;
        }
        self.clear_dirty_vertices();
        Some(dirty)
    }

    /// Replaces one base entry inside a vertex's canonical contiguous interval.
    pub fn replace_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
        new_entry: EdgeEntry,
    ) -> Option<EdgeEntry> {
        let base = self.base_neighborhood(ordinal)?;
        let degree = usize::try_from(base.degree).ok()?;
        if logical_index >= degree {
            return None;
        }
        let slot = usize::try_from(base.start.raw + logical_index as u64).ok()?;
        let old = *self.base_entries.get(slot)?;
        self.base_entries[slot] = new_entry;
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(old)
    }

    /// Tombstones one base entry inside a vertex's canonical contiguous interval.
    pub fn tombstone_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeEntry> {
        let base = self.base_neighborhood(ordinal)?;
        let degree = usize::try_from(base.degree).ok()?;
        if logical_index >= degree {
            return None;
        }
        let slot = usize::try_from(base.start.raw + logical_index as u64).ok()?;
        let old = *self.base_entries.get(slot)?;
        self.base_entries[slot] = EdgeEntry::new(old.target, old.meta.with_tombstone(true));
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(old)
    }

    /// Appends one entry to the overflow log and updates the vertex-local log head.
    pub fn append_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
        entry: EdgeEntry,
    ) -> Option<LogOffset> {
        let head = self.overflow_chain(vertex, ordinal)?.head;
        let new_offset = LogOffset::new(i32::try_from(self.overflow_entries.len()).ok()?);
        self.overflow_entries
            .push(OverflowEntry::new(edge_id, entry, head));
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        vertex_entry.log_offset = new_offset.raw;
        self.dirty_regions.vertex_table = true;
        self.dirty_regions.segment_log = true;
        Some(new_offset)
    }

    /// Returns whether this vertex can append one more entry directly to the
    /// tail of its base interval without shifting later base entries.
    pub fn can_append_base_entry(&self, ordinal: usize) -> Option<bool> {
        let base = self.base_neighborhood(ordinal)?;
        let end = usize::try_from(base.end_exclusive().raw).ok()?;
        Some(end == self.base_entries.len())
    }

    fn base_capacity_end_exclusive(&self, ordinal: usize) -> Option<usize> {
        if ordinal >= self.vertices.len() {
            return None;
        }
        if ordinal + 1 < self.vertices.len() {
            return usize::try_from(self.vertices.get(ordinal + 1)?.edge_index.raw).ok();
        }
        Some(self.base_entries.len())
    }

    /// Returns whether the last slot in the canonical base interval is a
    /// tombstone that can be reused without shifting later base entries.
    pub fn reusable_tombstoned_tail_base_slot(&self, ordinal: usize) -> Option<usize> {
        let base = self.base_neighborhood(ordinal)?;
        let degree = usize::try_from(base.degree).ok()?;
        if degree == 0 {
            return None;
        }
        let slot = usize::try_from(base.end_exclusive().raw.checked_sub(1)?).ok()?;
        self.base_entries
            .get(slot)?
            .meta
            .is_tombstone()
            .then_some(degree - 1)
    }

    /// Returns whether the first reserved slot immediately after the current
    /// live degree can be reused as base capacity.
    pub fn reusable_reserved_tail_base_slot(&self, ordinal: usize) -> Option<usize> {
        let base = self.base_neighborhood(ordinal)?;
        let degree = usize::try_from(base.degree).ok()?;
        let capacity_end = self.base_capacity_end_exclusive(ordinal)?;
        let slot = usize::try_from(base.start.raw + degree as u64).ok()?;
        if slot >= capacity_end {
            return None;
        }
        self.base_entries
            .get(slot)?
            .meta
            .is_tombstone()
            .then_some(degree)
    }

    /// Chooses whether a base insert can append or reuse the tail tombstone.
    pub fn choose_base_insert_slot(&self, ordinal: usize) -> Option<BaseInsertDecision> {
        if self.can_append_base_entry(ordinal)? {
            let logical_index = usize::try_from(self.vertices.get(ordinal)?.degree).ok()?;
            return Some(BaseInsertDecision::Append { logical_index });
        }
        if let Some(logical_index) = self.reusable_reserved_tail_base_slot(ordinal) {
            return Some(BaseInsertDecision::ReuseTombstone { logical_index });
        }
        self.reusable_tombstoned_tail_base_slot(ordinal)
            .map(|logical_index| BaseInsertDecision::ReuseTombstone { logical_index })
    }

    /// Appends one entry directly to the tail of the canonical base interval.
    ///
    /// This is a conservative append path: it succeeds only when the base
    /// interval already ends at the tail of the current base-entry region.
    pub fn append_base_entry(&mut self, ordinal: usize, entry: EdgeEntry) -> Option<usize> {
        if !self.can_append_base_entry(ordinal)? {
            return None;
        }

        let logical_index = usize::try_from(self.vertices.get(ordinal)?.degree).ok()?;
        self.base_entries.push(entry);
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        vertex_entry.degree = vertex_entry.degree.checked_add(1)?;
        self.dirty_regions.vertex_table = true;
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(logical_index)
    }

    /// Reuses one tombstoned slot within the current base-capacity span.
    pub fn reuse_tombstoned_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
        entry: EdgeEntry,
    ) -> Option<usize> {
        let base = self.base_neighborhood(ordinal)?;
        let capacity_end = self.base_capacity_end_exclusive(ordinal)?;
        let slot = usize::try_from(base.start.raw + logical_index as u64).ok()?;
        if slot >= capacity_end || !self.base_entries.get(slot)?.meta.is_tombstone() {
            return None;
        }
        self.base_entries[slot] = entry;
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        if logical_index == usize::try_from(vertex_entry.degree).ok()? {
            vertex_entry.degree = vertex_entry.degree.checked_add(1)?;
            self.dirty_regions.vertex_table = true;
        }
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(logical_index)
    }

    /// Inserts one entry into the canonical base interval without shifting
    /// later base entries.
    pub fn insert_base_entry(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
    ) -> Option<EdgeInsertPath> {
        match self.choose_base_insert_slot(ordinal)? {
            BaseInsertDecision::Append { .. } => {
                let inserted = self.append_base_entry(ordinal, entry)?;
                Some(EdgeInsertPath::BaseAppend {
                    logical_index: inserted,
                })
            }
            BaseInsertDecision::ReuseTombstone { .. } => {
                let inserted = self.reuse_tombstoned_base_entry(
                    ordinal,
                    match self.choose_base_insert_slot(ordinal)? {
                        BaseInsertDecision::ReuseTombstone { logical_index } => logical_index,
                        BaseInsertDecision::Append { .. } => return None,
                    },
                    entry,
                )?;
                Some(EdgeInsertPath::BaseReuseTombstone {
                    logical_index: inserted,
                })
            }
        }
    }

    /// Materializes the exact-label base subrange as base entries.
    pub fn base_entries_for_label(
        &self,
        ordinal: usize,
        label_id: LabelId,
    ) -> Option<Vec<EdgeEntry>> {
        let range = self.label_range_for(ordinal, label_id)?;
        let start = usize::try_from(range.start).ok()?;
        let end = start.checked_add(usize::try_from(range.len).ok()?)?;
        Some(self.base_entries.get(start..end)?.to_vec())
    }

    /// Returns the exact-label base subrange as a typed view.
    pub fn label_neighborhood(
        &self,
        ordinal: usize,
        label_id: LabelId,
    ) -> Option<LabelNeighborhood> {
        let range = self.label_range_for(ordinal, label_id)?;
        Some(self.layout.label_neighborhood(range))
    }

    /// Resolves the physical locator for one base entry in a vertex-local interval.
    pub fn base_edge_locator(
        &self,
        vertex: NodeId,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeLocator> {
        let base = self.base_neighborhood(ordinal)?;
        let degree = usize::try_from(base.degree).ok()?;
        if logical_index >= degree {
            return None;
        }
        let slot = u32::try_from(base.start.raw + logical_index as u64).ok()?;
        Some(EdgeLocator::new(self.layout.kind, vertex, slot))
    }

    /// Resolves the physical locator for one overflow entry in a vertex-local chain.
    pub fn overflow_edge_locator(
        &self,
        vertex: NodeId,
        ordinal: usize,
        overflow_index: usize,
    ) -> Option<EdgeLocator> {
        let overflow = self.overflow_entries_for(vertex, ordinal)?;
        if overflow_index >= overflow.len() {
            return None;
        }
        let entry = overflow.get(overflow_index)?;
        let head = self.overflow_chain(vertex, ordinal)?.head;
        let slot = if overflow_index == 0 {
            u32::try_from(head.raw).ok()?
        } else {
            let mut next = head;
            let mut current_slot = None;
            for _ in 0..=overflow_index {
                let current = self.overflow_entry(next)?;
                current_slot = u32::try_from(next.raw).ok();
                next = current.next;
            }
            current_slot?
        };
        let _ = entry;
        Some(EdgeLocator::new(self.layout.kind, vertex, slot))
    }

    /// Resolves the physical locator for one logical position in merged read order.
    pub fn edge_locator_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeLocator> {
        let base = self.base_neighborhood(ordinal)?;
        let base_degree = usize::try_from(base.degree).ok()?;
        if logical_index < base_degree {
            return self.base_edge_locator(vertex, ordinal, logical_index);
        }
        self.overflow_edge_locator(vertex, ordinal, logical_index - base_degree)
    }

    /// Classifies one locator as either a base slot or an overflow-log slot.
    pub fn resolve_edge_slot(
        &self,
        vertex: NodeId,
        ordinal: usize,
        locator: EdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        if locator.surface_kind() != self.layout.kind || locator.vertex != vertex {
            return None;
        }

        let base = self.base_neighborhood(ordinal)?;
        let start = u32::try_from(base.start.raw).ok()?;
        let end = u32::try_from(base.end_exclusive().raw).ok()?;
        if (start..end).contains(&locator.ordinal) {
            return Some(ResolvedEdgeSlot::Base {
                logical_index: usize::try_from(locator.ordinal - start).ok()?,
            });
        }

        let mut next = self.overflow_chain(vertex, ordinal)?.head;
        let mut overflow_index = 0usize;
        while !next.is_empty() {
            if u32::try_from(next.raw).ok()? == locator.ordinal {
                return Some(ResolvedEdgeSlot::Overflow {
                    overflow_index,
                    offset: next,
                });
            }
            let entry = self.overflow_entry(next)?;
            next = entry.next;
            overflow_index += 1;
        }

        None
    }

    /// Replaces one overflow entry identified by semantic edge id.
    pub fn replace_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
        new_entry: EdgeEntry,
    ) -> Option<OverflowEntry> {
        let mut next = self.overflow_chain(vertex, ordinal)?.head;
        while !next.is_empty() {
            let index = usize::try_from(next.raw).ok()?;
            let current = *self.overflow_entries.get(index)?;
            if current.edge_id == edge_id {
                self.overflow_entries[index] = OverflowEntry::new(edge_id, new_entry, current.next);
                self.dirty_regions.segment_log = true;
                return Some(current);
            }
            next = current.next;
        }
        None
    }

    /// Tombstones one overflow entry identified by semantic edge id.
    pub fn tombstone_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
    ) -> Option<OverflowEntry> {
        let mut next = self.overflow_chain(vertex, ordinal)?.head;
        while !next.is_empty() {
            let index = usize::try_from(next.raw).ok()?;
            let current = *self.overflow_entries.get(index)?;
            if current.edge_id == edge_id {
                self.overflow_entries[index] = OverflowEntry::new(
                    edge_id,
                    EdgeEntry::new(
                        current.entry.target,
                        current.entry.meta.with_tombstone(true),
                    ),
                    current.next,
                );
                self.dirty_regions.segment_log = true;
                return Some(current);
            }
            next = current.next;
        }
        None
    }

    /// Adds all known locators for one merged neighborhood into the provided sidecar.
    pub fn populate_locator_sidecar_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        base_edge_ids: &[EdgeId],
        sidecar: &mut EdgeLocatorSidecar,
    ) -> Option<()> {
        let base = self.base_neighborhood(ordinal)?;
        let base_degree = usize::try_from(base.degree).ok()?;
        if base_edge_ids.len() != base_degree {
            return None;
        }

        for (logical_index, &edge_id) in base_edge_ids.iter().enumerate() {
            let locator = self.base_edge_locator(vertex, ordinal, logical_index)?;
            sidecar.set(edge_id, locator);
        }

        let overflow_entries = self.overflow_entries_for(vertex, ordinal)?;
        for (overflow_index, entry) in overflow_entries.into_iter().enumerate() {
            let locator = self.overflow_edge_locator(vertex, ordinal, overflow_index)?;
            sidecar.set(entry.edge_id, locator);
        }

        Some(())
    }

    /// Builds a fresh locator sidecar for one merged neighborhood.
    pub fn build_locator_sidecar_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        base_edge_ids: &[EdgeId],
    ) -> Option<EdgeLocatorSidecar> {
        let mut sidecar = EdgeLocatorSidecar::new();
        self.populate_locator_sidecar_for(vertex, ordinal, base_edge_ids, &mut sidecar)?;
        Some(sidecar)
    }

    /// Populates a locator sidecar for every vertex ordinal in this surface.
    ///
    /// `vertex_ids` and `base_edge_ids_by_ordinal` must both be aligned with the
    /// surface's vertex ordinals. Base edge ids are supplied externally because
    /// canonical base entries intentionally do not carry semantic `EdgeId`s.
    pub fn populate_locator_sidecar_from_vertex_base_ids(
        &self,
        vertex_ids: &[NodeId],
        base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        sidecar: &mut EdgeLocatorSidecar,
    ) -> Option<()> {
        if vertex_ids.len() != self.vertices.len()
            || base_edge_ids_by_ordinal.len() != self.vertices.len()
        {
            return None;
        }

        for ordinal in 0..self.vertices.len() {
            self.populate_locator_sidecar_for(
                vertex_ids[ordinal],
                ordinal,
                &base_edge_ids_by_ordinal[ordinal],
                sidecar,
            )?;
        }

        Some(())
    }

    /// Builds a fresh locator sidecar for the entire surface.
    pub fn build_locator_sidecar_from_vertex_base_ids(
        &self,
        vertex_ids: &[NodeId],
        base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<EdgeLocatorSidecar> {
        let mut sidecar = EdgeLocatorSidecar::new();
        self.populate_locator_sidecar_from_vertex_base_ids(
            vertex_ids,
            base_edge_ids_by_ordinal,
            &mut sidecar,
        )?;
        Some(sidecar)
    }

    /// Populates locator entries only for one contiguous vertex window.
    pub fn populate_locator_sidecar_for_window(
        &self,
        start_ordinal: usize,
        vertex_ids: &[NodeId],
        base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        sidecar: &mut EdgeLocatorSidecar,
    ) -> Option<()> {
        let end_ordinal_exclusive = start_ordinal.checked_add(vertex_ids.len())?;
        if end_ordinal_exclusive > self.vertices.len()
            || base_edge_ids_by_ordinal.len() != vertex_ids.len()
        {
            return None;
        }

        for (offset, vertex_id) in vertex_ids.iter().copied().enumerate() {
            let ordinal = start_ordinal + offset;
            self.populate_locator_sidecar_for(
                vertex_id,
                ordinal,
                &base_edge_ids_by_ordinal[offset],
                sidecar,
            )?;
        }

        Some(())
    }

    /// Builds a pure local-rebalance delta for one surface-local vertex window.
    ///
    /// The returned delta compacts live base and overflow entries back into one
    /// contiguous base slice and resets every rewritten vertex to
    /// `EMPTY_LOG_OFFSET`. Applying the delta is a separate step.
    pub fn build_local_rebalance_delta(
        &self,
        anchor_ordinal: usize,
        start_ordinal: usize,
        end_ordinal_exclusive: usize,
        reserved_base_len: u32,
    ) -> Option<SurfaceLocalRebalanceDelta> {
        if start_ordinal >= end_ordinal_exclusive
            || end_ordinal_exclusive > self.vertices.len()
            || anchor_ordinal < start_ordinal
            || anchor_ordinal >= end_ordinal_exclusive
        {
            return None;
        }

        let layout = self.build_weighted_window_layout(
            anchor_ordinal,
            start_ordinal,
            end_ordinal_exclusive,
            reserved_base_len,
        )?;
        let mut live_entries_by_ordinal =
            Vec::with_capacity(end_ordinal_exclusive.saturating_sub(start_ordinal));
        for ordinal in start_ordinal..end_ordinal_exclusive {
            live_entries_by_ordinal.push(self.merged_live_entries_for_ordinal(ordinal)?);
        }
        let mut compacted_base_entries = Vec::new();
        let mut rewritten_vertices =
            Vec::with_capacity(end_ordinal_exclusive.saturating_sub(start_ordinal));

        for (offset, live_entries) in live_entries_by_ordinal.into_iter().enumerate() {
            let degree = *layout.live_degrees.get(offset)?;
            let reserved_len = *layout.reserved_lengths.get(offset)?;
            rewritten_vertices.push(VertexEntry::new(
                *layout.positions.get(offset)?,
                degree,
                EMPTY_LOG_OFFSET,
            ));
            compacted_base_entries.extend(live_entries);
            let placeholder_count = usize::try_from(reserved_len.checked_sub(degree)?).ok()?;
            compacted_base_entries.extend(std::iter::repeat_n(
                EdgeEntry::new(
                    NodeId::default(),
                    super::edge::EdgeMeta::UNLABELED.with_tombstone(true),
                ),
                placeholder_count,
            ));
        }

        Some(SurfaceLocalRebalanceDelta {
            start_ordinal,
            end_ordinal_exclusive,
            base_start: layout.base_start,
            compacted_base_entries,
            rewritten_vertices,
            reserved_base_len,
        })
    }

    /// Summarizes reclaimable base slack for one surface-local vertex window.
    ///
    /// Reclaimable slack is currently defined as tombstoned base-capacity
    /// slots inside the window, including reserved slack that lives beyond the
    /// current live degree of one vertex neighborhood.
    pub fn summarize_window_slack(
        &self,
        start_ordinal: usize,
        end_ordinal_exclusive: usize,
    ) -> Option<SurfaceWindowSlackSummary> {
        if start_ordinal >= end_ordinal_exclusive || end_ordinal_exclusive > self.vertices.len() {
            return None;
        }

        let mut total_base_slots = 0usize;
        let mut reclaimable_tombstones = 0usize;
        let mut overflow_entries_in_window = 0usize;

        for ordinal in start_ordinal..end_ordinal_exclusive {
            let base = self.base_neighborhood(ordinal)?;
            let capacity_end = self.base_capacity_end_exclusive(ordinal)?;
            let start = usize::try_from(base.start.raw).ok()?;
            let capacity_entries = self.base_entries.get(start..capacity_end)?;
            total_base_slots = total_base_slots.checked_add(capacity_entries.len())?;
            reclaimable_tombstones = reclaimable_tombstones.checked_add(
                capacity_entries
                    .iter()
                    .filter(|entry| entry.meta.is_tombstone())
                    .count(),
            )?;
            overflow_entries_in_window = overflow_entries_in_window
                .checked_add(self.overflow_entries_for_ordinal(ordinal)?.len())?;
        }

        Some(SurfaceWindowSlackSummary {
            start_ordinal,
            end_ordinal_exclusive,
            total_base_slots,
            reclaimable_tombstones,
            overflow_entries_in_window,
        })
    }

    /// Applies one previously built local-rebalance delta to this surface runtime.
    ///
    /// This rewrites the affected contiguous base slice, replaces the window's
    /// vertex entries, and shifts subsequent `edge_index` values when the new
    /// compacted slice has a different length than the old one.
    ///
    /// Overflow payload bytes are not garbage-collected here. The delta only
    /// disconnects rewritten vertices from their old overflow chains by
    /// resetting `log_offset` in the rewritten vertex entries.
    pub fn apply_local_rebalance_delta(
        &mut self,
        delta: SurfaceLocalRebalanceDelta,
    ) -> Option<SurfaceAppliedRebalanceSummary> {
        if delta.start_ordinal >= delta.end_ordinal_exclusive
            || delta.end_ordinal_exclusive > self.vertices.len()
        {
            return None;
        }
        if delta.rewritten_vertices.len()
            != delta
                .end_ordinal_exclusive
                .checked_sub(delta.start_ordinal)?
        {
            return None;
        }

        let old_start = usize::try_from(delta.base_start.raw).ok()?;
        let old_end = if delta.end_ordinal_exclusive < self.vertices.len() {
            usize::try_from(
                self.vertex_entry(delta.end_ordinal_exclusive)?
                    .edge_index
                    .raw,
            )
            .ok()?
        } else {
            self.base_entries.len()
        };
        if old_start > old_end || old_end > self.base_entries.len() {
            return None;
        }

        let mut previous_index = None;
        for vertex in &delta.rewritten_vertices {
            if vertex.log_offset != EMPTY_LOG_OFFSET {
                return None;
            }
            if let Some(previous) = previous_index {
                if vertex.edge_index.raw < previous {
                    return None;
                }
            } else if vertex.edge_index != delta.base_start {
                return None;
            }
            previous_index = Some(vertex.edge_index.raw + u64::from(vertex.degree));
        }
        if usize::try_from(delta.reserved_base_len).ok()? != delta.compacted_base_entries.len() {
            return None;
        }

        let old_len = old_end.checked_sub(old_start)?;
        let new_len = delta.compacted_base_entries.len();
        self.base_entries
            .splice(old_start..old_end, delta.compacted_base_entries);

        for (offset, vertex) in delta.rewritten_vertices.into_iter().enumerate() {
            self.vertices[delta.start_ordinal + offset] = vertex;
        }

        let shift = i64::try_from(new_len).ok()? - i64::try_from(old_len).ok()?;
        if shift != 0 {
            for vertex in self.vertices.iter_mut().skip(delta.end_ordinal_exclusive) {
                let shifted = i128::from(vertex.edge_index.raw) + i128::from(shift);
                if shifted < 0 || shifted > i128::from(u64::MAX) {
                    return None;
                }
                vertex.edge_index = EdgeIndex::new(shifted as u64);
            }
        }

        self.dirty_regions.vertex_table = true;
        self.dirty_regions.edge_entries = true;
        for ordinal in delta.start_ordinal..self.vertices.len() {
            let _ = self.mark_vertex_dirty(ordinal);
        }
        Some(SurfaceAppliedRebalanceSummary {
            start_ordinal: delta.start_ordinal,
            end_ordinal_exclusive: delta.end_ordinal_exclusive,
            old_span_len: u32::try_from(old_len).ok()?,
            new_span_len: u32::try_from(new_len).ok()?,
            displacement: shift,
        })
    }
}

/// Forward read-side runtime wrapper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardSurfaceRuntime(pub SurfaceRuntime);

impl ForwardSurfaceRuntime {
    /// Creates a typed forward-surface runtime.
    pub fn new(
        surface: ForwardSurface,
        vertices: Vec<VertexEntry>,
        base_entries: Vec<EdgeEntry>,
        overflow_entries: Vec<OverflowEntry>,
    ) -> Self {
        Self(SurfaceRuntime::new(
            surface.layout(),
            vertices,
            base_entries,
            overflow_entries,
            Vec::new(),
            Vec::new(),
        ))
    }

    /// Creates a forward-surface runtime with no overflow or label sidecar.
    pub fn without_overflow(surface: ForwardSurface, vertices: Vec<VertexEntry>) -> Self {
        Self(SurfaceRuntime::without_overflow(surface.layout(), vertices))
    }

    /// Appends one empty forward vertex slot.
    pub fn append_empty_vertex(&mut self) -> Option<usize> {
        self.0.append_empty_vertex()
    }

    /// Returns the canonical base-neighborhood view for one vertex ordinal.
    pub fn base_neighborhood(&self, ordinal: usize) -> Option<BaseNeighborhood> {
        self.0.base_neighborhood(ordinal)
    }

    /// Returns the merged read-side view for one vertex-local neighborhood.
    pub fn merged_neighborhood(
        &self,
        vertex: NodeId,
        ordinal: usize,
    ) -> Option<MergedNeighborhoodView> {
        self.0.merged_neighborhood(vertex, ordinal)
    }

    /// Materializes the overflow chain for one vertex-local neighborhood.
    pub fn overflow_entries_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
    ) -> Option<Vec<OverflowEntry>> {
        self.0.overflow_entries_for(vertex, ordinal)
    }

    /// Materializes merged read order for one vertex-local neighborhood.
    pub fn merged_entries_for(&self, vertex: NodeId, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        self.0.merged_entries_for(vertex, ordinal)
    }

    /// Resolves one physical locator inside the forward surface.
    pub fn edge_locator_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeLocator> {
        self.0.edge_locator_for(vertex, ordinal, logical_index)
    }

    /// Resolves whether a locator points at a base slot or an overflow slot.
    pub fn resolve_edge_slot(
        &self,
        vertex: NodeId,
        ordinal: usize,
        locator: EdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.0.resolve_edge_slot(vertex, ordinal, locator)
    }

    /// Builds a locator sidecar for one forward vertex-local neighborhood.
    pub fn build_locator_sidecar_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        base_edge_ids: &[EdgeId],
    ) -> Option<EdgeLocatorSidecar> {
        self.0
            .build_locator_sidecar_for(vertex, ordinal, base_edge_ids)
    }

    /// Materializes the exact-label base subrange for one vertex ordinal.
    pub fn base_entries_for_label(
        &self,
        ordinal: usize,
        label_id: LabelId,
    ) -> Option<Vec<EdgeEntry>> {
        self.0.base_entries_for_label(ordinal, label_id)
    }

    /// Refreshes label-sidecar state for dirty forward vertices.
    pub fn refresh_label_sidecar_for_dirty_vertices(&mut self) -> Option<Vec<usize>> {
        self.0.refresh_label_sidecar_for_dirty_vertices()
    }

    /// Replaces one forward base entry.
    pub fn replace_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
        new_entry: EdgeEntry,
    ) -> Option<EdgeEntry> {
        self.0.replace_base_entry(ordinal, logical_index, new_entry)
    }

    /// Tombstones one forward base entry.
    pub fn tombstone_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeEntry> {
        self.0.tombstone_base_entry(ordinal, logical_index)
    }

    /// Appends one forward overflow entry.
    pub fn append_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
        entry: EdgeEntry,
    ) -> Option<LogOffset> {
        self.0
            .append_overflow_entry(vertex, ordinal, edge_id, entry)
    }

    /// Returns whether the forward base interval can accept a tail append.
    pub fn can_append_base_entry(&self, ordinal: usize) -> Option<bool> {
        self.0.can_append_base_entry(ordinal)
    }

    /// Returns the base-insert decision for the forward surface.
    pub fn choose_base_insert_slot(&self, ordinal: usize) -> Option<BaseInsertDecision> {
        self.0.choose_base_insert_slot(ordinal)
    }

    /// Appends one entry directly to the forward base interval tail.
    pub fn append_base_entry(&mut self, ordinal: usize, entry: EdgeEntry) -> Option<usize> {
        self.0.append_base_entry(ordinal, entry)
    }

    /// Inserts one entry into the forward base interval without shifting later entries.
    pub fn insert_base_entry(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
    ) -> Option<EdgeInsertPath> {
        self.0.insert_base_entry(ordinal, entry)
    }

    /// Replaces one forward overflow entry identified by semantic edge id.
    pub fn replace_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
        new_entry: EdgeEntry,
    ) -> Option<OverflowEntry> {
        self.0
            .replace_overflow_entry(vertex, ordinal, edge_id, new_entry)
    }

    /// Tombstones one forward overflow entry identified by semantic edge id.
    pub fn tombstone_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
    ) -> Option<OverflowEntry> {
        self.0.tombstone_overflow_entry(vertex, ordinal, edge_id)
    }

    /// Returns the exact-label base subrange as a typed view.
    pub fn label_neighborhood(
        &self,
        ordinal: usize,
        label_id: LabelId,
    ) -> Option<LabelNeighborhood> {
        self.0.label_neighborhood(ordinal, label_id)
    }
}

/// Reverse read-side runtime wrapper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseSurfaceRuntime(pub SurfaceRuntime);

impl ReverseSurfaceRuntime {
    /// Creates a typed reverse-surface runtime.
    pub fn new(
        surface: ReverseSurface,
        vertices: Vec<VertexEntry>,
        base_entries: Vec<EdgeEntry>,
        overflow_entries: Vec<OverflowEntry>,
    ) -> Self {
        Self(SurfaceRuntime::new(
            surface.layout(),
            vertices,
            base_entries,
            overflow_entries,
            Vec::new(),
            Vec::new(),
        ))
    }

    /// Creates a reverse-surface runtime with no overflow or label sidecar.
    pub fn without_overflow(surface: ReverseSurface, vertices: Vec<VertexEntry>) -> Self {
        Self(SurfaceRuntime::without_overflow(surface.layout(), vertices))
    }

    /// Appends one empty reverse vertex slot.
    pub fn append_empty_vertex(&mut self) -> Option<usize> {
        self.0.append_empty_vertex()
    }

    /// Returns the canonical base-neighborhood view for one vertex ordinal.
    pub fn base_neighborhood(&self, ordinal: usize) -> Option<BaseNeighborhood> {
        self.0.base_neighborhood(ordinal)
    }

    /// Returns the merged read-side view for one vertex-local neighborhood.
    pub fn merged_neighborhood(
        &self,
        vertex: NodeId,
        ordinal: usize,
    ) -> Option<MergedNeighborhoodView> {
        self.0.merged_neighborhood(vertex, ordinal)
    }

    /// Materializes the overflow chain for one vertex-local neighborhood.
    pub fn overflow_entries_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
    ) -> Option<Vec<OverflowEntry>> {
        self.0.overflow_entries_for(vertex, ordinal)
    }

    /// Materializes merged read order for one vertex-local neighborhood.
    pub fn merged_entries_for(&self, vertex: NodeId, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        self.0.merged_entries_for(vertex, ordinal)
    }

    /// Resolves one physical locator inside the reverse surface.
    pub fn edge_locator_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeLocator> {
        self.0.edge_locator_for(vertex, ordinal, logical_index)
    }

    /// Resolves whether a locator points at a base slot or an overflow slot.
    pub fn resolve_edge_slot(
        &self,
        vertex: NodeId,
        ordinal: usize,
        locator: EdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.0.resolve_edge_slot(vertex, ordinal, locator)
    }

    /// Builds a locator sidecar for one reverse vertex-local neighborhood.
    pub fn build_locator_sidecar_for(
        &self,
        vertex: NodeId,
        ordinal: usize,
        base_edge_ids: &[EdgeId],
    ) -> Option<EdgeLocatorSidecar> {
        self.0
            .build_locator_sidecar_for(vertex, ordinal, base_edge_ids)
    }

    /// Materializes the exact-label base subrange for one vertex ordinal.
    pub fn base_entries_for_label(
        &self,
        ordinal: usize,
        label_id: LabelId,
    ) -> Option<Vec<EdgeEntry>> {
        self.0.base_entries_for_label(ordinal, label_id)
    }

    /// Refreshes label-sidecar state for dirty reverse vertices.
    pub fn refresh_label_sidecar_for_dirty_vertices(&mut self) -> Option<Vec<usize>> {
        self.0.refresh_label_sidecar_for_dirty_vertices()
    }

    /// Replaces one reverse base entry.
    pub fn replace_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
        new_entry: EdgeEntry,
    ) -> Option<EdgeEntry> {
        self.0.replace_base_entry(ordinal, logical_index, new_entry)
    }

    /// Tombstones one reverse base entry.
    pub fn tombstone_base_entry(
        &mut self,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<EdgeEntry> {
        self.0.tombstone_base_entry(ordinal, logical_index)
    }

    /// Appends one reverse overflow entry.
    pub fn append_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
        entry: EdgeEntry,
    ) -> Option<LogOffset> {
        self.0
            .append_overflow_entry(vertex, ordinal, edge_id, entry)
    }

    /// Returns whether the reverse base interval can accept a tail append.
    pub fn can_append_base_entry(&self, ordinal: usize) -> Option<bool> {
        self.0.can_append_base_entry(ordinal)
    }

    /// Returns the base-insert decision for the reverse surface.
    pub fn choose_base_insert_slot(&self, ordinal: usize) -> Option<BaseInsertDecision> {
        self.0.choose_base_insert_slot(ordinal)
    }

    /// Appends one entry directly to the reverse base interval tail.
    pub fn append_base_entry(&mut self, ordinal: usize, entry: EdgeEntry) -> Option<usize> {
        self.0.append_base_entry(ordinal, entry)
    }

    /// Inserts one entry into the reverse base interval without shifting later entries.
    pub fn insert_base_entry(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
    ) -> Option<EdgeInsertPath> {
        self.0.insert_base_entry(ordinal, entry)
    }

    /// Replaces one reverse overflow entry identified by semantic edge id.
    pub fn replace_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
        new_entry: EdgeEntry,
    ) -> Option<OverflowEntry> {
        self.0
            .replace_overflow_entry(vertex, ordinal, edge_id, new_entry)
    }

    /// Tombstones one reverse overflow entry identified by semantic edge id.
    pub fn tombstone_overflow_entry(
        &mut self,
        vertex: NodeId,
        ordinal: usize,
        edge_id: EdgeId,
    ) -> Option<OverflowEntry> {
        self.0.tombstone_overflow_entry(vertex, ordinal, edge_id)
    }

    /// Returns the exact-label base subrange as a typed view.
    pub fn label_neighborhood(
        &self,
        ordinal: usize,
        label_id: LabelId,
    ) -> Option<LabelNeighborhood> {
        self.0.label_neighborhood(ordinal, label_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{ForwardSurfaceRuntime, ReverseSurfaceRuntime, SurfaceRuntime};
    use crate::low_level::{
        EdgeEntry, EdgeIndex, EdgeLocator, EdgeMeta, ForwardSurface, LogOffset, OverflowEntry,
        RegionKind, RegionRef, RegionStorageKind, ReverseSurface, SurfaceKind, SurfaceRegions,
        VertexEntry, VertexLabelIndexEntry, VertexLabelRange, EMPTY_LOG_OFFSET,
    };
    use gleaph_graph_kernel::NodeId;

    fn forward_surface() -> ForwardSurface {
        ForwardSurface::new(SurfaceRegions::new(
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
        ))
    }

    fn reverse_surface() -> ReverseSurface {
        ReverseSurface::new(SurfaceRegions::new(
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseVertexTable,
                1,
                128,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseEdgeEntries,
                2,
                4096,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseLabelIndex,
                3,
                256,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseSegmentLog,
                4,
                1024,
            ),
        ))
    }

    #[test]
    fn surface_runtime_builds_base_neighborhood_from_vertex_table() {
        let runtime = SurfaceRuntime::without_overflow(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(4), 3, EMPTY_LOG_OFFSET)],
        );

        let base = runtime.base_neighborhood(0).expect("base neighborhood");
        assert_eq!(base.surface, SurfaceKind::Forward);
        assert_eq!(base.start, EdgeIndex::new(4));
        assert_eq!(base.degree, 3);
    }

    #[test]
    fn surface_runtime_builds_merged_view_with_empty_overflow() {
        let runtime = ForwardSurfaceRuntime::without_overflow(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(4), 3, EMPTY_LOG_OFFSET)],
        );

        let merged = runtime
            .merged_neighborhood(NodeId::from(1u8), 0)
            .expect("merged neighborhood");
        assert_eq!(merged.base.start, EdgeIndex::new(4));
        assert!(!merged.has_overflow());
    }

    #[test]
    fn reverse_runtime_keeps_reverse_surface_kind_in_views() {
        let runtime = ReverseSurfaceRuntime::new(
            reverse_surface(),
            vec![VertexEntry::new(EdgeIndex::new(9), 2, 7)],
            vec![
                EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(21u8), EdgeMeta::new(2, false)),
                EdgeEntry::new(NodeId::from(22u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(23u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(NodeId::from(24u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(25u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(NodeId::from(26u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(27u8), EdgeMeta::new(8, false)),
                EdgeEntry::new(NodeId::from(28u8), EdgeMeta::new(9, false)),
                EdgeEntry::new(NodeId::from(29u8), EdgeMeta::new(10, false)),
                EdgeEntry::new(NodeId::from(30u8), EdgeMeta::new(11, false)),
            ],
            vec![OverflowEntry::new(
                77,
                EdgeEntry::new(NodeId::from(11u8), EdgeMeta::new(5, false)),
                LogOffset::EMPTY,
            )],
        );

        let merged = runtime
            .merged_neighborhood(NodeId::from(5u8), 0)
            .expect("merged neighborhood");
        assert_eq!(merged.base.surface, SurfaceKind::Reverse);
        assert!(merged.has_overflow());
        assert_eq!(merged.overflow.head.raw, 7);
    }

    #[test]
    fn surface_runtime_can_follow_overflow_chain() {
        let runtime = ForwardSurfaceRuntime::new(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(4), 3, 1)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let entries = runtime
            .overflow_entries_for(NodeId::from(1u8), 0)
            .expect("overflow entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].edge_id, 42);
        assert_eq!(entries[1].edge_id, 41);
    }

    #[test]
    fn surface_runtime_materializes_merged_entries_in_base_then_overflow_order() {
        let runtime = ForwardSurfaceRuntime::new(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(2), 2, 1)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let merged = runtime
            .merged_entries_for(NodeId::from(7u8), 0)
            .expect("merged entries");
        assert_eq!(merged.len(), 4);
        assert_eq!(u64::from(merged[0].target), 3);
        assert_eq!(u64::from(merged[1].target), 4);
        assert_eq!(u64::from(merged[2].target), 10);
        assert_eq!(u64::from(merged[3].target), 9);
    }

    #[test]
    fn edge_locator_for_maps_base_and_overflow_positions() {
        let runtime = ForwardSurfaceRuntime::new(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(2), 2, 1)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let vertex = NodeId::from(7u8);
        let base_locator = runtime
            .edge_locator_for(vertex, 0, 0)
            .expect("base locator");
        let overflow_locator = runtime
            .edge_locator_for(vertex, 0, 2)
            .expect("overflow locator");
        let overflow_locator_2 = runtime
            .edge_locator_for(vertex, 0, 3)
            .expect("overflow locator");

        assert_eq!(base_locator.surface_kind(), SurfaceKind::Forward);
        assert_eq!(base_locator.ordinal, 2);
        assert_eq!(overflow_locator.ordinal, 1);
        assert_eq!(overflow_locator_2.ordinal, 0);
    }

    #[test]
    fn runtime_can_build_locator_sidecar_from_base_and_overflow_entries() {
        let runtime = ForwardSurfaceRuntime::new(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(2), 2, 1)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let sidecar = runtime
            .build_locator_sidecar_for(NodeId::from(7u8), 0, &[11, 12])
            .expect("sidecar");

        assert_eq!(sidecar.get(11).expect("base locator").ordinal, 2);
        assert_eq!(sidecar.get(12).expect("base locator").ordinal, 3);
        assert_eq!(sidecar.get(42).expect("overflow locator").ordinal, 1);
        assert_eq!(sidecar.get(41).expect("overflow locator").ordinal, 0);
    }

    #[test]
    fn runtime_rejects_locator_sidecar_build_when_base_edge_ids_do_not_match_degree() {
        let runtime = ForwardSurfaceRuntime::without_overflow(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(2), 2, EMPTY_LOG_OFFSET)],
        );

        assert!(runtime
            .build_locator_sidecar_for(NodeId::from(7u8), 0, &[11])
            .is_none());
    }

    #[test]
    fn runtime_can_build_locator_sidecar_for_whole_surface() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(2), 1, 0),
            ],
            vec![
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(5, false)),
            ],
            vec![OverflowEntry::new(
                30,
                EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(6, false)),
                LogOffset::EMPTY,
            )],
            Vec::new(),
            Vec::new(),
        );

        let sidecar = runtime
            .build_locator_sidecar_from_vertex_base_ids(
                &[NodeId::from(7u8), NodeId::from(8u8)],
                &[vec![10, 11], vec![12]],
            )
            .expect("full surface sidecar");

        assert_eq!(
            sidecar.get(10),
            Some(EdgeLocator::new(SurfaceKind::Forward, NodeId::from(7u8), 0))
        );
        assert_eq!(
            sidecar.get(11),
            Some(EdgeLocator::new(SurfaceKind::Forward, NodeId::from(7u8), 1))
        );
        assert_eq!(
            sidecar.get(12),
            Some(EdgeLocator::new(SurfaceKind::Forward, NodeId::from(8u8), 2))
        );
        assert_eq!(
            sidecar.get(30),
            Some(EdgeLocator::new(SurfaceKind::Forward, NodeId::from(8u8), 0))
        );
    }

    #[test]
    fn runtime_can_read_vertex_label_ranges() {
        let mut runtime = SurfaceRuntime::without_overflow(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
        );
        runtime.base_entries = vec![
            EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(3, false)),
            EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(4, false)),
        ];
        runtime.label_index_entries = vec![VertexLabelIndexEntry { start: 0, len: 2 }];
        runtime.label_ranges = vec![
            VertexLabelRange {
                label_id: 3,
                start: 0,
                len: 1,
            },
            VertexLabelRange {
                label_id: 4,
                start: 1,
                len: 1,
            },
        ];

        let ranges = runtime
            .label_ranges_for(0)
            .expect("label ranges should exist");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].label_id, 3);
        assert_eq!(ranges[1].label_id, 4);

        let label_entries = runtime
            .base_entries_for_label(0, 4)
            .expect("label-specific entries should exist");
        assert_eq!(label_entries.len(), 1);
        assert_eq!(u64::from(label_entries[0].target), 20);

        let label_view = runtime
            .label_neighborhood(0, 4)
            .expect("label neighborhood should exist");
        assert_eq!(label_view.label_id, 4);
        assert_eq!(label_view.start, EdgeIndex::new(1));
        assert_eq!(label_view.degree, 1);
        assert_eq!(label_view.end_exclusive(), EdgeIndex::new(2));
    }

    #[test]
    fn runtime_can_materialize_label_sidecar_from_base_entries() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 4, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(4), 3, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(6u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(7u8), EdgeMeta::new(6, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let (index_entries, ranges) = runtime
            .materialize_label_sidecar()
            .expect("materialized sidecar");

        assert_eq!(index_entries.len(), 2);
        assert_eq!(index_entries[0], VertexLabelIndexEntry::new(0, 2));
        assert_eq!(index_entries[1], VertexLabelIndexEntry::new(2, 2));

        assert_eq!(ranges[0], VertexLabelRange::new(3, 0, 2));
        assert_eq!(ranges[1], VertexLabelRange::new(3, 3, 1));
        assert_eq!(ranges[2], VertexLabelRange::new(5, 4, 2));
        assert_eq!(ranges[3], VertexLabelRange::new(6, 6, 1));
    }

    #[test]
    fn runtime_can_rebuild_label_sidecar_in_place() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(1), 3, EMPTY_LOG_OFFSET)],
            vec![
                EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(21u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(30u8), EdgeMeta::new(8, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        runtime.rebuild_label_sidecar().expect("rebuilt sidecar");

        assert_eq!(
            runtime.label_index_entries,
            vec![VertexLabelIndexEntry::new(0, 2)]
        );
        assert_eq!(
            runtime.label_ranges,
            vec![
                VertexLabelRange::new(7, 1, 2),
                VertexLabelRange::new(8, 3, 1),
            ]
        );
    }

    #[test]
    fn runtime_can_rebuild_label_sidecar_for_one_vertex_and_shift_following_offsets() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 3, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(3), 2, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
                EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(9, false)),
            ],
            Vec::new(),
            vec![
                VertexLabelIndexEntry::new(0, 2),
                VertexLabelIndexEntry::new(2, 1),
            ],
            vec![
                VertexLabelRange::new(7, 0, 2),
                VertexLabelRange::new(8, 2, 1),
                VertexLabelRange::new(9, 3, 2),
            ],
        );

        runtime.base_entries[2] = EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(7, false));
        runtime
            .rebuild_label_sidecar_for_vertex(0)
            .expect("rebuilt one vertex");

        assert_eq!(
            runtime.label_index_entries[0],
            VertexLabelIndexEntry::new(0, 1)
        );
        assert_eq!(
            runtime.label_index_entries[1],
            VertexLabelIndexEntry::new(1, 1)
        );
        assert_eq!(
            runtime.label_ranges,
            vec![
                VertexLabelRange::new(7, 0, 3),
                VertexLabelRange::new(9, 3, 2),
            ]
        );
    }

    #[test]
    fn runtime_local_label_rebuild_falls_back_to_full_rebuild_when_uninitialized() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(4, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        runtime
            .rebuild_label_sidecar_for_vertex(0)
            .expect("fallback rebuild");

        assert_eq!(
            runtime.label_index_entries,
            vec![VertexLabelIndexEntry::new(0, 2)]
        );
        assert_eq!(
            runtime.label_ranges,
            vec![
                VertexLabelRange::new(3, 0, 1),
                VertexLabelRange::new(4, 1, 1),
            ]
        );
    }

    #[test]
    fn base_mutation_marks_vertex_dirty_and_refreshes_only_those_vertices() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(2), 2, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(5, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        runtime.rebuild_label_sidecar().expect("initial sidecar");
        runtime
            .replace_base_entry(
                0,
                1,
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(3, false)),
            )
            .expect("base update");

        let dirty: Vec<_> = runtime.dirty_vertices().collect();
        assert_eq!(dirty, vec![0]);

        let refreshed = runtime
            .refresh_label_sidecar_for_dirty_vertices()
            .expect("refreshed");
        assert_eq!(refreshed, vec![0]);
        assert!(runtime.dirty_vertices().next().is_none());
        assert_eq!(
            runtime.label_index_entries[0],
            VertexLabelIndexEntry::new(0, 1)
        );
        assert_eq!(
            runtime.label_index_entries[1],
            VertexLabelIndexEntry::new(1, 1)
        );
        assert_eq!(
            runtime.label_ranges,
            vec![
                VertexLabelRange::new(3, 0, 2),
                VertexLabelRange::new(5, 2, 2),
            ]
        );
    }

    #[test]
    fn tombstone_base_entry_marks_vertex_dirty() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(4, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let old = runtime.tombstone_base_entry(0, 1).expect("tombstoned");
        assert_eq!(old.meta.label_id(), 4);
        assert_eq!(runtime.dirty_vertices().collect::<Vec<_>>(), vec![0]);
        assert!(runtime.base_entries[1].meta.is_tombstone());
    }

    #[test]
    fn overflow_append_updates_log_head_without_marking_label_sidecar_dirty() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
            vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(3, false))],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let offset = runtime
            .append_overflow_entry(
                NodeId::from(9u8),
                0,
                77,
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(4, false)),
            )
            .expect("overflow append");

        assert_eq!(offset.raw, 0);
        assert_eq!(runtime.vertices[0].log_offset, 0);
        assert!(runtime.dirty_vertices().next().is_none());
        assert_eq!(runtime.overflow_entries.len(), 1);
        assert_eq!(runtime.overflow_entries[0].edge_id, 77);
    }

    #[test]
    fn runtime_can_build_local_rebalance_delta_from_base_and_overflow() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, 0),
                VertexEntry::new(EdgeIndex::new(2), 1, 2),
            ],
            vec![
                EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(11u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(NodeId::from(12u8), EdgeMeta::new(5, false)),
            ],
            vec![
                OverflowEntry::new(
                    70,
                    EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(6, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    71,
                    EdgeEntry::new(NodeId::from(21u8), EdgeMeta::new(7, true)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    72,
                    EdgeEntry::new(NodeId::from(22u8), EdgeMeta::new(8, false)),
                    LogOffset::EMPTY,
                ),
            ],
            Vec::new(),
            Vec::new(),
        );

        let delta = runtime
            .build_local_rebalance_delta(0, 0, 2, 4)
            .expect("rebalance delta");

        assert_eq!(delta.start_ordinal, 0);
        assert_eq!(delta.end_ordinal_exclusive, 2);
        assert_eq!(delta.base_start, EdgeIndex::new(0));
        assert_eq!(
            delta.compacted_base_entries,
            vec![
                EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(NodeId::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(22u8), EdgeMeta::new(8, false)),
            ]
        );
        assert_eq!(
            delta.rewritten_vertices,
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(2), 2, EMPTY_LOG_OFFSET),
            ]
        );
    }

    #[test]
    fn runtime_can_summarize_window_slack_from_tombstoned_base_slots() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, true)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let summary = runtime
            .summarize_window_slack(0, 2)
            .expect("window slack summary");
        assert_eq!(summary.total_base_slots, 3);
        assert_eq!(summary.reclaimable_tombstones, 2);
        assert_eq!(summary.overflow_entries_in_window, 0);
        assert!(summary.has_reclaimable_slack());
        assert!(summary.can_absorb_additional_live_entries(1));
    }

    #[test]
    fn runtime_window_slack_accounts_for_existing_overflow_pressure() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, 0),
                VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(8, true)),
                EdgeEntry::new(NodeId::from(4u8), EdgeMeta::new(9, false)),
            ],
            vec![OverflowEntry::new(
                90,
                EdgeEntry::new(NodeId::from(5u8), EdgeMeta::new(10, false)),
                LogOffset::EMPTY,
            )],
            Vec::new(),
            Vec::new(),
        );

        let summary = runtime
            .summarize_window_slack(0, 2)
            .expect("window slack summary");
        assert_eq!(summary.reclaimable_tombstones, 1);
        assert_eq!(summary.overflow_entries_in_window, 1);
        assert!(!summary.can_absorb_additional_live_entries(1));
    }

    #[test]
    fn runtime_can_build_weighted_window_layout() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(2), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(11u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(NodeId::from(12u8), EdgeMeta::new(5, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let layout = runtime
            .build_weighted_window_layout(0, 0, 2, 6)
            .expect("weighted window layout");

        assert_eq!(layout.base_start, EdgeIndex::new(0));
        assert_eq!(layout.live_degrees, vec![2, 1]);
        assert_eq!(layout.capacity_span_len(), 6);
        assert_eq!(layout.reserved_lengths.iter().copied().sum::<u32>(), 6);
        assert_eq!(layout.positions, vec![EdgeIndex::new(0), EdgeIndex::new(3)]);
        assert_eq!(layout.end_exclusive(), Some(EdgeIndex::new(6)));
        assert_eq!(layout.displacement_against_current_span(3), 3);
    }

    #[test]
    fn runtime_can_apply_local_rebalance_delta_and_shift_following_vertices() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, 0),
                VertexEntry::new(EdgeIndex::new(2), 1, 2),
                VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(11u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(NodeId::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(13u8), EdgeMeta::new(6, false)),
            ],
            vec![
                OverflowEntry::new(
                    70,
                    EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(6, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    71,
                    EdgeEntry::new(NodeId::from(21u8), EdgeMeta::new(7, true)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    72,
                    EdgeEntry::new(NodeId::from(22u8), EdgeMeta::new(8, false)),
                    LogOffset::EMPTY,
                ),
            ],
            vec![
                VertexLabelIndexEntry::new(0, 0),
                VertexLabelIndexEntry::new(0, 0),
                VertexLabelIndexEntry::new(0, 0),
            ],
            Vec::new(),
        );

        let delta = runtime
            .build_local_rebalance_delta(0, 0, 2, 4)
            .expect("rebalance delta");
        let summary = runtime
            .apply_local_rebalance_delta(delta)
            .expect("apply rebalance delta");
        assert_eq!(summary.old_span_len, 3);
        assert_eq!(summary.new_span_len, 4);
        assert_eq!(summary.displacement, 1);

        assert_eq!(
            runtime.base_entries,
            vec![
                EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(NodeId::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(NodeId::from(22u8), EdgeMeta::new(8, false)),
                EdgeEntry::new(NodeId::from(13u8), EdgeMeta::new(6, false)),
            ]
        );
        assert_eq!(
            runtime.vertices[0],
            VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)
        );
        assert_eq!(
            runtime.vertices[1],
            VertexEntry::new(EdgeIndex::new(2), 2, EMPTY_LOG_OFFSET)
        );
        assert_eq!(
            runtime.vertices[2],
            VertexEntry::new(EdgeIndex::new(4), 1, EMPTY_LOG_OFFSET)
        );
        assert_eq!(runtime.dirty_vertices().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(runtime.dirty_regions.vertex_table);
        assert!(runtime.dirty_regions.edge_entries);
    }
}
