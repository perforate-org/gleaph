//! Graph store error type and conversions.

use super::super::stable::edge_label_catalog::EdgeLabelCatalogError;
use super::super::stable::edge_value_profiles::EdgeValueProfileStoreError;
use super::super::stable::edge_weight_profiles::EdgeWeightProfileStoreError;
use super::super::stable::vertex_label_catalog::VertexLabelCatalogError;
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
    VertexLabelCatalog(VertexLabelCatalogError),
    EdgeLabelCatalog(EdgeLabelCatalogError),
    EdgeValueProfile(EdgeValueProfileStoreError),
    EdgeWeightProfile(EdgeWeightProfileStoreError),
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
    /// Edge value byte width is not supported by labeled edge-value storage.
    InvalidEdgeValueWidth(usize),
    VertexPlacement(placement::VertexPlacementError),
    /// Router reports this shard-local vertex is frozen during migration.
    VertexMigrating,
    /// Shard-local CSR row is tombstoned (stale after migration).
    VertexTombstoned,
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
            Self::VertexLabelCatalog(err) => write!(f, "{err}"),
            Self::EdgeLabelCatalog(err) => write!(f, "{err}"),
            Self::EdgeValueProfile(err) => write!(f, "{err}"),
            Self::EdgeWeightProfile(err) => write!(f, "{err}"),
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
            Self::InvalidEdgeValueWidth(width) => {
                write!(f, "edge value byte width {width} is not supported")
            }
            Self::VertexPlacement(err) => write!(f, "{err}"),
            Self::VertexMigrating => write!(f, "vertex is frozen for migration on this shard"),
            Self::VertexTombstoned => write!(f, "vertex row is tombstoned on this shard"),
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(err) => Some(err),
            Self::VertexLabelCatalog(err) => Some(err),
            Self::EdgeLabelCatalog(err) => Some(err),
            Self::EdgeValueProfile(err) => Some(err),
            Self::EdgeWeightProfile(err) => Some(err),
            Self::PropertyCatalog(err) => Some(err),
            Self::VertexLabel(err) => Some(err),
            Self::PropertyValue(err) => Some(err),
            Self::VertexNotDetached { .. }
            | Self::EdgeNotFound { .. }
            | Self::InvalidEdgeLabelId(_)
            | Self::InvalidEdgeValueWidth(_)
            | Self::VertexPlacement(_)
            | Self::VertexMigrating
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

impl From<VertexLabelCatalogError> for GraphStoreError {
    fn from(value: VertexLabelCatalogError) -> Self {
        Self::VertexLabelCatalog(value)
    }
}

impl From<EdgeLabelCatalogError> for GraphStoreError {
    fn from(value: EdgeLabelCatalogError) -> Self {
        Self::EdgeLabelCatalog(value)
    }
}

impl From<EdgeValueProfileStoreError> for GraphStoreError {
    fn from(value: EdgeValueProfileStoreError) -> Self {
        Self::EdgeValueProfile(value)
    }
}

impl From<EdgeWeightProfileStoreError> for GraphStoreError {
    fn from(value: EdgeWeightProfileStoreError) -> Self {
        Self::EdgeWeightProfile(value)
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
