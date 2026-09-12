//! Router-owned durable bulk-load parent/receipt transitions (ADR 0057).
//!
//! Every method performs all fallible validation and encoding before the first stable write.  The
//! apply blocks only contain checked, already-validated values and stable-map `insert`/`remove`
//! operations; an unexpected post-write condition is a corruption trap rather than a recoverable
//! error.  Graph calls are intentionally outside this module: the Router workflow persists a
//! child envelope here, then awaits the pinned Graph shard and records evidence through these
//! transition methods.

#![allow(
    dead_code,
    reason = "public bulk-load workflow is wired by the Router API slice"
)]

use super::idempotency::client_mutation_key;
use super::{CLIENT_MUTATION_KEY_TTL_NS, RouterStore, validate_client_mutation_key};
use crate::facade::stable::bulk_load::{
    BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP, BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP,
    BulkLoadChunkProgressV1, BulkLoadChunkReceiptKey, BulkLoadChunkReceiptRecordV1,
    BulkLoadGraphReceiptV1, StableBulkLoadChunkReceiptMap,
};
use crate::facade::stable::label_stats::{
    BulkLoadCoordinatorV1, BulkLoadLifecycleV1, BulkLoadTargetV1, RouterMutationPayloadV1,
    RouterMutationRecord, RouterMutationRequestIdentityV1,
};
use crate::facade::stable::{
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS, ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER,
};
use crate::state::RouterError;
use crate::types::AtomicInsertReceiptV1;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_structures::Storable;
#[cfg(test)]
use std::cell::Cell;
use std::ops::Bound;

// Test-only read accounting makes the bounded Finalize/GC contract observable without changing
// the production facade or stable-map representation.
#[cfg(test)]
thread_local! {
    pub(crate) static BULK_LOAD_RECEIPT_ROW_READS: Cell<usize> = const { Cell::new(0) };
}

/// Result of the dedicated Start admission facade.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum BulkLoadStartAdmission {
    Created { mutation_id: MutationId },
    Replay { record: Box<RouterMutationRecord> },
}

/// Outcome of admitting one bulk-load update chunk: either permission to dispatch its rows, or a
/// signal that the chunk already finished and may only be replayed from its stored receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkLoadUpdateAdmission {
    /// The job is still working on this chunk with the caller's targets. The caller must consult
    /// [`BulkLoadUpdateDispatchGate`] before dispatching each row.
    Granted,
    /// The chunk is durably completed. The caller must return the stored receipt and must not
    /// dispatch any row, even when its own fingerprint matches.
    AlreadyCompleted,
}

/// Dispatch permission for one in-flight update chunk, re-read from durable state before each
/// row so an `Abort` cannot be overtaken by a suspended retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkLoadUpdateDispatchGate {
    /// The job is appending this chunk: any not-yet-written row may start.
    Open,
    /// The job is aborting this chunk: rows already dispatched may be settled, no new row may
    /// start.
    SettleOnly,
    /// The job is terminal, finalizing, or on another chunk: no row may start or settle here.
    Closed,
}

/// Result of one bounded receipt-GC step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BulkLoadGcStepResult {
    pub scanned: u32,
    pub removed: u32,
    pub done: bool,
}

fn expired_terminal(record: &RouterMutationRecord, now: u64) -> bool {
    record.is_terminal()
        && record
            .as_v1()
            .terminal_at_ns
            .is_some_and(|at| now.saturating_sub(at) > CLIENT_MUTATION_KEY_TTL_NS)
}

fn bulk_parent(record: &RouterMutationRecord) -> Result<&BulkLoadCoordinatorV1, RouterError> {
    let identity_is_bulk = matches!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::BulkLoadJob
    );
    match record.payload() {
        RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) => {
            assert!(
                identity_is_bulk,
                "bulk-load payload has a non-bulk request identity"
            );
            coordinator
                .validate()
                .unwrap_or_else(|error| panic!("invalid durable bulk-load coordinator: {error}"));
            assert_eq!(
                coordinator.lifecycle.is_terminal(),
                record.as_v1().terminal_at_ns.is_some(),
                "bulk-load terminal lifecycle and terminal retention anchor disagree"
            );
            Ok(coordinator)
        }
        _ if identity_is_bulk => {
            panic!("bulk-load identity has a non-bulk payload")
        }
        _ => Err(RouterError::Conflict(
            "client_mutation_key belongs to a different mutation family".into(),
        )),
    }
}

fn bulk_parent_mut(
    record: &mut RouterMutationRecord,
) -> Result<&mut BulkLoadCoordinatorV1, RouterError> {
    let identity_is_bulk = matches!(
        record.as_v1().request_identity,
        RouterMutationRequestIdentityV1::BulkLoadJob
    );
    match record.payload_mut() {
        RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) => {
            assert!(
                identity_is_bulk,
                "bulk-load payload has a non-bulk request identity"
            );
            Ok(coordinator)
        }
        _ if identity_is_bulk => panic!("bulk-load identity has a non-bulk payload"),
        _ => Err(RouterError::Conflict(
            "client_mutation_key belongs to a different mutation family".into(),
        )),
    }
}
fn ensure_record_bound(record: &RouterMutationRecord) {
    assert!(
        record.to_bytes().len()
            <= gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
        "bulk-load Router parent exceeds safe payload bound after a validated transition"
    );
}

fn ensure_receipt_bound(receipt: &BulkLoadChunkReceiptRecordV1) {
    assert!(
        receipt.to_bytes().len()
            <= gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
        "bulk-load receipt exceeds safe payload bound after a validated transition"
    );
}

fn receipt_key(job_id: MutationId, chunk_index: u32) -> BulkLoadChunkReceiptKey {
    BulkLoadChunkReceiptKey::new(job_id, chunk_index)
}

fn job_receipt_rows(
    map: &StableBulkLoadChunkReceiptMap,
    job_id: MutationId,
    start_index: u32,
    limit: usize,
) -> Vec<(BulkLoadChunkReceiptKey, BulkLoadChunkReceiptRecordV1)> {
    map.range((
        Bound::Included(receipt_key(job_id, start_index)),
        Bound::Unbounded,
    ))
    .take_while(|entry| entry.key().job_mutation_id == job_id)
    .take(limit)
    .map(|entry| {
        #[cfg(test)]
        BULK_LOAD_RECEIPT_ROW_READS.with(|reads| reads.set(reads.get() + 1));
        (*entry.key(), entry.value().clone())
    })
    .collect()
}

fn has_job_receipt_at_or_after(
    map: &StableBulkLoadChunkReceiptMap,
    job_id: MutationId,
    start_index: u32,
) -> bool {
    map.range((
        Bound::Included(receipt_key(job_id, start_index)),
        Bound::Unbounded,
    ))
    .next()
    .filter(|entry| entry.key().job_mutation_id == job_id)
    .is_some()
}

impl RouterStore {
    /// Test-feature-only fixture setup: expand one real, publicly completed chunk into exactly 65
    /// valid completed receipt rows at the actual MemoryId 49 owner. Public Start/Append/Finalize
    /// establish the parent, placement, and template receipt before this seam is used; only the
    /// otherwise expensive repeated Graph chunks are synthesized.
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn test_expand_completed_bulk_load_receipts(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Result<(), RouterError> {
        const TEST_RECEIPT_COUNT: u32 = 65;

        let key = client_mutation_key(caller, graph_id, client_key);
        let mut record = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        let job_id = record.as_v1().mutation_id;
        let mut coordinator = bulk_parent(&record)?.clone();
        if coordinator.lifecycle != BulkLoadLifecycleV1::Completed
            || coordinator.next_chunk_index != 1
            || coordinator.committed_chunk_count != 1
            || coordinator.completed_chunk_count != 1
            || coordinator.receipt_gc_cursor.is_some()
        {
            return Err(RouterError::Conflict(
                "test bulk-load GC fixture requires one completed public chunk".into(),
            ));
        }
        let template = ROUTER_BULK_LOAD_CHUNK_RECEIPTS
            .with_borrow(|map| map.get(&receipt_key(job_id, 0)))
            .ok_or_else(|| {
                RouterError::Internal("bulk-load fixture receipt 0 is missing".into())
            })?;
        if template.progress != BulkLoadChunkProgressV1::Completed {
            return Err(RouterError::Conflict(
                "test bulk-load GC fixture receipt must be completed".into(),
            ));
        }
        let public_receipt = template.public_receipt.as_ref().ok_or_else(|| {
            RouterError::Internal("completed bulk-load fixture lacks public receipt".into())
        })?;
        let scale = u64::from(TEST_RECEIPT_COUNT);
        let logical_operation_count = public_receipt
            .logical_operation_count
            .checked_mul(scale)
            .ok_or_else(|| RouterError::Conflict("bulk-load fixture count overflow".into()))?;
        let logical_vertex_count = public_receipt
            .logical_vertex_count
            .checked_mul(scale)
            .ok_or_else(|| RouterError::Conflict("bulk-load fixture count overflow".into()))?;
        let logical_edge_count = public_receipt
            .logical_edge_count
            .checked_mul(scale)
            .ok_or_else(|| RouterError::Conflict("bulk-load fixture count overflow".into()))?;

        let mut extra_rows = Vec::with_capacity((TEST_RECEIPT_COUNT - 1) as usize);
        for chunk_index in 1..TEST_RECEIPT_COUNT {
            let mut row = template.clone();
            row.child_mutation_id = job_id
                .checked_add(u64::from(chunk_index) + 1)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))?;
            row.validate().map_err(RouterError::Conflict)?;
            ensure_receipt_bound(&row);
            extra_rows.push((receipt_key(job_id, chunk_index), row));
        }
        coordinator.logical_operation_count = logical_operation_count;
        coordinator.logical_vertex_count = logical_vertex_count;
        coordinator.logical_edge_count = logical_edge_count;
        coordinator.next_chunk_index = TEST_RECEIPT_COUNT;
        coordinator.committed_chunk_count = TEST_RECEIPT_COUNT;
        coordinator.completed_chunk_count = TEST_RECEIPT_COUNT;
        coordinator.validate()?;
        *bulk_parent_mut(&mut record)? = coordinator;
        ensure_record_bound(&record);

        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| {
            for (row_key, row) in extra_rows {
                map.insert(row_key, row);
            }
        });
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
        Ok(())
    }

    /// Test-feature-only diagnostic for the exact bulk GC seam.
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn test_bulk_load_gc_probe(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Result<(bool, Option<u32>, u32, Option<String>), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let Some(record) = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&key)) else {
            return Ok((false, None, 0, None));
        };
        let coordinator = bulk_parent(&record)?;
        let job_id = record.as_v1().mutation_id;
        let receipt_count = ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
            map.range((Bound::Included(receipt_key(job_id, 0)), Bound::Unbounded))
                .take_while(|entry| entry.key().job_mutation_id == job_id)
                .count() as u32
        });
        let outcome = match &coordinator.lifecycle {
            BulkLoadLifecycleV1::Completed => Some("Completed".to_owned()),
            BulkLoadLifecycleV1::Aborted => Some("Aborted".to_owned()),
            BulkLoadLifecycleV1::Failed { reason } => Some(format!("Failed:{reason}")),
            _ => None,
        };
        Ok((true, coordinator.receipt_gc_cursor, receipt_count, outcome))
    }

    /// Dedicated synchronous Start admission.  It never enters the generic scalar reservation
    /// path and never writes an intermediate identity/payload.  On a missing key, the counter and
    /// final BulkLoadJob/Open record are co-written after all preflight checks.  On exact replay,
    /// neither counter nor placement is touched.
    pub(crate) fn start_bulk_load_job(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        target: BulkLoadTargetV1,
        now: u64,
    ) -> Result<BulkLoadStartAdmission, RouterError> {
        validate_client_mutation_key(client_key)?;
        target.validate()?;
        let key = client_mutation_key(caller, graph_id, client_key);
        if let Some(record) = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&key)) {
            let coordinator = bulk_parent(&record)?;
            if coordinator.receipt_gc_cursor.is_some() {
                return Err(RouterError::Conflict(
                    "client_mutation_key expired while bulk-load receipt GC is active".into(),
                ));
            }
            return Ok(BulkLoadStartAdmission::Replay {
                record: Box::new(record),
            });
        }

        let mutation_id = ROUTER_MUTATION_COUNTER.with_borrow(|counter| {
            counter
                .get()
                .checked_add(1)
                .filter(|next| *next != 0)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))
        })?;
        let coordinator = BulkLoadCoordinatorV1::new(target);
        let record = RouterMutationRecord::new_bulk_load(mutation_id, now, coordinator)?;
        let record_bytes = record.to_bytes();
        assert!(
            record_bytes.len()
                <= gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES,
            "bulk-load Start record bound changed after preflight"
        );

        // First durable write is the counter.  The remaining inserts are infallible stable-map
        // writes; an unexpected failure traps so IC message rollback restores the counter.
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(mutation_id));
        #[cfg(feature = "pocket-ic-e2e")]
        crate::test_fault::maybe_trap_after_bulk_start_counter();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
        #[cfg(feature = "pocket-ic-e2e")]
        crate::test_fault::maybe_trap_after_bulk_start_parent();
        Ok(BulkLoadStartAdmission::Created { mutation_id })
    }

    pub(crate) fn bulk_load_chunk_receipt(
        &self,
        job_mutation_id: MutationId,
        chunk_index: u32,
    ) -> Option<BulkLoadChunkReceiptRecordV1> {
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS
            .with_borrow(|map| map.get(&receipt_key(job_mutation_id, chunk_index)))
    }

    pub(crate) fn list_bulk_load_chunk_receipts(
        &self,
        job_mutation_id: MutationId,
        receipt_cursor: u32,
        max_receipts: u32,
    ) -> Result<Vec<(u32, BulkLoadChunkReceiptRecordV1)>, RouterError> {
        if max_receipts == 0
            || max_receipts > crate::facade::stable::bulk_load::MAX_BULK_LOAD_RECEIPTS_PER_PAGE
        {
            return Err(RouterError::InvalidArgument(format!(
                "max_receipts must be in 1..={}",
                crate::facade::stable::bulk_load::MAX_BULK_LOAD_RECEIPTS_PER_PAGE
            )));
        }
        Ok(ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
            job_receipt_rows(map, job_mutation_id, receipt_cursor, max_receipts as usize)
                .into_iter()
                .map(|(key, value)| (key.chunk_index, value))
                .collect()
        }))
    }

    /// Whether a committed receipt row exists at or after `receipt_cursor`.
    ///
    /// Status pagination uses this bounded existence probe to avoid returning a continuation
    /// cursor for a page that happens to be exactly full while still being the terminal page.
    pub(crate) fn bulk_load_has_chunk_receipt_at_or_after(
        &self,
        job_mutation_id: MutationId,
        receipt_cursor: u32,
    ) -> bool {
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS
            .with_borrow(|map| has_job_receipt_at_or_after(map, job_mutation_id, receipt_cursor))
    }

    /// Persist parent `AppendPending` and a complete child `CanonicalPending` envelope in one
    /// no-await stable boundary, allocating a distinct child id from the existing Router counter.
    pub(crate) fn admit_bulk_load_child(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
        mut child: BulkLoadChunkReceiptRecordV1,
    ) -> Result<MutationId, RouterError> {
        validate_client_mutation_key(client_key)?;
        let key = client_mutation_key(caller, graph_id, client_key);
        let mut parent = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        if parent.as_v1().mutation_id != parent_mutation_id {
            return Err(RouterError::Conflict(
                "bulk-load parent mutation id mismatch".into(),
            ));
        }
        let mut coordinator = bulk_parent(&parent)?.clone();
        if coordinator.receipt_gc_cursor.is_some() {
            return Err(RouterError::Conflict(
                "client_mutation_key expired while bulk-load receipt GC is active".into(),
            ));
        }
        let existing_key = receipt_key(parent_mutation_id, chunk_index);
        if let Some(existing) =
            ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| map.get(&existing_key))
        {
            if existing.chunk_fingerprint == chunk_fingerprint {
                existing.validate().unwrap_or_else(|error| {
                    panic!("invalid durable bulk-load child receipt: {error}")
                });
                return Ok(existing.child_mutation_id);
            }
            return Err(RouterError::Conflict(
                "bulk-load chunk index was already used for a different fingerprint".into(),
            ));
        }
        if !matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Open)
            || chunk_index != coordinator.next_chunk_index
        {
            return Err(RouterError::Conflict(
                "bulk-load append is not the next admissible chunk".into(),
            ));
        }
        if child.chunk_fingerprint != chunk_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load child fingerprint mismatch".into(),
            ));
        }
        child.child_mutation_id = 1;
        child.progress = BulkLoadChunkProgressV1::CanonicalPending;
        child.public_receipt = None;
        child.graph_receipt = None;
        child.completed_at_ns = None;
        // The child id is allocated only after all request/fingerprint validation is complete.
        let child_mutation_id = ROUTER_MUTATION_COUNTER.with_borrow(|counter| {
            counter
                .get()
                .checked_add(1)
                .filter(|next| *next != 0 && *next != parent_mutation_id)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))
        })?;
        child.child_mutation_id = child_mutation_id;
        child.validate().map_err(RouterError::InvalidArgument)?;
        let (request_graph_id, target_shard, target_canister) = child
            .graph_request
            .as_ref()
            .ok_or_else(|| RouterError::Internal("bulk-load child lacks Graph request".into()))?
            .target();
        if request_graph_id != graph_id
            || target_shard != coordinator.target.shard_id
            || target_canister != coordinator.target.graph_canister
        {
            return Err(RouterError::Conflict(
                "bulk-load child request target differs from pinned parent target".into(),
            ));
        }
        let receipt_key = existing_key;
        let receipt_absent =
            ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| map.get(&receipt_key).is_none());
        if !receipt_absent {
            return Err(RouterError::Conflict(
                "bulk-load child receipt row already exists".into(),
            ));
        }
        coordinator.lifecycle = BulkLoadLifecycleV1::AppendPending {
            chunk_index,
            fingerprint: chunk_fingerprint,
            child_mutation_id,
        };
        coordinator.validate()?;
        *bulk_parent_mut(&mut parent)? = coordinator;
        ensure_record_bound(&parent);
        ensure_receipt_bound(&child);

        // Counter, child row, and parent transition are the co-write.  No fallible operation is
        // reachable after this point.
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(child_mutation_id));
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.insert(receipt_key, child));
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, parent));
        Ok(child_mutation_id)
    }

    /// Admit an update chunk child row and grant (or refuse) permission to dispatch its rows.
    /// Unlike insert children, update rows carry resolved target identities rather than a Graph
    /// request. Resolution precedes admission; per-row GQL execution follows it. Completion
    /// publishes the full count, or the settled prefix when Abort discards an unwritten suffix.
    ///
    /// This is the canonical **dispatch-permission** boundary for the update lane. Dispatch may
    /// only be granted while the parent job is still open for this chunk and the child is not
    /// finished, so an `Abort` that has already closed the job (`AbortPending`/`Aborted`, both
    /// durable) can never be overtaken by a retry that was suspended earlier. A finished child
    /// returns [`BulkLoadUpdateAdmission::AlreadyCompleted`] — a receipt replay, *not* permission
    /// — so a stale caller cannot keep writing rows under a terminal job. Pending grants also
    /// require the caller's ordered targets to equal the durable list: another first Append may
    /// have won admission while this caller was resolving mutable match keys.
    pub(crate) fn admit_bulk_load_update_child(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
        resolved_update_vertex_ids: Option<Vec<Vec<u8>>>,
    ) -> Result<BulkLoadUpdateAdmission, RouterError> {
        validate_client_mutation_key(client_key)?;
        let key = client_mutation_key(caller, graph_id, client_key);
        let mut parent = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        if parent.as_v1().mutation_id != parent_mutation_id {
            return Err(RouterError::Conflict(
                "bulk-load parent mutation id mismatch".into(),
            ));
        }
        let mut coordinator = bulk_parent(&parent)?.clone();
        if coordinator.receipt_gc_cursor.is_some() {
            return Err(RouterError::Conflict(
                "client_mutation_key expired while bulk-load receipt GC is active".into(),
            ));
        }
        let existing_key = receipt_key(parent_mutation_id, chunk_index);
        if let Some(existing) =
            ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| map.get(&existing_key))
        {
            if existing.chunk_fingerprint != chunk_fingerprint
                || existing.updated_row_count.is_none()
            {
                return Err(RouterError::Conflict(
                    "bulk-load chunk index was already used for a different fingerprint".into(),
                ));
            }
            existing
                .validate()
                .unwrap_or_else(|error| panic!("invalid durable bulk-load child receipt: {error}"));
            // A finished chunk may only be *replayed*; it never re-opens dispatch permission,
            // because the enclosing job may already be terminal.
            if existing.progress.is_completed() {
                return Ok(BulkLoadUpdateAdmission::AlreadyCompleted);
            }
            if existing.resolved_update_vertex_ids != resolved_update_vertex_ids {
                return Err(RouterError::Conflict(
                    "bulk-load update targets differ from the admitted chunk".into(),
                ));
            }
            // A pending chunk keeps its grant only while the job is still working on this exact
            // chunk. `AppendPending` grants full dispatch; `AbortPending` grants only the
            // settle-only path, which [`Self::bulk_load_update_dispatch_gate`] enforces row by
            // row (an already-dispatched row may be settled, a never-started row may not). Every
            // other state — terminal, finalizing, another chunk, expired — refuses, so an Abort
            // ingressed while this caller was suspended cannot be overtaken by new row writes.
            match coordinator.lifecycle {
                BulkLoadLifecycleV1::AppendPending {
                    chunk_index: active,
                    child_mutation_id: active_child,
                    ..
                } if active == chunk_index && existing.child_mutation_id == active_child => {}
                BulkLoadLifecycleV1::AbortPending { active_chunk }
                    if active_chunk == chunk_index => {}
                BulkLoadLifecycleV1::AppendPending { .. } => {
                    return Err(RouterError::Conflict(
                        "bulk-load append is not the active chunk of this job".into(),
                    ));
                }
                _ => {
                    return Err(RouterError::Busy {
                        operation: "bulk_load.append".into(),
                    });
                }
            }
            return Ok(BulkLoadUpdateAdmission::Granted);
        }
        if !matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Open)
            || chunk_index != coordinator.next_chunk_index
        {
            return Err(RouterError::Conflict(
                "bulk-load append is not the next admissible chunk".into(),
            ));
        }
        let child_mutation_id = ROUTER_MUTATION_COUNTER.with_borrow(|counter| {
            counter
                .get()
                .checked_add(1)
                .filter(|next| *next != 0 && *next != parent_mutation_id)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))
        })?;
        let child = BulkLoadChunkReceiptRecordV1 {
            chunk_fingerprint,
            graph_request: None,
            graph_request_fingerprint: None,
            child_mutation_id,
            progress: BulkLoadChunkProgressV1::CanonicalPending,
            public_receipt: None,
            graph_receipt: None,
            resolved_update_vertex_ids,
            completed_at_ns: None,
            updated_row_count: Some(0),
        };
        child.validate().map_err(RouterError::InvalidArgument)?;
        coordinator.lifecycle = BulkLoadLifecycleV1::AppendPending {
            chunk_index,
            fingerprint: chunk_fingerprint,
            child_mutation_id,
        };
        coordinator.validate()?;
        *bulk_parent_mut(&mut parent)? = coordinator;
        ensure_record_bound(&parent);
        ensure_receipt_bound(&child);
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(child_mutation_id));
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.insert(existing_key, child));
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, parent));
        Ok(BulkLoadUpdateAdmission::Granted)
    }

    /// Current dispatch permission for one update chunk, read fresh from durable state. The bulk
    /// update loop re-checks this before every row so an `Abort` that ingressed while a previous
    /// row's Graph call was in flight cannot be followed by new row writes.
    ///
    /// `SettleOnly` means the job is winding down (`AbortPending` for this chunk): rows that were
    /// already dispatched may still be settled, but no never-dispatched row may start.
    pub(crate) fn bulk_load_update_dispatch_gate(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
    ) -> Result<BulkLoadUpdateDispatchGate, RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let gate = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| {
            let Some(record) = map.get(&key) else {
                return BulkLoadUpdateDispatchGate::Closed;
            };
            if record.as_v1().mutation_id != parent_mutation_id {
                return BulkLoadUpdateDispatchGate::Closed;
            }
            let Ok(coordinator) = bulk_parent(&record) else {
                return BulkLoadUpdateDispatchGate::Closed;
            };
            match coordinator.lifecycle {
                BulkLoadLifecycleV1::AppendPending {
                    chunk_index: active,
                    ..
                } if active == chunk_index => BulkLoadUpdateDispatchGate::Open,
                BulkLoadLifecycleV1::AbortPending { active_chunk }
                    if active_chunk == chunk_index =>
                {
                    BulkLoadUpdateDispatchGate::SettleOnly
                }
                _ => BulkLoadUpdateDispatchGate::Closed,
            }
        });
        Ok(gate)
    }

    /// Persist the committed prefix while retaining the pending child's fixed target identities.
    /// The client resends the fingerprint-identical payload; row journals settle the unrecorded
    /// suffix. The authored payload itself is not stored here.
    pub(crate) fn record_bulk_load_update_progress(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
        updated_row_count: u64,
    ) -> Result<(), RouterError> {
        self.with_bulk_child_transition(
            caller,
            graph_id,
            client_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            None,
            move |child, _| {
                if child.updated_row_count.is_none() {
                    return Err(RouterError::Conflict(
                        "bulk-load update progress targets an insert child row".into(),
                    ));
                }
                // The authored batch size is already durable in the retry guard, so the upper
                // bound needs no new field: a progress value can never exceed the rows that were
                // admitted. On a completed row the guard is compacted, so its recorded receipt
                // count is the bound instead.
                let admitted = match &child.resolved_update_vertex_ids {
                    Some(identities) => identities.len() as u64,
                    None => child.updated_row_count.unwrap_or(0),
                };
                if updated_row_count > admitted {
                    return Err(RouterError::Conflict(format!(
                        "bulk-load update progress {updated_row_count} exceeds the {admitted} admitted rows"
                    )));
                }
                if !matches!(child.progress, BulkLoadChunkProgressV1::CanonicalPending) {
                    // A terminal child never rewinds: an equal count is an idempotent replay,
                    // and a stale smaller notification cannot lower the durable prefix. A larger
                    // value contradicts the completed receipt, so it fails closed.
                    return match child.updated_row_count {
                        Some(recorded) if updated_row_count <= recorded => Ok(()),
                        _ => Err(RouterError::Conflict(
                            "bulk-load update progress conflicts with the completed chunk receipt"
                                .into(),
                        )),
                    };
                }
                // Monotonic within a pending child: a duplicate or delayed notification is a
                // no-op rather than a prefix rewind.
                if updated_row_count <= child.updated_row_count.unwrap_or(0) {
                    return Ok(());
                }
                child.updated_row_count = Some(updated_row_count);
                Ok(())
            },
        )
    }

    /// Complete an update chunk child row after every row of the candidate batch committed.
    /// `updated_row_count` is the committed prefix length; completing an already-`Completed`
    /// row is a no-op so `Append` replay converges. Committed and completed counters advance
    /// together, preserving the finalize aggregate invariant.
    pub(crate) fn complete_bulk_load_update_child(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
        updated_row_count: u64,
        now: u64,
    ) -> Result<(), RouterError> {
        self.with_bulk_child_transition(
            caller,
            graph_id,
            client_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            Some(now),
            move |child, coordinator| {
                if child.updated_row_count.is_none() {
                    return Err(RouterError::Conflict(
                        "bulk-load update completion targets an insert child row".into(),
                    ));
                }
                if child.progress == BulkLoadChunkProgressV1::Completed {
                    if let BulkLoadLifecycleV1::AbortPending { .. } = coordinator.lifecycle {
                        coordinator.completed_chunk_count = coordinator
                            .completed_chunk_count
                            .checked_add(1)
                            .ok_or_else(|| {
                                RouterError::InvalidArgument(
                                    "bulk-load completed count overflow".into(),
                                )
                            })?;
                        coordinator.next_chunk_index =
                            coordinator.next_chunk_index.checked_add(1).ok_or_else(|| {
                                RouterError::InvalidArgument(
                                    "bulk-load chunk index overflow".into(),
                                )
                            })?;
                        coordinator.lifecycle = BulkLoadLifecycleV1::Aborted;
                    }
                    return Ok(());
                }
                if child.progress != BulkLoadChunkProgressV1::CanonicalPending {
                    return Err(RouterError::Busy {
                        operation: "bulk_load.append".into(),
                    });
                }
                child.progress = BulkLoadChunkProgressV1::Completed;
                child.completed_at_ns = Some(now);
                child.updated_row_count = Some(updated_row_count);
                // The payload and resolved identities are only needed while this child is
                // resumable. A completed receipt is authoritative and intentionally compact.
                child.resolved_update_vertex_ids = None;
                coordinator.committed_chunk_count = coordinator
                    .committed_chunk_count
                    .checked_add(1)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load committed count overflow".into())
                    })?;
                coordinator.completed_chunk_count = coordinator
                    .completed_chunk_count
                    .checked_add(1)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load completed count overflow".into())
                    })?;
                coordinator.next_chunk_index =
                    coordinator.next_chunk_index.checked_add(1).ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load chunk index overflow".into())
                    })?;
                match coordinator.lifecycle {
                    BulkLoadLifecycleV1::AppendPending { .. } => {
                        coordinator.lifecycle = BulkLoadLifecycleV1::Open;
                    }
                    BulkLoadLifecycleV1::AbortPending { .. } => {
                        coordinator.lifecycle = BulkLoadLifecycleV1::Aborted;
                    }
                    _ => {
                        return Err(RouterError::Conflict(
                            "bulk-load child completion has no matching active parent".into(),
                        ));
                    }
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    fn with_bulk_child_transition<F>(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        expected_fingerprint: [u8; 32],
        terminal_at_ns: Option<u64>,
        transition: F,
    ) -> Result<(), RouterError>
    where
        F: FnOnce(
            &mut BulkLoadChunkReceiptRecordV1,
            &mut BulkLoadCoordinatorV1,
        ) -> Result<(), RouterError>,
    {
        let key = client_mutation_key(caller, graph_id, client_key);
        let mut parent = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        if parent.as_v1().mutation_id != parent_mutation_id {
            return Err(RouterError::Conflict(
                "bulk-load parent mutation id mismatch".into(),
            ));
        }
        let mut coordinator = bulk_parent(&parent)?.clone();
        if coordinator.receipt_gc_cursor.is_some() {
            return Err(RouterError::Conflict(
                "bulk-load child transition is closed while receipt GC is active".into(),
            ));
        }
        let receipt_key = receipt_key(parent_mutation_id, chunk_index);
        let mut child = ROUTER_BULK_LOAD_CHUNK_RECEIPTS
            .with_borrow(|map| map.get(&receipt_key))
            .ok_or_else(|| {
                RouterError::Internal("bulk-load child receipt row is missing".into())
            })?;
        if child.chunk_fingerprint != expected_fingerprint {
            return Err(RouterError::Conflict(
                "bulk-load child fingerprint mismatch".into(),
            ));
        }
        transition(&mut child, &mut coordinator)?;
        child.validate().map_err(RouterError::InvalidArgument)?;
        coordinator.validate()?;
        *bulk_parent_mut(&mut parent)? = coordinator;
        if let Some(terminal_at_ns) = terminal_at_ns {
            parent.mark_terminal_at_ns(terminal_at_ns);
        }
        ensure_record_bound(&parent);
        ensure_receipt_bound(&child);
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.insert(receipt_key, child));
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, parent));
        Ok(())
    }

    pub(crate) fn record_bulk_load_canonical_committed(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
        graph_receipt: BulkLoadGraphReceiptV1,
        public_receipt: AtomicInsertReceiptV1,
    ) -> Result<(), RouterError> {
        graph_receipt.validate().map_err(RouterError::Internal)?;
        public_receipt
            .validate()
            .map_err(RouterError::InvalidArgument)?;
        self.with_bulk_child_transition(
            caller,
            graph_id,
            client_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            None,
            move |child, coordinator| {
                match child.progress {
                    BulkLoadChunkProgressV1::CanonicalPending => {}
                    BulkLoadChunkProgressV1::CanonicalCommitted
                    | BulkLoadChunkProgressV1::ProjectionPending
                    | BulkLoadChunkProgressV1::RetirementPending
                    | BulkLoadChunkProgressV1::Completed => return Ok(()),
                }
                if child.graph_receipt.is_some() || child.public_receipt.is_some() {
                    return Err(RouterError::Conflict(
                        "bulk-load canonical receipt conflicts with persisted child evidence"
                            .into(),
                    ));
                }
                child.graph_receipt = Some(graph_receipt);
                child.public_receipt = Some(public_receipt.clone());
                child.progress = BulkLoadChunkProgressV1::CanonicalCommitted;
                coordinator.logical_operation_count = coordinator
                    .logical_operation_count
                    .checked_add(public_receipt.logical_operation_count)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load operation count overflow".into())
                    })?;
                coordinator.logical_vertex_count = coordinator
                    .logical_vertex_count
                    .checked_add(public_receipt.logical_vertex_count)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load vertex count overflow".into())
                    })?;
                coordinator.logical_edge_count = coordinator
                    .logical_edge_count
                    .checked_add(public_receipt.logical_edge_count)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load edge count overflow".into())
                    })?;
                coordinator.committed_chunk_count = coordinator
                    .committed_chunk_count
                    .checked_add(1)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load committed count overflow".into())
                    })?;
                Ok(())
            },
        )
    }

    pub(crate) fn record_bulk_load_projection_pending(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        self.with_bulk_child_transition(
            caller,
            graph_id,
            client_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            None,
            |child, _| {
                if matches!(
                    child.progress,
                    BulkLoadChunkProgressV1::CanonicalCommitted
                        | BulkLoadChunkProgressV1::ProjectionPending
                        | BulkLoadChunkProgressV1::RetirementPending
                        | BulkLoadChunkProgressV1::Completed
                ) {
                    if child.progress == BulkLoadChunkProgressV1::CanonicalCommitted {
                        child.progress = BulkLoadChunkProgressV1::ProjectionPending;
                    }
                    return Ok(());
                }
                Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                })
            },
        )
    }

    pub(crate) fn record_bulk_load_retirement_pending(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        self.with_bulk_child_transition(
            caller,
            graph_id,
            client_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            None,
            |child, _| {
                if matches!(
                    child.progress,
                    BulkLoadChunkProgressV1::ProjectionPending
                        | BulkLoadChunkProgressV1::RetirementPending
                        | BulkLoadChunkProgressV1::Completed
                ) {
                    if child.progress == BulkLoadChunkProgressV1::ProjectionPending {
                        child.progress = BulkLoadChunkProgressV1::RetirementPending;
                    }
                    return Ok(());
                }
                Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                })
            },
        )
    }

    /// Mark a child fully retired and advance the parent's committed prefix.  AbortPending enters
    /// terminal Aborted only in this transition, after exact child quiescence is proven.
    pub(crate) fn complete_bulk_load_child(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        parent_mutation_id: MutationId,
        chunk_index: u32,
        chunk_fingerprint: [u8; 32],
        now: u64,
    ) -> Result<(), RouterError> {
        self.with_bulk_child_transition(
            caller,
            graph_id,
            client_key,
            parent_mutation_id,
            chunk_index,
            chunk_fingerprint,
            Some(now),
            move |child, coordinator| {
                if child.progress == BulkLoadChunkProgressV1::Completed {
                    if let BulkLoadLifecycleV1::AbortPending { .. } = coordinator.lifecycle {
                        coordinator.completed_chunk_count = coordinator
                            .completed_chunk_count
                            .checked_add(1)
                            .ok_or_else(|| {
                                RouterError::InvalidArgument(
                                    "bulk-load completed count overflow".into(),
                                )
                            })?;
                        coordinator.next_chunk_index =
                            coordinator.next_chunk_index.checked_add(1).ok_or_else(|| {
                                RouterError::InvalidArgument(
                                    "bulk-load chunk index overflow".into(),
                                )
                            })?;
                        coordinator.lifecycle = BulkLoadLifecycleV1::Aborted;
                    }
                    return Ok(());
                }
                if child.progress != BulkLoadChunkProgressV1::RetirementPending {
                    return Err(RouterError::Busy {
                        operation: "bulk_load.append".into(),
                    });
                }
                child.progress = BulkLoadChunkProgressV1::Completed;
                child.completed_at_ns = Some(now);
                // Compact the row: a Completed child is never replayed, so drop the resolved
                // Graph request (the largest remaining payload) and its fingerprint. Status,
                // finalize, and GC then only ever decode receipt-sized rows.
                child.graph_request = None;
                child.graph_request_fingerprint = None;
                coordinator.completed_chunk_count = coordinator
                    .completed_chunk_count
                    .checked_add(1)
                    .ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load completed count overflow".into())
                    })?;
                coordinator.next_chunk_index =
                    coordinator.next_chunk_index.checked_add(1).ok_or_else(|| {
                        RouterError::InvalidArgument("bulk-load chunk index overflow".into())
                    })?;
                match coordinator.lifecycle {
                    BulkLoadLifecycleV1::AppendPending { .. } => {
                        coordinator.lifecycle = BulkLoadLifecycleV1::Open;
                    }
                    BulkLoadLifecycleV1::AbortPending { .. } => {
                        coordinator.lifecycle = BulkLoadLifecycleV1::Aborted;
                    }
                    _ => {
                        return Err(RouterError::Conflict(
                            "bulk-load child completion has no matching active parent".into(),
                        ));
                    }
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    pub(crate) fn begin_bulk_load_finalize(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Result<BulkLoadCoordinatorV1, RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let mut record = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        let mut coordinator = bulk_parent(&record)?.clone();
        if coordinator.receipt_gc_cursor.is_some() {
            return Err(RouterError::Conflict(
                "client_mutation_key expired while bulk-load receipt GC is active".into(),
            ));
        }
        match coordinator.lifecycle {
            BulkLoadLifecycleV1::Open => {
                if coordinator.committed_chunk_count != coordinator.completed_chunk_count {
                    return Err(RouterError::Busy {
                        operation: "bulk_load.append".into(),
                    });
                }
                coordinator.lifecycle = BulkLoadLifecycleV1::FinalizePending {
                    stage:
                        crate::facade::stable::label_stats::BulkLoadFinalizeStageV1::VerifyReceipts,
                    cursor: 0,
                };
            }
            BulkLoadLifecycleV1::FinalizePending { .. } => {}
            BulkLoadLifecycleV1::AppendPending { .. } => {
                return Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                });
            }
            BulkLoadLifecycleV1::AbortPending { .. } => {
                return Err(RouterError::Busy {
                    operation: "bulk_load.abort".into(),
                });
            }
            BulkLoadLifecycleV1::Completed => return Ok(coordinator),
            BulkLoadLifecycleV1::Aborted => {
                return Err(RouterError::Conflict("bulk-load job is aborted".into()));
            }
            BulkLoadLifecycleV1::Failed { ref reason } => {
                return Err(RouterError::Conflict(reason.clone()));
            }
        }
        coordinator.validate()?;
        *bulk_parent_mut(&mut record)? = coordinator.clone();
        ensure_record_bound(&record);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
        Ok(coordinator)
    }

    pub(crate) fn begin_bulk_load_abort(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        now: u64,
    ) -> Result<BulkLoadCoordinatorV1, RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let mut record = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        let mut coordinator = bulk_parent(&record)?.clone();
        if coordinator.receipt_gc_cursor.is_some() {
            return Err(RouterError::Conflict(
                "client_mutation_key expired while bulk-load receipt GC is active".into(),
            ));
        }
        match coordinator.lifecycle {
            BulkLoadLifecycleV1::Open => {
                if coordinator.committed_chunk_count != coordinator.completed_chunk_count
                    || coordinator.next_chunk_index != coordinator.completed_chunk_count
                {
                    return Err(RouterError::Busy {
                        operation: "bulk_load.append".into(),
                    });
                }
                coordinator.lifecycle = BulkLoadLifecycleV1::Aborted;
                coordinator.validate()?;
                *bulk_parent_mut(&mut record)? = coordinator.clone();
                record.mark_terminal_at_ns(now);
            }
            BulkLoadLifecycleV1::AppendPending { chunk_index, .. } => {
                coordinator.lifecycle = BulkLoadLifecycleV1::AbortPending {
                    active_chunk: chunk_index,
                };
            }
            BulkLoadLifecycleV1::AbortPending { .. } => {}
            BulkLoadLifecycleV1::FinalizePending { .. } | BulkLoadLifecycleV1::Completed => {
                return Err(RouterError::Conflict(
                    "bulk-load abort is not permitted after finalize/completion".into(),
                ));
            }
            BulkLoadLifecycleV1::Aborted => return Ok(coordinator),
            BulkLoadLifecycleV1::Failed { ref reason } => {
                return Err(RouterError::Conflict(reason.clone()));
            }
        }
        coordinator.validate()?;
        *bulk_parent_mut(&mut record)? = coordinator.clone();
        ensure_record_bound(&record);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
        Ok(coordinator)
    }

    pub(crate) fn finalize_bulk_load_step(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        now: u64,
    ) -> Result<BulkLoadCoordinatorV1, RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let mut record = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|map| map.get(&key))
            .ok_or_else(|| RouterError::NotFound(client_key.to_owned()))?;
        let mut coordinator = bulk_parent(&record)?.clone();
        let (stage, cursor) = match coordinator.lifecycle {
            BulkLoadLifecycleV1::FinalizePending { stage, cursor } => (stage, cursor),
            BulkLoadLifecycleV1::Completed => return Ok(coordinator),
            _ => {
                return Err(RouterError::Conflict(
                    "bulk-load finalize is not pending".into(),
                ));
            }
        };
        let _ = stage;
        let rows = ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
            job_receipt_rows(
                map,
                record.as_v1().mutation_id,
                cursor,
                BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP as usize,
            )
        });
        let mut next_cursor = cursor;
        for (row_key, row) in &rows {
            if row_key.chunk_index != next_cursor {
                return Err(RouterError::Conflict(
                    "bulk-load finalize receipt range is not a contiguous accepted prefix".into(),
                ));
            }
            if row.progress != BulkLoadChunkProgressV1::Completed {
                return Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                });
            }
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                RouterError::Conflict("bulk-load finalize cursor overflow".into())
            })?;
        }
        let has_more = ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
            has_job_receipt_at_or_after(map, record.as_v1().mutation_id, next_cursor)
        });
        if !has_more {
            if coordinator.committed_chunk_count != coordinator.completed_chunk_count
                || coordinator.next_chunk_index != coordinator.completed_chunk_count
                || next_cursor != coordinator.completed_chunk_count
            {
                return Err(RouterError::Conflict(
                    "bulk-load finalize aggregate counters do not match completed receipts".into(),
                ));
            }
            coordinator.lifecycle = BulkLoadLifecycleV1::Completed;
            coordinator.receipt_gc_cursor = None;
            *bulk_parent_mut(&mut record)? = coordinator.clone();
            record.mark_terminal_at_ns(now);
        } else {
            coordinator.lifecycle = BulkLoadLifecycleV1::FinalizePending {
                stage: crate::facade::stable::label_stats::BulkLoadFinalizeStageV1::VerifyReceipts,
                cursor: next_cursor,
            };
            *bulk_parent_mut(&mut record)? = coordinator.clone();
        }
        coordinator.validate()?;
        ensure_record_bound(&record);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
        Ok(coordinator)
    }

    pub(crate) fn bulk_load_receipt_gc_step(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        now: u64,
    ) -> Result<BulkLoadGcStepResult, RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let Some(mut record) = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|map| map.get(&key))
        else {
            return Ok(BulkLoadGcStepResult {
                scanned: 0,
                removed: 0,
                done: true,
            });
        };
        let job_id = record.as_v1().mutation_id;
        let mut coordinator = bulk_parent(&record)?.clone();
        if !expired_terminal(&record, now) {
            return Ok(BulkLoadGcStepResult {
                scanned: 0,
                removed: 0,
                done: false,
            });
        }
        if coordinator.committed_chunk_count != coordinator.completed_chunk_count
            || coordinator.next_chunk_index != coordinator.completed_chunk_count
        {
            return Err(RouterError::Conflict(
                "bulk-load receipt GC requires a quiescent completed prefix".into(),
            ));
        }
        let cursor = coordinator.receipt_gc_cursor.unwrap_or(0);
        let rows = ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow(|map| {
            job_receipt_rows(
                map,
                job_id,
                cursor,
                BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP as usize,
            )
        });
        let mut next_cursor = cursor;
        for (row_key, row) in &rows {
            if row_key.chunk_index != next_cursor {
                return Err(RouterError::Conflict(
                    "bulk-load receipt GC range is not a contiguous accepted prefix".into(),
                ));
            }
            if row.progress != BulkLoadChunkProgressV1::Completed {
                return Err(RouterError::Busy {
                    operation: "bulk_load.append".into(),
                });
            }
            next_cursor = next_cursor.checked_add(1).ok_or_else(|| {
                RouterError::Conflict("bulk-load receipt GC cursor overflow".into())
            })?;
        }
        if rows.is_empty() {
            let has_any = ROUTER_BULK_LOAD_CHUNK_RECEIPTS
                .with_borrow(|map| has_job_receipt_at_or_after(map, job_id, 0));
            if !has_any {
                // Parent removal is allowed only after an empty child range and a validated
                // terminal lifecycle; status cannot observe a GC-shaped replacement state.
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.remove(&key));
                return Ok(BulkLoadGcStepResult {
                    scanned: 0,
                    removed: 0,
                    done: true,
                });
            }
            // A durable cursor that points past a surviving row is an interrupted or otherwise
            // stale scan. Rewind it rather than removing the parent while child evidence remains.
            coordinator.receipt_gc_cursor = Some(0);
            coordinator.validate()?;
            *bulk_parent_mut(&mut record)? = coordinator;
            ensure_record_bound(&record);
            ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
            return Ok(BulkLoadGcStepResult {
                scanned: 0,
                removed: 0,
                done: false,
            });
        }
        let removed = rows.len() as u32;
        coordinator.receipt_gc_cursor = Some(next_cursor);
        coordinator.validate()?;
        *bulk_parent_mut(&mut record)? = coordinator;
        ensure_record_bound(&record);
        ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| {
            for (row_key, _) in &rows {
                map.remove(row_key);
            }
        });
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.insert(key, record));
        Ok(BulkLoadGcStepResult {
            scanned: removed,
            removed,
            done: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::bulk_load::{
        BulkLoadChunkEnvelopeV1, BulkLoadGraphReceiptV1, BulkLoadGraphRequestV1,
    };
    use crate::facade::stable::label_stats::BulkLoadTargetV1;
    use crate::facade::store::tests::test_init_args;
    use crate::types::{AtomicInsertReceiptV1, BulkLoadChunkV1};
    use candid::Principal;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::{
        GraphOrderedVertexBatchReceiptV1, OrderedVertexBatchGraphItemV1,
        OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedPropertyTable,
    };

    fn fixture_target() -> BulkLoadTargetV1 {
        BulkLoadTargetV1 {
            shard_id: ShardId::new(0),
            graph_canister: Principal::self_authenticating([11; 32]),
        }
    }

    fn fixture_child() -> (
        BulkLoadChunkReceiptRecordV1,
        BulkLoadGraphReceiptV1,
        AtomicInsertReceiptV1,
    ) {
        let target = fixture_target();
        let request = BulkLoadGraphRequestV1::Vertex(OrderedVertexBatchGraphRequestV1 {
            graph_id: GraphId::from_raw(1),
            target_shard_id: target.shard_id,
            target_graph_canister: target.graph_canister,
            resolved_labels: ResolvedLabelTable::default(),
            resolved_properties: ResolvedPropertyTable::default(),
            items: vec![OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: Vec::new(),
                resolved_initial_properties: Vec::new(),
            }],
        });
        let graph_request_fingerprint = request.fingerprint().unwrap();
        let chunk = BulkLoadChunkV1::Vertices(vec![crate::types::AtomicInsertVertexV1 {
            vertex_labels: Vec::new(),
            initial_properties: Vec::new(),
        }]);
        let chunk_fingerprint = BulkLoadChunkEnvelopeV1::from_chunk(&chunk)
            .fingerprint()
            .unwrap();
        let graph_receipt = BulkLoadGraphReceiptV1::Vertex(GraphOrderedVertexBatchReceiptV1 {
            logical_vertex_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
            allocated_vertex_ids: vec![1],
        });
        let public_receipt = AtomicInsertReceiptV1 {
            logical_operation_count: 1,
            logical_vertex_count: 1,
            logical_edge_count: 0,
            allocated_vertex_ids: vec![
                vec![0; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES],
            ],
        };
        let row = BulkLoadChunkReceiptRecordV1 {
            chunk_fingerprint,
            graph_request: Some(request),
            graph_request_fingerprint: Some(graph_request_fingerprint),
            child_mutation_id: 1,
            progress: BulkLoadChunkProgressV1::CanonicalPending,
            public_receipt: None,
            graph_receipt: None,
            resolved_update_vertex_ids: None,
            completed_at_ns: None,
            updated_row_count: None,
        };
        (row, graph_receipt, public_receipt)
    }

    /// T1: admission grants dispatch permission only while the job is still appending this exact
    /// chunk. A completed child is a receipt replay; an aborting or terminal job refuses, so a
    /// caller suspended before admission cannot start new row writes.
    #[test]
    fn update_admission_grants_permission_only_while_the_job_accepts_this_chunk() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([61; 32]);
        let graph_id = GraphId::from_raw(1);
        let parent_id = match store
            .start_bulk_load_job(caller, graph_id, "gate-job", fixture_target(), 1)
            .expect("start update job")
        {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            BulkLoadStartAdmission::Replay { .. } => panic!("first Start must create"),
        };
        let fingerprint = [14u8; 32];
        let admit = || {
            store.admit_bulk_load_update_child(
                caller,
                graph_id,
                "gate-job",
                parent_id,
                0,
                fingerprint,
                Some(vec![vec![1u8; 8], vec![2u8; 8]]),
            )
        };
        let gate = || {
            store
                .bulk_load_update_dispatch_gate(caller, graph_id, "gate-job", parent_id, 0)
                .expect("read dispatch gate")
        };

        // Fresh admission grants, and the gate is open.
        assert_eq!(admit(), Ok(BulkLoadUpdateAdmission::Granted));
        assert_eq!(gate(), BulkLoadUpdateDispatchGate::Open);
        // A retry of the same in-flight chunk is still a grant (resume).
        assert_eq!(admit(), Ok(BulkLoadUpdateAdmission::Granted));
        assert_eq!(gate(), BulkLoadUpdateDispatchGate::Open);

        // While aborting, a retry may only settle already-dispatched rows: the store grants the
        // attempt, and the per-row gate is what refuses to start a never-dispatched row.
        store
            .begin_bulk_load_abort(caller, graph_id, "gate-job", 4)
            .expect("abort with a pending child");
        assert_eq!(gate(), BulkLoadUpdateDispatchGate::SettleOnly);
        assert_eq!(
            admit(),
            Ok(BulkLoadUpdateAdmission::Granted),
            "a settle-only attempt is granted at admission"
        );
        assert!(matches!(
            store.admit_bulk_load_update_child(
                caller,
                graph_id,
                "gate-job",
                parent_id,
                0,
                [99u8; 32],
                Some(vec![vec![1u8; 8], vec![2u8; 8]]),
            ),
            Err(RouterError::Conflict(_))
        ));

        // Completing the chunk under the pending abort terminalizes the job; a stale retry with
        // the identical fingerprint now gets a receipt replay, never permission.
        store
            .complete_bulk_load_update_child(
                caller,
                graph_id,
                "gate-job",
                parent_id,
                0,
                fingerprint,
                2,
                5,
            )
            .expect("complete the chunk");
        assert_eq!(gate(), BulkLoadUpdateDispatchGate::Closed);
        assert_eq!(
            admit(),
            Ok(BulkLoadUpdateAdmission::AlreadyCompleted),
            "a finished chunk replays its receipt and grants nothing"
        );
    }

    #[test]
    fn bulk_load_update_admission_rejects_concurrent_suffix_retargeting() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([62; 32]);
        let graph_id = GraphId::from_raw(1);
        let client_key = "target-race";
        let parent_key = client_mutation_key(caller, graph_id, client_key);
        let parent_id = match store
            .start_bulk_load_job(caller, graph_id, client_key, fixture_target(), 1)
            .unwrap()
        {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            BulkLoadStartAdmission::Replay { .. } => panic!("first Start must create"),
        };
        let fingerprint = [15u8; 32];
        let admit = |ids| {
            store.admit_bulk_load_update_child(
                caller,
                graph_id,
                client_key,
                parent_id,
                0,
                fingerprint,
                ids,
            )
        };
        // Two first-Append contenders saw no child before resolution yielded. They resolved
        // the same authored payload differently only at the still-unstarted suffix ordinal.
        assert!(store.bulk_load_chunk_receipt(parent_id, 0).is_none());
        let winner = vec![vec![1u8; 8], vec![2u8; 8]];
        let contender = vec![winner[0].clone(), vec![3u8; 8]];
        assert_eq!(
            admit(Some(winner.clone())),
            Ok(BulkLoadUpdateAdmission::Granted)
        );
        // Model the winning prefix at this store boundary. Only row zero has a reservation;
        // no target-bound row fingerprint exists to protect row one from the contender.
        store
            .reserve_mutation_id_for_client_key(caller, graph_id, "target-race:0:u0", vec![4; 32])
            .unwrap();
        store
            .record_bulk_load_update_progress(
                caller,
                graph_id,
                client_key,
                parent_id,
                0,
                fingerprint,
                1,
            )
            .unwrap();
        let prefix_key = client_mutation_key(caller, graph_id, "target-race:0:u0");
        let suffix_key = client_mutation_key(caller, graph_id, "target-race:0:u1");
        assert!(store.router_mutation_record(&prefix_key).is_some());
        assert!(store.router_mutation_record(&suffix_key).is_none());

        for aborting in [false, true] {
            if aborting {
                store
                    .begin_bulk_load_abort(caller, graph_id, client_key, 4)
                    .unwrap();
            }
            let parent_before = store.router_mutation_record(&parent_key);
            let child_before = store.bulk_load_chunk_receipt(parent_id, 0);
            let prefix_before = store.router_mutation_record(&prefix_key);
            let counter_before = ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get());
            assert_eq!(child_before.as_ref().unwrap().updated_row_count, Some(1));
            assert_eq!(
                child_before.as_ref().unwrap().resolved_update_vertex_ids,
                Some(winner.clone())
            );
            for ids in [
                Some(contender.clone()),
                Some(vec![winner[1].clone(), winner[0].clone()]),
                Some(vec![winner[0].clone()]),
                None,
            ] {
                assert_eq!(
                    admit(ids),
                    Err(RouterError::Conflict(
                        "bulk-load update targets differ from the admitted chunk".into()
                    )),
                    "stale resolution must not grant dispatch or settlement permission"
                );
                assert_eq!(store.router_mutation_record(&parent_key), parent_before);
                assert_eq!(store.bulk_load_chunk_receipt(parent_id, 0), child_before);
                assert_eq!(store.router_mutation_record(&prefix_key), prefix_before);
                assert!(store.router_mutation_record(&suffix_key).is_none());
                assert_eq!(
                    ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get()),
                    counter_before
                );
            }
            assert_eq!(
                admit(Some(winner.clone())),
                Ok(BulkLoadUpdateAdmission::Granted)
            );
            assert_eq!(
                store.bulk_load_update_dispatch_gate(caller, graph_id, client_key, parent_id, 0),
                Ok(if aborting {
                    BulkLoadUpdateDispatchGate::SettleOnly
                } else {
                    BulkLoadUpdateDispatchGate::Open
                })
            );
        }
        store
            .complete_bulk_load_update_child(
                caller,
                graph_id,
                client_key,
                parent_id,
                0,
                fingerprint,
                1,
                5,
            )
            .unwrap();
        let completed = store.bulk_load_chunk_receipt(parent_id, 0).unwrap();
        assert_eq!(completed.resolved_update_vertex_ids, None);
        assert_eq!(completed.updated_row_count, Some(1));
        // Compacted target IDs are not needed for receipt replay, which grants no dispatch.
        assert_eq!(
            admit(Some(contender)),
            Ok(BulkLoadUpdateAdmission::AlreadyCompleted)
        );
        assert_eq!(admit(None), Ok(BulkLoadUpdateAdmission::AlreadyCompleted));
        assert_eq!(
            store.bulk_load_update_dispatch_gate(caller, graph_id, client_key, parent_id, 0),
            Ok(BulkLoadUpdateDispatchGate::Closed)
        );
        assert!(store.router_mutation_record(&suffix_key).is_none());
    }

    #[test]
    fn bulk_load_update_row_gc_keeps_pending_outcomes_and_unpins_completed_chunks() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let admin = Principal::self_authenticating([63; 32]);
        crate::facade::auth::grant_admins(&[admin]);
        let graph = GraphId::from_raw(1);
        let job = "row-retention";
        let parent = match store
            .start_bulk_load_job(admin, graph, job, fixture_target(), 0)
            .unwrap()
        {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            _ => panic!("new job"),
        };
        let chunk = BulkLoadChunkReceiptKey::new(parent, 0);
        store
            .admit_bulk_load_update_child(
                admin,
                graph,
                job,
                parent,
                0,
                [1; 32],
                Some(vec![vec![1; 8], vec![2; 8]]),
            )
            .unwrap();
        let now = CLIENT_MUTATION_KEY_TTL_NS * 2;
        let mut rows = Vec::new();
        for (ordinal, count) in [(0, 1), (1, 0)] {
            let key = client_mutation_key(admin, graph, &format!("{job}:0:u{ordinal}"));
            let mut record = RouterMutationRecord::new(100 + ordinal, 0, vec![count as u8]);
            record.as_v1_mut().request_identity = RouterMutationRequestIdentityV1::PlanExecution {
                request_fingerprint: vec![count as u8],
                bulk_load_chunk: Some(chunk),
            };
            record.as_v1_mut().routing_in_progress = false;
            record.as_v1_mut().completed_row_count = Some(count);
            record.mark_terminal_at_ns(0);
            super::super::idempotency::compact_completed_record(&mut record);
            // Exercise the persisted identity, not a live heap-only ownership marker.
            let record = RouterMutationRecord::from_bytes(record.to_bytes());
            assert_eq!(
                record.as_v1().request_identity.bulk_load_chunk(),
                Some(chunk)
            );
            ROUTER_MUTATION_BY_CLIENT_KEY
                .with_borrow_mut(|map| map.insert(key.clone(), record.clone()));
            rows.push((key, record));
        }
        store
            .record_bulk_load_update_progress(admin, graph, job, parent, 0, [1; 32], 1)
            .unwrap();
        let ordinary_key = client_mutation_key(admin, graph, "ordinary-expired");
        let mut ordinary = RouterMutationRecord::new(200, 0, vec![3]);
        ordinary.as_v1_mut().routing_in_progress = false;
        ordinary.as_v1_mut().completed_row_count = Some(0);
        ordinary.mark_terminal_at_ns(0);
        ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow_mut(|map| map.insert(ordinary_key.clone(), ordinary));
        for aborting in [false, true] {
            if aborting {
                store.begin_bulk_load_abort(admin, graph, job, now).unwrap();
            }
            store
                .admin_sweep_expired_client_mutation_keys_at(admin, None, 1000, now)
                .unwrap();
            assert!(
                store.router_mutation_record(&ordinary_key).is_none(),
                "the GC must actually run"
            );
            for (key, expected) in &rows {
                assert_eq!(
                    store.router_mutation_record(key).as_ref(),
                    Some(expected),
                    "pending bulk work must retain its exact zero/positive receipt beyond row TTL"
                );
                let reservation = store
                    .reserve_plan_mutation_at(
                        admin,
                        graph,
                        &key.client_key,
                        expected.as_v1().request_identity.clone(),
                        now,
                    )
                    .expect("pending bulk rows remain replayable beyond ordinary TTL");
                assert_eq!(reservation.mutation_id, expected.as_v1().mutation_id);
                assert!(!reservation.routing_owner);
                for owner in [None, Some(BulkLoadChunkReceiptKey::new(parent, 99))] {
                    assert_eq!(
                        store.reserve_plan_mutation_at(
                            admin,
                            graph,
                            &key.client_key,
                            RouterMutationRequestIdentityV1::PlanExecution {
                                request_fingerprint: expected
                                    .as_v1()
                                    .request_identity
                                    .request_fingerprint()
                                    .to_vec(),
                                bulk_load_chunk: owner,
                            },
                            now,
                        ),
                        Err(RouterError::Conflict(
                            "client_mutation_key was already used for a different request".into()
                        ))
                    );
                }
                assert_eq!(store.router_mutation_record(key).as_ref(), Some(expected));
            }
        }
        store
            .complete_bulk_load_update_child(admin, graph, job, parent, 0, [1; 32], 1, now)
            .unwrap();
        for (key, expected) in &rows {
            assert_eq!(
                store.reserve_plan_mutation_at(
                    admin,
                    graph,
                    &key.client_key,
                    expected.as_v1().request_identity.clone(),
                    now
                ),
                Err(RouterError::InvalidArgument(
                    "client_mutation_key expired; use a new key for a new mutation".into()
                ))
            );
        }
        let counter = ROUTER_MUTATION_COUNTER.with_borrow(|value| *value.get());
        assert_eq!(
            store.reserve_bulk_load_update_row(
                admin,
                graph,
                "new-after-completion",
                vec![9],
                chunk
            ),
            Err(RouterError::Conflict(
                "bulk-load update row requires a pending chunk".into()
            ))
        );
        assert_eq!(
            ROUTER_MUTATION_COUNTER.with_borrow(|value| *value.get()),
            counter
        );
        assert!(
            store
                .router_mutation_record(&client_mutation_key(admin, graph, "new-after-completion"))
                .is_none()
        );
        store
            .admin_sweep_expired_client_mutation_keys_at(admin, None, 1000, now)
            .unwrap();
        for (key, _) in rows {
            assert!(store.router_mutation_record(&key).is_none());
        }
        assert!(
            store
                .bulk_load_chunk_receipt(parent, 0)
                .unwrap()
                .progress
                .is_completed()
        );
    }

    #[test]
    fn bulk_load_update_progress_is_monotonic_bounded_and_terminal_safe() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([51; 32]);
        let graph_id = GraphId::from_raw(1);
        let parent_id = match store
            .start_bulk_load_job(caller, graph_id, "progress-job", fixture_target(), 1)
            .expect("start update job")
        {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            BulkLoadStartAdmission::Replay { .. } => panic!("first Start must create"),
        };
        let fingerprint = [13u8; 32];
        let progress = |count: u64| {
            store.record_bulk_load_update_progress(
                caller,
                graph_id,
                "progress-job",
                parent_id,
                0,
                fingerprint,
                count,
            )
        };
        let recorded = || {
            store
                .bulk_load_chunk_receipt(parent_id, 0)
                .expect("update row")
                .updated_row_count
                .expect("recorded prefix")
        };
        // A prefix for a chunk that was never admitted is refused, not written.
        let unadmitted = progress(1).expect_err("progress before admission must be refused");
        assert!(
            matches!(
                unadmitted,
                RouterError::Busy { .. } | RouterError::Internal(_) | RouterError::NotFound(_)
            ),
            "unexpected pre-admission error: {unadmitted:?}"
        );
        assert!(
            store.bulk_load_chunk_receipt(parent_id, 0).is_none(),
            "a refused progress notification must not create a child row"
        );
        store
            .admit_bulk_load_update_child(
                caller,
                graph_id,
                "progress-job",
                parent_id,
                0,
                fingerprint,
                Some(vec![vec![1u8; 8], vec![2u8; 8]]),
            )
            .expect("admit update child");
        assert_eq!(recorded(), 0);
        // 0 -> 1 -> 2 is a forward prefix.
        progress(1).expect("first row");
        assert_eq!(recorded(), 1);
        progress(2).expect("second row");
        assert_eq!(recorded(), 2);
        // A delayed 2 -> 1 notification must not rewind the durable prefix.
        progress(1).expect("duplicate/stale notification is a no-op");
        assert_eq!(
            recorded(),
            2,
            "the durable prefix must never move backwards"
        );
        progress(2).expect("same-prefix replay is idempotent");
        assert_eq!(recorded(), 2);
        // Exceeding the admitted row count is rejected and leaves the prefix unchanged.
        let over = progress(3).expect_err("a prefix beyond the admitted rows must reject");
        assert!(
            matches!(over, RouterError::Conflict(ref message) if message.contains("admitted rows")),
            "unexpected bound error: {over:?}"
        );
        assert_eq!(recorded(), 2, "a rejected bound must not change the prefix");
        // Terminal boundary: the row completes with the admitted count, and later notifications
        // can neither rewind nor extend it.
        store
            .complete_bulk_load_update_child(
                caller,
                graph_id,
                "progress-job",
                parent_id,
                0,
                fingerprint,
                2,
                9,
            )
            .expect("complete the chunk");
        assert_eq!(recorded(), 2);
        progress(1).expect("a late smaller notification is a no-op on a completed row");
        assert_eq!(recorded(), 2);
        progress(2).expect("a late equal notification is idempotent on a completed row");
        assert_eq!(recorded(), 2);
        let late_larger =
            progress(3).expect_err("a late larger notification contradicts the receipt");
        assert!(
            matches!(late_larger, RouterError::Conflict(_)),
            "unexpected completed-row error: {late_larger:?}"
        );
        assert_eq!(recorded(), 2);
    }

    #[test]
    fn bulk_load_update_abort_pending_terminalizes_only_after_the_chunk_completes() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([41; 32]);
        let graph_id = GraphId::from_raw(1);
        let admission = store
            .start_bulk_load_job(caller, graph_id, "prefix-job", fixture_target(), 1)
            .expect("start update job");
        let parent_id = match admission {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            BulkLoadStartAdmission::Replay { .. } => panic!("first Start must create"),
        };
        let fingerprint = [12u8; 32];
        store
            .admit_bulk_load_update_child(
                caller,
                graph_id,
                "prefix-job",
                parent_id,
                0,
                fingerprint,
                Some(vec![vec![1u8; 8], vec![2u8; 8]]),
            )
            .expect("admit update child");
        // Row one committed: the prefix is durable while the child stays pending.
        store
            .record_bulk_load_update_progress(
                caller,
                graph_id,
                "prefix-job",
                parent_id,
                0,
                fingerprint,
                1,
            )
            .expect("record committed prefix");
        let pending = store
            .bulk_load_chunk_receipt(parent_id, 0)
            .expect("pending update row");
        assert_eq!(pending.progress, BulkLoadChunkProgressV1::CanonicalPending);
        assert_eq!(pending.updated_row_count, Some(1));
        assert_eq!(
            pending.resolved_update_vertex_ids,
            Some(vec![vec![1u8; 8], vec![2u8; 8]]),
            "the retry target-identity guard must survive the prefix record"
        );
        // Finalize cannot proceed with a pending child, and Abort must not terminalize by
        // claiming the recorded prefix: a row of the still-running append can commit after the
        // abort ingress, which would leave a write on a terminal job that no receipt counts.
        assert!(matches!(
            store.begin_bulk_load_finalize(caller, graph_id, "prefix-job"),
            Err(RouterError::Busy { .. })
        ));
        let aborting = store
            .begin_bulk_load_abort(caller, graph_id, "prefix-job", 4)
            .expect("abort with a committed-prefix child");
        assert!(matches!(
            aborting.lifecycle,
            BulkLoadLifecycleV1::AbortPending { active_chunk: 0 }
        ));
        let still_pending = store
            .bulk_load_chunk_receipt(parent_id, 0)
            .expect("pending update row during abort");
        assert_eq!(
            still_pending.progress,
            BulkLoadChunkProgressV1::CanonicalPending,
            "abort must leave the in-flight update chunk open for its own rows to land"
        );
        // A further row of the running append still records its result while AbortPending.
        store
            .record_bulk_load_update_progress(
                caller,
                graph_id,
                "prefix-job",
                parent_id,
                0,
                fingerprint,
                2,
            )
            .expect("a row result must never be dropped by a pending abort");
        let recorded = store
            .bulk_load_chunk_receipt(parent_id, 0)
            .expect("update row after prefix record");
        assert_eq!(
            recorded.updated_row_count,
            Some(2),
            "the committed prefix must only move forward"
        );
        // Only the chunk's own completion terminalizes the abort, and it uses the true prefix.
        store
            .complete_bulk_load_update_child(
                caller,
                graph_id,
                "prefix-job",
                parent_id,
                0,
                fingerprint,
                recorded.updated_row_count.expect("committed prefix"),
                5,
            )
            .expect("complete the chunk that the pending abort was waiting for");
        let closed = store
            .router_mutation_record(&crate::facade::store::idempotency::client_mutation_key(
                caller,
                graph_id,
                "prefix-job",
            ))
            .expect("parent after prefix abort");
        let coordinator = bulk_parent(&closed).expect("coordinator");
        assert!(
            matches!(coordinator.lifecycle, BulkLoadLifecycleV1::Aborted),
            "abort must complete at the committed prefix, got {:?}",
            coordinator.lifecycle
        );
        let completed = store
            .bulk_load_chunk_receipt(parent_id, 0)
            .expect("completed row");
        assert_eq!(completed.updated_row_count, Some(2));
        assert!(
            completed.resolved_update_vertex_ids.is_none(),
            "completion must compact the retry guard"
        );
    }

    #[test]
    fn bulk_load_update_child_admit_complete_and_replay() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([31; 32]);
        let graph_id = GraphId::from_raw(1);
        let admission = store
            .start_bulk_load_job(caller, graph_id, "update-job", fixture_target(), 1)
            .expect("start update job");
        let parent_id = match admission {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            BulkLoadStartAdmission::Replay { .. } => panic!("first Start must create"),
        };
        let fingerprint = [9u8; 32];
        store
            .admit_bulk_load_update_child(
                caller,
                graph_id,
                "update-job",
                parent_id,
                0,
                fingerprint,
                Some(Vec::new()),
            )
            .expect("admit update child");
        // Exact replay of the same fingerprint replays without allocating a second child.
        store
            .admit_bulk_load_update_child(
                caller,
                graph_id,
                "update-job",
                parent_id,
                0,
                fingerprint,
                Some(Vec::new()),
            )
            .expect("replay update child");
        assert!(
            store.bulk_load_chunk_receipt(parent_id, 1).is_none(),
            "replay must not allocate a second row"
        );
        // A different fingerprint on the same chunk is a conflict, never a second row.
        let conflict = store.admit_bulk_load_update_child(
            caller,
            graph_id,
            "update-job",
            parent_id,
            0,
            [10u8; 32],
            Some(Vec::new()),
        );
        assert!(
            matches!(conflict, Err(RouterError::Conflict(_))),
            "divergent update chunk must conflict, got {conflict:?}"
        );
        // A completed row replays its stored receipt and rejects neither re-resolution nor a
        // second completion with a different count.
        store
            .complete_bulk_load_update_child(
                caller,
                graph_id,
                "update-job",
                parent_id,
                0,
                fingerprint,
                3,
                2,
            )
            .expect("complete update child");
        let row = store
            .bulk_load_chunk_receipt(parent_id, 0)
            .expect("update receipt row");
        assert_eq!(row.progress, BulkLoadChunkProgressV1::Completed);
        assert_eq!(row.updated_row_count, Some(3));
        assert!(
            row.graph_request.is_none(),
            "updates persist no Graph request"
        );
        // Completing the terminal row again is a no-op (replay-safe Finalize path).
        store
            .complete_bulk_load_update_child(
                caller,
                graph_id,
                "update-job",
                parent_id,
                0,
                fingerprint,
                3,
                3,
            )
            .expect("replayed complete");
        let parent = store
            .router_mutation_record(&crate::facade::store::idempotency::client_mutation_key(
                caller,
                graph_id,
                "update-job",
            ))
            .expect("parent record");
        let coordinator = match parent.payload() {
            RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) => coordinator.clone(),
            other => panic!("bulk parent payload, got {other:?}"),
        };
        assert_eq!(coordinator.committed_chunk_count, 1);
        assert_eq!(coordinator.completed_chunk_count, 1);
        assert_eq!(coordinator.next_chunk_index, 1);
    }

    #[test]
    fn bulk_load_start_exact_retry_does_not_allocate_or_repin() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([21; 32]);
        let target = fixture_target();
        let first = store
            .start_bulk_load_job(caller, GraphId::from_raw(1), "job", target.clone(), 1)
            .unwrap();
        let mutation_id = match first {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            BulkLoadStartAdmission::Replay { .. } => panic!("first Start must create"),
        };
        let second = store
            .start_bulk_load_job(
                caller,
                GraphId::from_raw(1),
                "job",
                BulkLoadTargetV1 {
                    shard_id: ShardId::new(9),
                    graph_canister: Principal::self_authenticating([22; 32]),
                },
                2,
            )
            .unwrap();
        let replay = match second {
            BulkLoadStartAdmission::Replay { record } => *record,
            BulkLoadStartAdmission::Created { .. } => panic!("exact Start retry allocated again"),
        };
        assert_eq!(replay.as_v1().mutation_id, mutation_id);
        let RouterMutationPayloadV1::BulkLoadCoordinator(coordinator) = replay.payload() else {
            panic!("missing bulk coordinator")
        };
        assert_eq!(coordinator.target, target);
        assert_eq!(
            store
                .router_mutation_record(&client_mutation_key(caller, GraphId::from_raw(1), "job",))
                .unwrap()
                .as_v1()
                .mutation_id,
            mutation_id
        );
    }

    #[test]
    fn bulk_load_start_preflight_failure_leaves_counter_and_map_unchanged() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([25; 32]);
        let error = store.start_bulk_load_job(
            caller,
            GraphId::from_raw(1),
            "invalid",
            BulkLoadTargetV1 {
                shard_id: ShardId::new(0),
                graph_canister: Principal::anonymous(),
            },
            1,
        );
        assert!(matches!(error, Err(RouterError::InvalidArgument(_))));
        assert!(
            store
                .router_mutation_record(&client_mutation_key(
                    caller,
                    GraphId::from_raw(1),
                    "invalid",
                ))
                .is_none()
        );
        let next_id = ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get());
        assert_eq!(next_id, 0);
    }

    #[cfg(feature = "pocket-ic-e2e")]
    #[test]
    fn bulk_load_start_fault_hooks_cover_both_durable_write_boundaries() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([26; 32]);
        let graph_id = GraphId::from_raw(1);

        crate::test_fault::arm(crate::test_fault::InjectedFault::TrapAfterBulkStartCounter);
        let counter_trap = catch_unwind(AssertUnwindSafe(|| {
            let _ = store.start_bulk_load_job(
                caller,
                graph_id,
                "counter-boundary",
                fixture_target(),
                1,
            );
        }));
        crate::test_fault::arm(crate::test_fault::InjectedFault::None);
        assert!(counter_trap.is_err());
        assert_eq!(
            ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get()),
            1,
            "host tests expose the exact post-counter trap boundary; PocketIC proves rollback"
        );
        assert!(
            store
                .router_mutation_record(&client_mutation_key(caller, graph_id, "counter-boundary",))
                .is_none()
        );

        store.init_from_args(&test_init_args());
        crate::test_fault::arm(crate::test_fault::InjectedFault::TrapAfterBulkStartParent);
        let parent_trap = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                store.start_bulk_load_job(caller, graph_id, "parent-boundary", fixture_target(), 1);
        }));
        crate::test_fault::arm(crate::test_fault::InjectedFault::None);
        assert!(parent_trap.is_err());
        assert_eq!(
            ROUTER_MUTATION_COUNTER.with_borrow(|counter| *counter.get()),
            1
        );
        assert!(
            store
                .router_mutation_record(&client_mutation_key(caller, graph_id, "parent-boundary",))
                .is_some(),
            "host tests expose the exact post-parent trap boundary; PocketIC proves rollback"
        );
    }

    #[test]
    fn bulk_load_child_lifecycle_preserves_prefix_and_finalize_requires_completion() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([31; 32]);
        let graph_id = GraphId::from_raw(1);
        let key = "job";
        let target = fixture_target();
        let start = store
            .start_bulk_load_job(caller, graph_id, key, target, 1)
            .unwrap();
        let parent_id = match start {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            _ => panic!("expected creation"),
        };
        let (child, graph_receipt, public_receipt) = fixture_child();
        let chunk_fingerprint = child.chunk_fingerprint;
        let child_id = store
            .admit_bulk_load_child(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                child.chunk_fingerprint,
                child,
            )
            .unwrap();
        assert_ne!(child_id, parent_id);
        let pending = store
            .bulk_load_chunk_receipt(parent_id, 0)
            .expect("child row");
        assert_eq!(pending.progress, BulkLoadChunkProgressV1::CanonicalPending);
        let replay_child_id = store
            .admit_bulk_load_child(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
                pending.clone(),
            )
            .unwrap();
        assert_eq!(replay_child_id, child_id);
        assert!(matches!(
            store.admit_bulk_load_child(caller, graph_id, key, parent_id, 0, [14; 32], pending,),
            Err(RouterError::Conflict(_))
        ));
        store
            .record_bulk_load_canonical_committed(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
                graph_receipt,
                public_receipt,
            )
            .unwrap();
        store
            .record_bulk_load_projection_pending(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
            )
            .unwrap();
        store
            .record_bulk_load_retirement_pending(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
            )
            .unwrap();
        store
            .complete_bulk_load_child(caller, graph_id, key, parent_id, 0, chunk_fingerprint, 5)
            .unwrap();
        let coordinator = store
            .begin_bulk_load_finalize(caller, graph_id, key)
            .unwrap();
        assert!(matches!(
            coordinator.lifecycle,
            BulkLoadLifecycleV1::FinalizePending { .. }
        ));
        let completed = store
            .finalize_bulk_load_step(caller, graph_id, key, 6)
            .unwrap();
        assert_eq!(completed.lifecycle, BulkLoadLifecycleV1::Completed);
        assert_eq!(
            store
                .bulk_load_chunk_receipt(parent_id, 0)
                .unwrap()
                .progress,
            BulkLoadChunkProgressV1::Completed
        );
        // Completed rows are compacted: the Graph request and its fingerprint are dropped.
        let completed_row = store.bulk_load_chunk_receipt(parent_id, 0).unwrap();
        assert!(completed_row.graph_request.is_none());
        assert!(completed_row.graph_request_fingerprint.is_none());
    }

    #[test]
    fn bulk_load_status_rejects_page_overflow_before_iteration() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        assert!(matches!(
            store.list_bulk_load_chunk_receipts(1, 0, 65),
            Err(RouterError::InvalidArgument(_))
        ));
    }

    #[test]
    fn bulk_load_receipt_gc_deletes_bound_and_parent_last() {
        let store = RouterStore::new();
        store.init_from_args(&test_init_args());
        let caller = Principal::self_authenticating([51; 32]);
        let graph_id = GraphId::from_raw(1);
        let key = "gc-job";
        let start = store
            .start_bulk_load_job(caller, graph_id, key, fixture_target(), 1)
            .unwrap();
        let parent_id = match start {
            BulkLoadStartAdmission::Created { mutation_id } => mutation_id,
            _ => panic!("expected creation"),
        };
        let (child, graph_receipt, public_receipt) = fixture_child();
        let chunk_fingerprint = child.chunk_fingerprint;
        store
            .admit_bulk_load_child(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                child.chunk_fingerprint,
                child,
            )
            .unwrap();
        store
            .record_bulk_load_canonical_committed(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
                graph_receipt,
                public_receipt,
            )
            .unwrap();
        store
            .record_bulk_load_projection_pending(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
            )
            .unwrap();
        store
            .record_bulk_load_retirement_pending(
                caller,
                graph_id,
                key,
                parent_id,
                0,
                chunk_fingerprint,
            )
            .unwrap();
        store
            .complete_bulk_load_child(caller, graph_id, key, parent_id, 0, chunk_fingerprint, 5)
            .unwrap();
        store
            .begin_bulk_load_finalize(caller, graph_id, key)
            .unwrap();
        store
            .finalize_bulk_load_step(caller, graph_id, key, 6)
            .unwrap();
        let after_retention = 6 + CLIENT_MUTATION_KEY_TTL_NS + 1;
        let first = store
            .bulk_load_receipt_gc_step(caller, graph_id, key, after_retention)
            .unwrap();
        assert_eq!(first.removed, 1);
        assert!(!first.done);
        assert!(store.bulk_load_chunk_receipt(parent_id, 0).is_none());
        assert!(matches!(
            store.start_bulk_load_job(caller, graph_id, key, fixture_target(), after_retention),
            Err(RouterError::Conflict(message)) if message.contains("expired")
        ));
        let (late_child, _, _) = fixture_child();
        assert!(matches!(
            store.admit_bulk_load_child(
                caller,
                graph_id,
                key,
                parent_id,
                1,
                late_child.chunk_fingerprint,
                late_child,
            ),
            Err(RouterError::Conflict(message)) if message.contains("expired")
        ));
        assert!(matches!(
            store.begin_bulk_load_finalize(caller, graph_id, key),
            Err(RouterError::Conflict(message)) if message.contains("expired")
        ));
        assert!(matches!(
            store.begin_bulk_load_abort(caller, graph_id, key, after_retention),
            Err(RouterError::Conflict(message)) if message.contains("expired")
        ));
        let second = store
            .bulk_load_receipt_gc_step(caller, graph_id, key, after_retention)
            .unwrap();
        assert!(second.done);
        assert!(
            store
                .router_mutation_record(&client_mutation_key(caller, graph_id, key))
                .is_none()
        );
    }
}
