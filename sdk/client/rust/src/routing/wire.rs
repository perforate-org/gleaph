//! Candid wire types for the inference canister's `route_and_extract` reply.
//!
//! The types live in the shared `gleaph-routing-wire` crate so the canister and the SDK have one
//! definition; this module keeps the historical names used by the client.

pub use gleaph_routing_wire::{RouteAndExtract as RouteAndExtractWire, SlotSpan as SlotSpanWire};

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
