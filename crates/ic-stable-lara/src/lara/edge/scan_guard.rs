//! Test-only guard that records maintenance-store reads during labeled scan paths.

use std::cell::Cell;

thread_local! {
    static SCAN_PATH_GUARD_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static SPAN_META_READS_UNDER_SCAN_GUARD: Cell<u32> = const { Cell::new(0) };
    static FREE_SPAN_READS_UNDER_SCAN_GUARD: Cell<u32> = const { Cell::new(0) };
    static OVERFLOW_CHAIN_REBUILDS_UNDER_SCAN_GUARD: Cell<u32> = const { Cell::new(0) };
}

/// Activates scan-path read accounting until dropped.
pub struct ScanPathGuard;

impl ScanPathGuard {
    /// Starts accounting maintenance-store reads until this guard is dropped.
    pub fn enter() -> Self {
        SPAN_META_READS_UNDER_SCAN_GUARD.with(|c| c.set(0));
        FREE_SPAN_READS_UNDER_SCAN_GUARD.with(|c| c.set(0));
        OVERFLOW_CHAIN_REBUILDS_UNDER_SCAN_GUARD.with(|c| c.set(0));
        SCAN_PATH_GUARD_ACTIVE.with(|g| g.set(true));
        Self
    }

    /// Span metadata reads observed while the guard is active.
    pub fn span_meta_reads() -> u32 {
        SPAN_META_READS_UNDER_SCAN_GUARD.with(|c| c.get())
    }

    /// Edge free-span reads observed while the guard is active.
    pub fn free_span_reads() -> u32 {
        FREE_SPAN_READS_UNDER_SCAN_GUARD.with(|c| c.get())
    }

    /// Overflow-log chain rebuilds in the phase-2 selective edge read while the guard is active.
    ///
    /// Incremented only on the sparse fallback of `read_out_edge_slots_for_label_with_replay`; a
    /// reused hybrid replay returns before this point. Lets tests prove phase-2 replay reuse: a
    /// caller that reuses the phase-1 replay records `0`, the sparse fallback records `>= 1`.
    pub fn overflow_chain_rebuilds() -> u32 {
        OVERFLOW_CHAIN_REBUILDS_UNDER_SCAN_GUARD.with(|c| c.get())
    }
}

impl Drop for ScanPathGuard {
    fn drop(&mut self) {
        SCAN_PATH_GUARD_ACTIVE.with(|g| g.set(false));
    }
}

pub(crate) fn record_span_meta_read() {
    SCAN_PATH_GUARD_ACTIVE.with(|g| {
        if g.get() {
            SPAN_META_READS_UNDER_SCAN_GUARD.with(|c| c.set(c.get().saturating_add(1)));
        }
    });
}

pub(crate) fn record_free_span_read() {
    SCAN_PATH_GUARD_ACTIVE.with(|g| {
        if g.get() {
            FREE_SPAN_READS_UNDER_SCAN_GUARD.with(|c| c.set(c.get().saturating_add(1)));
        }
    });
}

pub(crate) fn record_overflow_chain_rebuild() {
    SCAN_PATH_GUARD_ACTIVE.with(|g| {
        if g.get() {
            OVERFLOW_CHAIN_REBUILDS_UNDER_SCAN_GUARD.with(|c| c.set(c.get().saturating_add(1)));
        }
    });
}
