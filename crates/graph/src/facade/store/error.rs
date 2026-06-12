//! Graph store error type and conversions.

use super::super::{PropertyCatalogError, VertexLabelStoreError, VertexPropertyStoreError};
use crate::index::placement;
use gleaph_graph_kernel::entry::EdgeLabelId;
use ic_stable_lara::{
    DeferredBidirectionalLabeledError, VertexId, labeled::BucketLabelKey as LaraLabelId,
};
use std::fmt;

#[derive(Debug)]
pub enum GraphStoreError {
    Graph(DeferredBidirectionalLabeledError),
    PropertyCatalog(PropertyCatalogError),
    VertexLabel(VertexLabelStoreError),
    PropertyValue(VertexPropertyStoreError),
    /// `DELETE` vertex without `DETACH` while the vertex still has incident edges.
    VertexNotDetached {
        vertex_id: VertexId,
    },
    /// No outgoing edge record matches the handle on the owner's forward row.
    EdgeNotFound {
        owner_vertex_id: VertexId,
        label_id: LaraLabelId,
        slot_index: u32,
    },
    /// Edge label id is outside the inline edge band `0x0001..=0x3FFF`.
    InvalidEdgeLabelId(EdgeLabelId),
    /// Edge payload byte width is not supported by labeled edge-payload storage.
    InvalidEdgePayloadWidth(usize),
    /// Stored edge-payload bytes do not match the catalog label's configured width.
    EdgePayloadWidthMismatch {
        label: Option<EdgeLabelId>,
        expected: usize,
        actual: usize,
    },
    /// Remote CSR edge endpoints are not supported without federation stable.
    RemoteEdgeNotSupported,
    /// Federated expand returned or attempted to send invalid edge-payload bytes.
    FederatedExpandPayload {
        detail: String,
    },
    VertexPlacement(placement::VertexPlacementError),
    /// Shard-local CSR row is tombstoned.
    VertexTombstoned,
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
            Self::PropertyCatalog(err) => write!(f, "{err}"),
            Self::VertexLabel(err) => write!(f, "{err}"),
            Self::PropertyValue(err) => write!(f, "{err}"),
            Self::VertexNotDetached { vertex_id } => write!(
                f,
                "cannot delete vertex {vertex_id:?} without DETACH while it still has incident edges"
            ),
            Self::EdgeNotFound {
                owner_vertex_id,
                label_id,
                slot_index,
            } => write!(
                f,
                "no edge record for owner {owner_vertex_id:?}, label {label_id:?}, slot {slot_index}"
            ),
            Self::InvalidEdgeLabelId(id) => write!(
                f,
                "edge label id {} is not a catalog edge label (MSB clear, non-zero)",
                id.raw()
            ),
            Self::InvalidEdgePayloadWidth(width) => {
                write!(f, "edge payload byte width {width} is not supported")
            }
            Self::EdgePayloadWidthMismatch {
                label,
                expected,
                actual,
            } => match label {
                Some(id) => write!(
                    f,
                    "edge label {} expects {expected} value bytes, got {actual}",
                    id.raw()
                ),
                None => write!(
                    f,
                    "unlabeled edges expect {expected} value bytes, got {actual}"
                ),
            },
            Self::RemoteEdgeNotSupported => {
                write!(f, "remote CSR edge endpoints are not supported")
            }
            Self::FederatedExpandPayload { detail } => {
                write!(f, "invalid federated expand edge payload: {detail}")
            }
            Self::VertexPlacement(err) => write!(f, "{err}"),
            Self::VertexTombstoned => write!(f, "vertex row is tombstoned on this shard"),
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(err) => Some(err),
            Self::PropertyCatalog(err) => Some(err),
            Self::VertexLabel(err) => Some(err),
            Self::PropertyValue(err) => Some(err),
            Self::VertexNotDetached { .. }
            | Self::EdgeNotFound { .. }
            | Self::InvalidEdgeLabelId(_)
            | Self::InvalidEdgePayloadWidth(_)
            | Self::EdgePayloadWidthMismatch { .. }
            | Self::RemoteEdgeNotSupported
            | Self::FederatedExpandPayload { .. }
            | Self::VertexPlacement(_)
            | Self::VertexTombstoned => None,
        }
    }
}

impl From<placement::VertexPlacementError> for GraphStoreError {
    fn from(value: placement::VertexPlacementError) -> Self {
        Self::VertexPlacement(value)
    }
}

impl From<DeferredBidirectionalLabeledError> for GraphStoreError {
    fn from(value: DeferredBidirectionalLabeledError) -> Self {
        Self::Graph(value)
    }
}

impl From<PropertyCatalogError> for GraphStoreError {
    fn from(value: PropertyCatalogError) -> Self {
        Self::PropertyCatalog(value)
    }
}

impl From<VertexLabelStoreError> for GraphStoreError {
    fn from(value: VertexLabelStoreError) -> Self {
        Self::VertexLabel(value)
    }
}

impl From<VertexPropertyStoreError> for GraphStoreError {
    fn from(value: VertexPropertyStoreError) -> Self {
        Self::PropertyValue(value)
    }
}
