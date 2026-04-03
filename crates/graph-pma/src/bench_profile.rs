//! Opt-in timing splits for `GLEAPH_BENCH_PROFILE=1` (stderr report).
//!
//! Accumulators are **thread-local** so parallel `cargo test` targets do not mix samples.
//! Use `cargo test -- --test-threads=1` if you run multiple profile tests in one invocation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

thread_local! {
    static ACCUM: RefCell<Option<HashMap<&'static str, (Duration, u64)>>> = const { RefCell::new(None) };
}

thread_local! {
    static STATS: RefCell<HashMap<&'static str, u64>> = RefCell::new(HashMap::new());
}

fn accum_enabled() -> bool {
    static INIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INIT.get_or_init(|| std::env::var_os("GLEAPH_BENCH_PROFILE").is_some())
}

/// Initializes the per-thread accumulator when profiling is on.
#[allow(dead_code)]
pub fn reset() {
    if !accum_enabled() {
        return;
    }
    ACCUM.with(|cell| {
        *cell.borrow_mut() = Some(HashMap::new());
    });
    STATS.with(|cell| {
        cell.borrow_mut().clear();
    });
}

pub fn record(phase: &'static str, elapsed: Duration) {
    if !accum_enabled() {
        return;
    }
    ACCUM.with(|cell| {
        let mut slot = cell.borrow_mut();
        let map = slot.get_or_insert_with(HashMap::new);
        let e = map.entry(phase).or_insert((Duration::ZERO, 0));
        e.0 += elapsed;
        e.1 += 1;
    });
}

/// Thread-local counter for incremental I/O stats (bytes, page-equivalents, event counts).
pub fn record_stat(phase: &'static str, delta: u64) {
    if !accum_enabled() || delta == 0 {
        return;
    }
    STATS.with(|cell| {
        let mut map = cell.borrow_mut();
        *map.entry(phase).or_insert(0) += delta;
    });
}

pub struct PhaseGuard {
    phase: &'static str,
    start: Option<Instant>,
}

impl PhaseGuard {
    pub fn new(phase: &'static str) -> Self {
        Self {
            phase,
            start: accum_enabled().then(Instant::now),
        }
    }
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        if let Some(s) = self.start.take() {
            record(self.phase, s.elapsed());
        }
    }
}

/// Print totals to stderr (no-op if profiling disabled or empty).
#[allow(dead_code)]
pub fn dump_report(header: &str) {
    if !accum_enabled() {
        return;
    }
    ACCUM.with(|cell| {
        let slot = cell.borrow();
        let Some(map) = slot.as_ref() else {
            return;
        };
        if map.is_empty() {
            return;
        }
        eprintln!("GLEAPH_BENCH_PROFILE {header}");
        let mut keys: Vec<_> = map.keys().copied().collect();
        keys.sort();
        for k in keys {
            let (total, n) = map[k];
            let per_ns = total.as_nanos() as f64 / n.max(1) as f64;
            eprintln!("  {k}: {total:?} total, {n} calls, {per_ns:.1} ns/call");
        }
    });
    STATS.with(|cell| {
        let map = cell.borrow();
        if map.is_empty() {
            return;
        }
        eprintln!("GLEAPH_BENCH_PROFILE {header} stats");
        let mut keys: Vec<_> = map.keys().copied().collect();
        keys.sort();
        for k in keys {
            eprintln!("  {k}: {}", map[k]);
        }
    });
}
