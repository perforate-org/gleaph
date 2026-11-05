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
        need: u64,
        avoid: (u64, u64),
    ) -> Result<Option<u64>, LabeledOperationError> {
        Ok(self.with_slice(|slice| slice.place(need, avoid))?.flatten())
    }

    /// Places a leaf-wide region (`need` slots) in the vertex's own space.
    pub(crate) fn place_leaf_wide(
        &self,
        need: u64,
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
        new_content: u64,
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
