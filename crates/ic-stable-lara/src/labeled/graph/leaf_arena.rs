//! PMA leaf physical pin for labeled edge bytes (Phase A).
//!
//! Normal labeled rows reserve edge bytes inside the leaf block recorded in
//! [`SegmentSpanMetaStore`], mirroring core LARA `try_materialize_leaf_slab`.

use crate::{
    SegmentId, VertexId,
    labeled::{record::LabelBucket, slot_index::checked_add_slot_index},
    lara::{
        edge::{counts::segment_span_density, span_meta::SPAN_PHYSICAL_UNASSIGNED},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::LabeledLaraGraph;
use super::error::LabeledOperationError;

#[cfg(test)]
use super::OutEdgeOrder;

/// Edge-slab slots reserved per vertex within one PMA leaf block.
pub(crate) const fn labeled_leaf_vertex_edge_quota(segment_size: u32) -> u32 {
    if segment_size == 16 || segment_size == 0 {
        1
    } else {
        segment_size
    }
}

/// Initial edge-span reservation for the production segment16/quota1 policy.
pub(crate) fn labeled_leaf_initial_bucket_quota(segment_size: u32) -> u32 {
    if segment_size == 16 { 1 } else { 0 }
}

/// Total edge-slab slots reserved for one PMA leaf when pinned.
pub(crate) fn labeled_leaf_physical_block_len(segment_size: u32) -> u64 {
    u64::from(
        segment_size
            .max(1)
            .saturating_mul(labeled_leaf_vertex_edge_quota(segment_size)),
    )
}

fn labeled_vertex_edge_offset_in_leaf(vid: VertexId, segment_size: u32) -> u64 {
    u64::from(u32::from(vid) % segment_size.max(1))
        .saturating_mul(u64::from(labeled_leaf_vertex_edge_quota(segment_size)))
}

/// The caller's live region inside a leaf, which the arena must keep readable.
///
/// Placement never overlaps `source`, so the caller can read it before the new
/// span replaces it. The type is what keeps "no source" out of the API: a growth
/// always has a live region, and a caller that cannot name one has no business
/// asking the arena for room.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceRegion {
    start: u64,
    len: u64,
}

impl SourceRegion {
    pub(crate) fn new(start: u64, len: u64) -> Self {
        Self { start, len }
    }

    fn as_pair(self) -> (u64, u64) {
        (self.start, self.len)
    }
}

/// Owner of one PMA leaf's slot space.
///
/// Vocabulary (module-private; nothing outside this module needs it):
///
/// ```text
/// extent   [start, end)   the leaf's reservation; `counts.total == end - start`
///   slice  [start, end)   the exclusive space of one vertex
///     region              the space granted to one bucket (= content + its gap)
///       content           what the bucket really wrote (slab rows / tree root ids)
/// ```
///
/// Unused space has three owners: a **gap** belongs to its bucket (grow in place,
/// no arena call), a **free run** belongs to the slice (new region or region
/// growth), and **leaf slack** (before the first slice) belongs to nobody — only a
/// layout rewrite reclaims it.
///
/// Every resident of a leaf vertex must live inside this range: relocation copies the
/// block, density follows it, and the whole-cover release frees it. The type exists so
/// that "is this slot inside the pin" has one answer and one owner, instead of each
/// operation reading `labeled_leaf_physical_range` and deciding for itself.
pub(crate) struct LeafArena<'a, E: CsrEdge, M: Memory> {
    graph: &'a LabeledLaraGraph<E, M>,
    leaf: u32,
    extent: (u64, u64),
}

impl<'a, E, M> LeafArena<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Captures `vid`'s leaf pin, or `None` when the leaf has no pinned block yet.
    pub(crate) fn capture(
        graph: &'a LabeledLaraGraph<E, M>,
        vid: VertexId,
    ) -> Result<Option<Self>, LabeledOperationError> {
        let Some(range) = graph.labeled_leaf_physical_range(vid)? else {
            return Ok(None);
        };
        let header = graph.edges.header();
        let leaf = LabeledLaraGraph::<E, M>::leaf_index_for_vid(vid, header.segment_size.max(1));
        Ok(Some(Self {
            graph,
            leaf,
            extent: range,
        }))
    }

    pub(crate) fn leaf(&self) -> u32 {
        self.leaf
    }

    /// `(start_slot, block_len)` of the pinned block.
    pub(crate) fn extent(&self) -> (u64, u64) {
        self.extent
    }

    /// Slab spans owned by the other labeled vertices of this leaf.
    pub(crate) fn occupied(&self, exclude: VertexId) -> Vec<(u64, u64)> {
        // `occupied_spans` derives the leaf from `vid`; any member resolves the same leaf,
        // and `exclude` is the only vertex-specific input.
        let any_member = self
            .leaf
            .saturating_mul(self.graph.edges.header().segment_size.max(1));
        self.graph
            .labeled_leaf_occupied_spans(VertexId::from(any_member), exclude)
    }

    /// The vertex's resident slab span: first non-tiny bucket base plus `stored_slots`.
    pub(crate) fn vertex_space(&self, vid: VertexId) -> Option<(u64, u64)> {
        let vertex = self.graph.vertices.get(vid);
        let buckets = self.graph.read_vertex_label_buckets(&vertex).ok()?;
        let first = buckets.iter().find(|bucket| !bucket.is_tiny_mode())?;
        let base = first.edge_start();
        let end = checked_add_slot_index(base, u64::from(vertex.stored_slots))?;
        Some((base, end))
    }

    /// Every resident span in this leaf, per bucket plus bypass rows (the vertex row owns those).
    pub(crate) fn used_spans(&self) -> Vec<(u64, u64)> {
        let seg = self.graph.edges.header().segment_size.max(1);
        let start_vid = self.leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.graph.vertices.len());
        let mut spans = Vec::new();
        for ordinal in start_vid..end_vid {
            let vertex = self.graph.vertices.get(VertexId::from(ordinal));
            if vertex.stored_slots == 0 {
                continue;
            }
            if vertex.is_default_edge_labeled() {
                let base = vertex.base_slot_start();
                spans.push((base, base + u64::from(vertex.stored_slots)));
                continue;
            }
            let Ok(buckets) = self.graph.read_vertex_label_buckets(&vertex) else {
                continue;
            };
            // The declared cover (first non-tiny anchor + stored_slots) is the rule the leaf-mate
            // overlap guard uses; the per-bucket regions are the rule relocation copies. Both are
            // pushed so the heap never hands out a range either rule considers occupied.
            if vertex.stored_slots > 0
                && let Some(first) = buckets.iter().find(|bucket| !bucket.is_tiny_mode())
            {
                let base = first.edge_start();
                spans.push((base, base + u64::from(vertex.stored_slots)));
            }
            for bucket in &buckets {
                let Some((start, slots)) = super::compact::bucket_content_region(bucket) else {
                    continue;
                };
                if slots > 0 {
                    spans.push((start, start + u64::from(slots)));
                }
            }
        }
        spans.sort_unstable();
        spans
    }

    /// Leaf heap allocation: best fit over the per-bucket resident set, keeping `avoid` intact.
    pub(crate) fn allocate(
        &self,
        vid: VertexId,
        len: u64,
        avoid: Option<(u64, u64)>,
    ) -> Option<u64> {
        self.place(vid, len, avoid?)
    }

    /// Residents inside the block and pairwise disjoint.
    /// The leaf's slices: participants sorted by their first content start, each owning
    /// `[start_i, start_{i+1})`, the last up to the extent end. The slice is the vertex's
    /// exclusive space; the tiling lays the vertices out in start order with no gaps, so
    /// this partition needs no `stored_slots`.
    pub(crate) fn slices(&self) -> Vec<(VertexId, (u64, u64))> {
        let seg = self.graph.edges.header().segment_size.max(1);
        let start_vid = self.leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.graph.vertices.len());
        let extent_end = self.extent.0.saturating_add(self.extent.1);
        let mut starts: Vec<(u64, VertexId)> = Vec::new();
        for ordinal in start_vid..end_vid {
            let vid = VertexId::from(ordinal);
            let vertex = self.graph.vertices.get(vid);
            if vertex.stored_slots == 0 {
                continue;
            }
            let base = if vertex.is_default_edge_labeled() {
                Some(vertex.base_slot_start())
            } else {
                self.graph
                    .read_vertex_label_buckets(&vertex)
                    .ok()
                    .and_then(|buckets| {
                        buckets
                            .iter()
                            .find(|bucket| !bucket.is_tiny_mode())
                            .map(|bucket| bucket.edge_start())
                    })
            };
            if let Some(base) = base {
                starts.push((base, vid));
            }
        }
        starts.sort_unstable();
        let participants = starts.len();
        starts.dedup_by_key(|(base, _)| *base);
        debug_assert_eq!(
            participants,
            starts.len(),
            "two vertices of one leaf start at the same slot"
        );
        let mut slices = Vec::with_capacity(starts.len());
        for (i, (start, vid)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map(|(s, _)| *s).unwrap_or(extent_end);
            if end > *start {
                slices.push((*vid, (*start, end)));
            }
        }
        slices
    }

    /// The vertex's slice, or `None` when it owns no content.
    pub(crate) fn slice(&self, vid: VertexId) -> Option<(u64, u64)> {
        self.slices()
            .into_iter()
            .find(|(owner, _)| *owner == vid)
            .map(|(_, range)| range)
    }

    /// The regions one vertex's buckets occupy (its content). Tiny buckets own nothing.
    pub(crate) fn regions(&self, vid: VertexId) -> Vec<(u64, u64)> {
        let vertex = self.graph.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            let base = vertex.base_slot_start();
            let end = base.saturating_add(u64::from(vertex.stored_slots));
            return if end > base {
                vec![(base, end)]
            } else {
                Vec::new()
            };
        }
        let Ok(buckets) = self.graph.read_vertex_label_buckets(&vertex) else {
            return Vec::new();
        };
        buckets
            .iter()
            .filter_map(|bucket| super::compact::bucket_content_region(bucket))
            .filter(|(_, slots)| *slots > 0)
            .map(|(start, slots)| (start, start + u64::from(slots)))
            .collect()
    }

    /// End of the right-most content in the leaf.
    pub(crate) fn last_region_end(&self) -> u64 {
        let seg = self.graph.edges.header().segment_size.max(1);
        let start_vid = self.leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.graph.vertices.len());
        let mut last = self.extent.0;
        for ordinal in start_vid..end_vid {
            for (_, end) in self.regions(VertexId::from(ordinal)) {
                last = last.max(end);
            }
        }
        last
    }

    pub(crate) fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let extent_end = self.extent.0.saturating_add(self.extent.1);
        let slices = self.slices();
        // The partition: strictly increasing, inside the extent, the last one ending at
        // the extent end (anything before the first slice is leaf slack).
        for (vid, (start, end)) in &slices {
            if !self.extent_contains(*start, end.saturating_sub(*start)) {
                errors.push(format!("slice of {vid:?} {start}..{end} outside extent"));
            }
        }
        for pair in slices.windows(2) {
            if pair[0].1.1 > pair[1].1.0 {
                errors.push(format!(
                    "slices overlap: {:?} and {:?}",
                    pair[0].1, pair[1].1
                ));
            }
        }
        if let Some((_, (_, last_end))) = slices.last()
            && *last_end != extent_end
        {
            errors.push(format!(
                "last slice ends at {last_end}, extent ends at {extent_end}"
            ));
        }
        // Every region lives in its own vertex's slice, and regions of one slice do not
        // overlap.
        for (vid, (slice_start, slice_end)) in &slices {
            let regions = self.regions(*vid);
            for (start, end) in &regions {
                if *start < *slice_start || *end > *slice_end {
                    errors.push(format!(
                        "region of {vid:?} {start}..{end} outside its slice {slice_start}..{slice_end}"
                    ));
                }
            }
            for pair in regions.windows(2) {
                if pair[0].1 > pair[1].0 {
                    errors.push(format!(
                        "regions of {vid:?} overlap: {:?} and {:?}",
                        pair[0], pair[1]
                    ));
                }
            }
        }
        errors
    }

    /// Lowest free base in this pin offering `need` slots, keeping `avoid` (the caller's own
    /// live source) intact. Unlike `free_base`, the vertex's own span is not treated as free:
    /// the caller still has to read it before it can be overwritten.
    pub(crate) fn place(&self, _vid: VertexId, need: u64, avoid: (u64, u64)) -> Option<u64> {
        if need == 0 || need > self.extent.1 {
            return None;
        }
        let leaf_end = checked_add_slot_index(self.extent.0, self.extent.1)?;
        let mut spans = self.used_spans();
        spans.push(avoid);
        spans.sort_by_key(|(start, _)| *start);
        let mut cursor = self.extent.0;
        for (start, end) in &spans {
            if *start > cursor && start.saturating_sub(cursor) >= need {
                return Some(cursor);
            }
            if *end > cursor {
                cursor = *end;
            }
        }
        if checked_add_slot_index(cursor, need)? <= leaf_end {
            Some(cursor)
        } else {
            None
        }
    }

    /// Free slots at the very end of the block: the run a growth can extend
    /// without moving any resident.
    fn free_tail(&self) -> u64 {
        let (start, len) = self.extent;
        let Some(end) = checked_add_slot_index(start, len) else {
            return 0;
        };
        let last_end = self
            .used_spans()
            .iter()
            .map(|&(_, span_end)| span_end)
            .max()
            .unwrap_or(start)
            .min(end);
        end - last_end
    }

    /// Extends the block so that it ends with a free run of `len` slots.
    ///
    /// One free-span edit and no copy: the new slots come from the span that starts
    /// right after the block, and only the leaf's `total` moves (the block's slots are
    /// leaf geometry, not live slab rows). Returns false when that span is not free or
    /// the block would pass the slab capacity, which leaves the block unchanged: the
    /// caller has nowhere to grow and must relocate or fail.
    fn grow(&mut self, len: u64) -> bool {
        let (start, block_len) = self.extent;
        let tail_free = self.free_tail();
        if tail_free >= len {
            return true;
        }
        let extension = len - tail_free;
        let Some(new_len) = block_len.checked_add(extension) else {
            return false;
        };
        let Some(block_end) = checked_add_slot_index(start, block_len) else {
            return false;
        };
        let capacity = self.graph.edges.header().elem_capacity;
        let extended = if block_end == capacity {
            // The block ends at the slab end, so appending capacity extends it:
            // `allocate_span` hands out the old capacity as the new span's base.
            matches!(self.graph.edges.allocate_span(extension), Ok(base) if base == block_end)
        } else {
            self.graph
                .edges
                .try_extend_span(start, block_len, extension)
        };
        if !extended {
            return false;
        }
        let Ok(delta) = i64::try_from(extension) else {
            return false;
        };
        let leaf_vid = VertexId::from(
            self.leaf
                .saturating_mul(self.graph.edges.header().segment_size.max(1)),
        );
        if self
            .graph
            .edges
            .bump_vertex_segment_counts(leaf_vid, 0, delta)
            .is_err()
        {
            let _ = self
                .graph
                .edges
                .release_span(start.saturating_add(block_len), extension);
            return false;
        }
        self.extent.1 = new_len;
        true
    }

    pub(crate) fn extent_contains(&self, start: u64, len: u64) -> bool {
        let Some(end) = checked_add_slot_index(start, len) else {
            return false;
        };
        self.extent.0 <= start
            && end <= checked_add_slot_index(self.extent.0, self.extent.1).unwrap_or(u64::MAX)
    }

    /// Reports every resident of this leaf that lies outside the pin or outside its vertex's
    /// own cover. This is the contract check; it is not a repair.
    pub(crate) fn containment_errors(&self) -> Vec<String> {
        let seg = self.graph.edges.header().segment_size.max(1);
        let start_vid = self.leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.graph.vertices.len());
        let mut errors = Vec::new();
        for ordinal in start_vid..end_vid {
            let vid = VertexId::from(ordinal);
            let Some((base, end)) = self.vertex_space(vid) else {
                continue;
            };
            if !self.extent_contains(base, end.saturating_sub(base)) {
                errors.push(format!(
                    "owner {ordinal} cover[{base},{end}) outside pin {:?}",
                    Some(self.extent)
                ));
            }
            // The contract is per bucket: every resident region lives inside the pin. A
            // vertex's buckets are not required to be contiguous (promotion leaves the freed
            // span as leaf slack and places the root wherever the pin has room).
            let vertex = self.graph.vertices.get(vid);
            let Ok(buckets) = self.graph.read_vertex_label_buckets(&vertex) else {
                continue;
            };
            for bucket in &buckets {
                let Some((start, slots)) = super::compact::bucket_content_region(bucket) else {
                    continue;
                };
                let Some(limit) = checked_add_slot_index(start, u64::from(slots)) else {
                    continue;
                };
                if !self.extent_contains(start, u64::from(slots)) {
                    errors.push(format!(
                        "owner {ordinal} label{} resident[{start},{limit}) outside pin {:?}",
                        bucket.bucket_label_key().raw(),
                        Some(self.extent)
                    ));
                }
            }
        }
        errors
    }
}

impl<'a, E, M> LeafArena<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The single place that decides where a resident span lives.
    ///
    /// `source` is the caller's live region: it must stay readable, so the arena never
    /// hands out a base that overlaps it. Everything else about the choice - inside the
    /// block first, then the global free-span store - is this function's decision, not
    /// the caller's.
    pub(super) fn reserve(
        graph: &'a LabeledLaraGraph<E, M>,
        vid: VertexId,
        len: u64,
        source: SourceRegion,
    ) -> Result<u64, crate::GrowFailed> {
        if let Ok(Some(mut arena)) = Self::capture(graph, vid) {
            // 1. Rebalance inside the block: a free run already exists.
            if let Some(base) = arena.allocate(vid, len, Some(source.as_pair())) {
                return Ok(base);
            }
            // 2. Grow the block until it ends with a free run of `len`, then place.
            if arena.grow(len)
                && let Some(base) = arena.allocate(vid, len, Some(source.as_pair()))
            {
                return Ok(base);
            }
        }
        graph
            .edges
            .allocate_span_avoiding(len, Some(source.as_pair()))
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Returns `(physical_start, block_len)` when the leaf is pinned, else `None`.
    ///
    /// `block_len` tracks the leaf PMA `segment_edges_total` (grows on segment relocate).
    pub(crate) fn labeled_leaf_physical_range(
        &self,
        vid: VertexId,
    ) -> Result<Option<(u64, u64)>, LabeledOperationError> {
        Ok(self
            .edges
            .assigned_physical_range(vid)?
            .map(|range| (range.start_slot, range.len)))
    }

    /// Edge-slab spans `[base, base + stored_slots)` owned by other labeled vertices in
    /// `vid`'s PMA leaf. Used to place a new vertex's edge span without overlapping a
    /// leaf-mate whose weighted span exceeds the fixed per-vertex quota.
    pub(super) fn labeled_leaf_occupied_spans(
        &self,
        vid: VertexId,
        exclude: VertexId,
    ) -> Vec<(u64, u64)> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let mut spans = Vec::new();
        for vid_u in start_vid..end_vid {
            let other = VertexId::from(vid_u);
            if other == exclude {
                continue;
            }
            let vertex = self.vertices.get(other);
            if vertex.stored_slots == 0 {
                continue;
            }
            if vertex.is_default_edge_labeled() {
                let base = vertex.base_slot_start();
                let Some(end) = checked_add_slot_index(base, u64::from(vertex.stored_slots)) else {
                    continue;
                };
                spans.push((base, end));
                continue;
            }
            let Ok(buckets) = self.read_vertex_label_buckets(&vertex) else {
                continue;
            };
            // ADR 0096 §5: anchor at the first NON-TINY bucket (tiny anchors
            // are placeholders, not storage). Vertices without slab spans
            // contribute nothing below (their stored_slots is 0).
            let Some(first) = buckets.iter().find(|bucket| !bucket.is_tiny_mode()) else {
                continue;
            };
            let base = first.edge_start();
            let Some(end) = checked_add_slot_index(base, u64::from(vertex.stored_slots)) else {
                continue;
            };
            spans.push((base, end));
        }
        spans
    }

    /// `true` when some labeled vertex in `vid`'s PMA leaf reserves more than `quota`
    /// slots, i.e. the fixed-quota leaf layout no longer holds and a new vertex's quota
    /// offset may overlap a leaf-mate. Cheap: reads only vertex records, not buckets.
    fn labeled_leaf_has_oversized_vertex(&self, vid: VertexId, quota: u64) -> bool {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vid_u));
            if u64::from(vertex.stored_slots) > quota {
                return true;
            }
        }
        false
    }

    /// Lowest base offering `need` contiguous free slots in `vid`'s pinned leaf block that
    /// does not overlap any leaf-mate's span. Prefers the fixed per-vertex quota offset.
    pub(super) fn find_free_labeled_leaf_edge_base(
        &self,
        vid: VertexId,
        leaf_start: u64,
        leaf_len: u64,
        need: u64,
    ) -> Option<u64> {
        if need == 0 || need > leaf_len {
            return None;
        }
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)?;
        let quota = u64::from(labeled_leaf_vertex_edge_quota(seg));
        let preferred =
            checked_add_slot_index(leaf_start, labeled_vertex_edge_offset_in_leaf(vid, seg));

        // Fast path: when no leaf-mate's span exceeds the fixed per-vertex quota, the
        // fixed-quota layout is intact and the requesting vertex's quota offset is free.
        // This avoids reading every leaf-mate's buckets on the common (sparse) insert.
        //
        // Soundness rests on the weighted-slide invariant: a relocate/slide tiles the
        // *entire* leaf block across its active vertices (each `stored_slots` spans up to
        // the next vertex, the last to `leaf_end`). So a leaf with `k < seg` active
        // vertices — i.e. one that still has a free degree-0 slot to place — has an
        // average tile width `leaf_len / k > quota`, forcing at least one oversized
        // vertex and bypassing this fast path. When the fast path *does* fire, the leaf
        // is in the untiled fixed-quota layout where each vertex sits in its own quota
        // slot, so `preferred` is genuinely free. The debug check below pins that down.
        if need <= quota
            && !self.labeled_leaf_has_oversized_vertex(vid, quota)
            && let Some(preferred) = preferred
            && checked_add_slot_index(preferred, need)? <= leaf_end
        {
            #[cfg(debug_assertions)]
            {
                let preferred_end = checked_add_slot_index(preferred, need)?;
                let occupied = self.labeled_leaf_occupied_spans(vid, vid);
                debug_assert!(
                    occupied
                        .iter()
                        .all(|(s, e)| preferred_end <= *s || preferred >= *e),
                    "fixed-quota fast path returned base {preferred} (end {preferred_end}) overlapping a leaf-mate; layout is not fixed-quota intact"
                );
            }
            return Some(preferred);
        }

        let mut spans = self.labeled_leaf_occupied_spans(vid, vid);
        spans.sort_by_key(|(start, _)| *start);

        let fits = |base: u64| -> bool {
            match checked_add_slot_index(base, need) {
                Some(end) if end <= leaf_end => spans.iter().all(|(s, e)| end <= *s || base >= *e),
                _ => false,
            }
        };

        if let Some(preferred) = preferred
            && fits(preferred)
        {
            return Some(preferred);
        }

        let mut cursor = leaf_start;
        for (start, end) in &spans {
            if *start > cursor && start.saturating_sub(cursor) >= need {
                return Some(cursor);
            }
            if *end > cursor {
                cursor = *end;
            }
        }
        if checked_add_slot_index(cursor, need)? <= leaf_end {
            Some(cursor)
        } else {
            None
        }
    }

    /// Ensures the PMA leaf containing `vid` owns a contiguous edge-slab block, returning
    /// `(physical_start, block_len)`. Allocates the block on first use.
    pub(super) fn ensure_labeled_leaf_block_pinned(
        &self,
        vid: VertexId,
    ) -> Result<(u64, u64), LabeledOperationError> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let block_len = labeled_leaf_physical_block_len(seg);

        let span_rec = self.edges.span_meta_store().get(u64::from(leaf));
        if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
            let start = self
                .edges
                .allocate_span(block_len)
                .map_err(LabeledOperationError::from)?;
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), start)
                .map_err(LabeledOperationError::from)?;
            self.edges
                .bump_vertex_segment_counts(
                    vid,
                    0,
                    i64::try_from(block_len)
                        .map_err(|_| LaraOperationError::CollectAllocationOverflow)?,
                )
                .map_err(LabeledOperationError::from)?;
        }
        self.labeled_leaf_physical_range(vid)?
            .ok_or_else(|| LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Ensures the PMA leaf containing `vid` owns a contiguous edge-slab block and
    /// returns a free, non-overlapping first edge slot for that vertex's quota.
    pub(super) fn ensure_labeled_leaf_edge_physical_pin(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let (leaf_start, leaf_len) = self.ensure_labeled_leaf_block_pinned(vid)?;
        if let Some(base) = self.find_free_labeled_leaf_edge_base(
            vid,
            leaf_start,
            leaf_len,
            u64::from(labeled_leaf_vertex_edge_quota(
                self.edges.header().segment_size,
            )),
        ) {
            return Ok(base);
        }

        // No free, non-overlapping quota-sized region exists: the leaf block is full.
        // Returning the fixed quota offset here would land on a leaf-mate's span (the
        // data-loss bug class). The first-edge placement path never reaches this — it
        // uses `find_free` directly and relocates on `None` — so fail loud instead of
        // handing back an overlapping base. Callers needing room must relocate first.
        Err(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn labeled_leaf_geometry_stored_slots(&self, leaf: u32) -> u64 {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let mut total = 0u64;
        for vidx in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vidx));
            if vertex.is_default_edge_labeled() {
                continue;
            }
            total = total.saturating_add(u64::from(vertex.stored_slots));
        }
        total
    }

    pub(super) fn labeled_leaf_pma_density(&self, vid: VertexId) -> f64 {
        segment_span_density(self.leaf_segment_counts_for_vid(vid))
    }

    /// Interim geometry density (live / sum `stored_slots`); not used for maintenance after Phase B.
    pub(super) fn labeled_leaf_geometry_density(&self, vid: VertexId) -> f64 {
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(vid, header.segment_size);
        let geometry_total = self.labeled_leaf_geometry_stored_slots(leaf);
        if geometry_total == 0 {
            return 0.0;
        }
        let counts = self.leaf_segment_counts_for_vid(vid);
        (counts.actual.max(0) as f64) / (geometry_total as f64)
    }

    pub(super) fn try_labeled_vertex_edge_base_in_pinned_leaf(
        &self,
        vid: VertexId,
        new_alloc: u32,
    ) -> Result<Option<u64>, LabeledOperationError> {
        let Some((leaf_start, leaf_len)) = self.labeled_leaf_physical_range(vid)? else {
            return Ok(None);
        };
        let leaf_end = leaf_start + leaf_len;
        let vertex = self.vertices.get(vid);
        let base = if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            let header = self.edges.header();
            let vertex_offset = labeled_vertex_edge_offset_in_leaf(vid, header.segment_size.max(1));
            checked_add_slot_index(leaf_start, vertex_offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?
        } else {
            let buckets = self.read_vertex_label_buckets(&vertex)?;
            let Some(bucket) = buckets.iter().find(|bucket| !bucket.is_tiny_mode()) else {
                return Ok(None);
            };
            bucket.edge_start()
        };
        let end = checked_add_slot_index(base, u64::from(new_alloc))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        if end > leaf_end {
            return Ok(None);
        }
        let overlaps_leaf_mate = self
            .labeled_leaf_occupied_spans(vid, vid)
            .into_iter()
            .any(|(start, mate_end)| end > start && base < mate_end);
        Ok((!overlaps_leaf_mate).then_some(base))
    }

    /// Maximum `stored_slots` for `vid` from its first bucket base through leaf block end.
    pub(super) fn labeled_vertex_stored_slots_max_in_leaf(
        &self,
        vid: VertexId,
    ) -> Result<u32, LabeledOperationError> {
        let (leaf_start, leaf_len) = self
            .labeled_leaf_physical_range(vid)?
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let vertex = self.vertices.get(vid);
        let base = if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            self.ensure_labeled_leaf_edge_physical_pin(vid)?
        } else {
            // `unwrap_or` would eagerly run the (now occupancy-aware) pin on every call;
            // only resolve a pin when the vertex genuinely has no first bucket.
            // ADR 0096 §5: anchor at the first non-tiny bucket (tiny anchors
            // are placeholders); an all-tiny vertex has no slab base, so fall
            // through to pin resolution like the bucket-less case.
            match self
                .read_vertex_label_buckets(&vertex)?
                .iter()
                .find(|bucket| !bucket.is_tiny_mode())
            {
                Some(bucket) => bucket.edge_start(),
                None => self.ensure_labeled_leaf_edge_physical_pin(vid)?,
            }
        };
        let fit = leaf_end.saturating_sub(base);
        u32::try_from(fit).map_err(|_| LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn try_expand_labeled_leaf_in_place(
        &self,
        old_start: u64,
        old_len: u64,
        new_len: u64,
    ) -> Result<bool, LabeledOperationError> {
        if new_len <= old_len {
            return Ok(false);
        }
        let delta = new_len.saturating_sub(old_len);
        let adjacent_free_start = checked_add_slot_index(old_start, old_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let Some(right_free) = self
            .edges
            .free_span_store()
            .free_span_starting_at(adjacent_free_start)
        else {
            return Ok(false);
        };
        if right_free.len < delta {
            return Ok(false);
        }
        if self
            .edges
            .free_span_store()
            .take_prefix_at(adjacent_free_start, delta)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
            .is_none()
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// `true` when `[start, start + len)` lies in the vertex's current pinned leaf block.
    pub(super) fn labeled_edge_footprint_in_current_leaf_pin(
        &self,
        vid: VertexId,
        start: u64,
        len: u32,
    ) -> Result<bool, LabeledOperationError> {
        let Some((leaf_start, leaf_len)) = self.labeled_leaf_physical_range(vid)? else {
            return Ok(false);
        };
        let leaf_end = leaf_start + leaf_len;
        let end = checked_add_slot_index(start, u64::from(len))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        Ok(start >= leaf_start && end <= leaf_end)
    }

    pub(crate) fn labeled_bucket_edge_end_exclusive(
        bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots()))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Debug-only: asserts no two labeled vertices sharing `vid`'s PMA leaf reserve
    /// overlapping edge-slab spans.
    ///
    /// Guards the leaf-mate overlap bug class (ADR 0022): fixed-quota placement must
    /// never site a vertex's edge span on top of a leaf-mate's reservation. Each
    /// vertex's reserved span is `[first_bucket.edge_start(), + stored_slots)` — the
    /// same occupancy model [`Self::labeled_leaf_occupied_spans`] uses for placement —
    /// so adjacency (`a.end == b.start`) is allowed but any true overlap is a bug.
    /// Reads only vertex records and first buckets; gated to debug builds.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_no_labeled_leaf_mate_overlap(&self, vid: VertexId) {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let mut spans: Vec<(VertexId, u64, u64)> = Vec::new();
        for vid_u in start_vid..end_vid {
            let other = VertexId::from(vid_u);
            let vertex = self.vertices.get(other);
            if vertex.stored_slots == 0 {
                continue;
            }
            if vertex.is_default_edge_labeled() {
                let base = vertex.base_slot_start();
                let Some(end) = checked_add_slot_index(base, u64::from(vertex.stored_slots)) else {
                    continue;
                };
                spans.push((other, base, end));
                continue;
            }
            let Ok(buckets) = self.read_vertex_label_buckets(&vertex) else {
                continue;
            };
            // ADR 0096 §5: same first-non-tiny rule (see above); spanless
            // vertices contribute nothing to overlap detection.
            let Some(first) = buckets.iter().find(|bucket| !bucket.is_tiny_mode()) else {
                continue;
            };
            let base = first.edge_start();
            let Some(end) = checked_add_slot_index(base, u64::from(vertex.stored_slots)) else {
                continue;
            };
            spans.push((other, base, end));
        }
        spans.sort_by_key(|(_, base, _)| *base);
        for win in spans.windows(2) {
            let (a, _a_base, a_end) = win[0];
            let (b, b_base, _b_end) = win[1];
            assert!(
                a_end <= b_base,
                "leaf-mate edge spans overlap in leaf {leaf}: vid {a:?} reserves up to {a_end} but vid {b:?} starts at {b_base}"
            );
        }
    }

    pub(crate) fn assert_labeled_buckets_within_leaf_physical(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let (leaf_start, leaf_len) = match self.labeled_leaf_physical_range(vid)? {
            Some(range) => range,
            // Unpinned leaf: spanless (tiny-only) vertices are vacuously
            // within; slab spans without a pin are a genuine violation.
            None => {
                let buckets = self
                    .read_vertex_label_buckets(&vertex)
                    .map_err(|_| LaraOperationError::CollectAllocationOverflow)?;
                if buckets.iter().all(|bucket| bucket.is_tiny_mode()) {
                    return Ok(());
                }
                return Err(LaraOperationError::CollectAllocationOverflow.into());
            }
        };
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        let base = vertex.base_slot_start();
        for offset in 0..u64::from(vertex.degree()) {
            let slot = checked_add_slot_index(base, offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            // ADR 0096 §5: tiny buckets hold no slab bytes; there is no span
            // to contain. (An unpinned leaf with only tiny buckets returns
            // Ok below — promotion/maintenance pins it once spans exist.)
            if bucket.is_tiny_mode() {
                continue;
            }
            let bucket_end = Self::labeled_bucket_edge_end_exclusive(&bucket)?;
            assert!(
                bucket.edge_start() >= leaf_start && bucket_end <= leaf_end,
                "bucket edge range [{}, {bucket_end}) must lie in leaf physical [{leaf_start}, {leaf_end})",
                bucket.edge_start()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::{
        labeled::{bucket_label_key::BucketLabelKey, record::LabeledVertex},
        lara::edge::span_meta::SPAN_PHYSICAL_UNASSIGNED,
    };
    use std::ops::ControlFlow;

    fn hub_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
        LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            crate::labeled::InitialCapacities::uniform(1 << 20),
            BucketLabelKey::from_raw(1),
        )
        .unwrap()
    }

    #[test]
    fn labeled_span_meta_assigned_on_first_leaf_edge_write() {
        // ADR 0096 §5: tiny birth writes no slab bytes, so the first tiny
        // edge leaves the leaf unpinned; relocate pins it once spans exist.
        // (Pre-tiny, the first edge write pinned via quota reservation.)
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let leaf = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::leaf_index_for_vid(
            hub,
            graph.edges().header().segment_size,
        );
        assert_eq!(
            graph
                .edges()
                .span_meta_store()
                .get(u64::from(leaf))
                .physical_start,
            SPAN_PHYSICAL_UNASSIGNED
        );
        for target in [u32::from(dst), u32::from(dst) + 1, u32::from(dst) + 2] {
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    BucketLabelKey::from_raw(10),
                    TestEdge { target },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        assert_eq!(
            graph
                .edges()
                .span_meta_store()
                .get(u64::from(leaf))
                .physical_start,
            SPAN_PHYSICAL_UNASSIGNED,
            "tiny inserts must not pin the leaf"
        );
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                BucketLabelKey::from_raw(10),
                TestEdge {
                    target: u32::from(dst) + 3,
                },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        graph
            .relocate_labeled_leaf_physical_block(hub)
            .expect("relocate pins the leaf");
        assert_ne!(
            graph
                .edges()
                .span_meta_store()
                .get(u64::from(leaf))
                .physical_start,
            SPAN_PHYSICAL_UNASSIGNED
        );
    }

    #[test]
    fn labeled_leaf_vertices_share_span_meta_physical_start() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let neighbor = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label_a = BucketLabelKey::from_raw(10);
        let label_b = BucketLabelKey::from_raw(11);
        // ADR 0096 §5: promote past tiny (4 edges per bucket) so spans exist,
        // then relocate to pin (tiny birth pins nothing).
        for (vid, label) in [(hub, label_a), (neighbor, label_b)] {
            for target in [10u32, 11, 12, 13] {
                graph
                    .insert_edge_skip_leaf_cascade(
                        vid,
                        label,
                        TestEdge { target },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }
        graph
            .relocate_labeled_leaf_physical_block(hub)
            .expect("relocate pins the leaf");
        let (leaf_start, leaf_len) = graph
            .labeled_leaf_physical_range(hub)
            .unwrap()
            .expect("leaf pinned after labeled insert");
        assert_eq!(
            graph.labeled_leaf_physical_range(neighbor).unwrap(),
            Some((leaf_start, leaf_len))
        );
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
        graph
            .assert_labeled_buckets_within_leaf_physical(neighbor)
            .unwrap();
    }

    #[test]
    fn labeled_leaf_physical_block_covers_all_label_buckets() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let mut dsts = Vec::new();
        for _ in 0..4 {
            dsts.push(graph.push_vertex(LabeledVertex::default()).unwrap());
        }
        // ADR 0096 §5: promote each bucket past tiny (spans exist), pin via
        // relocate, then every slab span must lie in the pinned block.
        for label_idx in 0..4u16 {
            let label = BucketLabelKey::from_raw(20_000 + label_idx);
            for dst in &dsts {
                graph
                    .insert_edge_skip_leaf_cascade(
                        hub,
                        label,
                        TestEdge {
                            target: u32::from(*dst),
                        },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }
        graph
            .relocate_labeled_leaf_physical_block(hub)
            .expect("relocate pins the leaf");
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
    }

    #[test]
    fn labeled_reopen_preserves_leaf_physical_pin() {
        let vertices = mem();
        let buckets = mem();
        let bucket_free_spans = mem();
        let bucket_free_span_by_start = mem();
        let edge_counts = mem();
        let edges = mem();
        let edge_log = mem();
        let edge_span_meta = mem();
        let edge_free_spans = mem();
        let edge_free_span_by_start = mem();
        let inline_property_bytes_slab = mem();
        let value_free_spans = mem();
        let value_free_span_by_start = mem();
        let inline_property_bytes_log = mem();
        let value_blobs = mem();
        // Allocate the LTB memory once and reuse the same `VectorMemory`
        // across `new` and `init`; otherwise the LTB reopen path sees
        // an empty memory and fails with `Ltb(TruncatedHeader)`.
        let ltb = mem();
        let default_label = BucketLabelKey::from_raw(1);
        let elem_capacity = 1 << 20;

        let graph = LabeledLaraGraph::new(
            vertices.clone(),
            buckets.clone(),
            bucket_free_spans.clone(),
            bucket_free_span_by_start.clone(),
            edge_counts.clone(),
            edges.clone(),
            edge_log.clone(),
            edge_span_meta.clone(),
            edge_free_spans.clone(),
            edge_free_span_by_start.clone(),
            inline_property_bytes_slab.clone(),
            value_free_spans.clone(),
            value_free_span_by_start.clone(),
            inline_property_bytes_log.clone(),
            value_blobs.clone(),
            ltb.clone(),
            crate::labeled::InitialCapacities::uniform(elem_capacity),
            default_label,
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        // ADR 0096 §5: promote past tiny (spans exist), pin via relocate
        // (tiny birth pins nothing), then the pin must survive reopen.
        let mut dsts = Vec::new();
        for _ in 0..4 {
            let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
            dsts.push(u32::from(dst));
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    BucketLabelKey::from_raw(42),
                    TestEdge {
                        target: u32::from(dst),
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .relocate_labeled_leaf_physical_block(hub)
            .expect("relocate pins the leaf");
        let (pin_start, pin_len) = graph.labeled_leaf_physical_range(hub).unwrap().unwrap();

        let reopened = LabeledLaraGraph::init(
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            inline_property_bytes_slab,
            value_free_spans,
            value_free_span_by_start,
            inline_property_bytes_log,
            value_blobs,
            ltb,
            crate::labeled::InitialCapacities::uniform(elem_capacity),
            default_label,
        )
        .unwrap();
        assert_eq!(
            reopened.labeled_leaf_physical_range(hub).unwrap(),
            Some((pin_start, pin_len))
        );
        reopened
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
        let mut edges_out: Vec<TestEdge> = Vec::new();
        reopened
            .visit_edges(
                hub,
                BucketLabelKey::from_raw(42),
                OutEdgeOrder::Descending,
                |_slot, e| {
                    edges_out.push(e);
                    ControlFlow::<()>::Continue(())
                },
            )
            .map(|_| ())
            .unwrap();
        assert_eq!(edges_out.len(), 4);
        let mut targets: Vec<u32> = edges_out.iter().map(|e| e.target).collect();
        targets.sort_unstable();
        dsts.sort_unstable();
        assert_eq!(targets, dsts);
    }

    #[test]
    fn leaf_mate_overlap_guard_passes_for_normal_inserts() {
        let graph = hub_graph();
        for _ in 0..3 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let label = BucketLabelKey::from_raw(7);
        for vid_u in 0..3u32 {
            for t in 0..40u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        VertexId::from(vid_u),
                        label,
                        TestEdge { target: t },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
            // Settled per vertex: leaf-mates must never reserve overlapping spans.
            graph.assert_no_labeled_leaf_mate_overlap(VertexId::from(vid_u));
        }
    }

    #[test]
    fn leaf_mate_overlap_guard_detects_injected_overlap() {
        let graph = hub_graph();
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let label = BucketLabelKey::from_raw(7);
        // ADR 0096 §5/§3b: four inline inserts plus an explicit promotion so
        // both vertices own real slab spans (corrupting a tiny anchor is
        // meaningless — the guard skips spanless buckets by construction, and
        // K=4 keeps four edges inline).
        for vid_u in 0..2u32 {
            for target in [1u32, 2, 3, 4] {
                graph
                    .insert_edge_skip_leaf_cascade(
                        VertexId::from(vid_u),
                        label,
                        TestEdge { target },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
            crate::labeled::graph::test_support::promote_bucket_to_slab(
                &graph,
                VertexId::from(vid_u),
                label,
            );
            graph
                .compact_vertex_edge_span(VertexId::from(vid_u), 0)
                .unwrap();
        }
        graph.assert_no_labeled_leaf_mate_overlap(VertexId::from(0));

        // Corrupt vid 1's reservation to start on top of vid 0's span, then confirm the
        // guard fires. This simulates the data-loss bug class the guard exists to catch.
        let v0 = graph.vertices().get(VertexId::from(0));
        let base0 = graph
            .read_vertex_label_buckets(&v0)
            .unwrap()
            .first()
            .unwrap()
            .edge_start();
        let v1 = graph.vertices().get(VertexId::from(1));
        let slot1 = v1.base_slot_start();
        let bucket1 = graph.buckets().read_label_bucket_slot(slot1).unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(slot1, bucket1.with_edge_range(base0, bucket1.degree()))
            .unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            graph.assert_no_labeled_leaf_mate_overlap(VertexId::from(0));
        }));
        assert!(
            result.is_err(),
            "guard must panic on injected leaf-mate overlap"
        );
    }

    /// Upgrade-boundary corruption guard for the labeled engine (highest-risk
    /// relocation surface): drive multi-vertex, multi-label churn that forces
    /// overflow-log folds, leaf rebalances, and leaf/bucket relocations; reopen
    /// the same stable memories (as `post_upgrade` does); and require every
    /// per-label adjacency plus every structural invariant to survive — then
    /// mutate again to prove the reopened graph is fully operational.

    #[test]
    fn labeled_heavy_relocation_survives_reopen_without_corruption() {
        const VERTICES: u32 = 3;
        const LABELS: u16 = 4;
        const PER_LABEL: u32 = 20;
        let default_label = BucketLabelKey::from_raw(1);
        let elem_capacity = 1u64 << 20;

        let mems = labeled_memories();
        let graph = open_labeled_graph(&mems, elem_capacity, default_label);
        let mut ids = Vec::new();
        for _ in 0..VERTICES {
            ids.push(graph.push_vertex(LabeledVertex::default()).unwrap());
        }

        // Model: per vertex, per label, an ascending set of unique targets.
        let labels: Vec<BucketLabelKey> = (0..LABELS)
            .map(|l| BucketLabelKey::from_raw(10_000 + l))
            .collect();
        let mut expected: Vec<Vec<(BucketLabelKey, Vec<u32>)>> =
            vec![Vec::new(); VERTICES as usize];
        for (vi, &vid) in ids.iter().enumerate() {
            for (li, &label) in labels.iter().enumerate() {
                let mut targets = Vec::new();
                for i in 0..PER_LABEL {
                    let target = (vi as u32) * 100_000 + (li as u32) * 1_000 + i;
                    graph
                        .insert_edge(
                            vid,
                            label,
                            TestEdge { target },
                            crate::labeled::graph::EdgePlacementPolicy::Insertion,
                        )
                        .unwrap_or_else(|e| panic!("insert v{vi} l{li} i{i}: {e:?}"));
                    targets.push(target);
                }
                expected[vi].push((label, targets));
            }
        }
        let check = |g: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
                     expected: &[Vec<(BucketLabelKey, Vec<u32>)>],
                     phase: &str| {
            crate::labeled::invariants::assert_labeled_layout_invariants(
                g.vertices(),
                g.buckets(),
                g.edges(),
            );
            crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
                g.vertices(),
                g.buckets(),
                g.edges(),
            );
            for (vi, &vid) in ids.iter().enumerate() {
                g.assert_no_labeled_leaf_mate_overlap(vid);
                g.assert_labeled_buckets_within_leaf_physical(vid).unwrap();
                for (label, want) in &expected[vi] {
                    let mut got: Vec<u32> = g
                        .iter_edges_for_label(vid, *label)
                        .unwrap()
                        .into_iter()
                        .map(|e| e.target)
                        .collect();
                    got.sort_unstable();
                    let mut want = want.clone();
                    want.sort_unstable();
                    assert_eq!(
                        got, want,
                        "{phase}: v{vi} label {label:?} adjacency diverged"
                    );
                }
            }
        };
        check(&graph, &expected, "pre-reopen");

        // Cross the upgrade boundary: drop the in-memory graph, reopen the bytes.
        drop(graph);
        let reopened = reopen_labeled_graph(&mems, elem_capacity, default_label);
        check(&reopened, &expected, "post-reopen");

        // Continued inserts after reopen must relocate without corrupting state.
        for (vi, &vid) in ids.iter().enumerate() {
            for (li, &label) in labels.iter().enumerate() {
                let target = (vi as u32) * 100_000 + (li as u32) * 1_000 + PER_LABEL;
                reopened
                    .insert_edge(
                        vid,
                        label,
                        TestEdge { target },
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
                expected[vi][li].1.push(target);
            }
        }
        check(&reopened, &expected, "post-reopen-mutation");
    }

    #[test]
    fn reserve_grows_the_block_at_the_slab_end() {
        let graph = hub_graph();
        let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
        // A freshly pinned block sits at the slab end; one resident covers it.
        let (start, block_len) = graph.ensure_labeled_leaf_block_pinned(owner).unwrap();
        graph.vertices.set(
            owner,
            &LabeledVertex::from_parts(start, block_len as u32, block_len as u32, 0)
                .with_default_edge_labeled(true)
                .with_degree(block_len as u32)
                .with_stored_slots(block_len as u32),
        );

        let base =
            LeafArena::reserve(&graph, owner, 4, SourceRegion::new(start, block_len)).unwrap();

        assert_eq!(
            base,
            start + block_len,
            "the block grows by appending capacity"
        );
        assert_eq!(
            graph.leaf_segment_counts_for_vid(owner).total,
            i64::try_from(block_len + 4).unwrap()
        );
        assert_eq!(
            LeafArena::capture(&graph, owner).unwrap().unwrap().extent(),
            (start, block_len + 4)
        );
    }

    #[test]
    fn reserve_grows_the_block_into_a_free_tail() {
        let graph = hub_graph();
        let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
        // Pin the leaf through the production path, then let one resident cover the
        // whole block so placement inside it has no run to hand out.
        let (start, block_len) = graph.ensure_labeled_leaf_block_pinned(owner).unwrap();
        graph.vertices.set(
            owner,
            &LabeledVertex::from_parts(start, block_len as u32, block_len as u32, 0)
                .with_default_edge_labeled(true)
                .with_degree(block_len as u32)
                .with_stored_slots(block_len as u32),
        );
        // The store appends at `elem_capacity`, so the only free run is one that was
        // released: take four slots after the block and give them back.
        let tail = graph.edges.allocate_span(4).unwrap();
        assert_eq!(tail, start + block_len);
        graph.edges.release_span(tail, 4).unwrap();

        let base =
            LeafArena::reserve(&graph, owner, 4, SourceRegion::new(start, block_len)).unwrap();

        assert_eq!(base, tail, "the span grows into the block's freed tail");
        assert_eq!(
            graph.leaf_segment_counts_for_vid(owner).total,
            i64::try_from(block_len + 4).unwrap(),
            "the block absorbs the growth instead of the leaf losing the span"
        );
    }

    #[test]
    fn labeled_leaf_pma_density_matches_counts_store_when_pinned() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                BucketLabelKey::from_raw(10),
                TestEdge {
                    target: u32::from(dst),
                },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        let counts = graph.leaf_segment_counts_for_vid(hub);
        let density = graph.labeled_leaf_pma_density(hub);
        assert_eq!(
            density,
            counts.actual.max(0) as f64 / counts.total.max(1) as f64
        );
    }
}
