//! Autonomous expired-grant retention GC driver ([ADR 0083] §3).
//!
//! A self-rescheduling one-shot timer (`ic-cdk-timers`) that bounds the grant store's
//! expired-row tail: every tick runs one bounded [`gleaph_auth::GrantState::sweep_expired_rows`]
//! step — a canonical-order walk behind a heap-resident resume cursor that removes rows
//! whose `expires_at` passed the constant review window
//! ([`EXPIRED_ROW_RETENTION_NS`][gleaph_auth::EXPIRED_ROW_RETENTION_NS]). A large backlog
//! drains over successive ticks instead of one long call.
//!
//! Arming mirrors `crate::recovery::arm_if_needed`: idempotent and self-guarding, called
//! from the canister lifecycle hooks (`init` / `post_upgrade`), since timers do not
//! survive an upgrade. The cursor is heap-resident, so an upgrade restarts the walk from
//! the beginning of the keyspace — safe because a pass is idempotent ([ADR 0083] §3).
//!
//! Unlike the work-driven recovery driver, this timer is an **always-on heartbeat**: a
//! completed lap reschedules itself at daily-scale delay, while a mid-lap tick continues
//! promptly. Enforcement reads rows through expiry-aware `holds`, so sweeping
//! already-absent rows changes no verdict; the review surfaces (`list_elevations` /
//! `list_graph_grants`) are untouched code.

/// Grant keys examined per retention tick. Bounds the per-tick instruction cost; the
/// heap-resident cursor resumes the walk on the next tick so any backlog is still fully
/// covered ([ADR 0083] §3 bounded-per-tick invariant).
#[cfg(target_family = "wasm")]
const RETENTION_SCAN_BUDGET: usize = 16;

/// Delay between ticks while a sweep lap is still in progress (backlog draining).
#[cfg(any(target_family = "wasm", test))]
const RETENTION_LAP_DELAY: core::time::Duration = core::time::Duration::from_secs(2);

/// Delay before the next autonomous lap once the previous lap completed (daily scale,
/// [ADR 0083] §3 low-frequency GC).
#[cfg(any(target_family = "wasm", test))]
const RETENTION_IDLE_DELAY: core::time::Duration = core::time::Duration::from_secs(24 * 60 * 60);

/// Logical armed/idle owner of the retention timer, independent of its `TimerId`
/// handle. The same transition serves the wasm timer and the native unit seam, so
/// arming idempotence is covered without sleeping for an IC timer.
#[cfg(any(target_family = "wasm", test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RetentionSchedulerState {
    timer_armed: bool,
}

#[cfg(any(target_family = "wasm", test))]
impl RetentionSchedulerState {
    /// Arms iff idle; returns whether an IC timer must be scheduled. Repeat calls while
    /// armed are absorbed (idempotence): init and post_upgrade both arm unconditionally,
    /// and only one timer may exist.
    fn request_arm(&mut self) -> bool {
        if self.timer_armed {
            return false;
        }
        self.timer_armed = true;
        true
    }

    /// Consumes the armed flag at tick start; the tick end decides the re-arm delay.
    fn begin_tick(&mut self) {
        assert!(self.timer_armed, "retention tick without an armed timer");
        self.timer_armed = false;
    }

    /// Re-arms at the tick end ([ADR 0083] §3 autonomous heartbeat): prompt continuation
    /// while the lap is still draining (`mid_lap`), daily-scale delay once the lap
    /// completed. Returns the delay the caller must schedule.
    fn finish_tick(&mut self, mid_lap: bool) -> core::time::Duration {
        assert!(
            !self.timer_armed,
            "retention tick end while the timer was never consumed"
        );
        self.timer_armed = true;
        if mid_lap {
            RETENTION_LAP_DELAY
        } else {
            RETENTION_IDLE_DELAY
        }
    }
}

#[cfg(any(target_family = "wasm", test))]
thread_local! {
    static RETENTION_SCHEDULER: std::cell::RefCell<RetentionSchedulerState> =
        const { std::cell::RefCell::new(RetentionSchedulerState { timer_armed: false }) };
}

#[cfg(target_family = "wasm")]
thread_local! {
    /// The single in-flight retention timer, or `None` when idle. Rebuilt after upgrade.
    static RETENTION_TIMER: std::cell::RefCell<Option<ic_cdk_timers::TimerId>> =
        const { std::cell::RefCell::new(None) };
    /// Heap-resident sweep cursor ([ADR 0083] §3). `Some(key)` resumes strictly after
    /// that canonical key; `None` starts a fresh lap from the beginning. Lost on upgrade
    /// by design: a restart re-derives the same removals.
    static RETENTION_CURSOR: std::cell::RefCell<Option<gleaph_auth::GrantKey>> =
        const { std::cell::RefCell::new(None) };
}

/// Schedules the retention timer iff one is not already armed. Idempotent and
/// self-guarding; safe to call from both lifecycle hooks. A no-op on non-wasm builds,
/// where there is no timer runtime.
pub(crate) fn arm_if_needed() {
    #[cfg(any(target_family = "wasm", test))]
    {
        let schedule = RETENTION_SCHEDULER.with_borrow_mut(|scheduler| scheduler.request_arm());
        if schedule {
            #[cfg(target_family = "wasm")]
            {
                let timer_id = schedule_tick(RETENTION_LAP_DELAY);
                RETENTION_TIMER.with_borrow_mut(|slot| *slot = Some(timer_id));
            }
        }
    }
}

#[cfg(target_family = "wasm")]
fn schedule_tick(delay: core::time::Duration) -> ic_cdk_timers::TimerId {
    // The pass body never awaits: it is one local stable-memory slice, so the async
    // wrapper completes within its first poll and cannot interleave with grant writes.
    ic_cdk_timers::set_timer(delay, on_tick())
}

#[cfg(target_family = "wasm")]
async fn on_tick() {
    RETENTION_TIMER.with_borrow_mut(|slot| *slot = None);
    RETENTION_SCHEDULER.with_borrow_mut(RetentionSchedulerState::begin_tick);

    let resume_after = RETENTION_CURSOR.with_borrow(Clone::clone);
    let step = crate::facade::auth::sweep_expired_retention_rows(
        ic_cdk::api::time(),
        RETENTION_SCAN_BUDGET,
        resume_after.as_ref(),
    );
    let mid_lap = step.resume_after.is_some();
    // Store the resume point verbatim; `None` resets the walk to the beginning of the
    // keyspace, which is safe after an upgrade because a pass is idempotent.
    RETENTION_CURSOR.with_borrow_mut(|cursor| *cursor = step.resume_after);

    let next_delay =
        RETENTION_SCHEDULER.with_borrow_mut(|scheduler| scheduler.finish_tick(mid_lap));
    let timer_id = schedule_tick(next_delay);
    RETENTION_TIMER.with_borrow_mut(|slot| *slot = Some(timer_id));
}

#[cfg(test)]
mod tests {
    use super::{RETENTION_IDLE_DELAY, RETENTION_LAP_DELAY, RetentionSchedulerState};

    #[test]
    fn arming_is_idempotent_across_lifecycle_hooks() {
        let mut scheduler = RetentionSchedulerState::default();
        // init arms...
        assert!(
            scheduler.request_arm(),
            "the first arm from idle must schedule a timer"
        );
        assert_eq!(scheduler, RetentionSchedulerState { timer_armed: true });
        // ...post_upgrade (or any other call site) must not stack a second timer.
        assert!(
            !scheduler.request_arm(),
            "an arm while already armed must be absorbed"
        );
        assert_eq!(scheduler, RetentionSchedulerState { timer_armed: true });
    }

    #[test]
    fn tick_generation_consumes_the_arm_and_rearms_exactly_once() {
        let mut scheduler = RetentionSchedulerState::default();
        assert!(scheduler.request_arm());
        scheduler.begin_tick();
        assert_eq!(
            scheduler,
            RetentionSchedulerState { timer_armed: false },
            "begin_tick consumed the armed flag"
        );
        // Mid-lap outcome: re-arm at the prompt lap delay, exactly one timer.
        assert_eq!(scheduler.finish_tick(true), RETENTION_LAP_DELAY);
        assert_eq!(scheduler, RetentionSchedulerState { timer_armed: true });
        assert!(
            !scheduler.request_arm(),
            "the tick's own re-arm is the only one"
        );
    }

    #[test]
    fn completed_lap_rearms_at_the_daily_scale_delay() {
        let mut scheduler = RetentionSchedulerState::default();
        assert!(scheduler.request_arm());
        scheduler.begin_tick();
        // Lap-complete outcome: the heartbeat waits out the daily-scale delay. The
        // heap cursor reset to `None` rides the same outcome, so the next lap starts
        // from the beginning of the keyspace — safe because a pass is idempotent
        // ([ADR 0083] §3).
        assert_eq!(scheduler.finish_tick(false), RETENTION_IDLE_DELAY);
        assert_eq!(RETENTION_IDLE_DELAY.as_secs(), 24 * 60 * 60);
        assert!(RETENTION_LAP_DELAY < RETENTION_IDLE_DELAY);
    }

    #[test]
    #[should_panic(expected = "retention tick without an armed timer")]
    fn tick_without_an_armed_timer_is_corrupt_state() {
        RetentionSchedulerState::default().begin_tick();
    }
}
