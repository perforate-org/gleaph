//! Candid-shaped router types.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

pub use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
pub use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
pub use gleaph_graph_kernel::federation::{
    GlobalVertexId, GraphShardKey, LocalVertexId, ShardId, ShardRegistryEntry,
};
use gleaph_graph_kernel::plan_exec::{MutationId, MutationLifecyclePhase};

pub use crate::facade::stable::label_stats::{ClientMutationKey, RouterMutationRecord};

/// Operator/SDK-facing status of a federated mutation (ADR 0029 Phase 4).
///
/// Pull-based observability for the autonomous recovery driver: a caller polls this to learn
/// whether a saga converged, which shard is outstanding, and what (if any) explicit action
/// is required. It deliberately carries no read-your-writes token — the token is issued with
/// the original DML result; this query reports lifecycle, not freshness watermarks.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MutationStatus {
    pub mutation_id: MutationId,
    pub phase: MutationLifecyclePhase,
    /// Most recent recovery diagnostic, if any.
    pub last_error: Option<String>,
    /// First shard still outstanding (incomplete canonical write, else lagging projection).
    pub target_shard: Option<ShardId>,
    /// Human-readable next step: `none` when terminal/auto-converging, or the explicit retry
    /// guidance when caller action is required.
    pub next_action: String,
}

impl MutationStatus {
    pub fn from_record(record: &RouterMutationRecord) -> Self {
        let phase = record.lifecycle_phase();
        let target_shard = record
            .shards
            .iter()
            .find(|shard| !shard.completed)
            .or_else(|| {
                record
                    .shards
                    .iter()
                    .find(|shard| !shard.projection_advanced)
            })
            .map(|shard| shard.shard_id);
        let next_action = match phase {
            MutationLifecyclePhase::Completed => "none",
            MutationLifecyclePhase::Failed => "resubmit with a new client_mutation_key",
            MutationLifecyclePhase::Routing => {
                "routing in progress; retry the idempotent mutation if it does not settle"
            }
            MutationLifecyclePhase::CanonicalPending => {
                "retry the idempotent mutation to resume the remaining canonical shard writes"
            }
            MutationLifecyclePhase::CanonicalCommitted
            | MutationLifecyclePhase::ProjectionPending => {
                "none; projection recovery is automatic (poll mutation_status or use AtLeast reads)"
            }
        }
        .to_string();
        Self {
            mutation_id: record.mutation_id,
            phase,
            last_error: record.last_error.clone(),
            target_shard,
            next_action,
        }
    }
}

#[derive(CandidType, Deserialize)]
pub struct GrantRoleArgs {
    pub target: Principal,
    pub role: String,
    pub manager_caps: u64,
}

/// Arguments for one expired client-mutation-key sweep step. The sweep is
/// operator-driven (like backfill / label-stats projection): call repeatedly,
/// feeding `next_cursor` back as `start_after`, until `done` is true.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminSweepMutationKeysStepArgs {
    /// Resume scanning strictly after this key; `None` starts from the beginning.
    pub start_after: Option<ClientMutationKey>,
    /// Maximum journal entries to scan in this step (must be > 0).
    pub max_scan: u32,
}

/// Progress from one expired client-mutation-key sweep step.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminSweepMutationKeysStepResult {
    /// Entries examined in this step.
    pub scanned: u32,
    /// Expired entries removed in this step.
    pub removed: u32,
    /// Feed back as `start_after` to continue; `None` when the scan reached the end.
    pub next_cursor: Option<ClientMutationKey>,
    /// True when the whole journal has been scanned in this step.
    pub done: bool,
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

/// Which posting-backfill cursor a reset targets.
#[derive(
    CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum BackfillKind {
    Label,
    VertexProperty,
    Edge,
}

/// Operator recovery: clear a stuck `in_progress` claim on one shard's backfill
/// cursor (see ADR 0009). Only use after confirming no step is in flight for the
/// shard, since clearing a legitimately in-flight claim re-enables the cursor race.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminResetBackfillClaimArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    pub kind: BackfillKind,
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

/// Wire view of a derived vector-index activation state (ADR 0031 Slice 3). Mirrors the internal
/// `VectorIndexActivationState`; `DispatchEnabled` is unreachable in Slice 3 (fail-closed gate).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorIndexActivationStateView {
    Registered,
    DispatchBlockedMissingIncarnationFence,
    DispatchEnabled,
}

/// Admin: register a derived vector index for a logical graph (ADR 0031 Slice 3). The embedding is
/// identified **by name** (the Router interns it to a stable `EmbeddingNameId`), never by a raw id.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RegisterVectorIndexArgs {
    pub logical_graph_name: String,
    pub embedding_name: String,
    pub index_id: u32,
    pub dims: u16,
    /// Optional single dispatch target; rejected if anonymous. Slice 3 stores it as inspect-only
    /// metadata and never pushes it to graph shards or enables dispatch.
    pub target: Option<Principal>,
    pub if_not_exists: bool,
}

/// Admin: set (or replace) the single dispatch target of an existing vector index (ADR 0031 Slice 3).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SetVectorIndexTargetArgs {
    pub logical_graph_name: String,
    pub index_id: u32,
    pub target: Principal,
}

/// Wire view of a stored vector-index definition (ADR 0031 Slice 3). Algorithm-neutral: physical
/// search knobs (centroids, nlist, page geometry) are deliberately not exposed.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorIndexInfo {
    pub index_id: u32,
    pub embedding_name_id: u16,
    pub dims: u16,
    pub target: Option<Principal>,
    pub activation_state: VectorIndexActivationStateView,
}

/// Activation status + fail-closed explanation for one vector-index definition (ADR 0031 Slice 3).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorIndexActivationStatus {
    pub index_id: u32,
    pub activation_state: VectorIndexActivationStateView,
    /// `Some(reason)` while production dispatch/backfill is fail-closed; `None` otherwise.
    pub blocked_reason: Option<String>,
}

/// Admin: request a derived vector-index backfill step (ADR 0031 Slice 3). In Slice 3 this surface
/// **fails closed** (`VectorDispatchActivationBlocked`) for production execution — no graph backfill
/// endpoint is wired until incarnation fencing activates dispatch. It exists so operators can probe
/// the activation gate and so the admin contract is stable across the activation slice.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminVectorIndexBackfillStepArgs {
    pub logical_graph_name: String,
    pub index_id: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::label_stats::RouterMutationShard;

    fn shard(id: u32, completed: bool, projection_advanced: bool) -> RouterMutationShard {
        let mut shard = RouterMutationShard::new(ShardId::new(id), Principal::anonymous(), None);
        shard.completed = completed;
        shard.projection_advanced = projection_advanced;
        shard
    }

    fn record_with(shards: Vec<RouterMutationShard>) -> RouterMutationRecord {
        let mut record = RouterMutationRecord::new(7, 0, Vec::new());
        record.routing_in_progress = false;
        record.shards = shards;
        record
    }

    #[test]
    fn status_for_canonical_pending_points_at_outstanding_shard_and_asks_retry() {
        let record = record_with(vec![shard(0, true, true), shard(1, false, false)]);
        let status = MutationStatus::from_record(&record);
        assert_eq!(status.mutation_id, 7);
        assert_eq!(status.phase, MutationLifecyclePhase::CanonicalPending);
        assert_eq!(status.target_shard, Some(ShardId::new(1)));
        assert!(status.next_action.contains("retry"));
    }

    #[test]
    fn status_for_projection_pending_is_automatic_recovery() {
        let record = record_with(vec![shard(0, true, false)]);
        let status = MutationStatus::from_record(&record);
        assert_eq!(status.phase, MutationLifecyclePhase::CanonicalCommitted);
        assert_eq!(status.target_shard, Some(ShardId::new(0)));
        assert!(status.next_action.starts_with("none"));
    }

    #[test]
    fn status_for_completed_has_no_target_or_action() {
        let mut record = record_with(vec![shard(0, true, true)]);
        record.completed_row_count = Some(3);
        let status = MutationStatus::from_record(&record);
        assert_eq!(status.phase, MutationLifecyclePhase::Completed);
        assert_eq!(status.target_shard, None);
        assert_eq!(status.next_action, "none");
    }
}
