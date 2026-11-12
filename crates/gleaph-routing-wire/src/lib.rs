//! Candid wire contract for the routing inference canister.
//!
//! This crate is the single source of truth for the `route_and_extract` reply types. The canister
//! and the SDK client both depend on it; changing a field is a contract change and bumps
//! [`WIRE_VERSION`]. The canister serves `wire_version()` so a client can verify compatibility.

use candid::{CandidType, Deserialize};

/// Maximum question tokens the model contract accepts (ADR 0113). This is the single source of
/// truth for the canister truncation, the Router cap and the SDK tokenizer.
pub const MAX_QUESTION_TOKENS: usize = 256;

/// Wire version of the routing contract.
///
/// 1: `route_and_extract(token_ids)`.
/// 2: adds the text ingress types and the tokenizer fingerprint (ADR 0113).
pub const WIRE_VERSION: u32 = 2;

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

/// One BIO span over the original question text, in UTF-8 byte offsets.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct TextSlotSpan {
    /// Tag name without its B/I prefix.
    pub tag: String,
    /// Start byte offset into the question text (inclusive).
    pub start: u32,
    /// End byte offset into the question text (exclusive).
    pub end: u32,
    /// Mean per-token tag confidence.
    pub score: f32,
}

/// The canister's `route_and_extract_text` reply.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct TextRouteAndExtract {
    /// Routing probabilities in the pack's `route_options` order.
    pub probabilities: Vec<f32>,
    /// Slot spans in byte offsets over the original text.
    pub spans: Vec<TextSlotSpan>,
    /// Question token count after the model cap (at most 256).
    pub token_count: u32,
    /// CJK fraction of the decoded question (the same measure the abstain policy uses).
    pub cjk_fraction: f32,
}

/// Fingerprint of the tokenizer asset installed in the inference canister.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct TokenizerFingerprint {
    /// SHA-256 of the tokenizer JSON.
    pub sha256: Vec<u8>,
    /// Byte length of the tokenizer JSON.
    pub bytes: u64,
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

    #[test]
    fn text_reply_round_trips_through_candid() {
        let value = TextRouteAndExtract {
            probabilities: vec![0.9, 0.1],
            spans: vec![TextSlotSpan {
                tag: "concept".into(),
                start: 10,
                end: 18,
                score: 0.7,
            }],
            token_count: 12,
            cjk_fraction: 0.25,
        };
        let bytes = Encode!(&Ok::<_, String>(value.clone())).expect("encode");
        let decoded: Result<TextRouteAndExtract, String> =
            Decode!(&bytes, Result<TextRouteAndExtract, String>).expect("decode");
        assert_eq!(decoded.expect("ok"), value);
        assert_eq!(WIRE_VERSION, 2);
    }
}
