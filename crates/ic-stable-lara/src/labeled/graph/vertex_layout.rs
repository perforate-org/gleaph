//! The space one vertex holds in its leaf, borrowed for one operation.
//!
//! ADR 0100 unit three. A vertex's space used to be reachable through `LeafSlice`, obtained from an
//! `Option<LeafArena>` the caller had to capture first, and it `Deref`-ed to that arena - so a handle naming
//! one vertex could also place regions anywhere in the leaf. One handle, two granularities.
//!
//! This view answers for `src` alone. The leaf capture is lazy on purpose: the vertex-level rewrites
//! (`compact_span`, `rewrite_span`) do not need the extent, so requiring a capture would force callers to
//! invent a leaf they do not use - and silently skip the operation when the capture is absent, which is the
//! bug this design avoids by making `has_leaf` an explicit question.
//!
//! The layers stay put: `LeafArena` is the leaf-level allocator, `FreeSpanStore` keeps the ledger, and a
//! bucket's own span is `BucketLayout`'s business.

use crate::{
    VertexId,
    labeled::graph::LabeledOperationError,
    traits::{CsrEdge, CsrEdgeTombstone},
};
use ic_stable_structures::Memory;

use super::EdgeSlotMove;
use super::LabeledLaraGraph;
use super::leaf_arena::{LeafArena, LeafSlice};

/// See the module documentation.
pub(crate) struct VertexLayout<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    graph: &'a LabeledLaraGraph<E, M>,
    src: VertexId,
}

impl<'a, E, M> VertexLayout<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) const fn new(graph: &'a LabeledLaraGraph<E, M>, src: VertexId) -> Self {
        Self { graph, src }
    }

    /// The vertex this view answers for.
    pub(crate) const fn src(&self) -> VertexId {
        self.src
    }

    /// Whether this vertex has a captured leaf range at all.
    ///
    /// A vertex described straight from a fixture has no leaf; placement operations answer `None` for it
    /// while the rewrites still apply. Making that a question rather than a skip is deliberate: a caller
    /// that needs placement has to know it cannot place.
    pub(crate) fn has_leaf(&self) -> Result<bool, LabeledOperationError> {
        Ok(LeafArena::capture(self.graph, self.src)?.is_some())
    }

    /// Runs `f` against the vertex's leaf slice, if it has one.
    fn with_slice<T>(
        &self,
        f: impl FnOnce(&LeafSlice<'_, '_, E, M>) -> T,
    ) -> Result<Option<T>, LabeledOperationError> {
        Ok(LeafArena::capture(self.graph, self.src)?.map(|arena| f(&arena.leaf_slice(self.src))))
    }

    /// The leaf this vertex lives in, when it has one.
    pub(crate) fn leaf(&self) -> Result<Option<u32>, LabeledOperationError> {
        Ok(LeafArena::capture(self.graph, self.src)?.map(|arena| arena.leaf()))
    }

    /// The regions this vertex's buckets occupy, as `(start, end)` pairs.
    pub(crate) fn regions(&self) -> Result<Vec<(u64, u64)>, LabeledOperationError> {
        Ok(self
            .with_slice(|slice| slice.regions())?
            .unwrap_or_default())
    }

    /// What this vertex's buckets actually wrote, per bucket.
    pub(crate) fn contents(&self) -> Result<Vec<(u64, u64)>, LabeledOperationError> {
        Ok(self
            .with_slice(|slice| slice.contents())?
            .unwrap_or_default())
    }

    /// The vertex's extent: the space its leaf grants it.
    pub(crate) fn extent(&self) -> Result<Option<(u64, u64)>, LabeledOperationError> {
        self.with_slice(|slice| slice.extent())
    }

    /// Places `need` slots in the vertex's own space, keeping clear of `avoid`.
    pub(crate) fn place(
        &self,
        need: super::span_size::ContentSlots,
        avoid: (u64, u64),
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(self.with_slice(|slice| slice.place(need, avoid))?.flatten())
    }

    /// Places a leaf-wide region (`need` slots) in the vertex's own space.
    pub(crate) fn place_leaf_wide(
        &self,
        need: super::span_size::ContentSlots,
        avoid: (u64, u64),
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(self
            .with_slice(|slice| slice.place_leaf_wide(need, avoid))?
            .flatten())
    }

    /// Grows the region starting at `source_start` to hold `new_content`.
    pub(crate) fn grow_region(
        &self,
        source_start: u64,
        new_content: super::span_size::ContentSlots,
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(self
            .with_slice(|slice| slice.grow_region(source_start, new_content))?
            .flatten())
    }

    /// Allocates `len` slots from the leaf's free space, keeping clear of `avoid`.
    pub(crate) fn allocate(
        &self,
        len: u64,
        avoid: Option<(u64, u64)>,
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(self
            .with_slice(|slice| slice.allocate(len, avoid))?
            .flatten())
    }

    /// Hands out a range: takes whatever the free-span store still owns inside it.
    pub(crate) fn claim_placement(
        &self,
        start: u64,
        need: u64,
    ) -> Result<(), LabeledOperationError> {
        self.with_slice(|slice| slice.claim_placement(start, need))
            .map(|_| ())
    }

    /// Re-lays this vertex's edge span: buckets keep their rows, the slack is redistributed.
    ///
    /// `preferred_bucket` names the bucket the caller is about to insert into so it receives
    /// `preferred_extra` before the proportional gap placement; `in_window_layout` pins the base for a
    /// caller that is moving the span in place.
    pub(crate) fn rewrite_span(
        &self,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        force_slack_grow: bool,
        in_window_layout: Option<(u64, u32)>,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.rewrite_vertex_edge_span_impl(
            self.src,
            preferred_bucket,
            preferred_extra,
            force_slack_grow,
            in_window_layout,
        )
    }

    /// [`Self::rewrite_span`] that also names which base-resolution policy the caller accepts.
    pub(crate) fn rewrite_span_with_policy(
        &self,
        preferred_bucket: Option<u32>,
        preferred_extra: u32,
        force_slack_grow: bool,
        in_window_layout: Option<(u64, u32)>,
        policy: super::compact::SpanResolutionPolicy,
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.rewrite_vertex_edge_span_with_policy_impl(
            self.src,
            preferred_bucket,
            preferred_extra,
            force_slack_grow,
            in_window_layout,
            policy,
        )
    }

    /// Whether `[start, start + len)` lies inside this vertex's leaf extent.
    ///
    /// A span that does is leaf space and must not be returned to the free-span store; one that is not
    /// is a free span and must be.
    pub(crate) fn extent_contains(&self, start: u64, len: u64) -> bool {
        match LeafArena::capture(self.graph, self.src) {
            Ok(Some(arena)) => arena.extent_contains(start, len),
            _ => false,
        }
    }

    /// Makes room for `len` more slots in this vertex's space, growing the extent or moving the leaf.
    ///
    /// `avoid` names the region the growth must not collide with - usually the bucket that is about to
    /// grow, which owns those slots already.
    pub(crate) fn ensure_room_for(
        &self,
        len: super::span_size::ContentSlots,
        avoid: (u64, u64),
    ) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        LeafArena::ensure_room(
            self.graph,
            self.src,
            len,
            super::leaf_arena::SourceRegion::new(avoid.0, avoid.1),
        )
    }

    /// The arena's own consistency check for this vertex, as human-readable errors.
    pub(crate) fn validate(&self) -> Vec<String> {
        match LeafArena::capture(self.graph, self.src) {
            Ok(Some(arena)) => arena.validate(),
            _ => Vec::new(),
        }
    }

    /// Containment errors for this vertex's tree roots, as human-readable errors.
    pub(crate) fn containment_errors(&self) -> Vec<String> {
        match LeafArena::capture(self.graph, self.src) {
            Ok(Some(arena)) => arena.containment_errors(),
            _ => Vec::new(),
        }
    }

    /// Reserves a span of `len` slots for this vertex, growing the extent or relocating the leaf when
    /// the current one cannot hold it.
    ///
    /// This is the tree plane's entry: a tree root must land in the vertex's own space, and the answer
    /// is a base address rather than a "try" - either the room exists afterwards or the operation fails.
    /// `source` names the region being replaced so a relocation moves it instead of copying a stale one.
    pub(crate) fn reserve_span(
        &self,
        len: u64,
        source: super::leaf_arena::SourceRegion,
    ) -> Result<u64, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        LeafArena::reserve_relocating(self.graph, self.src, len, source)
    }

    /// The parts of `[start, start + len)` that fall outside this vertex's leaf extent.
    ///
    /// A released span may extend past the extent, because the slice is derived from resident regions;
    /// only what lies outside is a free span, and returning the rest would overlap a live extent.
    pub(crate) fn parts_outside_extent(&self, start: u64, len: u64) -> Vec<(u64, u64)> {
        match LeafArena::capture(self.graph, self.src) {
            Ok(Some(arena)) => arena.parts_outside_extent(start, len),
            _ => vec![(start, len)],
        }
    }

    /// Packs this vertex's buckets together and reports the slab-slot moves it performed.
    ///
    /// The bucket index the old signature carried was ignored (`let _ = bucket_index;`), so the operation is
    /// vertex-scoped and says so.
    pub(crate) fn compact_span_with_moves(&self) -> Result<Vec<EdgeSlotMove>, LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.graph.compact_vertex_edge_span_with_moves(self.src, 0)
    }

    /// [`Self::compact_span_with_moves`], discarding the moves.
    pub(crate) fn compact_span(&self) -> Result<(), LabeledOperationError>
    where
        E: CsrEdgeTombstone,
    {
        self.compact_span_with_moves().map(|_| ())
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The layout of one vertex, borrowed for one operation.
    ///
    /// ADR 0100 rule 2: this is the only way to a vertex-scoped placement or rewrite.
    pub(crate) fn vertex_layout(&self, src: VertexId) -> VertexLayout<'_, E, M> {
        VertexLayout::new(self, src)
    }
}
