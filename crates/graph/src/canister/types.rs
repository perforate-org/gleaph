//! Candid-shaped types for canister `init`.

use candid::{CandidType, Deserialize, Principal};

use gleaph_graph_kernel::federation::ShardId;

/// Result of [`super::handlers::e2e_insert_vertex`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexResult {
    pub local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub global_vertex_id: gleaph_graph_kernel::federation::GlobalVertexId,
}

/// Arguments for [`super::handlers::e2e_insert_directed_edge`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertDirectedEdgeArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
}

/// Arguments for [`super::handlers::e2e_insert_vertex_with_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithPropertyArgs {
    pub property_id: u32,
    pub value: i64,
}

/// Arguments for [`super::handlers::e2e_insert_vertex_with_two_properties`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithTwoPropertiesArgs {
    pub property_a: u32,
    pub value_a: i64,
    pub property_b: u32,
    pub value_b: i64,
}

/// Arguments for [`super::handlers::e2e_insert_directed_edge_with_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithPropertyArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub edge_label_id: u16,
    pub property_id: u32,
    pub value: i64,
}

/// Arguments for [`super::handlers::e2e_insert_undirected_edge_with_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertUndirectedEdgeWithPropertyArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub edge_label_id: u16,
    pub property_id: u32,
    pub value: i64,
}

/// Arguments supplied by the registry (or installer) on first `init`.
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct GraphInitArgs {
    pub logical_graph_name: Option<String>,
    /// Router canister for federation (required together with `shard_id`).
    #[serde(default)]
    pub router_canister: Option<Principal>,
    #[serde(default)]
    pub shard_id: Option<ShardId>,
    /// Index canister for install-time federation wiring.
    ///
    /// Canister init cannot perform inter-canister calls, so deployments pass this after the
    /// Router registry has been configured.
    #[serde(default)]
    pub index_canister: Option<Principal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_init_args_candid_hex() {
        let args = GraphInitArgs {
            logical_graph_name: None,
            router_canister: None,
            shard_id: None,
            index_canister: None,
        };
        let bytes = candid::encode_one(args).expect("encode GraphInitArgs");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("canbench init_args hex: {hex}");
        assert!(!hex.is_empty());
    }
}
