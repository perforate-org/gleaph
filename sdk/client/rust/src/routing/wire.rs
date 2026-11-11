//! Candid wire types for the inference canister's `route_and_extract` reply.
//!
//! The field names and types mirror `laya-infer-canister`'s `RouteAndExtract` and `SlotSpan`;
//! changing either side is a contract change and must update both.

use candid::{CandidType, Deserialize};

/// One BIO span recovered by the canister: tag name plus token indices.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct SlotSpanWire {
    /// Tag name without its B/I prefix (for example `concept`).
    pub tag: String,
    /// Start token index into the caller's `token_ids` (inclusive).
    pub start: u32,
    /// End token index into the caller's `token_ids` (exclusive).
    pub end: u32,
    /// Mean per-token tag confidence; used to pick between several spans for one slot.
    pub score: f32,
}

/// The canister's `route_and_extract` reply.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct RouteAndExtractWire {
    /// Routing probabilities in the pack's `route_options` order.
    pub probabilities: Vec<f32>,
    /// Slot spans over the question tokens.
    pub spans: Vec<SlotSpanWire>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn result_round_trips_through_candid() {
        let wire = RouteAndExtractWire {
            probabilities: vec![0.1, 0.2, 0.7],
            spans: vec![SlotSpanWire {
                tag: "concept".into(),
                start: 2,
                end: 4,
                score: 0.5,
            }],
        };
        let bytes = Encode!(&Ok::<_, String>(wire.clone())).expect("encode");
        let decoded: Result<RouteAndExtractWire, String> =
            Decode!(&bytes, Result<RouteAndExtractWire, String>).expect("decode");
        assert_eq!(decoded.expect("ok"), wire);
    }
}
