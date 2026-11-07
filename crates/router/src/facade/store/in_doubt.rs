//! The in-doubt decision for one mutation, owned by a view.
//!
//! A mutation is **in doubt** when the Router cannot establish whether its canonical write happened: the record is
//! non-terminal, so the Router will neither re-dispatch it (that risks applying it twice) nor report it complete (nothing
//! proves it ran). Databases call this an *in-doubt transaction* and never resolve it by garbage collection - they hand it
//! to an operator, whose decision is recorded and only then is the transaction's evidence released.
//!
//! This module is that owner, and it is the only place the rule is written down. Everything that asks the question - the
//! client-visible status, the operator listing, the resolution endpoint, and the retirement driver - asks it here, so the
//! answer cannot drift between them (ADR 0100: one path per operation owned by a view).
//!
//! **Why nothing here resolves automatically.** Absence of Graph evidence is not proof that the write did not happen: a
//! dispatch that is still in flight has not journaled yet, so an automatic abort could mark a mutation failed while the
//! Graph commits it a moment later. Resolving automatically requires a Graph-side fence that makes a late commit
//! impossible; until that exists, resolution is an operator decision and this view only *classifies* and *records*.

use super::super::stable::label_stats::{ClientMutationKey, RouterMutationRecord};
use super::RouterStore;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::plan_exec::MutationId;

/// How long a non-terminal mutation may go without progress before it is reported as unresolved.
///
/// This is a **reporting** threshold, not an expiry: nothing is evicted or aborted when it passes. It exists so an
/// operator can tell "slow, the driver is still working on it" from "nothing has moved in a day, this needs a decision".
/// Chosen well above the recovery driver's cadence (a 2 s floor with a bounded lap) and well below the 7-day Router
/// retention, so a stuck mutation is visible for days before anything else about it changes.
pub(crate) const UNRESOLVED_AFTER_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// What the view concludes about one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InDoubt {
    /// Terminal and settled: nothing is owed.
    Settled,
    /// Terminal, but the Graph has not acknowledged the retirement of its evidence yet. The record is pinned until it
    /// does; the recovery driver re-drives it, so this converges without a decision.
    AwaitingRetirement,
    /// Non-terminal and still making progress recently: the recovery driver owns it.
    Converging,
    /// Non-terminal and no progress for longer than [`UNRESOLVED_AFTER_NS`]: an operator decision is needed.
    Unresolved { idle_ns: u64 },
}

impl InDoubt {
    /// The idle time an operator listing reports, for the states it lists.
    pub(crate) fn idle_ns(self) -> Option<u64> {
        match self {
            Self::Unresolved { idle_ns } => Some(idle_ns),
            _ => None,
        }
    }

    /// The client-facing guidance for this state, if it differs from the lifecycle phase's own.
    pub(crate) fn next_action(self) -> Option<&'static str> {
        match self {
            Self::Settled | Self::Converging => None,
            Self::AwaitingRetirement => {
                Some("outcome captured; the graph is retiring its dedup evidence, no action needed")
            }
            Self::Unresolved { .. } => Some(
                "outcome unknown and no progress: an operator must resolve it \
                 (resolve_in_doubt_mutation) before the key can be reused",
            ),
        }
    }
}

/// Which decision an operator reached for an in-doubt mutation.
///
/// `AbortInDoubt` is the only verdict: a mutation whose canonical outcome cannot be established is recorded as failed so
/// the key is released, and the recorded decision is what makes that safe to report. There is deliberately no
/// "presume committed" - the Router cannot fabricate an outcome it never captured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InDoubtResolution {
    /// Record the mutation as failed because its outcome could not be established, and release its evidence.
    AbortInDoubt,
}

/// Borrowed view over one store, answering the in-doubt question for any mutation record.
pub(crate) struct InDoubtView<'a> {
    store: &'a RouterStore,
    now_ns: u64,
}

impl<'a> InDoubtView<'a> {
    pub(crate) fn new(store: &'a RouterStore, now_ns: u64) -> Self {
        Self { store, now_ns }
    }

    /// Classifies one record. The single rule every caller shares.
    pub(crate) fn classify(&self, record: &RouterMutationRecord) -> InDoubt {
        if record.is_terminal() {
            return if record.scalar_retirement_pending() {
                InDoubt::AwaitingRetirement
            } else {
                InDoubt::Settled
            };
        }
        // A record that never left routing has no dispatch to be in doubt about; it is a caller's in-flight attempt or an
        // abandoned reservation, which the reclaim reconciler owns rather than this view.
        if record.as_v1().routing_in_progress {
            return InDoubt::Converging;
        }
        let since = record
            .as_v1()
            .terminal_at_ns
            .or(record.as_v1().routing_lease_ns)
            .unwrap_or(0);
        let idle_ns = self.now_ns.saturating_sub(since);
        if idle_ns >= UNRESOLVED_AFTER_NS {
            InDoubt::Unresolved { idle_ns }
        } else {
            InDoubt::Converging
        }
    }

    /// The retirement receipt this record can state, if it owes a retirement.
    ///
    /// A scalar record states the canonical row count it captured; that is the exact receipt the Graph compares. `None`
    /// means "nothing to retire", which is a legitimate answer for a record that never captured an outcome.
    #[cfg(target_family = "wasm")]
    pub(crate) fn retirement_receipt(
        &self,
        record: &RouterMutationRecord,
    ) -> Option<(MutationId, candid::Principal, u64)> {
        self.store.scalar_retirement_receipt(record)
    }

    /// Records an operator's resolution and releases the obligation it settles.
    ///
    /// The decision is written **before** anything else, so a failure to retire the evidence afterwards cannot lose the
    /// fact that a human resolved this mutation - the retirement is best-effort and the evidence simply stays until it
    /// succeeds. Returns the mutation's id so the caller can report which one was resolved.
    pub(crate) fn resolve(
        &self,
        key: &ClientMutationKey,
        resolution: InDoubtResolution,
    ) -> Result<MutationId, RouterError> {
        let record = self
            .store
            .router_mutation_record(key)
            .ok_or_else(|| RouterError::NotFound("client_mutation_key is not known".into()))?;
        // Resolving a mutation that already settled would rewrite a durable outcome; a mutation that is still converging
        // is the driver's job, and aborting it would risk the late-commit race this view exists to avoid.
        match self.classify(&record) {
            InDoubt::Unresolved { .. } => {}
            InDoubt::Settled | InDoubt::AwaitingRetirement | InDoubt::Converging => {
                return Err(RouterError::Conflict(
                    "mutation has no unresolved outcome to abort".into(),
                ));
            }
        }
        let mutation_id = record.as_v1().mutation_id;
        let InDoubtResolution::AbortInDoubt = resolution;
        self.store.record_in_doubt_abort(
            key,
            mutation_id,
            "in-doubt: operator resolved the unknown outcome as not applied",
        )?;
        Ok(mutation_id)
    }

    /// The in-doubt records a caller can page through, oldest-idle first is not guaranteed - the scan is by key, bounded
    /// by `budget`, exactly like the retention sweep beside it.
    pub(crate) fn scan(
        &self,
        start_after: Option<&ClientMutationKey>,
        budget: usize,
    ) -> (
        Vec<(ClientMutationKey, u64)>,
        Option<ClientMutationKey>,
        u32,
    ) {
        self.store
            .scan_in_doubt_mutations(start_after, budget, |record| {
                self.classify(record).idle_ns()
            })
    }
}
