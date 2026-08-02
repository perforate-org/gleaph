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
        let (request_graph_id, target_shard, target_canister) = child.graph_request.target();
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
        let chunk_envelope = BulkLoadChunkEnvelopeV1::from_chunk(&chunk);
        let chunk_fingerprint = chunk_envelope.fingerprint().unwrap();
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
            chunk_envelope,
            graph_request: request,
            graph_request_fingerprint,
            child_mutation_id: 1,
            progress: BulkLoadChunkProgressV1::CanonicalPending,
            public_receipt: None,
            graph_receipt: None,
            completed_at_ns: None,
        };
        (row, graph_receipt, public_receipt)
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
                .router_mutation_record(caller, GraphId::from_raw(1), "job")
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
                .router_mutation_record(caller, GraphId::from_raw(1), "invalid")
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
                .router_mutation_record(caller, graph_id, "counter-boundary")
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
                .router_mutation_record(caller, graph_id, "parent-boundary")
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
                .router_mutation_record(caller, graph_id, key)
                .is_none()
        );
    }
}
