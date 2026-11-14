//! Shared wire facts for the TEXT index canister.
//!
//! The text canister and the Router both speak these two values: `admin_flush` returns
//! [`FlushReport`], and `search_candidates` admits at most [`MAX_CANDIDATE_KEYS`] keys. They live
//! here so the client-side admission cap and the canister-side rejection cannot drift.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Upper bound on distinct candidate keys per `search_candidates` call.
///
/// The Router's prefix cap funnels at most this many TEXT keys into one call; larger sets are
/// rejected before any posting work.
pub const MAX_CANDIDATE_KEYS: usize = 256;

/// Bounded flush-step outcome; repeat until `done`.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushReport {
    /// Pending ops applied during this call.
    pub drained_ops: u64,
    /// Ops still awaiting application.
    pub remaining_ops: u64,
    /// True when the pending log is fully applied.
    pub done: bool,
}

#[cfg(test)]
mod tests {
    use super::FlushReport;

    #[test]
    fn flush_report_round_trips_through_candid() {
        let report = FlushReport {
            drained_ops: 7,
            remaining_ops: 3,
            done: false,
        };
        let bytes = candid::encode_one(report).expect("encode");
        let decoded: FlushReport = candid::decode_one(&bytes).expect("decode");
        assert_eq!(decoded, report);
    }
}
