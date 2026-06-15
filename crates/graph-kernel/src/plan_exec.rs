//! Cross-canister GQL execution wire types (router → graph).
//!
//! IC surface rules (enforced by canister `#[query]` / `#[update]` attributes):
//! - **Query** programs use composite query on the router and `execute_*_query` on graph
//!   (read path; may call index / other canisters).
//! - **Update** programs use update on the router and `execute_*_update` on graph (DML and
//!   posting maintenance). A composite query must not invoke an update method.

use candid::CandidType;
use serde::{Deserialize, Serialize};

use crate::entry::{EdgeLabelId, EdgePayloadProfile, PropertyId, VertexLabelId};
use crate::federation::ShardId;

/// Router-issued mutation id. `0` is reserved; ids are never reused.
pub type MutationId = u64;

/// Shard-local label stats delta sequence. `0` is reserved; ids are never reused.
pub type ShardEventSeq = u64;

/// Selects the IC call kind for a wired program/plan (must match the canister entrypoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GqlExecutionMode {
    /// Read-only execution (`gql_query` / `execute_plan_query` / composite where needed).
    Query,
    /// Write path (`gql_execute` / `execute_plan_update`).
    Update,
}

/// Router → graph: execute a pre-built physical plan on a target shard.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanArgs {
    pub target_shard_id: ShardId,
    /// Router-issued idempotency key for update/DML execution.
    pub mutation_id: Option<MutationId>,
    pub plan_blob: Vec<u8>,
    pub params_blob: Vec<u8>,
    pub mode: GqlExecutionMode,
    /// When set, graph skips the first anchor `IndexScan` and binds these local vertex ids.
    pub seed_bindings_blob: Option<Vec<u8>>,
    /// Router-resolved label names referenced by the physical plan.
    pub resolved_labels: Option<ResolvedLabelTable>,
    /// Router-resolved property names referenced by the physical plan.
    pub resolved_properties: Option<ResolvedPropertyTable>,
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanResult {
    pub row_count: u64,
    /// Candid-encoded [`gleaph_gql_ic::IcWirePlanQueryResult`]; set on query shard execution.
    pub rows_blob: Option<Vec<u8>>,
    /// Forward out-adjacency hubs from a DML batch (router P3 auto-finalize hint).
    #[serde(default)]
    pub hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
}

/// Router read-path result: merged row count and optional materialized rows.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GqlQueryResult {
    pub row_count: u64,
    /// Candid-encoded [`gleaph_gql_ic::IcWirePlanQueryResult`] after federated merge.
    pub rows_blob: Option<Vec<u8>>,
}

impl GqlQueryResult {
    pub fn from_merged(merged: &ExecutePlanResult) -> Self {
        Self {
            row_count: merged.row_count,
            rows_blob: merged.rows_blob.clone(),
        }
    }

    pub fn row_count_only(row_count: u64) -> Self {
        Self {
            row_count,
            rows_blob: None,
        }
    }
}

/// Ordered label stats delta appended by graph shard DML (ADR 0015).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LabelStatsDeltaEventWire {
    pub mutation_id: MutationId,
    pub shard_event_seq: ShardEventSeq,
    pub label_stats_delta: LabelStatsDelta,
}

/// Per-label live count changes emitted by graph shard DML (ADR 0015).
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LabelStatsDelta {
    pub vertex: Vec<(VertexLabelId, i64)>,
    pub edge: Vec<(EdgeLabelId, i64)>,
}

/// Graph-local mutation journal state (ADR 0015).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MutationJournalState {
    Incomplete,
    Completed,
}

/// Graph shard mutation idempotency journal entry (ADR 0015).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphMutationJournalEntryWire {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub row_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
}

#[derive(Clone, Debug, Default, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedLabelTable {
    pub vertex: Vec<ResolvedVertexLabel>,
    pub edge: Vec<ResolvedEdgeLabel>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedVertexLabel {
    pub name: String,
    pub id: VertexLabelId,
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedEdgeLabel {
    pub name: String,
    pub id: EdgeLabelId,
    /// Router-owned logical schema (ADR 0008). Default `no_payload` when omitted on legacy wire.
    pub payload_profile: EdgePayloadProfile,
}

impl ResolvedEdgeLabel {
    pub fn new(
        name: impl Into<String>,
        id: EdgeLabelId,
        payload_profile: EdgePayloadProfile,
    ) -> Self {
        Self {
            name: name.into(),
            id,
            payload_profile,
        }
    }
}

impl ResolvedLabelTable {
    pub fn edge_payload_profile(&self, id: EdgeLabelId) -> Option<&EdgePayloadProfile> {
        self.edge
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.payload_profile)
    }

    pub fn edge_label_ids_with_nonzero_payload(&self) -> Vec<EdgeLabelId> {
        self.edge
            .iter()
            .filter(|entry| entry.payload_profile.required_byte_width() > 0)
            .map(|entry| entry.id)
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedPropertyTable {
    pub properties: Vec<ResolvedProperty>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedProperty {
    pub name: String,
    pub id: PropertyId,
}

/// Shard-local edge identity for router seed bindings (ADR 0009 phase D).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LocalEdgePosting {
    pub owner_vertex_id: u32,
    pub label_id: u16,
    pub slot_index: u32,
}

/// Router → graph seed bindings for a single variable on the target shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingEntry {
    pub variable: String,
    #[serde(default)]
    pub local_vertex_ids: Vec<u32>,
    #[serde(default)]
    pub local_edge_postings: Vec<LocalEdgePosting>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingsWire {
    pub entries: Vec<SeedBindingEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn execute_plan_result_roundtrip_with_hot_forward_vertices() {
        let result = ExecutePlanResult {
            row_count: 1,
            rows_blob: None,
            hot_forward_vertices: vec![7, 42],
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: ExecutePlanResult = Decode!(&bytes, ExecutePlanResult).expect("decode");
        assert_eq!(result, decoded);
    }

    #[test]
    fn execute_plan_result_roundtrip_with_rows_blob() {
        let result = ExecutePlanResult {
            row_count: 2,
            rows_blob: Some(vec![1, 2, 3]),
            hot_forward_vertices: Vec::new(),
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: ExecutePlanResult = Decode!(&bytes, ExecutePlanResult).expect("decode");
        assert_eq!(result, decoded);
    }

    #[test]
    fn gql_execution_mode_candid_roundtrip() {
        for mode in [GqlExecutionMode::Query, GqlExecutionMode::Update] {
            let bytes = Encode!(&mode).expect("encode");
            let decoded: GqlExecutionMode = Decode!(&bytes, GqlExecutionMode).expect("decode");
            assert_eq!(mode, decoded);
        }
    }

    #[test]
    fn execute_plan_args_with_seed_bindings_roundtrip() {
        let seed = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "u".into(),
                local_vertex_ids: vec![1, 2],
                local_edge_postings: Vec::new(),
            }],
        };
        let seed_blob = Encode!(&seed).expect("seed encode");
        let args = ExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            mutation_id: Some(1),
            plan_blob: vec![1, 2, 3],
            params_blob: vec![4],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: Some(seed_blob),
            resolved_labels: Some(ResolvedLabelTable {
                vertex: vec![ResolvedVertexLabel {
                    name: "User".into(),
                    id: VertexLabelId::from_raw(1),
                }],
                edge: vec![ResolvedEdgeLabel::new(
                    "KNOWS",
                    EdgeLabelId::from_raw(1),
                    EdgePayloadProfile::no_payload(),
                )],
            }),
            resolved_properties: Some(ResolvedPropertyTable {
                properties: vec![ResolvedProperty {
                    name: "name".into(),
                    id: PropertyId::from_raw(1),
                }],
            }),
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn seed_bindings_wire_roundtrip() {
        let wire = SeedBindingsWire {
            entries: vec![
                SeedBindingEntry {
                    variable: "a".into(),
                    local_vertex_ids: vec![10],
                    local_edge_postings: Vec::new(),
                },
                SeedBindingEntry {
                    variable: "b".into(),
                    local_vertex_ids: vec![20, 21],
                    local_edge_postings: Vec::new(),
                },
            ],
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(wire, decoded);
    }

    #[test]
    fn edge_seed_bindings_wire_roundtrip() {
        let wire = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "e".into(),
                local_vertex_ids: Vec::new(),
                local_edge_postings: vec![
                    LocalEdgePosting {
                        owner_vertex_id: 3,
                        label_id: 7,
                        slot_index: 1,
                    },
                    LocalEdgePosting {
                        owner_vertex_id: 4,
                        label_id: 7,
                        slot_index: 0,
                    },
                ],
            }],
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(wire, decoded);
    }
}
