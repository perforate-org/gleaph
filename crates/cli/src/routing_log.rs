//! Opt-in structured routing log for offline quality analysis.
//!
//! Disabled unless `--routing-log <path>` or `GLEAPH_ROUTING_LOG=<path>` is given. Each line is one
//! JSON object with the general-route decision, probabilities and typed slots. Raw question text is
//! **not** written by default; set `GLEAPH_ROUTING_LOG_QUESTIONS=1` to include a PII-redacted copy
//! (URLs, emails and long digit runs are replaced). The question is always reduced to a SHA-256
//! digest for deduplication.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gleaph_sdk::routing::Interpretation;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Append-only writer for routing events; construction never fails the command.
pub struct RoutingLog {
    file: Option<File>,
    raw_questions: bool,
}

impl RoutingLog {
    /// Open the log from the CLI flag or `GLEAPH_ROUTING_LOG`; failures only warn on stderr.
    pub fn from_args(flag: Option<&Path>) -> Self {
        let path = flag
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("GLEAPH_ROUTING_LOG").map(PathBuf::from));
        let Some(path) = path else {
            return Self {
                file: None,
                raw_questions: false,
            };
        };
        let raw_questions = std::env::var("GLEAPH_ROUTING_LOG_QUESTIONS")
            .map(|value| value == "1")
            .unwrap_or(false);
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Self {
                file: Some(file),
                raw_questions,
            },
            Err(error) => {
                eprintln!(
                    "warning: cannot open routing log {}: {error}",
                    path.display()
                );
                Self {
                    file: None,
                    raw_questions: false,
                }
            }
        }
    }

    /// Record a Router interpretation result. Public `interpret` does not expose probabilities,
    /// so the log carries status, operation, parameters and missing slots instead.
    pub fn record_interpretation(
        &mut self,
        question: &str,
        interpretation: &Interpretation,
        decision: Option<&str>,
        elapsed: Duration,
    ) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let digest = Sha256::digest(question.as_bytes());
        let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let parameters: Vec<Value> = interpretation
            .parameters
            .iter()
            .map(|parameter| {
                let value = match &parameter.value {
                    gleaph_router_wire::AskParameterValue::Int(value) => json!(value),
                    gleaph_router_wire::AskParameterValue::Text(value) => json!(value),
                };
                json!({"name": parameter.name, "value": value})
            })
            .collect();
        let line = json!({
            "timestamp_ms": timestamp_ms,
            "question_sha256": digest_hex,
            "question": if self.raw_questions {
                Some(redact(question))
            } else {
                None
            },
            "status": format!("{:?}", interpretation.status),
            "operation": interpretation.operation,
            "decision": decision,
            "parameters": parameters,
            "missing": interpretation.missing,
            "elapsed_ms": elapsed.as_millis() as u64,
        });
        if writeln!(file, "{line}").is_err() {
            self.file = None;
        }
    }
}

/// Replace URLs, emails and long digit runs with placeholders.
pub fn redact(text: &str) -> String {
    let mut words = Vec::new();
    for word in text.split_whitespace() {
        if word.starts_with("http://") || word.starts_with("https://") {
            words.push("<url>".to_owned());
        } else if word.contains('@') && word.contains('.') {
            words.push("<email>".to_owned());
        } else if word.chars().filter(char::is_ascii_digit).count() >= 6 {
            words.push("<number>".to_owned());
        } else {
            words.push(word.to_owned());
        }
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_replaces_urls_emails_and_long_numbers() {
        assert_eq!(
            redact("see https://example.com user@example.com 12345678"),
            "see <url> <email> <number>"
        );
    }
}
