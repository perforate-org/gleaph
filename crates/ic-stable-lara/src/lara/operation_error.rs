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
    /// Log replay ended before enough edges were read.
    LogChainShort,
    /// `degree * E::BYTES` overflowed when sizing a collect buffer.
    CollectAllocationOverflow,
    /// A CSR row degree reached the representable `u32` limit.
    RowDegreeOverflow,
    /// Unordered edge removal needs a slab-only row.
    RemoveRequiresSlabOnlyRow,
    /// Direct slab edge reads need a slab-only row.
    RowEdgeReadRequiresSlabOnlyRow,
    /// Clearing a row slab needs a slab-only row.
    ClearRowRequiresSlabOnlyRow,
    /// Overflow log has no free slot for another entry.
    SegmentLogFull,
    /// PMA segment-count vector shorter than tree height requires.
    SegmentCountsTreeTooSmall,
    /// Leaf segment counts lookup could not be performed (internal geometry).
    SegmentCountsOutOfRange,
    /// [`VertexId`] does not fit host log `i32` encoding.
    VertexIdExceedsI32,
    /// Stable memory grow/write failed while writing an edge slab slot.
    WriteEdgeSlotFailed(GrowFailed),
    /// Stable memory grow/write failed while writing the overflow log.
    WriteLogFailed(GrowFailed),
    /// [`crate::LaraGraph`] rebalance could not grow stable memory.
    RebalanceFailed(GrowFailed),
    /// [`crate::LaraGraph`] resize could not grow stable memory.
    ResizeFailed(GrowFailed),
    /// After a forward remove, the reverse orientation reported no matching edge (invariant break).
    DirectedRemoveOrientationMismatch,
    /// After removing one half of an undirected edge, the opposite orientation reported no matching edge.
    UndirectedRemoveOrientationMismatch,
}

impl fmt::Display for LaraOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexAccess(e) => write!(f, "{e}"),
            Self::VertexDeleted => write!(f, "vertex deleted"),
            Self::LogChainShort => write!(f, "log chain short"),
            Self::CollectAllocationOverflow => write!(f, "collect overflow"),
            Self::RowDegreeOverflow => write!(f, "row degree overflow"),
            Self::RemoveRequiresSlabOnlyRow => write!(f, "remove requires slab-only row"),
            Self::RowEdgeReadRequiresSlabOnlyRow => {
                write!(f, "row edge read requires slab-only row")
            }
            Self::ClearRowRequiresSlabOnlyRow => write!(f, "clear row requires slab-only row"),
            Self::SegmentLogFull => write!(f, "segment log full"),
            Self::SegmentCountsTreeTooSmall => write!(f, "segment counts tree too small"),
            Self::SegmentCountsOutOfRange => write!(f, "segment counts out of range"),
            Self::VertexIdExceedsI32 => write!(f, "vertex id exceeds i32"),
            Self::WriteEdgeSlotFailed(e) => write!(f, "write edge slot failed: {e}"),
            Self::WriteLogFailed(e) => write!(f, "write log failed: {e}"),
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
            | Self::WriteLogFailed(e)
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

    /// Graph vertex used for per-leaf overflow log addressing and PMA bumps.
    ///
    /// Synthetic accessors (for example [`crate::labeled::access::LabelEdgeSpanAccess`])
    /// return the real source [`VertexId`] here while [`Self::get`] / [`Self::set`]
    /// continue to use their logical row id (often `VertexId::from(0)`).
    #[inline]
    fn log_leaf_vertex(&self, id: VertexId) -> VertexId {
        id
    }

    fn get_in_range(&self, id: VertexId) -> Result<V, VertexAccessError> {
        if u32::from(id) >= self.len() {
            return Err(VertexAccessError::OutOfRange);
        }
        Ok(self.get(id))
    }
}
