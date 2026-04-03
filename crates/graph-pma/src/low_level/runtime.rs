//! In-memory surface runtimes built from low-level adjacency regions.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};

use super::edge::{EdgeEntry, LogicalEdgeLocator};
use super::ids::{EdgeRef, VertexRef};
use super::locator::EdgeLogicalLocatorSidecar;
use super::overflow::{LogOffset, OverflowChain, OverflowEntry};
use super::surface::{
    BaseNeighborhood, ForwardSurface, LabelNeighborhood, MergedNeighborhoodView, ReverseSurface,
    SurfaceLayout,
};
use super::vertex::{
    EMPTY_LOG_OFFSET, EdgeIndex, VertexEntry, VertexLabelIndexEntry, VertexLabelRange,
};
use gleaph_graph_kernel::{EdgeId, LabelId};

/// Minimal read-side runtime for one directional surface.
///
/// This is intentionally not a stable-memory IO layer yet. It only bundles the
/// surface layout, the vertex table, and enough accessors to produce
/// `BaseNeighborhood` and `MergedNeighborhoodView`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceRuntime {
    pub layout: SurfaceLayout,
    pub vertices: Vec<VertexEntry>,
    pub base_entries: SurfaceBaseStorage,
    pub overflow_entries: Vec<OverflowEntry>,
    pub label_index_entries: Vec<VertexLabelIndexEntry>,
    pub label_ranges: Vec<VertexLabelRange>,
    pub dirty_regions: SurfaceDirtyRegions,
    pub dirty_vertices: BTreeSet<usize>,
}

/// In-memory backing store for one surface's canonical base adjacency region.
///
/// Prefer [`Self::len`], [`Self::get`], [`Self::get_by_ref`], [`Self::segment_entries`],
/// [`Self::iter`], and related helpers over treating this type as a flat slice via [`Deref`], so
/// call sites stay correct as backing layout evolves.
///
/// **Span rewrites:** use [`Self::rewrite_vertex_window_span`] or [`Self::rewrite_span_by_ref`]
/// from outside this crate. [`Self::rewrite_span`] is `pub(crate)` and implements the splice on a
/// **global flattened index range** that must lie within a single segment; segmented backing rejects
/// cross-segment spans.
///
/// **Maintenance narrative:** graph-level structural work should prefer fresh edge segments plus
/// segment-local windows—see
/// [`GraphRuntime::apply_local_rebalance_delta_with_segment_replacement`](crate::low_level::graph::GraphRuntime::apply_local_rebalance_delta_with_segment_replacement).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SurfaceBaseStorage {
    backing: SurfaceBaseBacking,
    segment_layout: SurfaceBaseSegmentLayout,
}

/// Physical backing for one surface's canonical base adjacency region.
///
/// Today this is still one contiguous vector, but this enum is the seam for a
/// future segmented implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SurfaceBaseBacking {
    Contiguous(Vec<EdgeEntry>),
    Segmented(SegmentedBaseBacking),
}

impl Default for SurfaceBaseBacking {
    fn default() -> Self {
        Self::Contiguous(Vec::new())
    }
}

impl SurfaceBaseBacking {
    fn len(&self) -> usize {
        match self {
            Self::Contiguous(entries) => entries.len(),
            Self::Segmented(backing) => backing.len(),
        }
    }

    fn as_slice(&self) -> &[EdgeEntry] {
        match self {
            Self::Contiguous(entries) => entries.as_slice(),
            Self::Segmented(backing) => backing.flat_entries.as_slice(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [EdgeEntry] {
        match self {
            Self::Contiguous(entries) => entries.as_mut_slice(),
            Self::Segmented(backing) => backing.flat_entries.as_mut_slice(),
        }
    }

    fn get(&self, index: usize) -> Option<&EdgeEntry> {
        match self {
            Self::Contiguous(entries) => entries.get(index),
            Self::Segmented(backing) => backing.flat_entries.get(index),
        }
    }

    fn get_slice(&self, start: usize, end: usize) -> Option<&[EdgeEntry]> {
        match self {
            Self::Contiguous(entries) => entries.get(start..end),
            Self::Segmented(backing) => backing.flat_entries.get(start..end),
        }
    }

    fn global_index_for_ref(&self, edge_ref: EdgeRef) -> Option<usize> {
        match self {
            Self::Contiguous(_) => usize::try_from(edge_ref.start_slot()).ok(),
            Self::Segmented(backing) => backing.global_index_for_ref(edge_ref),
        }
    }

    fn global_span_for_ref_count(&self, edge_ref: EdgeRef, count: usize) -> Option<(usize, usize)> {
        let start = self.global_index_for_ref(edge_ref)?;
        let end = start.checked_add(count)?;
        match self {
            Self::Contiguous(entries) => (end <= entries.len()).then_some((start, end)),
            Self::Segmented(backing) => {
                backing.get_slice_by_ref(edge_ref, count)?;
                Some((start, end))
            }
        }
    }

    fn get_by_ref(&self, edge_ref: EdgeRef) -> Option<EdgeEntry> {
        match self {
            Self::Contiguous(entries) => entries
                .get(usize::try_from(edge_ref.start_slot()).ok()?)
                .copied(),
            Self::Segmented(backing) => backing.get_by_ref(edge_ref),
        }
    }

    fn get_slice_by_ref(&self, edge_ref: EdgeRef, count: usize) -> Option<&[EdgeEntry]> {
        match self {
            Self::Contiguous(entries) => {
                let start = usize::try_from(edge_ref.start_slot()).ok()?;
                entries.get(start..start.checked_add(count)?)
            }
            Self::Segmented(backing) => backing.get_slice_by_ref(edge_ref, count),
        }
    }

    fn push(&mut self, entry: EdgeEntry) {
        match self {
            Self::Contiguous(entries) => entries.push(entry),
            Self::Segmented(backing) => backing.push_contiguous_to_segment_zero(entry),
        }
    }

    fn replace(&mut self, index: usize, entry: EdgeEntry) -> Option<EdgeEntry> {
        match self {
            Self::Contiguous(entries) => {
                let old = *entries.get(index)?;
                entries[index] = entry;
                Some(old)
            }
            Self::Segmented(backing) => backing.replace_global(index, entry),
        }
    }

    fn replace_by_ref(&mut self, edge_ref: EdgeRef, entry: EdgeEntry) -> Option<EdgeEntry> {
        match self {
            Self::Contiguous(entries) => {
                let index = usize::try_from(edge_ref.start_slot()).ok()?;
                let old = *entries.get(index)?;
                entries[index] = entry;
                Some(old)
            }
            Self::Segmented(backing) => backing.replace_by_ref(edge_ref, entry),
        }
    }

    fn rewrite_span(
        &mut self,
        start: usize,
        end: usize,
        replacement: Vec<EdgeEntry>,
    ) -> Option<(usize, usize)> {
        match self {
            Self::Contiguous(entries) => {
                if start > end || end > entries.len() {
                    return None;
                }
                let old_len = end.checked_sub(start)?;
                let new_len = replacement.len();
                entries.splice(start..end, replacement);
                Some((old_len, new_len))
            }
            Self::Segmented(backing) => backing.rewrite_global_span(start, end, replacement),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SegmentedBaseBacking {
    segments: BTreeMap<u32, Vec<EdgeEntry>>,
    flat_entries: Vec<EdgeEntry>,
}

impl SegmentedBaseBacking {
    fn new(segments: BTreeMap<u32, Vec<EdgeEntry>>) -> Self {
        let mut backing = Self {
            segments,
            flat_entries: Vec::new(),
        };
        backing.rebuild_flat_entries();
        backing
    }

    fn len(&self) -> usize {
        self.flat_entries.len()
    }

    fn rebuild_flat_entries(&mut self) {
        self.flat_entries.clear();
        for entries in self.segments.values() {
            self.flat_entries.extend_from_slice(entries);
        }
    }

    fn get_by_ref(&self, edge_ref: EdgeRef) -> Option<EdgeEntry> {
        let index = usize::try_from(edge_ref.start_slot()).ok()?;
        self.segments
            .get(&edge_ref.segment_id())?
            .get(index)
            .copied()
    }

    fn get_slice_by_ref(&self, edge_ref: EdgeRef, count: usize) -> Option<&[EdgeEntry]> {
        let start = usize::try_from(edge_ref.start_slot()).ok()?;
        self.segments
            .get(&edge_ref.segment_id())?
            .get(start..start.checked_add(count)?)
    }

    fn global_index_for_ref(&self, edge_ref: EdgeRef) -> Option<usize> {
        let local_index = usize::try_from(edge_ref.start_slot()).ok()?;
        if self.segments.len() == 1 {
            let (&only_id, entries) = self.segments.iter().next()?;
            return (only_id == edge_ref.segment_id() && local_index <= entries.len())
                .then_some(local_index);
        }
        let mut global_index = 0usize;
        for (&segment_id, entries) in &self.segments {
            if segment_id == edge_ref.segment_id() {
                return (local_index <= entries.len())
                    .then_some(global_index.checked_add(local_index))
                    .flatten();
            }
            global_index = global_index.checked_add(entries.len())?;
        }
        None
    }

    fn locate_global_index(&self, index: usize) -> Option<(u32, usize)> {
        if self.segments.len() == 1 {
            let (&segment_id, entries) = self.segments.iter().next()?;
            return (index < entries.len()).then_some((segment_id, index));
        }
        let mut remaining = index;
        for (&segment_id, entries) in &self.segments {
            if remaining < entries.len() {
                return Some((segment_id, remaining));
            }
            remaining = remaining.checked_sub(entries.len())?;
        }
        None
    }

    fn replace_global(&mut self, index: usize, entry: EdgeEntry) -> Option<EdgeEntry> {
        let (segment_id, local_index) = self.locate_global_index(index)?;
        let old = *self.segments.get(&segment_id)?.get(local_index)?;
        self.segments.get_mut(&segment_id)?[local_index] = entry;
        self.flat_entries[index] = entry;
        Some(old)
    }

    fn replace_by_ref(&mut self, edge_ref: EdgeRef, entry: EdgeEntry) -> Option<EdgeEntry> {
        let local_index = usize::try_from(edge_ref.start_slot()).ok()?;
        let entries = self.segments.get_mut(&edge_ref.segment_id())?;
        let old = *entries.get(local_index)?;
        entries[local_index] = entry;
        self.rebuild_flat_entries();
        Some(old)
    }

    fn rewrite_global_span(
        &mut self,
        start: usize,
        end: usize,
        replacement: Vec<EdgeEntry>,
    ) -> Option<(usize, usize)> {
        let (segment_id, local_start) = self.locate_global_index(start)?;
        let (end_segment_id, local_end) = if end == self.len() {
            let (&last_segment_id, last_entries) = self.segments.iter().next_back()?;
            if start > end || end > self.len() {
                return None;
            }
            if end_segment_id_mismatch_for_tail(
                last_segment_id,
                segment_id,
                local_start,
                end,
                self.len(),
            ) {
                return None;
            }
            (last_segment_id, last_entries.len())
        } else {
            self.locate_global_index(end)?
        };
        if segment_id != end_segment_id {
            return None;
        }
        let entries = self.segments.get_mut(&segment_id)?;
        if local_start > local_end || local_end > entries.len() {
            return None;
        }
        let old_len = local_end.checked_sub(local_start)?;
        let new_len = replacement.len();
        entries.splice(local_start..local_end, replacement);
        self.rebuild_flat_entries();
        Some((old_len, new_len))
    }

    fn push_contiguous_to_segment_zero(&mut self, entry: EdgeEntry) {
        self.segments.entry(0).or_default().push(entry);
        self.flat_entries.push(entry);
    }
}

fn end_segment_id_mismatch_for_tail(
    last_segment_id: u32,
    start_segment_id: u32,
    _local_start: usize,
    _end: usize,
    _total_len: usize,
) -> bool {
    last_segment_id != start_segment_id
}

/// One resolved base-entry slot inside a surface-local backing store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceBaseSlot {
    edge_ref: EdgeRef,
    index: usize,
}

/// One contiguous base-entry span inside a surface-local backing store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceBaseSpan {
    start: SurfaceBaseSlot,
    end_exclusive: usize,
}

/// Segment-capacity metadata for one surface's base adjacency backing.
///
/// Segment `0` is the root segment for flattened edge-entry storage. Its capacity should track
/// the current backing length unless a larger manager-derived logical length is
/// known.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SurfaceBaseSegmentLayout {
    slot_capacities: BTreeMap<u32, u64>,
}

impl SurfaceBaseSegmentLayout {
    fn slot_capacity(&self, segment_id: u32) -> Option<u64> {
        self.slot_capacities.get(&segment_id).copied()
    }

    fn set_slot_capacity(&mut self, segment_id: u32, slot_capacity: u64) {
        self.slot_capacities.insert(segment_id, slot_capacity);
    }

    /// Updates segment **0** logical slot capacity to match contiguous tail length after in-memory
    /// pushes/splices when no `RegionManager` header exists (pure runtime / tests).
    ///
    /// Hydrated or manager-backed graphs should treat
    /// [`SurfaceBaseStorage::sync_segment_slot_capacity_from_manager`] (and related header sync) as
    /// authoritative; this helper only patches segment `0` when that metadata is absent.
    /// Call sites (segment `0` tracks **total** flattened backing length for these paths):
    /// - [`SurfaceBaseStorage::push`] — contiguous grow **or** segmented append on segment `0` only.
    /// - [`SurfaceBaseStorage::rewrite_span`] (crate-private) — after any single-segment splice.
    /// - [`From<Vec<EdgeEntry>>`] for `SurfaceBaseStorage` — **contiguous-only** construction.
    ///
    /// Shrinking or bypassing these calls requires listing invariants first; do not remove ad hoc.
    fn sync_segment_zero_slot_capacity_from_storage_len(&mut self, storage_len: usize) {
        self.slot_capacities.insert(0, storage_len as u64);
    }

    fn sync_slot_capacity_from_manager(&mut self, segment_id: u32, slot_capacity: u64) {
        let effective = if segment_id == 0 {
            self.slot_capacity(0)
                .unwrap_or(slot_capacity)
                .max(slot_capacity)
        } else {
            slot_capacity
        };
        self.set_slot_capacity(segment_id, effective);
    }
}

impl SurfaceBaseStorage {
    pub fn from_contiguous(entries: Vec<EdgeEntry>) -> Self {
        entries.into()
    }

    pub fn from_segmented(segments: BTreeMap<u32, Vec<EdgeEntry>>) -> Self {
        let mut segment_layout = SurfaceBaseSegmentLayout::default();
        for (&segment_id, entries) in &segments {
            segment_layout.set_slot_capacity(segment_id, entries.len() as u64);
        }
        Self {
            backing: SurfaceBaseBacking::Segmented(SegmentedBaseBacking::new(segments)),
            segment_layout,
        }
    }

    pub fn from_segmented_with_slot_capacities(
        segments: BTreeMap<u32, Vec<EdgeEntry>>,
        slot_capacities: BTreeMap<u32, u64>,
    ) -> Self {
        let mut storage = Self::from_segmented(segments);
        for (segment_id, slot_capacity) in slot_capacities {
            storage.set_segment_slot_capacity(segment_id, slot_capacity);
        }
        storage
    }

    pub fn segment_entries(&self) -> BTreeMap<u32, &[EdgeEntry]> {
        match &self.backing {
            SurfaceBaseBacking::Contiguous(entries) => {
                let mut by_segment = BTreeMap::new();
                by_segment.insert(0, entries.as_slice());
                by_segment
            }
            SurfaceBaseBacking::Segmented(backing) => backing
                .segments
                .iter()
                .map(|(&segment_id, entries)| (segment_id, entries.as_slice()))
                .collect(),
        }
    }

    pub fn owned_segment_entries(&self) -> BTreeMap<u32, Vec<EdgeEntry>> {
        self.segment_entries()
            .into_iter()
            .map(|(segment_id, entries)| (segment_id, entries.to_vec()))
            .collect()
    }

    pub fn owned_segment_slot_capacities(&self) -> BTreeMap<u32, u64> {
        self.segment_layout.slot_capacities.clone()
    }

    /// Visits each segment's live edge-entry slice for stable-memory writeback without building a
    /// `BTreeMap` on the contiguous hot path.
    pub(crate) fn foreach_segment_entry_slices<E>(
        &self,
        mut f: impl FnMut(u32, &[EdgeEntry]) -> Result<(), E>,
    ) -> Result<(), E> {
        match &self.backing {
            SurfaceBaseBacking::Contiguous(entries) => f(0, entries.as_slice()),
            SurfaceBaseBacking::Segmented(backing) => {
                for (&segment_id, entries) in &backing.segments {
                    f(segment_id, entries.as_slice())?;
                }
                Ok(())
            }
        }
    }

    pub fn is_segmented(&self) -> bool {
        matches!(self.backing, SurfaceBaseBacking::Segmented(_))
    }

    fn slot_for_ref(&self, edge_ref: EdgeRef) -> Option<SurfaceBaseSlot> {
        Some(SurfaceBaseSlot {
            edge_ref,
            index: self.backing.global_index_for_ref(edge_ref)?,
        })
    }

    pub fn len(&self) -> usize {
        self.backing.len()
    }

    /// Iterates base entries in **flattened logical order** (matches [`Deref`] slice order).
    pub fn iter(&self) -> std::slice::Iter<'_, EdgeEntry> {
        self.backing.as_slice().iter()
    }

    pub fn get(&self, index: usize) -> Option<&EdgeEntry> {
        self.backing.get(index)
    }

    pub fn get_slice(&self, start: usize, end: usize) -> Option<&[EdgeEntry]> {
        self.backing.get_slice(start, end)
    }

    pub fn segment_slot_capacity(&self, segment_id: u32) -> Option<u64> {
        self.segment_layout.slot_capacity(segment_id)
    }

    pub fn set_segment_slot_capacity(&mut self, segment_id: u32, slot_capacity: u64) {
        self.segment_layout
            .set_slot_capacity(segment_id, slot_capacity);
    }

    pub fn sync_segment_slot_capacity_from_manager(&mut self, segment_id: u32, slot_capacity: u64) {
        self.segment_layout
            .sync_slot_capacity_from_manager(segment_id, slot_capacity);
    }

    pub fn sync_segment_slot_capacities_from_manager(
        &mut self,
        headers: impl IntoIterator<Item = (u32, u64)>,
    ) {
        for (segment_id, slot_capacity) in headers {
            self.sync_segment_slot_capacity_from_manager(segment_id, slot_capacity);
        }
    }

    pub fn end_edge_ref(&self) -> Option<EdgeRef> {
        Some(EdgeRef::new(0, u64::try_from(self.len()).ok()?))
    }

    pub fn get_by_ref(&self, edge_ref: EdgeRef) -> Option<EdgeEntry> {
        self.backing.get_by_ref(edge_ref)
    }

    pub fn get_slice_by_ref(&self, edge_ref: EdgeRef, count: usize) -> Option<&[EdgeEntry]> {
        self.backing.get_slice_by_ref(edge_ref, count)
    }

    fn span_for_ref_count(&self, edge_ref: EdgeRef, count: usize) -> Option<SurfaceBaseSpan> {
        let start = self.slot_for_ref(edge_ref)?;
        let (_, end_exclusive) = self.backing.global_span_for_ref_count(edge_ref, count)?;
        Some(SurfaceBaseSpan {
            start,
            end_exclusive,
        })
    }

    pub fn edge_ref_for_vertex_logical_index(
        &self,
        current: VertexEntry,
        logical_index: usize,
    ) -> Option<EdgeRef> {
        let degree = usize::try_from(current.degree).ok()?;
        if logical_index >= degree {
            return None;
        }
        let start_slot = current
            .start_slot()
            .checked_add(u64::try_from(logical_index).ok()?)?;
        Some(current.edge_ref().with_start_slot(start_slot))
    }

    pub fn logical_index_for_edge_ref(
        &self,
        current: VertexEntry,
        edge_ref: EdgeRef,
    ) -> Option<usize> {
        if current.segment_id() != edge_ref.segment_id() {
            return None;
        }
        let degree = usize::try_from(current.degree).ok()?;
        let offset = edge_ref.start_slot().checked_sub(current.start_slot())?;
        let logical_index = usize::try_from(offset).ok()?;
        (logical_index < degree).then_some(logical_index)
    }

    pub fn live_slot_for_vertex_logical_index(
        &self,
        current: VertexEntry,
        logical_index: usize,
    ) -> Option<usize> {
        Some(
            self.slot_for_ref(self.edge_ref_for_vertex_logical_index(current, logical_index)?)?
                .index,
        )
    }

    pub fn capacity_end_exclusive_for_vertex(
        &self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
    ) -> Option<usize> {
        let start = self.slot_for_ref(current.edge_ref())?.index;
        let span_len =
            usize::try_from(current.reserved_span_len(next, segment_slot_capacity)?).ok()?;
        start.checked_add(span_len)
    }

    fn live_span_for_vertex(&self, current: VertexEntry) -> Option<SurfaceBaseSpan> {
        self.span_for_ref_count(current.edge_ref(), usize::try_from(current.degree).ok()?)
    }

    fn reserved_span_for_vertex(
        &self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
    ) -> Option<SurfaceBaseSpan> {
        let start = self.slot_for_ref(current.edge_ref())?;
        let end_exclusive =
            self.capacity_end_exclusive_for_vertex(current, next, segment_slot_capacity)?;
        Some(SurfaceBaseSpan {
            start,
            end_exclusive,
        })
    }

    pub fn live_slice_for_vertex(&self, current: VertexEntry) -> Option<&[EdgeEntry]> {
        let span = self.live_span_for_vertex(current)?;
        self.get_slice(span.start.index, span.end_exclusive)
    }

    pub fn live_entry_for_vertex(
        &self,
        current: VertexEntry,
        logical_index: usize,
    ) -> Option<EdgeEntry> {
        self.get(self.live_slot_for_vertex_logical_index(current, logical_index)?)
            .copied()
    }

    pub fn reserved_slice_for_vertex(
        &self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
    ) -> Option<&[EdgeEntry]> {
        let span = self.reserved_span_for_vertex(current, next, segment_slot_capacity)?;
        self.get_slice(span.start.index, span.end_exclusive)
    }

    pub fn reserved_slot_for_vertex_logical_index(
        &self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
        logical_index: usize,
    ) -> Option<usize> {
        let end = self.capacity_end_exclusive_for_vertex(current, next, segment_slot_capacity)?;
        let edge_ref = current.edge_ref().with_start_slot(
            current
                .start_slot()
                .checked_add(u64::try_from(logical_index).ok()?)?,
        );
        let slot = self.slot_for_ref(edge_ref)?.index;
        (slot < end).then_some(slot)
    }

    pub fn append_slot_for_vertex(&self, current: VertexEntry) -> Option<usize> {
        let live_span = self.live_span_for_vertex(current)?;
        (live_span.end_exclusive == self.len()).then_some(live_span.end_exclusive)
    }

    pub fn append_slot_for_vertex_with(
        &self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
    ) -> Option<usize> {
        let live_span = self.live_span_for_vertex(current)?;
        let reserved_span = self.reserved_span_for_vertex(current, next, segment_slot_capacity)?;
        (live_span.end_exclusive == reserved_span.end_exclusive
            && reserved_span.end_exclusive == self.len())
        .then_some(live_span.end_exclusive)
    }

    pub fn window_span_for_vertices(
        &self,
        first: VertexEntry,
        last: VertexEntry,
        after_last: Option<VertexEntry>,
        segment_slot_capacity: u64,
    ) -> Option<(usize, usize)> {
        if first.segment_id() != last.segment_id() {
            return None;
        }
        if let Some(after_last) = after_last
            && after_last.segment_id() != first.segment_id() && after_last.start_slot() != 0 {
                return None;
            }
        let start = self.slot_for_ref(first.edge_ref())?;
        let end_exclusive =
            self.capacity_end_exclusive_for_vertex(last, after_last, segment_slot_capacity)?;
        (start.index <= end_exclusive && end_exclusive <= self.len())
            .then_some((start.index, end_exclusive))
    }

    pub fn push(&mut self, entry: EdgeEntry) {
        self.backing.push(entry);
        // Contiguous: length is segment 0. Segmented: only `push_contiguous_to_segment_zero` is used here.
        self.segment_layout
            .sync_segment_zero_slot_capacity_from_storage_len(self.backing.len());
    }

    pub fn replace(&mut self, index: usize, entry: EdgeEntry) -> Option<EdgeEntry> {
        self.backing.replace(index, entry)
    }

    pub fn replace_by_ref(&mut self, edge_ref: EdgeRef, entry: EdgeEntry) -> Option<EdgeEntry> {
        self.backing.replace_by_ref(edge_ref, entry)
    }

    pub fn replace_live_entry_for_vertex(
        &mut self,
        current: VertexEntry,
        logical_index: usize,
        entry: EdgeEntry,
    ) -> Option<EdgeEntry> {
        self.replace(
            self.live_slot_for_vertex_logical_index(current, logical_index)?,
            entry,
        )
    }

    pub fn replace_reserved_entry_for_vertex(
        &mut self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
        logical_index: usize,
        entry: EdgeEntry,
    ) -> Option<EdgeEntry> {
        self.replace(
            self.reserved_slot_for_vertex_logical_index(
                current,
                next,
                segment_slot_capacity,
                logical_index,
            )?,
            entry,
        )
    }

    pub fn append_entry_for_vertex(
        &mut self,
        current: VertexEntry,
        entry: EdgeEntry,
    ) -> Option<usize> {
        let slot = self.append_slot_for_vertex(current)?;
        self.push(entry);
        self.set_segment_slot_capacity(
            current.segment_id(),
            u64::try_from(slot.checked_add(1)?).ok()?,
        );
        Some(slot)
    }

    pub fn append_entry_for_vertex_with(
        &mut self,
        current: VertexEntry,
        next: Option<VertexEntry>,
        segment_slot_capacity: u64,
        entry: EdgeEntry,
    ) -> Option<usize> {
        let slot = self.append_slot_for_vertex_with(current, next, segment_slot_capacity)?;
        self.push(entry);
        self.set_segment_slot_capacity(
            current.segment_id(),
            u64::try_from(slot.checked_add(1)?).ok()?,
        );
        Some(slot)
    }

    /// Rewrites entries in **[global flattened order]** over all segments (`0..len()`).
    ///
    /// Prefer [`Self::rewrite_vertex_window_span`] or [`Self::rewrite_span_by_ref`] so callers
    /// stay anchored to [`EdgeRef`] / vertex windows instead of raw global indices. Segmented
    /// backing rejects spans that are not confined to a single segment's local storage.
    pub(crate) fn rewrite_span(
        &mut self,
        start: usize,
        end: usize,
        replacement: Vec<EdgeEntry>,
    ) -> Option<(usize, usize)> {
        let (old_len, new_len) = self.backing.rewrite_span(start, end, replacement)?;
        // Same as `push`: no manager header → keep segment 0 capacity aligned with flat len.
        self.segment_layout
            .sync_segment_zero_slot_capacity_from_storage_len(self.backing.len());
        Some((old_len, new_len))
    }

    /// Rewrites `current_len` entries starting at `edge_ref` (same segment). Prefer this for
    /// ad-hoc spans when you already have an anchor [`EdgeRef`]. For a full **vertex window**
    /// (reserved span across multiple vertices), use [`Self::rewrite_vertex_window_span`].
    pub fn rewrite_span_by_ref(
        &mut self,
        edge_ref: EdgeRef,
        current_len: usize,
        replacement: Vec<EdgeEntry>,
    ) -> Option<(usize, usize)> {
        let span = self.span_for_ref_count(edge_ref, current_len)?;
        self.rewrite_span(span.start.index, span.end_exclusive, replacement)
    }

    /// Replaces the **reserved base span** covering vertices `first..=last` (Ordinal order)
    /// within a **single segment**. The splice is implemented via [`Self::rewrite_span`] on the
    /// derived global index range; segmented backing enforces that the range does not cross
    /// segment boundaries.
    pub fn rewrite_vertex_window_span(
        &mut self,
        first: VertexEntry,
        last: VertexEntry,
        after_last: Option<VertexEntry>,
        segment_slot_capacity: u64,
        replacement: Vec<EdgeEntry>,
    ) -> Option<(usize, usize)> {
        let (start, end) =
            self.window_span_for_vertices(first, last, after_last, segment_slot_capacity)?;
        let (old_len, new_len) = self.rewrite_span(start, end, replacement)?;
        let shift = i64::try_from(new_len).ok()? - i64::try_from(old_len).ok()?;
        let updated_capacity = if shift >= 0 {
            segment_slot_capacity.checked_add(u64::try_from(shift).ok()?)?
        } else {
            segment_slot_capacity.checked_sub(shift.unsigned_abs())?
        };
        self.set_segment_slot_capacity(first.segment_id(), updated_capacity);
        Some((old_len, new_len))
    }
}

impl From<Vec<EdgeEntry>> for SurfaceBaseStorage {
    fn from(value: Vec<EdgeEntry>) -> Self {
        let mut segment_layout = SurfaceBaseSegmentLayout::default();
        // Contiguous bootstrap only (`SurfaceBaseBacking::Contiguous`).
        segment_layout.sync_segment_zero_slot_capacity_from_storage_len(value.len());
        Self {
            backing: SurfaceBaseBacking::Contiguous(value),
            segment_layout,
        }
    }
}

impl SurfaceBaseStorage {
    #[cfg(test)]
    fn from_segmented_for_tests(segments: BTreeMap<u32, Vec<EdgeEntry>>) -> Self {
        Self::from_segmented(segments)
    }
}

impl Deref for SurfaceBaseStorage {
    type Target = [EdgeEntry];

    fn deref(&self) -> &Self::Target {
        self.backing.as_slice()
    }
}

impl DerefMut for SurfaceBaseStorage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.backing.as_mut_slice()
    }
}

/// Region-level dirty tracking for one surface runtime.
///
/// This is the bridge between in-memory mutation and stable-memory writeback.
/// Vertex-local dirtiness drives label-sidecar maintenance, while these flags
/// say which concrete regions need serialization.
///
/// **Suffix hints** (`vertex_table_suffix_start`, `label_index_append_from`) narrow
/// encode/write work when stable prefixes are still valid. `None` means the whole
/// region must be serialized from scratch for correctness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceDirtyRegions {
    pub vertex_table: bool,
    /// When set with [`Self::vertex_table`], rows `0..start` are guaranteed to match
    /// stable; only `vertices[start..]` need encoding for the next extent tail write.
    pub vertex_table_suffix_start: Option<u32>,
    pub edge_entries: bool,
    pub label_index: bool,
    /// When set with [`Self::label_index`], tail-append preconditions hold from this
    /// index-entry ordinal (must equal the prior stable index count at flush time).
    pub label_index_append_from: Option<u32>,
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
    /// Returns the segment id that backs this rewritten base span.
    pub const fn segment_id(&self) -> u32 {
        self.base_start.segment_id()
    }

    /// Returns the first slot inside the backing segment.
    pub const fn base_start_slot(&self) -> u64 {
        self.base_start.start_slot()
    }

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

    /// Returns a copy retargeted to a different backing segment.
    pub fn retargeted_to_segment(&self, segment_id: u32) -> Self {
        let mut rewritten_vertices = self.rewritten_vertices.clone();
        for vertex in &mut rewritten_vertices {
            vertex.edge_index = EdgeIndex::from(vertex.edge_ref().with_segment_id(segment_id));
        }
        let base_start = EdgeIndex::from(self.base_start.as_edge_ref().with_segment_id(segment_id));
        Self {
            start_ordinal: self.start_ordinal,
            end_ordinal_exclusive: self.end_ordinal_exclusive,
            base_start,
            compacted_base_entries: self.compacted_base_entries.clone(),
            rewritten_vertices,
            reserved_base_len: self.reserved_base_len,
        }
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
    /// Returns the segment id that backs this weighted window.
    pub const fn segment_id(&self) -> u32 {
        self.base_start.segment_id()
    }

    /// Returns the first slot inside the backing segment.
    pub const fn base_start_slot(&self) -> u64 {
        self.base_start.start_slot()
    }

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

/// Pure summary derived only from vertex-table entries in one ordinal window.
///
/// This is cheaper than [`SurfaceWindowSlackSummary`] because it does not need
/// base-entry or overflow traversal; it only inspects `VertexEntry` records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceVertexWindowSummary {
    pub start_ordinal: usize,
    pub end_ordinal_exclusive: usize,
    pub base_start: EdgeIndex,
    pub live_end_exclusive: EdgeIndex,
    pub total_live_degree: u32,
    pub max_live_degree: u32,
    pub vertices_with_overflow: usize,
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
    pub fn set_base_segment_slot_capacity(&mut self, segment_id: u32, slot_capacity: u64) {
        self.base_entries
            .set_segment_slot_capacity(segment_id, slot_capacity);
    }

    pub fn sync_base_segment_slot_capacity_from_manager(
        &mut self,
        segment_id: u32,
        slot_capacity: u64,
    ) {
        self.base_entries
            .sync_segment_slot_capacity_from_manager(segment_id, slot_capacity);
    }

    pub fn base_segment_slot_capacity(&self, segment_id: u32) -> Option<u64> {
        self.base_entries.segment_slot_capacity(segment_id)
    }

    /// Derives the reserved base-span length for one vertex ordinal under the
    /// current single-segment base-entry layout.
    pub fn vertex_reserved_span_len(&self, ordinal: usize) -> Option<u64> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        current.reserved_span_len(
            next,
            self.base_entries
                .segment_slot_capacity(current.segment_id())?,
        )
    }

    /// Derives the reserved base-span length for one vertex ordinal using a
    /// caller-provided segment-capacity resolver.
    pub fn vertex_reserved_span_len_with(
        &self,
        ordinal: usize,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<u64> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let capacity = segment_slot_capacity(current.segment_id())?;
        current.reserved_span_len(next, capacity)
    }

    /// Summarizes one vertex-table window using only `VertexEntry` records.
    pub fn summarize_vertex_window(
        &self,
        start_ordinal: usize,
        end_ordinal_exclusive: usize,
    ) -> Option<SurfaceVertexWindowSummary> {
        let entries = self.vertices.get(start_ordinal..end_ordinal_exclusive)?;
        summarize_vertex_window_entries(start_ordinal, entries)
    }

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
        let segment_id = base_start.segment_id();
        let base_start_slot = base_start.start_slot();

        let mut positions = Vec::with_capacity(live_degrees.len());
        let mut index_d = base_start_slot as f64;
        let mut previous_end = base_start_slot;
        for (offset, &degree) in live_degrees.iter().enumerate() {
            let raw_pos = if offset == 0 {
                base_start_slot
            } else {
                (index_d as u64).max(previous_end)
            };
            positions.push(EdgeIndex::from(super::ids::EdgeRef::new(
                segment_id, raw_pos,
            )));
            previous_end = raw_pos + u64::from(degree);
            index_d += f64::from(degree) + (step * f64::from(degree + 1));
        }

        let mut reserved_lengths = Vec::with_capacity(live_degrees.len());
        for offset in 0..live_degrees.len() {
            let start = positions[offset].start_slot();
            let next_start = positions
                .get(offset + 1)
                .map(|pos| pos.start_slot())
                .unwrap_or(base_start_slot + u64::from(reserved_base_len));
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
        let segment_id = base_start.segment_id();
        for ordinal in start_ordinal..end_ordinal_exclusive {
            if self.vertex_entry(ordinal)?.segment_id() != segment_id {
                return None;
            }
        }
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
            base_entries: base_entries.into(),
            overflow_entries,
            label_index_entries,
            label_ranges,
            dirty_regions: SurfaceDirtyRegions::default(),
            dirty_vertices: BTreeSet::new(),
        }
    }

    /// Like [`Self::new`], but accepts an already-built [`SurfaceBaseStorage`] for base adjacency.
    pub fn from_decoded_regions(
        layout: SurfaceLayout,
        vertices: Vec<VertexEntry>,
        base_entries: SurfaceBaseStorage,
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

    pub fn set_base_storage(&mut self, storage: SurfaceBaseStorage) {
        self.base_entries = storage;
        self.dirty_regions.edge_entries = true;
    }

    pub fn replace_base_storage_with_segmented(
        &mut self,
        segments: BTreeMap<u32, Vec<EdgeEntry>>,
        slot_capacities: BTreeMap<u32, u64>,
    ) {
        self.set_base_storage(SurfaceBaseStorage::from_segmented_with_slot_capacities(
            segments,
            slot_capacities,
        ));
    }

    /// If base adjacency uses contiguous backing, re-home the same live slots into segment `0` segmented storage.
    ///
    /// No-op if already segmented. Valid when every live [`EdgeRef`] uses segment `0`.
    pub fn migrate_contiguous_base_to_segment_zero(&mut self) {
        if self.base_entries.is_segmented() {
            return;
        }
        let len = self.base_entries.len();
        let slot_cap = self
            .base_entries
            .segment_slot_capacity(0)
            .unwrap_or(len as u64);
        let entries: Vec<EdgeEntry> = SurfaceBaseStorage::iter(&self.base_entries)
            .copied()
            .collect();
        self.set_base_storage(SurfaceBaseStorage::from_segmented_with_slot_capacities(
            BTreeMap::from([(0, entries)]),
            BTreeMap::from([(0, slot_cap)]),
        ));
    }

    pub(crate) fn materialize_segmented_base_storage_parts(
        &self,
    ) -> Option<(BTreeMap<u32, Vec<EdgeEntry>>, BTreeMap<u32, u64>)> {
        if self.base_entries.is_segmented() {
            return Some((
                self.base_entries.owned_segment_entries(),
                self.base_entries.owned_segment_slot_capacities(),
            ));
        }

        if self.vertices.is_empty() {
            return Some((
                BTreeMap::from([(
                    0,
                    SurfaceBaseStorage::iter(&self.base_entries)
                        .copied()
                        .collect(),
                )]),
                BTreeMap::from([(0, self.base_entries.segment_slot_capacity(0).unwrap_or(0))]),
            ));
        }

        let mut segments = BTreeMap::new();
        let mut slot_capacities = BTreeMap::new();
        let mut ordinal = 0usize;
        while ordinal < self.vertices.len() {
            let segment_id = self.vertex_entry(ordinal)?.segment_id();
            let end_ordinal_exclusive = self.segment_end_ordinal_exclusive(ordinal)?;
            let first = self.vertex_entry(ordinal)?;
            let last = self.vertex_entry(end_ordinal_exclusive.checked_sub(1)?)?;
            let after_last = self.vertex_entry(end_ordinal_exclusive);
            let slot_capacity = self.base_entries.segment_slot_capacity(segment_id)?;
            let (start, end) = self.base_entries.window_span_for_vertices(
                first,
                last,
                after_last,
                slot_capacity,
            )?;
            segments.insert(
                segment_id,
                self.base_entries.get_slice(start, end)?.to_vec(),
            );
            slot_capacities.insert(segment_id, slot_capacity);
            ordinal = end_ordinal_exclusive;
        }

        Some((segments, slot_capacities))
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
        let edge_index = EdgeIndex::from(self.base_entries.end_edge_ref()?);
        let old_label_index_len = self.label_index_entries.len();
        self.vertices
            .push(VertexEntry::new(edge_index, 0, EMPTY_LOG_OFFSET));
        self.label_index_entries.push(VertexLabelIndexEntry::new(
            u32::try_from(self.label_ranges.len()).ok()?,
            0,
        ));
        self.note_vertex_table_row_changed(ordinal);
        self.note_label_index_tail_extension(old_label_index_len);
        Some(ordinal)
    }

    fn note_vertex_table_row_changed(&mut self, ordinal: usize) {
        let ord = u32::try_from(ordinal).unwrap_or(u32::MAX);
        if !self.dirty_regions.vertex_table {
            self.dirty_regions.vertex_table = true;
            self.dirty_regions.vertex_table_suffix_start = Some(ord);
            return;
        }
        match self.dirty_regions.vertex_table_suffix_start {
            None => {}
            Some(s) => self.dirty_regions.vertex_table_suffix_start = Some(s.min(ord)),
        }
    }

    fn note_label_index_tail_extension(&mut self, appended_index_entry_at: usize) {
        let ord = match u32::try_from(appended_index_entry_at) {
            Ok(v) => v,
            Err(_) => {
                self.dirty_regions.label_index = true;
                self.dirty_regions.label_index_append_from = None;
                return;
            }
        };
        if !self.dirty_regions.label_index {
            self.dirty_regions.label_index = true;
            self.dirty_regions.label_index_append_from = Some(ord);
            return;
        }
        match self.dirty_regions.label_index_append_from {
            None => {}
            Some(a) => self.dirty_regions.label_index_append_from = Some(a.min(ord)),
        }
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
        if offset.is_empty() {
            return None;
        }
        self.overflow_entries.get(offset.index()?).copied()
    }

    /// Builds the canonical base-neighborhood view for one vertex ordinal.
    pub fn base_neighborhood(&self, ordinal: usize) -> Option<BaseNeighborhood> {
        let vertex = self.vertex_entry(ordinal)?;
        Some(self.layout.base_neighborhood(vertex))
    }

    /// Materializes base entries for one vertex ordinal.
    pub fn base_entries_for(&self, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        let current = self.vertex_entry(ordinal)?;
        Some(self.base_entries.live_slice_for_vertex(current)?.to_vec())
    }

    /// Materializes the full reserved base span for one vertex ordinal.
    pub fn reserved_base_entries_for(&self, ordinal: usize) -> Option<Vec<EdgeEntry>> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        Some(
            self.base_entries
                .reserved_slice_for_vertex(
                    current,
                    next,
                    self.base_entries
                        .segment_slot_capacity(current.segment_id())?,
                )?
                .to_vec(),
        )
    }

    /// Materializes the full reserved base span for one vertex ordinal using a
    /// caller-provided segment-capacity resolver.
    pub fn reserved_base_entries_for_with(
        &self,
        ordinal: usize,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<Vec<EdgeEntry>> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let capacity = segment_slot_capacity(current.segment_id())?;
        Some(
            self.base_entries
                .reserved_slice_for_vertex(current, next, capacity)?
                .to_vec(),
        )
    }

    /// Returns the overflow-chain descriptor for one vertex-local neighborhood.
    pub fn overflow_chain(&self, vertex_ref: VertexRef, ordinal: usize) -> Option<OverflowChain> {
        let entry = self.vertex_entry(ordinal)?;
        let head = if entry.has_overflow() {
            LogOffset::new(entry.log_offset)
        } else {
            LogOffset::EMPTY
        };
        Some(OverflowChain::new(self.layout.kind, vertex_ref, head))
    }

    /// Follows and materializes the overflow chain for one vertex-local neighborhood.
    pub fn overflow_entries_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<Vec<OverflowEntry>> {
        let _ = vertex_ref;
        self.overflow_entries_for_ordinal(ordinal)
    }

    /// Follows and materializes the overflow chain for one vertex ordinal.
    ///
    /// Unlike `overflow_entries_for`, this helper does not require the caller
    /// to provide the semantic vertex id because chain traversal depends only
    /// on the stored `log_offset`.
    pub fn overflow_entries_for_ordinal(&self, ordinal: usize) -> Option<Vec<OverflowEntry>> {
        let entry = self.vertex_entry(ordinal)?;
        let mut next = if entry.has_overflow() {
            LogOffset::new(entry.log_offset)
        } else {
            LogOffset::EMPTY
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
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<MergedNeighborhoodView> {
        let entry = self.vertex_entry(ordinal)?;
        let overflow = self.overflow_chain(vertex_ref, ordinal)?;
        Some(self.layout.merged_neighborhood(entry, overflow))
    }

    /// Materializes merged read order as base entries followed by overflow entries.
    pub fn merged_entries_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<Vec<EdgeEntry>> {
        let mut entries = self.base_entries_for(ordinal)?;
        let overflow = self.overflow_entries_for(vertex_ref, ordinal)?;
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
        let current = self.vertex_entry(ordinal)?;
        let slice = self.base_entries.live_slice_for_vertex(current)?;

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
            let global_start =
                u32::try_from(current.start_slot().checked_add(local_index as u64)?).ok()?;

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
        self.dirty_regions.label_index_append_from = None;
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
        self.dirty_regions.label_index_append_from = None;
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
        let current = self.vertex_entry(ordinal)?;
        let old =
            self.base_entries
                .replace_live_entry_for_vertex(current, logical_index, new_entry)?;
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
        let current = self.vertex_entry(ordinal)?;
        let old = self
            .base_entries
            .live_entry_for_vertex(current, logical_index)?;
        let old = self.base_entries.replace_live_entry_for_vertex(
            current,
            logical_index,
            EdgeEntry::new(old.target, old.meta.with_tombstone(true)),
        )?;
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(old)
    }

    /// Appends one entry to the overflow log and updates the vertex-local log head.
    pub fn append_overflow_entry(
        &mut self,
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
        entry: EdgeEntry,
    ) -> Option<LogOffset> {
        let head = self.overflow_chain(vertex_ref, ordinal)?.head;
        let new_offset = LogOffset::new(i32::try_from(self.overflow_entries.len()).ok()?);
        self.overflow_entries
            .push(OverflowEntry::new(edge_id, entry, head));
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        *vertex_entry = vertex_entry.with_overflow_head(new_offset.index().map(|v| v as u32));
        self.note_vertex_table_row_changed(ordinal);
        self.dirty_regions.segment_log = true;
        Some(new_offset)
    }

    /// Returns whether this vertex can append one more entry directly to the
    /// tail of its base interval without shifting later base entries.
    pub fn can_append_base_entry(&self, ordinal: usize) -> Option<bool> {
        let current = self.vertex_entry(ordinal)?;
        Some(self.base_entries.append_slot_for_vertex(current).is_some())
    }

    /// Returns whether this vertex can append one more entry directly to the
    /// tail of its base interval under a caller-provided segment-capacity resolver.
    pub fn can_append_base_entry_with(
        &self,
        ordinal: usize,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<bool> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        Some(
            self.base_entries
                .append_slot_for_vertex_with(
                    current,
                    next,
                    segment_slot_capacity(current.segment_id())?,
                )
                .is_some(),
        )
    }

    pub(crate) fn segment_end_ordinal_exclusive(&self, start_ordinal: usize) -> Option<usize> {
        let segment_id = self.vertex_entry(start_ordinal)?.segment_id();
        let mut ordinal = start_ordinal;
        while ordinal < self.vertices.len() {
            if self.vertex_entry(ordinal)?.segment_id() != segment_id {
                break;
            }
            ordinal += 1;
        }
        Some(ordinal)
    }

    pub(crate) fn segment_start_ordinal(&self, end_ordinal_in_segment: usize) -> Option<usize> {
        let segment_id = self.vertex_entry(end_ordinal_in_segment)?.segment_id();
        let mut ordinal = end_ordinal_in_segment;
        while ordinal > 0 {
            if self.vertex_entry(ordinal.checked_sub(1)?)?.segment_id() != segment_id {
                break;
            }
            ordinal -= 1;
        }
        Some(ordinal)
    }

    /// Returns whether the last slot in the canonical base interval is a
    /// tombstone that can be reused without shifting later base entries.
    pub fn reusable_tombstoned_tail_base_slot(&self, ordinal: usize) -> Option<usize> {
        let current = self.vertex_entry(ordinal)?;
        let degree = usize::try_from(current.degree).ok()?;
        if degree == 0 {
            return None;
        }
        self.base_entries
            .live_entry_for_vertex(current, degree.checked_sub(1)?)?
            .meta
            .is_tombstone()
            .then_some(degree - 1)
    }

    /// Returns whether the first reserved slot immediately after the current
    /// live degree can be reused as base capacity.
    pub fn reusable_reserved_tail_base_slot(&self, ordinal: usize) -> Option<usize> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let degree = usize::try_from(current.degree).ok()?;
        let slot = self.base_entries.reserved_slot_for_vertex_logical_index(
            current,
            next,
            self.base_entries
                .segment_slot_capacity(current.segment_id())?,
            degree,
        )?;
        self.base_entries
            .get(slot)
            .copied()?
            .meta
            .is_tombstone()
            .then_some(degree)
    }

    /// Returns whether the first reserved slot immediately after the current
    /// live degree can be reused as base capacity under a caller-provided
    /// segment-capacity resolver.
    pub fn reusable_reserved_tail_base_slot_with(
        &self,
        ordinal: usize,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<usize> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let degree = usize::try_from(current.degree).ok()?;
        let slot = self.base_entries.reserved_slot_for_vertex_logical_index(
            current,
            next,
            segment_slot_capacity(current.segment_id())?,
            degree,
        )?;
        self.base_entries
            .get(slot)
            .copied()?
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

    /// Chooses whether a base insert can append or reuse a tombstone using a
    /// caller-provided segment-capacity resolver.
    pub fn choose_base_insert_slot_with(
        &self,
        ordinal: usize,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<BaseInsertDecision> {
        if self
            .can_append_base_entry_with(ordinal, &mut segment_slot_capacity)?
        {
            let logical_index = usize::try_from(self.vertices.get(ordinal)?.degree).ok()?;
            return Some(BaseInsertDecision::Append { logical_index });
        }
        if let Some(logical_index) = self
            .reusable_reserved_tail_base_slot_with(ordinal, |segment_id| {
                segment_slot_capacity(segment_id)
            })
        {
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
        let current = self.vertex_entry(ordinal)?;
        let _slot = self.base_entries.append_entry_for_vertex(current, entry)?;
        let logical_index = usize::try_from(current.degree).ok()?;
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        vertex_entry.degree = vertex_entry.degree.checked_add(1)?;
        self.note_vertex_table_row_changed(ordinal);
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(logical_index)
    }

    /// Appends one entry directly to the tail of the canonical base interval
    /// using a caller-provided segment-capacity resolver.
    pub fn append_base_entry_with(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<usize> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let _slot = self.base_entries.append_entry_for_vertex_with(
            current,
            next,
            segment_slot_capacity(current.segment_id())?,
            entry,
        )?;
        let logical_index = usize::try_from(current.degree).ok()?;
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        vertex_entry.degree = vertex_entry.degree.checked_add(1)?;
        self.note_vertex_table_row_changed(ordinal);
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
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let slot = self.base_entries.reserved_slot_for_vertex_logical_index(
            current,
            next,
            self.base_entries
                .segment_slot_capacity(current.segment_id())?,
            logical_index,
        )?;
        if !self.base_entries.get(slot)?.meta.is_tombstone() {
            return None;
        }
        let _ = self.base_entries.replace_reserved_entry_for_vertex(
            current,
            next,
            self.base_entries
                .segment_slot_capacity(current.segment_id())?,
            logical_index,
            entry,
        )?;
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        if logical_index == usize::try_from(vertex_entry.degree).ok()? {
            vertex_entry.degree = vertex_entry.degree.checked_add(1)?;
            self.note_vertex_table_row_changed(ordinal);
        }
        self.dirty_regions.edge_entries = true;
        self.mark_vertex_dirty(ordinal)?;
        Some(logical_index)
    }

    /// Reuses one tombstoned slot within the current base-capacity span using
    /// a caller-provided segment-capacity resolver.
    pub fn reuse_tombstoned_base_entry_with(
        &mut self,
        ordinal: usize,
        logical_index: usize,
        entry: EdgeEntry,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<usize> {
        let current = self.vertex_entry(ordinal)?;
        let next = self.vertex_entry(ordinal + 1);
        let slot = self.base_entries.reserved_slot_for_vertex_logical_index(
            current,
            next,
            segment_slot_capacity(current.segment_id())?,
            logical_index,
        )?;
        if !self.base_entries.get(slot)?.meta.is_tombstone() {
            return None;
        }
        let _ = self.base_entries.replace_reserved_entry_for_vertex(
            current,
            next,
            segment_slot_capacity(current.segment_id())?,
            logical_index,
            entry,
        )?;
        let vertex_entry = self.vertices.get_mut(ordinal)?;
        if logical_index == usize::try_from(vertex_entry.degree).ok()? {
            vertex_entry.degree = vertex_entry.degree.checked_add(1)?;
            self.note_vertex_table_row_changed(ordinal);
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

    /// Inserts one entry into the canonical base interval without shifting
    /// later base entries, using a caller-provided segment-capacity resolver.
    pub fn insert_base_entry_with(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
        mut segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<EdgeInsertPath> {
        match self
            .choose_base_insert_slot_with(ordinal, &mut segment_slot_capacity)?
        {
            BaseInsertDecision::Append { .. } => {
                let inserted = self.append_base_entry_with(ordinal, entry, |segment_id| {
                    segment_slot_capacity(segment_id)
                })?;
                Some(EdgeInsertPath::BaseAppend {
                    logical_index: inserted,
                })
            }
            BaseInsertDecision::ReuseTombstone { .. } => {
                let inserted = self.reuse_tombstoned_base_entry_with(
                    ordinal,
                    match self.choose_base_insert_slot_with(ordinal, |segment_id| {
                        segment_slot_capacity(segment_id)
                    })? {
                        BaseInsertDecision::ReuseTombstone { logical_index } => logical_index,
                        BaseInsertDecision::Append { .. } => return None,
                    },
                    entry,
                    segment_slot_capacity,
                )?;
                Some(EdgeInsertPath::BaseReuseTombstone {
                    logical_index: inserted,
                })
            }
        }
    }

    /// Resolves the packed [`EdgeRef`] for one live base entry.
    pub fn base_edge_ref_for(
        &self,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<super::ids::EdgeRef> {
        let current = self.vertex_entry(ordinal)?;
        self.base_entries
            .edge_ref_for_vertex_logical_index(current, logical_index)
    }

    /// Resolves the logical base index for one packed [`EdgeRef`].
    pub fn logical_base_index_for_edge_ref(
        &self,
        ordinal: usize,
        edge_ref: super::ids::EdgeRef,
    ) -> Option<usize> {
        let current = self.vertex_entry(ordinal)?;
        self.base_entries
            .logical_index_for_edge_ref(current, edge_ref)
    }

    /// Materializes a contiguous base slice directly from a packed [`EdgeRef`].
    pub fn base_entries_by_ref(
        &self,
        edge_ref: super::ids::EdgeRef,
        count: usize,
    ) -> Option<Vec<EdgeEntry>> {
        Some(
            self.base_entries
                .get_slice_by_ref(edge_ref, count)?
                .to_vec(),
        )
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
        Some(self.base_entries.get_slice(start, end)?.to_vec())
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

    /// Resolves the logical locator for one logical position in merged read order.
    pub fn logical_edge_locator_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<LogicalEdgeLocator> {
        let base = self.base_neighborhood(ordinal)?;
        let base_degree = usize::try_from(base.degree).ok()?;
        if logical_index < base_degree {
            return Some(LogicalEdgeLocator::base(
                self.layout.kind,
                vertex_ref,
                u32::try_from(logical_index).ok()?,
            ));
        }
        Some(LogicalEdgeLocator::overflow(
            self.layout.kind,
            vertex_ref,
            u32::try_from(logical_index - base_degree).ok()?,
        ))
    }

    /// Classifies one logical locator as either a base slot or an overflow-log slot.
    pub fn resolve_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        if locator.surface_kind() != self.layout.kind || locator.vertex_ref != vertex_ref {
            return None;
        }
        let logical_index = usize::try_from(locator.logical_index).ok()?;
        if locator.is_overflow() {
            let overflow = self.overflow_entries_for(vertex_ref, ordinal)?;
            if logical_index >= overflow.len() {
                return None;
            }
            let head = self.overflow_chain(vertex_ref, ordinal)?.head;
            let offset = if logical_index == 0 {
                head
            } else {
                let mut next = head;
                let mut found = None;
                for _ in 0..=logical_index {
                    found = Some(next);
                    let entry = self.overflow_entry(next)?;
                    next = entry.next;
                }
                found?
            };
            return Some(ResolvedEdgeSlot::Overflow {
                overflow_index: logical_index,
                offset,
            });
        }

        let base = self.base_neighborhood(ordinal)?;
        let degree = usize::try_from(base.degree).ok()?;
        if logical_index >= degree {
            return None;
        }
        Some(ResolvedEdgeSlot::Base { logical_index })
    }

    /// Replaces one overflow entry identified by semantic edge id.
    pub fn replace_overflow_entry(
        &mut self,
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
        new_entry: EdgeEntry,
    ) -> Option<OverflowEntry> {
        let mut next = self.overflow_chain(vertex_ref, ordinal)?.head;
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
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
    ) -> Option<OverflowEntry> {
        let mut next = self.overflow_chain(vertex_ref, ordinal)?.head;
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

    /// Adds all known logical locators for one merged neighborhood into the provided sidecar.
    pub fn populate_logical_locator_sidecar_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        base_edge_ids: &[EdgeId],
        sidecar: &mut EdgeLogicalLocatorSidecar,
    ) -> Option<()> {
        let base = self.base_neighborhood(ordinal)?;
        let base_degree = usize::try_from(base.degree).ok()?;
        if base_edge_ids.len() != base_degree {
            return None;
        }

        for (logical_index, &edge_id) in base_edge_ids.iter().enumerate() {
            let locator = self.logical_edge_locator_for(vertex_ref, ordinal, logical_index)?;
            sidecar.set(edge_id, locator);
        }

        let overflow_entries = self.overflow_entries_for(vertex_ref, ordinal)?;
        for (overflow_index, entry) in overflow_entries.into_iter().enumerate() {
            let locator = LogicalEdgeLocator::overflow(
                self.layout.kind,
                vertex_ref,
                u32::try_from(overflow_index).ok()?,
            );
            sidecar.set(entry.edge_id, locator);
        }

        Some(())
    }

    /// Builds a fresh logical locator sidecar for one merged neighborhood.
    pub fn build_logical_locator_sidecar_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        base_edge_ids: &[EdgeId],
    ) -> Option<EdgeLogicalLocatorSidecar> {
        let mut sidecar = EdgeLogicalLocatorSidecar::new();
        self.populate_logical_locator_sidecar_for(
            vertex_ref,
            ordinal,
            base_edge_ids,
            &mut sidecar,
        )?;
        Some(sidecar)
    }

    /// Populates a logical locator sidecar for every vertex ordinal in this surface.
    pub fn populate_logical_locator_sidecar_from_vertex_base_ids(
        &self,
        vertex_ids: &[VertexRef],
        base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        sidecar: &mut EdgeLogicalLocatorSidecar,
    ) -> Option<()> {
        if vertex_ids.len() != self.vertices.len()
            || base_edge_ids_by_ordinal.len() != self.vertices.len()
        {
            return None;
        }

        for ordinal in 0..self.vertices.len() {
            self.populate_logical_locator_sidecar_for(
                vertex_ids[ordinal],
                ordinal,
                &base_edge_ids_by_ordinal[ordinal],
                sidecar,
            )?;
        }

        Some(())
    }

    /// Builds a fresh logical locator sidecar for the entire surface.
    pub fn build_logical_locator_sidecar_from_vertex_base_ids(
        &self,
        vertex_ids: &[VertexRef],
        base_edge_ids_by_ordinal: &[Vec<EdgeId>],
    ) -> Option<EdgeLogicalLocatorSidecar> {
        let mut sidecar = EdgeLogicalLocatorSidecar::new();
        self.populate_logical_locator_sidecar_from_vertex_base_ids(
            vertex_ids,
            base_edge_ids_by_ordinal,
            &mut sidecar,
        )?;
        Some(sidecar)
    }

    /// Populates logical locator entries only for one contiguous vertex window.
    pub fn populate_logical_locator_sidecar_for_window(
        &self,
        start_ordinal: usize,
        vertex_ids: &[VertexRef],
        base_edge_ids_by_ordinal: &[Vec<EdgeId>],
        sidecar: &mut EdgeLogicalLocatorSidecar,
    ) -> Option<()> {
        let end_ordinal_exclusive = start_ordinal.checked_add(vertex_ids.len())?;
        if end_ordinal_exclusive > self.vertices.len()
            || base_edge_ids_by_ordinal.len() != vertex_ids.len()
        {
            return None;
        }

        for (offset, vertex_ref) in vertex_ids.iter().copied().enumerate() {
            let ordinal = start_ordinal + offset;
            self.populate_logical_locator_sidecar_for(
                vertex_ref,
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
                    VertexRef::default(),
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
            let current = self.vertex_entry(ordinal)?;
            let next = self.vertex_entry(ordinal + 1);
            let capacity_entries = self.base_entries.reserved_slice_for_vertex(
                current,
                next,
                self.base_entries
                    .segment_slot_capacity(current.segment_id())?,
            )?;
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

    /// Applies one previously built local-rebalance delta to this surface runtime **in place**
    /// on the segments referenced by the current vertex window.
    ///
    /// This rewrites the affected **segment-local** contiguous base window via
    /// [`SurfaceBaseStorage::rewrite_vertex_window_span`], replaces the window's vertex entries,
    /// and shifts subsequent `edge_index` values when the new compacted slice has a different
    /// length than the old one. For maintenance that should **relocate** adjacency onto freshly
    /// allocated edge segments, use `GraphRuntime::apply_local_rebalance_delta_with_segment_replacement`
    /// instead.
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

        let last_ordinal = delta.end_ordinal_exclusive.checked_sub(1)?;
        let old_first = self.vertex_entry(delta.start_ordinal)?;
        let old_last = self.vertex_entry(last_ordinal)?;
        let after_last = self.vertex_entry(delta.end_ordinal_exclusive);
        let segment_slot_capacity = self
            .base_entries
            .segment_slot_capacity(old_first.segment_id())?;
        let _ = self.base_entries.window_span_for_vertices(
            old_first,
            old_last,
            after_last,
            segment_slot_capacity,
        )?;

        let mut previous_index = None;
        for vertex in &delta.rewritten_vertices {
            if vertex.has_overflow() {
                return None;
            }
            if vertex.segment_id() != delta.segment_id() {
                return None;
            }
            if let Some(previous) = previous_index {
                if vertex.start_slot() < previous {
                    return None;
                }
            } else if vertex.edge_index != delta.base_start {
                return None;
            }
            previous_index = Some(vertex.start_slot() + u64::from(vertex.degree));
        }
        if usize::try_from(delta.reserved_base_len).ok()? != delta.compacted_base_entries.len() {
            return None;
        }

        let (old_len, new_len) = self.base_entries.rewrite_vertex_window_span(
            old_first,
            old_last,
            after_last,
            segment_slot_capacity,
            delta.compacted_base_entries,
        )?;

        for (offset, vertex) in delta.rewritten_vertices.into_iter().enumerate() {
            self.vertices[delta.start_ordinal + offset] = vertex;
        }

        let shift = i64::try_from(new_len).ok()? - i64::try_from(old_len).ok()?;
        if shift != 0 {
            let segment_end = self.segment_end_ordinal_exclusive(delta.start_ordinal)?;
            for vertex in self
                .vertices
                .iter_mut()
                .take(segment_end)
                .skip(delta.end_ordinal_exclusive)
            {
                let shifted = i128::from(vertex.start_slot()) + i128::from(shift);
                if shifted < 0 || shifted > i128::from(u64::MAX) {
                    return None;
                }
                vertex.edge_index =
                    EdgeIndex::from(vertex.edge_ref().with_start_slot(shifted as u64));
            }
        }

        self.dirty_regions.vertex_table = true;
        self.dirty_regions.vertex_table_suffix_start = None;
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

pub(crate) fn summarize_vertex_window_entries(
    start_ordinal: usize,
    entries: &[VertexEntry],
) -> Option<SurfaceVertexWindowSummary> {
    let first = entries.first().copied()?;
    let last = entries.last().copied()?;
    let end_ordinal_exclusive = start_ordinal.checked_add(entries.len())?;
    let total_live_degree = entries
        .iter()
        .try_fold(0u32, |acc, entry| acc.checked_add(entry.degree))?;
    let max_live_degree = entries.iter().map(|entry| entry.degree).max()?;
    let vertices_with_overflow = entries.iter().filter(|entry| entry.has_overflow()).count();
    let live_end_exclusive = last.edge_index.checked_add(last.degree)?;

    Some(SurfaceVertexWindowSummary {
        start_ordinal,
        end_ordinal_exclusive,
        base_start: first.edge_index,
        live_end_exclusive,
        total_live_degree,
        max_live_degree,
        vertices_with_overflow,
    })
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

    pub fn set_base_segment_slot_capacity(&mut self, segment_id: u32, slot_capacity: u64) {
        self.0
            .set_base_segment_slot_capacity(segment_id, slot_capacity);
    }

    pub fn sync_base_segment_slot_capacity_from_manager(
        &mut self,
        segment_id: u32,
        slot_capacity: u64,
    ) {
        self.0
            .sync_base_segment_slot_capacity_from_manager(segment_id, slot_capacity);
    }

    /// Returns the canonical base-neighborhood view for one vertex ordinal.
    pub fn base_neighborhood(&self, ordinal: usize) -> Option<BaseNeighborhood> {
        self.0.base_neighborhood(ordinal)
    }

    /// Returns the merged read-side view for one vertex-local neighborhood.
    pub fn merged_neighborhood(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<MergedNeighborhoodView> {
        self.0.merged_neighborhood(vertex_ref, ordinal)
    }

    /// Materializes the overflow chain for one vertex-local neighborhood.
    pub fn overflow_entries_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<Vec<OverflowEntry>> {
        self.0.overflow_entries_for(vertex_ref, ordinal)
    }

    /// Materializes merged read order for one vertex-local neighborhood.
    pub fn merged_entries_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<Vec<EdgeEntry>> {
        self.0.merged_entries_for(vertex_ref, ordinal)
    }

    /// Resolves one physical locator inside the forward surface.
    pub fn logical_edge_locator_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<LogicalEdgeLocator> {
        self.0
            .logical_edge_locator_for(vertex_ref, ordinal, logical_index)
    }

    /// Resolves whether a locator points at a base slot or an overflow slot.
    pub fn resolve_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.0
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    /// Builds a locator sidecar for one forward vertex-local neighborhood.
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
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
        entry: EdgeEntry,
    ) -> Option<LogOffset> {
        self.0
            .append_overflow_entry(vertex_ref, ordinal, edge_id, entry)
    }

    /// Returns whether the forward base interval can accept a tail append.
    pub fn can_append_base_entry(&self, ordinal: usize) -> Option<bool> {
        self.0.can_append_base_entry(ordinal)
    }

    /// Returns the base-insert decision for the forward surface.
    pub fn choose_base_insert_slot(&self, ordinal: usize) -> Option<BaseInsertDecision> {
        self.0.choose_base_insert_slot(ordinal)
    }

    pub fn choose_base_insert_slot_with(
        &self,
        ordinal: usize,
        segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<BaseInsertDecision> {
        self.0
            .choose_base_insert_slot_with(ordinal, segment_slot_capacity)
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

    pub fn insert_base_entry_with(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
        segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<EdgeInsertPath> {
        self.0
            .insert_base_entry_with(ordinal, entry, segment_slot_capacity)
    }

    /// Replaces one forward overflow entry identified by semantic edge id.
    pub fn replace_overflow_entry(
        &mut self,
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
        new_entry: EdgeEntry,
    ) -> Option<OverflowEntry> {
        self.0
            .replace_overflow_entry(vertex_ref, ordinal, edge_id, new_entry)
    }

    /// Tombstones one forward overflow entry identified by semantic edge id.
    pub fn tombstone_overflow_entry(
        &mut self,
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
    ) -> Option<OverflowEntry> {
        self.0
            .tombstone_overflow_entry(vertex_ref, ordinal, edge_id)
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

    pub fn set_base_segment_slot_capacity(&mut self, segment_id: u32, slot_capacity: u64) {
        self.0
            .set_base_segment_slot_capacity(segment_id, slot_capacity);
    }

    pub fn sync_base_segment_slot_capacity_from_manager(
        &mut self,
        segment_id: u32,
        slot_capacity: u64,
    ) {
        self.0
            .sync_base_segment_slot_capacity_from_manager(segment_id, slot_capacity);
    }

    /// Returns the canonical base-neighborhood view for one vertex ordinal.
    pub fn base_neighborhood(&self, ordinal: usize) -> Option<BaseNeighborhood> {
        self.0.base_neighborhood(ordinal)
    }

    /// Returns the merged read-side view for one vertex-local neighborhood.
    pub fn merged_neighborhood(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<MergedNeighborhoodView> {
        self.0.merged_neighborhood(vertex_ref, ordinal)
    }

    /// Materializes the overflow chain for one vertex-local neighborhood.
    pub fn overflow_entries_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<Vec<OverflowEntry>> {
        self.0.overflow_entries_for(vertex_ref, ordinal)
    }

    /// Materializes merged read order for one vertex-local neighborhood.
    pub fn merged_entries_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
    ) -> Option<Vec<EdgeEntry>> {
        self.0.merged_entries_for(vertex_ref, ordinal)
    }

    /// Resolves one physical locator inside the reverse surface.
    pub fn logical_edge_locator_for(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        logical_index: usize,
    ) -> Option<LogicalEdgeLocator> {
        self.0
            .logical_edge_locator_for(vertex_ref, ordinal, logical_index)
    }

    /// Resolves whether a locator points at a base slot or an overflow slot.
    pub fn resolve_logical_edge_slot(
        &self,
        vertex_ref: VertexRef,
        ordinal: usize,
        locator: LogicalEdgeLocator,
    ) -> Option<ResolvedEdgeSlot> {
        self.0
            .resolve_logical_edge_slot(vertex_ref, ordinal, locator)
    }

    /// Builds a locator sidecar for one reverse vertex-local neighborhood.
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
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
        entry: EdgeEntry,
    ) -> Option<LogOffset> {
        self.0
            .append_overflow_entry(vertex_ref, ordinal, edge_id, entry)
    }

    /// Returns whether the reverse base interval can accept a tail append.
    pub fn can_append_base_entry(&self, ordinal: usize) -> Option<bool> {
        self.0.can_append_base_entry(ordinal)
    }

    /// Returns the base-insert decision for the reverse surface.
    pub fn choose_base_insert_slot(&self, ordinal: usize) -> Option<BaseInsertDecision> {
        self.0.choose_base_insert_slot(ordinal)
    }

    pub fn choose_base_insert_slot_with(
        &self,
        ordinal: usize,
        segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<BaseInsertDecision> {
        self.0
            .choose_base_insert_slot_with(ordinal, segment_slot_capacity)
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

    pub fn insert_base_entry_with(
        &mut self,
        ordinal: usize,
        entry: EdgeEntry,
        segment_slot_capacity: impl FnMut(u32) -> Option<u64>,
    ) -> Option<EdgeInsertPath> {
        self.0
            .insert_base_entry_with(ordinal, entry, segment_slot_capacity)
    }

    /// Replaces one reverse overflow entry identified by semantic edge id.
    pub fn replace_overflow_entry(
        &mut self,
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
        new_entry: EdgeEntry,
    ) -> Option<OverflowEntry> {
        self.0
            .replace_overflow_entry(vertex_ref, ordinal, edge_id, new_entry)
    }

    /// Tombstones one reverse overflow entry identified by semantic edge id.
    pub fn tombstone_overflow_entry(
        &mut self,
        vertex_ref: VertexRef,
        ordinal: usize,
        edge_id: EdgeId,
    ) -> Option<OverflowEntry> {
        self.0
            .tombstone_overflow_entry(vertex_ref, ordinal, edge_id)
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
    use std::collections::{BTreeMap, BTreeSet};

    use super::{ForwardSurfaceRuntime, ReverseSurfaceRuntime, SurfaceBaseStorage, SurfaceRuntime};
    use crate::low_level::{
        EMPTY_LOG_OFFSET, EdgeEntry, EdgeIndex, EdgeMeta, EdgeRef, ForwardSurface, LogOffset,
        OverflowEntry, RegionKind, RegionRef, RegionStorageKind, ResolvedEdgeSlot, ReverseSurface,
        SurfaceKind, SurfaceRegions, VertexEntry, VertexLabelIndexEntry, VertexLabelRange,
        VertexRef,
    };

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
            .merged_neighborhood(VertexRef::from(1u8).into(), 0)
            .expect("merged neighborhood");
        assert_eq!(merged.base.start, EdgeIndex::new(4));
        assert!(!merged.has_overflow());
    }

    #[test]
    fn vertex_reserved_span_len_uses_next_vertex_start_in_single_segment_mode() {
        let runtime = SurfaceRuntime::without_overflow(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(4), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(9), 1, EMPTY_LOG_OFFSET),
            ],
        );

        assert_eq!(runtime.vertex_reserved_span_len(0), Some(5));
    }

    #[test]
    fn vertex_reserved_span_len_uses_base_entries_len_for_last_vertex() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(4), 2, EMPTY_LOG_OFFSET)],
            vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(8u8), EdgeMeta::new(1, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(runtime.vertex_reserved_span_len(0), Some(4));
    }

    #[test]
    fn vertex_reserved_span_len_with_uses_segment_capacity_lookup() {
        let runtime = SurfaceRuntime::without_overflow(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new((3_u64 << 40) | 10), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new((4_u64 << 40) | 4), 1, EMPTY_LOG_OFFSET),
            ],
        );

        let reserved = runtime.vertex_reserved_span_len_with(0, |segment_id| match segment_id {
            3 => Some(64),
            _ => None,
        });

        assert_eq!(reserved, Some(54));
    }

    #[test]
    fn reserved_base_entries_for_uses_reserved_span_not_live_degree() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new(3), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
                EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(9, true)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(3, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(
            runtime.base_entries_for(0),
            Some(vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
            ])
        );
        assert_eq!(
            runtime.reserved_base_entries_for(0),
            Some(vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
                EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(9, true)),
            ])
        );
    }

    #[test]
    fn base_edge_ref_for_round_trips_with_logical_base_index() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new((7_u64 << 40) | 3), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new((7_u64 << 40) | 6), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(7, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let edge_ref = runtime.base_edge_ref_for(0, 1).expect("edge ref");
        assert_eq!(edge_ref.segment_id(), 7);
        assert_eq!(edge_ref.start_slot(), 4);
        assert_eq!(
            runtime.logical_base_index_for_edge_ref(0, edge_ref),
            Some(1)
        );
        assert_eq!(runtime.logical_base_index_for_edge_ref(1, edge_ref), None);
    }

    #[test]
    fn base_entries_by_ref_reads_contiguous_slice_from_packed_edge_ref() {
        let runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new((7_u64 << 40) | 3), 2, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new((7_u64 << 40) | 6), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(7, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let edge_ref = runtime.base_edge_ref_for(0, 0).expect("edge ref");
        assert_eq!(
            runtime.base_entries_by_ref(edge_ref, 2),
            Some(vec![
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(5, false)),
            ])
        );
    }

    #[test]
    fn segmented_base_storage_resolves_edge_refs_with_global_flat_indices() {
        let storage = super::SurfaceBaseStorage::from_segmented_for_tests(BTreeMap::from([
            (
                1,
                vec![
                    EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(1, false)),
                    EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(2, false)),
                ],
            ),
            (
                3,
                vec![
                    EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(3, false)),
                    EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(4, false)),
                    EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(5, false)),
                ],
            ),
        ]));

        let slot = storage
            .slot_for_ref(EdgeRef::new(3, 1))
            .expect("segment-local ref should resolve");
        assert_eq!(slot.index, 3);
        assert_eq!(
            storage.get_by_ref(EdgeRef::new(3, 1)),
            Some(EdgeEntry::new(
                VertexRef::from(21u8),
                EdgeMeta::new(4, false)
            ))
        );
        assert_eq!(
            storage.get_slice_by_ref(EdgeRef::new(3, 1), 2),
            Some(
                [
                    EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(4, false)),
                    EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(5, false)),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn segmented_surface_runtime_reads_base_entries_by_ref() {
        let runtime = SurfaceRuntime {
            layout: forward_surface().layout(),
            vertices: vec![VertexEntry::new(
                EdgeIndex::new((3_u64 << 40) | 1),
                2,
                EMPTY_LOG_OFFSET,
            )],
            base_entries: super::SurfaceBaseStorage::from_segmented_for_tests(BTreeMap::from([
                (
                    1,
                    vec![
                        EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(1, false)),
                        EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(2, false)),
                    ],
                ),
                (
                    3,
                    vec![
                        EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(3, false)),
                        EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(4, false)),
                        EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(5, false)),
                    ],
                ),
            ])),
            overflow_entries: Vec::new(),
            label_index_entries: vec![VertexLabelIndexEntry::new(0, 0)],
            label_ranges: Vec::new(),
            dirty_regions: super::SurfaceDirtyRegions::default(),
            dirty_vertices: BTreeSet::new(),
        };

        assert_eq!(
            runtime.base_entries_by_ref(EdgeRef::new(3, 1), 2),
            Some(vec![
                EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(5, false)),
            ])
        );
        assert_eq!(
            runtime.base_entries_for(0),
            Some(vec![
                EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(5, false)),
            ])
        );
    }

    #[test]
    fn reverse_runtime_keeps_reverse_surface_kind_in_views() {
        let runtime = ReverseSurfaceRuntime::new(
            reverse_surface(),
            vec![VertexEntry::new(EdgeIndex::new(9), 2, 7)],
            vec![
                EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(2, false)),
                EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(23u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(24u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(25u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(VertexRef::from(26u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(27u8), EdgeMeta::new(8, false)),
                EdgeEntry::new(VertexRef::from(28u8), EdgeMeta::new(9, false)),
                EdgeEntry::new(VertexRef::from(29u8), EdgeMeta::new(10, false)),
                EdgeEntry::new(VertexRef::from(30u8), EdgeMeta::new(11, false)),
            ],
            vec![OverflowEntry::new(
                77,
                EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(5, false)),
                LogOffset::EMPTY,
            )],
        );

        let merged = runtime
            .merged_neighborhood(VertexRef::from(5u8).into(), 0)
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let entries = runtime
            .overflow_entries_for(VertexRef::from(1u8).into(), 0)
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let merged = runtime
            .merged_entries_for(VertexRef::from(7u8).into(), 0)
            .expect("merged entries");
        assert_eq!(merged.len(), 4);
        assert_eq!(u64::from(merged[0].target), 3);
        assert_eq!(u64::from(merged[1].target), 4);
        assert_eq!(u64::from(merged[2].target), 10);
        assert_eq!(u64::from(merged[3].target), 9);
    }

    #[test]
    fn logical_edge_locator_round_trips_base_and_overflow_positions() {
        let runtime = ForwardSurfaceRuntime::new(
            forward_surface(),
            vec![VertexEntry::new(EdgeIndex::new(2), 2, 1)],
            vec![
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(1, false)),
            ],
            vec![
                OverflowEntry::new(
                    41,
                    EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    42,
                    EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(4, false)),
                    LogOffset::new(0),
                ),
            ],
        );

        let vertex = VertexRef::from(7u8);
        let base_locator = runtime
            .logical_edge_locator_for(vertex.into(), 0, 1)
            .expect("base logical locator");
        let overflow_locator = runtime
            .logical_edge_locator_for(vertex.into(), 0, 2)
            .expect("overflow logical locator");

        assert!(!base_locator.is_overflow());
        assert!(overflow_locator.is_overflow());
        assert_eq!(
            runtime.resolve_logical_edge_slot(vertex.into(), 0, base_locator),
            Some(ResolvedEdgeSlot::Base { logical_index: 1 })
        );
        assert!(matches!(
            runtime.resolve_logical_edge_slot(vertex.into(), 0, overflow_locator),
            Some(ResolvedEdgeSlot::Overflow {
                overflow_index: 0,
                ..
            })
        ));
    }

    #[test]
    fn runtime_can_read_vertex_label_ranges() {
        let mut runtime = SurfaceRuntime::without_overflow(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
        );
        runtime.base_entries = SurfaceBaseStorage::from_contiguous(vec![
            EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
            EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(4, false)),
        ]);
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(6u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(7u8), EdgeMeta::new(6, false)),
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
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(30u8), EdgeMeta::new(8, false)),
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(9, false)),
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

        runtime
            .base_entries
            .replace(
                2,
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(7, false)),
            )
            .expect("replace slot 2");
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(4, false)),
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(5, false)),
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
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(3, false)),
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(4, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let old = runtime.tombstone_base_entry(0, 1).expect("tombstoned");
        assert_eq!(old.meta.label_id(), 4);
        assert_eq!(runtime.dirty_vertices().collect::<Vec<_>>(), vec![0]);
        assert!(
            runtime
                .base_entries
                .get(1)
                .expect("entry 1")
                .meta
                .is_tombstone()
        );
    }

    #[test]
    fn overflow_append_updates_log_head_without_marking_label_sidecar_dirty() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 1, EMPTY_LOG_OFFSET)],
            vec![EdgeEntry::new(
                VertexRef::from(1u8),
                EdgeMeta::new(3, false),
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let offset = runtime
            .append_overflow_entry(
                VertexRef::from(9u8).into(),
                0,
                77,
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(4, false)),
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
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(5, false)),
            ],
            vec![
                OverflowEntry::new(
                    70,
                    EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(6, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    71,
                    EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(7, true)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    72,
                    EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(8, false)),
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
        assert_eq!(delta.segment_id(), 0);
        assert_eq!(delta.base_start_slot(), 0);
        assert_eq!(
            delta.compacted_base_entries,
            vec![
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(8, false)),
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
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, true)),
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
    fn weighted_window_layout_rejects_cross_segment_windows() {
        let runtime = SurfaceRuntime::without_overflow(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new((1_u64 << 40) | 0), 1, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new((2_u64 << 40) | 2), 1, EMPTY_LOG_OFFSET),
            ],
        );

        assert_eq!(runtime.build_weighted_window_layout(0, 0, 2, 4), None);
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
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(7, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(8, true)),
                EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(9, false)),
            ],
            vec![OverflowEntry::new(
                90,
                EdgeEntry::new(VertexRef::from(5u8), EdgeMeta::new(10, false)),
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
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(4, false)),
                EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(5, false)),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let layout = runtime
            .build_weighted_window_layout(0, 0, 2, 6)
            .expect("weighted window layout");

        assert_eq!(layout.base_start, EdgeIndex::new(0));
        assert_eq!(layout.segment_id(), 0);
        assert_eq!(layout.base_start_slot(), 0);
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
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(13u8), EdgeMeta::new(6, false)),
            ],
            vec![
                OverflowEntry::new(
                    70,
                    EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(6, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    71,
                    EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(7, true)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    72,
                    EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(8, false)),
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
        runtime.sync_base_segment_slot_capacity_from_manager(1, 4);
        runtime.sync_base_segment_slot_capacity_from_manager(2, 8);

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
            SurfaceBaseStorage::iter(&runtime.base_entries)
                .copied()
                .collect::<Vec<_>>(),
            vec![
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(6, false)),
                EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(8, false)),
                EdgeEntry::new(VertexRef::from(13u8), EdgeMeta::new(6, false)),
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

    #[test]
    fn runtime_apply_local_rebalance_delta_shifts_only_following_vertices_in_same_segment() {
        let mut runtime = SurfaceRuntime::new(
            forward_surface().layout(),
            vec![
                VertexEntry::new(EdgeIndex::new((1_u64 << 40) | 0), 2, 0),
                VertexEntry::new(EdgeIndex::new((1_u64 << 40) | 2), 1, 2),
                VertexEntry::new(EdgeIndex::new((1_u64 << 40) | 3), 1, EMPTY_LOG_OFFSET),
                VertexEntry::new(EdgeIndex::new((2_u64 << 40) | 7), 1, EMPTY_LOG_OFFSET),
            ],
            vec![
                EdgeEntry::new(VertexRef::from(10u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(4, true)),
                EdgeEntry::new(VertexRef::from(12u8), EdgeMeta::new(5, false)),
                EdgeEntry::new(VertexRef::from(13u8), EdgeMeta::new(6, false)),
            ],
            vec![
                OverflowEntry::new(
                    70,
                    EdgeEntry::new(VertexRef::from(20u8), EdgeMeta::new(6, false)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    71,
                    EdgeEntry::new(VertexRef::from(21u8), EdgeMeta::new(7, true)),
                    LogOffset::EMPTY,
                ),
                OverflowEntry::new(
                    72,
                    EdgeEntry::new(VertexRef::from(22u8), EdgeMeta::new(8, false)),
                    LogOffset::EMPTY,
                ),
            ],
            vec![
                VertexLabelIndexEntry::new(0, 0),
                VertexLabelIndexEntry::new(0, 0),
                VertexLabelIndexEntry::new(0, 0),
                VertexLabelIndexEntry::new(0, 0),
            ],
            Vec::new(),
        );
        runtime.sync_base_segment_slot_capacity_from_manager(1, 4);
        runtime.sync_base_segment_slot_capacity_from_manager(2, 8);

        let delta = runtime
            .build_local_rebalance_delta(0, 0, 2, 4)
            .expect("rebalance delta");
        let summary = runtime
            .apply_local_rebalance_delta(delta)
            .expect("apply rebalance delta");

        assert_eq!(summary.displacement, 1);
        assert_eq!(
            runtime.vertices[2],
            VertexEntry::new(EdgeIndex::new((1_u64 << 40) | 4), 1, EMPTY_LOG_OFFSET)
        );
        assert_eq!(
            runtime.vertices[3],
            VertexEntry::new(EdgeIndex::new((2_u64 << 40) | 7), 1, EMPTY_LOG_OFFSET)
        );
    }
}
