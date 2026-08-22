//! Router-owned durable intent for direct vertex-embedding ingestion.
//!
//! One row owns an allocated mutation id before the first Graph await and remains authoritative
//! until Router observes the Vector frontier that covers it. The row stores canonical inputs and
//! derives the exact Graph and Vector wire requests for replay.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{GraphShardKey, LocalVertexId, ShardId};
use gleaph_graph_kernel::vector_index::{
    IndexedEmbeddingSpec, VectorEmbeddingSyncOp, VectorSubject, VectorSyncBatchOutcome,
    VertexEmbeddingIngestionArgs,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::facade::stable::{ROUTER_MUTATION_COUNTER, ROUTER_VECTOR_INGEST_OUTBOX};
use crate::facade::stable::{graph_catalog, vector_index_catalog};

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .expect("outbox test lock")
}

/// The Router keeps a bounded number of direct-ingestion operations.
pub(crate) const MAX_VECTOR_INGEST_OUTBOX_ROWS: usize = 1024;

/// A single row must fit inside the safe inter-canister payload budget. The same bound is applied
/// before stable insertion, so a row cannot be persisted if its Candid representation is too large.
pub(crate) const MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES: usize =
    gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, CandidType, Serialize, Deserialize,
)]
#[allow(
    clippy::enum_variant_names,
    reason = "the Awaiting prefix is part of the persisted outbox phase vocabulary"
)]
pub(crate) enum VectorIngestIntentPhase {
    AwaitingGraph,
    AwaitingVector,
    /// A resolved intent retained until the exact Vector lane frontier is observed.
    ///
    /// Mutation IDs are allocated monotonically and never reused.  This phase has no legal
    /// transition other than retirement of its exact durable key after a successful frontier
    /// publication, so recovery snapshots need only retain the key, lane, and frontier.
    AwaitingFrontier,
}

/// Fixed-width identity of one durable direct-ingestion row.
///
/// The mutation identity is the primary ordering component.  The exact Vector lane and phase
/// are part of the durable identity as well, so a phase transition moves one old key to one new
/// key instead of rewriting a payload that a key-only scan would have to decode.  Mutation IDs
/// are allocated monotonically and never reused.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, CandidType, Serialize, Deserialize,
)]
pub(crate) struct VectorIngestOutboxKey {
    pub(crate) mutation_id: u64,
    pub(crate) vector_target: Principal,
    pub(crate) shard_id: ShardId,
    pub(crate) phase: VectorIngestIntentPhase,
}

impl VectorIngestOutboxKey {
    /// `mutation_id` (8) + principal length/padded bytes (1 + 29) + shard (4) + phase (1).
    pub(crate) const BYTE_WIDTH: usize = 43;
    const MAX_PRINCIPAL_BYTES: usize = 29;

    fn new(
        mutation_id: u64,
        vector_target: Principal,
        shard_id: ShardId,
        phase: VectorIngestIntentPhase,
    ) -> Result<Self, String> {
        if mutation_id == 0 {
            return Err("vector-ingest outbox mutation_id must be nonzero".to_string());
        }
        if vector_target.as_slice().len() > Self::MAX_PRINCIPAL_BYTES {
            return Err("vector-ingest outbox Vector target principal is too large".to_string());
        }
        Ok(Self {
            mutation_id,
            vector_target,
            shard_id,
            phase,
        })
    }

    pub(crate) fn from_state(state: &VectorIngestOutboxState) -> Self {
        Self::new(
            state.mutation_id,
            state.vector_target,
            state.shard_id,
            state.phase,
        )
        .expect("validated vector-ingest outbox state key")
    }

    fn phase_tag(phase: VectorIngestIntentPhase) -> u8 {
        match phase {
            VectorIngestIntentPhase::AwaitingGraph => 0,
            VectorIngestIntentPhase::AwaitingVector => 1,
            VectorIngestIntentPhase::AwaitingFrontier => 2,
        }
    }

    fn phase_from_tag(tag: u8) -> VectorIngestIntentPhase {
        match tag {
            0 => VectorIngestIntentPhase::AwaitingGraph,
            1 => VectorIngestIntentPhase::AwaitingVector,
            2 => VectorIngestIntentPhase::AwaitingFrontier,
            _ => panic!("unknown vector-ingest outbox phase tag {tag}"),
        }
    }
}

impl Storable for VectorIngestOutboxKey {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: Self::BYTE_WIDTH as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let principal = self.vector_target.as_slice();
        assert!(principal.len() <= Self::MAX_PRINCIPAL_BYTES);
        let mut bytes = [0u8; Self::BYTE_WIDTH];
        bytes[..8].copy_from_slice(&self.mutation_id.to_be_bytes());
        bytes[8] = principal.len() as u8;
        bytes[9..9 + principal.len()].copy_from_slice(principal);
        bytes[38..42].copy_from_slice(&self.shard_id.raw().to_be_bytes());
        bytes[42] = Self::phase_tag(self.phase);
        bytes.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes: [u8; Self::BYTE_WIDTH] = bytes
            .as_ref()
            .try_into()
            .expect("decode 43-byte VectorIngestOutboxKey");
        let mutation_id = u64::from_be_bytes(bytes[..8].try_into().expect("mutation id bytes"));
        let principal_len = usize::from(bytes[8]);
        assert!(
            principal_len <= Self::MAX_PRINCIPAL_BYTES,
            "vector-ingest outbox target principal is too large"
        );
        assert!(
            bytes[9 + principal_len..38].iter().all(|byte| *byte == 0),
            "vector-ingest outbox target principal has non-canonical padding"
        );
        let vector_target = Principal::from_slice(&bytes[9..9 + principal_len]);
        let shard_id = ShardId::new(u32::from_be_bytes(bytes[38..42].try_into().expect("shard")));
        let phase = Self::phase_from_tag(bytes[42]);
        Self::new(mutation_id, vector_target, shard_id, phase)
            .expect("decode valid VectorIngestOutboxKey")
    }
}

/// Canonical inputs for one direct-ingestion intent before its mutation id is allocated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewVectorIngestIntent {
    pub(crate) graph_id: GraphId,
    pub(crate) graph_target: Principal,
    pub(crate) vector_target: Principal,
    pub(crate) shard_id: ShardId,
    pub(crate) local_vertex_id: LocalVertexId,
    pub(crate) spec: IndexedEmbeddingSpec,
    pub(crate) bytes: Vec<u8>,
}

/// One durable direct-ingestion intent and its exact Graph and Vector targets.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct VectorIngestOutboxState {
    pub(crate) graph_id: GraphId,
    pub(crate) graph_target: Principal,
    pub(crate) vector_target: Principal,
    pub(crate) shard_id: ShardId,
    pub(crate) local_vertex_id: LocalVertexId,
    pub(crate) spec: IndexedEmbeddingSpec,
    pub(crate) mutation_id: u64,
    pub(crate) bytes: Vec<u8>,
    pub(crate) phase: VectorIngestIntentPhase,
}

/// The persisted value payload.  Key-owned identity (`mutation_id`, Vector target, shard, and
/// phase) is deliberately absent; [`VectorIngestOutboxKey`] is the sole stable source of those
/// facts.  Keeping the encoded payload opaque until an exact row is selected also lets key-only
/// scans avoid decoding large embedding bytes.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
struct VectorIngestOutboxPayload {
    graph_id: GraphId,
    graph_target: Principal,
    local_vertex_id: LocalVertexId,
    spec: IndexedEmbeddingSpec,
    bytes: Vec<u8>,
}

/// Opaque stable value for one outbox row.  `from_bytes` retains the encoded payload and does not
/// decode it; callers explicitly decode only after selecting an exact key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VectorIngestOutboxValue {
    encoded_payload: Vec<u8>,
}

impl VectorIngestOutboxValue {
    pub(crate) fn from_state(state: &VectorIngestOutboxState) -> Self {
        let payload = VectorIngestOutboxPayload {
            graph_id: state.graph_id,
            graph_target: state.graph_target,
            local_vertex_id: state.local_vertex_id,
            spec: state.spec.clone(),
            bytes: state.bytes.clone(),
        };
        Self {
            encoded_payload: Encode!(&payload).expect("encode VectorIngestOutboxPayload"),
        }
    }

    fn decode(&self) -> VectorIngestOutboxPayload {
        #[cfg(test)]
        VALUE_DECODE_COUNT.fetch_add(1, Ordering::Relaxed);
        Decode!(self.encoded_payload.as_slice(), VectorIngestOutboxPayload)
            .expect("decode VectorIngestOutboxPayload")
    }
}

#[cfg(test)]
static VALUE_DECODE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_value_decode_count() {
    VALUE_DECODE_COUNT.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn value_decode_count() -> usize {
    VALUE_DECODE_COUNT.load(Ordering::Relaxed)
}

impl Storable for VectorIngestOutboxValue {
    // The row admission check is separate from stable-node allocation.  Keeping the value
    // unbounded avoids making the 2 MiB transport ceiling determine every B-tree page size.
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.encoded_payload)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encoded_payload
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self {
            encoded_payload: bytes.into_owned(),
        }
    }
}

impl VectorIngestOutboxState {
    fn awaiting_graph(input: NewVectorIngestIntent, mutation_id: u64) -> Self {
        Self {
            graph_id: input.graph_id,
            graph_target: input.graph_target,
            vector_target: input.vector_target,
            shard_id: input.shard_id,
            local_vertex_id: input.local_vertex_id,
            spec: input.spec,
            mutation_id,
            bytes: input.bytes,
            phase: VectorIngestIntentPhase::AwaitingGraph,
        }
    }

    pub(crate) fn graph_args(&self) -> VertexEmbeddingIngestionArgs {
        VertexEmbeddingIngestionArgs {
            local_vertex_id: self.local_vertex_id,
            spec: self.spec.clone(),
            mutation_id: self.mutation_id,
        }
    }

    pub(crate) fn vector_operation(&self) -> VectorEmbeddingSyncOp {
        VectorEmbeddingSyncOp {
            index_id: self.spec.index_id,
            embedding_name_id: self.spec.embedding_name_id,
            subject: VectorSubject::Vertex {
                shard_id: self.shard_id,
                vertex_id: self.local_vertex_id,
            },
            mutation_id: self.mutation_id,
            encoding: self.spec.encoding,
            dims: self.spec.dims,
            metric: self.spec.metric,
            bytes: self.bytes.clone(),
            remove: false,
        }
    }

    fn matches(&self, expected: &Self) -> bool {
        self == expected
    }

    fn awaiting_vector(&self) -> Self {
        Self {
            phase: VectorIngestIntentPhase::AwaitingVector,
            ..self.clone()
        }
    }

    fn awaiting_frontier(&self) -> Self {
        Self {
            phase: VectorIngestIntentPhase::AwaitingFrontier,
            ..self.clone()
        }
    }

    fn encode_checked(&self) -> Result<Vec<u8>, String> {
        let bytes =
            Encode!(self).map_err(|error| format!("encode vector-ingest outbox row: {error}"))?;
        if bytes.len() > MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES {
            return Err(format!(
                "vector-ingest outbox row encoding {} exceeds maximum {} bytes",
                bytes.len(),
                MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES
            ));
        }
        Ok(bytes)
    }

    fn key(&self) -> VectorIngestOutboxKey {
        VectorIngestOutboxKey::from_state(self)
    }

    fn value(&self) -> VectorIngestOutboxValue {
        VectorIngestOutboxValue::from_state(self)
    }
}

pub(crate) type VectorIngestOutboxRow = VectorIngestOutboxState;

pub(crate) fn state_from_entry(
    key: VectorIngestOutboxKey,
    value: VectorIngestOutboxValue,
) -> VectorIngestOutboxState {
    let payload = value.decode();
    VectorIngestOutboxState {
        graph_id: payload.graph_id,
        graph_target: payload.graph_target,
        vector_target: key.vector_target,
        shard_id: key.shard_id,
        local_vertex_id: payload.local_vertex_id,
        spec: payload.spec,
        mutation_id: key.mutation_id,
        bytes: payload.bytes,
        phase: key.phase,
    }
}

thread_local! {
    /// Heap-only exclusion for rows whose originating API call is still driving initial delivery.
    /// An upgrade clears it, allowing durable recovery to resume every unresolved row.
    static INITIAL_DELIVERY_ACTIVE: RefCell<BTreeSet<u64>> = const { RefCell::new(BTreeSet::new()) };
    /// Heap-only catalog cursor for markerless frontier discovery. The stable shard catalog is
    /// the source of truth, so losing this cursor on upgrade only restarts enumeration.
    static FRONTIER_CATALOG_CURSOR: RefCell<Option<GraphShardKey>> = const { RefCell::new(None) };
    /// Heap-only last-observed progress per exact lane. It suppresses duplicate markerless calls
    /// after an observed success, while an error/unknown response leaves the hint unchanged so
    /// catalog rediscovery retries the monotonic Vector endpoint. Upgrade clears it naturally.
    static FRONTIER_LANE_PROGRESS: RefCell<BTreeMap<(Principal, ShardId), u64>> =
        const { RefCell::new(BTreeMap::new()) };
    /// Heap-only liveness hint for the current forward catalog lap. It distinguishes an empty
    /// catalog/ineligible-only lap, which may stop the scheduler, from a completed lap that did
    /// observe an attached lane and must reserve the next lap. Upgrade clears it with the cursor.
    static FRONTIER_LAP_SAW_ATTACHED_LANE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

pub(crate) struct InitialDeliveryGuard {
    mutation_ids: Vec<u64>,
}

impl InitialDeliveryGuard {
    pub(crate) fn new(rows: &[VectorIngestOutboxState]) -> Self {
        let mutation_ids: Vec<_> = rows.iter().map(|row| row.mutation_id).collect();
        INITIAL_DELIVERY_ACTIVE.with_borrow_mut(|active| {
            for mutation_id in &mutation_ids {
                assert!(
                    active.insert(*mutation_id),
                    "vector-ingest initial delivery already owns mutation_id {mutation_id}"
                );
            }
        });
        Self { mutation_ids }
    }
}

impl Drop for InitialDeliveryGuard {
    fn drop(&mut self) {
        INITIAL_DELIVERY_ACTIVE.with_borrow_mut(|active| {
            for mutation_id in &self.mutation_ids {
                assert!(
                    active.remove(mutation_id),
                    "vector-ingest initial delivery lost mutation_id {mutation_id}"
                );
            }
        });
    }
}

fn initial_delivery_active(mutation_id: u64) -> bool {
    INITIAL_DELIVERY_ACTIVE.with_borrow(|active| active.contains(&mutation_id))
}

fn validate_catalog_identity(input: &NewVectorIngestIntent) -> Result<(), String> {
    let shard =
        graph_catalog::lookup_shard_entry(input.graph_id, input.shard_id).ok_or_else(|| {
            format!(
                "vector-ingest outbox shard {:?} is not registered for graph {:?}",
                input.shard_id, input.graph_id
            )
        })?;
    if !shard.index_attached || shard.graph_canister != input.graph_target {
        return Err(format!(
            "vector-ingest outbox shard {:?} is not live at exact Graph target {}",
            input.shard_id, input.graph_target
        ));
    }
    if !shard.vector_index_attached || shard.vector_canister != Some(input.vector_target) {
        return Err(format!(
            "vector-ingest outbox shard {:?} is not attached to exact Vector target {}",
            input.shard_id, input.vector_target
        ));
    }

    let definition = vector_index_catalog::get_vector_index(input.graph_id, input.spec.index_id)
        .ok_or_else(|| {
            format!(
                "vector-ingest outbox index {} is not defined for graph {:?}",
                input.spec.index_id, input.graph_id
            )
        })?;
    let definition_target = definition
        .target
        .map(|target| target.canister)
        .ok_or_else(|| {
            format!(
                "vector-ingest outbox index {} has no target",
                input.spec.index_id
            )
        })?;
    if definition_target != input.vector_target {
        return Err(format!(
            "vector-ingest outbox index {} is not attached to exact Vector target {}",
            input.spec.index_id, input.vector_target
        ));
    }
    if input.spec.embedding_name_id != definition.embedding_name_id.raw()
        || input.spec.kind != definition.kind
        || input.spec.encoding != definition.encoding
        || input.spec.dims != definition.dims
        || input.spec.metric != definition.metric
        || input.spec.labels != definition.labels
    {
        return Err(format!(
            "vector-ingest outbox intent does not match current definition {}",
            input.spec.index_id
        ));
    }
    Ok(())
}

/// Allocate a checked mutation-id range and persist every exact `AwaitingGraph` intent in one
/// synchronous Router operation. All returned errors occur before the counter or outbox changes.
pub(crate) fn admit_awaiting_graph(
    inputs: Vec<NewVectorIngestIntent>,
) -> Result<Vec<VectorIngestOutboxState>, String> {
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    for input in &inputs {
        validate_catalog_identity(input)?;
    }

    let allocated_through = ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get());
    let mut identities = BTreeSet::new();
    let mut prepared = Vec::with_capacity(inputs.len());
    for (offset, input) in inputs.into_iter().enumerate() {
        let increment = u64::try_from(offset)
            .map_err(|_| "vector-ingest mutation-id offset exceeds u64".to_string())?
            .checked_add(1)
            .ok_or_else(|| "vector-ingest mutation-id increment overflow".to_string())?;
        let mutation_id = allocated_through
            .checked_add(increment)
            .ok_or_else(|| "vector-ingest mutation-id range exhausted".to_string())?;
        if mutation_id == 0 || !identities.insert(mutation_id) {
            return Err("vector-ingest mutation-id range is invalid".to_string());
        }
        let state = VectorIngestOutboxState::awaiting_graph(input, mutation_id);
        state.encode_checked()?;
        prepared.push(state);
    }
    let final_mutation_id = prepared
        .last()
        .expect("nonempty vector-ingest admission")
        .mutation_id;
    let prepared_entries: Vec<_> = prepared
        .iter()
        .map(|state| (state.key(), state.value()))
        .collect();

    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let current_len = usize::try_from(table.len())
            .map_err(|_| "vector-ingest outbox row count exceeds usize".to_string())?;
        let final_len = current_len
            .checked_add(prepared.len())
            .ok_or_else(|| "vector-ingest outbox row count overflow".to_string())?;
        if final_len > MAX_VECTOR_INGEST_OUTBOX_ROWS {
            return Err(format!(
                "vector-ingest outbox capacity {} exceeded by {} rows",
                MAX_VECTOR_INGEST_OUTBOX_ROWS, final_len
            ));
        }
        for (key, _) in &prepared_entries {
            if table.contains_key(key) {
                return Err(format!(
                    "vector-ingest outbox mutation_id {} already exists",
                    key.mutation_id
                ));
            }
        }

        // No fallible operation remains after this point.
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(final_mutation_id));
        for (key, value) in prepared_entries {
            assert!(
                table.insert(key, value).is_none(),
                "vector-ingest outbox identity was inserted during preflight"
            );
        }
        Ok(prepared)
    })
}

#[cfg(test)]
pub(crate) fn insert_intents_for_test(rows: &[VectorIngestOutboxState]) -> Result<(), String> {
    for row in rows {
        if row.mutation_id == 0 {
            return Err("vector-ingest outbox mutation_id must be nonzero".to_string());
        }
        row.encode_checked()?;
    }
    let prepared_entries: Vec<_> = rows.iter().map(|row| (row.key(), row.value())).collect();
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let final_len = usize::try_from(table.len())
            .map_err(|_| "vector-ingest outbox row count exceeds usize".to_string())?
            .checked_add(rows.len())
            .ok_or_else(|| "vector-ingest outbox row count overflow".to_string())?;
        if final_len > MAX_VECTOR_INGEST_OUTBOX_ROWS {
            return Err(format!(
                "vector-ingest outbox capacity {} exceeded by {} rows",
                MAX_VECTOR_INGEST_OUTBOX_ROWS, final_len
            ));
        }
        for (key, _) in &prepared_entries {
            if table.contains_key(key) {
                return Err(format!(
                    "vector-ingest outbox mutation_id {} already exists",
                    key.mutation_id
                ));
            }
        }
        for (key, value) in prepared_entries {
            table.insert(key, value);
        }
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn intent_for_test(
    input: NewVectorIngestIntent,
    mutation_id: u64,
    phase: VectorIngestIntentPhase,
) -> VectorIngestOutboxState {
    let state = VectorIngestOutboxState::awaiting_graph(input, mutation_id);
    match phase {
        VectorIngestIntentPhase::AwaitingGraph => state,
        VectorIngestIntentPhase::AwaitingVector => state.awaiting_vector(),
        VectorIngestIntentPhase::AwaitingFrontier => state.awaiting_frontier(),
    }
}

/// Return whether bounded direct-ingestion work remains for an exact Vector target and shard.
///
/// The outbox itself is capped at [`MAX_VECTOR_INGEST_OUTBOX_ROWS`], so one complete scan is
/// bounded and sufficient. Router shard unregister uses this gate before changing any lifecycle
/// state; a matching suffix must drain before the target/attachment identity can be detached.
pub(crate) fn has_pending_for_target_shard(vector_target: Principal, shard_id: ShardId) -> bool {
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
        table
            .keys()
            .any(|key| key.vector_target == vector_target && key.shard_id == shard_id)
    })
}

/// Return whether any direct-ingestion suffix remains. Graph unregister uses this conservative
/// final purge gate because a suffix row is durable work that must not be orphaned by catalog purge.
pub(crate) fn has_pending() -> bool {
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| !table.is_empty())
}

/// Test-only key projection of the durable direct-ingestion outbox. Values are deliberately not
/// decoded: the probe exposes only the mutation ceiling and exact stable-key identities in the
/// B-tree's canonical order.
#[cfg(feature = "pocket-ic-e2e")]
pub(crate) fn test_outbox_probe() -> (u64, Vec<(u64, Principal, ShardId, u8)>) {
    let mutation_ceiling = ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get());
    let identities = ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
        table
            .keys()
            .map(|key| {
                (
                    key.mutation_id,
                    key.vector_target,
                    key.shard_id,
                    VectorIngestOutboxKey::phase_tag(key.phase),
                )
            })
            .collect()
    });
    (mutation_ceiling, identities)
}

/// Snapshot at most `budget` pending rows after `start_after`.
pub(crate) fn scan(
    start_after: Option<u64>,
    budget: usize,
) -> (Vec<VectorIngestOutboxRow>, Option<u64>, u32) {
    let mut rows = Vec::new();
    let mut last_key = None;
    let mut scanned = 0u32;
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
        // Select the bounded identity batch from keys only.  Values are decoded below only for
        // these exact keys; unrelated large payload rows remain untouched.
        let selected_keys: Vec<_> = table
            .keys()
            .filter(|key| start_after.is_none_or(|cursor| key.mutation_id > cursor))
            .take(budget)
            .collect();
        for key in selected_keys {
            let mutation_id = key.mutation_id;
            scanned = scanned.saturating_add(1);
            last_key = Some(mutation_id);
            let value = table
                .get(&key)
                .expect("selected vector-ingest outbox key disappeared");
            let row = state_from_entry(key, value);
            rows.push(row);
        }
    });
    (rows, last_key, scanned)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VectorFrontierSnapshot {
    pub(crate) vector_target: Principal,
    pub(crate) shard_id: ShardId,
    pub(crate) frontier: u64,
    pub(crate) marker_keys: Vec<VectorIngestOutboxKey>,
}

#[derive(Default)]
struct FrontierLane {
    oldest_unresolved: Option<u64>,
    marker_keys: Vec<VectorIngestOutboxKey>,
}

fn record_frontier_key_in_lane(
    lane: &mut FrontierLane,
    key: VectorIngestOutboxKey,
) -> Result<(), String> {
    if key.mutation_id == 0 {
        return Err("vector-ingest frontier row mutation_id must be nonzero".to_string());
    }

    match key.phase {
        VectorIngestIntentPhase::AwaitingGraph | VectorIngestIntentPhase::AwaitingVector => {
            lane.oldest_unresolved = Some(
                lane.oldest_unresolved
                    .map_or(key.mutation_id, |oldest| oldest.min(key.mutation_id)),
            );
        }
        VectorIngestIntentPhase::AwaitingFrontier => lane.marker_keys.push(key),
    }
    Ok(())
}

#[cfg(any(test, feature = "canbench"))]
fn record_frontier_key(
    lanes: &mut BTreeMap<(Principal, ShardId), FrontierLane>,
    key: VectorIngestOutboxKey,
) -> Result<(), String> {
    let lane = lanes.entry((key.vector_target, key.shard_id)).or_default();
    record_frontier_key_in_lane(lane, key)
}

fn finish_frontier_snapshot(
    vector_target: Principal,
    shard_id: ShardId,
    lane: FrontierLane,
    allocated_through: u64,
) -> Result<VectorFrontierSnapshot, String> {
    let frontier = match lane.oldest_unresolved {
        Some(oldest) => oldest.checked_sub(1).ok_or_else(|| {
            "vector-ingest unresolved mutation_id cannot derive a frontier".to_string()
        })?,
        None => allocated_through,
    };
    let marker_keys = lane
        .marker_keys
        .into_iter()
        .filter(|key| key.mutation_id <= frontier)
        .collect();
    Ok(VectorFrontierSnapshot {
        vector_target,
        shard_id,
        frontier,
        marker_keys,
    })
}

#[cfg(any(test, feature = "canbench"))]
fn finish_frontier_snapshots(
    lanes: BTreeMap<(Principal, ShardId), FrontierLane>,
    allocated_through: u64,
) -> Result<Vec<VectorFrontierSnapshot>, String> {
    let mut snapshots = Vec::new();
    for ((vector_target, shard_id), lane) in lanes {
        if lane.marker_keys.is_empty() {
            continue;
        }
        let snapshot = finish_frontier_snapshot(vector_target, shard_id, lane, allocated_through)?;
        // A lane whose markers are all above its current safe frontier remains durable but is not
        // a publishable snapshot yet.  It must not be described as having been published.
        if snapshot.marker_keys.is_empty() {
            continue;
        }
        snapshots.push(snapshot);
    }
    Ok(snapshots)
}

/// Derive one exact safe publication snapshot for every marked `(Vector target, shard)` lane.
/// Production recovery streams MemoryId 53 once and retains only lane state plus marker keys;
/// it never materializes all rows or copies their payloads into a heap projection.
/// `AwaitingGraph` and `AwaitingVector` are unresolved replay work; `AwaitingFrontier` rows are
/// eligible markers and do not block their own lane.
#[cfg(test)]
pub(crate) fn derive_frontier_snapshots_from_rows(
    rows: &[VectorIngestOutboxRow],
    allocated_through: u64,
) -> Result<Vec<VectorFrontierSnapshot>, String> {
    let mut lanes = BTreeMap::new();
    for row in rows {
        record_frontier_key(&mut lanes, row.key())?;
    }
    finish_frontier_snapshots(lanes, allocated_through)
}

/// Snapshot all marked lanes from the sole durable outbox and the current mutation ceiling.
/// The stable map is streamed exactly once; only compact per-lane state is retained.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn derive_frontier_snapshots() -> Result<Vec<VectorFrontierSnapshot>, String> {
    let allocated_through = ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get());
    let mut lanes = BTreeMap::new();
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
        for key in table.keys() {
            record_frontier_key(&mut lanes, key)?;
        }
        Ok::<_, String>(())
    })?;
    finish_frontier_snapshots(lanes, allocated_through)
}

/// Derive the safe frontier for one exact catalog-attached lane.
///
/// Unlike [`derive_frontier_snapshots`], this returns a snapshot even when the lane has no
/// `AwaitingFrontier` marker. The catalog is the caller's lane-discovery source; this bounded
/// MemoryId 53 scan supplies only the exact-lane unresolved floor and captured marker keys.
/// Marker keys may therefore be empty for a Graph-only lane or for a retry whose markers are
/// above the current safe frontier.
pub(crate) fn derive_frontier_snapshot_for_lane(
    vector_target: Principal,
    shard_id: ShardId,
) -> Result<VectorFrontierSnapshot, String> {
    let allocated_through = ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get());
    let mut lane = FrontierLane::default();
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
        for key in table
            .keys()
            .filter(|key| key.vector_target == vector_target && key.shard_id == shard_id)
        {
            record_frontier_key_in_lane(&mut lane, key)?;
        }
        Ok::<_, String>(())
    })?;
    finish_frontier_snapshot(vector_target, shard_id, lane, allocated_through)
}

/// Retire only the exact marker rows captured before an observed Vector frontier reply. Every row
/// is preflighted before the first removal, so a stale snapshot cannot partially retire newer work.
/// Mutation IDs are never reused and `AwaitingFrontier` has no legal transition except retirement;
/// rechecking the current row's phase and exact lane is therefore sufficient for this key snapshot.
pub(crate) fn retire_frontier_snapshot(snapshot: &VectorFrontierSnapshot) -> Result<(), String> {
    if snapshot.marker_keys.is_empty() {
        return Ok(());
    }
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let mut captured_keys = BTreeSet::new();
        for marker_key in &snapshot.marker_keys {
            if !captured_keys.insert(*marker_key) {
                return Err(format!(
                    "vector-ingest frontier marker {} is duplicated in its snapshot",
                    marker_key.mutation_id
                ));
            }
            if marker_key.mutation_id == 0 || marker_key.mutation_id > snapshot.frontier {
                return Err(format!(
                    "vector-ingest frontier marker {} is outside captured frontier {}",
                    marker_key.mutation_id, snapshot.frontier
                ));
            }
            if !table.contains_key(marker_key) {
                return Err(format!(
                    "vector-ingest frontier marker {} disappeared before retirement",
                    marker_key.mutation_id
                ));
            }
            if marker_key.phase != VectorIngestIntentPhase::AwaitingFrontier {
                return Err(format!(
                    "vector-ingest frontier marker {} is not AwaitingFrontier",
                    marker_key.mutation_id
                ));
            }
            if marker_key.vector_target != snapshot.vector_target
                || marker_key.shard_id != snapshot.shard_id
            {
                return Err(format!(
                    "vector-ingest frontier marker {} is outside its captured lane",
                    marker_key.mutation_id
                ));
            }
        }
        for marker_key in &snapshot.marker_keys {
            assert!(
                table.remove(marker_key).is_some(),
                "validated vector-ingest frontier marker disappeared"
            );
        }
        Ok(())
    })
}

/// Group only the rows that are ready for Vector by their persisted exact lane.  The input is
/// already in mutation-id order from the durable-key scan, and `Vec::push` preserves that order
/// inside each lane while the map keeps lanes independent.
fn group_vector_rows(
    rows: Vec<VectorIngestOutboxRow>,
) -> BTreeMap<(Principal, ShardId), Vec<VectorIngestOutboxRow>> {
    let mut groups: BTreeMap<(Principal, ShardId), Vec<VectorIngestOutboxRow>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((row.vector_target, row.shard_id))
            .or_default()
            .push(row);
    }
    groups
}

/// Apply an observed exact Graph acceptance to the submitted `AwaitingGraph` row.
pub(crate) fn observe_graph_accept(
    submitted: &VectorIngestOutboxState,
    returned_mutation_id: u64,
) -> Result<VectorIngestOutboxState, String> {
    if submitted.phase != VectorIngestIntentPhase::AwaitingGraph {
        return Err("Graph acceptance requires an AwaitingGraph intent".to_string());
    }
    if returned_mutation_id != submitted.mutation_id {
        return Err(format!(
            "Graph returned mutation_id {returned_mutation_id} for intent {}",
            submitted.mutation_id
        ));
    }
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let old_key = submitted.key();
        let current_value = table.get(&old_key).ok_or_else(|| {
            format!(
                "vector-ingest outbox row {} disappeared before Graph acceptance",
                submitted.mutation_id
            )
        })?;
        let current = state_from_entry(old_key, current_value.clone());
        if !current.matches(submitted) {
            return Err(format!(
                "vector-ingest outbox row {} no longer matches Graph submission",
                submitted.mutation_id
            ));
        }
        let next = current.awaiting_vector();
        let next_key = next.key();
        if table.contains_key(&next_key) {
            return Err(format!(
                "vector-ingest outbox row {} already has its next phase",
                submitted.mutation_id
            ));
        }
        assert!(table.remove(&old_key).is_some());
        assert!(table.insert(next_key, current_value).is_none());
        Ok(next)
    })
}

/// Resolve an observed exact Graph rejection by retaining a durable frontier marker.
pub(crate) fn observe_graph_reject(submitted: &VectorIngestOutboxState) -> Result<(), String> {
    if submitted.phase != VectorIngestIntentPhase::AwaitingGraph {
        return Err("Graph rejection requires an AwaitingGraph intent".to_string());
    }
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let old_key = submitted.key();
        let current_value = table.get(&old_key).ok_or_else(|| {
            format!(
                "vector-ingest outbox row {} disappeared before Graph rejection",
                submitted.mutation_id
            )
        })?;
        let current = state_from_entry(old_key, current_value.clone());
        if !current.matches(submitted) {
            return Err(format!(
                "vector-ingest outbox row {} no longer matches Graph submission",
                submitted.mutation_id
            ));
        }
        let next = current.awaiting_frontier();
        let next_key = next.key();
        if table.contains_key(&next_key) {
            return Err(format!(
                "vector-ingest outbox row {} already has its next phase",
                submitted.mutation_id
            ));
        }
        assert!(table.remove(&old_key).is_some());
        assert!(table.insert(next_key, current_value).is_none());
        Ok(())
    })
}

/// Apply a validated Vector outcome to the exact pending snapshot used for the request. All row
/// identity checks and outcome validation happen before the first transition, so a stale,
/// malformed, or cross-target response leaves the outbox unchanged. The acknowledged prefix is
/// retained as `AwaitingFrontier` until Router observes the corresponding Vector frontier reply.
pub(crate) fn apply_outcome(
    submitted: &[VectorIngestOutboxRow],
    outcome: VectorSyncBatchOutcome,
) -> Result<(), String> {
    outcome.validate(submitted.len()).map_err(str::to_string)?;
    if submitted.is_empty() {
        return Err("vector-ingest outbox outcome requires a nonempty submission".to_string());
    }
    let applied = match &outcome {
        VectorSyncBatchOutcome::Progress { applied }
        | VectorSyncBatchOutcome::Terminal { applied, .. } => *applied as usize,
    };

    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let mut transitions = Vec::new();
        let mut next_keys = BTreeSet::new();
        for (index, expected) in submitted.iter().enumerate() {
            if expected.phase != VectorIngestIntentPhase::AwaitingVector {
                return Err(format!(
                    "vector-ingest outbox row {} is not awaiting Vector",
                    expected.mutation_id
                ));
            }
            let old_key = expected.key();
            let current_value = table.get(&old_key).ok_or_else(|| {
                format!(
                    "vector-ingest outbox row {} disappeared before outcome",
                    expected.mutation_id
                )
            })?;
            let state = state_from_entry(old_key, current_value.clone());
            if !state.matches(expected) {
                return Err(format!(
                    "vector-ingest outbox row {} no longer matches submitted operation",
                    expected.mutation_id
                ));
            }
            if index < applied {
                let next = expected.awaiting_frontier();
                let next_key = next.key();
                if !next_keys.insert(next_key) || table.contains_key(&next_key) {
                    return Err(format!(
                        "vector-ingest outbox row {} already has its frontier phase",
                        expected.mutation_id
                    ));
                }
                transitions.push((old_key, next_key, current_value));
            }
        }

        debug_assert_eq!(transitions.len(), applied);
        for (old_key, next_key, value) in transitions {
            assert!(table.remove(&old_key).is_some());
            assert!(
                table.insert(next_key, value).is_none(),
                "validated vector-ingest outbox prefix row disappeared"
            );
        }
        Ok(())
    })
}

#[cfg(test)]
pub(crate) fn total_len() -> u64 {
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| table.len())
}

#[cfg(test)]
pub(crate) fn clear_for_test() {
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| table.clear_new());
    FRONTIER_CATALOG_CURSOR.with_borrow_mut(|cursor| *cursor = None);
    FRONTIER_LANE_PROGRESS.with_borrow_mut(|progress| progress.clear());
    FRONTIER_LAP_SAW_ATTACHED_LANE.with(|saw| saw.set(false));
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecoveryPassOutcome {
    pub next_cursor: Option<u64>,
    pub found: bool,
}

/// Discover and attempt at most one catalog-attached frontier lane.
///
/// Catalog selection advances the heap cursor before the Vector await. A marker-backed snapshot
/// is always eligible for the existing exact marker retirement contract. A markerless snapshot
/// is sent only when it advances the last frontier observed for that exact lane; failed/unknown
/// calls leave that hint unchanged so the next catalog lap retries, and an upgrade clears the hint
/// so rediscovery is retryable without durable progress state.
async fn run_catalog_frontier_pass() -> bool {
    let start_after = FRONTIER_CATALOG_CURSOR.with_borrow(|cursor| *cursor);
    if start_after.is_none() {
        FRONTIER_LAP_SAW_ATTACHED_LANE.with(|saw| saw.set(false));
    }
    let page = match graph_catalog::scan_attached_vector_lane(start_after) {
        Ok(page) => page,
        Err(_) => {
            // Fail closed on catalog corruption and retry from the canonical first key on the
            // next timer pass. Keeping the scheduler alive makes the failure observable/retryable
            // after an operator repairs the catalog.
            FRONTIER_CATALOG_CURSOR.with_borrow_mut(|cursor| *cursor = None);
            return true;
        }
    };

    // This is deliberately before the remote call. A failing lane therefore cannot monopolize
    // the next tick, while a page with no eligible row still advances toward later catalog keys.
    FRONTIER_CATALOG_CURSOR.with_borrow_mut(|cursor| *cursor = page.next_cursor);
    let Some(lane) = page.lane else {
        // A full ineligible page keeps the scheduler alive so the next tick can inspect later
        // catalog keys. At the catalog end, only a lap that observed an attached lane reserves a
        // fresh pass; an empty/ineligible-only catalog stops without a hot loop.
        if catalog_page_requires_follow_up(&page) {
            return true;
        }
        return FRONTIER_LAP_SAW_ATTACHED_LANE.with(|saw| {
            let saw_attached_lane = saw.get();
            if saw_attached_lane {
                FRONTIER_CATALOG_CURSOR.with_borrow_mut(|cursor| *cursor = None);
            }
            saw_attached_lane
        });
    };
    FRONTIER_LAP_SAW_ATTACHED_LANE.with(|saw| saw.set(true));

    let snapshot = match derive_frontier_snapshot_for_lane(lane.vector_target, lane.shard_id) {
        Ok(snapshot) => snapshot,
        Err(_) => return true,
    };
    let exact_lane = (snapshot.vector_target, snapshot.shard_id);
    if snapshot.marker_keys.is_empty()
        && FRONTIER_LANE_PROGRESS.with_borrow(|progress| {
            progress
                .get(&exact_lane)
                .is_some_and(|last| snapshot.frontier <= *last)
        })
    {
        return true;
    }

    let publication_succeeded = crate::vector_sync::publish_router_frontier(
        snapshot.vector_target,
        snapshot.shard_id,
        snapshot.frontier,
    )
    .await
    .is_ok();
    if publication_succeeded {
        FRONTIER_LANE_PROGRESS.with_borrow_mut(|progress| {
            let previous = progress.entry(exact_lane).or_insert(0);
            *previous = (*previous).max(snapshot.frontier);
        });
        if !snapshot.marker_keys.is_empty() {
            let _ = retire_frontier_snapshot(&snapshot);
        }
    }
    true
}

fn catalog_page_requires_follow_up(page: &graph_catalog::AttachedVectorLanePage) -> bool {
    page.next_cursor.is_some()
}

/// Run one bounded recovery pass. Operations are grouped only in heap for one call and every group
/// is fenced by the exact target captured in each stable row.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the Router recovery timer")
)]
pub(crate) async fn run_recovery_pass(
    start_after: Option<u64>,
    budget: usize,
) -> RecoveryPassOutcome {
    let (rows, last_key, scanned) = scan(start_after, budget);
    let mut vector_rows = Vec::new();
    let mut found = false;
    for row in rows {
        found = true;
        if initial_delivery_active(row.mutation_id) {
            continue;
        }
        match row.phase {
            VectorIngestIntentPhase::AwaitingGraph => {
                match crate::graph_client::stamp_embedding(row.graph_target, row.graph_args()).await
                {
                    Ok(crate::graph_client::GraphStampOutcome::Accepted(mutation_id)) => {
                        if let Ok(next) = observe_graph_accept(&row, mutation_id) {
                            vector_rows.push(next);
                        }
                    }
                    Ok(crate::graph_client::GraphStampOutcome::Rejected(_)) => {
                        let _ = observe_graph_reject(&row);
                    }
                    Err(_) => {}
                }
            }
            VectorIngestIntentPhase::AwaitingVector => vector_rows.push(row),
            VectorIngestIntentPhase::AwaitingFrontier => {}
        }
    }

    let groups = group_vector_rows(vector_rows);
    for ((vector_target, _shard_id), submitted) in groups {
        let operations: Vec<_> = submitted
            .iter()
            .map(VectorIngestOutboxState::vector_operation)
            .collect();
        let Ok(outcome) =
            crate::vector_sync::vector_sync_batch_outcome(vector_target, operations).await
        else {
            continue;
        };
        let _ = apply_outcome(&submitted, outcome);
    }

    // Catalog discovery makes markerless Graph-owned lanes eligible while preserving the same
    // one-lane frontier endpoint and exact marker retirement for direct-ingestion rows.
    found |= run_catalog_frontier_pass().await;

    RecoveryPassOutcome {
        next_cursor: if scanned < budget as u32 {
            None
        } else {
            last_key
        },
        found,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::ROUTER_SHARDS;
    use candid::Principal;
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::{
        GraphShardKey, LocalVertexId, ShardId, ShardRegistryEntry,
    };
    use gleaph_graph_kernel::vector_index::{
        IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
        VectorSyncTerminalError,
    };
    use ic_stable_structures::memory_manager::MemoryId;
    use ic_stable_structures::{BTreeMap, Cell, VectorMemory};
    use ic_stable_variable_memory_manager::MemoryManager;
    fn target(seed: u8) -> Principal {
        Principal::from_slice(&[seed; 29])
    }

    fn spec() -> IndexedEmbeddingSpec {
        IndexedEmbeddingSpec {
            embedding_name_id: 3,
            index_id: 7,
            kind: VectorIndexKind::IvfFlat,
            metric: VectorMetric::L2Squared,
            encoding: VectorEncoding::F32,
            dims: 1,
            labels: vec![VertexLabelId::from_raw(1)],
        }
    }

    fn intent(
        mutation_id: u64,
        value: u8,
        phase: VectorIngestIntentPhase,
    ) -> VectorIngestOutboxState {
        let state = VectorIngestOutboxState::awaiting_graph(
            NewVectorIngestIntent {
                graph_id: GraphId::from_raw(1),
                graph_target: target(9),
                vector_target: target(1),
                shard_id: ShardId::new(2),
                local_vertex_id: LocalVertexId::from(value as u32),
                spec: spec(),
                bytes: vec![value, 0, 0, 0],
            },
            mutation_id,
        );
        match phase {
            VectorIngestIntentPhase::AwaitingGraph => state,
            VectorIngestIntentPhase::AwaitingVector => state.awaiting_vector(),
            VectorIngestIntentPhase::AwaitingFrontier => state.awaiting_frontier(),
        }
    }

    fn vector_intent(mutation_id: u64, value: u8) -> VectorIngestOutboxState {
        intent(mutation_id, value, VectorIngestIntentPhase::AwaitingVector)
    }

    fn graph_intent(mutation_id: u64, value: u8) -> VectorIngestOutboxState {
        intent(mutation_id, value, VectorIngestIntentPhase::AwaitingGraph)
    }

    #[test]
    fn outbox_key_serialization_is_fixed_width_round_trip_and_order_compatible() {
        let keys = vec![
            VectorIngestOutboxKey::new(
                2,
                Principal::from_slice(&[7]),
                ShardId::new(4),
                VectorIngestIntentPhase::AwaitingGraph,
            )
            .expect("valid key"),
            VectorIngestOutboxKey::new(
                1,
                Principal::from_slice(&[9; 29]),
                ShardId::new(3),
                VectorIngestIntentPhase::AwaitingVector,
            )
            .expect("valid key"),
            VectorIngestOutboxKey::new(
                1,
                Principal::from_slice(&[7]),
                ShardId::new(4),
                VectorIngestIntentPhase::AwaitingFrontier,
            )
            .expect("valid key"),
            VectorIngestOutboxKey::new(
                1,
                Principal::from_slice(&[7]),
                ShardId::new(4),
                VectorIngestIntentPhase::AwaitingVector,
            )
            .expect("valid key"),
            VectorIngestOutboxKey::new(
                1,
                Principal::from_slice(&[7]),
                ShardId::new(4),
                VectorIngestIntentPhase::AwaitingGraph,
            )
            .expect("valid key"),
            VectorIngestOutboxKey::new(
                1,
                Principal::from_slice(&[7, 0]),
                ShardId::new(1),
                VectorIngestIntentPhase::AwaitingGraph,
            )
            .expect("valid key"),
        ];

        for key in &keys {
            let encoded = key.to_bytes();
            assert_eq!(encoded.len(), VectorIngestOutboxKey::BYTE_WIDTH);
            assert_eq!(&encoded[..8], &key.mutation_id.to_be_bytes());
            assert_eq!(
                VectorIngestOutboxKey::from_bytes(Cow::Owned(encoded.into_owned())),
                *key
            );
        }

        let mut by_struct = keys.clone();
        by_struct.sort();
        let mut by_bytes = keys;
        by_bytes.sort_by(|left, right| left.to_bytes().cmp(&right.to_bytes()));
        assert_eq!(
            by_bytes, by_struct,
            "stable key bytes must preserve mutation/target/shard/phase order"
        );
    }

    #[test]
    fn initial_delivery_guard_excludes_only_its_scoped_mutation_ids() {
        let rows = [graph_intent(41, 1), graph_intent(42, 2)];
        assert!(!initial_delivery_active(41));
        {
            let _guard = InitialDeliveryGuard::new(&rows);
            assert!(initial_delivery_active(41));
            assert!(initial_delivery_active(42));
            assert!(!initial_delivery_active(43));
        }
        assert!(!initial_delivery_active(41));
        assert!(!initial_delivery_active(42));
    }

    fn clear_production_rows() {
        ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| table.clear_new());
    }

    fn attach_catalog_lanes(lanes: &[(Principal, ShardId)]) {
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        for (index, (vector_target, shard_id)) in lanes.iter().enumerate() {
            let graph_id = GraphId::from_raw(100 + index as u32);
            let key = GraphShardKey::new(graph_id, *shard_id);
            shards_insert_catalog_lane(
                key,
                ShardRegistryEntry {
                    shard_id: *shard_id,
                    graph_canister: Principal::from_slice(&[(100 + index) as u8; 29]),
                    index_canister: Principal::management_canister(),
                    graph_id,
                    registered_at_ns: 0,
                    index_attached: true,
                    vector_canister: Some(*vector_target),
                    vector_index_attached: true,
                },
            );
        }
    }

    fn shards_insert_catalog_lane(key: GraphShardKey, entry: ShardRegistryEntry) {
        ROUTER_SHARDS.with_borrow_mut(|shards| {
            shards.insert(key, entry.into());
        });
    }

    fn production_snapshot() -> Vec<(u64, VectorIngestOutboxState)> {
        ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
            table
                .iter()
                .map(|entry| {
                    let key = *entry.key();
                    (key.mutation_id, state_from_entry(key, entry.value()))
                })
                .collect()
        })
    }

    fn production_bytes_snapshot() -> Vec<(Vec<u8>, Vec<u8>)> {
        ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
            table
                .iter()
                .map(|entry| {
                    (
                        entry.key().to_bytes().into_owned(),
                        entry.value().to_bytes().into_owned(),
                    )
                })
                .collect()
        })
    }

    #[test]
    fn vector_rows_group_by_exact_target_and_shard_preserving_mutation_order() {
        let first_target = target(1);
        let second_target = target(2);
        let rows = vec![
            VectorIngestOutboxState {
                vector_target: first_target,
                shard_id: ShardId::new(2),
                ..vector_intent(10, 1)
            },
            VectorIngestOutboxState {
                vector_target: first_target,
                shard_id: ShardId::new(3),
                ..vector_intent(11, 2)
            },
            VectorIngestOutboxState {
                vector_target: second_target,
                shard_id: ShardId::new(2),
                ..vector_intent(12, 3)
            },
            VectorIngestOutboxState {
                vector_target: first_target,
                shard_id: ShardId::new(2),
                ..vector_intent(13, 4)
            },
        ];

        let groups = group_vector_rows(rows);
        assert_eq!(groups.len(), 3);
        assert_eq!(
            groups
                .get(&(first_target, ShardId::new(2)))
                .expect("first target/shard group")
                .iter()
                .map(|row| row.mutation_id)
                .collect::<Vec<_>>(),
            vec![10, 13]
        );
        assert_eq!(
            groups
                .get(&(first_target, ShardId::new(3)))
                .expect("first target/second shard group")
                .iter()
                .map(|row| row.mutation_id)
                .collect::<Vec<_>>(),
            vec![11]
        );
        assert_eq!(
            groups
                .get(&(second_target, ShardId::new(2)))
                .expect("second target/first shard group")
                .iter()
                .map(|row| row.mutation_id)
                .collect::<Vec<_>>(),
            vec![12]
        );
    }

    #[test]
    fn append_uses_one_stable_owner_and_reopens() {
        let _guard = test_lock();
        clear_production_rows();
        let row = vector_intent(41, 9);
        insert_intents_for_test(std::slice::from_ref(&row)).expect("append row");
        let (rows, _, _) = scan(None, 8);
        assert_eq!(rows, vec![row.clone()]);
        assert_eq!(total_len(), 1);
        clear_production_rows();

        let memory = VectorMemory::default();
        let manager = MemoryManager::init_with_policies(
            memory.clone(),
            2,
            &[(MemoryId::new(52), 1), (MemoryId::new(53), 16)],
        );
        let mut vector_index_allocator = Cell::init(manager.get(MemoryId::new(52)), 1u32);
        let mut map: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(manager.get(MemoryId::new(53)));
        let state = vector_intent(42, 8);
        vector_index_allocator.set(17);
        let key = VectorIngestOutboxKey::from_state(&state);
        map.insert(key, VectorIngestOutboxValue::from_state(&state));
        drop(map);
        drop(vector_index_allocator);
        drop(manager);

        let reopened_manager = MemoryManager::init_with_policies(
            memory,
            2,
            &[(MemoryId::new(52), 1), (MemoryId::new(53), 16)],
        );
        let reopened_allocator = Cell::init(reopened_manager.get(MemoryId::new(52)), 1u32);
        let reopened: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(reopened_manager.get(MemoryId::new(53)));
        let reopened_state =
            state_from_entry(key, reopened.get(&key).expect("reopened outbox value"));
        assert_eq!(reopened_state, state);
        assert_eq!(*reopened_allocator.get(), 17);
    }

    #[test]
    fn awaiting_frontier_round_trips_and_reopens() {
        let _guard = test_lock();
        clear_production_rows();
        let row = intent(42, 8, VectorIngestIntentPhase::AwaitingFrontier);
        insert_intents_for_test(std::slice::from_ref(&row)).expect("append frontier marker");
        assert_eq!(scan(None, 8).0, vec![row.clone()]);
        clear_production_rows();

        let memory = VectorMemory::default();
        let manager =
            MemoryManager::init_with_policies(memory.clone(), 2, &[(MemoryId::new(53), 16)]);
        let mut map: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(manager.get(MemoryId::new(53)));
        let key = VectorIngestOutboxKey::from_state(&row);
        map.insert(key, VectorIngestOutboxValue::from_state(&row));
        drop(map);
        drop(manager);

        let reopened_manager =
            MemoryManager::init_with_policies(memory, 2, &[(MemoryId::new(53), 16)]);
        let reopened: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(reopened_manager.get(MemoryId::new(53)));
        assert_eq!(
            state_from_entry(key, reopened.get(&key).expect("reopened outbox value")),
            row
        );
    }

    #[test]
    fn reopening_multiple_keys_preserves_primary_mutation_order() {
        let memory = VectorMemory::default();
        let manager =
            MemoryManager::init_with_policies(memory.clone(), 2, &[(MemoryId::new(53), 16)]);
        let mut map: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(manager.get(MemoryId::new(53)));
        let states = vec![
            VectorIngestOutboxState {
                vector_target: target(2),
                shard_id: ShardId::new(4),
                ..intent(42, 2, VectorIngestIntentPhase::AwaitingFrontier)
            },
            VectorIngestOutboxState {
                vector_target: target(1),
                shard_id: ShardId::new(3),
                ..intent(41, 1, VectorIngestIntentPhase::AwaitingVector)
            },
            VectorIngestOutboxState {
                vector_target: target(1),
                shard_id: ShardId::new(2),
                ..intent(43, 3, VectorIngestIntentPhase::AwaitingGraph)
            },
        ];
        for state in &states {
            let key = VectorIngestOutboxKey::from_state(state);
            assert!(
                map.insert(key, VectorIngestOutboxValue::from_state(state))
                    .is_none()
            );
        }
        drop(map);
        drop(manager);

        let reopened_manager =
            MemoryManager::init_with_policies(memory, 2, &[(MemoryId::new(53), 16)]);
        let reopened: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(reopened_manager.get(MemoryId::new(53)));
        let expected_keys = {
            let mut keys: Vec<_> = states
                .iter()
                .map(VectorIngestOutboxKey::from_state)
                .collect();
            keys.sort();
            keys
        };
        let actual_keys: Vec<_> = reopened.iter().map(|entry| *entry.key()).collect();
        assert_eq!(actual_keys, expected_keys);
        let actual_states: Vec<_> = reopened
            .iter()
            .map(|entry| state_from_entry(*entry.key(), entry.value()))
            .collect();
        assert_eq!(
            actual_states
                .iter()
                .map(|state| state.mutation_id)
                .collect::<Vec<_>>(),
            vec![41, 42, 43]
        );
    }

    #[test]
    fn graph_acceptance_and_rejection_transition_only_exact_rows() {
        let _guard = test_lock();
        clear_production_rows();
        let accepted = graph_intent(43, 8);
        let rejected = graph_intent(44, 9);
        insert_intents_for_test(&[accepted.clone(), rejected.clone()]).expect("seed intents");

        let malformed = observe_graph_accept(&accepted, 99)
            .expect_err("mismatched Graph stamp must fail closed");
        assert!(malformed.contains("returned mutation_id"), "{malformed}");
        assert_eq!(scan(None, 8).0, vec![accepted.clone(), rejected.clone()]);

        let awaiting_vector =
            observe_graph_accept(&accepted, accepted.mutation_id).expect("accept exact intent");
        assert_eq!(
            awaiting_vector.phase,
            VectorIngestIntentPhase::AwaitingVector
        );
        assert_eq!(awaiting_vector.vector_operation().mutation_id, 43);
        assert_eq!(awaiting_vector.vector_operation().bytes, accepted.bytes);

        observe_graph_reject(&rejected).expect("reject exact intent");
        let rejected_frontier = intent(44, 9, VectorIngestIntentPhase::AwaitingFrontier);
        assert_eq!(scan(None, 8).0, vec![awaiting_vector, rejected_frontier]);
        clear_production_rows();
    }

    #[test]
    fn stale_graph_callback_cannot_change_a_different_phase() {
        let _guard = test_lock();
        clear_production_rows();
        let awaiting_graph = graph_intent(45, 7);
        insert_intents_for_test(std::slice::from_ref(&awaiting_graph)).expect("seed intent");
        let awaiting_vector = observe_graph_accept(&awaiting_graph, 45).expect("accept intent");

        assert!(observe_graph_reject(&awaiting_graph).is_err());
        assert_eq!(scan(None, 8).0, vec![awaiting_vector]);
        clear_production_rows();
    }

    #[test]
    fn phase_transition_collision_is_rejected_before_removing_the_old_key() {
        let _guard = test_lock();
        clear_production_rows();
        let awaiting_graph = graph_intent(46, 7);
        let existing_next_phase = intent(46, 8, VectorIngestIntentPhase::AwaitingVector);
        insert_intents_for_test(&[awaiting_graph.clone(), existing_next_phase.clone()])
            .expect("seed phase collision");
        let before = scan(None, 8).0;

        let error = observe_graph_accept(&awaiting_graph, awaiting_graph.mutation_id)
            .expect_err("existing next phase must reject the transition");
        assert!(error.contains("already has its next phase"), "{error}");
        assert_eq!(scan(None, 8).0, before);
        clear_production_rows();
    }

    #[test]
    fn progress_transitions_only_exact_applied_prefix_and_stale_replay_is_rejected() {
        let _guard = test_lock();
        clear_production_rows();
        let rows = vec![
            vector_intent(51, 1),
            vector_intent(52, 2),
            vector_intent(53, 3),
        ];
        insert_intents_for_test(&rows).expect("append rows");
        apply_outcome(&rows, VectorSyncBatchOutcome::Progress { applied: 1 })
            .expect("progress transition");
        let (remaining, _, _) = scan(None, 8);
        assert_eq!(
            remaining,
            vec![
                intent(51, 1, VectorIngestIntentPhase::AwaitingFrontier),
                rows[1].clone(),
                rows[2].clone()
            ]
        );
        // A duplicate response for the already transitioned prefix cannot mutate a newer phase.
        let before_stale_replay = production_bytes_snapshot();
        let duplicate = apply_outcome(&rows, VectorSyncBatchOutcome::Progress { applied: 1 })
            .expect_err("stale prefix response");
        assert!(
            duplicate.contains("disappeared before outcome"),
            "{duplicate}"
        );
        assert_eq!(
            production_bytes_snapshot(),
            before_stale_replay,
            "stale prefix response must leave every compact key and value byte unchanged"
        );
        assert_eq!(scan(None, 8).0, remaining);

        // A lost response leaves the exact suffix pending, so replaying that suffix remains safe.
        let replay = vec![rows[1].clone(), rows[2].clone()];
        apply_outcome(&replay, VectorSyncBatchOutcome::Progress { applied: 1 })
            .expect("replayed suffix progress");
        let (remaining, _, _) = scan(None, 8);
        assert_eq!(
            remaining,
            vec![
                intent(51, 1, VectorIngestIntentPhase::AwaitingFrontier),
                intent(52, 2, VectorIngestIntentPhase::AwaitingFrontier),
                rows[2].clone()
            ]
        );
        clear_production_rows();
    }

    #[test]
    fn multirow_progress_collision_rejects_before_any_prefix_transition() {
        let _guard = test_lock();
        clear_production_rows();
        let rows = vec![vector_intent(81, 1), vector_intent(82, 2)];
        let existing_frontier = intent(82, 99, VectorIngestIntentPhase::AwaitingFrontier);
        insert_intents_for_test(&[rows[0].clone(), rows[1].clone(), existing_frontier.clone()])
            .expect("seed applied prefix and later destination collision");
        let before_bytes = production_bytes_snapshot();

        let error = apply_outcome(&rows, VectorSyncBatchOutcome::Progress { applied: 2 })
            .expect_err("later applied row destination collision");
        assert!(error.contains("already has its frontier phase"), "{error}");

        let after = scan(None, 8).0;
        assert!(
            after.contains(&rows[0]),
            "earlier applied row must not transition"
        );
        assert!(
            after.contains(&rows[1]),
            "later submitted row must remain pending"
        );
        assert!(
            after.contains(&existing_frontier),
            "pre-existing frontier destination must remain unchanged"
        );
        assert_eq!(
            production_bytes_snapshot(),
            before_bytes,
            "a destination collision must preserve every stable key and value byte"
        );
        clear_production_rows();
    }

    #[test]
    fn malformed_outcome_leaves_rows_unchanged() {
        let _guard = test_lock();
        clear_production_rows();
        let rows = vec![vector_intent(61, 1), vector_intent(62, 2)];
        insert_intents_for_test(&rows).expect("append rows");
        let error = apply_outcome(
            &rows,
            VectorSyncBatchOutcome::Terminal {
                applied: 1,
                failed_index: 0,
                error: VectorSyncTerminalError::SubjectTablePressure,
            },
        )
        .expect_err("malformed terminal outcome");
        assert!(error.contains("failed_index == applied"));
        let (remaining, _, _) = scan(None, 8);
        assert_eq!(remaining, rows);
        clear_production_rows();
    }

    #[test]
    fn terminal_transitions_prefix_and_retains_failed_and_suffix_as_pending() {
        let _guard = test_lock();
        clear_production_rows();
        let rows = vec![
            vector_intent(71, 1),
            vector_intent(72, 2),
            vector_intent(73, 3),
        ];
        insert_intents_for_test(&rows).expect("append rows");
        apply_outcome(
            &rows,
            VectorSyncBatchOutcome::Terminal {
                applied: 1,
                failed_index: 1,
                error: VectorSyncTerminalError::IndexDefinitionTablePressure,
            },
        )
        .expect("terminal transition");
        let expected_scan = vec![
            intent(71, 1, VectorIngestIntentPhase::AwaitingFrontier),
            rows[1].clone(),
            rows[2].clone(),
        ];
        let (next_scan, _, _) = scan(None, 8);
        assert_eq!(next_scan, expected_scan);
        let expected_states = expected_scan
            .iter()
            .map(|row| (row.mutation_id, row.clone()))
            .collect::<Vec<_>>();
        assert_eq!(production_snapshot(), expected_states);
        clear_production_rows();
    }

    #[test]
    fn contiguous_frontier_blocks_exact_lane_and_ignores_other_lanes() {
        let _guard = test_lock();
        clear_production_rows();
        let exact_target = target(1);
        let other_target = target(2);
        let other_unresolved = VectorIngestOutboxState {
            vector_target: other_target,
            ..vector_intent(1, 3)
        };
        let other_marker = VectorIngestOutboxState {
            vector_target: other_target,
            ..intent(2, 4, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let exact_marker = VectorIngestOutboxState {
            vector_target: exact_target,
            ..intent(10, 1, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let exact_unresolved = VectorIngestOutboxState {
            vector_target: exact_target,
            ..intent(20, 2, VectorIngestIntentPhase::AwaitingVector)
        };
        let same_target_other_shard_marker = VectorIngestOutboxState {
            vector_target: exact_target,
            shard_id: ShardId::new(3),
            ..intent(5, 5, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let same_target_other_shard_unresolved = VectorIngestOutboxState {
            vector_target: exact_target,
            shard_id: ShardId::new(3),
            ..intent(6, 6, VectorIngestIntentPhase::AwaitingVector)
        };
        insert_intents_for_test(&[
            exact_marker.clone(),
            exact_unresolved,
            other_unresolved,
            other_marker.clone(),
            same_target_other_shard_marker.clone(),
            same_target_other_shard_unresolved,
        ])
        .expect("seed lane intents");

        let snapshots =
            derive_frontier_snapshots_from_rows(&scan(None, MAX_VECTOR_INGEST_OUTBOX_ROWS).0, 100)
                .expect("derive lane frontiers");
        let exact = snapshots
            .iter()
            .find(|snapshot| snapshot.vector_target == exact_target)
            .expect("exact lane snapshot");
        assert_eq!(exact.shard_id, ShardId::new(2));
        assert_eq!(exact.frontier, 19);
        assert_eq!(
            exact.marker_keys,
            vec![VectorIngestOutboxKey::from_state(&exact_marker)]
        );
        let same_target_other_shard = snapshots
            .iter()
            .find(|snapshot| {
                snapshot.vector_target == exact_target && snapshot.shard_id == ShardId::new(3)
            })
            .expect("same target's other shard snapshot");
        assert_eq!(same_target_other_shard.frontier, 5);
        assert_eq!(
            same_target_other_shard.marker_keys,
            vec![VectorIngestOutboxKey::from_state(
                &same_target_other_shard_marker
            )]
        );
        assert!(
            !snapshots
                .iter()
                .any(|snapshot| snapshot.vector_target == other_target),
            "the other lane's older unresolved intent blocks only that lane"
        );
        clear_production_rows();
    }

    #[test]
    fn markerless_frontier_uses_allocation_ceiling_and_exact_lane_unresolved_floor() {
        let _guard = test_lock();
        clear_production_rows();
        let selected_target = target(1);
        let other_target = target(2);
        let selected_marker = VectorIngestOutboxState {
            vector_target: selected_target,
            ..intent(10, 1, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let selected_unresolved = VectorIngestOutboxState {
            vector_target: selected_target,
            ..intent(20, 2, VectorIngestIntentPhase::AwaitingVector)
        };
        let selected_later_marker = VectorIngestOutboxState {
            vector_target: selected_target,
            ..intent(30, 3, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let other_unresolved = VectorIngestOutboxState {
            vector_target: other_target,
            ..intent(3, 4, VectorIngestIntentPhase::AwaitingGraph)
        };
        insert_intents_for_test(&[
            selected_marker.clone(),
            selected_unresolved,
            selected_later_marker.clone(),
            other_unresolved.clone(),
        ])
        .expect("seed exact-lane intents");
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(100));

        let selected = derive_frontier_snapshot_for_lane(selected_target, ShardId::new(2))
            .expect("derive selected exact lane");
        assert_eq!(selected.frontier, 19);
        assert_eq!(
            selected.marker_keys,
            vec![VectorIngestOutboxKey::from_state(&selected_marker)]
        );
        assert!(
            !selected
                .marker_keys
                .contains(&VectorIngestOutboxKey::from_state(&selected_later_marker)),
            "markers above the exact-lane frontier remain uncaptured"
        );

        clear_production_rows();
        insert_intents_for_test(std::slice::from_ref(&other_unresolved))
            .expect("seed unrelated-lane intent");
        let markerless = derive_frontier_snapshot_for_lane(selected_target, ShardId::new(2))
            .expect("derive empty exact lane");
        assert_eq!(markerless.frontier, 100);
        assert!(markerless.marker_keys.is_empty());
        let before_markerless_success = production_bytes_snapshot();
        retire_frontier_snapshot(&markerless).expect("empty marker snapshot is a stable no-op");
        assert_eq!(production_bytes_snapshot(), before_markerless_success);
        clear_production_rows();
    }

    #[test]
    fn frontier_response_loss_retains_exact_marker_snapshot() {
        let _guard = test_lock();
        clear_production_rows();
        let marker = intent(31, 1, VectorIngestIntentPhase::AwaitingFrontier);
        let other_marker = intent(32, 2, VectorIngestIntentPhase::AwaitingFrontier);
        insert_intents_for_test(&[marker.clone(), other_marker.clone()]).expect("seed markers");
        let before = production_snapshot();
        let snapshots =
            derive_frontier_snapshots_from_rows(&scan(None, MAX_VECTOR_INGEST_OUTBOX_ROWS).0, 32)
                .expect("derive marker snapshot");
        let snapshot = snapshots.first().expect("marker snapshot");
        assert_eq!(snapshot.frontier, 32);
        assert_eq!(
            snapshot.marker_keys,
            vec![
                VectorIngestOutboxKey::from_state(&marker),
                VectorIngestOutboxKey::from_state(&other_marker)
            ]
        );

        // A lost/unknown bounded response does not call retirement; the exact captured markers
        // remain the durable retry source unchanged.
        assert_eq!(production_snapshot(), before);
        clear_production_rows();
    }

    #[test]
    fn frontier_derivation_and_retirement_use_keys_without_decoding_large_values() {
        let _guard = test_lock();
        clear_production_rows();
        let marker = VectorIngestOutboxState {
            ..intent(1, 1, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let large_marker = VectorIngestOutboxState {
            bytes: vec![0xA5; 1024 * 1024],
            ..intent(2, 2, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let large_unresolved = VectorIngestOutboxState {
            bytes: vec![0x5A; 1024 * 1024],
            ..intent(3, 3, VectorIngestIntentPhase::AwaitingVector)
        };
        insert_intents_for_test(&[
            marker.clone(),
            large_marker.clone(),
            large_unresolved.clone(),
        ])
        .expect("seed markers and unresolved blocker");
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(3));
        reset_value_decode_count();

        assert!(has_pending_for_target_shard(
            marker.vector_target,
            marker.shard_id
        ));
        assert!(has_pending());
        assert_eq!(
            value_decode_count(),
            0,
            "key-only pending probes decoded a payload"
        );

        let (selected, last_key, scanned) = scan(None, 1);
        assert_eq!(selected, vec![marker.clone()]);
        assert_eq!(last_key, Some(1));
        assert_eq!(scanned, 1);
        assert_eq!(
            value_decode_count(),
            1,
            "scan must decode exactly its selected bounded batch"
        );
        reset_value_decode_count();

        let snapshot = derive_frontier_snapshots()
            .expect("derive key-only frontier snapshot")
            .into_iter()
            .next()
            .expect("marker snapshot");
        assert_eq!(
            snapshot.marker_keys,
            vec![
                VectorIngestOutboxKey::from_state(&marker),
                VectorIngestOutboxKey::from_state(&large_marker)
            ]
        );
        assert_eq!(snapshot.frontier, 2);
        assert_eq!(
            value_decode_count(),
            0,
            "frontier derivation decoded a payload"
        );

        retire_frontier_snapshot(&snapshot).expect("retire exact marker key");
        assert_eq!(
            value_decode_count(),
            0,
            "frontier retirement decoded a payload"
        );
        assert!(
            has_pending_for_target_shard(large_unresolved.vector_target, large_unresolved.shard_id),
            "unresolved large payload must remain pending after marker retirement"
        );
        clear_production_rows();
    }

    #[test]
    fn frontier_retirement_rejects_changed_key_without_partial_removal() {
        let _guard = test_lock();
        clear_production_rows();
        let first = intent(41, 1, VectorIngestIntentPhase::AwaitingFrontier);
        let second = intent(42, 2, VectorIngestIntentPhase::AwaitingFrontier);
        insert_intents_for_test(&[first.clone(), second.clone()]).expect("seed markers");
        let snapshot = derive_frontier_snapshots_from_rows(&scan(None, 8).0, 42)
            .expect("derive snapshot")
            .into_iter()
            .next()
            .expect("frontier snapshot");
        let changed = VectorIngestOutboxState {
            shard_id: ShardId::new(9),
            ..first.clone()
        };
        ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
            let first_key = VectorIngestOutboxKey::from_state(&first);
            assert!(table.remove(&first_key).is_some());
            table.insert(
                VectorIngestOutboxKey::from_state(&changed),
                VectorIngestOutboxValue::from_state(&changed),
            );
        });
        let error = retire_frontier_snapshot(&snapshot).expect_err("changed marker key");
        assert!(error.contains("disappeared"), "{error}");
        assert_eq!(total_len(), 2, "retirement must preflight before removal");
        assert_eq!(
            scan(None, 8).0[0],
            changed,
            "changed marker key remains after rejected snapshot"
        );
        assert_eq!(
            scan(None, 8).0[1],
            second,
            "unchanged marker remains after rejected snapshot"
        );

        let wrong_lane = VectorFrontierSnapshot {
            vector_target: target(8),
            shard_id: first.shard_id,
            frontier: 42,
            marker_keys: vec![VectorIngestOutboxKey::from_state(&changed)],
        };
        assert!(
            retire_frontier_snapshot(&wrong_lane)
                .expect_err("forged lane must not retire a marker")
                .contains("outside its captured lane")
        );
        assert_eq!(total_len(), 2, "forged snapshots must not mutate the map");
        clear_production_rows();
    }

    #[test]
    fn frontier_retirement_rejects_a_marker_that_changed_phase() {
        let _guard = test_lock();
        clear_production_rows();
        let marker = intent(47, 1, VectorIngestIntentPhase::AwaitingFrontier);
        insert_intents_for_test(std::slice::from_ref(&marker)).expect("seed marker");
        let captured_key = VectorIngestOutboxKey::from_state(&marker);
        let changed_key = VectorIngestOutboxKey {
            phase: VectorIngestIntentPhase::AwaitingVector,
            ..captured_key
        };
        ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
            let value = table.remove(&captured_key).expect("captured marker");
            assert!(table.insert(changed_key, value).is_none());
        });

        let snapshot = VectorFrontierSnapshot {
            vector_target: marker.vector_target,
            shard_id: marker.shard_id,
            frontier: marker.mutation_id,
            marker_keys: vec![changed_key],
        };
        let error = retire_frontier_snapshot(&snapshot).expect_err("changed phase");
        assert!(error.contains("not AwaitingFrontier"), "{error}");
        assert_eq!(total_len(), 1);
        assert_eq!(
            scan(None, 8).0[0].phase,
            VectorIngestIntentPhase::AwaitingVector
        );
        clear_production_rows();
    }

    #[test]
    fn frontier_retirement_removes_only_unchanged_captured_markers() {
        let _guard = test_lock();
        clear_production_rows();
        let captured = intent(51, 1, VectorIngestIntentPhase::AwaitingFrontier);
        let later = intent(52, 2, VectorIngestIntentPhase::AwaitingFrontier);
        insert_intents_for_test(std::slice::from_ref(&captured)).expect("seed captured marker");
        let snapshot = derive_frontier_snapshots_from_rows(&scan(None, 8).0, 51)
            .expect("derive snapshot")
            .into_iter()
            .next()
            .expect("frontier snapshot");
        insert_intents_for_test(std::slice::from_ref(&later)).expect("append later marker");

        retire_frontier_snapshot(&snapshot).expect("retire observed frontier snapshot");
        assert_eq!(scan(None, 8).0, vec![later]);
        clear_production_rows();
    }

    #[test]
    fn recovery_frontier_failure_rotates_to_next_lane_and_wraps() {
        let _guard = test_lock();
        clear_for_test();
        let first_target = target(1);
        let second_target = target(2);
        let first = VectorIngestOutboxState {
            vector_target: first_target,
            ..intent(61, 1, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let second = VectorIngestOutboxState {
            vector_target: second_target,
            ..intent(62, 2, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let same_target_other_shard = VectorIngestOutboxState {
            vector_target: first_target,
            shard_id: ShardId::new(3),
            ..intent(63, 3, VectorIngestIntentPhase::AwaitingFrontier)
        };
        insert_intents_for_test(&[
            first.clone(),
            second.clone(),
            same_target_other_shard.clone(),
        ])
        .expect("seed lanes");
        attach_catalog_lanes(&[
            (first_target, ShardId::new(2)),
            (first_target, ShardId::new(3)),
            (second_target, ShardId::new(2)),
        ]);
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(63));

        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let calls_for_publisher = calls.clone();
        let attempts_for_publisher = attempts.clone();
        let _publisher = crate::vector_sync::install_frontier_publisher(
            move |vector_target, shard_id, frontier| {
                calls_for_publisher
                    .borrow_mut()
                    .push((vector_target, shard_id, frontier));
                let attempt = attempts_for_publisher.get();
                attempts_for_publisher.set(attempt + 1);
                if attempt == 0 {
                    Err("first lane unavailable".to_string())
                } else {
                    Ok(())
                }
            },
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            scan(None, 8).0,
            vec![
                first.clone(),
                second.clone(),
                same_target_other_shard.clone()
            ],
            "a failed publication retains the exact marker snapshot"
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            scan(None, 8).0,
            vec![first.clone(), second.clone()],
            "the next healthy same-target/different-shard lane retires only its marker"
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            scan(None, 8).0,
            vec![first.clone()],
            "the next target lane retires only its captured marker"
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            scan(None, 8).0,
            vec![first.clone()],
            "catalog end reserves the next lap without same-call wrap"
        );
        assert_eq!(calls.borrow().len(), 3);

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert!(
            scan(None, 8).0.is_empty(),
            "rotation wraps to the first lane"
        );
        assert_eq!(
            *calls.borrow(),
            vec![
                // Marker-only lanes publish the global allocated-through ceiling, rather than
                // the largest marker mutation id in that lane.
                (first_target, ShardId::new(2), 63),
                (first_target, ShardId::new(3), 63),
                (second_target, ShardId::new(2), 63),
                (first_target, ShardId::new(2), 63),
            ]
        );
        clear_production_rows();
    }

    #[test]
    fn recovery_catalog_terminates_after_exactly_64_incomplete_rows() {
        let _guard = test_lock();
        clear_for_test();
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        let vector_target = target(8);
        for graph_raw in 1..=64 {
            let graph_id = GraphId::from_raw(graph_raw);
            let key = GraphShardKey::new(graph_id, ShardId::new(0));
            ROUTER_SHARDS.with_borrow_mut(|shards| {
                shards.insert(
                    key,
                    ShardRegistryEntry {
                        shard_id: key.shard_id,
                        graph_canister: Principal::from_slice(&[graph_raw as u8; 29]),
                        index_canister: Principal::management_canister(),
                        graph_id,
                        registered_at_ns: 0,
                        index_attached: true,
                        vector_canister: Some(vector_target),
                        vector_index_attached: false,
                    }
                    .into(),
                );
            });
        }

        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let calls_for_publisher = calls.clone();
        let _publisher = crate::vector_sync::install_frontier_publisher(
            move |vector_target, shard_id, frontier| {
                calls_for_publisher
                    .borrow_mut()
                    .push((vector_target, shard_id, frontier));
                Ok(())
            },
        );

        let first = futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            first,
            RecoveryPassOutcome {
                next_cursor: None,
                found: true,
            },
            "a full ineligible page keeps the catalog lap alive"
        );
        assert!(calls.borrow().is_empty());

        let second = futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            second,
            RecoveryPassOutcome {
                next_cursor: None,
                found: false,
            },
            "the empty page after exactly 64 ineligible rows ends the lap"
        );
        assert!(calls.borrow().is_empty());

        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        clear_for_test();
    }

    #[test]
    fn recovery_frontier_success_retires_only_captured_marker_when_later_work_arrives() {
        let _guard = test_lock();
        clear_for_test();
        let target = target(1);
        let captured = VectorIngestOutboxState {
            vector_target: target,
            ..intent(71, 1, VectorIngestIntentPhase::AwaitingFrontier)
        };
        let later = VectorIngestOutboxState {
            vector_target: target,
            ..intent(72, 2, VectorIngestIntentPhase::AwaitingFrontier)
        };
        insert_intents_for_test(std::slice::from_ref(&captured)).expect("seed captured marker");
        attach_catalog_lanes(&[(target, ShardId::new(2))]);
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(72));
        let later_for_publisher = later.clone();
        let _publisher = crate::vector_sync::install_frontier_publisher(
            move |_vector_target, _shard_id, _frontier| {
                insert_intents_for_test(std::slice::from_ref(&later_for_publisher))
                    .expect("append concurrent later marker");
                Ok(())
            },
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            scan(None, 8).0,
            vec![later],
            "a later marker must survive retirement of the captured key"
        );
        clear_production_rows();
    }

    #[test]
    fn markerless_response_loss_restart_and_detach_are_idempotent_and_fail_closed() {
        let _guard = test_lock();
        clear_for_test();
        let target = target(4);
        let shard_id = ShardId::new(2);
        attach_catalog_lanes(&[(target, shard_id)]);
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(10));

        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let calls_for_publisher = calls.clone();
        let attempts_for_publisher = attempts.clone();
        let _publisher = crate::vector_sync::install_frontier_publisher(
            move |vector_target, shard_id, frontier| {
                calls_for_publisher
                    .borrow_mut()
                    .push((vector_target, shard_id, frontier));
                let attempt = attempts_for_publisher.get();
                attempts_for_publisher.set(attempt + 1);
                if attempt == 0 {
                    Err("response lost after unknown markerless apply".to_string())
                } else {
                    Ok(())
                }
            },
        );

        // Empty MemoryId 53 still discovers the attached catalog lane. An unknown response does
        // not update the heap hint, so the cursor advances to the catalog end before the next lap
        // retries the same safe ceiling.
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert!(scan(None, 8).0.is_empty());
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            calls.borrow().len(),
            1,
            "catalog-end tick must not retry yet"
        );
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            *calls.borrow(),
            vec![(target, shard_id, 10), (target, shard_id, 10)]
        );

        // An observed success records only a heap hint; it is not a second durable owner and
        // the catalog-end tick after it suppresses a duplicate call while the safe frontier is
        // unchanged.
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(calls.borrow().len(), 2);

        // Upgrade loses the hint and cursor. Catalog rediscovery retries safely; Vector's
        // monotonic endpoint makes the duplicate application idempotent.
        FRONTIER_CATALOG_CURSOR.with_borrow_mut(|cursor| *cursor = None);
        FRONTIER_LANE_PROGRESS.with_borrow_mut(|progress| progress.clear());
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(calls.borrow().len(), 3);

        // Detach removes the canonical catalog lane. A stale heap state cannot publish to it.
        ROUTER_SHARDS.with_borrow_mut(|shards| shards.clear_new());
        FRONTIER_CATALOG_CURSOR.with_borrow_mut(|cursor| *cursor = None);
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(calls.borrow().len(), 3);
        clear_for_test();
    }

    #[test]
    fn markerless_failure_rotates_to_next_lane_and_wraps_without_starvation() {
        let _guard = test_lock();
        clear_for_test();
        let vector_target = target(4);
        let shard_a = ShardId::new(2);
        let shard_b = ShardId::new(3);
        attach_catalog_lanes(&[(vector_target, shard_a), (vector_target, shard_b)]);
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(10));

        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let calls_for_publisher = calls.clone();
        let _publisher = crate::vector_sync::install_frontier_publisher(
            move |vector_target, shard_id, frontier| {
                calls_for_publisher
                    .borrow_mut()
                    .push((vector_target, shard_id, frontier));
                if shard_id == shard_a {
                    Err("lane A unavailable".to_string())
                } else {
                    Ok(())
                }
            },
        );

        // A markerless failure keeps the catalog cursor advanced, so the next pass visits lane B.
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            *calls.borrow(),
            vec![(vector_target, shard_a, 10)],
            "one recovery pass makes at most one remote call"
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            *calls.borrow(),
            vec![(vector_target, shard_a, 10), (vector_target, shard_b, 10),],
            "lane B must run before lane A is retried"
        );

        // The catalog-end pass reserves the next lap without wrapping in the same tick.
        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            *calls.borrow(),
            vec![(vector_target, shard_a, 10), (vector_target, shard_b, 10),],
            "catalog-end pass must not make a second remote call"
        );

        futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            *calls.borrow(),
            vec![
                (vector_target, shard_a, 10),
                (vector_target, shard_b, 10),
                (vector_target, shard_a, 10),
            ],
            "the failed lane retries only after the catalog lap wraps"
        );
        clear_for_test();
    }

    #[test]
    fn short_empty_catalog_page_does_not_keep_recovery_alive() {
        let short_empty = graph_catalog::AttachedVectorLanePage {
            lane: None,
            next_cursor: None,
            scanned: 1,
        };
        assert!(!catalog_page_requires_follow_up(&short_empty));

        let full_empty = graph_catalog::AttachedVectorLanePage {
            lane: None,
            next_cursor: Some(GraphShardKey::new(GraphId::from_raw(64), ShardId::new(0))),
            scanned: graph_catalog::VECTOR_LANE_CATALOG_PAGE_BUDGET as u32,
        };
        assert!(catalog_page_requires_follow_up(&full_empty));
    }

    #[test]
    fn append_capacity_and_row_encoding_are_preflighted_without_partial_write() {
        let _guard = test_lock();
        clear_production_rows();
        let rows: Vec<_> = (1..MAX_VECTOR_INGEST_OUTBOX_ROWS as u64)
            .map(|mutation_id| vector_intent(mutation_id, 1))
            .collect();
        insert_intents_for_test(&rows).expect("fill bounded outbox to one row below capacity");
        let before_late_failure = production_snapshot();
        let late_rows = vec![
            vector_intent(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64, 1),
            vector_intent(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1, 1),
        ];
        let error = insert_intents_for_test(&late_rows).expect_err("late multirow capacity error");
        assert!(error.contains("capacity"));
        assert_eq!(production_snapshot(), before_late_failure);

        let final_row = vector_intent(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64, 1);
        insert_intents_for_test(std::slice::from_ref(&final_row)).expect("fill exact capacity");
        let full_snapshot = production_snapshot();
        let extra = vector_intent(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1, 1);
        let error =
            insert_intents_for_test(std::slice::from_ref(&extra)).expect_err("capacity error");
        assert!(error.contains("capacity"));
        assert_eq!(production_snapshot(), full_snapshot);

        clear_production_rows();
        let existing = vector_intent(8, 1);
        insert_intents_for_test(std::slice::from_ref(&existing)).expect("seed existing row");
        let before_oversized = production_snapshot();
        let oversized = VectorIngestOutboxState {
            bytes: vec![0; MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES],
            ..vector_intent(9, 1)
        };
        let error =
            insert_intents_for_test(std::slice::from_ref(&oversized)).expect_err("encoding error");
        assert!(error.contains("encoding"));
        assert_eq!(production_snapshot(), before_oversized);
        clear_production_rows();
    }

    #[test]
    fn frontier_markers_fill_capacity_without_transient_phase_overflow() {
        let _guard = test_lock();
        clear_production_rows();
        let markers: Vec<_> = (1..MAX_VECTOR_INGEST_OUTBOX_ROWS as u64)
            .map(|mutation_id| intent(mutation_id, 1, VectorIngestIntentPhase::AwaitingFrontier))
            .collect();
        insert_intents_for_test(&markers).expect("fill marker capacity to one below limit");
        let pending = vector_intent(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64, 2);
        insert_intents_for_test(std::slice::from_ref(&pending))
            .expect("append final AwaitingVector row");
        assert_eq!(total_len(), MAX_VECTOR_INGEST_OUTBOX_ROWS as u64);

        apply_outcome(
            std::slice::from_ref(&pending),
            VectorSyncBatchOutcome::Progress { applied: 1 },
        )
        .expect("move the full-capacity suffix row to a marker");
        assert_eq!(total_len(), MAX_VECTOR_INGEST_OUTBOX_ROWS as u64);
        let rows = scan(None, MAX_VECTOR_INGEST_OUTBOX_ROWS).0;
        assert_eq!(rows.len(), MAX_VECTOR_INGEST_OUTBOX_ROWS);
        assert_eq!(
            rows.last().expect("final marker").phase,
            VectorIngestIntentPhase::AwaitingFrontier
        );

        let extra = intent(
            MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1,
            3,
            VectorIngestIntentPhase::AwaitingFrontier,
        );
        let error = insert_intents_for_test(std::slice::from_ref(&extra))
            .expect_err("a marker beyond capacity must be rejected");
        assert!(error.contains("capacity"), "{error}");
        assert_eq!(total_len(), MAX_VECTOR_INGEST_OUTBOX_ROWS as u64);
        clear_production_rows();
    }

    #[test]
    fn outbox_value_is_unbounded_but_row_admission_remains_bounded() {
        assert_eq!(
            <VectorIngestOutboxValue as Storable>::BOUND,
            StorableBound::Unbounded
        );
        assert_eq!(
            <VectorIngestOutboxKey as Storable>::BOUND,
            StorableBound::Bounded {
                max_size: VectorIngestOutboxKey::BYTE_WIDTH as u32,
                is_fixed_size: true
            }
        );
        let key = VectorIngestOutboxKey::from_state(&vector_intent(100, 1));
        assert_eq!(key.to_bytes().len(), VectorIngestOutboxKey::BYTE_WIDTH);
        assert_eq!(
            <u64 as Storable>::BOUND,
            StorableBound::Bounded {
                max_size: 8,
                is_fixed_size: true
            }
        );
        let state = vector_intent(101, 1);
        assert!(
            state.encode_checked().expect("small row encoding").len()
                <= MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES
        );
    }

    #[test]
    fn small_outbox_row_does_not_allocate_from_transport_ceiling() {
        let memory = VectorMemory::default();
        let mut map: BTreeMap<VectorIngestOutboxKey, VectorIngestOutboxValue, _> =
            BTreeMap::init(memory.clone());
        let state = vector_intent(101, 1);
        map.insert(
            VectorIngestOutboxKey::from_state(&state),
            VectorIngestOutboxValue::from_state(&state),
        );

        // Stable allocation must remain materially smaller than the independent 2 MiB transport
        // admission ceiling. Read the backing memory directly so the assertion does not depend on
        // a page-size constant.
        let allocated_bytes = memory.borrow().len() as u64;
        assert!(
            allocated_bytes.saturating_mul(2) < MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES as u64,
            "small outbox row allocated {allocated_bytes} bytes"
        );
    }

    #[test]
    fn recovery_transport_retains_exact_id_target_and_payload_for_retry() {
        let _guard = test_lock();
        clear_production_rows();
        let row = VectorIngestOutboxState {
            vector_target: target(7),
            ..vector_intent(91, 42)
        };
        insert_intents_for_test(std::slice::from_ref(&row)).expect("append row");

        let pass = futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            pass,
            RecoveryPassOutcome {
                next_cursor: None,
                found: true,
            }
        );
        assert_eq!(production_snapshot(), vec![(91, row.clone())]);
        let (reconstructed, _, _) = scan(None, 8);
        assert_eq!(reconstructed, vec![row]);
        clear_production_rows();
    }
}
