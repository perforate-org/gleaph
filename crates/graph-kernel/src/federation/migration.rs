//! Wire types for federated vertex migration (export/import between graph shards).

use candid::CandidType;
use serde::{Deserialize, Serialize};

use super::{LocalVertexId, LogicalVertexId, ShardId};
use crate::entry::{EdgeLabelId, PropertyId, VertexLabelId};

/// Outgoing adjacency exported from the source shard (forward CSR only).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExportedOutEdge {
    pub catalog_label: Option<EdgeLabelId>,
    pub undirected: bool,
    pub inline_value: u16,
    pub target: ExportedEdgeTarget,
    pub properties: Vec<ExportedProperty>,
}

/// Edge endpoint identified by stable logical vertex id (remapped on import).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ExportedEdgeTarget {
    Local { logical_vertex_id: LogicalVertexId },
    Remote { logical_vertex_id: LogicalVertexId },
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExportedProperty {
    pub property_id: PropertyId,
    pub value_bytes: Vec<u8>,
}

/// Vertex payload moved from source graph shard to destination.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExportedVertex {
    pub logical_vertex_id: LogicalVertexId,
    /// Source shard id (informational; router is authoritative after finish).
    pub source_shard_id: ShardId,
    pub source_local_vertex_id: LocalVertexId,
    /// Encoded [`crate::entry::Vertex`] row (`Vertex::BYTES` / `LabeledVertex` wire).
    pub vertex_row_bytes: Vec<u8>,
    pub labels: Vec<VertexLabelId>,
    pub properties: Vec<ExportedProperty>,
    pub out_edges: Vec<ExportedOutEdge>,
}
