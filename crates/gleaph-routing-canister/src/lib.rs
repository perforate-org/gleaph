//! Inference canister: the frozen schema/catalog-conditioned general router.
//!
//! The canister no longer stores or executes an INT8 routing pack (ADR 0114) and no longer owns a
//! tokenizer (ADR 0116). It executes the query-only general route over a Router-supplied graph
//! context; all routing state lives in the Router.

use candid::Encode;
use glev_general_router::{
    RoutingContext, SlotLiteral,
    context::{EdgeSpec, GraphSchema, LabelSpec, OperationCard, ParameterSpec, PropertySpec},
    extract_slots, frozen_router, rule_abstain,
};

mod domains;

use gleaph_routing_wire::WIRE_VERSION;
pub use gleaph_routing_wire::{
    GENERAL_ROUTE_VERSION, GeneralRouteBatchRequest, GeneralRouteRequest, GeneralRouteResult,
    GeneralRoutingContext, GeneralSlot, GeneralSlotLiteral, MAX_GENERAL_BATCH_QUESTIONS,
    MAX_GENERAL_BATCH_TEXT_BYTES, MAX_GENERAL_CONTEXT_BYTES, MAX_GENERAL_OPERATIONS,
    MAX_GENERAL_TEXT_BYTES,
};

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

fn validate_route_context(
    context: &GeneralRoutingContext,
) -> Result<(Vec<u8>, RoutingContext), String> {
    if context.operations.len() > MAX_GENERAL_OPERATIONS {
        return Err(format!(
            "context has {} operations, limit {MAX_GENERAL_OPERATIONS}",
            context.operations.len()
        ));
    }
    let encoded = Encode!(context).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_GENERAL_CONTEXT_BYTES {
        return Err(format!(
            "context is {} bytes, limit {MAX_GENERAL_CONTEXT_BYTES}",
            encoded.len()
        ));
    }
    let fingerprint = gleaph_routing_wire::general_context_fingerprint(context);
    if !context.context_fingerprint.is_empty() && context.context_fingerprint != fingerprint {
        return Err("context fingerprint mismatch".into());
    }
    let router_context = wire_to_router_context(context)?;
    Ok((fingerprint, router_context))
}

fn route_text(
    text: &str,
    context: &GeneralRoutingContext,
    router_context: &RoutingContext,
    fingerprint: &[u8],
) -> Result<GeneralRouteResult, String> {
    if text.len() > MAX_GENERAL_TEXT_BYTES {
        return Err(format!("text exceeds {MAX_GENERAL_TEXT_BYTES} bytes"));
    }
    let router = frozen_router().map_err(|error| error.to_string())?;
    let (ordered, maximum) = router.predict(text, router_context);
    let probabilities = router
        .probabilities(text, router_context)
        .into_iter()
        .map(|(_, probability)| probability as f32)
        .collect::<Vec<_>>();
    let abstain_rule = rule_abstain(text);
    let abstain_threshold = context.abstain_threshold as f64;
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
        let extraction = extract_slots(text, card);
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
        // ADR 0117: a declared domain completes and canonicalizes slots after rule extraction. The
        // completeness rule then follows the model's required-slot contract, so a completed slot
        // can rescue an operation and an ambiguous one still abstains.
        if let Some(wire_card) = context
            .operations
            .iter()
            .find(|operation| &operation.id == operation_id)
        {
            domains::complete(text, &wire_card.parameters, &mut slots);
            if domains::has_domain(&wire_card.parameters) {
                incomplete = wire_card.parameters.iter().any(|parameter| {
                    parameter.required && !slots.iter().any(|slot| slot.name == parameter.name)
                });
            }
        }
    }
    Ok(GeneralRouteResult {
        operation: selected,
        probabilities,
        slots,
        incomplete,
        abstain: abstain_rule || abstain_by_threshold || incomplete,
        context_fingerprint: fingerprint.to_vec(),
        model_version: router.artifact().model_version,
    })
}

fn general_route_impl(request: GeneralRouteRequest) -> Result<GeneralRouteResult, String> {
    let (fingerprint, router_context) = validate_route_context(&request.context)?;
    route_text(
        &request.text,
        &request.context,
        &router_context,
        &fingerprint,
    )
}

/// Raw calibration batch: validate the shared context once, then score each text in order. This
/// is the threshold-zero diagnostic used by the calibration tool; it is not a production route.
#[ic_cdk::query]
fn route_general_batch(
    request: GeneralRouteBatchRequest,
) -> Result<Vec<GeneralRouteResult>, String> {
    if request.texts.is_empty() || request.texts.len() > MAX_GENERAL_BATCH_QUESTIONS {
        return Err(format!(
            "batch must contain 1..={MAX_GENERAL_BATCH_QUESTIONS} questions"
        ));
    }
    let total_bytes: usize = request.texts.iter().map(String::len).sum();
    if total_bytes > MAX_GENERAL_BATCH_TEXT_BYTES {
        return Err(format!(
            "batch texts are {total_bytes} bytes, limit {MAX_GENERAL_BATCH_TEXT_BYTES}"
        ));
    }
    let (fingerprint, router_context) = validate_route_context(&request.context)?;
    request
        .texts
        .iter()
        .map(|text| route_text(text, &request.context, &router_context, &fingerprint))
        .collect()
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
                        domain: Vec::new(),
                    },
                    GeneralParameter {
                        name: "to".into(),
                        parameter_type: "int".into(),
                        required: true,
                        domain: Vec::new(),
                    },
                    GeneralParameter {
                        name: "genre".into(),
                        parameter_type: "string".into(),
                        required: true,
                        domain: Vec::new(),
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
                    domain: Vec::new(),
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
                    domain: Vec::new(),
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
                    domain: Vec::new(),
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
    fn raw_batch_shares_one_context_and_preserves_order() {
        let context = context(6);
        let request = GeneralRouteBatchRequest {
            texts: vec![
                "1990年代のSF映画を教えて".to_owned(),
                "Delete every film.".to_owned(),
            ],
            context,
        };
        let results = route_general_batch(request).expect("batch");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].operation.as_deref(), Some("films-by-year"));
        assert!(results[1].abstain);
        assert_eq!(
            results[0].context_fingerprint,
            results[1].context_fingerprint
        );
    }

    #[test]
    fn wire_reports_six() {
        assert_eq!(wire_version(), 6);
    }
}
