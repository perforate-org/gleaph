//! Candid wire contract for the routing inference canister.
//!
//! This crate is the single source of truth for the `route_and_extract` reply types. The canister
//! and the SDK client both depend on it; changing a field is a contract change and bumps
//! [`WIRE_VERSION`]. The canister serves `wire_version()` so a client can verify compatibility.

use candid::{CandidType, Deserialize};

/// Wire version of the `route_and_extract` contract.
pub const WIRE_VERSION: u32 = 1;

/// One BIO span recovered by the canister: tag name plus token indices.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct SlotSpan {
    /// Tag name without its B/I prefix (for example `concept`).
    pub tag: String,
    /// Start token index into the caller's `token_ids` (inclusive).
    pub start: u32,
    /// End token index into the caller's `token_ids` (exclusive).
    pub end: u32,
    /// Mean per-token tag confidence; clients use it to pick between several spans for one slot.
    pub score: f32,
}

/// The canister's `route_and_extract` reply.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct RouteAndExtract {
    /// Routing probabilities in the pack's `route_options` order.
    pub probabilities: Vec<f32>,
    /// Slot spans over the question tokens.
    pub spans: Vec<SlotSpan>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn result_round_trips_through_candid() {
        let value = RouteAndExtract {
            probabilities: vec![0.1, 0.2, 0.7],
            spans: vec![SlotSpan {
                tag: "concept".into(),
                start: 2,
                end: 4,
                score: 0.5,
            }],
        };
        let bytes = Encode!(&Ok::<_, String>(value.clone())).expect("encode");
        let decoded: Result<RouteAndExtract, String> =
            Decode!(&bytes, Result<RouteAndExtract, String>).expect("decode");
        assert_eq!(decoded.expect("ok"), value);
    }
}
