//! Candid-shaped router types.

use candid::{CandidType, Decode, Encode, Principal};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

pub use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
pub use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId, PropertyId, VertexLabelId};
pub use gleaph_graph_kernel::federation::{
    GlobalVertexId, GraphShardKey, LocalVertexId, ShardId, ShardRegistryEntry,
};
use gleaph_graph_kernel::plan_exec::{MutationId, MutationLifecyclePhase};
pub use gleaph_graph_kernel::provisioning::wire::{
    CreatedResource, ProvisionRequest, ProvisionResult, ProvisionResultOutcome,
    ProvisionableResource, RouterProvisionAck,
};
pub use gleaph_graph_kernel::provisioning::{ProvisionableResourceKind, ProvisioningIntentKey};
use gleaph_graph_kernel::vector_index::{
    VectorMaintenanceFailure, VectorMaintenancePolicy, VectorMaintenanceState,
    VectorMaintenanceStepResult, VectorMetric, VectorPartitionPageHealth, VectorRebuildStatus,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};

pub use crate::facade::stable::label_stats::{ClientMutationKey, RouterMutationRecord};
use crate::facade::stable::vector_maintenance_policy::VectorMaintenancePolicyRecord;

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

/// Admin: wire (or retrofit) a derived vector-index target onto an already-registered shard and
/// drive the attach handshake (ADR 0031 Slice 4). The Router records the target in the shard
/// registry, calls the graph shard's router-guarded `admin_set_vector_index_canister` so its
/// **local** `FederationRouting` carries the target, attaches the shard to the vector canister, and
/// only then flips the durable `vector_index_attached` readiness bit. Idempotent; serves both fresh
/// and existing (upgraded) shards. Rejects an anonymous target.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminAttachVectorIndexShardArgs {
    pub logical_graph_name: String,
    pub shard_id: ShardId,
    pub vector_index_canister: Principal,
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
    DispatchBlocked,
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
    /// Optional metric; defaults to `L2Squared` if omitted for wire stability.
    pub metric: Option<VectorMetric>,
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
    pub metric: VectorMetric,
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

/// Admin: drive one bounded derived vector-index backfill step on a graph shard (ADR 0031 Slice 5).
/// The caller supplies an explicit resume cursor (`start_vertex_id`) and budget (`max_vertices`) and
/// loops, feeding [`AdminVectorIndexBackfillStepResult::next_vertex_id`] until `done`. Fails closed
/// (`VectorDispatchActivationBlocked`) while dispatch is not ready (global flag off or shards not
/// vector-attached).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdminVectorIndexBackfillStepArgs {
    pub logical_graph_name: String,
    pub index_id: u32,
    pub shard_id: ShardId,
    pub start_vertex_id: LocalVertexId,
    /// Maximum local vertices to scan on the shard in this step (clamped to ≥ 1 by the worker).
    pub max_vertices: u32,
}

/// Public exact vector-search request (ADR 0031 Slice 5). The Router resolves the
/// `logical_graph_name` and `index_id` to the single activated target and forwards an exact
/// `ivf_flat` scan. The `F32` encoding and metric are supplied from the stored definition.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterVectorSearchRequest {
    pub logical_graph_name: String,
    pub index_id: u32,
    /// `dims * 4` bytes of little-endian `f32` query components.
    pub query: Vec<u8>,
    pub dims: u16,
    pub top_k: u32,
}

/// Admin: ingest one finite F32 vertex embedding through Router into the owning Graph shard
/// (plan 0048). The caller supplies only the logical graph name, the opaque encoded vertex id,
/// the registered embedding name, and the vector values; Router resolves ownership and the
/// definition and dispatches a single canonical write.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AdminIngestVertexEmbeddingArgs {
    pub logical_graph_name: String,
    /// Opaque 8-byte graph-scoped vertex id (`ELEMENT_ID(v)`).
    pub encoded_vertex_id: Vec<u8>,
    pub embedding_name: String,
    pub values: Vec<f32>,
}

/// Progress from one derived vector-index backfill step (ADR 0031 Slice 5).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminVectorIndexBackfillStepResult {
    pub shard_id: ShardId,
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub embeddings_synced: u32,
    pub done: bool,
}

/// Admin: create or replace a vector maintenance policy (ADR 0031 Slice 10). Router-owned SSOT for
/// maintenance thresholds + per-step budgets; validated and stored only when the vector-index
/// definition exists. Default state is absent (the push scheduler is a no-op until set + enabled).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SetVectorMaintenancePolicyArgs {
    pub logical_graph_name: String,
    pub index_id: u32,
    pub enabled: bool,
    pub policy: VectorMaintenancePolicy,
    pub target_nlist: Option<u32>,
    pub sample_limit: u32,
    pub scan_max_pages: u32,
    pub rebuild_max_subjects: u32,
    pub cleanup_max_work: u32,
}

/// Operator-facing view of a stored vector maintenance policy (ADR 0031 Slice 10).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VectorMaintenancePolicyView {
    pub graph_id: u32,
    pub index_id: u32,
    pub enabled: bool,
    pub policy: VectorMaintenancePolicy,
    pub target_nlist: Option<u32>,
    pub sample_limit: u32,
    pub scan_max_pages: u32,
    pub rebuild_max_subjects: u32,
    pub cleanup_max_work: u32,
}

impl From<VectorMaintenancePolicyRecord> for VectorMaintenancePolicyView {
    fn from(record: VectorMaintenancePolicyRecord) -> Self {
        Self {
            graph_id: record.graph_id.raw(),
            index_id: record.index_id,
            enabled: record.enabled,
            policy: record.policy,
            target_nlist: record.target_nlist,
            sample_limit: record.sample_limit,
            scan_max_pages: record.scan_max_pages,
            rebuild_max_subjects: record.rebuild_max_subjects,
            cleanup_max_work: record.cleanup_max_work,
        }
    }
}

/// Outcome of one Router-push maintenance step (ADR 0031 Slice 10). `Disabled` is a Router-level
/// no-op (absent or disabled policy); otherwise the vector canister's bounded step result is relayed.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum VectorMaintenanceStepOutcome {
    /// No policy exists or it is disabled; no work was forwarded.
    Disabled,
    /// The vector canister advanced one bounded maintenance unit.
    Stepped(VectorMaintenanceStepResult),
}

/// Cursor-redacted projection of the vector canister's [`VectorMaintenanceState`] for the Router
/// aggregate status (ADR 0031 Slice 10). The opaque resume cursor bytes are collapsed to a
/// `cursor_present` flag so the Router status surface honours the "present/absent, not decoded"
/// contract and never leaks internal stable `PageKey` bytes.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum VectorMaintenanceStateView {
    /// No maintenance in progress.
    Idle,
    /// A bounded page-health scan is accumulating counters; the resume cursor is reported as
    /// present/absent only.
    Scanning {
        /// Whether a resume cursor is set (`true`) or the scan would (re)start from the lower bound.
        cursor_present: bool,
        /// `true` once the scan has covered every page of the scoped version.
        exhausted: bool,
        /// Additive page-health accumulated so far, scoped by its `index_id`/`index_version`.
        merged: VectorPartitionPageHealth,
    },
    /// A prior step failed; recovery requires an explicit `admin_vector_maintenance_reset`.
    Failed(VectorMaintenanceFailure),
}

impl From<VectorMaintenanceState> for VectorMaintenanceStateView {
    fn from(state: VectorMaintenanceState) -> Self {
        match state {
            VectorMaintenanceState::Idle => Self::Idle,
            VectorMaintenanceState::Scanning {
                cursor,
                exhausted,
                merged,
            } => Self::Scanning {
                cursor_present: cursor.is_some(),
                exhausted,
                merged,
            },
            VectorMaintenanceState::Failed(failure) => Self::Failed(failure),
        }
    }
}

/// Aggregated maintenance status for one vector index (ADR 0031 Slice 10): Router-owned policy +
/// readiness, plus the forwarded vector-canister execution and rebuild state when reachable. Cursors
/// inside `maintenance_state` are reported present/absent, not decoded.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct VectorMaintenanceStatusView {
    pub index_id: u32,
    /// Whether a Router policy exists and is enabled.
    pub policy_enabled: bool,
    /// Resolved single vector target, if set.
    pub target: Option<Principal>,
    /// Per-graph dispatch readiness (global flag on AND shards vector-attached).
    pub dispatch_ready: bool,
    /// `Some(reason)` while forwarding is fail-closed; `None` once ready.
    pub blocked_reason: Option<String>,
    /// Forwarded vector-canister maintenance execution state with the resume cursor redacted to a
    /// present/absent flag; `None` if unreachable.
    pub maintenance_state: Option<VectorMaintenanceStateView>,
    /// Forwarded vector-canister rebuild status; `None` if unreachable.
    pub rebuild_status: Option<VectorRebuildStatus>,
}

// === ADR 0035 provisioning types ==============================================

/// Stable-memory key for Map 45: RouterProvisioningRequest by (request_id, deployment_id).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProvisioningRequestKey {
    pub request_id: String,
    pub deployment_id: String,
}

impl ProvisioningRequestKey {
    pub(crate) fn new(request_id: &str, deployment_id: &str) -> Self {
        Self {
            request_id: request_id.to_owned(),
            deployment_id: deployment_id.to_owned(),
        }
    }
}

impl Storable for ProvisioningRequestKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.request_id.len() + self.deployment_id.len());
        out.extend_from_slice(&(self.request_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.request_id.as_bytes());
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut offset = 0usize;
        let request_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("request_id len"),
        ) as usize;
        offset += 4;
        let request_id = String::from_utf8(bytes[offset..offset + request_id_len].to_vec())
            .expect("request_id utf8");
        offset += request_id_len;
        let deployment_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("deployment_id len"),
        ) as usize;
        offset += 4;
        let deployment_id = String::from_utf8(bytes[offset..offset + deployment_id_len].to_vec())
            .expect("deployment_id utf8");
        Self {
            request_id,
            deployment_id,
        }
    }
}

/// Secondary index key for Map 46: (deployment_id, graph_name, request_id) → ProvisioningRequestKey.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ProvisioningByGraphKey {
    pub deployment_id: String,
    pub graph_name: String,
    pub request_id: String,
}

impl ProvisioningByGraphKey {
    pub(crate) fn new(deployment_id: &str, graph_name: &str, request_id: &str) -> Self {
        Self {
            deployment_id: deployment_id.to_owned(),
            graph_name: graph_name.to_owned(),
            request_id: request_id.to_owned(),
        }
    }
}

impl Storable for ProvisioningByGraphKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            12 + self.deployment_id.len() + self.graph_name.len() + self.request_id.len(),
        );
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out.extend_from_slice(&(self.graph_name.len() as u32).to_le_bytes());
        out.extend_from_slice(self.graph_name.as_bytes());
        out.extend_from_slice(&(self.request_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.request_id.as_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut offset = 0usize;
        let deployment_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("deployment_id len"),
        ) as usize;
        offset += 4;
        let deployment_id = String::from_utf8(bytes[offset..offset + deployment_id_len].to_vec())
            .expect("deployment_id utf8");
        offset += deployment_id_len;
        let graph_name_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("graph_name len"),
        ) as usize;
        offset += 4;
        let graph_name = String::from_utf8(bytes[offset..offset + graph_name_len].to_vec())
            .expect("graph_name utf8");
        offset += graph_name_len;
        let request_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("request_id len"),
        ) as usize;
        offset += 4;
        let request_id = String::from_utf8(bytes[offset..offset + request_id_len].to_vec())
            .expect("request_id utf8");
        Self {
            deployment_id,
            graph_name,
            request_id,
        }
    }
}

/// Intent lock held marker for Map 47 value — unit struct avoids () Storable ambiguity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IntentLockMarker;

impl Storable for IntentLockMarker {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&[])
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::new()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        assert!(bytes.as_ref().is_empty(), "IntentLockMarker is zero bytes");
        Self
    }
}

/// Router-side lifecycle state for a provisioning request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub(crate) enum RouterProvisioningRequestState {
    Pending,
    Submitted,
    AwaitingAck,
    Completed,
    Failed { reason: String },
}

/// Router canonical record for an issuance intent (ADR 0035 §Router orchestration state).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub(crate) struct RouterProvisioningRequest {
    pub request_id: String,
    pub request_fingerprint: String,
    pub caller: Principal,
    pub graph_name: String,
    pub reserved_graph_id: Option<GraphId>,
    pub requested_resources: Vec<ProvisionableResource>,
    pub state: RouterProvisioningRequestState,
    pub provision_receipt: Option<ProvisionResult>,
    pub created_at_ns: u64,
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum RouterProvisioningRequestStableRecord {
    V1(RouterProvisioningRequest),
}

impl Storable for RouterProvisioningRequest {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&RouterProvisioningRequestStableRecord::V1(self.clone()))
                .expect("encode RouterProvisioningRequest"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&RouterProvisioningRequestStableRecord::V1(self))
            .expect("encode RouterProvisioningRequest")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), RouterProvisioningRequestStableRecord)
            .expect("decode RouterProvisioningRequest")
        {
            RouterProvisioningRequestStableRecord::V1(v1) => v1,
        }
    }
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
