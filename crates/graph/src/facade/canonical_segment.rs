//! Path-independent guard for the canonical mutation segment (ADR 0091).
//!
//! The canonical segment commits canonical graph state and projection intent in
//! one IC message segment with no inter-canister call/commit point (ADR 0029 §1).
//! Before this guard existed, the only enforcement was "apply_canonical_mutation_segment
//! takes no PropertyIndexLookup handle and runs CALL procedures synchronously" — a
//! structural but not path-independent guarantee. A new inter-canister chokepoint
//! (peer-shard client, additional subgraph client, etc.) added inside the segment
//! would silently extend the critical section across a commit point.
//!
//! This module turns that guarantee into a path-independent runtime guard:
//! [`CanonicalSegmentGuard::enter`] must be held for the lifetime of any work that
//! must not perform inter-canister calls, and inter-canister chokepoint APIs must
//! invoke [`assert_no_canonical_segment`] to fail loudly if they are reached
//! during a guarded scope.
//!
//! The guard is a stack counter so a future legitimate nested read phase inside
//! the segment (none today) can call `enter()` again. A guard whose Drop leaves
//! the counter non-zero is an invariant violation and traps the whole message.

use std::cell::Cell;

thread_local! {
    static CANONICAL_SEGMENT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Read-only accessor for tests, debug assertions, and inter-canister chokepoint checks.
///
/// Production code paths that issue inter-canister calls must wrap the call site
/// with [`assert_no_canonical_segment`]. The result is a `u32` so a non-zero value
/// always means "a canonical segment is active".
#[inline]
pub fn canonical_segment_depth() -> u32 {
    CANONICAL_SEGMENT_DEPTH.with(Cell::get)
}

/// Assert that no canonical mutation segment is currently active.
///
/// Called at every inter-canister chokepoint (graph-index client lookup, future
/// peer-shard client, future Router call client). On violation, traps the entire
/// message so the canonical segment rolls back atomically (Property 5).
#[inline]
pub fn assert_no_canonical_segment(chokepoint: &'static str) {
    let depth = canonical_segment_depth();
    assert_eq!(
        depth, 0,
        "inter-canister call '{chokepoint}' reached inside canonical mutation segment (depth={depth})"
    );
}

/// RAII guard that marks the current call stack as inside a canonical mutation
/// segment.
///
/// Created via [`CanonicalSegmentGuard::enter`]. The guard increments a thread-local
/// depth counter on construction and decrements it on drop. A guard whose Drop
/// observes a depth that does not return to zero traps the message.
pub struct CanonicalSegmentGuard {
    _private: (),
}

impl CanonicalSegmentGuard {
    /// Enter a canonical mutation segment. Must be the first statement of any
    /// function that performs canonical writes and projection intent without
    /// inter-canister calls.
    ///
    /// Holds the guard until it goes out of scope.
    pub fn enter() -> Self {
        CANONICAL_SEGMENT_DEPTH.with(|depth| {
            let next = depth.get().saturating_add(1);
            depth.set(next);
        });
        Self { _private: () }
    }
}

impl Drop for CanonicalSegmentGuard {
    fn drop(&mut self) {
        CANONICAL_SEGMENT_DEPTH.with(|depth| {
            let current = depth.get();
            assert!(
                current > 0,
                "CanonicalSegmentGuard dropped without a matching enter (depth={current})"
            );
            depth.set(current - 1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::ManuallyDrop;
    use std::panic::{self, AssertUnwindSafe};

    #[test]
    fn guard_balances_on_normal_scope_exit() {
        assert_eq!(canonical_segment_depth(), 0);
        {
            let _g = CanonicalSegmentGuard::enter();
            assert_eq!(canonical_segment_depth(), 1);
        }
        assert_eq!(canonical_segment_depth(), 0);
    }

    #[test]
    fn assert_no_canonical_segment_passes_outside_guard() {
        assert_no_canonical_segment("test_outside");
    }

    #[test]
    #[should_panic(expected = "inter-canister call")]
    fn assert_no_canonical_segment_panics_inside_guard() {
        let _g = CanonicalSegmentGuard::enter();
        assert_no_canonical_segment("test_inside");
    }

    #[test]
    fn nested_enter_increments_depth() {
        let _outer = CanonicalSegmentGuard::enter();
        let _inner = CanonicalSegmentGuard::enter();
        assert_eq!(canonical_segment_depth(), 2);
    }

    /// Wrong-impl test for the `Drop`-balance trap. Driving a `Drop` to run
    /// with the counter already at zero must panic; this is the only public
    /// surface that exercises the `current > 0` check inside `Drop::drop`.
    /// A wrong implementation that decrements without the `current > 0`
    /// guard would silently wrap the counter to `u32::MAX`, leaving the next
    /// `assert_no_canonical_segment` call to see a non-zero depth and panic
    /// even though no guard is live — exactly the staleness that the
    /// `Drop`-balance trap is meant to catch.
    ///
    /// The double-drop is reached by constructing a `ManuallyDrop` wrapper,
    /// dropping the inner value once (the legitimate, balanced path), and
    /// then dropping the same memory a second time. The second drop sees a
    /// counter that is already zero and must trap. `catch_unwind` lets the
    /// test observe the trap without aborting the test process.
    #[test]
    fn double_drop_traps_when_depth_already_zero() {
        // Wrap a guard so we can drop the same memory twice.
        let mut md = ManuallyDrop::new(CanonicalSegmentGuard::enter());
        // First drop: the counter goes 1 -> 0. This is the balanced path.
        unsafe { ManuallyDrop::drop(&mut md) };
        // At this point the depth must be zero. A second drop on the same
        // memory sees `current == 0` and must trap on the `current > 0`
        // assertion. `AssertUnwindSafe` is required because the value behind
        // `&mut md` is no longer initialized in the type system, but we only
        // need the `Drop` impl to run.
        let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
            ManuallyDrop::drop(&mut md);
        }));
        let panic = result.expect_err("double-drop on a balanced guard must panic");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&'static str>().copied())
            .unwrap_or("");
        assert!(
            message.contains("matching enter") && message.contains("depth=0"),
            "double-drop panic must mention both the matching-enter trap and the depth, got: {message:?}"
        );
        assert_eq!(canonical_segment_depth(), 0);
    }
}

#[cfg(feature = "pocket-ic-e2e")]
pub mod e2e {
    //! Test-only seam for PocketIC E2E: the canonical segment path can be
    //! forced to invoke `assert_no_canonical_segment` from inside the segment
    //! so the whole-message rollback is observable end-to-end.
    //!
    //! Compiled only under `pocket-ic-e2e`; production builds are unaffected.

    use super::assert_no_canonical_segment;

    /// Simulate an inter-canister chokepoint reached from inside a canonical
    /// mutation segment. Traps the message in release builds; panics in host
    /// builds. PocketIC E2E asserts that the whole message rolls back and no
    /// partial canonical write survives.
    pub fn e2e_simulate_inter_canister_call_inside_segment() {
        assert_no_canonical_segment("e2e_simulate_inter_canister_call_inside_segment");
    }
}
