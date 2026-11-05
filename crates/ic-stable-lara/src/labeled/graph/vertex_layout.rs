//! The space one vertex holds in its leaf, borrowed for one operation.
//!
//! ADR 0100 unit three. A vertex's space used to be reachable through `LeafSlice`, which was obtained from
//! an `Option<LeafArena>` that callers had to capture first and which `Deref`-ed to the whole arena - so a
//! handle that names one vertex could also place regions anywhere in the leaf. This view is vertex-scoped
//! and nothing narrower or wider: it captures the defragmentated extent once and every operation answers
//! for `src` alone.
//!
//! The layers stay put: `LeafArena` is the leaf-level allocator, `FreeSpanStore` keeps the ledger, and a
//! bucket's own span is `BucketLayout`'s business. Vertex-level *rewrites* (re-laying the tiling, packing
//! buckets left) belong here and move in next; this cut only takes over the placement queries that today
//! reach the arena directly.

use crate::{VertexId, labeled::graph::LabeledOperationError, traits::CsrEdge};
use ic_stable_structures::Memory;

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
    arena: LeafArena<'a, E, M>,
}

impl<'a, E, M> VertexLayout<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) const fn new(
        graph: &'a LabeledLaraGraph<E, M>,
        src: VertexId,
        arena: LeafArena<'a, E, M>,
    ) -> Self {
        Self { graph, src, arena }
    }

    /// The vertex this view answers for.
    pub(crate) const fn src(&self) -> VertexId {
        self.src
    }

    fn slice(&self) -> LeafSlice<'_, '_, E, M> {
        self.arena.leaf_slice(self.src)
    }

    /// The leaf this vertex lives in.
    pub(crate) fn leaf(&self) -> u32 {
        self.arena.leaf()
    }

    /// The regions this vertex's buckets occupy, as `(start, end)` pairs.
    pub(crate) fn regions(&self) -> Vec<(u64, u64)> {
        self.slice().regions()
    }

    /// What this vertex's buckets actually wrote, per bucket.
    pub(crate) fn contents(&self) -> Vec<(u64, u64)> {
        self.slice().contents()
    }

    /// The vertex's extent: the space its leaf grants it.
    pub(crate) fn extent(&self) -> (u64, u64) {
        self.slice().extent()
    }

    /// Places `need` slots in the vertex's own space, keeping clear of `avoid`.
    pub(crate) fn place(&self, need: u64, avoid: (u64, u64)) -> Option<u64> {
        self.slice().place(need, avoid)
    }

    /// Places a leaf-wide region (`need` slots) in the vertex's own space.
    pub(crate) fn place_leaf_wide(&self, need: u64, avoid: (u64, u64)) -> Option<u64> {
        self.slice().place_leaf_wide(need, avoid)
    }

    /// Grows the region starting at `source_start` to hold `new_content`.
    pub(crate) fn grow_region(&self, source_start: u64, new_content: u64) -> Option<u64> {
        self.slice().grow_region(source_start, new_content)
    }

    /// Allocates `len` slots from the leaf's free space, keeping clear of `avoid`.
    pub(crate) fn allocate(&self, len: u64, avoid: Option<(u64, u64)>) -> Option<u64> {
        self.slice().allocate(len, avoid)
    }

    /// Hands out a range: takes whatever the free-span store still owns inside it.
    ///
    /// Kept on the view rather than on the arena so the claim happens at the same granularity as the
    /// placement that made it necessary.
    pub(crate) fn claim_placement(&self, start: u64, need: u64) {
        self.slice().claim_placement(start, need)
    }
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// The space `src` holds in its leaf, or `None` when the vertex has no captured range.
    ///
    /// ADR 0100 rule 2: this is the only way to a vertex-scoped placement.
    pub(crate) fn vertex_layout(
        &self,
        src: VertexId,
    ) -> Result<Option<VertexLayout<'_, E, M>>, LabeledOperationError> {
        Ok(LeafArena::capture(self, src)?.map(|arena| VertexLayout::new(self, src, arena)))
    }
}
