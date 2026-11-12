//! Inference canister: tokenizer asset plus the frozen schema/catalog-conditioned general router.
//!
//! The canister no longer stores or executes an INT8 routing pack (ADR 0114). It owns the tokenizer
//! (ADR 0113) for exact quota token counts, and executes the additive general route over a
//! Router-supplied graph context. Routing is a query-safe operation; the token-count call remains an
//! update because the tokenizer is heap state.

use std::cell::RefCell;

use candid::{CandidType, Encode};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{Cell as StableCell, DefaultMemoryImpl};
use tokenizers::Tokenizer;

use glev_general_router::{
    RoutingContext, SlotLiteral,
    context::{EdgeSpec, GraphSchema, LabelSpec, OperationCard, ParameterSpec, PropertySpec},
    extract_slots, frozen_router, rule_abstain,
};

pub use gleaph_routing_wire::{
    GENERAL_ROUTE_VERSION, GeneralRouteRequest, GeneralRouteResult, GeneralRoutingContext,
    GeneralSlot, GeneralSlotLiteral, MAX_GENERAL_CONTEXT_BYTES, MAX_GENERAL_OPERATIONS,
    MAX_GENERAL_TEXT_BYTES,
};
use gleaph_routing_wire::{TokenizerFingerprint, WIRE_VERSION};
use sha2::Digest;

/// Deterministic getrandom backend for wasm32-unknown-unknown (ADR 0113). The tokenizer's hash
/// maps need a successful fill, not unpredictable bytes.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom::Error> {
    // SAFETY: getrandom hands us a valid writable buffer of `len` bytes.
    unsafe { core::ptr::write_bytes(dest, 0, len) };
    Ok(())
}

#[derive(CandidType, serde::Deserialize)]
struct InstallChunk {
    offset: u64,
    bytes: Vec<u8>,
}

/// Tokenizer JSON bytes (ADR 0113).
const TOKENIZER_MEMORY_ID: MemoryId = MemoryId::new(1);
/// Tokenizer fingerprint: 32-byte SHA-256 followed by the u64 byte length.
const TOKENIZER_FINGERPRINT_MEMORY_ID: MemoryId = MemoryId::new(2);

struct StableTokenizer {
    _manager: MemoryManager<DefaultMemoryImpl>,
    bytes: StableCell<Vec<u8>, VirtualMemory<DefaultMemoryImpl>>,
    fingerprint: StableCell<Vec<u8>, VirtualMemory<DefaultMemoryImpl>>,
}

impl StableTokenizer {
    fn new() -> Self {
        let manager = MemoryManager::init(DefaultMemoryImpl::default());
        let bytes = StableCell::init(manager.get(TOKENIZER_MEMORY_ID), Vec::new());
        let fingerprint =
            StableCell::init(manager.get(TOKENIZER_FINGERPRINT_MEMORY_ID), Vec::new());
        Self {
            _manager: manager,
            bytes,
            fingerprint,
        }
    }
}

thread_local! {
    static STABLE_TOKENIZER: RefCell<StableTokenizer> = RefCell::new(StableTokenizer::new());
}

#[derive(Default)]
struct State {
    tokenizer: Option<Tokenizer>,
    tokenizer_staging: Vec<u8>,
    tokenizer_fingerprint: Option<TokenizerFingerprint>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn parse_tokenizer(bytes: &[u8]) -> Result<Tokenizer, String> {
    let mut tokenizer = Tokenizer::from_bytes(bytes).map_err(|error| error.to_string())?;
    tokenizer
        .with_truncation(None)
        .map_err(|error| error.to_string())?;
    Ok(tokenizer)
}

fn cjk_fraction(tokenizer: &Tokenizer, ids: &[u32]) -> f32 {
    let Ok(text) = tokenizer.decode(ids, true) else {
        return 0.0;
    };
    gleaph_routing_wire::cjk_fraction(&text)
}

fn fingerprint_bytes(fingerprint: &TokenizerFingerprint) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(&fingerprint.sha256);
    bytes.extend_from_slice(&fingerprint.bytes.to_le_bytes());
    bytes
}

fn fingerprint_from_bytes(bytes: &[u8]) -> Option<TokenizerFingerprint> {
    if bytes.len() != 40 {
        return None;
    }
    Some(TokenizerFingerprint {
        sha256: bytes[..32].to_vec(),
        bytes: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
    })
}

#[ic_cdk::update]
fn admin_install_tokenizer_chunk(chunk: InstallChunk) -> Result<u64, String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("tokenizer install rejected: caller is not a controller".into());
    }
    let end = usize::try_from(chunk.offset)
        .ok()
        .and_then(|offset| offset.checked_add(chunk.bytes.len()))
        .ok_or_else(|| "tokenizer offset overflow".to_owned())?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.tokenizer_staging.len() < end {
            state.tokenizer_staging.resize(end, 0);
        }
        let offset = chunk.offset as usize;
        state.tokenizer_staging[offset..end].copy_from_slice(&chunk.bytes);
        Ok(state.tokenizer_staging.len() as u64)
    })
}

#[ic_cdk::update]
fn admin_install_tokenizer_finish() -> Result<TokenizerFingerprint, String> {
    if !ic_cdk::api::is_controller(&ic_cdk::api::msg_caller()) {
        return Err("tokenizer install rejected: caller is not a controller".into());
    }
    let bytes = STATE.with(|state| state.borrow().tokenizer_staging.clone());
    if bytes.is_empty() {
        return Err("no tokenizer bytes installed".into());
    }
    let tokenizer = parse_tokenizer(&bytes)?;
    let digest = sha2::Sha256::digest(&bytes);
    let fingerprint = TokenizerFingerprint {
        sha256: digest.to_vec(),
        bytes: bytes.len() as u64,
    };
    STABLE_TOKENIZER.with(|store| {
        let mut store = store.borrow_mut();
        store.bytes.set(bytes.clone());
        store.fingerprint.set(fingerprint_bytes(&fingerprint));
    });
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.tokenizer = Some(tokenizer);
        state.tokenizer_staging.clear();
        state.tokenizer_fingerprint = Some(fingerprint.clone());
    });
    Ok(fingerprint)
}

/// SHA-256 and length of the installed tokenizer asset, if any.
#[ic_cdk::query]
fn tokenizer_fingerprint() -> Option<TokenizerFingerprint> {
    STATE.with(|state| state.borrow().tokenizer_fingerprint.clone())
}

/// Token count and CJK fraction for the Router's exact quota charge.
#[ic_cdk::query]
fn tokenizer_token_count(text: String) -> Result<(u32, f32), String> {
    STATE.with(|state| {
        let state = state.borrow();
        let tokenizer = state
            .tokenizer
            .as_ref()
            .ok_or("tokenizer is not installed")?;
        let encoding = tokenizer
            .encode(text, false)
            .map_err(|error| error.to_string())?;
        let ids = encoding.get_ids();
        let count = u32::try_from(ids.len()).map_err(|_| "token count overflow".to_owned())?;
        Ok((count, cjk_fraction(tokenizer, ids)))
    })
}

/// Reload the persisted tokenizer after an upgrade.
#[ic_cdk::post_upgrade]
fn post_upgrade() {
    let bytes = STABLE_TOKENIZER.with(|store| store.borrow().bytes.get().clone());
    if bytes.is_empty() {
        return;
    }
    let Ok(tokenizer) = parse_tokenizer(&bytes) else {
        return;
    };
    let fingerprint =
        STABLE_TOKENIZER.with(|store| fingerprint_from_bytes(store.borrow().fingerprint.get()));
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.tokenizer = Some(tokenizer);
        state.tokenizer_fingerprint = fingerprint;
    });
}

fn wire_to_router_context(context: &GeneralRoutingContext) -> Result<RoutingContext, String> {
    let graph = GraphSchema {
        labels: context
            .graph
            .labels
            .iter()
            .map(|label| {
                Ok(LabelSpec {
                    name: label.name.clone(),
                    properties: label
                        .properties
                        .iter()
                        .map(|property| {
                            Ok(PropertySpec {
                                name: property.name.clone(),
                                property_type: property.property_type.clone(),
                            })
                        })
                        .collect::<Result<Vec<_>, String>>()?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        edges: context
            .graph
            .edges
            .iter()
            .map(|edge| EdgeSpec {
                name: edge.name.clone(),
                from_label: edge.from_label.clone(),
                to_label: edge.to_label.clone(),
            })
            .collect(),
    };
    let operations = context
        .operations
        .iter()
        .map(|operation| OperationCard {
            id: operation.id.clone(),
            description: operation.description.clone(),
            parameters: operation
                .parameters
                .iter()
                .map(|parameter| ParameterSpec {
                    name: parameter.name.clone(),
                    parameter_type: parameter.parameter_type.clone(),
                    required: parameter.required,
                })
                .collect(),
            result: operation.result.clone(),
        })
        .collect();
    let context = RoutingContext {
        schema_version: 1,
        graph,
        operations,
        abstain_option: context.abstain_option.clone(),
    };
    context.validate().map_err(|error| error.to_string())?;
    Ok(context)
}

fn general_route_impl(request: GeneralRouteRequest) -> Result<GeneralRouteResult, String> {
    if request.text.len() > MAX_GENERAL_TEXT_BYTES {
        return Err(format!("text exceeds {MAX_GENERAL_TEXT_BYTES} bytes"));
    }
    if request.context.operations.len() > MAX_GENERAL_OPERATIONS {
        return Err(format!(
            "context has {} operations, limit {MAX_GENERAL_OPERATIONS}",
            request.context.operations.len()
        ));
    }
    let encoded = Encode!(&request.context).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_GENERAL_CONTEXT_BYTES {
        return Err(format!(
            "context is {} bytes, limit {MAX_GENERAL_CONTEXT_BYTES}",
            encoded.len()
        ));
    }
    let fingerprint = gleaph_routing_wire::general_context_fingerprint(&request.context);
    if !request.context.context_fingerprint.is_empty()
        && request.context.context_fingerprint != fingerprint
    {
        return Err("context fingerprint mismatch".into());
    }
    let router_context = wire_to_router_context(&request.context)?;
    let router = frozen_router().map_err(|error| error.to_string())?;
    let (ordered, maximum) = router.predict(&request.text, &router_context);
    let probabilities = router
        .probabilities(&request.text, &router_context)
        .into_iter()
        .map(|(_, probability)| probability as f32)
        .collect::<Vec<_>>();
    let abstain_rule = rule_abstain(&request.text);
    let abstain_threshold = request.context.abstain_threshold as f64;
    let abstain_by_threshold = maximum <= abstain_threshold;
    let selected = if abstain_rule || abstain_by_threshold {
        None
    } else {
        ordered.first().cloned()
    };
    let mut slots = Vec::new();
    let mut incomplete = false;
    if let Some(operation_id) = &selected
        && let Some(card) = router_context
            .operations
            .iter()
            .find(|card| &card.id == operation_id)
    {
        let extraction = extract_slots(&request.text, card);
        incomplete = !extraction.complete();
        slots = extraction
            .values
            .into_values()
            .map(|value| GeneralSlot {
                name: value.name,
                value: match value.value {
                    SlotLiteral::Int(value) => GeneralSlotLiteral::Int(value),
                    SlotLiteral::Text(value) => GeneralSlotLiteral::Text(value),
                },
                surface: value.surface,
                start: value.start as u32,
                end: value.end as u32,
                rule: value.rule,
            })
            .collect();
    }
    Ok(GeneralRouteResult {
        operation: selected,
        probabilities,
        slots,
        incomplete,
        abstain: abstain_rule || abstain_by_threshold || incomplete,
        context_fingerprint: fingerprint,
        model_version: router.artifact().model_version,
    })
}

#[ic_cdk::query]
fn route_general(request: GeneralRouteRequest) -> Result<GeneralRouteResult, String> {
    general_route_impl(request)
}

#[ic_cdk::query]
fn general_route_version() -> u32 {
    GENERAL_ROUTE_VERSION
}

#[ic_cdk::query]
fn routing_model_version() -> Result<u32, String> {
    frozen_router()
        .map(|router| router.artifact().model_version)
        .map_err(|error| error.to_string())
}

#[ic_cdk::query]
fn wire_version() -> u32 {
    WIRE_VERSION
}

/// Test-only instruction probe for the general route benchmark. Compiled only with `bench`.
#[cfg(feature = "bench")]
#[ic_cdk::query]
fn bench_route_general(request: GeneralRouteRequest) -> Result<(GeneralRouteResult, u64), String> {
    let before = ic_cdk::api::performance_counter(0);
    let result = general_route_impl(request)?;
    let after = ic_cdk::api::performance_counter(0);
    Ok((result, after.saturating_sub(before)))
}

#[cfg(test)]
mod general_route_tests {
    use super::*;
    use gleaph_routing_wire::{
        GeneralEdge, GeneralGraphSchema, GeneralLabel, GeneralOperation, GeneralParameter,
        GeneralProperty,
    };

    fn context(operations: usize) -> GeneralRoutingContext {
        let mut cards = vec![
            GeneralOperation {
                id: "films-by-year".into(),
                description: "List films whose year is in a range and whose genre matches.".into(),
                parameters: vec![
                    GeneralParameter {
                        name: "from".into(),
                        parameter_type: "int".into(),
                        required: true,
                    },
                    GeneralParameter {
                        name: "to".into(),
                        parameter_type: "int".into(),
                        required: true,
                    },
                    GeneralParameter {
                        name: "genre".into(),
                        parameter_type: "string".into(),
                        required: true,
                    },
                ],
                result: vec!["title".into()],
            },
            GeneralOperation {
                id: "synopsis-search".into(),
                description: "Search film synopses for a text query.".into(),
                parameters: vec![GeneralParameter {
                    name: "query".into(),
                    parameter_type: "string".into(),
                    required: true,
                }],
                result: vec!["title".into()],
            },
            GeneralOperation {
                id: "similar-films".into(),
                description: "Find films similar to an anchor film.".into(),
                parameters: vec![GeneralParameter {
                    name: "anchor".into(),
                    parameter_type: "string".into(),
                    required: true,
                }],
                result: vec!["title".into()],
            },
        ];
        while cards.len() < operations {
            let index = cards.len();
            cards.push(GeneralOperation {
                id: format!("synopsis-search-alt{index}"),
                description: "Search film synopses for a text query, alternative catalog entry."
                    .into(),
                parameters: vec![GeneralParameter {
                    name: "query".into(),
                    parameter_type: "string".into(),
                    required: true,
                }],
                result: vec!["title".into()],
            });
        }
        let mut context = GeneralRoutingContext {
            schema_version: 1,
            graph: GeneralGraphSchema {
                labels: vec![GeneralLabel {
                    name: "Film".into(),
                    properties: vec![GeneralProperty {
                        name: "title".into(),
                        property_type: "string".into(),
                    }],
                }],
                edges: vec![GeneralEdge {
                    name: "SIMILAR_TO".into(),
                    from_label: "Film".into(),
                    to_label: "Film".into(),
                }],
            },
            operations: cards,
            abstain_option: "none-of-these".into(),
            abstain_threshold: 0.2,
            policy_version: 1,
            context_fingerprint: vec![],
        };
        context.context_fingerprint = gleaph_routing_wire::general_context_fingerprint(&context);
        context
    }

    #[test]
    fn general_route_selects_and_extracts() {
        let result = general_route_impl(GeneralRouteRequest {
            text: "1990年代のSF映画を教えて".into(),
            context: context(6),
        })
        .expect("route");
        assert_eq!(result.operation.as_deref(), Some("films-by-year"));
        assert!(!result.abstain);
        assert!(!result.incomplete);
        assert_eq!(result.slots.len(), 3);
        assert_eq!(result.model_version, 1);
        assert_eq!(result.probabilities.len(), 6);
    }

    #[test]
    fn general_route_abstains_on_oos_and_fingerprint_mismatch() {
        let mut request = GeneralRouteRequest {
            text: "Delete the film document.".into(),
            context: context(6),
        };
        let oos = general_route_impl(request.clone()).expect("route");
        assert!(oos.abstain);
        assert!(oos.operation.is_none());
        request.context.context_fingerprint = vec![0; 32];
        assert!(general_route_impl(request).is_err());
    }

    #[test]
    fn wire_reports_three() {
        assert_eq!(wire_version(), 3);
    }
}
