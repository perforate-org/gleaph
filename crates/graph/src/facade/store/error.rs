//! Graph store error type and conversions.

use super::super::{PropertyCatalogError, VertexLabelStoreError, VertexPropertyStoreError};
use gleaph_graph_kernel::canonical_export::CanonicalExportError;
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId};
use ic_stable_lara::{
    DeferredBidirectionalLabeledError, VertexId, labeled::BucketLabelKey as LaraLabelId,
};
use ic_stable_roaring::BitmapError;
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
    /// Edge inline property byte width is not supported by labeled edge-inline-property-bytes storage.
    InvalidEdgeInlinePropertyBytesWidth(usize),
    /// Stored edge-inline-property-bytes bytes do not match the catalog label's configured width.
    EdgeInlinePropertyBytesWidthMismatch {
        label: Option<EdgeLabelId>,
        expected: usize,
        actual: usize,
    },
    /// Remote CSR edge endpoints are not supported without federation stable.
    RemoteEdgeNotSupported,
    /// Federated expand returned or attempted to send invalid edge-inline-property-bytes bytes.
    FederatedExpandPayload {
        detail: String,
    },
    /// Shard-local CSR row is tombstoned.
    VertexTombstoned,
    /// Recording a vertex in the pending-purge set failed (ADR 0021). Surfaced
    /// before the vertex is tombstoned so a tracking failure can never leave a
    /// tombstoned vertex with ungated, visible incident edges.
    PendingPurgeTracking(BitmapError),
    /// One vertex item repeats an initial property id.
    DuplicateBulkVertexProperty {
        vertex_ordinal: usize,
        property_id: PropertyId,
    },
    /// CounterpartScan failed to resolve the canonical edge occurrence.
    CounterpartLookup(ic_stable_lara::labeled::bidirectional::counterpart::CounterpartLookupError),
    /// A Building/Sealing index DML failed its Graph-owned phase, epoch, namespace, or capacity
    /// admission before canonical storage mutation.
    IndexBuildAdmission(CanonicalExportError),
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
            Self::InvalidEdgeInlinePropertyBytesWidth(width) => {
                write!(
                    f,
                    "edge inline property byte width {width} is not supported"
                )
            }
            Self::EdgeInlinePropertyBytesWidthMismatch {
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
                write!(
                    f,
                    "invalid federated expand edge inline property bytes: {detail}"
                )
            }
            Self::VertexTombstoned => write!(f, "vertex row is tombstoned on this shard"),
            Self::PendingPurgeTracking(err) => {
                write!(f, "failed to record vertex pending-purge: {err}")
            }
            Self::DuplicateBulkVertexProperty {
                vertex_ordinal,
                property_id,
            } => write!(
                f,
                "bulk vertex item {vertex_ordinal} repeats property id {}",
                property_id.raw()
            ),
            Self::CounterpartLookup(err) => {
                write!(f, "edge counterpart lookup failed: {err}")
            }
            Self::IndexBuildAdmission(detail) => {
                write!(f, "index build DML admission failed: {detail}")
            }
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
            Self::PendingPurgeTracking(err) => Some(err),
            Self::CounterpartLookup(err) => Some(err),
            Self::VertexNotDetached { .. }
            | Self::EdgeNotFound { .. }
            | Self::InvalidEdgeLabelId(_)
            | Self::InvalidEdgeInlinePropertyBytesWidth(_)
            | Self::EdgeInlinePropertyBytesWidthMismatch { .. }
            | Self::RemoteEdgeNotSupported
            | Self::FederatedExpandPayload { .. }
            | Self::VertexTombstoned => None,
            Self::IndexBuildAdmission(error) => Some(error),
            Self::DuplicateBulkVertexProperty { .. } => None,
        }
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

impl From<ic_stable_lara::labeled::bidirectional::counterpart::CounterpartLookupError> for GraphStoreError {
    fn from(value: ic_stable_lara::labeled::bidirectional::counterpart::CounterpartLookupError) -> Self {
        Self::CounterpartLookup(value)
    }
}
