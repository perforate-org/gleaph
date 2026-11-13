//! High-level ask client: call the Router once and expose the typed result.

use super::AskError;
use super::transport::AskTransport;
use gleaph_router_wire::{Answer, Interpretation, ReadMode};
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
pub struct AskClient {
    transport: Box<dyn AskTransport>,
}

impl AskClient {
    /// Prepare a client over a Router transport.
    pub fn new_text_only(transport: Box<dyn AskTransport>) -> Self {
        Self { transport }
    }

    /// Interpret question text into a prepared operation and typed parameters.
    ///
    /// This is a free Router composite query and never executes a prepared query.
    pub fn interpret(&self, question: &str) -> Result<Interpretation, AskError> {
        self.transport
            .interpret(question)
            .map_err(AskError::Transport)
    }

    /// Interpret and execute the selected read-only prepared query.
    pub fn ask(&self, question: &str, read_mode: ReadMode) -> Result<Answer, AskError> {
        self.transport
            .ask(question, read_mode)
            .map_err(AskError::Transport)
    }

    /// Controller/tenant diagnostic: raw interpretation with threshold forced to zero.
    pub fn debug_interpret(&self, question: &str) -> Result<GeneralRoute, AskError> {
        let wire = self
            .transport
            .debug_interpret(question)
            .map_err(AskError::Transport)?;
        general_route_from_wire(question, wire)
    }

    /// Controller/tenant diagnostic batch over one threshold-zero context.
    pub fn debug_interpret_batch(&self, questions: &[&str]) -> Result<Vec<GeneralRoute>, AskError> {
        let texts: Vec<String> = questions
            .iter()
            .map(|question| (*question).to_owned())
            .collect();
        let wire = self
            .transport
            .debug_interpret_batch(&texts)
            .map_err(AskError::Transport)?;
        if wire.len() != questions.len() {
            return Err(AskError::Wire(
                "debug interpretation reply length does not match the request".to_owned(),
            ));
        }
        questions
            .iter()
            .zip(wire)
            .map(|(question, result)| general_route_from_wire(question, result))
            .collect()
    }

    /// Read the graph's `(abstain_threshold_bps, policy_version)` routing profile.
    pub fn ask_config(&self, graph: &str) -> Result<(u32, u32), AskError> {
        self.transport
            .ask_config(graph)
            .map_err(AskError::Transport)
    }

    /// Enable routing for the graph; returns the deployment inference canister principal.
    pub fn enable_ask(&self, graph: &str) -> Result<candid::Principal, AskError> {
        self.transport
            .enable_ask(graph)
            .map_err(AskError::Transport)
    }

    /// Disable routing for the graph.
    pub fn disable_ask(&self, graph: &str) -> Result<(), AskError> {
        self.transport
            .disable_ask(graph)
            .map_err(AskError::Transport)
    }

    /// Read the graph's declared parameter domains (ADR 0117), sorted by parameter name.
    pub fn parameter_values(
        &self,
        graph: &str,
    ) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, AskError> {
        self.transport
            .parameter_values(graph)
            .map_err(AskError::Transport)
    }

    /// Set or clear one parameter's declared domain. Empty `values` clears it; the Router enforces
    /// controller/tenant authority and the ADR 0117 bounds.
    pub fn set_parameter_values(
        &self,
        graph: &str,
        parameter: &str,
        values: &[gleaph_routing_wire::GeneralDomainValue],
    ) -> Result<(), AskError> {
        self.transport
            .set_parameter_values(graph, parameter, values)
            .map_err(AskError::Transport)
    }

    /// Set the graph's routing profile; the Router enforces controller/tenant authority.
    pub fn set_ask_config(
        &self,
        graph: &str,
        abstain_threshold_bps: u32,
        policy_version: u32,
    ) -> Result<(), AskError> {
        self.transport
            .set_ask_config(graph, abstain_threshold_bps, policy_version)
            .map_err(AskError::Transport)
    }
}

fn general_route_from_wire(
    question: &str,
    wire: GeneralRouteResult,
) -> Result<GeneralRoute, AskError> {
    let character_count = question.chars().count();
    if wire
        .slots
        .iter()
        .any(|slot| slot.start >= slot.end || slot.end as usize > character_count)
    {
        return Err(AskError::Wire(
            "general slot span is outside the question".to_owned(),
        ));
    }
    if wire.context_fingerprint.len() != 32 {
        return Err(AskError::Wire(
            "general route returned an invalid context fingerprint".to_owned(),
        ));
    }
    if wire.incomplete && !wire.abstain {
        return Err(AskError::Wire(
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

impl std::fmt::Debug for AskClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AskClient").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockTransport {
        abstain: bool,
    }

    impl AskTransport for MockTransport {
        fn interpret(&self, _text: &str) -> Result<Interpretation, String> {
            if self.abstain {
                return Ok(Interpretation {
                    status: gleaph_router_wire::InterpretationStatus::Unsupported,
                    operation: None,
                    parameters: Vec::new(),
                    missing: Vec::new(),
                });
            }
            Ok(Interpretation {
                status: gleaph_router_wire::InterpretationStatus::Answered,
                operation: Some("films-by-conditions".into()),
                parameters: vec![
                    gleaph_router_wire::AskParameter {
                        name: "from".into(),
                        value: gleaph_router_wire::AskParameterValue::Int(1990),
                    },
                    gleaph_router_wire::AskParameter {
                        name: "genre".into(),
                        value: gleaph_router_wire::AskParameterValue::Text("SF".into()),
                    },
                ],
                missing: Vec::new(),
            })
        }

        fn ask(&self, text: &str, _read_mode: ReadMode) -> Result<Answer, String> {
            let interpretation = self.interpret(text)?;
            if interpretation.status != gleaph_router_wire::InterpretationStatus::Answered {
                return Ok(Answer {
                    status: gleaph_router_wire::AskStatus::Unsupported,
                    interpretation,
                    rows: None,
                    failure: None,
                });
            }
            Ok(Answer {
                status: gleaph_router_wire::AskStatus::Answered,
                interpretation,
                rows: Some(gleaph_router_wire::AnswerRows {
                    row_count: 2,
                    rows_blob: None,
                }),
                failure: None,
            })
        }

        fn debug_interpret(&self, _text: &str) -> Result<GeneralRouteResult, String> {
            Ok(GeneralRouteResult {
                operation: (!self.abstain).then(|| "films-by-conditions".into()),
                probabilities: vec![0.9, 0.1],
                slots: Vec::new(),
                incomplete: false,
                abstain: self.abstain,
                context_fingerprint: vec![7; 32],
                model_version: 1,
            })
        }

        fn debug_interpret_batch(
            &self,
            texts: &[String],
        ) -> Result<Vec<GeneralRouteResult>, String> {
            texts.iter().map(|_| self.debug_interpret("")).collect()
        }

        fn ask_config(&self, _graph: &str) -> Result<(u32, u32), String> {
            Ok((2_000, 1))
        }

        fn set_ask_config(&self, _graph: &str, bps: u32, policy: u32) -> Result<(), String> {
            (bps == 2_000 && policy == 1)
                .then_some(())
                .ok_or_else(|| "unexpected profile".to_owned())
        }
    }

    #[test]
    fn interpret_exposes_operation_and_parameters() {
        let client = AskClient::new_text_only(Box::new(MockTransport { abstain: false }));
        let interpretation = client
            .interpret("1990年代のSF映画を教えて")
            .expect("interpret");
        assert_eq!(
            interpretation.status,
            gleaph_router_wire::InterpretationStatus::Answered
        );
        assert_eq!(
            interpretation.operation.as_deref(),
            Some("films-by-conditions")
        );
        assert_eq!(interpretation.parameters.len(), 2);
        assert!(interpretation.missing.is_empty());
    }

    #[test]
    fn ask_executes_and_returns_rows() {
        let client = AskClient::new_text_only(Box::new(MockTransport { abstain: false }));
        let answer = client
            .ask("1990年代のSF映画を教えて", ReadMode::Eventual)
            .expect("ask");
        assert_eq!(answer.status, gleaph_router_wire::AskStatus::Answered);
        assert_eq!(answer.rows.expect("rows").row_count, 2);
    }

    #[test]
    fn abstaining_interpretation_is_unsupported() {
        let client = AskClient::new_text_only(Box::new(MockTransport { abstain: true }));
        let interpretation = client.interpret("Delete the film.").expect("interpret");
        assert_eq!(
            interpretation.status,
            gleaph_router_wire::InterpretationStatus::Unsupported
        );
        assert!(interpretation.operation.is_none());
    }
}
