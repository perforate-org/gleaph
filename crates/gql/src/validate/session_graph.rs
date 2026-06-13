//! Session graph seed for ingress validation (ADR 0011 §2).

use std::cell::RefCell;

use rapidhash::RapidHashSet;

thread_local! {
    static ACTIVE_VALIDATION_SEED: RefCell<Option<SessionGraphSeed>> = const { RefCell::new(None) };
}

/// Resolved session graph names supplied by the router ingress layer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionGraphSeed {
    /// Effective current graph catalog name after `session_activity` and default resolution.
    pub current_graph: Option<String>,
    /// Caller home graph catalog name when defined (sole visible graph today).
    pub home_graph: Option<String>,
}

struct SeedGuard;

impl Drop for SeedGuard {
    fn drop(&mut self) {
        ACTIVE_VALIDATION_SEED.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Install `seed` for the duration of a validation pass (`Drop` clears it).
pub(super) fn with_validation_seed<R>(seed: Option<&SessionGraphSeed>, f: impl FnOnce() -> R) -> R {
    let _guard = seed.map(|s| {
        ACTIVE_VALIDATION_SEED.with(|cell| *cell.borrow_mut() = Some(s.clone()));
        SeedGuard
    });
    f()
}

pub(super) fn ingress_seed_active() -> bool {
    ACTIVE_VALIDATION_SEED.with(|cell| cell.borrow().is_some())
}

pub(super) fn active_seed() -> Option<SessionGraphSeed> {
    ACTIVE_VALIDATION_SEED.with(|cell| cell.borrow().clone())
}

pub(super) fn initial_graph_scope() -> RapidHashSet<String> {
    let mut graph_scope = RapidHashSet::default();
    if let Some(name) = active_seed().and_then(|seed| seed.current_graph) {
        graph_scope.insert(name);
    }
    graph_scope
}
