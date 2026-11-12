//! Router-funded inference quota (ADR 0112).
//!
//! Best-effort, heap-only fixed-window accounting per `(caller, graph)`. Graph tenancy and the
//! per-graph inference flag remain the authorization boundary; this module only bounds how much
//! Router-funded model work one caller may request in a window.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;

use crate::facade::stable::router_config::InferenceQuotaConfig;
use crate::state::RouterError;

/// Client-side question cap (SDK `MAX_QUESTION_TOKENS`); larger requests are invalid.
pub(crate) const MAX_QUESTION_TOKENS: usize = 256;

/// Bound on distinct `(caller, graph)` windows kept in the heap.
const MAX_WINDOWS: usize = 4096;

#[derive(Clone, Copy)]
struct Window {
    started_at_ns: u64,
    units_used: u32,
}

thread_local! {
    static WINDOWS: RefCell<BTreeMap<(Principal, GraphId), Window>> =
        const { RefCell::new(BTreeMap::new()) };
    static CONSUMED_UNITS: Cell<u64> = const { Cell::new(0) };
    static DENIED_REQUESTS: Cell<u64> = const { Cell::new(0) };
}

/// Units charged for a question of `tokens` token ids, mirroring the model's 32-token windows
/// with a 10-token stride for longer questions.
pub(crate) fn units_for_tokens(tokens: usize) -> u32 {
    if tokens <= 32 {
        1
    } else {
        let extra = (tokens - 32).div_ceil(10);
        1 + u32::try_from(extra).unwrap_or(u32::MAX - 1)
    }
}

/// Consume quota for one routing request. `exempt` is the controller bypass.
pub(crate) fn consume(
    caller: Principal,
    graph: GraphId,
    tokens: usize,
    config: InferenceQuotaConfig,
    now_ns: u64,
    exempt: bool,
) -> Result<(), RouterError> {
    if tokens > MAX_QUESTION_TOKENS {
        return Err(RouterError::InvalidArgument(format!(
            "question has {tokens} token ids; max {MAX_QUESTION_TOKENS}"
        )));
    }
    if !config.enabled || exempt {
        return Ok(());
    }
    let window_ns = u64::from(config.window_secs).saturating_mul(1_000_000_000);
    let units = units_for_tokens(tokens);
    WINDOWS.with_borrow_mut(|windows| {
        if windows.len() >= MAX_WINDOWS && !windows.contains_key(&(caller, graph)) {
            evict_stale(windows, now_ns, window_ns);
            if windows.len() >= MAX_WINDOWS
                && let Some(oldest) = windows.keys().next().copied()
            {
                windows.remove(&oldest);
            }
        }
        let entry = windows.entry((caller, graph)).or_insert(Window {
            started_at_ns: now_ns,
            units_used: 0,
        });
        if now_ns.saturating_sub(entry.started_at_ns) >= window_ns {
            entry.started_at_ns = now_ns;
            entry.units_used = 0;
        }
        // Charge atomically against the window; exhaustion leaves the window unchanged.
        let next = entry.units_used.saturating_add(units);
        if next > config.units_per_window {
            DENIED_REQUESTS.with(|denied| denied.set(denied.get() + 1));
            return Err(RouterError::Busy {
                operation: "inference_quota".to_owned(),
            });
        }
        entry.units_used = next;
        CONSUMED_UNITS.with(|consumed| consumed.set(consumed.get() + u64::from(units)));
        Ok(())
    })
}

fn evict_stale(windows: &mut BTreeMap<(Principal, GraphId), Window>, now_ns: u64, window_ns: u64) {
    windows.retain(|_, window| now_ns.saturating_sub(window.started_at_ns) < window_ns);
}

/// `(consumed units, denied requests)` for diagnostics.
pub(crate) fn stats() -> (u64, u64) {
    (
        CONSUMED_UNITS.with(Cell::get),
        DENIED_REQUESTS.with(Cell::get),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(units: u32) -> InferenceQuotaConfig {
        InferenceQuotaConfig {
            enabled: true,
            window_secs: 60,
            units_per_window: units,
        }
    }

    #[test]
    fn units_follow_the_model_windows() {
        assert_eq!(units_for_tokens(1), 1);
        assert_eq!(units_for_tokens(32), 1);
        assert_eq!(units_for_tokens(33), 2);
        assert_eq!(units_for_tokens(42), 2);
        assert_eq!(units_for_tokens(43), 3);
        assert_eq!(units_for_tokens(256), 24);
    }

    #[test]
    fn window_exhausts_and_resets() {
        let caller = Principal::from_slice(&[0x41; 29]);
        let graph = GraphId::from_raw(7);
        let cfg = config(2);
        assert!(consume(caller, graph, 10, cfg, 0, false).is_ok());
        assert!(consume(caller, graph, 10, cfg, 1, false).is_ok());
        assert!(matches!(
            consume(caller, graph, 10, cfg, 2, false),
            Err(RouterError::Busy { .. })
        ));
        // The next window starts fresh.
        let next_window = 60 * 1_000_000_000;
        assert!(consume(caller, graph, 10, cfg, next_window, false).is_ok());
    }

    #[test]
    fn controllers_bypass_the_quota() {
        let caller = Principal::from_slice(&[0x42; 29]);
        let graph = GraphId::from_raw(8);
        for _ in 0..100 {
            assert!(consume(caller, graph, 256, config(1), 0, true).is_ok());
        }
    }

    #[test]
    fn over_long_questions_are_rejected_before_counting() {
        let caller = Principal::from_slice(&[0x43; 29]);
        let graph = GraphId::from_raw(9);
        assert!(matches!(
            consume(
                caller,
                graph,
                MAX_QUESTION_TOKENS + 1,
                config(100),
                0,
                false
            ),
            Err(RouterError::InvalidArgument(_))
        ));
    }

    #[test]
    fn disabled_quota_allows_everything() {
        let caller = Principal::from_slice(&[0x44; 29]);
        let graph = GraphId::from_raw(10);
        let mut cfg = config(1);
        cfg.enabled = false;
        for _ in 0..10 {
            assert!(consume(caller, graph, 10, cfg, 0, false).is_ok());
        }
    }
}
