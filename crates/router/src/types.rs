//! Candid-shaped router types.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

pub use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
pub use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
pub use gleaph_graph_kernel::federation::{
    GlobalVertexId, GraphShardKey, LocalVertexId, ShardId, ShardRegistryEntry,
};

#[derive(CandidType, Deserialize)]
pub struct GrantRoleArgs {
    pub target: Principal,
    pub role: String,
    pub manager_caps: u64,
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminRegisterShardArgs {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
    pub logical_graph_name: String,
}

/// One router-orchestrated batch of label posting backfill on a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminLabelBackfillStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum local vertices to scan on the shard in this step (must be > 0).
    pub max_vertices: u32,
}

/// Progress from one router backfill step.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminLabelBackfillStepResult {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}

/// Router-stable cursor for label posting backfill on one shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LabelBackfillShardStatus {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub done: bool,
}

/// One router-orchestrated batch of vertex property posting backfill on a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminVertexPropertyBackfillStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum local vertices to scan on the shard in this step (must be > 0).
    pub max_vertices: u32,
}

/// Progress from one router vertex property backfill step.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminVertexPropertyBackfillStepResult {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}

/// Router-stable cursor for vertex property posting backfill on one shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexPropertyBackfillShardStatus {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub done: bool,
}

/// One router-orchestrated batch of edge property posting backfill on a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminEdgeBackfillStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum edge property entries to scan on the shard in this step (must be > 0).
    pub max_entries: u32,
}

/// Progress from one router edge backfill step.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminEdgeBackfillStepResult {
    pub shard_id: ShardId,
    pub next_after_key: Option<Vec<u8>>,
    pub entries_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}

/// Router-stable cursor for edge property posting backfill on one shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EdgeBackfillShardStatus {
    pub shard_id: ShardId,
    pub after_key: Option<Vec<u8>>,
    pub done: bool,
}

/// One router-orchestrated batch advancing label stats projection for a graph shard.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminLabelStatsProjectionStepArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    /// Maximum pending deltas to apply from the shard log in this step (must be > 0).
    pub max_deltas: u32,
}

/// Progress from one router label stats projection step.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminLabelStatsProjectionStepResult {
    pub shard_id: ShardId,
    pub deltas_drained: u32,
    pub deltas_applied: u32,
    pub done: bool,
}
