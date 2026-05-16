//! Bidirectional labeled LARA graph wrappers.

pub(crate) mod deferred;

use crate::{
    VertexCount, VertexId,
    labeled::{BucketLabelKey, graph::LabeledLaraGraph},
    traits::CsrEdge,
};
use ic_stable_structures::Memory;
use std::fmt;

pub use deferred::{
    BidirectionalMaintenanceReport as LabeledBidirectionalMaintenanceReport,
    DeferredBidirectionalLabeledError, DeferredBidirectionalLabeledLaraGraph, Orientation,
};

/// Errors returned by bidirectional labeled graph operations.
#[derive(Debug)]
pub enum BidirectionalLabeledError {
    /// Forward orientation failed.
    Forward(crate::labeled::graph::LabeledOperationError),
    /// Reverse orientation failed.
    Reverse(crate::labeled::graph::LabeledOperationError),
    /// Stable memory grow or format initialization failed.
    Grow(crate::GrowFailed),
    /// The two orientations do not contain the same number of vertex rows.
    VertexCountMismatch {
        /// Forward vertex count.
        forward: VertexCount,
        /// Reverse vertex count.
        reverse: VertexCount,
    },
    /// Addressing a vertex outside `0..vertex_count`.
    VertexOutOfRange {
        /// Requested vertex id.
        vid: VertexId,
        /// Current vertex column length.
        len: VertexCount,
    },
}

impl fmt::Display for BidirectionalLabeledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(err) => write!(f, "forward store: {err}"),
            Self::Reverse(err) => write!(f, "reverse store: {err}"),
            Self::Grow(err) => write!(f, "format / grow: {err}"),
            Self::VertexCountMismatch { forward, reverse } => write!(
                f,
                "vertex column length mismatch: forward={forward} reverse={reverse}"
            ),
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
        }
    }
}

impl std::error::Error for BidirectionalLabeledError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Forward(err) | Self::Reverse(err) => Some(err),
            Self::Grow(err) => Some(err),
            Self::VertexCountMismatch { .. } | Self::VertexOutOfRange { .. } => None,
        }
    }
}

impl From<crate::GrowFailed> for BidirectionalLabeledError {
    fn from(value: crate::GrowFailed) -> Self {
        Self::Grow(value)
    }
}

/// Two synchronized labeled CSR stores: forward out-adjacency and reverse in-adjacency.
pub struct BidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    forward: LabeledLaraGraph<E, M>,
    reverse: LabeledLaraGraph<E, M>,
}

impl<E, M> BidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Creates fresh forward and reverse labeled stores over the supplied memories.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        forward_vertices: M,
        forward_buckets: M,
        forward_bucket_free_spans: M,
        forward_bucket_free_span_by_start: M,
        forward_edge_counts: M,
        forward_edges: M,
        forward_edge_log: M,
        forward_edge_span_meta: M,
        forward_edge_free_spans: M,
        forward_edge_free_span_by_start: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_bucket_free_spans: M,
        reverse_bucket_free_span_by_start: M,
        reverse_edge_counts: M,
        reverse_edges: M,
        reverse_edge_log: M,
        reverse_edge_span_meta: M,
        reverse_edge_free_spans: M,
        reverse_edge_free_span_by_start: M,
        elem_capacity: u64,
        default_label: BucketLabelKey,
    ) -> Result<Self, BidirectionalLabeledError> {
        let forward = LabeledLaraGraph::new(
            forward_vertices,
            forward_buckets,
            forward_bucket_free_spans,
            forward_bucket_free_span_by_start,
            forward_edge_counts,
            forward_edges,
            forward_edge_log,
            forward_edge_span_meta,
            forward_edge_free_spans,
            forward_edge_free_span_by_start,
            elem_capacity,
            default_label,
        )?;
        let reverse = LabeledLaraGraph::new(
            reverse_vertices,
            reverse_buckets,
            reverse_bucket_free_spans,
            reverse_bucket_free_span_by_start,
            reverse_edge_counts,
            reverse_edges,
            reverse_edge_log,
            reverse_edge_span_meta,
            reverse_edge_free_spans,
            reverse_edge_free_span_by_start,
            elem_capacity,
            default_label,
        )?;
        Ok(Self { forward, reverse })
    }

    /// Returns the forward out-adjacency orientation.
    pub fn forward(&self) -> &LabeledLaraGraph<E, M> {
        &self.forward
    }

    /// Returns the reverse in-adjacency orientation.
    pub fn reverse(&self) -> &LabeledLaraGraph<E, M> {
        &self.reverse
    }

    /// Returns the shared vertex count after checking both orientations agree.
    pub fn vertex_count(&self) -> Result<VertexCount, BidirectionalLabeledError> {
        let forward = self.forward.vertex_count();
        let reverse = self.reverse.vertex_count();
        if forward != reverse {
            return Err(BidirectionalLabeledError::VertexCountMismatch { forward, reverse });
        }
        Ok(forward)
    }

    /// Appends one vertex row to both orientations.
    pub fn push_vertex(&self) -> Result<VertexId, BidirectionalLabeledError> {
        let _ = self.vertex_count()?;
        self.forward
            .push_vertex(crate::labeled::record::LabeledVertex::default())?;
        self.reverse
            .push_vertex(crate::labeled::record::LabeledVertex::default())?;
        Ok(VertexId::from(
            self.forward.vertex_count().0.saturating_sub(1),
        ))
    }

    /// Inserts one directed edge, writing `forward_edge` and `reverse_edge` into opposite orientations.
    pub fn insert_directed_edge(
        &self,
        src: VertexId,
        dst: VertexId,
        label_id: BucketLabelKey,
        forward_edge: E,
        reverse_edge: E,
    ) -> Result<(), BidirectionalLabeledError> {
        self.forward
            .insert_edge(src, label_id, forward_edge)
            .map_err(BidirectionalLabeledError::Forward)?;
        self.reverse
            .insert_edge(dst, label_id, reverse_edge)
            .map_err(BidirectionalLabeledError::Reverse)?;
        Ok(())
    }

    /// Iterates forward outgoing edges for one label.
    pub fn iter_out_edges_for_label(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Result<Vec<E>, BidirectionalLabeledError> {
        self.forward
            .iter_edges_for_label(src, label_id)
            .map_err(BidirectionalLabeledError::Forward)
    }

    /// Lazy iterator over all forward out-edges (every label). See [`LabeledLaraGraph::out_edges_iter`].
    pub fn forward_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>, BidirectionalLabeledError>
    {
        self.forward
            .out_edges_iter(src)
            .map_err(BidirectionalLabeledError::Forward)
    }

    /// Lazy iterator over all reverse out-edges at `dst` (incoming in forward orientation).
    pub fn reverse_out_edges_iter(
        &self,
        dst: VertexId,
    ) -> Result<crate::labeled::graph::LabeledOutEdgesIter<'_, E, M>, BidirectionalLabeledError>
    {
        self.reverse
            .out_edges_iter(dst)
            .map_err(BidirectionalLabeledError::Reverse)
    }
}
