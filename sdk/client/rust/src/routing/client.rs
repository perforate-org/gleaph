//! High-level routing client: tokenize, call the inference canister once, bind slot values.

use std::path::Path;
use std::sync::OnceLock;

use super::RoutingError;
use super::slots::{EntityVocab, SlotBindings, SpannedTag, bind_spans};
use super::tokenizer::{RoutingTokenizer, TokenizedQuestion};
use super::transport::RoutingTransport;
use super::wire::SlotSpanWire;

/// Result of one `route_and_extract` call.
#[derive(Debug, Clone)]
pub struct Extraction {
    /// Routing probabilities in `options` order.
    pub probabilities: Vec<f32>,
    /// Bio spans over the question tokens.
    pub spans: Vec<SpannedTag>,
    /// Option names in probability order (from the pack via the canister).
    pub options: Vec<String>,
}

/// Abstain policy applied to the routing probabilities.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbstainPolicy {
    /// Temperature applied to the probabilities before comparing with the threshold.
    pub temperature: f32,
    /// `none-of-these` probability at or above which the question abstains.
    pub none_threshold: f32,
}

impl AbstainPolicy {
    /// Short questions keep the training-time policy (T = 2.5, none >= 0.23).
    pub const SHORT: Self = Self {
        temperature: 2.5,
        none_threshold: 0.23,
    };
    /// Long questions with substantial CJK content use a higher abstain threshold because the
    /// context windows raise the none probability; measured on JEMHop contexts.
    pub const CJK_LONG: Self = Self {
        temperature: 1.0,
        none_threshold: 0.50,
    };

    /// Questions longer than 96 tokens need the tighter threshold: the real-OOS refusal at 128
    /// tokens falls below 90% at 0.30 and recovers at 0.25.
    pub const VERY_LONG: Self = Self {
        temperature: 1.0,
        none_threshold: 0.25,
    };

    /// Chunked long questions use the real-document calibrated policy (T = 1.0, none >= 0.30);
    /// synthetic fillers need a lower threshold than real prose, but real corpora keep holdout OOS
    /// refusal around 95% at 0.30 while covering ~10pt more questions than 0.25.
    pub const LONG: Self = Self {
        temperature: 1.0,
        none_threshold: 0.30,
    };

    /// Policy for a question with `tokens` question tokens and a CJK character fraction.
    pub fn for_question(tokens: usize, cjk_fraction: f32) -> Self {
        if tokens <= 32 {
            Self::SHORT
        } else if cjk_fraction >= 0.10 {
            Self::CJK_LONG
        } else if tokens > 96 {
            Self::VERY_LONG
        } else {
            Self::LONG
        }
    }
}

impl Extraction {
    /// Route or abstain under the length-aware policy.
    ///
    /// The probabilities are temperature-scaled and the method returns `None` when the abstain
    /// option reaches the policy threshold; otherwise the best non-abstain option wins (earlier
    /// options break ties).
    pub fn decision(&self, token_count: usize, cjk_fraction: f32) -> Option<&str> {
        let policy = AbstainPolicy::for_question(token_count, cjk_fraction);
        let abstain = self
            .options
            .iter()
            .position(|option| option == "none-of-these");
        let scaled: Vec<f32> = self
            .probabilities
            .iter()
            .map(|probability| {
                if policy.temperature == 1.0 {
                    *probability
                } else {
                    probability.powf(1.0 / policy.temperature)
                }
            })
            .collect();
        let total: f32 = scaled.iter().sum();
        let normalized: Vec<f32> = if total > 0.0 {
            scaled.iter().map(|value| value / total).collect()
        } else {
            scaled
        };
        if let Some(index) = abstain
            && normalized
                .get(index)
                .is_some_and(|value| *value >= policy.none_threshold)
        {
            return None;
        }
        let mut best: Option<(usize, f32)> = None;
        for (index, probability) in normalized.iter().enumerate() {
            if Some(index) == abstain {
                continue;
            }
            if best.is_none_or(|(_, current)| *probability > current) {
                best = Some((index, *probability));
            }
        }
        best.and_then(|(index, _)| self.options.get(index))
            .map(String::as_str)
    }

    /// Best routing option when it is not the abstain option.
    pub fn best(&self) -> Option<&str> {
        let (index, _) = self
            .probabilities
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))?;
        self.options.get(index).map(String::as_str)
    }
}

/// Client for the question-only routing prompt.
pub struct RoutingClient {
    tokenizer: RoutingTokenizer,
    transport: Box<dyn RoutingTransport>,
    options: OnceLock<Vec<String>>,
}

impl RoutingClient {
    /// Load the tokenizer and prepare the transport. Option names are fetched lazily on the first
    /// `route_and_extract` call.
    pub fn new(
        tokenizer_path: &Path,
        transport: Box<dyn RoutingTransport>,
    ) -> Result<Self, RoutingError> {
        Ok(Self {
            tokenizer: RoutingTokenizer::from_file(tokenizer_path)?,
            transport,
            options: OnceLock::new(),
        })
    }

    /// Tokenise one question locally.
    pub fn tokenize(&self, question: &str) -> Result<TokenizedQuestion, RoutingError> {
        self.tokenizer.tokenize(question)
    }

    /// Decide the operation for `extraction`, using the tokenizer to detect CJK-heavy long
    /// questions (which need the higher CJK abstain threshold).
    pub fn decide<'a>(
        &self,
        extraction: &'a Extraction,
        question: &TokenizedQuestion,
    ) -> Option<&'a str> {
        let cjk_fraction = self.tokenizer.cjk_fraction(&question.ids);
        extraction.decision(question.ids.len(), cjk_fraction)
    }

    /// Call the inference canister once and return probabilities plus spans.
    pub fn route_and_extract(
        &self,
        question: &TokenizedQuestion,
    ) -> Result<Extraction, RoutingError> {
        let wire = self
            .transport
            .route_and_extract(&question.ids)
            .map_err(RoutingError::Transport)?;
        if self.options.get().is_none() {
            let options = self
                .transport
                .route_options()
                .map_err(RoutingError::Transport)?;
            let _ = self.options.set(options);
        }
        let options = self
            .options
            .get()
            .ok_or_else(|| RoutingError::Wire("route options are unknown".into()))?;
        if wire.probabilities.len() != options.len() {
            return Err(RoutingError::Wire(format!(
                "got {} probabilities for {} options",
                wire.probabilities.len(),
                options.len()
            )));
        }
        if wire
            .spans
            .iter()
            .any(|span| span.start >= span.end || span.end as usize > question.ids.len())
        {
            return Err(RoutingError::Wire(
                "slot span is outside the token range".into(),
            ));
        }
        Ok(Extraction {
            probabilities: wire.probabilities,
            spans: wire
                .spans
                .into_iter()
                .map(
                    |SlotSpanWire {
                         tag,
                         start,
                         end,
                         score,
                     }| SpannedTag {
                        tag,
                        start: start as usize,
                        end: end as usize,
                        score,
                    },
                )
                .collect(),
            options: options.clone(),
        })
    }

    /// Bind the operation's slot values from the model spans, falling back to the deterministic
    /// rule extractor when the spans cannot be bound (the extractor is fail-closed on ambiguity).
    pub fn bind_slots_with_fallback(
        &self,
        question: &str,
        tokenized: &TokenizedQuestion,
        extraction: &Extraction,
        operation: &str,
        vocab: &EntityVocab,
    ) -> Result<SlotBindings, RoutingError> {
        match self.bind_slots(question, tokenized, extraction, operation, vocab) {
            Ok(bindings) => Ok(bindings),
            Err(_) => Ok(super::slots::extract(question, operation, vocab)?),
        }
    }

    /// Bind the operation's slot values from the model spans.
    pub fn bind_slots(
        &self,
        question: &str,
        tokenized: &TokenizedQuestion,
        extraction: &Extraction,
        operation: &str,
        vocab: &EntityVocab,
    ) -> Result<SlotBindings, RoutingError> {
        // The canister returns spans over the question tokens, matching the tokenizer offsets.
        let spans = &extraction.spans;
        Ok(bind_spans(
            question,
            &tokenized.offsets,
            spans,
            operation,
            vocab,
        )?)
    }
}

impl std::fmt::Debug for RoutingClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoutingClient")
            .field("tokenizer", &self.tokenizer)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::tokenizer::TokenizedQuestion;
    use crate::routing::wire::{RouteAndExtractWire, SlotSpanWire};

    struct MockTransport {
        calls: std::sync::atomic::AtomicUsize,
        spans: Vec<SlotSpanWire>,
    }

    impl RoutingTransport for MockTransport {
        fn route_and_extract(&self, token_ids: &[u32]) -> Result<RouteAndExtractWire, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(!token_ids.is_empty());
            Ok(RouteAndExtractWire {
                probabilities: vec![0.01, 0.01, 0.95, 0.01, 0.01, 0.01],
                spans: self.spans.clone(),
            })
        }

        fn route_options(&self) -> Result<Vec<String>, String> {
            Ok(vec![
                "citation-reach".into(),
                "document-text-search".into(),
                "shortest-path".into(),
                "team-readable-documents".into(),
                "variable-length-reach".into(),
                "none-of-these".into(),
            ])
        }
    }

    fn span_for(question: &str, tokenized: &TokenizedQuestion, needle: &str) -> (u32, u32) {
        let start = question.find(needle).expect("needle");
        let end = start + needle.len();
        let first = tokenized
            .offsets
            .iter()
            .position(|(token_start, token_end)| *token_start <= start && *token_end > start)
            .expect("first token");
        let last = tokenized
            .offsets
            .iter()
            .rposition(|(token_start, token_end)| *token_start < end && *token_end >= end)
            .expect("last token");
        (first as u32, (last + 1) as u32)
    }

    fn extraction(none: f32, option: f32) -> Extraction {
        Extraction {
            probabilities: vec![option, option, option, option, option, none],
            spans: Vec::new(),
            options: vec![
                "citation-reach".into(),
                "document-text-search".into(),
                "shortest-path".into(),
                "team-readable-documents".into(),
                "variable-length-reach".into(),
                "none-of-these".into(),
            ],
        }
    }

    #[test]
    fn abstain_policy_is_length_aware() {
        // Short: T=2.5 pushes a 0.25 none probability below 0.23 after normalisation.
        assert_eq!(
            extraction(0.25, 0.15).decision(10, 0.0),
            Some("citation-reach")
        );
        // Short: a dominant none abstains.
        assert_eq!(extraction(0.5, 0.1).decision(10, 0.0), None);
        // Long: T=1 and a 0.30 threshold; 0.25 routes, 0.30 abstains.
        assert_eq!(
            extraction(0.25, 0.15).decision(40, 0.0),
            Some("citation-reach")
        );
        assert_eq!(extraction(0.30, 0.14).decision(40, 0.0), None);
        // Very long English questions use 0.25; 0.28 abstains at 128 tokens but routes at 96.
        assert_eq!(
            extraction(0.28, 0.144).decision(96, 0.0),
            Some("citation-reach")
        );
        assert_eq!(extraction(0.28, 0.144).decision(128, 0.0), None);
        assert_eq!(AbstainPolicy::for_question(96, 0.0), AbstainPolicy::LONG);
        assert_eq!(
            AbstainPolicy::for_question(128, 0.0),
            AbstainPolicy::VERY_LONG
        );
        // CJK long questions use a 0.50 threshold; 0.45 still routes, 0.55 abstains.
        assert_eq!(
            extraction(0.45, 0.11).decision(40, 0.5),
            Some("citation-reach")
        );
        assert_eq!(extraction(0.55, 0.09).decision(40, 0.5), None);
        assert_eq!(AbstainPolicy::for_question(32, 0.9), AbstainPolicy::SHORT);
        assert_eq!(AbstainPolicy::for_question(33, 0.0), AbstainPolicy::LONG);
        assert_eq!(
            AbstainPolicy::for_question(33, 0.5),
            AbstainPolicy::CJK_LONG
        );
    }

    #[test]
    fn bind_falls_back_to_the_rule_extractor() {
        let path = match std::env::var_os("GLEAPH_TOKENIZER") {
            Some(path) => path,
            None => return,
        };
        let transport = MockTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            spans: Vec::new(),
        };
        let client = RoutingClient::new(Path::new(&path), Box::new(transport)).expect("client");
        let question = "What is the shortest path between Vector search and Traversal?";
        let tokenized = client.tokenize(question).expect("tokenize");
        let extraction = client.route_and_extract(&tokenized).expect("extract");
        let mut vocab = EntityVocab::default();
        vocab.concepts = vec!["Vector search".into(), "Traversal".into()];
        let bindings = client
            .bind_slots_with_fallback(question, &tokenized, &extraction, "shortest-path", &vocab)
            .expect("fallback bind");
        assert_eq!(bindings.from.as_deref(), Some("Vector search"));
        assert_eq!(bindings.to.as_deref(), Some("Traversal"));
    }

    #[test]
    fn client_routes_and_binds_slots() {
        let path = match std::env::var_os("GLEAPH_TOKENIZER") {
            Some(path) => path,
            None => return,
        };
        let question = "What is the shortest path between Vector search and Traversal?";
        let tokenizer = RoutingTokenizer::from_file(Path::new(&path)).expect("tokenizer");
        let probe = tokenizer.tokenize(question).expect("tokenize");
        let transport = MockTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            spans: vec![
                SlotSpanWire {
                    tag: "concept".into(),
                    start: span_for(question, &probe, "Vector search").0,
                    end: span_for(question, &probe, "Vector search").1,
                    score: 0.5,
                },
                SlotSpanWire {
                    tag: "concept".into(),
                    start: span_for(question, &probe, "Traversal").0,
                    end: span_for(question, &probe, "Traversal").1,
                    score: 0.5,
                },
            ],
        };
        let client = RoutingClient::new(Path::new(&path), Box::new(transport)).expect("client");
        let tokenized = client.tokenize(question).expect("tokenize");
        let extraction = client.route_and_extract(&tokenized).expect("extract");
        assert_eq!(extraction.best(), Some("shortest-path"));
        let mut vocab = EntityVocab::default();
        vocab.concepts = vec!["Vector search".into(), "Traversal".into()];
        let bindings = client
            .bind_slots(question, &tokenized, &extraction, "shortest-path", &vocab)
            .expect("bind");
        assert_eq!(bindings.from.as_deref(), Some("Vector search"));
        assert_eq!(bindings.to.as_deref(), Some("Traversal"));
    }
}
