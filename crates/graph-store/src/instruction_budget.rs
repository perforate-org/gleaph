//! Bounded work using IC instruction counts on wasm32; no-op on native builds.
//!
//! ## Measuring instruction cost for the periodic maintenance timer (wasm32)
//!
//! 1. In the canister timer closure (or a thin test hook), call
//!    `ic_cdk::api::performance_counter(PerformanceCounterType::InstructionCounter)` immediately
//!    before and after the sequence you want to bound: graph
//!    [`gleaph_graph_store::integration::GraphStoreKernelOverlayGraph::graph_maintenance_timer_tick_bounded`],
//!    optional [`gleaph_graph_store::integration::GraphStoreKernelOverlayGraph::property_maintenance_flush_step_bounded`],
//!    then [`gleaph_graph_store::integration::GraphStoreKernelOverlayGraph::vacuum_step`].
//! 2. Subtract end − start for the delta (instructions).
//! 3. Compare against subnet per-message limits and set
//!    [`DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_MARGIN`],
//!    [`DEFAULT_PROPERTY_FLUSH_INSTRUCTION_MARGIN`], and the corresponding budgets so the sum of
//!    drain + property flush + vacuum stays safely under the envelope you measured.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Approximate per-message instruction ceiling on IC subnets (order of 40B). Not an API guarantee;
/// tune using canbench / staging `performance_counter` on the full timer path
/// (`graph_maintenance_timer_tick_bounded` + optional property flush + `vacuum_step`).
pub const IC_PER_MESSAGE_INSTRUCTION_BUDGET_HINT: u64 = 40_000_000_000;

/// Safety margin subtracted from [`IC_PER_MESSAGE_INSTRUCTION_BUDGET_HINT`] for drain work alone.
/// After measuring end-to-end tick cost, adjust this (or replace with an explicit limit from data).
pub const DEFAULT_MAINTENANCE_DRAIN_INSTRUCTION_MARGIN: u64 = 50_000_000;

/// Safety margin for [`DEFAULT_PROPERTY_FLUSH_INSTRUCTION_BUDGET`] (separate from drain).
pub const DEFAULT_PROPERTY_FLUSH_INSTRUCTION_MARGIN: u64 = 50_000_000;

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
