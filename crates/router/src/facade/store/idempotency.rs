//! Mutation idempotency and client mutation journal.

use super::super::stable::label_stats::{
    ClientMutationKey, MutationReservationIndexEntry, RouterMutationPayloadV1,
    RouterMutationRecord, RouterMutationShardV1,
};
use super::super::stable::{
    ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER, ROUTER_MUTATION_RESERVATION_INDEX,
};
use super::{
    CLIENT_MUTATION_KEY_TTL_NS, ClientMutationReservation, ROUTING_LEASE_TTL_NS, RouterStore,
    ic_time_ns, validate_client_mutation_key,
};
use crate::facade::auth;
use crate::state::RouterError;
use crate::types::{AdminSweepMutationKeysStepResult, ShardId};
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::{MutationId, ResolvedLabelTable, ResolvedPropertyTable};
use gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;
use ic_stable_structures::Storable;
use std::cell::RefCell;
use std::ops::Bound;

thread_local! {
    /// Ephemeral round-robin cursor for amortized GC (ADR 0025, mechanism B). It is
    /// heap-only on purpose: resetting to the start on upgrade just restarts the lap,
    /// and the journal itself (the source of truth) is fully stable.
    static MUTATION_GC_CURSOR: RefCell<Option<ClientMutationKey>> = const { RefCell::new(None) };
}

/// Entries examined per amortized GC step on the mutation-reservation path. Each new
/// reservation evicts up to this many expired records, so eviction keeps pace with the
/// only source of growth (new client keys) and the journal converges to its TTL window.
const MUTATION_GC_BUDGET: u32 = 2;

struct OrderedBatchTransitionErrors {
    mutation_id_mismatch: &'static str,
    request_fingerprint_mismatch: &'static str,
    inactive_routing_reservation: &'static str,
    already_completed: &'static str,
    non_pristine_scalar_reservation: &'static str,
    oversized_record: &'static str,
}

struct OrderedBatchTransition {
    request_identity: crate::facade::stable::label_stats::RouterMutationRequestIdentityV1,
    resolved_labels: ResolvedLabelTable,
    resolved_properties: ResolvedPropertyTable,
    payload: RouterMutationPayloadV1,
    errors: OrderedBatchTransitionErrors,
}

/// The only durable writes an ordered-retirement callback may make.
///
/// `Persist` advances the exact replay target to `RetirementPending`; `Completed` replaces that
/// target only after the Graph retirement acknowledgement has been checked; `IdempotentNoop`
/// leaves either an exact pending or an exact compacted-terminal replay untouched.
enum OrderedBatchRetirementUpdate {
    Persist,
    Completed { row_count: u64 },
    IdempotentNoop,
}

/// The only durable writes an ordered batch progress callback may make.
///
/// `Persist` records the requested progress transition; `IdempotentNoop` leaves an exact replay
/// untouched.
enum OrderedBatchProgressUpdate {
    Persist,
    IdempotentNoop,
}

/// Normalized read-only view of the family-specific target fields that gate an ordered retirement
/// completion. The payload-family match and its diagnostic remain at the caller, while this type
/// owns the common fingerprint, exact-receipt, and projection-watermark checks.
struct OrderedRetirementCompletionState<'a, Receipt> {
    graph_request_fingerprint: [u8; 32],
    retirement_pending_receipt: Option<&'a Receipt>,
    projection_watermark: Option<&'a gleaph_graph_kernel::plan_exec::MutationTokenShard>,
}

impl<Receipt: PartialEq> OrderedRetirementCompletionState<'_, Receipt> {
    fn require_exact(
        &self,
        graph_request_fingerprint: [u8; 32],
        receipt: &Receipt,
        fingerprint_mismatch: &'static str,
        retirement_pending_required: &'static str,
        projection_watermark_required: &'static str,
    ) -> Result<gleaph_graph_kernel::plan_exec::MutationTokenShard, RouterError> {
        if self.graph_request_fingerprint != graph_request_fingerprint {
            return Err(RouterError::Conflict(fingerprint_mismatch.into()));
        }
        if self.retirement_pending_receipt != Some(receipt) {
            return Err(RouterError::Conflict(retirement_pending_required.into()));
        }
        self.projection_watermark
            .cloned()
            .ok_or_else(|| RouterError::Conflict(projection_watermark_required.into()))
    }
}

fn require_ordered_retirement_target(
    target_graph_request_fingerprint: [u8; 32],
    projection_watermark: Option<&gleaph_graph_kernel::plan_exec::MutationTokenShard>,
    graph_request_fingerprint: [u8; 32],
    fingerprint_mismatch: &'static str,
    projection_watermark_required: &'static str,
) -> Result<(), RouterError> {
    if target_graph_request_fingerprint != graph_request_fingerprint {
        return Err(RouterError::Conflict(fingerprint_mismatch.into()));
    }
    if projection_watermark.is_none() {
        return Err(RouterError::Conflict(projection_watermark_required.into()));
    }
    Ok(())
}

fn exact_completed_ordered_retirement_replay<Receipt: PartialEq>(
    existing_graph_request_fingerprint: &[u8; 32],
    existing_receipt: &Receipt,
    graph_request_fingerprint: [u8; 32],
    receipt: &Receipt,
    fingerprint_mismatch: &'static str,
    ordered_replay_payload_required: &'static str,
) -> Result<OrderedBatchRetirementUpdate, RouterError> {
    if existing_graph_request_fingerprint != &graph_request_fingerprint {
        return Err(RouterError::Conflict(fingerprint_mismatch.into()));
    }
    if existing_receipt == receipt {
        return Ok(OrderedBatchRetirementUpdate::IdempotentNoop);
    }
    Err(RouterError::Conflict(
        ordered_replay_payload_required.into(),
    ))
}

#[cfg(test)]
pub(crate) fn reset_mutation_gc_cursor_for_test() {
    MUTATION_GC_CURSOR.with_borrow_mut(|cursor| *cursor = None);
}

/// Non-terminal reservation count for `mutation_id` from the reverse index (ADR 0030 slice 6).
/// The row exists iff the count is non-zero, so a missing row reads as `0`.
fn reservation_slot_count_raw(mutation_id: MutationId) -> u32 {
    ROUTER_MUTATION_RESERVATION_INDEX
        .with_borrow(|idx| idx.get(&mutation_id).map_or(0, |entry| entry.nonterminal))
}

/// `true` while `mutation_id` still owns at least one non-terminal reservation — its record must
/// not be GC'd, since the reclaim reconciler needs it to make a terminal-failure decision.
fn reservation_slot_pinned_raw(mutation_id: MutationId) -> bool {
    reservation_slot_count_raw(mutation_id) > 0
}

/// `true` while `(graph_id, mutation_id)` still owns at least one pending unique-effect discovery
/// row — its record must not be GC'd, since Driver 2 reads this record's terminal completion state
/// before it removes the row (ADR 0030 slice 6). A `Release`/orphan mutation has no reservation, so
/// this is its only GC pin.
fn pending_effect_pinned_raw(graph_id: GraphId, mutation_id: MutationId) -> bool {
    crate::facade::stable::unique_effect_pending::pending_effect_pinned(graph_id, mutation_id)
}

/// Ordered public batches retain their client key for as long as the durable Router replay target
/// is non-terminal. Their Graph journal can outlive the ordinary seven-day client-key window, and
/// an exact retry remains the only safe way to resolve a lost canonical/retirement callback.
fn ordered_replay_target_active(record: &RouterMutationRecord) -> bool {
    matches!(
        record.payload(),
        RouterMutationPayloadV1::OrderedEdgeBatch(_)
            | RouterMutationPayloadV1::OrderedVertexBatch(_)
            | RouterMutationPayloadV1::OrderedMixedBatch(_)
    ) && !record.is_terminal()
}

/// Scan up to `budget` records starting strictly after `start_after`, removing those
/// past [`CLIENT_MUTATION_KEY_TTL_NS`] that are not actively routing. Returns
/// `(scanned, removed, last_examined_key)`. Terminal records use their durable
/// `terminal_at_ns` anchor; non-terminal records are never age-evictable.
fn evict_expired_client_mutation_keys(
    start_after: Option<&ClientMutationKey>,
    budget: usize,
    now: u64,
) -> (u32, u32, Option<ClientMutationKey>, Vec<ClientMutationKey>) {
    let mut scanned: u32 = 0;
    let mut last_key: Option<ClientMutationKey> = None;
    let mut expired_bulk_loads = Vec::new();
    // Each evictable candidate is captured with its `mutation_id` so the apply removes the reverse
    // index row in lockstep with the record (ADR 0030 slice 6): one read-only preflight, then a
    // failure-free apply, never a partial removal.
    let mut expired: Vec<(ClientMutationKey, MutationId)> = Vec::new();
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
        let lower = match start_after {
            Some(key) => Bound::Excluded(key.clone()),
            None => Bound::Unbounded,
        };
        for entry in m.range((lower, Bound::Unbounded)).take(budget) {
            let key = entry.key().clone();
            let record = entry.value();
            scanned += 1;
            // ADR 0029 Phase 4: only *terminal* sagas are TTL-evictable. A non-terminal
            // saga (CanonicalPending / CanonicalCommitted / ProjectionPending / Routing) is
            // retained as a recovery target so the recovery driver can still converge it;
            // evicting it would silently strand unfinished cross-canister work.
            //
            // ADR 0030 slice 6: even a *terminal* record stays pinned while it still owns a
            // non-terminal reservation — the reclaim reconciler resolves a reservation's claim to
            // this record to decide a terminal failure, so evicting it would strand that claim.
            // It also stays pinned while any pending unique-effect discovery row remains, since
            // Driver 2 reads this record's completion state before removing the row (the only pin a
            // Release/orphan mutation has, as it owns no reservation).
            let expired_terminal = record.is_terminal()
                && record.as_v1().terminal_at_ns.is_some_and(|terminal_at| {
                    now.saturating_sub(terminal_at) > CLIENT_MUTATION_KEY_TTL_NS
                });
            if expired_terminal
                && matches!(
                    record.payload(),
                    RouterMutationPayloadV1::BulkLoadCoordinator(_)
                )
            {
                // Bulk parents own a durable receipt range and must be cleaned by the dedicated
                // bounded GC step.  Do not remove the client binding here: the GC step advances
                // the durable receipt cursor and removes the parent only after the child range is
                // empty.
                expired_bulk_loads.push(key.clone());
            } else if expired_terminal
                && !reservation_slot_pinned_raw(record.as_v1().mutation_id)
                && !pending_effect_pinned_raw(key.graph_id, record.as_v1().mutation_id)
            {
                expired.push((key.clone(), record.as_v1().mutation_id));
            }
            last_key = Some(key);
        }
    });
    let removed = expired.len() as u32;
    if removed > 0 {
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            for (key, _) in &expired {
                m.remove(key);
            }
        });
        // Defensive: the reverse row is already absent for an unpinned mutation (it is removed when
        // the last non-terminal reservation leaves), but remove it here too so record and reverse
        // row can never diverge.
        ROUTER_MUTATION_RESERVATION_INDEX.with_borrow_mut(|idx| {
            for (_, mutation_id) in &expired {
                idx.remove(mutation_id);
            }
        });
    }
    (scanned, removed, last_key, expired_bulk_loads)
}

/// Drop the heavy fields of a fully completed + projected record. The resolved
/// label/property tables and the shard fan-out are never read again once replay
/// short-circuits on `completed_row_count` (ADR 0025, mechanism E); `mutation_id`,
/// `created_at_ns`, `terminal_at_ns`, `request_fingerprint`, and `completed_row_count` remain for
/// idempotent replay and TTL eviction.
pub(crate) fn compact_completed_record(record: &mut RouterMutationRecord) {
    use crate::facade::stable::label_stats::RouterMutationPayloadV1;
    record.as_v1_mut().resolved_labels = None;
    record.as_v1_mut().resolved_properties = None;
    // ADR 0025 mechanism E: scalar terminal representation stays `Scalar { shards: [] }`.
    match record.payload().clone() {
        RouterMutationPayloadV1::BulkLoadCoordinator(_) => {}
        RouterMutationPayloadV1::Scalar { .. } => {
            record.payload_mut().scalar_clear_shards();
        }
        RouterMutationPayloadV1::OrderedEdgeBatchRouting
        | RouterMutationPayloadV1::OrderedEdgeBatch(_)
        | RouterMutationPayloadV1::OrderedVertexBatchRouting
        | RouterMutationPayloadV1::OrderedVertexBatch(_)
        | RouterMutationPayloadV1::OrderedMixedBatchRouting
        | RouterMutationPayloadV1::OrderedMixedBatch(_)
        | RouterMutationPayloadV1::CompletedOrderedEdgeBatch { .. }
        | RouterMutationPayloadV1::CompletedOrderedVertexBatch { .. }
        | RouterMutationPayloadV1::CompletedOrderedMixedBatch { .. } => {}
    }
    record.mark_terminal_at_ns(ic_time_ns());
}

impl RouterStore {
    pub fn allocate_mutation_id(&self) -> Result<MutationId, RouterError> {
        ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| {
            let next = counter
                .get()
                .checked_add(1)
                .ok_or_else(|| RouterError::IdExhausted("mutation_id".into()))?;
            if next == 0 {
                return Err(RouterError::IdExhausted("mutation_id".into()));
            }
            counter.set(next);
            Ok(next)
        })
    }

    pub fn reserve_mutation_id_for_client_key(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        request_fingerprint: Vec<u8>,
    ) -> Result<ClientMutationReservation, RouterError> {
        self.reserve_mutation_id_for_client_key_at(
            caller,
            graph_id,
            client_key,
            request_fingerprint,
            ic_time_ns(),
        )
    }

    pub(crate) fn reserve_mutation_id_for_client_key_at(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        request_fingerprint: Vec<u8>,
        now: u64,
    ) -> Result<ClientMutationReservation, RouterError> {
        validate_client_mutation_key(client_key)?;
        let key = client_mutation_key(caller, graph_id, client_key);
        if let Some(mut record) = ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key)) {
            if record.is_terminal()
                && record.as_v1().terminal_at_ns.is_some_and(|terminal_at| {
                    now.saturating_sub(terminal_at) > CLIENT_MUTATION_KEY_TTL_NS
                })
                && !ordered_replay_target_active(&record)
            {
                return Err(RouterError::InvalidArgument(
                    "client_mutation_key expired; use a new key for a new mutation".into(),
                ));
            }
            if record.as_v1().request_identity.request_fingerprint() != request_fingerprint {
                return Err(RouterError::Conflict(
                    "client_mutation_key was already used for a different request".into(),
                ));
            }
            // ADR 0030 slice 6: a terminally-failed mutation is irreversible — never re-dispatch it
            // under this key. The reclaim reconciler relies on this: once it cancels a reservation
            // on terminal-failure grounds, no later canonical write for this mutation can arrive, so
            // the same key must keep returning the stored terminal error (a new key starts fresh).
            if let Some(error) = &record.as_v1().terminal_failure {
                return Err(RouterError::Conflict(error.clone()));
            }
            // Ordered replay payloads own their durable Graph target rather than the scalar shard
            // list. They must never enter the pristine scalar-reservation branch below, even when
            // the request is retried after the ordinary client-key TTL.
            if matches!(
                record.payload(),
                RouterMutationPayloadV1::OrderedEdgeBatch(_)
                    | RouterMutationPayloadV1::OrderedVertexBatch(_)
                    | RouterMutationPayloadV1::OrderedMixedBatch(_)
                    | RouterMutationPayloadV1::CompletedOrderedEdgeBatch { .. }
                    | RouterMutationPayloadV1::CompletedOrderedVertexBatch { .. }
                    | RouterMutationPayloadV1::CompletedOrderedMixedBatch { .. }
            ) {
                return Ok(ClientMutationReservation {
                    mutation_id: record.as_v1().mutation_id,
                    routing_owner: false,
                });
            }
            if record.as_v1().routing_in_progress {
                // ADR 0029 Phase 4: honor an unexpired routing lease, but let a retry
                // reclaim one whose owner crashed before persisting the dispatch envelope.
                // Reclaiming is safe — `routing_in_progress == true` implies no envelope and
                // thus no canonical write has happened yet.
                let lease_live = record
                    .as_v1()
                    .routing_lease_ns
                    .is_some_and(|started| now.saturating_sub(started) <= ROUTING_LEASE_TTL_NS);
                if lease_live {
                    return Err(RouterError::Conflict(
                        "client_mutation_key is already in progress; retry later".into(),
                    ));
                }
                record.as_v1_mut().routing_lease_ns = Some(now);
                let mutation_id = record.as_v1().mutation_id;
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
                    m.insert(key, record);
                });
                return Ok(ClientMutationReservation {
                    mutation_id,
                    routing_owner: true,
                });
            }
            if record.shards().is_empty() && record.as_v1().completed_row_count.is_none() {
                record.as_v1_mut().routing_in_progress = true;
                record.as_v1_mut().routing_lease_ns = Some(now);
                let mutation_id = record.as_v1().mutation_id;
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
                    m.insert(key, record);
                });
                return Ok(ClientMutationReservation {
                    mutation_id,
                    routing_owner: true,
                });
            }
            return Ok(ClientMutationReservation {
                mutation_id: record.as_v1().mutation_id,
                routing_owner: false,
            });
        }
        let mutation_id = self.allocate_mutation_id()?;
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            m.insert(
                key,
                RouterMutationRecord::new(mutation_id, now, request_fingerprint),
            );
        });
        // Amortized GC (ADR 0025, mechanism B): every new reservation evicts a bounded
        // slice of expired records, so the journal stays bounded automatically without a
        // timer or a separate time-ordered index.
        self.gc_expired_client_mutation_keys(now);
        Ok(ClientMutationReservation {
            mutation_id,
            routing_owner: true,
        })
    }

    /// Amortized, automatic eviction of expired records. Advances a heap round-robin
    /// cursor over the journal keyspace, examining [`MUTATION_GC_BUDGET`] records per
    /// call and wrapping at the end. Driven by [`reserve_mutation_id_for_client_key_at`]
    /// (the sole growth source), so the journal converges to its TTL working set.
    pub(crate) fn gc_expired_client_mutation_keys(&self, now: u64) -> bool {
        let start = MUTATION_GC_CURSOR.with_borrow(|cursor| cursor.clone());
        let (scanned, _removed, last_key, expired_bulk_loads) =
            evict_expired_client_mutation_keys(start.as_ref(), MUTATION_GC_BUDGET as usize, now);
        let bulk_gc_pending = self.drive_expired_bulk_load_gc(&expired_bulk_loads, now);
        // A bulk parent with receipts remaining must be revisited on the next write.  Resetting
        // the ordinary journal cursor prevents a short final page from skipping that parent until
        // a later round-robin lap.
        let next = if bulk_gc_pending || scanned < MUTATION_GC_BUDGET {
            None
        } else {
            last_key
        };
        MUTATION_GC_CURSOR.with_borrow_mut(|cursor| *cursor = next);
        bulk_gc_pending
    }

    /// Drive one bounded receipt-GC step for each expired bulk parent found in the current journal
    /// slice.  A child that is still pending keeps the parent for a later recovery lap; malformed
    /// durable bulk state remains a corruption trap rather than being silently evicted.
    fn drive_expired_bulk_load_gc(&self, keys: &[ClientMutationKey], now: u64) -> bool {
        let mut pending = false;
        for key in keys {
            match self.bulk_load_receipt_gc_step(key.caller, key.graph_id, &key.client_key, now) {
                Ok(step) => pending |= !step.done,
                Err(RouterError::Busy { .. }) => pending = true,
                Err(error) => panic!("bulk-load receipt GC failed: {error}"),
            }
        }
        pending
    }

    /// Remove expired client-mutation idempotency records in a bounded, paginated
    /// pass. The journal (`ROUTER_MUTATION_BY_CLIENT_KEY`) is keyed by
    /// `(caller, graph_id, client_key)` with no time ordering, so eviction scans a
    /// budgeted slice of the keyspace per call; the operator drives it to
    /// completion by feeding `next_cursor` back as `start_after` (the router has no
    /// timer — maintenance is operator-driven, like backfill / projection).
    ///
    /// Only records past [`CLIENT_MUTATION_KEY_TTL_NS`] that are **not**
    /// `routing_in_progress` are removed, so an in-flight reservation is never
    /// yanked. Records within the TTL window are retained for idempotent replay.
    pub fn admin_sweep_expired_client_mutation_keys(
        &self,
        caller: Principal,
        start_after: Option<ClientMutationKey>,
        max_scan: u32,
    ) -> Result<AdminSweepMutationKeysStepResult, RouterError> {
        self.admin_sweep_expired_client_mutation_keys_at(
            caller,
            start_after,
            max_scan,
            ic_time_ns(),
        )
    }

    pub(crate) fn admin_sweep_expired_client_mutation_keys_at(
        &self,
        caller: Principal,
        start_after: Option<ClientMutationKey>,
        max_scan: u32,
        now: u64,
    ) -> Result<AdminSweepMutationKeysStepResult, RouterError> {
        auth::require_admin(&caller)?;
        if max_scan == 0 {
            return Err(RouterError::InvalidArgument(
                "max_scan must be greater than zero".into(),
            ));
        }

        let (scanned, removed, last_key, expired_bulk_loads) =
            evict_expired_client_mutation_keys(start_after.as_ref(), max_scan as usize, now);
        let _bulk_gc_pending = self.drive_expired_bulk_load_gc(&expired_bulk_loads, now);

        // Fewer entries scanned than the budget means the range was exhausted.
        let done = scanned < max_scan;
        Ok(AdminSweepMutationKeysStepResult {
            scanned,
            removed,
            next_cursor: if done { None } else { last_key },
            done,
        })
    }

    pub fn router_mutation_record(&self, key: &ClientMutationKey) -> Option<RouterMutationRecord> {
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(key))
    }

    /// Whether the record under `key` is the **same mutation** as `mutation_id` **and** has reached a
    /// terminal lifecycle phase (completed or terminally failed) — i.e. that exact mutation's effect
    /// generation has finished, so Driver 2 (ADR 0030 slice 6) may safely drain its pending effects.
    /// `None` when the record is gone (the GC pin should prevent this while a pending-effect row
    /// remains) **or** when `record.as_v1().mutation_id != mutation_id` (a same-client-key retry recycled the
    /// record onto a *different* mutation, so this record cannot prove the pending mutation terminal);
    /// both are hold signals, never a drain.
    #[cfg_attr(
        not(target_family = "wasm"),
        allow(
            dead_code,
            reason = "driven by the wasm recovery timer (Driver 2); resolvers are unit-tested"
        )
    )]
    pub(crate) fn mutation_terminal_for(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
    ) -> Option<bool> {
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
            m.get(key)
                .filter(|record| record.as_v1().mutation_id == mutation_id)
                .map(|record| record.is_terminal())
        })
    }

    /// Record a recovery diagnostic on a mutation, surfaced by `mutation_status` (ADR 0029
    /// Phase 4). No-op if the record is gone or already terminal.
    pub fn record_router_mutation_last_error(
        &self,
        key: &ClientMutationKey,
        error: String,
    ) -> Result<(), RouterError> {
        let error = crate::facade::stable::label_stats::bound_mutation_recovery_diagnostic(error);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            if let Some(mut record) = m.get(key)
                && !record.is_terminal()
            {
                record.as_v1_mut().last_error = Some(error);
                m.insert(key.clone(), record);
            }
            Ok(())
        })
    }

    /// Establish irreversible terminal-failure as Cancel grounds for the reclaim reconciler (ADR
    /// 0030 slice 6), no `await`. Returns `true` iff — after this call — mutation `mutation_id` under
    /// `key` is terminally failed, so the caller may cancel its reservation **and** decrement the
    /// non-terminal count in this same message. The record-side gate (the proof's all-`proof_scope`-
    /// absent half is the caller's):
    /// - `mutation_id` must match the record (guards a recycled/reused client key);
    /// - already `terminal_failure` ⇒ `true` (idempotent — a *sibling* reservation of the same
    ///   already-failed mutation is still cancelable, and the predicate below would reject it);
    /// - otherwise eligible only if [`RouterMutationRecord::is_uncommitted_dispatch`]: a durable
    ///   dispatch envelope exists but no shard's canonical write committed and routing is released.
    ///
    /// The predicate re-check is the recovery race guard: between the proof's absence read and this
    /// commit, a same-key retry may have re-routed the mutation (`Routing`) or a canonical write may
    /// have completed on a shard. Either makes it ineligible, the flip is refused (`false`), and the
    /// caller must `hold` rather than cancel.
    pub(crate) fn terminally_fail_uncommitted_dispatch(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        error: String,
    ) -> bool {
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let Some(mut record) = m.get(key) else {
                return false;
            };
            if record.as_v1().mutation_id != mutation_id {
                return false;
            }
            if record.as_v1().terminal_failure.is_some() {
                return true;
            }
            if !record.is_uncommitted_dispatch() {
                return false;
            }
            record.as_v1_mut().terminal_failure = Some(error);
            record.mark_terminal_at_ns(ic_time_ns());
            m.insert(key.clone(), record);
            true
        })
    }

    /// Read-only overflow preflight for the non-terminal reservation count of `mutation_id` (ADR
    /// 0030 slice 6). `fresh_upper` is the *maximum* number of reservations a Try could freshly
    /// insert (its claim count); the actual fresh count is `<= fresh_upper`. Returns `Err` if even
    /// that upper bound would overflow `u32`, so the count is rejected **before** any reservation is
    /// written. Once this passes, [`apply_reservation_slots`](Self::apply_reservation_slots) with the
    /// real fresh count is infallible. Mutates nothing.
    pub(crate) fn preflight_reservation_slots(
        &self,
        mutation_id: MutationId,
        fresh_upper: u32,
    ) -> Result<(), RouterError> {
        reservation_slot_count_raw(mutation_id)
            .checked_add(fresh_upper)
            .ok_or_else(|| {
                RouterError::Internal(format!(
                    "non-terminal reservation count overflow for mutation {mutation_id}"
                ))
            })
            .map(|_| ())
    }

    /// Apply of a Try's fresh reservations to the reverse index (ADR 0030 slice 6): bump
    /// `mutation_id`'s non-terminal count by `fresh`, creating the row (pinned to `key`) on the
    /// first reservation. Must run in the same no-`await` message as the reservation insert and only
    /// after [`preflight_reservation_slots`](Self::preflight_reservation_slots) has cleared overflow.
    /// A `fresh` of zero (a pure idempotent replay) is a no-op, so replays never create a row.
    ///
    /// This is a GC-pin safety mechanism, so it is **fail-closed**: rather than masking a corrupt
    /// count, it traps (rolling back the whole message) if an existing row is owned by a different
    /// client key — `mutation_id` maps to exactly one [`ClientMutationKey`] — or if the bump
    /// overflows despite the preflight (which would mean the preflight was bypassed). On the IC a
    /// trap is the only rollback, so an inconsistency must trap here, not be silently absorbed.
    pub(crate) fn apply_reservation_slots(
        &self,
        mutation_id: MutationId,
        key: &ClientMutationKey,
        fresh: u32,
    ) {
        if fresh == 0 {
            return;
        }
        ROUTER_MUTATION_RESERVATION_INDEX.with_borrow_mut(|idx| {
            let nonterminal = match idx.get(&mutation_id) {
                Some(existing) => {
                    assert!(
                        &existing.client_key == key,
                        "reverse index row for mutation {mutation_id} is owned by a different \
                         client key; a mutation_id must map to exactly one ClientMutationKey \
                         (ADR 0030 slice 6 invariant)"
                    );
                    existing.nonterminal.checked_add(fresh).unwrap_or_else(|| {
                        panic!(
                            "non-terminal reservation count for mutation {mutation_id} overflowed \
                             on apply despite the overflow preflight (ADR 0030 slice 6 invariant)"
                        )
                    })
                }
                None => fresh,
            };
            idx.insert(
                mutation_id,
                MutationReservationIndexEntry {
                    client_key: key.clone(),
                    nonterminal,
                },
            );
        });
    }

    /// Release of one non-terminal reservation slot for `mutation_id` (ADR 0030 slice 6): decrement
    /// the count on a `FreshlyCommitted` Confirm or a reclaim Cancel, removing the row when it
    /// reaches zero (which un-pins the owning record for TTL GC).
    ///
    /// This is a GC-pin safety mechanism, so it is **fail-closed**: every release must correspond to
    /// a reservation counted at Try, so a missing row (or a stored count already at zero, which the
    /// row invariant forbids) is an under-count that would let a pinned record be GC'd while a
    /// non-terminal sibling reservation still depends on it. Rather than mask it with a no-op, this
    /// traps, rolling back the Confirm/Cancel that issued the bad release in the same message.
    pub(crate) fn release_reservation_slot(&self, mutation_id: MutationId) {
        ROUTER_MUTATION_RESERVATION_INDEX.with_borrow_mut(|idx| {
            let mut entry = idx.get(&mutation_id).unwrap_or_else(|| {
                panic!(
                    "reservation slot release for mutation {mutation_id} with no reverse index row: \
                     a Confirm/Cancel decremented a reservation that was never counted at Try \
                     (ADR 0030 slice 6 invariant)"
                )
            });
            entry.nonterminal = entry.nonterminal.checked_sub(1).unwrap_or_else(|| {
                panic!(
                    "reservation slot release for mutation {mutation_id} at zero count: the reverse \
                     index row must not exist with a zero count (ADR 0030 slice 6 invariant)"
                )
            });
            if entry.nonterminal == 0 {
                idx.remove(&mutation_id);
            } else {
                idx.insert(mutation_id, entry);
            }
        });
    }

    /// Resolve a reservation's claim (`mutation_id`) to the owning record's [`ClientMutationKey`] via
    /// the reverse index (ADR 0030 slice 6). The reclaim reconciler uses this to find the record for
    /// a terminal-failure decision; a missing row means no non-terminal reservation remains, so the
    /// reconciler must `hold` rather than guess.
    pub(crate) fn reservation_index_client_key(
        &self,
        mutation_id: MutationId,
    ) -> Option<ClientMutationKey> {
        ROUTER_MUTATION_RESERVATION_INDEX
            .with_borrow(|idx| idx.get(&mutation_id).map(|entry| entry.client_key))
    }

    /// Bounded scan for sagas the recovery driver can converge: non-terminal records that
    /// have a persisted dispatch envelope and are not currently held by an active routing
    /// lease (ADR 0029 Phase 4). Returns `(recoverable_keys, last_examined, scanned)`; the
    /// caller advances a round-robin cursor with `last_examined`.
    pub fn scan_recoverable_mutations(
        &self,
        start_after: Option<&ClientMutationKey>,
        budget: usize,
    ) -> (Vec<ClientMutationKey>, Option<ClientMutationKey>, u32) {
        let mut scanned: u32 = 0;
        let mut last_key: Option<ClientMutationKey> = None;
        let mut recoverable: Vec<ClientMutationKey> = Vec::new();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| {
            let lower = match start_after {
                Some(key) => Bound::Excluded(key.clone()),
                None => Bound::Unbounded,
            };
            for entry in m.range((lower, Bound::Unbounded)).take(budget) {
                let key = entry.key().clone();
                let record = entry.value();
                scanned += 1;
                let has_dispatch_envelope = !record.shards().is_empty()
                    || matches!(
                        record.payload(),
                        crate::facade::stable::label_stats::RouterMutationPayloadV1::OrderedEdgeBatch(
                            _
                        )
                        | crate::facade::stable::label_stats::RouterMutationPayloadV1::OrderedVertexBatch(
                            _
                        )
                        | crate::facade::stable::label_stats::RouterMutationPayloadV1::OrderedMixedBatch(
                            _
                        )
                    );
                if !record.as_v1().routing_in_progress
                    && !record.is_terminal()
                    && has_dispatch_envelope
                {
                    recoverable.push(key.clone());
                }
                last_key = Some(key);
            }
        });
        (recoverable, last_key, scanned)
    }

    fn transition_scalar_reservation_to_ordered_payload(
        key: &ClientMutationKey,
        mutation_id: MutationId,
        transition: OrderedBatchTransition,
    ) -> Result<(), RouterError> {
        let OrderedBatchTransition {
            request_identity,
            resolved_labels,
            resolved_properties,
            payload,
            errors,
        } = transition;
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let v1 = record.as_v1_mut();
            if v1.mutation_id != mutation_id {
                return Err(RouterError::Conflict(errors.mutation_id_mismatch.into()));
            }
            if v1.request_identity.request_fingerprint() != request_identity.request_fingerprint() {
                return Err(RouterError::Conflict(
                    errors.request_fingerprint_mismatch.into(),
                ));
            }
            if !v1.routing_in_progress {
                return Err(RouterError::Conflict(
                    errors.inactive_routing_reservation.into(),
                ));
            }
            if v1.completed_row_count.is_some() {
                return Err(RouterError::Conflict(errors.already_completed.into()));
            }
            if !matches!(
                v1.payload,
                RouterMutationPayloadV1::Scalar { ref shards } if shards.is_empty()
            ) {
                return Err(RouterError::Conflict(
                    errors.non_pristine_scalar_reservation.into(),
                ));
            }
            v1.request_identity = request_identity;
            v1.resolved_labels = Some(resolved_labels);
            v1.resolved_properties = Some(resolved_properties);
            v1.payload = payload;
            v1.routing_in_progress = false;
            v1.routing_lease_ns = None;
            if record.to_bytes().len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
                return Err(RouterError::InvalidArgument(errors.oversized_record.into()));
            }
            m.insert(key, record);
            Ok(())
        })
    }

    fn apply_ordered_batch_retirement<F>(
        key: &ClientMutationKey,
        mutation_id: MutationId,
        mutation_id_mismatch: &'static str,
        update_payload: F,
    ) -> Result<(), RouterError>
    where
        F: FnOnce(
            &mut RouterMutationPayloadV1,
        ) -> Result<OrderedBatchRetirementUpdate, RouterError>,
    {
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let update = {
                let v1 = record.as_v1_mut();
                if v1.mutation_id != mutation_id {
                    return Err(RouterError::Conflict(mutation_id_mismatch.into()));
                }
                update_payload(&mut v1.payload)?
            };
            match update {
                OrderedBatchRetirementUpdate::Persist => {
                    m.insert(key, record);
                }
                OrderedBatchRetirementUpdate::Completed { row_count } => {
                    let v1 = record.as_v1_mut();
                    v1.completed_row_count = Some(row_count);
                    v1.resolved_labels = None;
                    v1.resolved_properties = None;
                    v1.routing_in_progress = false;
                    v1.routing_lease_ns = None;
                    record.mark_terminal_at_ns(ic_time_ns());
                    m.insert(key, record);
                }
                OrderedBatchRetirementUpdate::IdempotentNoop => {}
            }
            Ok(())
        })
    }

    fn apply_ordered_batch_progress_update<F>(
        key: &ClientMutationKey,
        mutation_id: MutationId,
        mutation_id_mismatch: &'static str,
        update_payload: F,
    ) -> Result<(), RouterError>
    where
        F: FnOnce(&mut RouterMutationPayloadV1) -> Result<OrderedBatchProgressUpdate, RouterError>,
    {
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let update = {
                let v1 = record.as_v1_mut();
                if v1.mutation_id != mutation_id {
                    return Err(RouterError::Conflict(mutation_id_mismatch.into()));
                }
                update_payload(&mut v1.payload)?
            };
            match update {
                OrderedBatchProgressUpdate::Persist => {
                    m.insert(key, record);
                }
                OrderedBatchProgressUpdate::IdempotentNoop => {}
            }
            Ok(())
        })
    }

    /// Atomically transition a pristine scalar reservation to an ordered Graph replay payload.
    ///
    /// The public fingerprint and item count remain Router-owned identity; the target validates
    /// the independently encoded Graph request fingerprint before the routing lease is released.
    pub fn transition_to_ordered_edge_batch(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        request_identity: crate::facade::stable::label_stats::RouterMutationRequestIdentityV1,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        target: crate::facade::stable::label_stats::RouterOrderedEdgeBatchTargetV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedEdgeBatchTargetProgressV1, RouterMutationRequestIdentityV1,
            RouterOrderedEdgeBatchReplayV1,
        };
        let public_item_count = match &request_identity {
            RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                public_item_count, ..
            } => *public_item_count,
            _ => {
                return Err(RouterError::InvalidArgument(
                    "ordered edge batch transition requires an OrderedEdgeBatch request identity"
                        .into(),
                ));
            }
        };
        target.validate()?;
        if target.request.items.len() != public_item_count as usize {
            return Err(RouterError::InvalidArgument(
                "ordered public item count does not match Graph request".into(),
            ));
        }
        if !matches!(
            target.progress,
            OrderedEdgeBatchTargetProgressV1::CanonicalPending
        ) {
            return Err(RouterError::InvalidArgument(
                "ordered Graph target must be pristine at admission".into(),
            ));
        }
        Self::transition_scalar_reservation_to_ordered_payload(
            key,
            mutation_id,
            OrderedBatchTransition {
                request_identity,
                resolved_labels,
                resolved_properties,
                payload: RouterMutationPayloadV1::OrderedEdgeBatch(Box::new(
                    RouterOrderedEdgeBatchReplayV1 { target },
                )),
                errors: OrderedBatchTransitionErrors {
                    mutation_id_mismatch: "mutation_id mismatch at ordered batch transition",
                    request_fingerprint_mismatch: "public request fingerprint mismatch at ordered batch transition",
                    inactive_routing_reservation: "ordered batch transition requires an active routing reservation",
                    already_completed: "ordered batch transition refused: record already completed",
                    non_pristine_scalar_reservation: "ordered batch transition requires a pristine scalar reservation",
                    oversized_record: "ordered edge batch record exceeds safe inter-canister payload bound",
                },
            },
        )
    }

    /// Atomically transition a pristine scalar reservation to an ordered Graph vertex replay
    /// payload. The vertex variant is deliberately separate from the edge variant so a client key
    /// cannot replay a request through the wrong Graph endpoint or receipt type.
    pub fn transition_to_ordered_vertex_batch(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        request_identity: crate::facade::stable::label_stats::RouterMutationRequestIdentityV1,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        target: crate::facade::stable::label_stats::RouterOrderedVertexBatchTargetV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedVertexBatchTargetProgressV1, RouterMutationRequestIdentityV1,
            RouterOrderedVertexBatchReplayV1,
        };
        let public_item_count = match &request_identity {
            RouterMutationRequestIdentityV1::OrderedVertexBatch {
                public_item_count, ..
            } => *public_item_count,
            _ => {
                return Err(RouterError::InvalidArgument(
                    "ordered vertex batch transition requires an OrderedVertexBatch request identity"
                        .into(),
                ));
            }
        };
        target.validate()?;
        if target.request.items.len() != public_item_count as usize {
            return Err(RouterError::InvalidArgument(
                "ordered public vertex item count does not match Graph request".into(),
            ));
        }
        if !matches!(
            target.progress,
            OrderedVertexBatchTargetProgressV1::CanonicalPending
        ) {
            return Err(RouterError::InvalidArgument(
                "ordered vertex Graph target must be pristine at admission".into(),
            ));
        }
        Self::transition_scalar_reservation_to_ordered_payload(
            key,
            mutation_id,
            OrderedBatchTransition {
                request_identity,
                resolved_labels,
                resolved_properties,
                payload: RouterMutationPayloadV1::OrderedVertexBatch(Box::new(
                    RouterOrderedVertexBatchReplayV1 { target },
                )),
                errors: OrderedBatchTransitionErrors {
                    mutation_id_mismatch: "mutation_id mismatch at ordered vertex batch transition",
                    request_fingerprint_mismatch: "public request fingerprint mismatch at ordered vertex batch transition",
                    inactive_routing_reservation: "ordered vertex batch transition requires an active routing reservation",
                    already_completed: "ordered vertex batch transition refused: record already completed",
                    non_pristine_scalar_reservation: "ordered vertex batch transition requires a pristine scalar reservation",
                    oversized_record: "ordered vertex batch record exceeds safe inter-canister payload bound",
                },
            },
        )
    }

    /// Atomically transition a pristine scalar reservation to an ordered Graph mixed replay
    /// payload. The phase counts remain Router-owned request identity and are checked against the
    /// immutable Graph operation table before the routing lease is released.
    pub fn transition_to_ordered_mixed_batch(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        request_identity: crate::facade::stable::label_stats::RouterMutationRequestIdentityV1,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        target: crate::facade::stable::label_stats::RouterOrderedMixedBatchTargetV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedMixedBatchTargetProgressV1, RouterMutationRequestIdentityV1,
            RouterOrderedMixedBatchReplayV1,
        };
        let (public_operation_count, public_vertex_count, public_edge_count) =
            match &request_identity {
                RouterMutationRequestIdentityV1::OrderedMixedBatch {
                    public_operation_count,
                    public_vertex_count,
                    public_edge_count,
                    ..
                } => (
                    *public_operation_count,
                    *public_vertex_count,
                    *public_edge_count,
                ),
                _ => {
                    return Err(RouterError::InvalidArgument(
                    "ordered mixed batch transition requires an OrderedMixedBatch request identity"
                        .into(),
                ));
                }
            };
        target.validate()?;
        if target.request.operations.len() != public_operation_count as usize {
            return Err(RouterError::InvalidArgument(
                "ordered mixed public operation count does not match Graph request".into(),
            ));
        }
        let actual_vertex_count = target
            .request
            .operations
            .iter()
            .filter(|operation| {
                matches!(
                    operation,
                    gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Vertex(_)
                )
            })
            .count() as u32;
        let actual_edge_count = public_operation_count
            .checked_sub(actual_vertex_count)
            .ok_or_else(|| {
                RouterError::InvalidArgument("mixed operation count underflow".into())
            })?;
        if actual_vertex_count != public_vertex_count || actual_edge_count != public_edge_count {
            return Err(RouterError::InvalidArgument(
                "ordered mixed phase counts do not match Graph request".into(),
            ));
        }
        if !matches!(
            target.progress,
            OrderedMixedBatchTargetProgressV1::CanonicalPending
        ) {
            return Err(RouterError::InvalidArgument(
                "ordered mixed Graph target must be pristine at admission".into(),
            ));
        }
        Self::transition_scalar_reservation_to_ordered_payload(
            key,
            mutation_id,
            OrderedBatchTransition {
                request_identity,
                resolved_labels,
                resolved_properties,
                payload: RouterMutationPayloadV1::OrderedMixedBatch(Box::new(
                    RouterOrderedMixedBatchReplayV1 { target },
                )),
                errors: OrderedBatchTransitionErrors {
                    mutation_id_mismatch: "mutation_id mismatch at ordered mixed batch transition",
                    request_fingerprint_mismatch: "public request fingerprint mismatch at ordered mixed batch transition",
                    inactive_routing_reservation: "ordered mixed batch transition requires an active routing reservation",
                    already_completed: "ordered mixed batch transition refused: record already completed",
                    non_pristine_scalar_reservation: "ordered mixed batch transition requires a pristine scalar reservation",
                    oversized_record: "ordered mixed batch record exceeds safe inter-canister payload bound",
                },
            },
        )
    }

    /// Persist the Graph-owned canonical receipt for an ordered vertex batch.
    pub fn record_ordered_vertex_batch_canonical_committed(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        receipt: gleaph_graph_kernel::plan_exec::GraphOrderedVertexBatchReceiptV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        receipt
            .validate()
            .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let v1 = record.as_v1_mut();
            if v1.mutation_id != mutation_id {
                return Err(RouterError::Conflict(
                    "mutation_id mismatch at ordered vertex canonical completion".into(),
                ));
            }
            let replay = match &mut v1.payload {
                RouterMutationPayloadV1::OrderedVertexBatch(replay) => replay,
                _ => {
                    return Err(RouterError::Conflict(
                        "ordered vertex canonical completion requires an OrderedVertexBatch payload".into(),
                    ));
                }
            };
            if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                return Err(RouterError::Conflict(
                    "Graph request fingerprint mismatch at ordered vertex canonical completion".into(),
                ));
            }
            match &replay.target.progress {
                OrderedVertexBatchTargetProgressV1::CanonicalPending => {
                    replay.target.progress =
                        OrderedVertexBatchTargetProgressV1::CanonicalCommitted(receipt);
                    m.insert(key, record);
                    Ok(())
                }
                OrderedVertexBatchTargetProgressV1::CanonicalCommitted(existing)
                    if existing == &receipt => Ok(()),
                _ => Err(RouterError::Conflict(
                    "ordered vertex canonical completion received after progress advanced".into(),
                )),
            }
        })
    }

    /// Persist the Graph-owned canonical receipt for an ordered mixed batch.
    pub fn record_ordered_mixed_batch_canonical_committed(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        receipt: gleaph_graph_kernel::plan_exec::GraphOrderedMixedBatchReceiptV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedMixedBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        receipt
            .validate()
            .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered mixed canonical completion",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedMixedBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered mixed canonical completion requires an OrderedMixedBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered mixed canonical completion"
                            .into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedMixedBatchTargetProgressV1::CanonicalPending => {
                        replay.target.progress =
                            OrderedMixedBatchTargetProgressV1::CanonicalCommitted(receipt);
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedMixedBatchTargetProgressV1::CanonicalCommitted(existing)
                        if existing == &receipt =>
                    {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered mixed canonical completion conflicts with persisted progress"
                            .into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_mixed_batch_projection_pending(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedMixedBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered mixed projection pending",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedMixedBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered mixed projection pending requires an OrderedMixedBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered mixed projection pending"
                            .into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedMixedBatchTargetProgressV1::CanonicalCommitted(receipt) => {
                        replay.target.progress =
                            OrderedMixedBatchTargetProgressV1::ProjectionPending(receipt.clone());
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedMixedBatchTargetProgressV1::ProjectionPending(_) => {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered mixed projection pending conflicts with persisted progress".into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_mixed_batch_projection_advanced(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        watermark: gleaph_graph_kernel::plan_exec::MutationTokenShard,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedMixedBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered mixed projection advancement",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedMixedBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered mixed projection advancement requires an OrderedMixedBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered mixed projection advancement"
                            .into(),
                    ));
                }
                if watermark.shard_id != replay.target.request.target_shard_id {
                    return Err(RouterError::Conflict(
                        "ordered mixed projection watermark targets a different shard".into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedMixedBatchTargetProgressV1::ProjectionPending(receipt) => {
                        replay.target.progress =
                            OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(receipt.clone());
                        replay.target.projection_watermark = Some(watermark);
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(_)
                        if replay.target.projection_watermark.as_ref() == Some(&watermark) =>
                    {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered mixed projection advancement conflicts with persisted progress"
                            .into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_mixed_batch_retirement_pending(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::OrderedMixedBatchTargetProgressV1;
        Self::apply_ordered_batch_retirement(
            key,
            mutation_id,
            "mutation_id mismatch at ordered mixed retirement pending",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedMixedBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered mixed retirement pending requires an OrderedMixedBatch payload"
                                .into(),
                        ));
                    }
                };
                require_ordered_retirement_target(
                    replay.target.graph_request_fingerprint,
                    replay.target.projection_watermark.as_ref(),
                    graph_request_fingerprint,
                    "Graph request fingerprint mismatch at ordered mixed retirement pending",
                    "ordered mixed retirement requires a persisted projection watermark",
                )?;
                match &replay.target.progress {
                    OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(receipt) => {
                        replay.target.progress =
                            OrderedMixedBatchTargetProgressV1::RetirementPending(receipt.clone());
                        Ok(OrderedBatchRetirementUpdate::Persist)
                    }
                    OrderedMixedBatchTargetProgressV1::RetirementPending(_) => {
                        Ok(OrderedBatchRetirementUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered mixed retirement pending conflicts with persisted progress".into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_mixed_batch_retired(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        receipt: gleaph_graph_kernel::plan_exec::GraphOrderedMixedBatchReceiptV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::OrderedMixedBatchTargetProgressV1;
        receipt
            .validate()
            .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        Self::apply_ordered_batch_retirement(
            key,
            mutation_id,
            "mutation_id mismatch at ordered mixed retirement completion",
            |payload| {
                let watermark = match payload {
                    RouterMutationPayloadV1::OrderedMixedBatch(replay) => {
                        OrderedRetirementCompletionState {
                            graph_request_fingerprint: replay.target.graph_request_fingerprint,
                            retirement_pending_receipt: match &replay.target.progress {
                                OrderedMixedBatchTargetProgressV1::RetirementPending(receipt) => {
                                    Some(receipt)
                                }
                                _ => None,
                            },
                            projection_watermark: replay.target.projection_watermark.as_ref(),
                        }
                        .require_exact(
                            graph_request_fingerprint,
                            &receipt,
                            "Graph request fingerprint mismatch at ordered mixed retirement completion",
                            "ordered mixed retirement completion requires RetirementPending progress",
                            "ordered mixed retirement completion requires a projection watermark",
                        )?
                    }
                    RouterMutationPayloadV1::CompletedOrderedMixedBatch {
                        graph_request_fingerprint: existing_graph_request_fingerprint,
                        receipt: existing_receipt,
                        ..
                    } => {
                        return exact_completed_ordered_retirement_replay(
                            existing_graph_request_fingerprint,
                            existing_receipt,
                            graph_request_fingerprint,
                            &receipt,
                            "Graph request fingerprint mismatch at ordered mixed retirement completion",
                            "ordered mixed retirement completion requires an ordered replay payload",
                        );
                    }
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered mixed retirement completion requires an ordered replay payload"
                                .into(),
                        ));
                    }
                };
                let row_count = receipt.logical_operation_count;
                *payload = RouterMutationPayloadV1::CompletedOrderedMixedBatch {
                    graph_request_fingerprint,
                    receipt,
                    projection_watermark: watermark,
                };
                Ok(OrderedBatchRetirementUpdate::Completed { row_count })
            },
        )
    }

    pub fn record_ordered_vertex_batch_projection_pending(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered vertex projection pending",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedVertexBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered vertex projection pending requires an OrderedVertexBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered vertex projection pending"
                            .into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedVertexBatchTargetProgressV1::CanonicalCommitted(receipt) => {
                        replay.target.progress =
                            OrderedVertexBatchTargetProgressV1::ProjectionPending(receipt.clone());
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedVertexBatchTargetProgressV1::ProjectionPending(_) => {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered vertex projection pending conflicts with persisted progress"
                            .into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_vertex_batch_projection_advanced(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        watermark: gleaph_graph_kernel::plan_exec::MutationTokenShard,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered vertex projection advancement",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedVertexBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered vertex projection advancement requires an OrderedVertexBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered vertex projection advancement"
                            .into(),
                    ));
                }
                if watermark.shard_id != replay.target.request.target_shard_id {
                    return Err(RouterError::Conflict(
                        "ordered vertex projection watermark targets a different shard".into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedVertexBatchTargetProgressV1::ProjectionPending(receipt) => {
                        replay.target.progress =
                            OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(receipt.clone());
                        replay.target.projection_watermark = Some(watermark);
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(_)
                        if replay.target.projection_watermark.as_ref() == Some(&watermark) =>
                    {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered vertex projection advancement conflicts with persisted progress"
                            .into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_vertex_batch_retirement_pending(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::OrderedVertexBatchTargetProgressV1;
        Self::apply_ordered_batch_retirement(
            key,
            mutation_id,
            "mutation_id mismatch at ordered vertex retirement pending",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedVertexBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered vertex retirement pending requires an OrderedVertexBatch payload"
                                .into(),
                        ));
                    }
                };
                require_ordered_retirement_target(
                    replay.target.graph_request_fingerprint,
                    replay.target.projection_watermark.as_ref(),
                    graph_request_fingerprint,
                    "Graph request fingerprint mismatch at ordered vertex retirement pending",
                    "ordered vertex retirement requires a persisted projection watermark",
                )?;
                match &replay.target.progress {
                    OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(receipt) => {
                        replay.target.progress =
                            OrderedVertexBatchTargetProgressV1::RetirementPending(receipt.clone());
                        Ok(OrderedBatchRetirementUpdate::Persist)
                    }
                    OrderedVertexBatchTargetProgressV1::RetirementPending(_) => {
                        Ok(OrderedBatchRetirementUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered vertex retirement pending conflicts with persisted progress"
                            .into(),
                    )),
                }
            },
        )
    }

    pub fn record_ordered_vertex_batch_retired(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        receipt: gleaph_graph_kernel::plan_exec::GraphOrderedVertexBatchReceiptV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::OrderedVertexBatchTargetProgressV1;
        receipt
            .validate()
            .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        Self::apply_ordered_batch_retirement(
            key,
            mutation_id,
            "mutation_id mismatch at ordered vertex retirement completion",
            |payload| {
                let watermark = match payload {
                    RouterMutationPayloadV1::OrderedVertexBatch(replay) => {
                        OrderedRetirementCompletionState {
                            graph_request_fingerprint: replay.target.graph_request_fingerprint,
                            retirement_pending_receipt: match &replay.target.progress {
                                OrderedVertexBatchTargetProgressV1::RetirementPending(receipt) => {
                                    Some(receipt)
                                }
                                _ => None,
                            },
                            projection_watermark: replay.target.projection_watermark.as_ref(),
                        }
                        .require_exact(
                            graph_request_fingerprint,
                            &receipt,
                            "Graph request fingerprint mismatch at ordered vertex retirement completion",
                            "ordered vertex retirement completion requires RetirementPending progress",
                            "ordered vertex retirement completion requires a projection watermark",
                        )?
                    }
                    RouterMutationPayloadV1::CompletedOrderedVertexBatch {
                        graph_request_fingerprint: existing_graph_request_fingerprint,
                        receipt: existing_receipt,
                        ..
                    } => {
                        return exact_completed_ordered_retirement_replay(
                            existing_graph_request_fingerprint,
                            existing_receipt,
                            graph_request_fingerprint,
                            &receipt,
                            "Graph request fingerprint mismatch at ordered vertex retirement completion",
                            "ordered vertex retirement completion requires an ordered replay payload",
                        );
                    }
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered vertex retirement completion requires an ordered replay payload"
                                .into(),
                        ));
                    }
                };
                let row_count = receipt.logical_vertex_count;
                *payload = RouterMutationPayloadV1::CompletedOrderedVertexBatch {
                    graph_request_fingerprint,
                    receipt,
                    projection_watermark: watermark,
                };
                Ok(OrderedBatchRetirementUpdate::Completed { row_count })
            },
        )
    }

    /// Persist the Graph-owned canonical receipt for an ordered edge batch.
    ///
    /// The transition is idempotent for the exact same receipt and rejects every other progress
    /// state, so a lost Router callback cannot overwrite a later projection or retirement state.
    pub fn record_ordered_edge_batch_canonical_committed(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        receipt: gleaph_graph_kernel::plan_exec::GraphOrderedEdgeBatchReceiptV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedEdgeBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        receipt
            .validate()
            .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered canonical completion",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedEdgeBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered canonical completion requires an OrderedEdgeBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered canonical completion".into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedEdgeBatchTargetProgressV1::CanonicalPending => {
                        replay.target.progress =
                            OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(receipt);
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(existing)
                        if existing == &receipt =>
                    {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered canonical completion conflicts with persisted progress".into(),
                    )),
                }
            },
        )
    }

    /// Mark an ordered batch as waiting for Router-owned projection convergence.
    pub fn record_ordered_edge_batch_projection_pending(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedEdgeBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered projection pending",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedEdgeBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered projection pending requires an OrderedEdgeBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered projection pending".into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(receipt) => {
                        replay.target.progress =
                            OrderedEdgeBatchTargetProgressV1::ProjectionPending(receipt.clone());
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedEdgeBatchTargetProgressV1::ProjectionPending(_) => {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered projection pending conflicts with persisted progress".into(),
                    )),
                }
            },
        )
    }

    /// Persist the projection watermark after an ordered batch reaches all required projections.
    pub fn record_ordered_edge_batch_projection_advanced(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        watermark: gleaph_graph_kernel::plan_exec::MutationTokenShard,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::{
            OrderedEdgeBatchTargetProgressV1, RouterMutationPayloadV1,
        };
        Self::apply_ordered_batch_progress_update(
            key,
            mutation_id,
            "mutation_id mismatch at ordered projection advancement",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedEdgeBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered projection advancement requires an OrderedEdgeBatch payload"
                                .into(),
                        ));
                    }
                };
                if replay.target.graph_request_fingerprint != graph_request_fingerprint {
                    return Err(RouterError::Conflict(
                        "Graph request fingerprint mismatch at ordered projection advancement"
                            .into(),
                    ));
                }
                if watermark.shard_id != replay.target.request.target_shard_id {
                    return Err(RouterError::Conflict(
                        "ordered projection watermark targets a different shard".into(),
                    ));
                }
                match &replay.target.progress {
                    OrderedEdgeBatchTargetProgressV1::ProjectionPending(receipt) => {
                        replay.target.progress =
                            OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(receipt.clone());
                        replay.target.projection_watermark = Some(watermark);
                        Ok(OrderedBatchProgressUpdate::Persist)
                    }
                    OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(_)
                        if replay.target.projection_watermark.as_ref() == Some(&watermark) =>
                    {
                        Ok(OrderedBatchProgressUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered projection advancement conflicts with persisted progress".into(),
                    )),
                }
            },
        )
    }

    /// Persist that Router has begun the fingerprint-bound Graph retirement call.
    pub fn record_ordered_edge_batch_retirement_pending(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::OrderedEdgeBatchTargetProgressV1;
        Self::apply_ordered_batch_retirement(
            key,
            mutation_id,
            "mutation_id mismatch at ordered retirement pending",
            |payload| {
                let replay = match payload {
                    RouterMutationPayloadV1::OrderedEdgeBatch(replay) => replay,
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered retirement pending requires an OrderedEdgeBatch payload"
                                .into(),
                        ));
                    }
                };
                require_ordered_retirement_target(
                    replay.target.graph_request_fingerprint,
                    replay.target.projection_watermark.as_ref(),
                    graph_request_fingerprint,
                    "Graph request fingerprint mismatch at ordered retirement pending",
                    "ordered retirement requires a persisted projection watermark",
                )?;
                match &replay.target.progress {
                    OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(receipt) => {
                        replay.target.progress =
                            OrderedEdgeBatchTargetProgressV1::RetirementPending(receipt.clone());
                        Ok(OrderedBatchRetirementUpdate::Persist)
                    }
                    OrderedEdgeBatchTargetProgressV1::RetirementPending(_) => {
                        Ok(OrderedBatchRetirementUpdate::IdempotentNoop)
                    }
                    _ => Err(RouterError::Conflict(
                        "ordered retirement pending conflicts with persisted progress".into(),
                    )),
                }
            },
        )
    }

    /// Finalize an ordered mutation after the Graph retirement acknowledgement is durable.
    pub fn record_ordered_edge_batch_retired(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
        receipt: gleaph_graph_kernel::plan_exec::GraphOrderedEdgeBatchReceiptV1,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::OrderedEdgeBatchTargetProgressV1;
        receipt
            .validate()
            .map_err(|error| RouterError::InvalidArgument(error.into()))?;
        Self::apply_ordered_batch_retirement(
            key,
            mutation_id,
            "mutation_id mismatch at ordered retirement completion",
            |payload| {
                let watermark = match payload {
                    RouterMutationPayloadV1::OrderedEdgeBatch(replay) => {
                        OrderedRetirementCompletionState {
                            graph_request_fingerprint: replay.target.graph_request_fingerprint,
                            retirement_pending_receipt: match &replay.target.progress {
                                OrderedEdgeBatchTargetProgressV1::RetirementPending(receipt) => {
                                    Some(receipt)
                                }
                                _ => None,
                            },
                            projection_watermark: replay.target.projection_watermark.as_ref(),
                        }
                        .require_exact(
                            graph_request_fingerprint,
                            &receipt,
                            "Graph request fingerprint mismatch at ordered retirement completion",
                            "ordered retirement completion requires RetirementPending progress",
                            "ordered retirement completion requires a projection watermark",
                        )?
                    }
                    RouterMutationPayloadV1::CompletedOrderedEdgeBatch {
                        graph_request_fingerprint: existing_graph_request_fingerprint,
                        receipt: existing_receipt,
                        ..
                    } => {
                        return exact_completed_ordered_retirement_replay(
                            existing_graph_request_fingerprint,
                            existing_receipt,
                            graph_request_fingerprint,
                            &receipt,
                            "Graph request fingerprint mismatch at ordered retirement completion",
                            "ordered retirement completion requires an ordered replay payload",
                        );
                    }
                    _ => {
                        return Err(RouterError::Conflict(
                            "ordered retirement completion requires an ordered replay payload"
                                .into(),
                        ));
                    }
                };
                let row_count = receipt.logical_edge_count;
                *payload = RouterMutationPayloadV1::CompletedOrderedEdgeBatch {
                    graph_request_fingerprint,
                    receipt,
                    projection_watermark: watermark,
                };
                Ok(OrderedBatchRetirementUpdate::Completed { row_count })
            },
        )
    }
    pub fn record_router_mutation_shards(
        &self,
        key: &ClientMutationKey,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        shards: Vec<RouterMutationShardV1>,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::RouterMutationPayloadV1;
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            // Only a pristine Scalar reservation may be replaced by the scalar shard envelope.
            let existing = match &record.as_v1().payload {
                RouterMutationPayloadV1::Scalar { shards }
                    if record.as_v1().completed_row_count.is_none() =>
                {
                    shards
                }
                _ => {
                    return Err(RouterError::Conflict(
                        "scalar shard writer requires a pristine Scalar payload".into(),
                    ));
                }
            };
            if existing.is_empty() {
                // First persistence of the dispatch envelope.
            } else if existing.len() == shards.len()
                && existing.iter().zip(&shards).all(|(e, s)| {
                    e.shard_id == s.shard_id
                        && e.graph_canister == s.graph_canister
                        && e.seed_bindings_blob == s.seed_bindings_blob
                })
                && record.as_v1().resolved_labels.as_ref() == Some(&resolved_labels)
                && record.as_v1().resolved_properties.as_ref() == Some(&resolved_properties)
            {
                // The durable envelope is already recorded. Leave the existing progress flags
                // (completed / projection_advanced / row_count) untouched so a retry does not
                // conflict with a partially-converged saga.
                return Ok(());
            } else {
                return Err(RouterError::Conflict(
                    "scalar shard writer requires a pristine Scalar payload".into(),
                ));
            }
            record.as_v1_mut().resolved_labels = Some(resolved_labels);
            record.as_v1_mut().resolved_properties = Some(resolved_properties);
            record.as_v1_mut().routing_in_progress = false;
            record.as_v1_mut().payload = RouterMutationPayloadV1::Scalar { shards };
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_completed_without_shards(
        &self,
        key: &ClientMutationKey,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        row_count: u64,
    ) -> Result<(), RouterError> {
        use crate::facade::stable::label_stats::RouterMutationPayloadV1;
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            match &record.as_v1().payload {
                RouterMutationPayloadV1::Scalar { shards }
                    if shards.is_empty() && record.as_v1().completed_row_count.is_none() => {}
                RouterMutationPayloadV1::Scalar { shards }
                    if shards.is_empty()
                        && record.as_v1().completed_row_count == Some(row_count)
                        && record.as_v1().resolved_labels.as_ref() == Some(&resolved_labels)
                        && record.as_v1().resolved_properties.as_ref()
                            == Some(&resolved_properties) =>
                {
                    return Ok(());
                }
                _ => {
                    return Err(RouterError::Conflict(
                        "scalar completion writer requires a pristine Scalar payload".into(),
                    ));
                }
            }
            record.as_v1_mut().resolved_labels = Some(resolved_labels);
            record.as_v1_mut().resolved_properties = Some(resolved_properties);
            record.as_v1_mut().completed_row_count = Some(row_count);
            record.mark_terminal_at_ns(ic_time_ns());
            record.as_v1_mut().routing_in_progress = false;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn abandon_router_mutation_routing_reservation(
        &self,
        key: &ClientMutationKey,
    ) -> Result<(), RouterError> {
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            record.as_v1_mut().routing_in_progress = false;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_completed(
        &self,
        key: &ClientMutationKey,
        shard_id: ShardId,
        row_count: u64,
    ) -> Result<(), RouterError> {
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards().is_empty() && record.as_v1().completed_row_count.is_some() {
                // A concurrent/replayed path may have compacted this mutation after the caller's
                // pre-dispatch check. Graph mutation idempotency has already made it terminal;
                // there is no shard envelope left to update.
                return Ok(());
            }
            let shards = record.shards_mut().ok_or(RouterError::Internal(
                "mutation payload has no shard envelope".into(),
            ))?;
            let shard = shards
                .iter_mut()
                .find(|shard| shard.shard_id() == shard_id)
                .ok_or(RouterError::ShardNotRegistered)?;
            shard.set_completed(true);
            shard.set_projection_advanced(false);
            shard.set_row_count(row_count);
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_projection_advanced(
        &self,
        key: &ClientMutationKey,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        let key = key.clone();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards().is_empty() && record.as_v1().completed_row_count.is_some() {
                return Ok(());
            }
            let shards = record.shards_mut().ok_or(RouterError::Internal(
                "mutation payload has no shard envelope".into(),
            ))?;
            let shard = shards
                .iter_mut()
                .find(|shard| shard.shard_id() == shard_id)
                .ok_or(RouterError::ShardNotRegistered)?;
            shard.set_projection_advanced(true);
            // Once every shard is completed and projected, the mutation is fully done:
            // pin the final row count and drop the heavy fields (ADR 0025, mechanism E).
            // Subsequent replays short-circuit on completed_row_count and never read them.
            if record
                .shards()
                .iter()
                .all(|shard| shard.completed() && shard.projection_advanced())
            {
                let total = record
                    .shards()
                    .iter()
                    .fold(0u64, |total, shard| total.saturating_add(shard.row_count()));
                record.as_v1_mut().completed_row_count = Some(total);
                record.mark_terminal_at_ns(ic_time_ns());
                compact_completed_record(&mut record);
            }
            m.insert(key, record);
            Ok(())
        })
    }

    /// Test-only (`pocket-ic-e2e`): insert a non-terminal federated mutation record that the
    /// autonomous recovery driver can converge without a client in the loop. Every shard is marked
    /// canonical-complete; every shard except the highest `shard_id` is marked projection-advanced,
    /// leaving the record `ProjectionPending` on a multi-shard graph (or `CanonicalCommitted` on a
    /// single shard) — both projection-only recoverable states. `mutation_id` must name a mutation
    /// already committed on those shards so the driver finds a graph journal entry to project
    /// through. This builds the one persisted saga state that is unreachable through the black-box
    /// DML path (canonical durable, projection lagging), so the timer's autonomous convergence can
    /// be exercised end-to-end.
    #[cfg(feature = "pocket-ic-e2e")]
    pub fn test_insert_projection_pending_record(
        &self,
        key: &ClientMutationKey,
        mutation_id: MutationId,
        row_count: u64,
        shards: &[gleaph_graph_kernel::federation::ShardRegistryEntry],
    ) -> Result<(), RouterError> {
        let key = key.clone();
        let highest = shards.iter().map(|shard| shard.shard_id).max();
        let mut record = RouterMutationRecord::new(mutation_id, ic_time_ns(), Vec::new());
        record.as_v1_mut().routing_in_progress = false;
        record.as_v1_mut().payload =
            crate::facade::stable::label_stats::RouterMutationPayloadV1::Scalar {
                shards: shards
                    .iter()
                    .map(|shard| {
                        let mut entry =
                            RouterMutationShardV1::new(shard.shard_id, shard.graph_canister, None);
                        entry.set_completed(true);
                        entry.set_row_count(row_count);
                        entry.set_projection_advanced(Some(shard.shard_id) != highest);
                        entry
                    })
                    .collect(),
            };
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.insert(key, record));
        Ok(())
    }

    pub fn router_mutation_completed_row_count(&self, key: &ClientMutationKey) -> Option<u64> {
        let record = self.router_mutation_record(key)?;
        if let Some(row_count) = record.as_v1().completed_row_count {
            return Some(row_count);
        }
        if record.shards().is_empty()
            || record
                .shards()
                .iter()
                .any(|shard| !shard.completed() || !shard.projection_advanced())
        {
            return None;
        }
        Some(
            record
                .shards()
                .iter()
                .fold(0u64, |total, shard| total.saturating_add(shard.row_count())),
        )
    }
}

pub(crate) fn client_mutation_key(
    caller: Principal,
    graph_id: GraphId,
    client_key: &str,
) -> ClientMutationKey {
    ClientMutationKey::new(caller, graph_id, client_key.to_owned())
}
