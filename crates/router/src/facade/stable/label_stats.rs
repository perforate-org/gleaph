//! Router-owned label stats aggregates and client mutation records (ADR 0015).

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    MutationId, MutationLifecyclePhase, ResolvedLabelTable, ResolvedPropertyTable,
};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabelStats {
    pub live_count: u64,
    pub total_adds: u64,
    pub total_removes: u64,
}

impl Storable for LabelStats {
    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.live_count.to_le_bytes());
        out.extend_from_slice(&self.total_adds.to_le_bytes());
        out.extend_from_slice(&self.total_removes.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut live = [0; 8];
        let mut adds = [0; 8];
        let mut removes = [0; 8];
        live.copy_from_slice(&bytes[0..8]);
        adds.copy_from_slice(&bytes[8..16]);
        removes.copy_from_slice(&bytes[16..24]);
        Self {
            live_count: u64::from_le_bytes(live),
            total_adds: u64::from_le_bytes(adds),
            total_removes: u64::from_le_bytes(removes),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphLabelKey {
    pub graph_id: GraphId,
    pub label_id: u16,
}

impl GraphLabelKey {
    pub const fn new(graph_id: GraphId, label_id: u16) -> Self {
        Self { graph_id, label_id }
    }
}

impl Storable for GraphLabelKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut label = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        label.copy_from_slice(&bytes[4..6]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphLabelShardKey {
    pub graph_id: GraphId,
    pub shard_id: ShardId,
    pub label_id: u16,
}

impl GraphLabelShardKey {
    pub const fn new(graph_id: GraphId, shard_id: ShardId, label_id: u16) -> Self {
        Self {
            graph_id,
            shard_id,
            label_id,
        }
    }
}

impl Storable for GraphLabelShardKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 10,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut shard = [0; 4];
        let mut label = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        shard.copy_from_slice(&bytes[4..8]);
        label.copy_from_slice(&bytes[8..10]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            shard_id: ShardId::from_le_bytes(shard),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientMutationKey {
    pub caller: Principal,
    pub graph_id: GraphId,
    pub client_key: String,
}

impl ClientMutationKey {
    pub fn new(caller: Principal, graph_id: GraphId, client_key: String) -> Self {
        Self {
            caller,
            graph_id,
            client_key,
        }
    }
}

impl Storable for ClientMutationKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ClientMutationKey"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ClientMutationKey")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ClientMutationKey")
    }
}

/// Reverse index row `mutation_id → (ClientMutationKey, nonterminal reservation count)` (ADR 0030
/// slice 6). It exists **iff** `nonterminal > 0`: created when a mutation's first unique reservation
/// is taken (Try) and removed when its last non-terminal reservation leaves (`FreshlyCommitted`
/// Confirm, or reclaim Cancel). It lets the reclaim reconciler resolve a reservation's `ClaimId`
/// (`mutation_id`) to the owning `RouterMutationRecord`, and pins that record against TTL GC while
/// any non-terminal reservation still depends on it for a terminal-failure decision.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MutationReservationIndexEntry {
    pub client_key: ClientMutationKey,
    pub nonterminal: u32,
}

impl Storable for MutationReservationIndexEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode MutationReservationIndexEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode MutationReservationIndexEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode MutationReservationIndexEntry")
    }
}

/// Versioned Router mutation saga record (ADR 0044).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterMutationRecord {
    V1(RouterMutationRecordV1),
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterMutationRecordV1 {
    pub mutation_id: MutationId,
    pub created_at_ns: u64,
    pub request_fingerprint: Vec<u8>,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub completed_row_count: Option<u64>,
    pub routing_in_progress: bool,
    pub shards: Vec<RouterMutationShard>,
    /// Wall-clock time the current routing lease was acquired (ADR 0029 Phase 4). Set
    /// whenever `routing_in_progress` is flipped to `true`; lets a retry reclaim a routing
    /// reservation whose owner trapped before persisting the dispatch envelope. `None` for
    /// records that never held an active routing lease (pre-Phase-4 records decode as `None`).
    #[serde(default)]
    pub routing_lease_ns: Option<u64>,
    /// Last recovery diagnostic (ADR 0029 Phase 4), surfaced by `mutation_status` for
    /// operators. `None` until a recovery step records why a saga cannot yet converge.
    #[serde(default)]
    pub last_error: Option<String>,
    /// **Irreversible** terminal-failure marker (ADR 0030 slice 6). `Some(error)` means the
    /// mutation failed permanently and must **not** be re-dispatched under this client key — a
    /// retry returns the stored error verbatim, so only a *new* client key may attempt the work
    /// again. Distinct from the *retryable* `Failed` lifecycle phase (`shards.is_empty() &&
    /// completed_row_count.is_none()`), which a same-key retry can still re-route. It is the only
    /// state the reclaim reconciler may use as Cancel grounds: it guarantees no later canonical
    /// dispatch for this mutation can still arrive and commit after the proof's absence read.
    #[serde(default)]
    pub terminal_failure: Option<String>,
    /// True when this record represents a bulk group of operations sharing the same plan
    /// and dispatched under a single mutation id (ADR 0044).
    #[serde(default)]
    pub is_bulk: bool,
    /// Bulk-specific state; present only when `is_bulk` is true.
    #[serde(default)]
    pub bulk_state: Option<RouterBulkMutationState>,
}

/// Versioned bulk mutation state attached to a Router saga record (ADR 0044).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RouterBulkMutationState {
    V1(RouterBulkMutationStateV1),
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterBulkMutationStateV1 {
    /// Total number of operations in the bulk group.
    pub total_ops: u32,
}

impl RouterMutationRecord {
    pub fn new(mutation_id: MutationId, created_at_ns: u64, request_fingerprint: Vec<u8>) -> Self {
        Self::V1(RouterMutationRecordV1 {
            mutation_id,
            created_at_ns,
            request_fingerprint,
            resolved_labels: None,
            resolved_properties: None,
            completed_row_count: None,
            routing_in_progress: true,
            shards: Vec::new(),
            routing_lease_ns: Some(created_at_ns),
            last_error: None,
            terminal_failure: None,
            is_bulk: false,
            bulk_state: None,
        })
    }

    pub(crate) fn as_v1(&self) -> &RouterMutationRecordV1 {
        match self {
            RouterMutationRecord::V1(v1) => v1,
        }
    }

    pub(crate) fn as_v1_mut(&mut self) -> &mut RouterMutationRecordV1 {
        match self {
            RouterMutationRecord::V1(v1) => v1,
        }
    }

    /// `true` once the saga reaches a terminal phase. An irreversible `terminal_failure` takes
    /// priority (it forces [`Self::lifecycle_phase`] to `Failed`); otherwise terminality is the
    /// progress-derived `Completed`/`Failed`. Terminal records are the only ones eligible for TTL
    /// eviction (gated additionally by the non-terminal reservation count in ADR 0030 slice 6);
    /// non-terminal sagas are retained as recovery targets (ADR 0029 Phase 4).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.lifecycle_phase(),
            MutationLifecyclePhase::Completed | MutationLifecyclePhase::Failed
        )
    }

    /// `true` once the saga is **irreversibly** terminally failed (ADR 0030 slice 6): a same-key
    /// retry returns the stored error rather than re-dispatching.
    pub fn is_terminally_failed(&self) -> bool {
        self.as_v1().terminal_failure.is_some()
    }

    /// `true` while a unique-reservation-holding mutation is eligible to be flipped to irreversible
    /// `terminal_failure` by the reclaim reconciler (ADR 0030 slice 6): a durable dispatch envelope
    /// exists, **no** shard's canonical write has committed, routing is released, and it is not
    /// already terminal-failed.
    ///
    /// Try runs *after* the dispatch envelope is recorded, so a reservation-holding record always
    /// has `shards` populated — the reachable predicate is "envelope present **and** no completed
    /// canonical shard", not `shards.is_empty()`. The proof's other half (every `proof_scope` shard
    /// reachable and reporting the `Acquire` absent) is the caller's responsibility; this is only the
    /// record-side gate. Any other state (`Routing`, a completed canonical shard) must `hold`.
    pub fn is_uncommitted_dispatch(&self) -> bool {
        self.as_v1().terminal_failure.is_none()
            && !self.as_v1().routing_in_progress
            && !self.as_v1().shards.is_empty()
            && self.as_v1().shards.iter().all(|shard| !shard.completed())
    }

    /// Derive the ADR 0029 federated mutation lifecycle phase from the existing saga
    /// progress fields. This is a pure projection of the record's state, not a separate
    /// stored field, so the per-shard `completed`/`projection_advanced` flags and
    /// `completed_row_count` remain the single source of truth.
    ///
    /// The contract this enforces: the record never reports [`MutationLifecyclePhase::Completed`]
    /// while a required canonical shard outcome or a required projection watermark is still
    /// outstanding.
    pub fn lifecycle_phase(&self) -> MutationLifecyclePhase {
        // An irreversible terminal failure (ADR 0030 slice 6) is authoritative over the
        // progress-derived phase: a reclaim-cancelled mutation reports `Failed` even though it still
        // carries a dispatch envelope (its canonical write never committed).
        if self.as_v1().terminal_failure.is_some() {
            return MutationLifecyclePhase::Failed;
        }
        // A pinned row count is the terminal "all canonical + all projections converged"
        // signal; the heavy shard fan-out is compacted away once it is set.
        if self.as_v1().completed_row_count.is_some() {
            return MutationLifecyclePhase::Completed;
        }
        if self.as_v1().routing_in_progress {
            return MutationLifecyclePhase::Routing;
        }
        if self.as_v1().shards.is_empty() {
            // Routing was released without a durable dispatch envelope and no canonical
            // write committed (e.g. a validation/planning failure that freed the
            // reservation). The key is still re-reservable for a fresh attempt.
            return MutationLifecyclePhase::Failed;
        }
        if self.as_v1().shards.iter().any(|shard| !shard.completed()) {
            return MutationLifecyclePhase::CanonicalPending;
        }
        // Every shard's canonical write is durable from here on.
        if self
            .as_v1()
            .shards
            .iter()
            .all(|shard| shard.projection_advanced())
        {
            return MutationLifecyclePhase::Completed;
        }
        if self
            .as_v1()
            .shards
            .iter()
            .any(|shard| shard.projection_advanced())
        {
            return MutationLifecyclePhase::ProjectionPending;
        }
        MutationLifecyclePhase::CanonicalCommitted
    }
}

/// Stable-memory wire envelope for [`RouterMutationRecord`].
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
enum RouterMutationStableRecord {
    V1(RouterMutationRecord),
}

impl Storable for RouterMutationRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&RouterMutationStableRecord::V1(self.clone()))
                .expect("encode RouterMutationRecord"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&RouterMutationStableRecord::V1(self)).expect("encode RouterMutationRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), RouterMutationStableRecord)
            .expect("decode RouterMutationRecord")
        {
            RouterMutationStableRecord::V1(v1) => v1,
        }
    }
}

/// Versioned Router mutation shard outcome (ADR 0044).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum RouterMutationShard {
    V1(RouterMutationShardV1),
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterMutationShardV1 {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub seed_bindings_blob: Option<Vec<u8>>,
    pub completed: bool,
    pub projection_advanced: bool,
    pub row_count: u64,
}

impl RouterMutationShard {
    pub fn new(
        shard_id: ShardId,
        graph_canister: Principal,
        seed_bindings_blob: Option<Vec<u8>>,
    ) -> Self {
        Self::V1(RouterMutationShardV1 {
            shard_id,
            graph_canister,
            seed_bindings_blob,
            completed: false,
            projection_advanced: false,
            row_count: 0,
        })
    }

    fn as_v1(&self) -> &RouterMutationShardV1 {
        match self {
            RouterMutationShard::V1(v1) => v1,
        }
    }

    fn as_v1_mut(&mut self) -> &mut RouterMutationShardV1 {
        match self {
            RouterMutationShard::V1(v1) => v1,
        }
    }

    // Field accessors delegating to the active variant.
    pub fn shard_id(&self) -> ShardId {
        self.as_v1().shard_id
    }
    pub fn graph_canister(&self) -> Principal {
        self.as_v1().graph_canister
    }
    pub fn seed_bindings_blob(&self) -> &Option<Vec<u8>> {
        &self.as_v1().seed_bindings_blob
    }
    pub fn completed(&self) -> bool {
        self.as_v1().completed
    }
    pub fn projection_advanced(&self) -> bool {
        self.as_v1().projection_advanced
    }
    pub fn row_count(&self) -> u64 {
        self.as_v1().row_count
    }
    pub fn set_completed(&mut self, completed: bool) {
        self.as_v1_mut().completed = completed;
    }
    pub fn set_projection_advanced(&mut self, advanced: bool) {
        self.as_v1_mut().projection_advanced = advanced;
    }
    pub fn set_row_count(&mut self, row_count: u64) {
        self.as_v1_mut().row_count = row_count;
    }
    pub fn set_seed_bindings_blob(&mut self, blob: Option<Vec<u8>>) {
        self.as_v1_mut().seed_bindings_blob = blob;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::Storable;

    #[test]
    fn router_mutation_record_round_trips_through_storable() {
        let record = RouterMutationRecord::new(1, 42, vec![9, 8]);
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert_eq!(decoded.as_v1().mutation_id, 1);
        assert!(decoded.as_v1().routing_in_progress);
    }

    fn shard(shard_id: u32, completed: bool, projection_advanced: bool) -> RouterMutationShard {
        let mut s = RouterMutationShard::new(ShardId(shard_id), Principal::anonymous(), None);
        s.set_completed(completed);
        s.set_projection_advanced(projection_advanced);
        s
    }

    fn record_with_shards(shards: Vec<RouterMutationShard>) -> RouterMutationRecord {
        let mut record = RouterMutationRecord::new(1, 0, Vec::new());
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().shards = shards;
        record
    }

    // ADR 0029 Phase 0 characterization: each saga progress state maps to exactly one
    // lifecycle phase, derived from the existing fields (no new stored status).
    #[test]
    fn lifecycle_phase_tracks_saga_progress() {
        // Routing: reservation taken, no envelope persisted yet.
        let routing = RouterMutationRecord::new(1, 0, Vec::new());
        assert_eq!(routing.lifecycle_phase(), MutationLifecyclePhase::Routing);

        // Canonical pending: at least one shard outcome unknown.
        let canonical_pending =
            record_with_shards(vec![shard(0, true, true), shard(1, false, false)]);
        assert_eq!(
            canonical_pending.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalPending
        );

        // Canonical committed: all shards durable, no projection advanced.
        let canonical_committed =
            record_with_shards(vec![shard(0, true, false), shard(1, true, false)]);
        assert_eq!(
            canonical_committed.lifecycle_phase(),
            MutationLifecyclePhase::CanonicalCommitted
        );

        // Projection pending: canonical durable, some (not all) projections caught up.
        let projection_pending =
            record_with_shards(vec![shard(0, true, true), shard(1, true, false)]);
        assert_eq!(
            projection_pending.lifecycle_phase(),
            MutationLifecyclePhase::ProjectionPending
        );

        // Completed: all shards canonical + projected.
        let completed_via_shards =
            record_with_shards(vec![shard(0, true, true), shard(1, true, true)]);
        assert_eq!(
            completed_via_shards.lifecycle_phase(),
            MutationLifecyclePhase::Completed
        );

        // Completed: compacted record with a pinned row count.
        let mut completed_compacted = RouterMutationRecord::new(1, 0, Vec::new());
        completed_compacted.as_v1_mut().routing_in_progress = false;
        completed_compacted.as_v1_mut().completed_row_count = Some(7);
        assert_eq!(
            completed_compacted.lifecycle_phase(),
            MutationLifecyclePhase::Completed
        );

        // Failed: routing released with no durable shard envelope.
        let failed = record_with_shards(Vec::new());
        assert_eq!(failed.lifecycle_phase(), MutationLifecyclePhase::Failed);
    }

    // ADR 0029 Phase 0 contract: Router must never report `Completed` while any required
    // canonical shard outcome or projection watermark is still outstanding.
    #[test]
    fn lifecycle_phase_never_completes_with_outstanding_work() {
        let unfinished_states = [
            record_with_shards(vec![shard(0, false, false)]),
            record_with_shards(vec![shard(0, true, false)]),
            record_with_shards(vec![shard(0, true, true), shard(1, false, false)]),
            record_with_shards(vec![shard(0, true, true), shard(1, true, false)]),
        ];
        for record in unfinished_states {
            assert_ne!(
                record.lifecycle_phase(),
                MutationLifecyclePhase::Completed,
                "incomplete saga must not report Completed: {:?}",
                record.as_v1().shards
            );
        }
    }
}
