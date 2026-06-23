//! Cross-shard uniqueness DROP CONSTRAINT drain — ADR 0030 slice 9, Driver 3.
//!
//! `DROP CONSTRAINT` flips a constraint `Active → Dropping` synchronously and returns; this lane
//! converges a `Dropping` constraint to `Removed` (terminal record deletion) **only** when the full
//! completion gate holds. It reuses the existing reservation-purge primitive and leans on Driver 1
//! ([`crate::reclaim`]) and Driver 2 ([`crate::effect_recovery`]) for the cases it must not resolve
//! itself (a `Reserved`/`Reclaiming` reservation, a still-pinned `Acquire`/`Release` effect).
//!
//! Per `Dropping` constraint:
//!
//! 1. **Purge** one bounded page of the constraint's reservations
//!    ([`reservation_catalog::purge_constraint_reservations`]): clean `Committed` rows are removed
//!    (values freed); `Reserved` rows are kicked into `Reclaiming` for Driver 1; `Reclaiming` and
//!    `Committed`-with-pending-ack rows are left for Driver 1.
//! 2. **Completion gate** — transition `Dropping → Removed` (delete the record) only when **all**
//!    hold, otherwise hold for a later lap:
//!    - no `UNIQUE_RESERVATIONS` rows remain for `(graph_id, constraint_id)`; **and**
//!    - a complete lap of the graph's `UNIQUE_EFFECT_PENDING` rows finds **no row that could carry
//!      this `constraint_id`** — meaning, for every row, either its owning mutation is terminal *and*
//!      its shard outbox holds no pinned effect for this `constraint_id`, **or** (cheaper, checked
//!      first) the mutation is terminal with an empty/clean outbox. A **non-terminal** owning
//!      mutation is held: it can still emit an `Acquire`/`Release` carrying this `constraint_id` after
//!      we delete the record, so its future effects cannot be ruled out; **and**
//!    - the constraint record's `drop_scan_generation` is unchanged across that lap (no pending-effect
//!      row was registered mid-lap behind the row cursor).
//!
//! The gate is the tombstone-release safety hinge: because the same `ConstraintNameId` is reused
//! after `Removed`, a leftover pinned (or *not-yet-emitted but still-possible*) `Acquire`/`Release`
//! carrying `constraint_id` could be drained by Driver 2 against the *new* constraint after re-CREATE.
//! "No reservations only" — and even "no currently-pinned effect" — is therefore insufficient: a
//! pending row whose owning mutation is still non-terminal can emit a matching effect *later*.
//!
//! **Bounded, cross-tick probe (round-robin like the other recovery lanes).** The pending-effect
//! probe never materializes the whole graph: it pages `unique_effect_pending::scan_graph_rows` a
//! bounded slice at a time, carrying a per-constraint **row cursor** across recovery ticks in
//! ephemeral [`DROP_PROBE_STATE`]. A lap is "clean" only after the row cursor reaches the graph end
//! (a short page is EOF) with no held row, *and* `drop_scan_generation` is unchanged from the value
//! snapshotted when the lap began. Any held row, or a generation change, resets the lap so a fresh
//! complete lap must prove cleanliness. The state is purely an in-memory bound/cursor: losing it on
//! upgrade simply restarts the lap, which is safe.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::facade::stable::constraint_catalog::{
    self, DroppingConstraint, UniqueConstraintKey, UniqueEnforcementStrategy,
};
use crate::facade::stable::reservation_catalog::{self, ProofShard};
use crate::facade::stable::unique_effect_pending::{
    self, PendingEffectRow, UniqueEffectPendingKey,
};
use crate::facade::store::RouterStore;
use crate::graph_client::{purge_local_unique_constraint, read_unique_mutation_effects};
use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};

/// Reservations purged per constraint per pass. Bounded so a constraint with a large committed-value
/// set is drained across laps rather than in one message.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
const PURGE_PAGE: usize = 32;

/// Pending-effect rows examined per constraint per pass. Bounds the probe's per-tick heap/instruction
/// cost; the per-constraint row cursor resumes the scan on the next tick so a graph with many pending
/// rows is still walked in full across laps.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
const PROBE_ROW_PAGE: usize = 32;

/// Effects pulled per outbox page in the pending-effect probe. The shard clamps to its own hard
/// maximum, so this is an upper bound; a short page is not EOF, only an empty page is.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
const EFFECT_PROBE_PAGE: u32 = 256;

/// Local unique entries purged per `ShardLocalGlobal` constraint per pass (ADR 0030 slice 10). The
/// owning shard clamps to its own bound; a constraint with a large local value set drains across laps.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
const LOCAL_PURGE_PAGE: u32 = 256;

thread_local! {
    /// Ephemeral, in-memory drop-drain probe state per `Dropping` constraint. Not stable: on upgrade
    /// it resets to an empty map, which restarts each constraint's probe lap from the beginning —
    /// always safe, since a fresh complete lap must independently prove cleanliness before `Removed`.
    #[cfg_attr(
        not(target_family = "wasm"),
        allow(
            dead_code,
            reason = "driven by the wasm recovery timer (drop-drain lane)"
        )
    )]
    static DROP_PROBE_STATE: RefCell<HashMap<(GraphId, ConstraintNameId), ProbeLap>> =
        RefCell::new(HashMap::new());
}

/// One constraint's in-progress pending-effect probe lap.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
#[derive(Clone, Copy, Debug)]
struct ProbeLap {
    /// Resume point in the graph's pending-effect rows; `None` means "next slice starts a fresh lap".
    row_cursor: Option<UniqueEffectPendingKey>,
    /// `drop_scan_generation` snapshotted when this lap began. A change by lap end means a
    /// pending-effect row was registered mid-lap, so the lap is not clean.
    lap_start_generation: u64,
}

/// Whether processing one `Dropping` constraint left work that should be retried on a later lap.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainOutcome {
    /// The completion gate held: the record was deleted (`Removed`), or the record had already
    /// vanished. Nothing more to do for this constraint.
    Done,
    /// Reservations or pending effects remain (or a shard was unreachable / a row registered
    /// mid-lap): retry on a later lap.
    Pending,
}

/// Whether one pending-effect row could carry the constraint being dropped.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowProbe {
    /// The row cannot affect this `constraint_id`: its owning mutation is terminal (no future
    /// effects) and its outbox holds no pinned effect for this `constraint_id`.
    Clear,
    /// The row must hold the drop: the mutation is non-terminal (a future effect for this
    /// `constraint_id` cannot be ruled out), an outbox effect already carries this `constraint_id`,
    /// or the shard was unreachable.
    Holds,
}

/// Decision of the termination gate, factored out of [`probe_row`] so the safety-critical rule —
/// "a row whose owning mutation cannot be proven terminal holds the drop **without** consulting a
/// (possibly transiently-empty) outbox" — is unit-testable without cross-canister I/O.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane); the core is unit-tested"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalGate {
    /// The owning mutation cannot be proven terminal (`Some(false)` non-terminal, or `None`
    /// missing/recycled): it can still emit an effect for this `constraint_id`, so hold immediately —
    /// an empty outbox does **not** make it clean.
    Hold,
    /// The owning mutation is terminal: its outbox is the final effect set and must be scanned.
    ScanOutbox,
}

/// Termination gate (mirrors Driver 2's fail-closed rule): only a terminal mutation's effect set is
/// final, so only then is its outbox worth scanning. A non-terminal or missing/recycled record holds.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane); the core is unit-tested"
    )
)]
fn terminal_gate(mutation_terminal: Option<bool>) -> TerminalGate {
    match mutation_terminal {
        Some(true) => TerminalGate::ScanOutbox,
        Some(false) | None => TerminalGate::Hold,
    }
}

/// Probe one pending-effect row against `constraint_id` (ADR 0030 slice 9 completion gate).
///
/// Termination first (cheap, no `await`): a non-terminal — or missing/recycled — owning mutation can
/// still emit an `Acquire`/`Release` for this `constraint_id`, so it **holds** without consulting the
/// outbox. Only once the mutation is proven terminal is its outbox the authoritative, final effect
/// set; it is then fully paginated (an empty page is the only EOF) and holds iff any pinned effect
/// carries this `constraint_id`. An unreachable shard cannot prove cleanliness, so it holds.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
async fn probe_row(
    store: &RouterStore,
    constraint_id: ConstraintNameId,
    row: PendingEffectRow,
) -> RowProbe {
    // Termination gate: a row whose owning mutation cannot be proven terminal holds without an outbox
    // read (an empty outbox does not make a still-emitting mutation clean).
    match terminal_gate(store.mutation_terminal_for(&row.record.client_key, row.key.mutation_id)) {
        TerminalGate::ScanOutbox => {}
        TerminalGate::Hold => return RowProbe::Holds,
    }

    let canister = row.record.canister;
    let mut cursor: Option<u32> = None;
    loop {
        let page = match read_unique_mutation_effects(
            canister,
            row.key.mutation_id,
            cursor,
            EFFECT_PROBE_PAGE,
        )
        .await
        {
            Ok(page) => page,
            // Unreachable shard: cannot prove the constraint is clean — hold.
            Err(_) => return RowProbe::Holds,
        };
        // Only an empty page is end-of-stream (the shard clamps `limit` to its own cap).
        let Some(last_ordinal) = page.last().map(|effect| effect.effect_id.effect_ordinal) else {
            break;
        };
        if page
            .iter()
            .any(|effect| effect.constraint_id == constraint_id)
        {
            return RowProbe::Holds;
        }
        cursor = Some(last_ordinal);
    }
    RowProbe::Clear
}

/// Read this constraint's probe lap, starting (or restarting) it at `current_generation` whenever
/// there is no lap in progress or the generation has already moved since the lap began.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
fn load_probe_lap(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    current_generation: u64,
) -> ProbeLap {
    DROP_PROBE_STATE.with_borrow_mut(|state| {
        let lap = state.entry((graph_id, constraint_id)).or_insert(ProbeLap {
            row_cursor: None,
            lap_start_generation: current_generation,
        });
        // A generation change since the lap began means a row registered behind the cursor — restart.
        if lap.lap_start_generation != current_generation {
            lap.row_cursor = None;
            lap.lap_start_generation = current_generation;
        }
        *lap
    })
}

/// Persist the advanced row cursor for an in-progress lap.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
fn store_probe_cursor(graph_id: GraphId, constraint_id: ConstraintNameId, lap: ProbeLap) {
    DROP_PROBE_STATE.with_borrow_mut(|state| {
        state.insert((graph_id, constraint_id), lap);
    });
}

/// Drop this constraint's probe lap (held row, gate met, or record gone): the next visit starts a
/// fresh complete lap.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
fn clear_probe_lap(graph_id: GraphId, constraint_id: ConstraintNameId) {
    DROP_PROBE_STATE.with_borrow_mut(|state| {
        state.remove(&(graph_id, constraint_id));
    });
}

/// Drain one `Dropping` constraint: purge one reservation page, then advance the bounded completion
/// gate (one pending-effect row slice per call).
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
async fn drain_constraint(store: &RouterStore, dc: DroppingConstraint) -> DrainOutcome {
    let graph_id = dc.graph_id;
    let constraint_id = dc.constraint_name_id;

    // ADR 0030 slice 10: a `ShardLocalGlobal` constraint never used reservations or outbox effects,
    // so its DROP drains the owning shard's local unique table directly and gates `Removed` on that
    // table being empty — it must never traverse the federated reservation/effect gates below.
    if let Some(rec) =
        constraint_catalog::find_unique_constraint_any_lifecycle(graph_id, constraint_id)
        && rec.strategy == UniqueEnforcementStrategy::ShardLocalGlobal
    {
        return drain_shard_local_constraint(graph_id, constraint_id, rec.owning_shard).await;
    }

    // 1. Purge one bounded page of this constraint's reservations (frees clean Committed, kicks
    //    Reserved). Re-scanned from the prefix start each pass: removed rows disappear, so successive
    //    passes make progress; Reserved/Reclaiming/pending-ack rows are converged by Driver 1.
    reservation_catalog::purge_constraint_reservations(graph_id, constraint_id, None, PURGE_PAGE);

    // 2a. All reservations must be gone before the pending-effect lap is meaningful. While any remain,
    //     restart the probe so it begins fresh once they clear.
    if reservation_catalog::constraint_has_reservations(graph_id, constraint_id) {
        clear_probe_lap(graph_id, constraint_id);
        return DrainOutcome::Pending;
    }

    // 2b. Pending-effect gate, walked in bounded row slices across ticks. Snapshot the
    //     scan-invalidation token from the record; the lap is valid only if it is unchanged from the
    //     value the lap began with (a mid-lap pending-effect registration bumps it).
    let Some(rec) =
        constraint_catalog::find_unique_constraint_any_lifecycle(graph_id, constraint_id)
    else {
        // The record vanished (e.g. graph teardown); nothing to drive.
        clear_probe_lap(graph_id, constraint_id);
        return DrainOutcome::Done;
    };
    let lap = load_probe_lap(graph_id, constraint_id, rec.drop_scan_generation);

    let (rows, last_examined, scanned) =
        unique_effect_pending::scan_graph_rows(graph_id, lap.row_cursor.as_ref(), PROBE_ROW_PAGE);
    for row in rows {
        match probe_row(store, constraint_id, row).await {
            // A row could still carry this constraint: not clean — restart the lap for a later tick.
            RowProbe::Holds => {
                clear_probe_lap(graph_id, constraint_id);
                return DrainOutcome::Pending;
            }
            RowProbe::Clear => {}
        }
    }

    // A full page means more rows remain: advance the cursor and continue the lap next tick.
    if scanned == PROBE_ROW_PAGE as u32 {
        store_probe_cursor(
            graph_id,
            constraint_id,
            ProbeLap {
                row_cursor: last_examined,
                lap_start_generation: lap.lap_start_generation,
            },
        );
        return DrainOutcome::Pending;
    }

    // The row lap completed with no held row. Re-read the record and confirm the scan-invalidation
    // token is unchanged across the *entire* lap (any registration since lap start invalidates it).
    match constraint_catalog::find_unique_constraint_any_lifecycle(graph_id, constraint_id) {
        Some(after) if after.drop_scan_generation == lap.lap_start_generation => {
            // 3. Full completion gate holds: delete the record (`Removed`); the id is now safe to
            //    reuse because no reservation, no pinned effect, and no non-terminal pending mutation
            //    can reference it.
            constraint_catalog::remove_dropped_constraint_record(graph_id, constraint_id);
            clear_probe_lap(graph_id, constraint_id);
            DrainOutcome::Done
        }
        // A row registered mid-lap (generation bumped): restart a fresh lap.
        Some(_) => {
            clear_probe_lap(graph_id, constraint_id);
            DrainOutcome::Pending
        }
        // The record vanished during the probe: nothing more to do.
        None => {
            clear_probe_lap(graph_id, constraint_id);
            DrainOutcome::Done
        }
    }
}

/// Drain one `Dropping` `ShardLocalGlobal` constraint (ADR 0030 slice 10): purge one bounded page of
/// the owning shard's local unique table for this constraint and transition `Dropping → Removed` only
/// once that shard confirms the constraint's local range is empty.
///
/// Fail-closed: the purge targets the **exact recorded owning canister** (`ProofShard.graph_canister`),
/// so shard-id reuse cannot misroute it. A record without an `owning_shard`, a still-non-empty table,
/// or an unreachable owner all **hold** the constraint `Dropping` rather than risk a premature
/// `Removed` while local values may still exist.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
async fn drain_shard_local_constraint(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    owning_shard: Option<ProofShard>,
) -> DrainOutcome {
    // A `ShardLocalGlobal` record must carry its owning shard identity; without it the local table
    // cannot be proven drained, so hold rather than delete the record.
    let Some(owning) = owning_shard else {
        return DrainOutcome::Pending;
    };
    match purge_local_unique_constraint(owning.graph_canister, constraint_id, LOCAL_PURGE_PAGE)
        .await
    {
        // The owner confirms its local table for this constraint is empty: safe to reuse the id.
        Ok(true) => {
            constraint_catalog::remove_dropped_constraint_record(graph_id, constraint_id);
            DrainOutcome::Done
        }
        // Entries remain (`Ok(false)`) or the owner was unreachable (`Err`): hold for a later lap.
        Ok(false) | Err(_) => DrainOutcome::Pending,
    }
}

/// Run one bounded drop-drain sweep starting after `cursor`. Returns the next scan cursor (`None`
/// when the constraint keyspace was exhausted — start a fresh lap) and whether any `Dropping`
/// constraint still needs a later lap.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (drop-drain lane)"
    )
)]
pub(crate) async fn run_constraint_drop_pass(
    cursor: Option<UniqueConstraintKey>,
    budget: usize,
    _now: u64,
) -> (Option<UniqueConstraintKey>, bool) {
    let store = RouterStore::new();
    let (dropping, last_examined, scanned) =
        constraint_catalog::scan_dropping_constraints(cursor.as_ref(), budget);
    let mut found = false;
    for dc in dropping {
        match drain_constraint(&store, dc).await {
            DrainOutcome::Pending => found = true,
            DrainOutcome::Done => {}
        }
    }
    let lap_complete = scanned < budget as u32;
    let next = if lap_complete { None } else { last_examined };
    (next, found)
}

#[cfg(test)]
mod tests {
    use super::{TerminalGate, terminal_gate};

    /// The completion gate's safety hinge (ADR 0030 slice 9): a pending-effect row whose owning
    /// mutation cannot be proven terminal holds the drop **without** consulting the outbox, so a
    /// transiently-empty outbox can never let a still-emitting mutation pass the gate. Only a proven
    /// terminal mutation (`Some(true)`) defers to its (now-final) outbox.
    #[test]
    fn terminal_gate_holds_on_non_terminal_or_missing_without_outbox() {
        // Non-terminal owning mutation: can still emit an effect for this constraint_id → Hold.
        assert_eq!(terminal_gate(Some(false)), TerminalGate::Hold);
        // Missing/recycled record (cannot prove termination): fail-closed → Hold.
        assert_eq!(terminal_gate(None), TerminalGate::Hold);
        // Terminal: the outbox is the final effect set and must be scanned.
        assert_eq!(terminal_gate(Some(true)), TerminalGate::ScanOutbox);
    }
}
