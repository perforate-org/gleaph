//! Errors for LARA graph read/write paths and vertex-column addressing.

use crate::traits::CsrVertex;
use crate::{GrowFailed, VertexId};
use std::fmt;

/// Addressing a [`VertexId`] outside the vertex column length (`0 .. len`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexAccessError {
    /// The requested `VertexId` is outside `0 .. len`.
    OutOfRange,
}

impl fmt::Display for VertexAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => write!(f, "vertex out of range"),
        }
    }
}

impl std::error::Error for VertexAccessError {}

/// Failures from [`crate::LaraGraph`] / [`super::edge::EdgeStore`] operations (excluding slab init and `GrowFailed` on unrelated helpers).
#[derive(Debug)]
pub enum LaraOperationError {
    /// See [`VertexAccessError`].
    VertexAccess(VertexAccessError),
    /// Mutating API on a logically deleted vertex row.
    VertexDeleted,
    /// A spill replay yielded a different number of live edges than the row's degree (corrupt run
    /// length or degree).
    SpillReplayDegreeMismatch,
    /// The located delete target is not a live edge of its bucket.
    DeleteTargetNotLive,
    /// A spill row index is at or beyond the run's used length (fail-closed read/write).
    SpillRowOutOfRange,
    /// The spill arena cannot serve this bucket any more (class cap or arena limit): the caller
    /// promotes the bucket to tree mode (ADR 0097 §6).
    SpillNeedsPromotion,
    /// A complete read would examine more physical slots than the caller permits.
    ReadLimitExceeded,
    /// A complete value read exceeds the caller's admitted body-byte allowance.
    ReadByteLimitExceeded,
    /// The fail-closed "no room" answer for an operation that cannot proceed.
    ///
    /// The name promises one cause - a `degree * E::BYTES` collect buffer overflowing - but the variant is
    /// the generic refusal at a few hundred sites: a span that cannot be placed, a bucket row that must
    /// exist and does not, a release the allocator rejects. Construct it through [`Self::overflow`], which
    /// records the caller (and traps with it under `GLEAPH_TRACE_OVERFLOW=1`), so the reason is found where
    /// the refusal is raised rather than guessed from this name. A site whose refusal has a name of its own
    /// belongs in its own variant; this one is the shared floor.
    CollectAllocationOverflow,
    /// A CSR row degree reached the representable `u32` limit.
    RowDegreeOverflow,
    /// Unordered edge removal needs a slab-only row.
    RemoveRequiresSlabOnlyRow,
    /// Direct slab edge reads need a slab-only row.
    RowEdgeReadRequiresSlabOnlyRow,
    /// Clearing a row slab needs a slab-only row.
    ClearRowRequiresSlabOnlyRow,

    /// PMA segment-count vector shorter than tree height requires.
    SegmentCountsTreeTooSmall,
    /// Leaf segment counts lookup could not be performed (internal geometry).
    SegmentCountsOutOfRange,
    /// Stable memory grow/write failed while writing an edge slab slot.
    WriteEdgeSlotFailed(GrowFailed),
    /// Stable memory grow/write failed while writing a spill run.
    WriteSpillFailed(GrowFailed),
    /// [`crate::LaraGraph`] rebalance could not grow stable memory.
    RebalanceFailed(GrowFailed),
    /// [`crate::LaraGraph`] resize could not grow stable memory.
    ResizeFailed(GrowFailed),
    /// After a forward remove, the reverse orientation reported no matching edge (invariant break).
    DirectedRemoveOrientationMismatch,
    /// After removing one half of an undirected edge, the opposite orientation reported no matching edge.
    UndirectedRemoveOrientationMismatch,
}

impl LaraOperationError {
    /// The fail-closed "no room" answer.
    ///
    /// This is one of the most-reached errors in the subsystem and was spelled out as a bare variant at
    /// roughly three hundred sites, which made the one question that matters - *where did it come from* -
    /// unanswerable. Every construction goes through here and records its caller, so a failing run can be
    /// asked which constructions preceded the error (`GLEAPH_TRACE_OVERFLOW=1` panics instead).
    #[track_caller]
    pub(crate) fn overflow() -> Self {
        let site = std::panic::Location::caller();
        GLEAPH_OVERFLOW_SITES.with(|sites| {
            let mut sites = sites.borrow_mut();
            if sites.len() < 64 {
                sites.push(site);
            }
        });
        if std::env::var_os("GLEAPH_TRACE_OVERFLOW").is_some() {
            panic!("CollectAllocationOverflow constructed at {site:?}");
        }
        Self::CollectAllocationOverflow
    }
}

thread_local! {
    /// Call sites of the `overflow()` constructions seen on this thread, oldest first.
    static GLEAPH_OVERFLOW_SITES: std::cell::RefCell<Vec<&'static std::panic::Location<'static>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The `overflow()` call sites recorded on this thread, oldest first (at most 64).
pub fn overflow_sites() -> Vec<&'static std::panic::Location<'static>> {
    GLEAPH_OVERFLOW_SITES.with(|sites| sites.borrow().clone())
}

impl fmt::Display for LaraOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexAccess(e) => write!(f, "{e}"),
            Self::VertexDeleted => write!(f, "vertex deleted"),
            Self::SpillReplayDegreeMismatch => write!(f, "spill replay does not match row degree"),
            Self::DeleteTargetNotLive => write!(f, "delete target is not a live edge"),
            Self::SpillRowOutOfRange => {
                write!(f, "spill row index is beyond the run's used length")
            }
            Self::SpillNeedsPromotion => {
                write!(f, "spill arena exhausted: promote the bucket to tree mode")
            }
            Self::ReadLimitExceeded => write!(f, "physical edge read limit exceeded"),
            Self::ReadByteLimitExceeded => write!(f, "property body read byte limit exceeded"),
            Self::CollectAllocationOverflow => write!(f, "collect overflow"),
            Self::RowDegreeOverflow => write!(f, "row degree overflow"),
            Self::RemoveRequiresSlabOnlyRow => write!(f, "remove requires slab-only row"),
            Self::RowEdgeReadRequiresSlabOnlyRow => {
                write!(f, "row edge read requires slab-only row")
            }
            Self::ClearRowRequiresSlabOnlyRow => write!(f, "clear row requires slab-only row"),
            Self::SegmentCountsTreeTooSmall => write!(f, "segment counts tree too small"),
            Self::SegmentCountsOutOfRange => write!(f, "segment counts out of range"),
            Self::WriteEdgeSlotFailed(e) => write!(f, "write edge slot failed: {e}"),
            Self::WriteSpillFailed(e) => write!(f, "write spill failed: {e}"),
            Self::RebalanceFailed(e) => write!(f, "rebalance failed: {e}"),
            Self::ResizeFailed(e) => write!(f, "resize failed: {e}"),
            Self::DirectedRemoveOrientationMismatch => {
                write!(f, "directed remove orientation mismatch")
            }
            Self::UndirectedRemoveOrientationMismatch => {
                write!(f, "undirected remove orientation mismatch")
            }
        }
    }
}

impl std::error::Error for LaraOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::VertexAccess(e) => Some(e),
            Self::WriteEdgeSlotFailed(e)
            | Self::WriteSpillFailed(e)
            | Self::RebalanceFailed(e)
            | Self::ResizeFailed(e) => Some(e),
            _ => None,
        }
    }
}

impl From<VertexAccessError> for LaraOperationError {
    fn from(value: VertexAccessError) -> Self {
        Self::VertexAccess(value)
    }
}

/// Vertex column access for [`super::edge::EdgeStore`].
pub(crate) trait VertexAccess<V: CsrVertex> {
    fn len(&self) -> u32;
    fn get(&self, id: VertexId) -> V;
    fn set(&self, id: VertexId, item: &V);

    /// Prepare an existing-row write without lazy encoding, offset calculation or grow.
    /// Accessors without that writer capability deliberately reject.
    fn prepare_set(
        &self,
        _: VertexId,
        _: &V,
    ) -> Result<super::vertex::PreparedVertexWrite<'_, V>, LaraOperationError>
    where
        Self: Sized,
    {
        Err(LaraOperationError::CollectAllocationOverflow)
    }

    /// Graph vertex whose PMA leaf counts an edge write bumps.
    ///
    /// Synthetic accessors (for example [`crate::labeled::access::LabelEdgeSpanAccess`])
    /// return the real source [`VertexId`] here while [`Self::get`] / [`Self::set`]
    /// continue to use their logical row id (often `VertexId::from(0)`).
    #[inline]
    fn count_leaf_vertex(&self, id: VertexId) -> VertexId {
        id
    }

    #[inline]
    fn get_in_range(&self, id: VertexId) -> Result<V, VertexAccessError> {
        if u32::from(id) >= self.len() {
            return Err(VertexAccessError::OutOfRange);
        }
        Ok(self.get(id))
    }
}
