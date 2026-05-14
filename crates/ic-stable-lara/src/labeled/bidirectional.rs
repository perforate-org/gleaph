//! Bidirectional labeled LARA graph wrappers.

use crate::{
    VertexCount, VertexId,
    labeled::{
        deferred::{DeferredError, DeferredLabeledLaraGraph},
        graph::{InitError, LabeledLaraGraph, LabeledOperationError},
        record::LabelId,
    },
    lara::maintenance::MaintenanceBudget,
    traits::CsrEdge,
};
use ic_stable_structures::Memory;
use std::fmt;

/// Errors returned by bidirectional labeled graph operations.
#[derive(Debug)]
pub enum BidirectionalLabeledError {
    Forward(LabeledOperationError),
    Reverse(LabeledOperationError),
    ForwardInit(InitError),
    ReverseInit(InitError),
    VertexCountMismatch {
        forward: VertexCount,
        reverse: VertexCount,
    },
    VertexOutOfRange {
        vid: VertexId,
        len: VertexCount,
    },
}

impl fmt::Display for BidirectionalLabeledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forward(err) => write!(f, "forward store: {err}"),
            Self::Reverse(err) => write!(f, "reverse store: {err}"),
            Self::ForwardInit(err) => write!(f, "forward init failed: {err}"),
            Self::ReverseInit(err) => write!(f, "reverse init failed: {err}"),
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

impl std::error::Error for BidirectionalLabeledError {}

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
    pub fn new(
        forward_vertices: M,
        forward_buckets: M,
        forward_edges: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_edges: M,
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, BidirectionalLabeledError> {
        let forward = LabeledLaraGraph::new(
            forward_vertices,
            forward_buckets,
            forward_edges,
            elem_capacity,
            default_label,
        )
        .map_err(|_| BidirectionalLabeledError::Forward(LabeledOperationError::EdgeSlabFull))?;
        let reverse = LabeledLaraGraph::new(
            reverse_vertices,
            reverse_buckets,
            reverse_edges,
            elem_capacity,
            default_label,
        )
        .map_err(|_| BidirectionalLabeledError::Reverse(LabeledOperationError::EdgeSlabFull))?;
        Ok(Self { forward, reverse })
    }

    pub fn forward(&self) -> &LabeledLaraGraph<E, M> {
        &self.forward
    }

    pub fn reverse(&self) -> &LabeledLaraGraph<E, M> {
        &self.reverse
    }

    pub fn vertex_count(&self) -> Result<VertexCount, BidirectionalLabeledError> {
        let forward = self.forward.vertex_count();
        let reverse = self.reverse.vertex_count();
        if forward != reverse {
            return Err(BidirectionalLabeledError::VertexCountMismatch { forward, reverse });
        }
        Ok(forward)
    }

    pub fn push_vertex(&self) -> Result<VertexId, BidirectionalLabeledError> {
        let _ = self.vertex_count()?;
        self.forward
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .map_err(|_| BidirectionalLabeledError::Forward(LabeledOperationError::EdgeSlabFull))?;
        self.reverse
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .map_err(|_| BidirectionalLabeledError::Reverse(LabeledOperationError::EdgeSlabFull))?;
        Ok(VertexId::from(
            self.forward.vertex_count().0.saturating_sub(1),
        ))
    }

    pub fn insert_directed_edge(
        &self,
        src: VertexId,
        dst: VertexId,
        label_id: LabelId,
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

    pub fn iter_out_edges_for_label(
        &self,
        src: VertexId,
        label_id: LabelId,
    ) -> Result<impl Iterator<Item = E> + '_, BidirectionalLabeledError> {
        self.forward
            .iter_edges_for_label(src, label_id)
            .map_err(BidirectionalLabeledError::Forward)
    }
}

/// Deferred-maintenance bidirectional labeled LARA graph wrapper.
pub struct DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    forward: DeferredLabeledLaraGraph<E, M>,
    reverse: DeferredLabeledLaraGraph<E, M>,
}

impl<E, M> DeferredBidirectionalLabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub fn new(
        forward_vertices: M,
        forward_buckets: M,
        forward_edges: M,
        forward_queue: M,
        reverse_vertices: M,
        reverse_buckets: M,
        reverse_edges: M,
        reverse_queue: M,
        elem_capacity: u64,
        default_label: LabelId,
    ) -> Result<Self, DeferredError> {
        let forward = DeferredLabeledLaraGraph::new(
            LabeledLaraGraph::new(
                forward_vertices,
                forward_buckets,
                forward_edges,
                elem_capacity,
                default_label,
            )
            .map_err(|_| DeferredError::Inner(LabeledOperationError::EdgeSlabFull))?,
            forward_queue,
        )?;
        let reverse = DeferredLabeledLaraGraph::new(
            LabeledLaraGraph::new(
                reverse_vertices,
                reverse_buckets,
                reverse_edges,
                elem_capacity,
                default_label,
            )
            .map_err(|_| DeferredError::Inner(LabeledOperationError::EdgeSlabFull))?,
            reverse_queue,
        )?;
        Ok(Self { forward, reverse })
    }

    pub fn forward(&self) -> &DeferredLabeledLaraGraph<E, M> {
        &self.forward
    }

    pub fn reverse(&self) -> &DeferredLabeledLaraGraph<E, M> {
        &self.reverse
    }

    pub fn maintenance(
        &self,
        budget: MaintenanceBudget,
    ) -> (
        crate::lara::maintenance::MaintenanceWorkReport,
        crate::lara::maintenance::MaintenanceWorkReport,
    ) {
        (
            self.forward.maintenance(budget),
            self.reverse.maintenance(budget),
        )
    }
}
