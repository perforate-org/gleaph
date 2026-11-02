//! Shared instruction-budget policy, cutoff predicate, and measured per-operation cost learning.
//!
//! This crate is the instruction-dimension counterpart of `gleaph-message-sizing`: it owns the
//! platform instruction ceilings, the pure cutoff predicate, the static pre-dispatch admission
//! guard, and the per-operation cost tracker used by canister batch execution. It depends on
//! nothing in the canister stack except `ic-cdk`, which sits behind the default-on `ic-cdk`
//! feature so that pure-kernel consumers such as `gleaph-graph-kernel` can reference the
//! constants and predicates with `default-features = false`.

/// ICP's per-message instruction ceiling for update calls, heartbeats, and timers.
pub const MAX_UPDATE_CALL_INSTRUCTIONS: u64 = 40_000_000_000;

/// Conservative dynamic query budget used by bounded Graph query execution.
pub const MAX_QUERY_CALL_INSTRUCTIONS: u64 = 5_000_000_000;

/// Instruction headroom reserved below the update-call ceiling for final response and
/// bookkeeping work. Dynamic update budgets must be derived from this value.
pub const UPDATE_CALL_INSTRUCTION_HEADROOM: u64 = 5_000_000_000;

/// Safe dynamic update budget after reserving [`UPDATE_CALL_INSTRUCTION_HEADROOM`].
pub const MAX_DYNAMIC_UPDATE_INSTRUCTIONS: u64 =
    MAX_UPDATE_CALL_INSTRUCTIONS - UPDATE_CALL_INSTRUCTION_HEADROOM;

/// Conservative cost estimate used only for pre-dispatch batch sizing when an endpoint has no
/// resumable per-operation cursor. Measured operation cost remains authoritative for continuation
/// paths.
pub const GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION: u64 = 500_000_000;

/// Headroom reserved by Graph's dynamic batch cutoff for final bookkeeping.
pub const GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM: u64 = 2_000_000_000;

/// Headroom reserved by Router's dynamic batch cutoff for dispatch and finalization.
///
/// Consumed by the Router between-chunk guard (`graph_batch_chunk_within_update_budget`,
/// GAP-2026-08-04-001): dispatching another Graph batch chunk must leave this much of the
/// 40B update ceiling for chunk-loop finalization, journal reads, token merge, and response
/// encoding.
pub const ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM: u64 = 4_000_000_000;

/// Measured Router-side work to decide, encode-probe, and merge one dynamic Graph batch
/// chunk (GAP-2026-07-17-001 canbench evidence: worst decision cost 37.34M instructions,
/// ~34% margin). Consumed by `graph_batch_chunk_within_update_budget`.
pub const ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE: u64 = 50_000_000;

/// One derived-state family that drains outbox work with its own cost profile.
///
/// Planes differ in what one operation costs: rekeying a posting is far cheaper than rebuilding a
/// text posting, and a vector sync carries its own batch protocol. A page bound therefore belongs to
/// the plane, never to the producer that filled the outbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plane {
    /// Derived index postings: rekey one edge from its old ordinal to its new one.
    Index,
    /// Text inverted index: postings, possibly rebuilt per operation.
    Text,
    /// Vector store: batched sync with its own retry protocol.
    Vector,
    /// Unique-index effects delivered through their own outbox.
    UniqueEffect,
}

/// The instruction budget one plane may spend applying a page in a single message.
///
/// Derived from the update-call ceiling minus the shared headroom, so a plane that is also doing
/// its own bookkeeping in that message still has room to finish and return.
pub const fn plane_budget(plane: Plane) -> u64 {
    let _ = plane;
    MAX_UPDATE_CALL_INSTRUCTIONS - UPDATE_CALL_INSTRUCTION_HEADROOM
}

/// Declared worst-case instruction cost of applying one operation in `plane`.
///
/// These are **declared ceilings**, not measurements: a page's observed cost replaces them at
/// runtime, and never exceeds them. Raising one is a claim that the plane's worst case grew, and
/// that claim belongs in the same review as the code that made it grow.
pub const fn plane_op_estimate(plane: Plane) -> u64 {
    match plane {
        Plane::Index => 320_000_000,
        Plane::Text => 3_500_000_000,
        Plane::Vector => 1_000_000_000,
        Plane::UniqueEffect => 100_000_000,
    }
}

/// Operations a plane may take in one page before measurement refines the number.
///
/// The consumer derives its own constant from this, and a test asserts the quotient, so the page
/// size is a reviewed number rather than a literal someone guessed.
pub const fn initial_ops_per_page(plane: Plane) -> u32 {
    let ops = plane_budget(plane) / plane_op_estimate(plane);
    if ops > u32::MAX as u64 {
        u32::MAX
    } else {
        ops as u32
    }
}

/// Conservative per-message cap for Graph's deferred LARA maintenance timer.
pub const MAX_TIMER_MAINTENANCE_INSTRUCTIONS: u64 = 32_000_000_000;

/// Reserved instruction headroom inside the timer maintenance cap.
pub const TIMER_MAINTENANCE_INSTRUCTION_HEADROOM: u64 = 100_000_000;

const _: () = {
    assert!(MAX_QUERY_CALL_INSTRUCTIONS < MAX_UPDATE_CALL_INSTRUCTIONS);
    assert!(GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM < MAX_UPDATE_CALL_INSTRUCTIONS);
    assert!(ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM < MAX_UPDATE_CALL_INSTRUCTIONS);
    assert!(TIMER_MAINTENANCE_INSTRUCTION_HEADROOM < MAX_TIMER_MAINTENANCE_INSTRUCTIONS);
    assert!(MAX_TIMER_MAINTENANCE_INSTRUCTIONS < MAX_UPDATE_CALL_INSTRUCTIONS);
};

/// Largest operation count that fits `safe_budget` at `estimate_per_operation`.
///
/// This is the single derivation that turns the budget constants into a per-call operation
/// ceiling. A zero estimate is treated as "each operation costs nothing" and never divides by
/// zero.
pub fn max_operation_count(estimate_per_operation: u64, safe_budget: u64) -> usize {
    usize::try_from(safe_budget / estimate_per_operation.max(1)).unwrap_or(usize::MAX)
}

/// Static pre-dispatch admission guard for endpoints without a resumable per-operation cursor.
///
/// Returns [`BudgetError::ExceedsBudget`] when `operation_count` exceeds
/// [`max_operation_count`]. Measured operation cost remains authoritative for continuation
/// paths; this guard is the conservative pre-dispatch check only.
pub fn preflight_operation_count(
    operation_count: usize,
    estimate_per_operation: u64,
    safe_budget: u64,
) -> Result<(), BudgetError> {
    let max = max_operation_count(estimate_per_operation, safe_budget);
    if operation_count > max {
        return Err(BudgetError::ExceedsBudget {
            operation_count,
            estimate_per_operation,
            safe_budget,
        });
    }
    Ok(())
}

/// Why a static operation-count preflight rejected a candidate batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetError {
    /// The count times the per-operation estimate exceeds the safe budget.
    ExceedsBudget {
        operation_count: usize,
        estimate_per_operation: u64,
        safe_budget: u64,
    },
}

/// Returns true when starting the next operation would risk exceeding `ceiling` after the
/// estimated next-operation cost and the reserved response/drain work.
///
/// `used` is the caller's current instruction count for the relevant scope (per-message or
/// call-context); callers read it with [`instruction_counter`] or
/// [`call_context_instruction_counter`]. `next_op_estimate` is the measured per-operation cost
/// (see [`OpCostTracker`]); callers without a measured estimate pass the conservative
/// per-operation constant. `response_reserve` and `drain_reserve` cover final bookkeeping and
/// post-batch derived work that must complete in the same call. The predicate saturates so an
/// overflowing projection always cuts off.
pub fn should_cutoff(
    ceiling: u64,
    used: u64,
    next_op_estimate: u64,
    response_reserve: u64,
    drain_reserve: u64,
) -> bool {
    used.saturating_add(next_op_estimate)
        .saturating_add(response_reserve)
        .saturating_add(drain_reserve)
        >= ceiling
}

/// Measured per-operation instruction cost learning for resumable batch execution.
///
/// `next_op_estimate` returns the largest cost observed so far, or `fallback_estimate` before the
/// first observation. A tracker is shape-specific caller-owned optimization state; it is never
/// correctness state and must not be shared across unrelated operation shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpCostTracker {
    fallback_estimate: u64,
    max_seen: u64,
}

impl OpCostTracker {
    pub const fn new(fallback_estimate: u64) -> Self {
        Self {
            fallback_estimate,
            max_seen: 0,
        }
    }

    /// Record one completed operation's measured cost.
    pub fn observe(&mut self, cost: u64) {
        self.max_seen = self.max_seen.max(cost);
    }

    /// Conservative estimate for the next operation: the largest measured cost, or the fallback
    /// before any observation.
    pub fn next_op_estimate(&self) -> u64 {
        self.max_seen.max(self.fallback_estimate)
    }

    /// Largest measured per-operation cost so far (0 before any observation).
    pub fn max_seen(&self) -> u64 {
        self.max_seen
    }
}

/// Per-message instruction counter for the current canister call.
///
/// Wasm canister builds read `ic_cdk`'s per-message counter; host builds (unit tests) return 0.
#[cfg(feature = "ic-cdk")]
pub fn instruction_counter() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::instruction_counter()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

/// Call-context instruction counter for the current canister ingress.
///
/// Wasm canister builds read `ic_cdk`'s call-context counter; host builds (unit tests) return 0.
#[cfg(feature = "ic-cdk")]
pub fn call_context_instruction_counter() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::call_context_instruction_counter()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn every_plane_fits_at_least_one_operation_in_a_page() {
        for plane in [
            Plane::Index,
            Plane::Text,
            Plane::Vector,
            Plane::UniqueEffect,
        ] {
            assert!(
                plane_op_estimate(plane) <= plane_budget(plane),
                "{plane:?} cannot fit a single operation in its own budget"
            );
            assert!(
                initial_ops_per_page(plane) >= 1,
                "{plane:?} must start at one operation, not zero"
            );
            assert_eq!(
                initial_ops_per_page(plane),
                (plane_budget(plane) / plane_op_estimate(plane)) as u32,
                "the page size is the reviewed quotient, not a guessed literal"
            );
        }
    }

    #[test]
    fn the_text_plane_takes_fewer_operations_than_the_index_plane() {
        // A text posting rebuild is an order of magnitude dearer than a posting rekey, which is the
        // whole reason a page bound belongs to the plane rather than to the producer.
        assert!(initial_ops_per_page(Plane::Text) < initial_ops_per_page(Plane::Index));
    }
    use super::*;

    #[test]
    fn dynamic_update_budget_leaves_shared_headroom() {
        assert_eq!(
            MAX_DYNAMIC_UPDATE_INSTRUCTIONS + UPDATE_CALL_INSTRUCTION_HEADROOM,
            MAX_UPDATE_CALL_INSTRUCTIONS
        );
    }

    #[test]
    fn preflight_derives_the_shared_operation_bound() {
        let max = max_operation_count(
            GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION,
            MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
        );
        assert_eq!(
            max as u64,
            MAX_DYNAMIC_UPDATE_INSTRUCTIONS / GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION
        );
        assert!(
            preflight_operation_count(
                max,
                GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION,
                MAX_DYNAMIC_UPDATE_INSTRUCTIONS
            )
            .is_ok()
        );
        assert_eq!(
            preflight_operation_count(
                max + 1,
                GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION,
                MAX_DYNAMIC_UPDATE_INSTRUCTIONS
            ),
            Err(BudgetError::ExceedsBudget {
                operation_count: max + 1,
                estimate_per_operation: GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION,
                safe_budget: MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
            })
        );
    }

    #[test]
    fn zero_estimate_preflight_never_divides_by_zero() {
        assert!(preflight_operation_count(1, 0, MAX_DYNAMIC_UPDATE_INSTRUCTIONS).is_ok());
    }

    #[test]
    fn should_cutoff_fires_at_the_projected_ceiling() {
        assert!(!should_cutoff(100, 20, 30, 40, 9));
        assert!(should_cutoff(100, 20, 30, 40, 10));
        assert!(should_cutoff(100, 20, 30, 40, 11));
        assert!(should_cutoff(100, 100, 0, 0, 0));
    }

    #[test]
    fn should_cutoff_saturates_instead_of_wrapping() {
        assert!(should_cutoff(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX
        ));
    }

    #[test]
    fn op_cost_tracker_uses_fallback_until_an_observation() {
        let mut tracker = OpCostTracker::new(50);
        assert_eq!(tracker.next_op_estimate(), 50);
        assert_eq!(tracker.max_seen(), 0);
        tracker.observe(10);
        assert_eq!(tracker.next_op_estimate(), 50);
        tracker.observe(80);
        assert_eq!(tracker.next_op_estimate(), 80);
        assert_eq!(tracker.max_seen(), 80);
    }
}
