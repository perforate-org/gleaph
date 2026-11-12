//! Candid wire contract for the routing inference canister (ADR 0114).
//!
//! The canister executes the frozen schema/catalog-conditioned general router as a query. This
//! crate is the single source of truth for the context and general-route types. Changing a field
//! is a contract change and bumps [`WIRE_VERSION`]; the canister serves `wire_version()` so a
//! client can verify compatibility.

use candid::{CandidType, Deserialize};

/// Wire version of the inference-canister contract.
///
/// 1: fixed-intent token routing.
/// 2: text ingress and tokenizer fingerprint (ADR 0113).
/// 3: general route only; fixed-intent neural routing removed (ADR 0114).
/// 4: query-only general route; tokenizer/quota contract removed.
/// 5: raw calibration batch query (same graph/context, bounded question count).
pub const WIRE_VERSION: u32 = 6;

// ---------------------------------------------------------------------------------------------
// General routing contract.
// ---------------------------------------------------------------------------------------------

/// Version of the general route surface.
pub const GENERAL_ROUTE_VERSION: u32 = 1;
/// Maximum operations accepted in one graph routing context.
pub const MAX_GENERAL_OPERATIONS: usize = 64;
/// Maximum serialized context bytes accepted by the canister.
pub const MAX_GENERAL_CONTEXT_BYTES: usize = 32 * 1024;
/// Maximum question text bytes accepted by the general route.
pub const MAX_GENERAL_TEXT_BYTES: usize = 16 * 1024;
/// Maximum parameters that may declare a value domain on one graph.
pub const MAX_DOMAIN_PARAMETERS: usize = 16;
/// Maximum canonical values in one parameter's domain.
pub const MAX_DOMAIN_VALUES: usize = 64;
/// Maximum UTF-8 bytes of one canonical domain value.
pub const MAX_DOMAIN_VALUE_BYTES: usize = 128;
/// Maximum UTF-8 bytes of all domain values on one graph.
pub const MAX_DOMAIN_TOTAL_BYTES: usize = 8 * 1024;
/// Maximum questions in one raw calibration batch.
pub const MAX_GENERAL_BATCH_QUESTIONS: usize = 8;
/// Maximum combined question text bytes in one raw calibration batch.
pub const MAX_GENERAL_BATCH_TEXT_BYTES: usize = 64 * 1024;

/// One typed parameter in an operation card.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralParameter {
    pub name: String,
    #[serde(rename = "type")]
    pub parameter_type: String,
    pub required: bool,
    /// Canonical values this parameter accepts (ADR 0117), sorted and bounded. Empty means the
    /// parameter has no declared domain and the extractor's rules are the only source of values.
    #[serde(default)]
    pub domain: Vec<String>,
}

/// One prepared operation exposed to the general router.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralOperation {
    pub id: String,
    pub description: String,
    pub parameters: Vec<GeneralParameter>,
    pub result: Vec<String>,
}

/// One property on a graph label.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralProperty {
    pub name: String,
    #[serde(rename = "type")]
    pub property_type: String,
}

/// One graph label.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralLabel {
    pub name: String,
    pub properties: Vec<GeneralProperty>,
}

/// One graph edge.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralEdge {
    pub name: String,
    #[serde(rename = "from")]
    pub from_label: String,
    #[serde(rename = "to")]
    pub to_label: String,
}

/// The graph-type summary sent with the operations.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralGraphSchema {
    pub labels: Vec<GeneralLabel>,
    pub edges: Vec<GeneralEdge>,
}

/// Per-graph routing context supplied by the Router. The canister verifies the fingerprint.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralRoutingContext {
    pub schema_version: u32,
    pub graph: GeneralGraphSchema,
    pub operations: Vec<GeneralOperation>,
    pub abstain_option: String,
    pub abstain_threshold: f32,
    pub policy_version: u32,
    /// SHA-256 of the Candid encoding of this value with an empty fingerprint field.
    pub context_fingerprint: Vec<u8>,
}

/// Request for the general route method.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralRouteRequest {
    pub text: String,
    pub context: GeneralRoutingContext,
}

/// Request for the raw calibration batch: one context, several question texts.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralRouteBatchRequest {
    pub texts: Vec<String>,
    pub context: GeneralRoutingContext,
}

/// Typed slot value returned by the general router.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub enum GeneralSlotLiteral {
    Int(i64),
    Text(String),
}

/// One typed slot value with its character span (not byte offsets).
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralSlot {
    pub name: String,
    pub value: GeneralSlotLiteral,
    pub surface: String,
    pub start: u32,
    pub end: u32,
    pub rule: String,
}

/// Reply from the general route method.
#[derive(Debug, Clone, PartialEq, CandidType, Deserialize)]
pub struct GeneralRouteResult {
    /// Selected operation, absent when the decision abstains.
    pub operation: Option<String>,
    /// Probabilities in context operation order.
    pub probabilities: Vec<f32>,
    pub slots: Vec<GeneralSlot>,
    /// At least one required slot is missing or ambiguous; callers must not execute.
    pub incomplete: bool,
    /// The model, threshold or strong OOS rule decided not to execute.
    pub abstain: bool,
    pub context_fingerprint: Vec<u8>,
    pub model_version: u32,
}

/// SHA-256 of the Candid-encoded context with the fingerprint field cleared. The Router computes
/// this before sending; the canister recomputes it and rejects a mismatch.
pub fn general_context_fingerprint(context: &GeneralRoutingContext) -> Vec<u8> {
    use candid::Encode;
    use sha2::{Digest, Sha256};
    let mut cleared = context.clone();
    cleared.context_fingerprint.clear();
    let bytes = Encode!(&cleared).expect("general context Candid encoding");
    Sha256::digest(bytes).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    fn context() -> GeneralRoutingContext {
        GeneralRoutingContext {
            schema_version: 1,
            graph: GeneralGraphSchema {
                labels: vec![GeneralLabel {
                    name: "Film".into(),
                    properties: vec![GeneralProperty {
                        name: "title".into(),
                        property_type: "string".into(),
                    }],
                }],
                edges: vec![],
            },
            operations: vec![GeneralOperation {
                id: "film-search".into(),
                description: "Search films.".into(),
                parameters: vec![GeneralParameter {
                    name: "query".into(),
                    parameter_type: "string".into(),
                    required: true,
                    domain: Vec::new(),
                }],
                result: vec!["title".into()],
            }],
            abstain_option: "none-of-these".into(),
            abstain_threshold: 0.4,
            policy_version: 1,
            context_fingerprint: vec![],
        }
    }

    #[test]
    fn wire_version_is_five() {
        assert_eq!(WIRE_VERSION, 6);
    }

    #[test]
    fn general_context_fingerprint_ignores_the_fingerprint_field() {
        let mut value = context();
        let expected = general_context_fingerprint(&value);
        value.context_fingerprint = vec![1, 2, 3];
        assert_eq!(general_context_fingerprint(&value), expected);
        assert_eq!(expected.len(), 32);
    }

    #[test]
    fn general_route_types_round_trip_through_candid() {
        let request = GeneralRouteRequest {
            text: "find films".into(),
            context: context(),
        };
        let bytes = Encode!(&request).expect("encode");
        let decoded: GeneralRouteRequest = Decode!(&bytes, GeneralRouteRequest).expect("decode");
        assert_eq!(decoded, request);
        let result = GeneralRouteResult {
            operation: Some("film-search".into()),
            probabilities: vec![1.0],
            slots: vec![GeneralSlot {
                name: "query".into(),
                value: GeneralSlotLiteral::Text("films".into()),
                surface: "films".into(),
                start: 5,
                end: 10,
                rule: "query".into(),
            }],
            incomplete: false,
            abstain: false,
            context_fingerprint: vec![0; 32],
            model_version: 1,
        };
        let bytes = Encode!(&result).expect("encode");
        let decoded: GeneralRouteResult = Decode!(&bytes, GeneralRouteResult).expect("decode");
        assert_eq!(decoded, result);
    }
}
