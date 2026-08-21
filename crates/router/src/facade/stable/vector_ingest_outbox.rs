//! Router-owned durable suffix for direct vertex-embedding ingestion.
//!
//! Graph stamps are validation-only effects for this path. Once all successful stamps have been
//! collected, this map becomes the durable owner of the exact vector operations before the first
//! Vector call. One row is one operation, keyed by its mutation id; no heap batch is persisted.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSyncBatchOutcome};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::Bound;

use crate::facade::stable::ROUTER_VECTOR_INGEST_OUTBOX;
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

/// One durable pending direct-ingestion operation and its exact Vector target.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct VectorIngestOutboxState {
    pub(crate) vector_target: Principal,
    pub(crate) operation: VectorEmbeddingSyncOp,
}

impl VectorIngestOutboxState {
    fn pending(vector_target: Principal, operation: VectorEmbeddingSyncOp) -> Self {
        Self {
            vector_target,
            operation,
        }
    }

    fn matches_pending(&self, vector_target: Principal, operation: &VectorEmbeddingSyncOp) -> bool {
        self.vector_target == vector_target && self.operation == *operation
    }

    fn pending_parts(&self) -> (Principal, VectorEmbeddingSyncOp) {
        (self.vector_target, self.operation.clone())
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

pub(crate) type VectorIngestOutboxRow = (u64, Principal, VectorEmbeddingSyncOp);

/// Append all successfully stamped operations after a read-only preflight. No stable row is
/// changed when capacity, duplicate identity, or per-row encoding validation fails.
pub(crate) fn append_pending(rows: &[VectorIngestOutboxRow]) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut identities = BTreeSet::new();
    let prepared: Vec<_> = rows
        .iter()
        .map(|(mutation_id, vector_target, operation)| {
            if *mutation_id == 0 {
                return Err("vector-ingest outbox mutation_id must be nonzero".to_string());
            }
            if operation.mutation_id != *mutation_id {
                return Err(format!(
                    "vector-ingest outbox key {mutation_id} disagrees with operation mutation_id {}",
                    operation.mutation_id
                ));
            }
            if !identities.insert(*mutation_id) {
                return Err(format!(
                    "duplicate vector-ingest outbox mutation_id {mutation_id}"
                ));
            }
            let state = VectorIngestOutboxState::pending(*vector_target, operation.clone());
            state.encode_checked().map(|bytes| (state, bytes))
        })
        .collect::<Result<_, _>>()?;

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
        for (mutation_id, _, _) in rows {
            if table.get(mutation_id).is_some() {
                return Err(format!(
                    "vector-ingest outbox mutation_id {mutation_id} already exists"
                ));
            }
        }

        // Every fallible check above completed before this first stable mutation.
        for ((mutation_id, _, _), (state, _bytes)) in rows.iter().zip(prepared) {
            assert!(
                table.insert(*mutation_id, state).is_none(),
                "vector-ingest outbox identity was inserted during preflight"
            );
        }
        Ok(())
    })
}

/// Revalidate and append direct-ingestion work for one graph in one synchronous Router operation.
///
/// Graph stamping happens across awaits, so the shard row, its exact attached Vector target, and
/// the immutable definition may have changed before the suffix is admitted. This boundary owns the
/// final check immediately before the first outbox write. No await can interleave the checks and
/// [`append_pending`], and a failed check leaves both the catalog and the outbox unchanged.
pub(crate) fn append_pending_for_graph(
    graph_id: GraphId,
    rows: &[VectorIngestOutboxRow],
) -> Result<(), String> {
    for (_, vector_target, operation) in rows {
        let shard_id = operation.subject.shard_id();
        let shard = graph_catalog::lookup_shard_entry(graph_id, shard_id).ok_or_else(|| {
            format!(
                "vector-ingest outbox shard {shard_id:?} is no longer registered for graph {graph_id:?}"
            )
        })?;
        if !shard.index_attached {
            return Err(format!(
                "vector-ingest outbox shard {shard_id:?} is no longer live"
            ));
        }
        if !shard.vector_index_attached || shard.vector_canister != Some(*vector_target) {
            return Err(format!(
                "vector-ingest outbox shard {shard_id:?} is not attached to exact target {vector_target}"
            ));
        }

        let definition = vector_index_catalog::get_vector_index(graph_id, operation.index_id)
            .ok_or_else(|| {
                format!(
                    "vector-ingest outbox index {} is no longer defined for graph {graph_id:?}",
                    operation.index_id
                )
            })?;
        let definition_target =
            definition
                .target
                .map(|target| target.canister)
                .ok_or_else(|| {
                    format!(
                        "vector-ingest outbox index {} has no immutable target",
                        operation.index_id
                    )
                })?;
        if definition_target != *vector_target {
            return Err(format!(
                "vector-ingest outbox index {} target changed from {vector_target} to {definition_target}",
                operation.index_id
            ));
        }
        if operation.embedding_name_id != definition.embedding_name_id.raw()
            || operation.encoding != definition.encoding
            || operation.dims != definition.dims
            || operation.metric != definition.metric
        {
            return Err(format!(
                "vector-ingest outbox operation does not match current definition {}",
                operation.index_id
            ));
        }
    }

    append_pending(rows)
}

/// Return whether bounded direct-ingestion work remains for an exact Vector target and shard.
///
/// The outbox itself is capped at [`MAX_VECTOR_INGEST_OUTBOX_ROWS`], so one complete scan is
/// bounded and sufficient. Router shard unregister uses this gate before changing any lifecycle
/// state; a matching suffix must drain before the target/attachment identity can be detached.
pub(crate) fn has_pending_for_target_shard(vector_target: Principal, shard_id: ShardId) -> bool {
    let (rows, _, _) = scan(None, MAX_VECTOR_INGEST_OUTBOX_ROWS);
    rows.into_iter().any(|(_, target, operation)| {
        target == vector_target && operation.subject.shard_id() == shard_id
    })
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
            let (vector_target, operation) = entry.value().pending_parts();
            rows.push((mutation_id, vector_target, operation));
        }
    });
    (rows, last_key, scanned)
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
        for (mutation_id, vector_target, operation) in submitted {
            let state = table.get(mutation_id).ok_or_else(|| {
                format!("vector-ingest outbox row {mutation_id} disappeared before outcome")
            })?;
            if !state.matches_pending(*vector_target, operation) {
                return Err(format!(
                    "vector-ingest outbox row {mutation_id} no longer matches submitted operation"
                ));
            }
        }

        let applied = match outcome {
            VectorSyncBatchOutcome::Progress { applied }
            | VectorSyncBatchOutcome::Terminal { applied, .. } => applied as usize,
        };

        for (mutation_id, _, _) in &submitted[..applied] {
            assert!(
                table.remove(mutation_id).is_some(),
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

    let mut groups: BTreeMap<Principal, Vec<VectorIngestOutboxRow>> = BTreeMap::new();
    for row in rows {
        groups.entry(row.1).or_default().push(row);
    }

    let mut found = false;
    for (vector_target, submitted) in groups {
        found = true;
        let operations: Vec<_> = submitted
            .iter()
            .map(|(_, _, operation)| operation.clone())
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
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{
        VectorEncoding, VectorMetric, VectorSubject, VectorSyncTerminalError,
    };
    use ic_stable_structures::memory_manager::MemoryId;
    use ic_stable_structures::{BTreeMap, Cell, VectorMemory};
    use ic_stable_variable_memory_manager::MemoryManager;
    fn target(seed: u8) -> Principal {
        Principal::from_slice(&[seed; 29])
    }

    fn operation(mutation_id: u64, value: u8) -> VectorEmbeddingSyncOp {
        VectorEmbeddingSyncOp {
            index_id: 7,
            embedding_name_id: 3,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: value as u32,
            },
            mutation_id,
            encoding: VectorEncoding::F32,
            dims: 1,
            metric: VectorMetric::L2Squared,
            bytes: vec![value, 0, 0, 0],
            remove: false,
        }
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
        let row = (41, target(1), operation(41, 9));
        append_pending(std::slice::from_ref(&row)).expect("append row");
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
        let state = VectorIngestOutboxState::pending(target(2), operation(42, 8));
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
    fn progress_removes_only_exact_applied_prefix_and_replay_is_idempotent() {
        let _guard = test_lock();
        clear_production_rows();
        let rows = vec![
            (51, target(1), operation(51, 1)),
            (52, target(1), operation(52, 2)),
            (53, target(1), operation(53, 3)),
        ];
        append_pending(&rows).expect("append rows");
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
        let rows = vec![
            (61, target(1), operation(61, 1)),
            (62, target(1), operation(62, 2)),
        ];
        append_pending(&rows).expect("append rows");
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
            (71, target(1), operation(71, 1)),
            (72, target(1), operation(72, 2)),
            (73, target(1), operation(73, 3)),
        ];
        append_pending(&rows).expect("append rows");
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
            .map(|(mutation_id, vector_target, operation)| {
                (
                    *mutation_id,
                    VectorIngestOutboxState::pending(*vector_target, operation.clone()),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(production_snapshot(), expected_states);
        clear_production_rows();
    }

    #[test]
    fn append_capacity_and_row_encoding_are_preflighted_without_partial_write() {
        let _guard = test_lock();
        clear_production_rows();
        let rows: Vec<_> = (1..MAX_VECTOR_INGEST_OUTBOX_ROWS as u64)
            .map(|mutation_id| (mutation_id, target(1), operation(mutation_id, 1)))
            .collect();
        append_pending(&rows).expect("fill bounded outbox to one row below capacity");
        let before_late_failure = production_snapshot();
        let late_rows = vec![
            (
                MAX_VECTOR_INGEST_OUTBOX_ROWS as u64,
                target(1),
                operation(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64, 1),
            ),
            (
                MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1,
                target(1),
                operation(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1, 1),
            ),
        ];
        let error = append_pending(&late_rows).expect_err("late multirow capacity error");
        assert!(error.contains("capacity"));
        assert_eq!(production_snapshot(), before_late_failure);

        let final_row = (
            MAX_VECTOR_INGEST_OUTBOX_ROWS as u64,
            target(1),
            operation(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64, 1),
        );
        append_pending(std::slice::from_ref(&final_row)).expect("fill exact capacity");
        let full_snapshot = production_snapshot();
        let extra = (
            MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1,
            target(1),
            operation(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 + 1, 1),
        );
        let error = append_pending(std::slice::from_ref(&extra)).expect_err("capacity error");
        assert!(error.contains("capacity"));
        assert_eq!(production_snapshot(), full_snapshot);

        clear_production_rows();
        let existing = (8, target(1), operation(8, 1));
        append_pending(std::slice::from_ref(&existing)).expect("seed existing row");
        let before_oversized = production_snapshot();
        let oversized = (
            9,
            target(1),
            VectorEmbeddingSyncOp {
                bytes: vec![0; MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES],
                ..operation(9, 1)
            },
        );
        let error = append_pending(std::slice::from_ref(&oversized)).expect_err("encoding error");
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
        let state = VectorIngestOutboxState::pending(target(1), operation(101, 1));
        assert!(
            state.encode_checked().expect("small row encoding").len()
                <= MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES
        );
    }

    #[test]
    fn small_outbox_row_does_not_allocate_from_transport_ceiling() {
        let memory = VectorMemory::default();
        let mut map: BTreeMap<u64, VectorIngestOutboxState, _> = BTreeMap::init(memory.clone());
        map.insert(
            101,
            VectorIngestOutboxState::pending(target(1), operation(101, 1)),
        );

        // The old bounded value made one small-row insertion choose a page derived from the
        // complete 2 MiB admission ceiling. Two times that allocation must exceed the ceiling.
        // Read the backing memory directly so the assertion is independent of a page-size
        // constant while remaining derived from the shared admission limit.
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
        let row = (91, target(7), operation(91, 42));
        append_pending(std::slice::from_ref(&row)).expect("append row");

        let pass = futures::executor::block_on(run_recovery_pass(None, 8));
        assert_eq!(
            pass,
            RecoveryPassOutcome {
                next_cursor: None,
                found: true,
            }
        );
        assert_eq!(
            production_snapshot(),
            vec![(91, VectorIngestOutboxState::pending(row.1, row.2.clone()),)]
        );
        let (reconstructed, _, _) = scan(None, 8);
        assert_eq!(reconstructed, vec![row]);
        clear_production_rows();
    }
}
