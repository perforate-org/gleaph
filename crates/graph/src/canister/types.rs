//! Candid-shaped types for canister `init`.

#[cfg(feature = "pocket-ic-e2e")]
use candid::{CandidType, Deserialize};

/// Shared Graph init args, single-sourced in `gleaph_graph_kernel::provisioning` so the Router
/// can construct `install_args` for a Provision-issued Graph shard without depending on this crate.
pub use gleaph_graph_kernel::provisioning::init_args::GraphInitArgs;

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

/// Arguments for [`super::handlers::e2e_insert_vertex_with_label`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithLabelArgs {
    pub label_id: u16,
}
/// Arguments for [`super::handlers::e2e_insert_vertex_with_label_and_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithLabelAndPropertyArgs {
    pub label_id: u16,
    pub property_id: u32,
    pub value: i64,
}
/// Arguments for the text-seed E2E variant (ADR 0059 §Text build kind backfill proof).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithLabelAndTextPropertyArgs {
    pub label_id: u16,
    pub property_id: u32,
    pub value: String,
}
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithLabelAndTwoPropertiesArgs {
    pub label_id: u16,
    pub property_a: u32,
    pub value_a: i64,
    pub property_b: u32,
    pub value_b: i64,
}

/// One field value inside an E2E seed record (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub enum E2eRecordFieldValue {
    Int(i64),
    IntList(Vec<i64>),
    Record(Vec<(String, E2eRecordFieldValue)>),
}

/// Arguments for [`super::handlers::e2e_insert_vertex_with_label_and_record`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertVertexWithLabelAndRecordArgs {
    pub label_id: u16,
    pub property_id: u32,
    /// Record fields stored verbatim under `property_id`.
    pub record: Vec<(String, E2eRecordFieldValue)>,
}

#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eSetVertexPropertyArgs {
    pub local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub property_id: u32,
    pub value: i64,
}

/// Arguments for [`super::handlers::e2e_set_vertex_record`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eSetVertexRecordArgs {
    pub local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub property_id: u32,
    /// Record fields stored verbatim under `property_id`.
    pub record: Vec<(String, E2eRecordFieldValue)>,
}

/// Arguments for [`super::handlers::e2e_insert_directed_edge_with_label`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithLabelArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub edge_label_id: u16,
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

/// Arguments for [`super::handlers::e2e_insert_directed_edge_with_inline_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eInsertDirectedEdgeWithInlinePropertyArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub edge_label_id: u16,
    pub inline_property_bytes: Vec<u8>,
    pub inline_property_profile: gleaph_graph_kernel::entry::EdgeInlinePropertyProfile,
}

/// Arguments for [`super::handlers::e2e_enqueue_forward_compaction`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eEnqueueForwardCompactionArgs {
    pub local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
}

/// Arguments for [`super::handlers::e2e_delete_directed_edge_with_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eDeleteDirectedEdgeArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub property_id: u32,
}

/// Arguments for [`super::handlers::e2e_set_edge_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eSetEdgePropertyArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub property_id: u32,
    pub value: i64,
}

/// Arguments for [`super::handlers::e2e_reverse_resolved_edge_property`] (PocketIC E2E only).
#[cfg(feature = "pocket-ic-e2e")]
#[derive(CandidType, Deserialize, Clone, Debug)]
pub struct E2eReverseResolvedEdgePropertyArgs {
    pub source_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub target_local_vertex_id: gleaph_graph_kernel::federation::LocalVertexId,
    pub property_id: u32,
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
