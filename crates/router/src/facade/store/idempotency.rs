//! Mutation idempotency and client mutation journal.

use super::super::stable::label_stats::{
    ClientMutationKey, MutationReservationIndexEntry, RouterMutationRecord, RouterMutationShard,
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

/// Scan up to `budget` records starting strictly after `start_after`, removing those
/// past [`CLIENT_MUTATION_KEY_TTL_NS`] that are not actively routing. Returns
/// `(scanned, removed, last_examined_key)`. `created_at_ns` on the record stays the sole
/// source of truth for age.
fn evict_expired_client_mutation_keys(
    start_after: Option<&ClientMutationKey>,
    budget: usize,
    now: u64,
) -> (u32, u32, Option<ClientMutationKey>) {
    let mut scanned: u32 = 0;
    let mut last_key: Option<ClientMutationKey> = None;
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
            if record.is_terminal()
                && now.saturating_sub(record.created_at_ns) > CLIENT_MUTATION_KEY_TTL_NS
                && !reservation_slot_pinned_raw(record.mutation_id)
                && !pending_effect_pinned_raw(key.graph_id, record.mutation_id)
            {
                expired.push((key.clone(), record.mutation_id));
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
    (scanned, removed, last_key)
}

/// Drop the heavy fields of a fully completed + projected record. The resolved
/// label/property tables and the shard fan-out are never read again once replay
/// short-circuits on `completed_row_count` (ADR 0025, mechanism E); `mutation_id`,
/// `created_at_ns`, `request_fingerprint`, and `completed_row_count` remain for
/// idempotent replay and TTL eviction.
fn compact_completed_record(record: &mut RouterMutationRecord) {
    record.resolved_labels = None;
    record.resolved_properties = None;
    record.shards = Vec::new();
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
            if now.saturating_sub(record.created_at_ns) > CLIENT_MUTATION_KEY_TTL_NS {
                return Err(RouterError::InvalidArgument(
                    "client_mutation_key expired; use a new key for a new mutation".into(),
                ));
            }
            if record.request_fingerprint != request_fingerprint {
                return Err(RouterError::Conflict(
                    "client_mutation_key was already used for a different request".into(),
                ));
            }
            // ADR 0030 slice 6: a terminally-failed mutation is irreversible — never re-dispatch it
            // under this key. The reclaim reconciler relies on this: once it cancels a reservation
            // on terminal-failure grounds, no later canonical write for this mutation can arrive, so
            // the same key must keep returning the stored terminal error (a new key starts fresh).
            if let Some(error) = &record.terminal_failure {
                return Err(RouterError::Conflict(error.clone()));
            }
            if record.routing_in_progress {
                // ADR 0029 Phase 4: honor an unexpired routing lease, but let a retry
                // reclaim one whose owner crashed before persisting the dispatch envelope.
                // Reclaiming is safe — `routing_in_progress == true` implies no envelope and
                // thus no canonical write has happened yet.
                let lease_live = record
                    .routing_lease_ns
                    .is_some_and(|started| now.saturating_sub(started) <= ROUTING_LEASE_TTL_NS);
                if lease_live {
                    return Err(RouterError::Conflict(
                        "client_mutation_key is already in progress; retry later".into(),
                    ));
                }
                record.routing_lease_ns = Some(now);
                let mutation_id = record.mutation_id;
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
                    m.insert(key, record);
                });
                return Ok(ClientMutationReservation {
                    mutation_id,
                    routing_owner: true,
                });
            }
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.routing_in_progress = true;
                record.routing_lease_ns = Some(now);
                let mutation_id = record.mutation_id;
                ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
                    m.insert(key, record);
                });
                return Ok(ClientMutationReservation {
                    mutation_id,
                    routing_owner: true,
                });
            }
            return Ok(ClientMutationReservation {
                mutation_id: record.mutation_id,
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
    pub(crate) fn gc_expired_client_mutation_keys(&self, now: u64) {
        let start = MUTATION_GC_CURSOR.with_borrow(|cursor| cursor.clone());
        let (scanned, _removed, last_key) =
            evict_expired_client_mutation_keys(start.as_ref(), MUTATION_GC_BUDGET as usize, now);
        let next = if scanned < MUTATION_GC_BUDGET {
            None
        } else {
            last_key
        };
        MUTATION_GC_CURSOR.with_borrow_mut(|cursor| *cursor = next);
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

        let (scanned, removed, last_key) =
            evict_expired_client_mutation_keys(start_after.as_ref(), max_scan as usize, now);

        // Fewer entries scanned than the budget means the range was exhausted.
        let done = scanned < max_scan;
        Ok(AdminSweepMutationKeysStepResult {
            scanned,
            removed,
            next_cursor: if done { None } else { last_key },
            done,
        })
    }

    pub fn router_mutation_record(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Option<RouterMutationRecord> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow(|m| m.get(&key))
    }

    /// Whether the record under `key` is the **same mutation** as `mutation_id` **and** has reached a
    /// terminal lifecycle phase (completed or terminally failed) — i.e. that exact mutation's effect
    /// generation has finished, so Driver 2 (ADR 0030 slice 6) may safely drain its pending effects.
    /// `None` when the record is gone (the GC pin should prevent this while a pending-effect row
    /// remains) **or** when `record.mutation_id != mutation_id` (a same-client-key retry recycled the
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
                .filter(|record| record.mutation_id == mutation_id)
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
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            if let Some(mut record) = m.get(key)
                && !record.is_terminal()
            {
                record.last_error = Some(error);
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
            if record.mutation_id != mutation_id {
                return false;
            }
            if record.terminal_failure.is_some() {
                return true;
            }
            if !record.is_uncommitted_dispatch() {
                return false;
            }
            record.terminal_failure = Some(error);
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
                if !record.routing_in_progress && !record.is_terminal() && !record.shards.is_empty()
                {
                    recoverable.push(key.clone());
                }
                last_key = Some(key);
            }
        });
        (recoverable, last_key, scanned)
    }

    pub fn record_router_mutation_shards(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        shards: Vec<RouterMutationShard>,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.resolved_labels = Some(resolved_labels);
                record.resolved_properties = Some(resolved_properties);
                record.routing_in_progress = false;
                record.shards = shards;
                m.insert(key, record);
            }
            Ok(())
        })
    }

    pub fn record_router_mutation_completed_without_shards(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        resolved_labels: ResolvedLabelTable,
        resolved_properties: ResolvedPropertyTable,
        row_count: u64,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            if record.shards.is_empty() && record.completed_row_count.is_none() {
                record.resolved_labels = Some(resolved_labels);
                record.resolved_properties = Some(resolved_properties);
                record.completed_row_count = Some(row_count);
                record.routing_in_progress = false;
                m.insert(key, record);
            }
            Ok(())
        })
    }

    pub fn abandon_router_mutation_routing_reservation(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            record.routing_in_progress = false;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_completed(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        shard_id: ShardId,
        row_count: u64,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let shard = record
                .shards
                .iter_mut()
                .find(|shard| shard.shard_id == shard_id)
                .ok_or(RouterError::ShardNotRegistered)?;
            shard.completed = true;
            shard.projection_advanced = false;
            shard.row_count = row_count;
            m.insert(key, record);
            Ok(())
        })
    }

    pub fn record_router_mutation_shard_projection_advanced(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        shard_id: ShardId,
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            let mut record = m
                .get(&key)
                .ok_or_else(|| RouterError::Internal("client mutation record missing".into()))?;
            let shard = record
                .shards
                .iter_mut()
                .find(|shard| shard.shard_id == shard_id)
                .ok_or(RouterError::ShardNotRegistered)?;
            shard.projection_advanced = true;
            // Once every shard is completed and projected, the mutation is fully done:
            // pin the final row count and drop the heavy fields (ADR 0025, mechanism E).
            // Subsequent replays short-circuit on completed_row_count and never read them.
            if record
                .shards
                .iter()
                .all(|shard| shard.completed && shard.projection_advanced)
            {
                let total = record
                    .shards
                    .iter()
                    .fold(0u64, |total, shard| total.saturating_add(shard.row_count));
                record.completed_row_count = Some(total);
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
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
        mutation_id: MutationId,
        row_count: u64,
        shards: &[gleaph_graph_kernel::federation::ShardRegistryEntry],
    ) -> Result<(), RouterError> {
        let key = client_mutation_key(caller, graph_id, client_key);
        let highest = shards.iter().map(|shard| shard.shard_id).max();
        let mut record = RouterMutationRecord::new(mutation_id, ic_time_ns(), Vec::new());
        record.routing_in_progress = false;
        record.shards = shards
            .iter()
            .map(|shard| {
                let mut entry =
                    RouterMutationShard::new(shard.shard_id, shard.graph_canister, None);
                entry.completed = true;
                entry.row_count = row_count;
                entry.projection_advanced = Some(shard.shard_id) != highest;
                entry
            })
            .collect();
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| m.insert(key, record));
        Ok(())
    }

    pub fn router_mutation_completed_row_count(
        &self,
        caller: Principal,
        graph_id: GraphId,
        client_key: &str,
    ) -> Option<u64> {
        let record = self.router_mutation_record(caller, graph_id, client_key)?;
        if let Some(row_count) = record.completed_row_count {
            return Some(row_count);
        }
        if record.shards.is_empty()
            || record
                .shards
                .iter()
                .any(|shard| !shard.completed || !shard.projection_advanced)
        {
            return None;
        }
        Some(
            record
                .shards
                .iter()
                .fold(0u64, |total, shard| total.saturating_add(shard.row_count)),
        )
    }
}

fn client_mutation_key(
    caller: Principal,
    graph_id: GraphId,
    client_key: &str,
) -> ClientMutationKey {
    ClientMutationKey::new(caller, graph_id, client_key.to_owned())
}
