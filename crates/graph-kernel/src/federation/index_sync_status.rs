//! Router→graph index convergence snapshot for seeding and backfill orchestration.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Graph-side snapshot of durable derived-index work not yet delivered to graph-index.
///
/// The social-demo seeding orchestrator needs one observable "the index caught up" signal
/// before dispatching dependent edge waves, whose `MATCH` anchors resolve through
/// graph-index property postings. The existing [`crate::plan_exec::MutationId`] watermark
/// (`index_pending_min_mutation_id`) covers only the repair journal (failed-flush
/// re-application); the durable first-delivery outbox is a separate backlog that can also
/// pin convergence, so this snapshot reports both queues and a derived `converged` flag.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IndexSyncStatus {
    /// Derived-index ops still in the durable first-delivery outbox (never yet delivered).
    pub derived_index_outbox_len: u64,
    /// Failed-flush ops persisted in the repair journal awaiting re-application.
    pub repair_journal_len: u64,
    /// `derived_index_outbox_len == 0 && repair_journal_len == 0`.
    pub converged: bool,
}

impl IndexSyncStatus {
    pub fn new(derived_index_outbox_len: u64, repair_journal_len: u64) -> Self {
        Self {
            derived_index_outbox_len,
            repair_journal_len,
            converged: derived_index_outbox_len == 0 && repair_journal_len == 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn index_sync_status_candid_roundtrip_preserves_convergence() {
        let pending = IndexSyncStatus::new(3, 7);
        let bytes = Encode!(&pending).expect("encode");
        assert_eq!(Decode!(&bytes, IndexSyncStatus).expect("decode"), pending);
        assert!(!pending.converged);

        assert!(IndexSyncStatus::new(0, 0).converged);
        assert!(!IndexSyncStatus::new(1, 0).converged);
        assert!(!IndexSyncStatus::new(0, 1).converged);
    }
}
