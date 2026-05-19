//! Candid-shaped router types.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

pub use gleaph_gql_ic::graph_registry::{
    GraphRegistryEntry, GraphStatus, ProvisioningState,
};
pub use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
pub use gleaph_graph_kernel::federation::{
    LocalVertexId, LogicalVertexId, PhysicalVertexLocation, ShardId, ShardRegistryEntry,
    VertexPlacement,
};

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminRegisterShardArgs {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
    pub logical_graph_name: String,
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CommitVertexPlacementArgs {
    pub logical_vertex_id: LogicalVertexId,
    pub local_vertex_id: LocalVertexId,
}
