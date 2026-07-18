//! In-memory instruction-cost log buffer used by the `batch-instr-log` feature.
//!
//! The IC canister log ring buffer is small (tens of records), so high-volume
//! per-operation phase logs are overwritten almost immediately.  This module
//! keeps a heap-resident copy of every emitted log line so it can be dumped
//! through a query endpoint after a long batch run.

use std::cell::RefCell;

thread_local! {
    static LOG_BUFFER: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

#[cfg(all(feature = "batch-instr-log", target_family = "wasm"))]
pub fn push(line: String) {
    LOG_BUFFER.with(|buf| buf.borrow_mut().push(line));
}

#[cfg(all(feature = "batch-instr-log", not(target_family = "wasm")))]
#[allow(dead_code)]
pub fn push(_line: String) {}

#[cfg(not(feature = "batch-instr-log"))]
#[inline]
pub fn push(_line: String) {}

/// Return all buffered log lines and clear the buffer.
pub fn take() -> Vec<String> {
    LOG_BUFFER.with(|buf| buf.borrow_mut().drain(..).collect())
}
