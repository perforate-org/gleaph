//! Router-owned durable intent for direct vertex-embedding ingestion.
//!
//! One row owns an allocated mutation id before the first Graph await and remains authoritative
//! until Graph rejects it or Vector acknowledges it. The row stores canonical inputs and derives
//! the exact Graph and Vector wire requests for replay.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{LocalVertexId, ShardId};
use gleaph_graph_kernel::vector_index::{
    IndexedEmbeddingSpec, VectorEmbeddingSyncOp, VectorSubject, VectorSyncBatchOutcome,
    VertexEmbeddingIngestionArgs,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ops::Bound;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum VectorIngestIntentPhase {
    AwaitingGraph,
    AwaitingVector,
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
}

impl Storable for VectorIngestOutboxState {
    // The 2 MiB limit is an admission/transport rule, not a per-node storage allocation hint.
    // Keeping this value unbounded makes StableBTreeMap use its normal small pages while
    // `encode_checked` remains the single write-boundary check for the row-size ceiling.
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorIngestOutboxState"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorIngestOutboxState")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode VectorIngestOutboxState")
    }
}

pub(crate) type VectorIngestOutboxRow = VectorIngestOutboxState;

thread_local! {
    /// Heap-only exclusion for rows whose originating API call is still driving initial delivery.
    /// An upgrade clears it, allowing durable recovery to resume every unresolved row.
    static INITIAL_DELIVERY_ACTIVE: RefCell<BTreeSet<u64>> = const { RefCell::new(BTreeSet::new()) };
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
        for state in &prepared {
            if table.get(&state.mutation_id).is_some() {
                return Err(format!(
                    "vector-ingest outbox mutation_id {} already exists",
                    state.mutation_id
                ));
            }
        }

        // No fallible operation remains after this point.
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(final_mutation_id));
        for state in &prepared {
            assert!(
                table.insert(state.mutation_id, state.clone()).is_none(),
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
        for row in rows {
            if table.get(&row.mutation_id).is_some() {
                return Err(format!(
                    "vector-ingest outbox mutation_id {} already exists",
                    row.mutation_id
                ));
            }
        }
        for row in rows {
            table.insert(row.mutation_id, row.clone());
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
    }
}

/// Return whether bounded direct-ingestion work remains for an exact Vector target and shard.
///
/// The outbox itself is capped at [`MAX_VECTOR_INGEST_OUTBOX_ROWS`], so one complete scan is
/// bounded and sufficient. Router shard unregister uses this gate before changing any lifecycle
/// state; a matching suffix must drain before the target/attachment identity can be detached.
pub(crate) fn has_pending_for_target_shard(vector_target: Principal, shard_id: ShardId) -> bool {
    let (rows, _, _) = scan(None, MAX_VECTOR_INGEST_OUTBOX_ROWS);
    rows.into_iter()
        .any(|row| row.vector_target == vector_target && row.shard_id == shard_id)
}

/// Return whether any direct-ingestion suffix remains. Graph unregister uses this conservative
/// final purge gate because a suffix row is durable work that must not be orphaned by catalog purge.
pub(crate) fn has_pending() -> bool {
    let (_, _, scanned) = scan(None, MAX_VECTOR_INGEST_OUTBOX_ROWS);
    scanned != 0
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
        let lower = start_after.map_or(Bound::Unbounded, Bound::Excluded);
        for entry in table.range((lower, Bound::Unbounded)).take(budget) {
            let mutation_id = *entry.key();
            scanned = scanned.saturating_add(1);
            last_key = Some(mutation_id);
            let row = entry.value();
            assert_eq!(
                mutation_id, row.mutation_id,
                "vector-ingest outbox key disagrees with canonical mutation id"
            );
            rows.push(row);
        }
    });
    (rows, last_key, scanned)
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
        let current = table.get(&submitted.mutation_id).ok_or_else(|| {
            format!(
                "vector-ingest outbox row {} disappeared before Graph acceptance",
                submitted.mutation_id
            )
        })?;
        if !current.matches(submitted) {
            return Err(format!(
                "vector-ingest outbox row {} no longer matches Graph submission",
                submitted.mutation_id
            ));
        }
        let next = current.awaiting_vector();
        table.insert(next.mutation_id, next.clone());
        Ok(next)
    })
}

/// Resolve an observed exact Graph rejection without creating Vector work.
pub(crate) fn observe_graph_reject(submitted: &VectorIngestOutboxState) -> Result<(), String> {
    if submitted.phase != VectorIngestIntentPhase::AwaitingGraph {
        return Err("Graph rejection requires an AwaitingGraph intent".to_string());
    }
    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        let current = table.get(&submitted.mutation_id).ok_or_else(|| {
            format!(
                "vector-ingest outbox row {} disappeared before Graph rejection",
                submitted.mutation_id
            )
        })?;
        if !current.matches(submitted) {
            return Err(format!(
                "vector-ingest outbox row {} no longer matches Graph submission",
                submitted.mutation_id
            ));
        }
        assert!(table.remove(&submitted.mutation_id).is_some());
        Ok(())
    })
}

/// Apply a validated Vector outcome to the exact pending snapshot used for the request. All row
/// identity checks and outcome validation happen before the first removal, so a stale,
/// malformed, or cross-target response leaves the outbox unchanged.
pub(crate) fn apply_outcome(
    submitted: &[VectorIngestOutboxRow],
    outcome: VectorSyncBatchOutcome,
) -> Result<(), String> {
    outcome.validate(submitted.len()).map_err(str::to_string)?;
    if submitted.is_empty() {
        return Err("vector-ingest outbox outcome requires a nonempty submission".to_string());
    }

    ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        for expected in submitted {
            if expected.phase != VectorIngestIntentPhase::AwaitingVector {
                return Err(format!(
                    "vector-ingest outbox row {} is not awaiting Vector",
                    expected.mutation_id
                ));
            }
            let state = table.get(&expected.mutation_id).ok_or_else(|| {
                format!(
                    "vector-ingest outbox row {} disappeared before outcome",
                    expected.mutation_id
                )
            })?;
            if !state.matches(expected) {
                return Err(format!(
                    "vector-ingest outbox row {} no longer matches submitted operation",
                    expected.mutation_id
                ));
            }
        }

        let applied = match outcome {
            VectorSyncBatchOutcome::Progress { applied }
            | VectorSyncBatchOutcome::Terminal { applied, .. } => applied as usize,
        };

        for row in &submitted[..applied] {
            assert!(
                table.remove(&row.mutation_id).is_some(),
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecoveryPassOutcome {
    pub next_cursor: Option<u64>,
    pub found: bool,
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
    use std::collections::BTreeMap;

    let (rows, last_key, scanned) = scan(start_after, budget);
    if rows.is_empty() {
        return RecoveryPassOutcome {
            next_cursor: if scanned < budget as u32 {
                None
            } else {
                last_key
            },
            found: false,
        };
    }

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
        }
    }

    let mut groups: BTreeMap<Principal, Vec<VectorIngestOutboxRow>> = BTreeMap::new();
    for row in vector_rows {
        groups.entry(row.vector_target).or_default().push(row);
    }
    for (vector_target, submitted) in groups {
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
    use candid::Principal;
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::{LocalVertexId, ShardId};
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
        }
    }

    fn vector_intent(mutation_id: u64, value: u8) -> VectorIngestOutboxState {
        intent(mutation_id, value, VectorIngestIntentPhase::AwaitingVector)
    }

    fn graph_intent(mutation_id: u64, value: u8) -> VectorIngestOutboxState {
        intent(mutation_id, value, VectorIngestIntentPhase::AwaitingGraph)
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

    fn production_snapshot() -> Vec<(u64, VectorIngestOutboxState)> {
        ROUTER_VECTOR_INGEST_OUTBOX.with_borrow(|table| {
            table
                .iter()
                .map(|entry| (*entry.key(), entry.value()))
                .collect()
        })
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
        let mut map: BTreeMap<u64, VectorIngestOutboxState, _> =
            BTreeMap::init(manager.get(MemoryId::new(53)));
        let state = vector_intent(42, 8);
        vector_index_allocator.set(17);
        map.insert(42, state.clone());
        drop(map);
        drop(vector_index_allocator);
        drop(manager);

        let reopened_manager = MemoryManager::init_with_policies(
            memory,
            2,
            &[(MemoryId::new(52), 1), (MemoryId::new(53), 16)],
        );
        let reopened_allocator = Cell::init(reopened_manager.get(MemoryId::new(52)), 1u32);
        let reopened: BTreeMap<u64, VectorIngestOutboxState, _> =
            BTreeMap::init(reopened_manager.get(MemoryId::new(53)));
        assert_eq!(reopened.get(&42), Some(state));
        assert_eq!(*reopened_allocator.get(), 17);
    }

    #[test]
    fn graph_acceptance_transitions_exact_row_and_rejection_resolves_only_exact_row() {
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
        assert_eq!(scan(None, 8).0, vec![awaiting_vector]);
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
    fn progress_removes_only_exact_applied_prefix_and_replay_is_idempotent() {
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
        assert_eq!(remaining, rows[1..].to_vec());
        // A lost response leaves the exact suffix pending; replaying the same acknowledged prefix
        // is represented by applying the same prefix response only after it is still present.
        let replay = vec![rows[1].clone(), rows[2].clone()];
        apply_outcome(&replay, VectorSyncBatchOutcome::Progress { applied: 1 })
            .expect("replayed suffix progress");
        let (remaining, _, _) = scan(None, 8);
        assert_eq!(remaining, vec![rows[2].clone()]);
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
    fn terminal_removes_prefix_and_retains_failed_and_suffix_as_pending() {
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
        let expected_scan = rows[1..].to_vec();
        let (next_scan, _, _) = scan(None, 8);
        assert_eq!(next_scan, expected_scan);
        let expected_states = rows[1..]
            .iter()
            .map(|row| (row.mutation_id, row.clone()))
            .collect::<Vec<_>>();
        assert_eq!(production_snapshot(), expected_states);
        clear_production_rows();
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
    fn outbox_value_is_unbounded_but_row_admission_remains_bounded() {
        assert_eq!(
            <VectorIngestOutboxState as Storable>::BOUND,
            StorableBound::Unbounded
        );
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
        let mut map: BTreeMap<u64, VectorIngestOutboxState, _> = BTreeMap::init(memory.clone());
        map.insert(101, vector_intent(101, 1));

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
