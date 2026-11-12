//! Opt-in structured routing log for offline quality analysis.
//!
//! Disabled unless `--routing-log <path>` or `GLEAPH_ROUTING_LOG=<path>` is given. Each line is one
//! JSON object with the routing decision, probabilities and spans. Raw question text is **not**
//! written by default; set `GLEAPH_ROUTING_LOG_QUESTIONS=1` to include a PII-redacted copy
//! (URLs, emails and long digit runs are replaced). The question is always reduced to a SHA-256
//! digest for deduplication.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gleaph_sdk::routing::{Extraction, GeneralRoute, TextExtraction, TokenizedQuestion};
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

    /// Append one event; write errors disable the logger instead of failing the routing command.
    pub fn record(
        &mut self,
        question: &str,
        tokenized: &TokenizedQuestion,
        extraction: &Extraction,
        decision: Option<&str>,
        elapsed: Duration,
    ) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = entry(
            question,
            tokenized,
            extraction,
            decision,
            elapsed,
            self.raw_questions,
        );
        if writeln!(file, "{line}").is_err() {
            self.file = None;
        }
    }

    /// Record a Router general-route result (ADR 0114): operation, typed slots and fail-closed flags.
    pub fn record_general(
        &mut self,
        question: &str,
        route: &GeneralRoute,
        decision: Option<&str>,
        elapsed: Duration,
    ) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = entry_general(question, route, decision, elapsed, self.raw_questions);
        if writeln!(file, "{line}").is_err() {
            self.file = None;
        }
    }

    /// Record a Router text-routing result (ADR 0113); spans are UTF-8 byte offsets.
    pub fn record_text(
        &mut self,
        question: &str,
        extraction: &TextExtraction,
        decision: Option<&str>,
        elapsed: Duration,
    ) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let line = entry_text(question, extraction, decision, elapsed, self.raw_questions);
        if writeln!(file, "{line}").is_err() {
            self.file = None;
        }
    }
}

fn entry_general(
    question: &str,
    route: &GeneralRoute,
    decision: Option<&str>,
    elapsed: Duration,
    raw_questions: bool,
) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let digest = Sha256::digest(question.as_bytes());
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let slots = route
        .slots
        .iter()
        .map(|slot| {
            let value = match &slot.value {
                gleaph_sdk::routing::GeneralSlotLiteral::Int(value) => json!(value),
                gleaph_sdk::routing::GeneralSlotLiteral::Text(value) => json!(value),
            };
            json!({
                "name": slot.name,
                "value": value,
                "surface": slot.surface,
                "start": slot.start,
                "end": slot.end,
                "rule": slot.rule,
            })
        })
        .collect::<Vec<Value>>();
    let mut object = json!({
        "timestamp_ms": timestamp_ms,
        "question_sha256": digest_hex,
        "question_chars": question.chars().count(),
        "decision": decision,
        "abstained": decision.is_none(),
        "incomplete": route.incomplete,
        "probabilities": route.probabilities,
        "slots": slots,
        "model_version": route.model_version,
        "elapsed_us": elapsed.as_micros() as u64,
        "route": "general",
    });
    if raw_questions {
        object["question"] = Value::String(redact(question));
    }
    object.to_string()
}

fn entry_text(
    question: &str,
    extraction: &TextExtraction,
    decision: Option<&str>,
    elapsed: Duration,
    raw_questions: bool,
) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let digest = Sha256::digest(question.as_bytes());
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut object = json!({
        "timestamp_ms": timestamp_ms,
        "question_sha256": digest_hex,
        "question_chars": question.chars().count(),
        "token_count": extraction.token_count,
        "cjk_fraction": extraction.cjk_fraction,
        "truncated": false,
        "decision": decision,
        "abstained": decision.is_none(),
        "probabilities": extraction.probabilities,
        "spans": extraction.spans.iter().map(|span| json!({
            "tag": span.tag,
            "start": span.start,
            "end": span.end,
            "score": span.score,
        })).collect::<Vec<Value>>(),
        "elapsed_us": elapsed.as_micros() as u64,
        "route": "text",
    });
    if raw_questions {
        object["question"] = Value::String(redact(question));
    }
    object.to_string()
}

/// Replace URLs, emails and long digit runs with placeholders.
pub fn redact(text: &str) -> String {
    let mut words = Vec::new();
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let digits = word.chars().filter(char::is_ascii_digit).count();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            words.push("[url]".to_owned());
        } else if word.contains('@') && word.contains('.') {
            words.push("[email]".to_owned());
        } else if digits >= 7 {
            words.push("[number]".to_owned());
        } else {
            words.push(word.to_owned());
        }
    }
    words.join(" ")
}

fn entry(
    question: &str,
    tokenized: &TokenizedQuestion,
    extraction: &Extraction,
    decision: Option<&str>,
    elapsed: Duration,
    raw_questions: bool,
) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let digest = Sha256::digest(question.as_bytes());
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut object = json!({
        "timestamp_ms": timestamp_ms,
        "question_sha256": digest_hex,
        "question_chars": question.chars().count(),
        "token_count": tokenized.ids.len(),
        "truncated": tokenized.truncated,
        "decision": decision,
        "abstained": decision.is_none(),
        "probabilities": extraction.probabilities,
        "spans": extraction
            .spans
            .iter()
            .map(|span| json!({
                "tag": span.tag,
                "start": span.start,
                "end": span.end,
                "score": span.score,
            }))
            .collect::<Vec<Value>>(),
        "elapsed_us": elapsed.as_micros() as u64,
    });
    if raw_questions {
        object["question"] = Value::String(redact(question));
    }
    object.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_sdk::routing::{Extraction, SpannedTag, TokenizedQuestion};

    fn tokenized() -> TokenizedQuestion {
        TokenizedQuestion {
            ids: vec![2, 10, 11, 12],
            offsets: vec![(0, 1), (2, 3), (4, 5), (6, 7)],
            truncated: false,
        }
    }

    fn extraction() -> Extraction {
        Extraction {
            probabilities: vec![0.1, 0.2, 0.3, 0.4, 0.0, 0.0],
            spans: vec![SpannedTag {
                tag: "anchor".into(),
                start: 1,
                end: 2,
                score: 0.7,
            }],
            options: vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
                "none-of-these".into(),
            ],
        }
    }

    #[test]
    fn redacts_urls_emails_and_long_numbers() {
        let redacted =
            redact("mail me at a@example.com or see https://example.com/x then call 09012345678");
        assert!(!redacted.contains("a@example.com"));
        assert!(!redacted.contains("https://example.com/x"));
        assert!(!redacted.contains("09012345678"));
        assert!(
            redacted.contains("[email]")
                && redacted.contains("[url]")
                && redacted.contains("[number]")
        );
        // Short numbers such as depths stay readable.
        assert_eq!(redact("citations to depth 2"), "citations to depth 2");
    }

    #[test]
    fn default_entry_has_no_question_text() {
        let line = entry(
            "From Graph databases to depth 2",
            &tokenized(),
            &extraction(),
            Some("citation-reach"),
            Duration::from_millis(3),
            false,
        );
        assert!(!line.contains("Graph databases"));
        assert!(line.contains("\"question_sha256\":"));
        assert!(line.contains("\"abstained\":false"));
        assert!(line.contains("\"decision\":\"citation-reach\""));
        assert!(line.contains("\"score\":0.699999"));
    }

    #[test]
    fn writes_opted_in_event_to_the_file() {
        let path =
            std::env::temp_dir().join(format!("gleaph-routing-log-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut log = RoutingLog::from_args(Some(&path));
        log.record(
            "From Graph databases to depth 2",
            &tokenized(),
            &extraction(),
            Some("citation-reach"),
            Duration::from_millis(2),
        );
        let line = std::fs::read_to_string(&path).expect("log written");
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains(r#""decision":"citation-reach""#));
        assert!(line.contains(r#""token_count":4"#));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn raw_question_is_redacted() {
        let line = entry(
            "call 09012345678 or visit https://example.com",
            &tokenized(),
            &extraction(),
            None,
            Duration::from_millis(1),
            true,
        );
        assert!(line.contains("[number]") && line.contains("[url]"));
        assert!(!line.contains("09012345678") && !line.contains("https://example.com"));
        assert!(line.contains("\"abstained\":true"));
    }
}
