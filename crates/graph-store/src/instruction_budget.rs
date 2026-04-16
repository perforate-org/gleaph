//! Bounded work using IC instruction counts on wasm32; no-op on native builds.
//!
//! ## Measuring instruction cost for the periodic maintenance timer (wasm32)
//!
//! The canister wires a `performance_counter` around the full maintenance callback (graph tick +
//! optional property flush + vacuum + stable flush). When
//! `LOG_MAINTENANCE_TICK_INSTRUCTION_METRICS` is enabled in `gleaph-graph`, each invocation logs:
//!
//! ```text
//! [gleaph-graph] maintenance_tick_instructions total=T drain=D property_flush=P
//! ```
//!
//! - **`total`**: whole timer callback including `flush_graph_stable_full` (not just drain).
//! - **`drain`**: `instructions_used` from [`GraphStoreMaintenanceDirtyDrainSummary`](crate::facade::GraphStoreMaintenanceDirtyDrainSummary)
//!   for the graph tick's drain step (0 if the tick failed before drain finished).
//! - **`property_flush`**: `InstructionBudget::used()` for the property flush attempt when backlog
//!   was non-empty.
//!
//! **Wasm canbench (gleaph-graph, `canbench-rs`, release wasm):** Person ring 256, same tick args as
//! the benches. One representative run: **`bench_graph_maintenance_timer_tick_idle_person_ring_256`**
//! total **~507K** instructions (`graph_maint_tick_drain` **~25K**, `graph_maint_tick_queued`
//! **~440K**) — idle dirty btree means almost all tick work is the **queued** phase, not drain.
//! **`bench_graph_maintenance_timer_tick_dirty_person_ring_256`** (merge `[100, 200)` each sample)
//! total **~3.13M** (`graph_maint_tick_drain` **~1.16M**, `graph_maint_tick_queued` **~1.92M**).
//! Rebuild wasm before comparing deltas. Scopes live in
//! [`crate::integration::GraphStoreKernelOverlayGraph::graph_maintenance_timer_tick_bounded`]
//! (`graph_maint_tick_drain` / `graph_maint_tick_queued`); the gleaph-graph bench adds
//! `graph_maintenance_timer_tick`. Note: [`DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_BUDGET`] applies to
//! the drain path only — queued maintenance is not instruction-capped by that budget.
//!
//! DML terminal flush (`gql_exec_plan_flush` in gleaph-graph) includes stable maintenance-dirty
//! ordinal merges under **`pma_maint_dirty_ordinal_note`** (after `pma_graph_refresh_write`, still
//! inside graph-store `canbench-rs`); compare that scope when insert/mutation benches shift without
//! `pma_graph_refresh_write` moving.
//!
//! **Tuning margins (after collecting peaks on staging, e.g. busy graph + property dirty):**
//!
//! 1. Record high `D` with `budget_exhausted == false` in drain summary; if drain often exhausts,
//!    raise [`DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_BUDGET`] by lowering
//!    [`DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_MARGIN`], or raise the explicit drain limit directly.
//! 2. Record high `P`; size [`DEFAULT_PROPERTY_FLUSH_INSTRUCTION_BUDGET`] the same way via
//!    [`DEFAULT_PROPERTY_FLUSH_INSTRUCTION_MARGIN`].
//! 3. Ensure `T` stays safely below the subnet per-message instruction ceiling (order of 40B).
//!    If `T` is high, reduce per-tick work (`max_vertices_for_tick`, `max_maintenance_cycles`,
//!    vacuum ops) before shrinking margins.
//!
//! Default margins below reserve multi‑billion instruction headroom under the 40B hint so queued
//! maintenance, vacuum, and stable flush in the same callback are less likely to contend with drain
//! or property budgets; tighten from real `T`/`D`/`P` logs when available.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Approximate per-message instruction ceiling on IC subnets (order of 40B). Not an API guarantee;
/// tune using canbench / staging `performance_counter` on the full timer path
/// (`graph_maintenance_timer_tick_bounded` + optional property flush + `vacuum_step`).
pub const IC_PER_MESSAGE_INSTRUCTION_BUDGET_HINT: u64 = 40_000_000_000;

/// Safety margin subtracted from [`IC_PER_MESSAGE_INSTRUCTION_BUDGET_HINT`] for drain work alone.
/// Tuned with staging `maintenance_tick_instructions` logs (`drain=` / `total=`).
pub const DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_MARGIN: u64 = 2_000_000_000;

/// Safety margin for [`DEFAULT_PROPERTY_FLUSH_INSTRUCTION_BUDGET`] (separate from drain).
/// Tuned with staging `maintenance_tick_instructions` logs (`property_flush=` / `total=`).
pub const DEFAULT_PROPERTY_FLUSH_INSTRUCTION_MARGIN: u64 = 1_000_000_000;

/// Default instruction budget passed into [`crate::facade::GraphStore::drain_maintenance_dirty_into_queue_at_epoch_with_budget`]
/// from the canister timer.
pub const DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_BUDGET: u64 = IC_PER_MESSAGE_INSTRUCTION_BUDGET_HINT
    .saturating_sub(DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_MARGIN);

/// Instruction budget for [`gleaph_graph_store::integration::GraphStoreKernelOverlayGraph::property_maintenance_flush_step_bounded`]
/// on the canister timer (separate from graph drain).
pub const DEFAULT_PROPERTY_FLUSH_INSTRUCTION_BUDGET: u64 = IC_PER_MESSAGE_INSTRUCTION_BUDGET_HINT
    .saturating_sub(DEFAULT_PROPERTY_FLUSH_INSTRUCTION_MARGIN);

/// Tracks instruction consumption against a limit (wasm), or is inert (native release).
#[derive(Debug)]
pub struct InstructionBudget {
    baseline: u64,
    limit: u64,
    checkpoint_tick: usize,
    #[cfg(not(target_arch = "wasm32"))]
    test_counter: Option<Arc<AtomicU64>>,
}

#[cfg(target_arch = "wasm32")]
#[inline]
fn read_instruction_counter() -> u64 {
    ic_cdk::api::performance_counter(ic_cdk::api::PerformanceCounterType::InstructionCounter)
}

impl InstructionBudget {
    /// Captures the current instruction count as baseline; `exhausted` becomes true once
    /// `used() >= limit` (wasm). Native release: counter reads as zero → never exhausted unless
    /// `limit == 0`.
    pub fn new(limit: u64) -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            Self {
                baseline: read_instruction_counter(),
                limit,
                checkpoint_tick: 0,
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self {
                baseline: 0,
                limit,
                checkpoint_tick: 0,
                test_counter: None,
            }
        }
    }

    /// Test-only: `used()` advances when the atomic is incremented externally.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub fn new_for_test(counter: Arc<AtomicU64>, limit: u64) -> Self {
        let baseline = counter.load(Ordering::Relaxed);
        Self {
            baseline,
            limit,
            checkpoint_tick: 0,
            test_counter: Some(counter),
        }
    }

    #[inline]
    fn read_raw(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        {
            read_instruction_counter()
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            match &self.test_counter {
                // Each read advances the counter so tests can simulate instruction growth.
                Some(c) => c.fetch_add(1, Ordering::Relaxed),
                None => 0,
            }
        }
    }

    #[inline]
    pub fn used(&self) -> u64 {
        self.read_raw().saturating_sub(self.baseline)
    }

    #[inline]
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used())
    }

    #[inline]
    pub fn exhausted(&self) -> bool {
        self.used() >= self.limit
    }

    /// Returns true every `n` invocations (and on the first call when `n <= 1`) so callers can
    /// amortize `exhausted()` / `used()` syscalls.
    pub fn checkpoint_every(&mut self, n: usize) -> bool {
        if n <= 1 {
            return true;
        }
        self.checkpoint_tick = self.checkpoint_tick.wrapping_add(1);
        self.checkpoint_tick % n == 0
    }
}
