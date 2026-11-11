//! Jev-compatible decision-model client used by `gleaph ask`.
//!
//! The wire shape is the System One request (`POST /v1/systemone`): one shared `state` plus typed
//! questions, answered in a single forward pass. `gleaph ask` uses parallel `noul` (yes/no)
//! questions — one per prepared-query candidate — so the returned probabilities are directly
//! comparable, the arrangement DecisionRAG measured as better than independent scoring.
//!
//! No model is required to use the CLI: this client is only constructed when `--model-url` is
//! given. It performs no authorization and sees only the candidate descriptions that `gleaph ask`
//! already derived from the local prepared directory.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Serialize;

/// Failure while asking a decision model.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// Building or sending the HTTP request failed.
    #[error("request failed: {0}")]
    Request(String),
    /// The endpoint returned a non-success status.
    #[error("endpoint returned HTTP {0}")]
    Status(u16),
    /// The response body was not the expected JSON shape.
    #[error("invalid response: {0}")]
    Response(String),
    /// The response did not answer a candidate question.
    #[error("response did not answer candidate {0:?}")]
    MissingAnswer(String),
}

/// How to pose the candidate question.
///
/// Decision models are trained on specific primitives. `Choice` normalises across a declared option
/// set and is the right shape for routing a question onto a small catalog; `Noul` (one yes/no
/// question per candidate) is the shape DecisionRAG uses for large candidate levels where a
/// multiple-choice request would exceed the option cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum JudgeMode {
    /// Choose per request: `Choice` when the candidate set fits the option cap, else `Noul`.
    Auto,
    /// One multiple-choice question over all candidates.
    Choice,
    /// One yes/no question per candidate.
    Noul,
}

impl JudgeMode {
    /// Resolve `Auto` against the candidate count.
    pub fn resolve(self, candidate_count: usize) -> Self {
        match self {
            JudgeMode::Auto if candidate_count <= MAX_CHOICE_OPTIONS => JudgeMode::Choice,
            JudgeMode::Auto => JudgeMode::Noul,
            other => other,
        }
    }
}

/// Option cap for one multiple-choice question.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// One candidate's calibrated yes/no probability.
#[derive(Clone, Debug, PartialEq)]
pub struct Judgement {
    /// Prepared-operation name.
    pub name: String,
    /// Probability that this candidate is the query the question asks for.
    pub noul: f64,
}

/// A decision-model client for one endpoint and model.
#[derive(Clone, Debug)]
pub struct DecisionModelClient {
    endpoint: String,
    model: String,
    api_key: Option<String>,
    timeout: Duration,
}

impl DecisionModelClient {
    /// Create a client. `endpoint` is the full System One URL.
    pub fn new(
        endpoint: String,
        model: String,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            endpoint,
            model,
            api_key,
            timeout,
        }
    }

    /// Ask one yes/no question per candidate against a shared context.
    ///
    /// `candidates` is `(operation name, description)`. The returned list preserves candidate
    /// order; ranking is the caller's job so that abstain policy stays in one place.
    pub fn judge(
        &self,
        question: &str,
        candidates: &[(String, String)],
        mode: JudgeMode,
    ) -> Result<Vec<Judgement>, ModelError> {
        let mode = mode.resolve(candidates.len());
        let request = build_request(&self.model, question, candidates, mode);
        let client = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|error| ModelError::Request(error.to_string()))?;
        let mut builder = client.post(&self.endpoint).json(&request);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder
            .send()
            .map_err(|error| ModelError::Request(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ModelError::Status(status.as_u16()));
        }
        let body = response
            .text()
            .map_err(|error| ModelError::Request(error.to_string()))?;
        parse_judgements(&body, candidates, mode)
    }
}

/// The request body sent to the endpoint.
#[derive(Debug, Serialize)]
struct SystemOneRequest {
    model: String,
    state: String,
    questions: BTreeMap<String, Question>,
}

#[derive(Debug, Serialize)]
struct Question {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    criteria: Option<BTreeMap<String, String>>,
}

fn question_id(index: usize) -> String {
    format!("candidate_{index}")
}

fn build_request(
    model: &str,
    question: &str,
    candidates: &[(String, String)],
    mode: JudgeMode,
) -> SystemOneRequest {
    match mode {
        JudgeMode::Choice => {
            let mut criteria = BTreeMap::new();
            for (name, description) in candidates {
                criteria.insert(name.clone(), description.clone());
            }
            let mut questions = BTreeMap::new();
            questions.insert(
                "route".to_owned(),
                Question {
                    kind: "choice",
                    instructions: "Which prepared query answers this question?".to_owned(),
                    criteria: Some(criteria),
                },
            );
            SystemOneRequest {
                model: model.to_owned(),
                state: question.to_owned(),
                questions,
            }
        }
        JudgeMode::Noul | JudgeMode::Auto => {
            let mut state = String::from("Question: ");
            state.push_str(question);
            state.push_str("\nCandidate prepared queries:\n");
            for (index, (name, description)) in candidates.iter().enumerate() {
                state.push_str(&format!("{index}. {name}: {description}\n"));
            }
            let mut questions = BTreeMap::new();
            for (index, (name, _)) in candidates.iter().enumerate() {
                questions.insert(
                    question_id(index),
                    Question {
                        kind: "noul",
                        instructions: format!(
                            "Is candidate {index} ({name}) the prepared query that answers the question?"
                        ),
                        criteria: None,
                    },
                );
            }
            SystemOneRequest {
                model: model.to_owned(),
                state,
                questions,
            }
        }
    }
}

/// Parse a System One response into per-candidate probabilities.
fn parse_judgements(
    body: &str,
    candidates: &[(String, String)],
    mode: JudgeMode,
) -> Result<Vec<Judgement>, ModelError> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| ModelError::Response(error.to_string()))?;
    let answers = value
        .get("answers")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| ModelError::Response("missing `answers` object".into()))?;
    if mode == JudgeMode::Choice {
        let answer = answers
            .get("route")
            .ok_or_else(|| ModelError::Response("missing route answer".into()))?;
        let choice = answer
            .get("choice")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ModelError::Response("missing choice".into()))?;
        if !candidates.iter().any(|(name, _)| name == choice) {
            return Err(ModelError::Response(format!(
                "choice {choice:?} is not a declared candidate"
            )));
        }
        let probabilities = answer
            .get("probabilities")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| ModelError::Response("missing probabilities".into()))?;
        let mut judgements = Vec::with_capacity(candidates.len());
        for (name, _) in candidates {
            let noul = probabilities
                .get(name)
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            judgements.push(Judgement {
                name: name.clone(),
                noul,
            });
        }
        return Ok(judgements);
    }
    let mut judgements = Vec::with_capacity(candidates.len());
    for (index, (name, _)) in candidates.iter().enumerate() {
        let answer = answers
            .get(&question_id(index))
            .ok_or_else(|| ModelError::MissingAnswer(name.clone()))?;
        let noul = answer
            .get("noul")
            .and_then(serde_json::Value::as_f64)
            .or_else(|| answer.as_f64())
            .ok_or_else(|| ModelError::MissingAnswer(name.clone()))?;
        if !(0.0..=1.0).contains(&noul) {
            return Err(ModelError::Response(format!(
                "candidate {name:?} returned noul outside 0..1: {noul}"
            )));
        }
        judgements.push(Judgement {
            name: name.clone(),
            noul,
        });
    }
    Ok(judgements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn candidates() -> Vec<(String, String)> {
        vec![
            ("variable-length-reach".into(), "reachable concepts".into()),
            ("citation-reach".into(), "documents through CITES".into()),
        ]
    }

    #[test]
    fn request_is_system_one_shaped() {
        let request = build_request(
            "jev-latest",
            "which documents cite?",
            &candidates(),
            JudgeMode::Noul,
        );
        let value = serde_json::to_value(&request).expect("serialise");
        assert_eq!(value["model"], "jev-latest");
        let state = value["state"].as_str().expect("state");
        assert!(state.contains("Question: which documents cite?"));
        assert!(state.contains("0. variable-length-reach"));
        let questions = value["questions"].as_object().expect("questions");
        assert_eq!(questions.len(), 2);
        assert_eq!(questions["candidate_0"]["type"], "noul");
    }

    #[test]
    fn parse_reads_nouls_in_candidate_order() {
        let body = r#"{"answers":{"candidate_1":{"noul":0.9},"candidate_0":{"noul":0.1}}}"#;
        let judgements = parse_judgements(body, &candidates(), JudgeMode::Noul).expect("parse");
        assert_eq!(judgements[0].name, "variable-length-reach");
        assert_eq!(judgements[0].noul, 0.1);
        assert_eq!(judgements[1].noul, 0.9);
    }

    #[test]
    fn parse_reports_a_missing_answer() {
        let error = parse_judgements(
            r#"{"answers":{"candidate_0":{"noul":0.5}}}"#,
            &candidates(),
            JudgeMode::Noul,
        )
        .expect_err("missing answer");
        assert!(matches!(error, ModelError::MissingAnswer(name) if name == "citation-reach"));
    }

    #[test]
    fn judge_posts_the_request_and_reads_the_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_in_server = Arc::clone(&captured);
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).expect("read");
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let body_start = header_end + 4;
                    let content_length = text[..header_end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if buffer.len() >= body_start + content_length {
                        *captured_in_server.lock().expect("lock") = text;
                        break;
                    }
                }
            }
            let body = r#"{"answers":{"candidate_0":{"noul":0.2},"candidate_1":{"noul":0.8}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        let client = DecisionModelClient::new(
            format!("http://{address}/v1/systemone"),
            "jev-latest".into(),
            None,
            Duration::from_secs(5),
        );
        let judgements = client
            .judge("which documents cite?", &candidates(), JudgeMode::Noul)
            .expect("judge");
        handle.join().expect("server");
        assert_eq!(judgements[1].noul, 0.8);
        let request = captured.lock().expect("lock").clone();
        assert!(request.starts_with("POST /v1/systemone"));
        assert!(request.contains("\"model\":\"jev-latest\""));
        assert!(request.contains("\"type\":\"noul\""));
    }
}
