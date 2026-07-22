//! Cross-canister GQL execution wire types (router → graph).
//!
//! IC surface rules (enforced by canister `#[query]` / `#[update]` attributes):
//! - **Query** programs use composite query on the router and `execute_*_query` on graph
//!   (read path; may call index / other canisters).
//! - **Update** programs use update on the router and `execute_*_update` on graph (DML and
//!   posting maintenance). A composite query must not invoke an update method.

use candid::{CandidType, Encode};
use serde::{Deserialize, Serialize};

use crate::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;

use crate::entry::{
    ConstraintNameId, EdgeInlineValueProfile, EdgeLabelId, PropertyId, VertexLabelId,
};
use crate::federation::ShardId;

/// Router-issued mutation id. `0` is reserved; ids are never reused.
pub type MutationId = u64;

/// Shard-local label stats delta sequence. `0` is reserved; ids are never reused.
pub type ShardEventSeq = u64;

/// Maximum UTF-8 byte length of an error returned by the typed V1 batch endpoint.
///
/// Typed admission includes this bound in its worst-case response-size proof. Keep the truncation
/// policy beside the public wire contract so the classifier and Graph response path cannot drift.
pub const MAX_TYPED_BATCH_ERROR_BYTES: usize = 4 * 1024;

/// Bound one typed-batch error without splitting a UTF-8 code point.
pub fn bound_typed_batch_error(mut error: String) -> String {
    if error.len() <= MAX_TYPED_BATCH_ERROR_BYTES {
        return error;
    }
    let mut end = MAX_TYPED_BATCH_ERROR_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error.truncate(end);
    error
}

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
    /// Per-graph key for ELEMENT_ID/path id encoding.
    pub element_id_encoding_key: [u8; 16],
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
    /// Router-sourced indexed-property catalog for this operation (ADR 0023 D1/D3).
    /// Consulted ephemerally by shard DML to decide which postings to maintain.
    pub indexed_properties: Option<crate::index::IndexedPropertyCatalog>,
    /// Cross-shard uniqueness claims the shard must `Acquire` for the element it creates in this
    /// segment (ADR 0030 slice 5). The Router has already reserved each `(constraint_id,
    /// encoded_value)` via the no-`await` Try; the shard mints `ClaimId(mutation_id, claim_ordinal)`
    /// and pins one `Acquire` receipt per claim so the Router can Confirm it. `None`/empty when the
    /// operation touches no constrained property.
    pub unique_claims: Option<Vec<UniqueClaimDispatch>>,
    /// Constrained `(vertex_label, property)` set the shard consults when this segment can delete or
    /// remove a constrained element, so it can pin one `Release` receipt per freed value (ADR 0030
    /// slice 5b). Like `indexed_properties` this is an ephemeral per-operation slice of the Router's
    /// constraint catalog (no persistent shard-side catalog; ADR 0023). `None`/empty when the
    /// operation cannot release a constrained value.
    pub constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
    /// `ShardLocalGlobal` fast-path claims (ADR 0030 slice 10). Unlike `unique_claims`, these were
    /// **not** reserved through the Router (no Try/Acquire/Confirm). The single owning shard enforces
    /// graph-wide uniqueness entirely in its local unique table: it preflights every claim against
    /// the table and, only if all are clean, inserts them inside the same canonical write segment.
    /// `None`/empty when no constrained property uses the `ShardLocalGlobal` strategy.
    pub local_unique_claims: Option<Vec<UniqueClaimDispatch>>,
    /// Constrained `(vertex_label, property)` set for `ShardLocalGlobal` constraints (ADR 0030 slice
    /// 10). A delete/remove of such an element frees its value directly in the local unique table
    /// (owner-matched), rather than pinning an outbox `Release`. `None`/empty when no constrained
    /// property uses the `ShardLocalGlobal` strategy.
    pub local_constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
    /// Router-sourced indexed-embedding catalog for this operation (ADR 0031 Slice 3). Mirrors
    /// `indexed_properties`: an ephemeral per-operation slice the shard consults to decide which
    /// derived vector-embedding mutations to dispatch. In Slice 3 the Router builder is fail-closed
    /// (always `None`/empty) until delete-spanning incarnation fencing activates dispatch, so
    /// production shards never receive a non-empty catalog and vector sync stays inert.
    pub indexed_embeddings: Option<crate::vector_index::IndexedEmbeddingCatalog>,
    /// Router-resolved non-leading vector search hits for `PlanOp::Search` (ADR 0034 Slice 5).
    /// Per-shard shard-local relation containing the bound vertex id and the user-visible scalar.
    /// `None` for plans without a supported non-leading `SEARCH`.
    pub resolved_search_blob: Option<Vec<u8>>,
}

/// A bounded group of independent plan executions sent in one Router → Graph call.
///
/// Each item retains its own mutation identity and execution payload. The Graph executes items
/// independently; this type is a transport aggregation only and does not make the group atomic.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanBatchArgs {
    pub operations: Vec<ExecutePlanArgs>,
    pub mode: ExecutePlanBatchMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ExecutePlanBatchMode {
    Fixed,
    Dynamic,
}

/// Per-item outcomes for [`ExecutePlanBatchArgs`]. Keeping the result at item granularity lets the
/// Router continue its existing saga/recovery handling after a later item fails.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanBatchResult {
    pub results: Vec<Result<ExecutePlanResult, String>>,
    /// Index of the first operation not attempted, when Dynamic mode hit the Graph budget.
    pub next_index: Option<u32>,
}

/// Router → graph: shared typed bulk execution envelope (ADR 0047).
///
/// This is the production transport for homogeneous groups where every operation has the same
/// target shard and shares immutable plan/catalog context. Per-operation data is reduced to the
/// params blob and the already-decoded complete-row seed relation.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanBatchTypedArgs {
    pub shared: ExecutePlanBatchTypedShared,
    pub operations: Vec<ExecutePlanTypedOp>,
    pub batch_mode: ExecutePlanBatchMode,
}

/// Immutable context shared by every operation in a typed bulk group.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanBatchTypedShared {
    pub target_shard_id: ShardId,
    /// Per-graph key for ELEMENT_ID/path id encoding.
    pub element_id_encoding_key: [u8; 16],
    /// Router-issued idempotency key for the whole bulk group.
    pub mutation_id: MutationId,
    pub plan_blob: Vec<u8>,
    /// Router-resolved label names referenced by the physical plan.
    pub resolved_labels: Option<ResolvedLabelTable>,
    /// Router-resolved property names referenced by the physical plan.
    pub resolved_properties: Option<ResolvedPropertyTable>,
    /// Router-sourced indexed-property catalog for this operation (ADR 0023 D1/D3).
    pub indexed_properties: Option<crate::index::IndexedPropertyCatalog>,
}

/// One typed bulk operation with an already-decoded complete-row seed.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanTypedOp {
    /// Per-operation GQL parameter map, already encoded.
    pub params_blob: Vec<u8>,
    /// Required complete-row seed relation. Zero matches use an empty `rows` vector.
    pub seed: SeedBindingsWire,
}

impl ExecutePlanBatchTypedArgs {
    /// Structural validation shared by Router admission and Graph entry.
    ///
    /// Checks cardinality, complete-row shape, per-operation bounds, and the encoded request size.
    /// It does not re-encode individual seeds; the one full-request encode is the only byte proof.
    pub fn validate(&self) -> Result<(), String> {
        const MAX_OPS: usize = 1024;
        const MAX_ROWS_PER_SEED: usize = 1024;
        let ops = self.operations.len();
        if ops == 0 || ops > MAX_OPS {
            return Err(format!(
                "typed batch V1 requires 1..={MAX_OPS} operations, got {ops}"
            ));
        }
        for (i, op) in self.operations.iter().enumerate() {
            if !op.seed.entries.is_empty() {
                return Err(format!(
                    "typed batch V1 op {i} contains grouped seed entries"
                ));
            }
            if !op.seed.complete_prefix_rows {
                return Err(format!(
                    "typed batch V1 op {i} requires complete_prefix_rows=true"
                ));
            }
            if op.seed.rows.len() > MAX_ROWS_PER_SEED {
                return Err(format!(
                    "typed batch V1 op {i} exceeds {MAX_ROWS_PER_SEED} seed rows"
                ));
            }
            if op.params_blob.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
                return Err(format!(
                    "typed batch V1 op {i} params exceed safe payload bound"
                ));
            }
        }
        let encoded =
            Encode!(self).map_err(|e| format!("typed batch V1 request encode failed: {e}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err(format!(
                "typed batch V1 request exceeds the safe payload limit of {}",
                MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
            ));
        }
        Ok(())
    }
}

/// Graph canister execution capabilities advertised to the Router (ADR 0047).
///
/// This response is intentionally explicit: each capability is a named, versioned boolean
/// so that Router activation remains fail-closed and future capabilities are added only when
/// their semantics are known.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphExecutionCapabilities {
    pub typed_seed_batch_v1: bool,
}

/// One cross-shard uniqueness claim dispatched to the shard for `Acquire` (ADR 0030 slice 5).
///
/// `claim_ordinal` is the claim's deterministic position within the mutation; combined with the
/// envelope's `mutation_id` it yields the immutable `ClaimId` the Router reserved. `encoded_value`
/// is the canonical key the Router already validated and reserved, carried verbatim so the shard's
/// pinned receipt and the Router's reservation reference identical bytes.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct UniqueClaimDispatch {
    pub claim_ordinal: u32,
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
}

/// One constrained `(vertex_label, property)` dispatched to the shard so a delete/remove can pin a
/// `Release` for the freed value (ADR 0030 slice 5b).
///
/// The ids are Router-interned and match the shard's stored vertex labels/property ids verbatim
/// (the Router is the sole interner; it ships `ResolvedLabelTable`/`ResolvedPropertyTable` and the
/// shard persists those same ids), so the shard matches a deleted vertex's labels/properties with
/// no translation. `constraint_id` is the reservation-key constraint the freed value belongs to.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ConstrainedPropertyDispatch {
    pub vertex_label_id: VertexLabelId,
    pub property_id: PropertyId,
    pub constraint_id: ConstraintNameId,
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ExecutePlanResult {
    pub row_count: u64,
    /// Candid-encoded [`gleaph_gql_ic::IcWirePlanQueryResult`]; set on query shard execution.
    pub rows_blob: Option<Vec<u8>>,
    /// Forward out-adjacency hubs from a DML batch (router P3 auto-finalize hint).
    pub hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
}

/// Federated mutation lifecycle phase (ADR 0029).
///
/// Router owns the transitions; this is the wire projection a client receives for an
/// idempotent mutation. It is deliberately distinct from [`MutationJournalState`], which
/// only attests a *shard-local* replayable outcome and never describes cross-canister
/// projection convergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MutationLifecyclePhase {
    /// Router is resolving and durably recording the immutable dispatch envelope.
    Routing,
    /// At least one required canonical shard outcome is not yet known.
    CanonicalPending,
    /// All required canonical shard writes are durable; no projection has advanced yet.
    CanonicalCommitted,
    /// Canonical writes are durable; one or more required derived projections still lag.
    ProjectionPending,
    /// Canonical writes and every projection required by the mutation contract converged.
    Completed,
    /// Validation or execution failed before any canonical write committed.
    Failed,
}

/// Read-your-writes token for a federated mutation (ADR 0029 §5, Phase 2).
///
/// Issued with an idempotent DML result. It names the mutation and the per-shard
/// projection watermarks a later read must reach to observe this mutation's effects.
/// It is deliberately **not** a global snapshot timestamp: graph-index freshness is
/// keyed by the monotonic `mutation_id` (a shard's index work for `mutation_id` is
/// applied once its repair watermark passes it), and label-stats freshness by each
/// shard's delta [`ShardEventSeq`]. Phase 2 *issues* the token; Phase 3 enforces it via
/// [`ReadMode::AtLeast`].
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MutationToken {
    pub mutation_id: MutationId,
    pub shards: Vec<MutationTokenShard>,
}

/// Read freshness contract a caller selects per read (ADR 0029 §5, Phase 3).
///
/// This lives at the Gleaph integration boundary, not in the generic GQL crates:
/// it is keyed by Gleaph-specific projection watermarks (`MutationToken`).
///
/// - [`ReadMode::Eventual`] is non-blocking and may observe documented projection lag
///   (count-only under-count, posting lag). It is the default and matches the
///   historical `gql_query` behavior.
/// - [`ReadMode::AtLeast`] enforces a read barrier: the read is served only once every
///   shard in the token has caught its label-stats and graph-index watermarks; otherwise
///   the router returns a retryable projection-lag error without serving stale state.
/// - [`ReadMode::Canonical`] requests owner-served truth for every shape. It is **not yet
///   implemented** (Phase 3 deferred); the router rejects it so callers do not silently
///   receive `Eventual` semantics under a stronger label.
#[derive(Clone, Debug, PartialEq, Eq, Default, CandidType, Serialize, Deserialize)]
pub enum ReadMode {
    /// Non-blocking; may observe documented projection lag.
    #[default]
    Eventual,
    /// Block (retryable) until every shard reaches the token's watermarks.
    AtLeast(MutationToken),
    /// Owner-served truth for every shape (deferred; rejected by the router for now).
    Canonical,
}

/// Per-shard watermarks a read must reach for read-your-writes against one mutation.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct MutationTokenShard {
    pub shard_id: ShardId,
    /// Highest label-stats delta seq this mutation emitted on the shard. The Router
    /// label-stats projection must reach this seq to satisfy a count-only
    /// read-your-writes. `None` when the mutation emitted no label-stats delta here.
    pub label_stats_seq: Option<ShardEventSeq>,
}

/// Router read-path result: merged row count and optional materialized rows.
///
/// `phase` is populated only for idempotent mutations, where Router tracks a federated
/// saga; it is `None` for read queries and for non-idempotent escape-hatch writes that
/// carry no tracked mutation record (ADR 0029).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GqlQueryResult {
    pub row_count: u64,
    /// Candid-encoded [`gleaph_gql_ic::IcWirePlanQueryResult`] after federated merge.
    pub rows_blob: Option<Vec<u8>>,
    /// Federated mutation lifecycle phase for idempotent mutations (ADR 0029).
    pub phase: Option<MutationLifecyclePhase>,
    /// Read-your-writes token for idempotent mutations (ADR 0029 §5, Phase 2). `None`
    /// for reads and untracked escape-hatch writes.
    pub token: Option<MutationToken>,
}

impl GqlQueryResult {
    pub fn from_merged(merged: &ExecutePlanResult) -> Self {
        Self {
            row_count: merged.row_count,
            rows_blob: merged.rows_blob.clone(),
            phase: None,
            token: None,
        }
    }

    pub fn row_count_only(row_count: u64) -> Self {
        Self {
            row_count,
            rows_blob: None,
            phase: None,
            token: None,
        }
    }

    /// Attach a federated mutation lifecycle phase (ADR 0029).
    #[must_use]
    pub fn with_phase(mut self, phase: MutationLifecyclePhase) -> Self {
        self.phase = Some(phase);
        self
    }

    /// Attach a read-your-writes mutation token (ADR 0029 §5, Phase 2).
    #[must_use]
    pub fn with_token(mut self, token: MutationToken) -> Self {
        self.token = Some(token);
        self
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
///
/// This is a *shard-local* idempotency outcome, not a cross-canister status. `Completed`
/// here means the shard-local canonical mutation outcome is durable and replayable; it
/// does **not** imply that derived projections (graph-index postings, Router label stats)
/// have converged. Cross-canister convergence is tracked separately by Router's
/// [`MutationLifecyclePhase`] (ADR 0029).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum MutationJournalState {
    Incomplete,
    Completed,
}

/// Versioned graph shard mutation idempotency journal entry (ADR 0015, ADR 0044).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphMutationJournalEntryWire {
    V1(GraphMutationJournalEntryWireV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphMutationJournalEntryWireV1 {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub row_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    /// Forward hubs observed during DML, persisted so router recovery can still finalize.
    pub hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
    /// Bulk operation cursor: for a bulk mutation, points at the next unexecuted
    /// operation index. For a single mutation it is `None`.
    #[serde(default)]
    pub next_index: Option<u32>,
    /// Bulk-specific progress metadata; present only when `next_index` is used.
    #[serde(default)]
    pub bulk_progress: Option<GraphBulkMutationProgress>,
}

/// Versioned bulk mutation progress metadata stored in a Graph journal entry (ADR 0044).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum GraphBulkMutationProgress {
    V1(GraphBulkMutationProgressV1),
}

#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GraphBulkMutationProgressV1 {
    pub operation_count: u32,
    pub completed_count: u32,
}

impl GraphMutationJournalEntryWire {
    pub fn new(
        mutation_id: MutationId,
        state: MutationJournalState,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<crate::federation::LocalVertexId>,
    ) -> Self {
        Self::V1(GraphMutationJournalEntryWireV1 {
            mutation_id,
            state,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            next_index: None,
            bulk_progress: None,
        })
    }

    fn as_v1(&self) -> &GraphMutationJournalEntryWireV1 {
        match self {
            GraphMutationJournalEntryWire::V1(v1) => v1,
        }
    }

    fn as_v1_mut(&mut self) -> &mut GraphMutationJournalEntryWireV1 {
        match self {
            GraphMutationJournalEntryWire::V1(v1) => v1,
        }
    }

    pub fn mutation_id(&self) -> MutationId {
        self.as_v1().mutation_id
    }
    pub fn state(&self) -> MutationJournalState {
        self.as_v1().state
    }
    pub fn row_count(&self) -> u64 {
        self.as_v1().row_count
    }
    pub fn emitted_delta_first_seq(&self) -> Option<ShardEventSeq> {
        self.as_v1().emitted_delta_first_seq
    }
    pub fn emitted_delta_last_seq(&self) -> Option<ShardEventSeq> {
        self.as_v1().emitted_delta_last_seq
    }
    pub fn hot_forward_vertices(&self) -> &Vec<crate::federation::LocalVertexId> {
        &self.as_v1().hot_forward_vertices
    }
    pub fn next_index(&self) -> Option<u32> {
        self.as_v1().next_index
    }
    pub fn bulk_progress(&self) -> &Option<GraphBulkMutationProgress> {
        &self.as_v1().bulk_progress
    }

    pub fn set_state(&mut self, state: MutationJournalState) {
        self.as_v1_mut().state = state;
    }
    pub fn set_row_count(&mut self, row_count: u64) {
        self.as_v1_mut().row_count = row_count;
    }
    pub fn set_emitted_delta_first_seq(&mut self, seq: Option<ShardEventSeq>) {
        self.as_v1_mut().emitted_delta_first_seq = seq;
    }
    pub fn set_emitted_delta_last_seq(&mut self, seq: Option<ShardEventSeq>) {
        self.as_v1_mut().emitted_delta_last_seq = seq;
    }
    pub fn set_hot_forward_vertices(&mut self, vertices: Vec<crate::federation::LocalVertexId>) {
        self.as_v1_mut().hot_forward_vertices = vertices;
    }
    pub fn set_next_index(&mut self, next_index: Option<u32>) {
        self.as_v1_mut().next_index = next_index;
    }
    pub fn set_bulk_progress(&mut self, bulk_progress: Option<GraphBulkMutationProgress>) {
        self.as_v1_mut().bulk_progress = bulk_progress;
    }
}

impl GraphBulkMutationProgress {
    pub fn new(operation_count: u32, completed_count: u32) -> Self {
        Self::V1(GraphBulkMutationProgressV1 {
            operation_count,
            completed_count,
        })
    }

    pub fn operation_count(&self) -> u32 {
        match self {
            GraphBulkMutationProgress::V1(v1) => v1.operation_count,
        }
    }

    pub fn completed_count(&self) -> u32 {
        match self {
            GraphBulkMutationProgress::V1(v1) => v1.completed_count,
        }
    }
}

/// Router → graph: read a batch of mutation journal entries in one call.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GetMutationJournalEntriesArgs {
    pub mutation_ids: Vec<MutationId>,
}

/// Graph → router: ordered optional journal entries for [`GetMutationJournalEntriesArgs`].
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct GetMutationJournalEntriesResult {
    pub entries: Vec<Option<GraphMutationJournalEntryWire>>,
    /// Smallest mutation id not included because the Graph canister neared its instruction budget.
    /// When present, the Router must issue a follow-up batch read for this and larger ids.
    pub next: Option<MutationId>,
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

/// Physical field descriptor for one fixed-size inline edge STRUCT slot.
///
/// Router derives this from the canonical declaration order; Graph receives it as a plan-scoped
/// projection and must not persist or infer it. Each descriptor carries only the data Graph needs
/// to validate and decode the payload slice: field name, byte offset, and exact scalar profile.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedInlineStructField {
    pub name: String,
    pub byte_offset: u16,
    pub profile: EdgeInlineValueProfile,
}

/// Router-derived resolved schema for the named inline edge property of one concrete label.
///
/// Replaces the ambiguous `Option<PropertyId>` parallel wire state with one explicit enum:
/// - `None`: this label has no named inline property.
/// - `Scalar { property_id }`: one fixed-width scalar inline property.
/// - `Struct { property_id, fields }`: one fixed-size inline STRUCT, declaration-ordered.
///
/// Graph receives this as a plan-scoped projection and must not persist or infer it.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub enum ResolvedInlineSchema {
    #[serde(rename = "scalar")]
    Scalar { property_id: PropertyId },
    #[serde(rename = "struct")]
    Struct {
        property_id: PropertyId,
        fields: Vec<ResolvedInlineStructField>,
    },
}

impl ResolvedInlineSchema {
    /// The inline property identity for this schema, regardless of scalar or struct shape.
    #[inline]
    pub fn property_id(&self) -> PropertyId {
        match self {
            Self::Scalar { property_id } | Self::Struct { property_id, .. } => *property_id,
        }
    }

    /// True when this resolved schema is a struct projection.
    #[inline]
    pub fn is_struct(&self) -> bool {
        matches!(self, Self::Struct { .. })
    }

    /// True when this resolved schema is a scalar projection.
    #[inline]
    pub fn is_scalar(&self) -> bool {
        matches!(self, Self::Scalar { .. })
    }
}

#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedEdgeLabel {
    pub name: String,
    pub id: EdgeLabelId,
    /// Router-owned logical schema (ADR 0008). Default `no_inline_value` when omitted on legacy wire.
    pub inline_value_profile: EdgeInlineValueProfile,
    /// Router-derived named inline property schema for this concrete edge label (ADR 0034 Slices 21/24/25).
    /// `None` for labels with no named inline slot; otherwise a scalar or struct projection.
    /// Graph receives this as a plan-scoped projection and must not persist or infer it.
    pub inline_schema: Option<ResolvedInlineSchema>,
}

impl ResolvedEdgeLabel {
    pub fn new(
        name: impl Into<String>,
        id: EdgeLabelId,
        inline_value_profile: EdgeInlineValueProfile,
    ) -> Self {
        Self::with_inline_schema(name, id, inline_value_profile, None)
    }

    pub fn with_inline_schema(
        name: impl Into<String>,
        id: EdgeLabelId,
        inline_value_profile: EdgeInlineValueProfile,
        inline_schema: Option<ResolvedInlineSchema>,
    ) -> Self {
        Self {
            name: name.into(),
            id,
            inline_value_profile,
            inline_schema,
        }
    }

    /// Scalar convenience constructor: builds a `Scalar { property_id }` resolved inline schema.
    pub fn with_inline_property(
        name: impl Into<String>,
        id: EdgeLabelId,
        inline_value_profile: EdgeInlineValueProfile,
        inline_property_id: Option<PropertyId>,
    ) -> Self {
        let inline_schema =
            inline_property_id.map(|property_id| ResolvedInlineSchema::Scalar { property_id });
        Self::with_inline_schema(name, id, inline_value_profile, inline_schema)
    }

    /// The inline property identity projected from Router schema, if any.
    #[inline]
    pub fn inline_property_id(&self) -> Option<PropertyId> {
        self.inline_schema
            .as_ref()
            .map(ResolvedInlineSchema::property_id)
    }

    /// The resolved inline schema projection, if any.
    #[inline]
    pub fn inline_schema(&self) -> Option<&ResolvedInlineSchema> {
        self.inline_schema.as_ref()
    }
}

impl ResolvedLabelTable {
    pub fn edge_inline_value_profile(&self, id: EdgeLabelId) -> Option<&EdgeInlineValueProfile> {
        self.edge
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| &entry.inline_value_profile)
    }

    pub fn resolved_edge_label(&self, id: EdgeLabelId) -> Option<&ResolvedEdgeLabel> {
        self.edge.iter().find(|entry| entry.id == id)
    }

    pub fn edge_label_ids_with_nonzero_payload(&self) -> Vec<EdgeLabelId> {
        self.edge
            .iter()
            .filter(|entry| entry.inline_value_profile.required_byte_width() > 0)
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
    pub local_vertex_ids: Vec<u32>,
    pub local_edge_postings: Vec<LocalEdgePosting>,
}

/// One vertex binding inside a row-shaped seed, with optional label constraints enforced during
/// hydration. Carrying the label ids on the seed row lets the Router express a leading
/// `NodeScan(variable, label = Some(...))` without leaking label-name resolution into the graph
/// canister. Label ids are stored as raw `u16` because Candid does not subtype through the
/// `VertexLabelId` newtype inside a vector.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SeedVertexBinding {
    pub variable: String,
    pub local_vertex_id: u32,
    pub required_vertex_label_ids: Vec<u16>,
}

/// One scalar binding inside a row-shaped seed. Used to carry a `SEARCH ... SCORE/DISTANCE AS alias`
/// value alongside its matched vertex binding without requiring a second grouped seed entry.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct SeedFloat64Binding {
    pub variable: String,
    pub value: f64,
}

/// One complete seed row produced by Router-side vector-search lowering. Each hit becomes one row
/// carrying the matched vertex and the score/distance alias. Row-shaped seeds are processed
/// independently; a row is skipped if any of its required vertex bindings is missing, tombstoned, or
/// fails the required label check.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct SeedRowWire {
    pub vertex_bindings: Vec<SeedVertexBinding>,
    pub float64_bindings: Vec<SeedFloat64Binding>,
}

/// Router → graph seed bindings. `entries` is the legacy grouped-anchor path; `rows` is the
/// row-shaped path introduced for GQL `SEARCH` lowering. Both may be present; a plan that uses row
/// seeds has already had its leading anchor stripped, so the graph executor consumes only `rows`.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct SeedBindingsWire {
    pub entries: Vec<SeedBindingEntry>,
    pub rows: Vec<SeedRowWire>,
    /// When true, `rows` are complete for the entire read prefix and the Graph executor may skip
    /// the whole prefix rather than only the leading index-anchor ops. Introduced for ADR 0046
    /// Phase 1 multi-variable seed relations; `false` preserves the legacy `SEARCH`/single-variable
    /// semantics. Missing field decodes as `false` for stable blobs encoded before this addition.
    #[serde(default)]
    pub complete_prefix_rows: bool,
}

/// One vertex hit inside a Router-resolved non-leading `SEARCH` relation (ADR 0034 Slice 5).
/// Carries only the provider-neutral shard-local vertex id and the user-visible scalar value.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedSearchVertexHitWire {
    pub local_vertex_id: u32,
    pub value: f64,
}

/// Router-resolved relation for one non-leading `PlanOp::Search` (ADR 0034 Slice 5).
///
/// `binding` names the vertex variable that must already be bound when the operator executes.
/// `output_alias` names the scalar binding to add to each surviving row. The Graph executor joins
/// `input_rows[d]` against `vertex_hits.local_vertex_id` and binds `output_alias` to the matching
/// `value`.
#[derive(Clone, Debug, PartialEq, CandidType, Serialize, Deserialize)]
pub struct ResolvedSearchWire {
    pub binding: String,
    pub output_alias: String,
    pub vertex_hits: Vec<ResolvedSearchVertexHitWire>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EdgeInlineValueEncoding;
    use crate::federation::ElementIdEncodingKey;
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
    fn execute_plan_batch_result_roundtrip_preserves_ordered_partial_outcomes() {
        let result = ExecutePlanBatchResult {
            results: vec![
                Ok(ExecutePlanResult {
                    row_count: 3,
                    rows_blob: None,
                    hot_forward_vertices: vec![9],
                }),
                Err("item failed".to_string()),
            ],
            next_index: Some(1),
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: ExecutePlanBatchResult =
            Decode!(&bytes, ExecutePlanBatchResult).expect("decode");
        assert_eq!(result, decoded);
    }

    #[test]
    fn typed_batch_error_bound_is_utf8_safe_and_exact() {
        let prefix = "x".repeat(MAX_TYPED_BATCH_ERROR_BYTES - 1);
        let bounded = bound_typed_batch_error(format!("{prefix}étail"));
        assert_eq!(bounded.len(), MAX_TYPED_BATCH_ERROR_BYTES - 1);
        assert_eq!(bounded, prefix);
        assert_eq!(
            bound_typed_batch_error("short error".to_string()),
            "short error"
        );
    }

    #[test]
    fn execute_plan_batch_args_roundtrip_preserves_dynamic_mode() {
        let args = ExecutePlanBatchArgs {
            operations: Vec::new(),
            mode: ExecutePlanBatchMode::Dynamic,
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanBatchArgs = Decode!(&bytes, ExecutePlanBatchArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn mutation_token_candid_roundtrip() {
        let token = MutationToken {
            mutation_id: 42,
            shards: vec![
                MutationTokenShard {
                    shard_id: ShardId::new(0),
                    label_stats_seq: Some(7),
                },
                MutationTokenShard {
                    shard_id: ShardId::new(1),
                    label_stats_seq: None,
                },
            ],
        };
        let bytes = Encode!(&token).expect("encode");
        let decoded: MutationToken = Decode!(&bytes, MutationToken).expect("decode");
        assert_eq!(token, decoded);
    }

    #[test]
    fn read_mode_candid_roundtrip_all_variants() {
        for mode in [
            ReadMode::Eventual,
            ReadMode::Canonical,
            ReadMode::AtLeast(MutationToken {
                mutation_id: 11,
                shards: vec![MutationTokenShard {
                    shard_id: ShardId::new(3),
                    label_stats_seq: Some(4),
                }],
            }),
        ] {
            let bytes = Encode!(&mode).expect("encode");
            let decoded: ReadMode = Decode!(&bytes, ReadMode).expect("decode");
            assert_eq!(mode, decoded);
        }
        assert_eq!(ReadMode::default(), ReadMode::Eventual);
    }

    #[test]
    fn gql_query_result_carries_phase_and_token() {
        let result = GqlQueryResult::row_count_only(3)
            .with_phase(MutationLifecyclePhase::ProjectionPending)
            .with_token(MutationToken {
                mutation_id: 9,
                shards: vec![MutationTokenShard {
                    shard_id: ShardId::new(2),
                    label_stats_seq: Some(5),
                }],
            });
        let bytes = Encode!(&result).expect("encode");
        let decoded: GqlQueryResult = Decode!(&bytes, GqlQueryResult).expect("decode");
        assert_eq!(result, decoded);
        assert_eq!(
            decoded.phase,
            Some(MutationLifecyclePhase::ProjectionPending)
        );
        assert_eq!(decoded.token.expect("token").mutation_id, 9);
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
            rows: vec![SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "d".into(),
                    local_vertex_id: 7,
                    required_vertex_label_ids: vec![3],
                }],
                float64_bindings: vec![SeedFloat64Binding {
                    variable: "distance".into(),
                    value: 1.5,
                }],
            }],
            complete_prefix_rows: false,
        };
        let seed_blob = Encode!(&seed).expect("seed encode");
        let args = ExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
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
                edge: vec![ResolvedEdgeLabel::with_inline_property(
                    "KNOWS",
                    EdgeLabelId::from_raw(1),
                    EdgeInlineValueProfile::no_inline_value(),
                    None,
                )],
            }),
            resolved_properties: Some(ResolvedPropertyTable {
                properties: vec![ResolvedProperty {
                    name: "name".into(),
                    id: PropertyId::from_raw(1),
                }],
            }),
            indexed_properties: None,
            unique_claims: Some(vec![UniqueClaimDispatch {
                claim_ordinal: 0,
                constraint_id: ConstraintNameId::from_raw(3),
                encoded_value: vec![9, 8, 7],
            }]),
            constrained_properties: Some(vec![ConstrainedPropertyDispatch {
                vertex_label_id: VertexLabelId::from_raw(2),
                property_id: PropertyId::from_raw(1),
                constraint_id: ConstraintNameId::from_raw(3),
            }]),
            local_unique_claims: Some(vec![UniqueClaimDispatch {
                claim_ordinal: 0,
                constraint_id: ConstraintNameId::from_raw(4),
                encoded_value: vec![5, 6],
            }]),
            local_constrained_properties: Some(vec![ConstrainedPropertyDispatch {
                vertex_label_id: VertexLabelId::from_raw(2),
                property_id: PropertyId::from_raw(1),
                constraint_id: ConstraintNameId::from_raw(4),
            }]),
            indexed_embeddings: Some(crate::vector_index::IndexedEmbeddingCatalog {
                embeddings: vec![crate::vector_index::IndexedEmbeddingSpec {
                    embedding_name_id: 5,
                    index_id: 11,
                    kind: crate::vector_index::VectorIndexKind::IvfFlat,
                    metric: crate::vector_index::VectorMetric::L2Squared,
                    encoding: crate::vector_index::VectorEncoding::F32,
                    dims: 16,
                }],
            }),
            resolved_search_blob: None,
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn resolved_search_wire_roundtrip() {
        let wire = ResolvedSearchWire {
            binding: "d".into(),
            output_alias: "similarity".into(),
            vertex_hits: vec![
                ResolvedSearchVertexHitWire {
                    local_vertex_id: 7,
                    value: 0.75,
                },
                ResolvedSearchVertexHitWire {
                    local_vertex_id: 42,
                    value: f64::NEG_INFINITY,
                },
            ],
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: ResolvedSearchWire = Decode!(&bytes, ResolvedSearchWire).expect("decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn resolved_search_wire_empty_hits_roundtrip() {
        let wire = ResolvedSearchWire {
            binding: "d".into(),
            output_alias: "distance".into(),
            vertex_hits: Vec::new(),
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: ResolvedSearchWire = Decode!(&bytes, ResolvedSearchWire).expect("decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn execute_plan_args_legacy_blob_without_resolved_search_decodes_as_none() {
        #[derive(CandidType, Serialize)]
        struct LegacyExecutePlanArgs {
            target_shard_id: ShardId,
            element_id_encoding_key: [u8; 16],
            mutation_id: Option<MutationId>,
            plan_blob: Vec<u8>,
            params_blob: Vec<u8>,
            mode: GqlExecutionMode,
            seed_bindings_blob: Option<Vec<u8>>,
            resolved_labels: Option<ResolvedLabelTable>,
            resolved_properties: Option<ResolvedPropertyTable>,
            indexed_properties: Option<crate::index::IndexedPropertyCatalog>,
            unique_claims: Option<Vec<UniqueClaimDispatch>>,
            constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
            local_unique_claims: Option<Vec<UniqueClaimDispatch>>,
            local_constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
            indexed_embeddings: Option<crate::vector_index::IndexedEmbeddingCatalog>,
        }
        let legacy = LegacyExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
            mutation_id: None,
            plan_blob: vec![1, 2],
            params_blob: vec![3],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: None,
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
            local_unique_claims: None,
            local_constrained_properties: None,
            indexed_embeddings: None,
        };
        let bytes = Encode!(&legacy).expect("encode legacy");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode legacy");
        assert_eq!(decoded.resolved_search_blob, None);
    }

    #[test]
    fn execute_plan_args_with_resolved_search_blob_roundtrip() {
        let search_wire = ResolvedSearchWire {
            binding: "d".into(),
            output_alias: "similarity".into(),
            vertex_hits: vec![ResolvedSearchVertexHitWire {
                local_vertex_id: 7,
                value: 0.75,
            }],
        };
        let search_blob = Encode!(&search_wire).expect("search encode");
        let args = ExecutePlanArgs {
            target_shard_id: ShardId::new(0),
            element_id_encoding_key: ElementIdEncodingKey::host_test_fixture().0,
            mutation_id: None,
            plan_blob: vec![1, 2],
            params_blob: vec![3],
            mode: GqlExecutionMode::Query,
            seed_bindings_blob: None,
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
            local_unique_claims: None,
            local_constrained_properties: None,
            indexed_embeddings: None,
            resolved_search_blob: Some(search_blob),
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanArgs = Decode!(&bytes, ExecutePlanArgs).expect("decode");
        assert_eq!(decoded.resolved_search_blob, args.resolved_search_blob);
        let decoded_search: ResolvedSearchWire = Decode!(
            decoded.resolved_search_blob.as_ref().unwrap(),
            ResolvedSearchWire
        )
        .expect("decode inner search wire");
        assert_eq!(decoded_search, search_wire);
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
            rows: vec![SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "d".into(),
                    local_vertex_id: 5,
                    required_vertex_label_ids: vec![2],
                }],
                float64_bindings: vec![SeedFloat64Binding {
                    variable: "score".into(),
                    value: 0.25,
                }],
            }],
            complete_prefix_rows: false,
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(decoded.entries, wire.entries);
        assert_eq!(decoded.rows, wire.rows);
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
            rows: Vec::new(),
            complete_prefix_rows: false,
        };
        let bytes = Encode!(&wire).expect("encode");
        let decoded: SeedBindingsWire = Decode!(&bytes, SeedBindingsWire).expect("decode");
        assert_eq!(decoded.entries, wire.entries);
        assert_eq!(decoded.rows, wire.rows);
    }

    #[test]
    #[should_panic(expected = "field rows is not optional field")]
    fn seed_bindings_wire_entries_only_blob_rejects_decode() {
        // Pre-Slice 3 blobs carried only `entries`. With `rows` as a required field the decoder now
        // rejects them. This is acceptable for Slice 3 because the only stored `seed_bindings_blob`
        // values belong to DML mutation envelopes, and `SEARCH` lowering applies only to read
        // queries.
        #[derive(CandidType, Serialize)]
        struct LegacySeedBindingsWire {
            entries: Vec<SeedBindingEntry>,
        }
        let legacy = LegacySeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "u".into(),
                local_vertex_ids: vec![1, 2],
                local_edge_postings: Vec::new(),
            }],
        };
        let legacy_bytes = Encode!(&legacy).expect("encode legacy wire");
        let _: SeedBindingsWire = Decode!(&legacy_bytes, SeedBindingsWire).expect("decode legacy");
    }
    #[test]
    fn resolved_edge_label_inline_property_id_roundtrip() {
        let label = ResolvedEdgeLabel::with_inline_property(
            "ROAD".to_string(),
            EdgeLabelId::from_raw(7),
            EdgeInlineValueProfile {
                byte_width: 4,
                encoding: EdgeInlineValueEncoding::F32,
            },
            Some(PropertyId::from_raw(42)),
        );
        let bytes = Encode!(&label).expect("encode ResolvedEdgeLabel with inline property id");
        let decoded: ResolvedEdgeLabel = Decode!(&bytes, ResolvedEdgeLabel).expect("decode");
        assert_eq!(decoded, label);
        assert_eq!(decoded.inline_property_id(), Some(PropertyId::from_raw(42)));
        assert!(matches!(
            decoded.inline_schema,
            Some(ResolvedInlineSchema::Scalar { property_id })
            if property_id == PropertyId::from_raw(42)
        ));
    }

    #[test]
    fn resolved_edge_label_struct_schema_roundtrip() {
        let label = ResolvedEdgeLabel::with_inline_schema(
            "AFFINITY".to_string(),
            EdgeLabelId::from_raw(7),
            EdgeInlineValueProfile::opaque_bytes(16),
            Some(ResolvedInlineSchema::Struct {
                property_id: PropertyId::from_raw(42),
                fields: vec![
                    ResolvedInlineStructField {
                        name: "score".to_string(),
                        byte_offset: 0,
                        profile: EdgeInlineValueProfile {
                            byte_width: 4,
                            encoding: EdgeInlineValueEncoding::F32,
                        },
                    },
                    ResolvedInlineStructField {
                        name: "confidence".to_string(),
                        byte_offset: 4,
                        profile: EdgeInlineValueProfile {
                            byte_width: 4,
                            encoding: EdgeInlineValueEncoding::F32,
                        },
                    },
                    ResolvedInlineStructField {
                        name: "updated_at".to_string(),
                        byte_offset: 8,
                        profile: EdgeInlineValueProfile {
                            byte_width: 8,
                            encoding: EdgeInlineValueEncoding::RawU64,
                        },
                    },
                ],
            }),
        );
        let bytes = Encode!(&label).expect("encode ResolvedEdgeLabel with struct schema");
        let decoded: ResolvedEdgeLabel = Decode!(&bytes, ResolvedEdgeLabel).expect("decode");
        assert_eq!(decoded, label);
        assert_eq!(decoded.inline_property_id(), Some(PropertyId::from_raw(42)));
        assert!(
            decoded
                .inline_schema()
                .is_some_and(ResolvedInlineSchema::is_struct)
        );
    }

    #[test]
    fn resolved_label_table_resolves_edge_label_with_inline_id() {
        let table = ResolvedLabelTable {
            vertex: Vec::new(),
            edge: vec![ResolvedEdgeLabel::with_inline_property(
                "ROAD".to_string(),
                EdgeLabelId::from_raw(7),
                EdgeInlineValueProfile {
                    byte_width: 4,
                    encoding: EdgeInlineValueEncoding::F32,
                },
                Some(PropertyId::from_raw(42)),
            )],
        };
        let entry = table
            .resolved_edge_label(EdgeLabelId::from_raw(7))
            .expect("label");
        assert_eq!(entry.inline_property_id(), Some(PropertyId::from_raw(42)));
        assert!(matches!(
            entry.inline_schema,
            Some(ResolvedInlineSchema::Scalar { property_id })
            if property_id == PropertyId::from_raw(42)
        ));
    }
    #[test]
    fn execute_plan_batch_typed_args_roundtrip_and_validation() {
        let args = ExecutePlanBatchTypedArgs {
            shared: ExecutePlanBatchTypedShared {
                target_shard_id: ShardId(1),
                element_id_encoding_key: [0u8; 16],
                mutation_id: 42,
                plan_blob: vec![1, 2, 3],
                resolved_labels: None,
                resolved_properties: None,
                indexed_properties: None,
            },
            operations: vec![ExecutePlanTypedOp {
                params_blob: vec![7, 8, 9],
                seed: SeedBindingsWire {
                    entries: vec![],
                    rows: vec![],
                    complete_prefix_rows: true,
                },
            }],
            batch_mode: ExecutePlanBatchMode::Dynamic,
        };
        args.validate().expect("valid typed args");
        let bytes = Encode!(&args).expect("encode");
        let decoded: ExecutePlanBatchTypedArgs =
            Decode!(&bytes, ExecutePlanBatchTypedArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn execute_plan_batch_typed_args_rejects_grouped_entries() {
        let args = ExecutePlanBatchTypedArgs {
            shared: ExecutePlanBatchTypedShared {
                target_shard_id: ShardId(1),
                element_id_encoding_key: [0u8; 16],
                mutation_id: 42,
                plan_blob: vec![1, 2, 3],
                resolved_labels: None,
                resolved_properties: None,
                indexed_properties: None,
            },
            operations: vec![ExecutePlanTypedOp {
                params_blob: vec![],
                seed: SeedBindingsWire {
                    entries: vec![SeedBindingEntry {
                        variable: "x".into(),
                        local_vertex_ids: vec![1],
                        local_edge_postings: vec![],
                    }],
                    rows: vec![],
                    complete_prefix_rows: true,
                },
            }],
            batch_mode: ExecutePlanBatchMode::Fixed,
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn execute_plan_batch_typed_args_rejects_incomplete_prefix_rows() {
        let args = ExecutePlanBatchTypedArgs {
            shared: ExecutePlanBatchTypedShared {
                target_shard_id: ShardId(1),
                element_id_encoding_key: [0u8; 16],
                mutation_id: 42,
                plan_blob: vec![1, 2, 3],
                resolved_labels: None,
                resolved_properties: None,
                indexed_properties: None,
            },
            operations: vec![ExecutePlanTypedOp {
                params_blob: vec![],
                seed: SeedBindingsWire {
                    entries: vec![],
                    rows: vec![],
                    complete_prefix_rows: false,
                },
            }],
            batch_mode: ExecutePlanBatchMode::Fixed,
        };
        assert!(args.validate().is_err());
    }
}

#[cfg(test)]
mod graph_execution_capabilities_tests {
    use super::GraphExecutionCapabilities;
    use candid::{Decode, Encode};

    #[test]
    fn roundtrip_encodes_typed_seed_batch_v1() {
        let caps = GraphExecutionCapabilities {
            typed_seed_batch_v1: true,
        };
        let bytes = Encode!(&caps).expect("encode capabilities");
        let decoded: GraphExecutionCapabilities =
            Decode!(&bytes, GraphExecutionCapabilities).expect("decode capabilities");
        assert!(decoded.typed_seed_batch_v1);
    }

    #[test]
    fn defaults_typed_seed_batch_v1_to_false() {
        // Candid decodes missing fields via serde default.
        let bytes = Encode!(&GraphExecutionCapabilities {
            typed_seed_batch_v1: false,
        })
        .expect("encode capabilities");
        let decoded: GraphExecutionCapabilities =
            Decode!(&bytes, GraphExecutionCapabilities).expect("decode capabilities");
        assert!(!decoded.typed_seed_batch_v1);
    }
}
