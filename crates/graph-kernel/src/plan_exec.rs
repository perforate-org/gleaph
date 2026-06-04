//! Cross-canister GQL execution wire types (router → graph).
//!
//! IC surface rules (enforced by canister `#[query]` / `#[update]` attributes):
//! - **Query** programs use composite query on the router and `execute_*_query` on graph
//!   (read path; may call index / other canisters).
//! - **Update** programs use update on the router and `execute_*_update` on graph (DML and
//!   posting maintenance). A composite query must not invoke an update method.

use candid::CandidType;
use serde::{Deserialize, Serialize};

use crate::entry::{EdgeLabelId, VertexLabelId};
use crate::federation::ShardId;

/// Router-issued mutation id. `0` is reserved; ids are never reused.
pub type MutationId = u64;

/// Shard-local telemetry event sequence. `0` is reserved; ids are never reused.
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
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanResult {
    pub row_count: u64,
    pub label_telemetry_events: Vec<LabelTelemetryEventWire>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LabelTelemetryEventWire {
    pub mutation_id: MutationId,
    pub shard_event_seq: ShardEventSeq,
    pub label_usage_delta: LabelUsageDelta,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MutationOutcomeWire {
    pub mutation_id: MutationId,
    pub completed: bool,
    pub row_count: u64,
    pub label_telemetry_events: Vec<LabelTelemetryEventWire>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedLabelTable {
    pub vertex: Vec<ResolvedVertexLabel>,
    pub edge: Vec<ResolvedEdgeLabel>,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedVertexLabel {
    pub name: String,
    pub id: VertexLabelId,
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ResolvedEdgeLabel {
    pub name: String,
    pub id: EdgeLabelId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct LabelUsageDelta {
    pub vertex: Vec<(VertexLabelId, i64)>,
    pub edge: Vec<(EdgeLabelId, i64)>,
}

/// Router → graph seed bindings for a single variable on the target shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingEntry {
    pub variable: String,
    pub local_vertex_ids: Vec<u32>,
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
            }],
        };
        let seed_blob = Encode!(&seed).expect("seed encode");
        let args = ExecutePlanArgs {
            target_shard_id: 7,
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
                edge: vec![ResolvedEdgeLabel {
                    name: "KNOWS".into(),
                    id: EdgeLabelId::from_raw(1),
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
                },
                SeedBindingEntry {
                    variable: "b".into(),
                    local_vertex_ids: vec![20, 21],
                },
            ],
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(wire, decoded);
    }
}
