//! Autonomous federated-saga recovery driver (ADR 0029 Phase 4).
//!
//! A self-rescheduling one-shot timer (`ic-cdk-timers`) that converges non-terminal
//! federated mutations without a client in the loop. It is armed event-driven from the
//! mutation path (after an idempotent DML leaves a saga non-terminal) and from canister
//! lifecycle hooks (`init` / `post_upgrade`), since timers do not survive an upgrade.
//!
//! Scope is deliberately **projection-only**: each tick scans a bounded slice of the
//! client-mutation journal for recoverable sagas (canonical writes already durable, only
//! label-stats projection lagging) and drives them forward with idempotent,
//! cursor-guarded projection advancement via [`crate::gql::recover_mutation_record`]. The
//! driver never re-dispatches canonical DML — that is the single operation that risks
//! double-apply, and is left to explicit retry-driven recovery surfaced through
//! `mutation_status`. A stuck *routing* reservation is reclaimed separately, by lease
//! expiry on the next retry (see `ROUTING_LEASE_TTL_NS`), not by this timer.
//!
//! Liveness is autonomous; observability is pull-based: a recovered saga becomes visible
//! through `AtLeast(token)` reads succeeding and through the `mutation_status` query.

/// Records examined per tick. Bounds the per-tick instruction cost; the round-robin cursor
/// resumes the scan on the next tick so a large journal is still fully covered.
#[cfg(target_family = "wasm")]
const RECOVERY_SCAN_BUDGET: usize = 16;

/// Reservations examined per tick by the ADR 0030 slice-6 reclaim reconciler (Driver 1). Separate,
/// smaller budget than the projection scan: each candidate can fan out cross-canister `Acquire`
/// proof reads, so its per-tick instruction cost is higher.
#[cfg(target_family = "wasm")]
const RECLAIM_SCAN_BUDGET: usize = 8;

/// Discovery rows examined per tick by the ADR 0030 slice-6 unified effect reconciler (Driver 2).
/// Same small budget rationale as the reclaim scan: each row can page a shard's effects and ack.
#[cfg(target_family = "wasm")]
const EFFECT_SCAN_BUDGET: usize = 8;

/// Direct vector-ingestion outbox rows examined per recovery tick. Each exact `(target, shard)`
/// group issues one bounded typed batch call; after resolution, one rotated Vector lane is
/// attempted per pass and stable rows remain pending until that frontier reply is observed.
#[cfg(target_family = "wasm")]
const VECTOR_INGEST_OUTBOX_SCAN_BUDGET: usize = 16;

/// Constraint records examined per tick by the ADR 0030 slice-9 drop-drain driver (Driver 3). Small
/// budget: each `Dropping` constraint can purge a reservation page and page each shard's outbox.
#[cfg(target_family = "wasm")]
const CONSTRAINT_DROP_SCAN_BUDGET: usize = 8;

/// Retired physical posting namespaces examined per tick by the ADR 0023 D6 retirement
/// drain driver (Driver 4). Each record issues at most one bounded inter-canister purge
/// step per pending target.
#[cfg(target_family = "wasm")]
const INDEX_RETIREMENT_SCAN_BUDGET: usize = 8;

/// Delay between ticks while a lap is still in progress (more keyspace to scan).
#[cfg(any(target_family = "wasm", test))]
const RECOVERY_FLOOR_DELAY: core::time::Duration = core::time::Duration::from_secs(2);

/// Watchdog cadence. Created at install/upgrade so a trap inside a recovery pass cannot take
/// away the lane's only wake-up.
#[cfg(target_family = "wasm")]
const RECOVERY_WATCHDOG_INTERVAL: core::time::Duration = core::time::Duration::from_secs(300);

/// How long past its deadline a timer may stay unobserved before the watchdog re-arms it.
#[cfg(target_family = "wasm")]
const RECOVERY_WATCHDOG_GRACE: core::time::Duration = core::time::Duration::from_secs(300);

/// Delay before starting a fresh lap when the previous lap still found recoverable sagas
/// (e.g. a shard whose graph projection had not yet caught up). Backs off so a persistently
/// lagging shard is retried without hot-looping.
#[cfg(target_family = "wasm")]
const RECOVERY_RELAXED_DELAY: core::time::Duration = core::time::Duration::from_secs(30);

#[cfg(any(target_family = "wasm", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryArmAction {
    Schedule,
    Latched,
    AlreadyScheduled,
}

/// Owns the recovery timer's logical lifecycle independently of its `TimerId` handle. The same
/// transition is used by the wasm timer and the native unit seam, so tests cover append-after-idle
/// and append-while-running behavior without sleeping for an IC timer.
#[cfg(any(target_family = "wasm", test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecoverySchedulerState {
    timer_armed: bool,
    running: bool,
    arm_requested: bool,
}

#[cfg(any(target_family = "wasm", test))]
impl RecoverySchedulerState {
    fn request_arm(&mut self) -> RecoveryArmAction {
        if self.running {
            self.arm_requested = true;
            RecoveryArmAction::Latched
        } else if self.timer_armed {
            RecoveryArmAction::AlreadyScheduled
        } else {
            self.timer_armed = true;
            RecoveryArmAction::Schedule
        }
    }

    fn begin_pass(&mut self) {
        assert!(!self.running, "recovery pass already running");
        self.timer_armed = false;
        self.running = true;
    }

    fn finish_pass(
        &mut self,
        pass_next: Option<core::time::Duration>,
    ) -> Option<core::time::Duration> {
        assert!(self.running, "recovery pass is not running");
        self.running = false;
        let arm_requested = std::mem::take(&mut self.arm_requested);
        let next = recovery_schedule_delay(pass_next, arm_requested);
        self.timer_armed = next.is_some();
        next
    }

    #[cfg(all(feature = "pocket-ic-e2e", target_family = "wasm"))]
    fn disarm(&mut self) {
        self.timer_armed = false;
    }
}

#[cfg(any(target_family = "wasm", test))]
/// The cursor/lap state one bounded recovery lane owns. A fresh lap starts with no cursor, so
/// `start` also clears the previous lap's found/wake flags; every lane calls the same sequence.
pub(super) struct LaneCursor<K> {
    cursor: std::cell::RefCell<Option<K>>,
    found: std::cell::Cell<bool>,
    wake_ns: std::cell::Cell<Option<u64>>,
}

impl<K: Clone> LaneCursor<K> {
    pub(super) const fn new() -> Self {
        Self {
            cursor: std::cell::RefCell::new(None),
            found: std::cell::Cell::new(false),
            wake_ns: std::cell::Cell::new(None),
        }
    }

    /// The cursor for the next bounded step; a fresh lap resets the lap flags.
    pub(super) fn start(&self) -> Option<K> {
        let start = self.cursor.borrow().clone();
        if start.is_none() {
            self.found.set(false);
            self.wake_ns.set(None);
        }
        start
    }

    pub(super) fn mark_found(&self) {
        self.found.set(true);
    }

    pub(super) fn set_next(&self, next: Option<K>) {
        *self.cursor.borrow_mut() = next;
    }

    pub(super) fn pending(&self) -> bool {
        self.cursor.borrow().is_some()
    }

    pub(super) fn found(&self) -> bool {
        self.found.get()
    }

    pub(super) fn note_wake(&self, wake_ns: u64) {
        let next = match self.wake_ns.get() {
            Some(current) => current.min(wake_ns),
            None => wake_ns,
        };
        self.wake_ns.set(Some(next));
    }

    pub(super) fn wake(&self) -> Option<u64> {
        self.wake_ns.get()
    }
}

thread_local! {
    /// Logical timer/running/lost-wake owner. The actual `TimerId` remains separate because it is
    /// needed only to cancel a scheduled IC timer.
    static RECOVERY_SCHEDULER: std::cell::RefCell<RecoverySchedulerState> =
        const { std::cell::RefCell::new(RecoverySchedulerState {
            timer_armed: false,
            running: false,
            arm_requested: false,
        }) };
}

#[cfg(target_family = "wasm")]
thread_local! {
    /// The single in-flight recovery timer, or `None` when idle. Rebuilt after upgrade.
    static RECOVERY_TIMER: std::cell::RefCell<Option<ic_cdk_timers::TimerId>> =
        const { std::cell::RefCell::new(None) };
    /// Absolute time the in-flight recovery timer is due, for the watchdog's liveness check.
    static RECOVERY_NEXT_DEADLINE_NS: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
    /// Independent interval that notices a latched `timer_armed` whose timer a trapped pass consumed.
    static RECOVERY_WATCHDOG_TIMER: std::cell::RefCell<Option<ic_cdk_timers::TimerId>> =
        const { std::cell::RefCell::new(None) };
    /// One lane's round-robin cursor, lap-found flag and earliest quarantine wake. Every lane
    /// uses this shape so a cursor and its lap flag cannot drift apart.
    pub(super) static RECOVERY_CURSOR: LaneCursor<
        crate::facade::stable::label_stats::ClientMutationKey,
    > = LaneCursor::new();
    pub(super) static RECLAIM_CURSOR: LaneCursor<
        crate::facade::stable::reservation_catalog::UniqueReservationKey,
    > = LaneCursor::new();
    pub(super) static EFFECT_CURSOR: LaneCursor<
        crate::facade::stable::unique_effect_pending::UniqueEffectPendingKey,
    > = LaneCursor::new();
    pub(super) static VECTOR_INGEST_OUTBOX_CURSOR: LaneCursor<u64> = LaneCursor::new();
    pub(super) static CONSTRAINT_DROP_CURSOR: LaneCursor<
        crate::facade::stable::constraint_catalog::UniqueConstraintKey,
    > = LaneCursor::new();
    pub(super) static INDEX_RETIREMENT_CURSOR: LaneCursor<u64> = LaneCursor::new();
    /// Test-feature-only deterministic GC seam. It is heap-only and absent from production builds;
    /// the ADR 0057 PocketIC test clears the ordinary recovery timer before advancing simulated
    /// time so one explicit bulk receipt-GC step remains exactly observable.
    #[cfg(feature = "pocket-ic-e2e")]
    static TEST_RECOVERY_PAUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Schedules the recovery timer iff one is not already armed or running. Idempotent and
/// self-guarding; safe to call from every mutation site and from lifecycle hooks. A no-op
/// on non-wasm builds, where there is no timer runtime.
pub(crate) fn arm_if_needed() {
    #[cfg(any(target_family = "wasm", test))]
    {
        #[cfg(all(feature = "pocket-ic-e2e", target_family = "wasm"))]
        if TEST_RECOVERY_PAUSED.with(std::cell::Cell::get) {
            return;
        }

        let action = RECOVERY_SCHEDULER.with_borrow_mut(|scheduler| scheduler.request_arm());
        if action == RecoveryArmAction::Schedule {
            #[cfg(target_family = "wasm")]
            {
                let timer_id = schedule(RECOVERY_FLOOR_DELAY);
                RECOVERY_TIMER.with_borrow_mut(|slot| *slot = Some(timer_id));
                RECOVERY_NEXT_DEADLINE_NS
                    .with(|deadline| deadline.set(Some(deadline_after(RECOVERY_FLOOR_DELAY))));
            }
        }
    }
}

/// Test-feature-only: cancel and suppress the autonomous recovery timer so a PocketIC test can
/// drive one exact production-owned bulk receipt-GC step. Upgrade resets this heap flag; the test
/// calls its exact-step endpoint immediately after reopen, before the newly scheduled timer is due.
#[cfg(all(feature = "pocket-ic-e2e", target_family = "wasm"))]
pub(crate) fn test_pause_for_exact_bulk_gc() {
    TEST_RECOVERY_PAUSED.with(|paused| paused.set(true));
    RECOVERY_SCHEDULER.with_borrow_mut(RecoverySchedulerState::disarm);
    RECOVERY_TIMER.with_borrow_mut(|slot| {
        if let Some(timer_id) = slot.take() {
            ic_cdk_timers::clear_timer(timer_id);
        }
    });
}

#[cfg(all(feature = "pocket-ic-e2e", not(target_family = "wasm")))]
pub(crate) fn test_pause_for_exact_bulk_gc() {}

#[cfg(target_family = "wasm")]
fn schedule(delay: core::time::Duration) -> ic_cdk_timers::TimerId {
    ic_cdk_timers::set_timer(delay, on_tick())
}

#[cfg(target_family = "wasm")]
async fn on_tick_migratory() {
    ic_cdk::futures::spawn_migratory(on_tick());
}

#[cfg(target_family = "wasm")]
fn schedule_migratory(delay: core::time::Duration) -> ic_cdk_timers::TimerId {
    ic_cdk_timers::set_timer(delay, on_tick_migratory())
}

/// Selects the next recovery delay after a pass. A durable arm request raised while the pass was
/// awaiting a remote call takes priority so work arriving after the scan cannot lose its wake-up.
#[cfg(any(target_family = "wasm", test))]
fn recovery_schedule_delay(
    pass_next: Option<core::time::Duration>,
    arm_requested: bool,
) -> Option<core::time::Duration> {
    arm_requested.then_some(RECOVERY_FLOOR_DELAY).or(pass_next)
}

/// Runs one bounded recovery pass, then reschedules per the lap state.
#[cfg(target_family = "wasm")]
async fn on_tick() {
    RECOVERY_TIMER.with_borrow_mut(|slot| *slot = None);
    #[cfg(all(feature = "pocket-ic-e2e", target_family = "wasm"))]
    if TEST_RECOVERY_PAUSED.with(std::cell::Cell::get) {
        return;
    }
    RECOVERY_SCHEDULER.with_borrow_mut(RecoverySchedulerState::begin_pass);

    let next = run_recovery_pass().await;

    let next = RECOVERY_SCHEDULER.with_borrow_mut(|scheduler| scheduler.finish_pass(next));
    if let Some(delay) = next {
        let id = schedule(delay);
        RECOVERY_TIMER.with_borrow_mut(|slot| *slot = Some(id));
        RECOVERY_NEXT_DEADLINE_NS.with(|deadline| deadline.set(Some(deadline_after(delay))));
    }
}

/// Creates the recovery watchdog interval once per canister instance.
///
/// A recovery pass reschedules itself only after it returns. If it traps, the IC restores the
/// heap to its pre-message state (`timer_armed == true`) while the one-shot timer that fired is
/// gone; every later `arm_if_needed` then reports `AlreadyScheduled` and the lane goes silent.
/// The watchdog compares the scheduled deadline with the clock and re-arms an overdue lane.
#[cfg(target_family = "wasm")]
pub(crate) fn arm_watchdog() {
    if RECOVERY_WATCHDOG_TIMER.with_borrow(Option::is_some) {
        return;
    }
    let timer_id = ic_cdk_timers::set_timer_interval(RECOVERY_WATCHDOG_INTERVAL, || async {
        watchdog_tick();
    });
    RECOVERY_WATCHDOG_TIMER.with_borrow_mut(|slot| *slot = Some(timer_id));
}

#[cfg(not(target_family = "wasm"))]
pub(crate) fn arm_watchdog() {}

#[cfg(target_family = "wasm")]
fn deadline_after(delay: core::time::Duration) -> u64 {
    ic_cdk::api::time().saturating_add(delay.as_nanos() as u64)
}

#[cfg(target_family = "wasm")]
fn watchdog_tick() {
    let now = ic_cdk::api::time();
    let (armed, running) =
        RECOVERY_SCHEDULER.with_borrow(|scheduler| (scheduler.timer_armed, scheduler.running));
    let overdue = RECOVERY_NEXT_DEADLINE_NS.with(|deadline| {
        deadline
            .get()
            .is_some_and(|at| now > at.saturating_add(RECOVERY_WATCHDOG_GRACE.as_nanos() as u64))
    });
    if armed && !running && overdue {
        // Drop the latched state and arm a fresh timer. Recovery is idempotent, so an extra
        // pass is safe; leaving the latch in place would stop every lane permanently.
        RECOVERY_SCHEDULER.with_borrow_mut(|scheduler| {
            scheduler.timer_armed = false;
            scheduler.arm_requested = false;
        });
        RECOVERY_NEXT_DEADLINE_NS.with(|deadline| deadline.set(None));
        RECOVERY_TIMER.with_borrow_mut(|slot| *slot = None);
        arm_if_needed();
    }
}

/// Scans the next bounded slice of the client-mutation journal and drives any recoverable
/// sagas toward terminal. Returns the reschedule delay (`None` stops the timer until the
/// next mutation re-arms it).
#[cfg(target_family = "wasm")]
async fn run_recovery_pass() -> Option<core::time::Duration> {
    use crate::facade::store::RouterStore;

    let store = RouterStore::new();
    // The journal's existing bounded GC owner also advances durable bulk-load receipt cleanup.
    // Keep the timer alive while a bulk parent has more receipts to delete; the parent cursor is
    // durable, so a later tick resumes exactly where the previous bounded step stopped.
    let bulk_gc_pending = store.gc_expired_client_mutation_keys(ic_cdk::api::time());
    let start = RECOVERY_CURSOR.start();

    let (keys, last_examined, scanned) =
        store.scan_recoverable_mutations(start.as_ref(), RECOVERY_SCAN_BUDGET);
    if !keys.is_empty() {
        RECOVERY_CURSOR.mark_found();
    }
    for key in keys {
        // Best-effort: a transient failure (e.g. graph briefly unavailable) leaves the saga
        // non-terminal so the next lap retries it.
        let _ = crate::gql::recover_mutation_record(&store, &key).await;
    }

    // ADR 0030 slice 6: drive a bounded slice of the reservation table through the reclaim
    // reconciler (Driver 1) on the same tick, with its own round-robin cursor. Best-effort: an
    // unreachable shard leaves the reservation held for the next lap.
    let reclaim_start = RECLAIM_CURSOR.start();
    let (reclaim_next, reclaim_found) =
        crate::reclaim::run_reclaim_pass(reclaim_start, RECLAIM_SCAN_BUDGET, ic_cdk::api::time())
            .await;
    if reclaim_found {
        RECLAIM_CURSOR.mark_found();
    }
    RECLAIM_CURSOR.set_next(reclaim_next.clone());

    // ADR 0030 slice 6: drive a bounded slice of the pending-effect discovery index through the
    // unified effect reconciler (Driver 2) on the same tick, with its own round-robin cursor.
    // Best-effort: an unreachable shard or a still-non-terminal mutation leaves the row for the next
    // lap; a quarantined orphan is parked behind its backoff.
    let effect_start = EFFECT_CURSOR.start();
    let effect_outcome = crate::effect_recovery::run_effect_recovery_pass(
        effect_start,
        EFFECT_SCAN_BUDGET,
        ic_cdk::api::time(),
    )
    .await;
    if effect_outcome.found {
        EFFECT_CURSOR.mark_found();
    }
    if let Some(wake) = effect_outcome.earliest_wake_ns {
        EFFECT_CURSOR.note_wake(wake);
    }
    let effect_next = effect_outcome.next_cursor;
    EFFECT_CURSOR.set_next(effect_next);

    // Direct vector-ingestion suffixes use the same bounded recovery timer and post-upgrade lane.
    // The durable row captures the exact target, so this pass never re-resolves a target from the
    // mutable Router catalog after an await.
    let vector_outbox_start = VECTOR_INGEST_OUTBOX_CURSOR.start();
    let vector_outbox_outcome = crate::facade::stable::vector_ingest_outbox::run_recovery_pass(
        vector_outbox_start,
        VECTOR_INGEST_OUTBOX_SCAN_BUDGET,
    )
    .await;
    if vector_outbox_outcome.found {
        VECTOR_INGEST_OUTBOX_CURSOR.mark_found();
    }
    let vector_outbox_next = vector_outbox_outcome.next_cursor;
    VECTOR_INGEST_OUTBOX_CURSOR.set_next(vector_outbox_next);

    // ADR 0030 slice 9: drive a bounded slice of the constraint catalog through the drop-drain
    // driver (Driver 3) on the same tick, with its own round-robin cursor. Best-effort: a constraint
    // whose reservations/effects have not fully drained is held for the next lap.
    let drop_start = CONSTRAINT_DROP_CURSOR.start();
    let (drop_next, drop_found) = crate::constraint_drop::run_constraint_drop_pass(
        drop_start,
        CONSTRAINT_DROP_SCAN_BUDGET,
        ic_cdk::api::time(),
    )
    .await;
    if drop_found {
        CONSTRAINT_DROP_CURSOR.mark_found();
    }
    CONSTRAINT_DROP_CURSOR.set_next(drop_next);

    // ADR 0023 D6: drive a bounded slice of the retired physical posting namespaces
    // through the retirement drain driver (Driver 4) on the same tick. Best-effort: an
    // unreachable index canister holds its record with its durable resume cursor for the
    // next lap.
    let retirement_start = INDEX_RETIREMENT_CURSOR.start();
    let (retirement_next, retirement_found) = crate::index_retirement::run_index_retirement_pass(
        retirement_start,
        INDEX_RETIREMENT_SCAN_BUDGET,
    )
    .await;
    if retirement_found {
        INDEX_RETIREMENT_CURSOR.mark_found();
    }
    INDEX_RETIREMENT_CURSOR.set_next(retirement_next);

    // Advance the cursor. A short scan (fewer than the budget) means we reached the end of
    // the keyspace, so reset to start a fresh lap next time.
    let lap_complete = scanned < RECOVERY_SCAN_BUDGET as u32;
    let next_cursor = if lap_complete { None } else { last_examined };
    RECOVERY_CURSOR.set_next(next_cursor.clone());

    if bulk_gc_pending
        || RECOVERY_CURSOR.pending()
        || RECLAIM_CURSOR.pending()
        || EFFECT_CURSOR.pending()
        || VECTOR_INGEST_OUTBOX_CURSOR.pending()
        || CONSTRAINT_DROP_CURSOR.pending()
        || INDEX_RETIREMENT_CURSOR.pending()
    {
        // Mid-lap on any driver: keep scanning promptly.
        return Some(RECOVERY_FLOOR_DELAY);
    }
    // All laps complete: start another (backed-off) lap only if the just-finished lap found work on
    // any driver (accumulated across all pages).
    if RECOVERY_CURSOR.found()
        || RECLAIM_CURSOR.found()
        || EFFECT_CURSOR.found()
        || VECTOR_INGEST_OUTBOX_CURSOR.found()
        || CONSTRAINT_DROP_CURSOR.found()
        || INDEX_RETIREMENT_CURSOR.found()
    {
        return Some(RECOVERY_RELAXED_DELAY);
    }
    // No urgent work, but a quarantined effect row is parked behind a backoff: re-arm a one-shot
    // timer for its deadline so the keyspace is re-checked rather than going dark until the next
    // mutation. Floored so a just-passed deadline still backs off a tick.
    if let Some(wake) = EFFECT_CURSOR.wake() {
        let remaining = wake.saturating_sub(ic_cdk::api::time());
        return Some(core::time::Duration::from_nanos(remaining).max(RECOVERY_RELAXED_DELAY));
    }
    // Nothing outstanding: stop and let the next mutation re-arm.
    None
}

#[cfg(test)]
mod tests {
    use super::{
        LaneCursor, RECOVERY_FLOOR_DELAY, RECOVERY_SCHEDULER, RecoverySchedulerState,
        arm_if_needed, recovery_schedule_delay,
    };
    use crate::facade::stable::vector_ingest_outbox;
    use candid::Principal;
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::{LocalVertexId, ShardId};

    #[test]
    fn lane_cursor_owns_cursor_found_and_wake() {
        let lane = LaneCursor::<u64>::new();
        assert_eq!(lane.start(), None);
        lane.mark_found();
        lane.note_wake(10);
        lane.note_wake(7);
        lane.set_next(Some(5));
        assert!(lane.pending());
        assert!(lane.found());
        assert_eq!(lane.wake(), Some(7));
        assert_eq!(lane.start(), Some(5));
        assert!(lane.found(), "a continuing lap keeps its found flag");
        lane.set_next(None);
        assert_eq!(lane.start(), None);
        assert!(!lane.found(), "a fresh lap clears the old lap's flag");
        assert_eq!(lane.wake(), None);
    }

    use gleaph_graph_kernel::vector_index::{
        IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
    };

    fn intent(
        mutation_id: u64,
        vertex_id: u32,
        vector_target: Principal,
    ) -> vector_ingest_outbox::VectorIngestOutboxState {
        vector_ingest_outbox::intent_for_test(
            vector_ingest_outbox::NewVectorIngestIntent {
                graph_id: GraphId::from_raw(1),
                graph_target: Principal::from_slice(&[8; 29]),
                vector_target,
                shard_id: ShardId::new(2),
                local_vertex_id: LocalVertexId::from(vertex_id),
                spec: IndexedEmbeddingSpec {
                    embedding_name_id: 3,
                    index_id: 7,
                    kind: VectorIndexKind::IvfFlat,
                    encoding: VectorEncoding::F32,
                    dims: 1,
                    metric: VectorMetric::L2Squared,
                    labels: vec![VertexLabelId::from_raw(1)],
                },
                bytes: vec![vertex_id as u8, 0, 0, 0],
            },
            mutation_id,
            vector_ingest_outbox::VectorIngestIntentPhase::AwaitingVector,
        )
    }

    #[test]
    fn arm_requested_preserves_wake_after_empty_pass() {
        assert_eq!(
            recovery_schedule_delay(None, true),
            Some(RECOVERY_FLOOR_DELAY),
            "an arm request raised during the pass must schedule the floor delay"
        );
        assert_eq!(
            recovery_schedule_delay(None, false),
            None,
            "an empty pass without a concurrent arm request must stop"
        );
    }

    #[test]
    fn append_after_idle_arms_and_running_append_latches_next_pass() {
        let _guard = vector_ingest_outbox::test_lock();
        vector_ingest_outbox::clear_for_test();
        RECOVERY_SCHEDULER
            .with_borrow_mut(|scheduler| *scheduler = RecoverySchedulerState::default());

        let target = Principal::from_slice(&[7; 29]);
        let first = intent(101, 1, target);
        vector_ingest_outbox::insert_intents_for_test(std::slice::from_ref(&first))
            .expect("append durable work while recovery is idle");

        // The production direct-ingestion owner calls arm_if_needed immediately after this append.
        arm_if_needed();
        assert_eq!(
            RECOVERY_SCHEDULER.with_borrow(|scheduler| *scheduler),
            RecoverySchedulerState {
                timer_armed: true,
                running: false,
                arm_requested: false,
            }
        );
        arm_if_needed();
        assert_eq!(
            RECOVERY_SCHEDULER.with_borrow(|scheduler| *scheduler),
            RecoverySchedulerState {
                timer_armed: true,
                running: false,
                arm_requested: false,
            },
            "a second arm while idle must not schedule another pass"
        );

        RECOVERY_SCHEDULER.with_borrow_mut(RecoverySchedulerState::begin_pass);
        let second = intent(102, 2, target);
        vector_ingest_outbox::insert_intents_for_test(std::slice::from_ref(&second))
            .expect("append durable work while recovery is running");
        arm_if_needed();
        assert_eq!(
            RECOVERY_SCHEDULER.with_borrow(|scheduler| *scheduler),
            RecoverySchedulerState {
                timer_armed: false,
                running: true,
                arm_requested: true,
            },
            "an append during the await must latch a follow-up pass"
        );

        let next = RECOVERY_SCHEDULER.with_borrow_mut(|scheduler| scheduler.finish_pass(None));
        assert_eq!(next, Some(RECOVERY_FLOOR_DELAY));
        assert_eq!(
            RECOVERY_SCHEDULER.with_borrow(|scheduler| *scheduler),
            RecoverySchedulerState {
                timer_armed: true,
                running: false,
                arm_requested: false,
            },
            "an empty pass must preserve the latched wake"
        );

        vector_ingest_outbox::clear_for_test();
        RECOVERY_SCHEDULER
            .with_borrow_mut(|scheduler| *scheduler = RecoverySchedulerState::default());
    }
}
