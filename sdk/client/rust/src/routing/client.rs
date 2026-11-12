//! High-level general-routing client: call the Router once and expose the typed result.

use super::RoutingError;
use super::transport::RoutingTransport;
use gleaph_routing_wire::{GeneralRouteResult, GeneralSlotLiteral};

/// Result of one Router general-route call (ADR 0114): selected operation, typed slots and the
/// fail-closed abstain/incomplete flags.
#[derive(Debug, Clone)]
pub struct GeneralRoute {
    /// Selected prepared operation, absent when the decision abstains.
    pub operation: Option<String>,
    /// Probabilities in context operation order.
    pub probabilities: Vec<f32>,
    /// Typed slot values with character spans.
    pub slots: Vec<gleaph_routing_wire::GeneralSlot>,
    /// At least one required slot is missing or ambiguous.
    pub incomplete: bool,
    /// The model or policy decided not to execute.
    pub abstain: bool,
    /// Router context fingerprint (32 bytes).
    pub context_fingerprint: Vec<u8>,
    /// Frozen routing model version.
    pub model_version: u32,
}

impl GeneralRoute {
    /// The routable operation, or `None` when the result abstains or is incomplete.
    pub fn decision(&self) -> Option<&str> {
        if self.abstain || self.incomplete {
            return None;
        }
        self.operation.as_deref()
    }

    /// First text value for a slot name, if present.
    pub fn text_slot(&self, name: &str) -> Option<&str> {
        self.slots.iter().find_map(|slot| {
            (slot.name == name).then_some(match &slot.value {
                GeneralSlotLiteral::Text(value) => Some(value.as_str()),
                GeneralSlotLiteral::Int(_) => None,
            })?
        })
    }

    /// First integer value for a slot name, if present.
    pub fn int_slot(&self, name: &str) -> Option<i64> {
        self.slots.iter().find_map(|slot| {
            (slot.name == name).then_some(match slot.value {
                GeneralSlotLiteral::Int(value) => Some(value),
                GeneralSlotLiteral::Text(_) => None,
            })?
        })
    }
}

/// Client for the Router's general route. No local tokenizer or neural pack is loaded.
pub struct RoutingClient {
    transport: Box<dyn RoutingTransport>,
}

impl RoutingClient {
    /// Prepare a client over a Router transport.
    pub fn new_text_only(transport: Box<dyn RoutingTransport>) -> Self {
        Self { transport }
    }

    /// Route question text through the Router and validate the fail-closed reply contract.
    pub fn route_general(&self, question: &str) -> Result<GeneralRoute, RoutingError> {
        let wire: GeneralRouteResult = self
            .transport
            .route_general(question)
            .map_err(RoutingError::Transport)?;
        general_route_from_wire(question, wire)
    }

    /// Controller-only raw route for calibration (threshold forced to zero by the Router).
    pub fn route_general_raw(&self, question: &str) -> Result<GeneralRoute, RoutingError> {
        let wire: GeneralRouteResult = self
            .transport
            .route_general_raw(question)
            .map_err(RoutingError::Transport)?;
        general_route_from_wire(question, wire)
    }

    /// Raw calibration batch over one shared context (Router composite query, threshold zero).
    pub fn route_general_raw_batch(
        &self,
        questions: &[&str],
    ) -> Result<Vec<GeneralRoute>, RoutingError> {
        let texts: Vec<String> = questions
            .iter()
            .map(|question| (*question).to_owned())
            .collect();
        let wire = self
            .transport
            .route_general_raw_batch(&texts)
            .map_err(RoutingError::Transport)?;
        if wire.len() != questions.len() {
            return Err(RoutingError::Wire(
                "general batch reply length does not match the request".to_owned(),
            ));
        }
        questions
            .iter()
            .zip(wire)
            .map(|(question, result)| general_route_from_wire(question, result))
            .collect()
    }

    /// Read the graph's `(abstain_threshold_bps, policy_version)` routing profile.
    pub fn routing_profile(&self, graph: &str) -> Result<(u32, u32), RoutingError> {
        self.transport
            .routing_profile(graph)
            .map_err(RoutingError::Transport)
    }

    /// Set the graph's routing profile; the Router enforces controller/tenant authority.
    pub fn set_routing_profile(
        &self,
        graph: &str,
        abstain_threshold_bps: u32,
        policy_version: u32,
    ) -> Result<(), RoutingError> {
        self.transport
            .set_routing_profile(graph, abstain_threshold_bps, policy_version)
            .map_err(RoutingError::Transport)
    }
}

fn general_route_from_wire(
    question: &str,
    wire: GeneralRouteResult,
) -> Result<GeneralRoute, RoutingError> {
    let character_count = question.chars().count();
    if wire
        .slots
        .iter()
        .any(|slot| slot.start >= slot.end || slot.end as usize > character_count)
    {
        return Err(RoutingError::Wire(
            "general slot span is outside the question".to_owned(),
        ));
    }
    if wire.context_fingerprint.len() != 32 {
        return Err(RoutingError::Wire(
            "general route returned an invalid context fingerprint".to_owned(),
        ));
    }
    if wire.incomplete && !wire.abstain {
        return Err(RoutingError::Wire(
            "incomplete general route must abstain".to_owned(),
        ));
    }
    Ok(GeneralRoute {
        operation: wire.operation,
        probabilities: wire.probabilities,
        slots: wire.slots,
        incomplete: wire.incomplete,
        abstain: wire.abstain,
        context_fingerprint: wire.context_fingerprint,
        model_version: wire.model_version,
    })
}

impl std::fmt::Debug for RoutingClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoutingClient")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTransport {
        abstain: bool,
    }

    impl RoutingTransport for MockTransport {
        fn route_general(&self, _text: &str) -> Result<GeneralRouteResult, String> {
            Ok(GeneralRouteResult {
                operation: (!self.abstain).then(|| "films-by-conditions".into()),
                probabilities: vec![0.9, 0.1],
                slots: if self.abstain {
                    vec![]
                } else {
                    vec![
                        gleaph_routing_wire::GeneralSlot {
                            name: "from".into(),
                            value: GeneralSlotLiteral::Int(1990),
                            surface: "1990".into(),
                            start: 0,
                            end: 4,
                            rule: "range".into(),
                        },
                        gleaph_routing_wire::GeneralSlot {
                            name: "genre".into(),
                            value: GeneralSlotLiteral::Text("SF".into()),
                            surface: "SF".into(),
                            start: 5,
                            end: 7,
                            rule: "category".into(),
                        },
                    ]
                },
                incomplete: false,
                abstain: self.abstain,
                context_fingerprint: vec![7; 32],
                model_version: 1,
            })
        }

        fn route_general_raw(&self, text: &str) -> Result<GeneralRouteResult, String> {
            self.route_general(text)
        }

        fn route_general_raw_batch(
            &self,
            texts: &[String],
        ) -> Result<Vec<GeneralRouteResult>, String> {
            texts.iter().map(|text| self.route_general(text)).collect()
        }

        fn routing_profile(&self, _graph: &str) -> Result<(u32, u32), String> {
            Ok((2_000, 1))
        }

        fn set_routing_profile(&self, _graph: &str, bps: u32, policy: u32) -> Result<(), String> {
            (bps == 2_000 && policy == 1)
                .then_some(())
                .ok_or_else(|| "unexpected profile".to_owned())
        }
    }

    #[test]
    fn general_route_exposes_decision_and_typed_slots() {
        let client = RoutingClient::new_text_only(Box::new(MockTransport { abstain: false }));
        let route = client
            .route_general("1990年代のSF映画を教えて")
            .expect("general route");
        assert_eq!(route.decision(), Some("films-by-conditions"));
        assert_eq!(route.int_slot("from"), Some(1990));
        assert_eq!(route.text_slot("genre"), Some("SF"));
        assert_eq!(route.model_version, 1);
        assert_eq!(route.context_fingerprint, vec![7; 32]);
        let raw = client
            .route_general_raw("1990年代のSF映画を教えて")
            .expect("raw route");
        assert_eq!(raw.operation.as_deref(), Some("films-by-conditions"));
    }

    #[test]
    fn abstaining_route_has_no_decision() {
        let client = RoutingClient::new_text_only(Box::new(MockTransport { abstain: true }));
        let route = client
            .route_general("Delete the film.")
            .expect("general route");
        assert!(route.abstain);
        assert_eq!(route.decision(), None);
    }
}
