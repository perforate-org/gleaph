//! Natural-language routing over the local prepared-query catalog.
//!
//! This is the first Phase-0 slice of the natural-language query surface described in
//! `design/investigations/2026-11-09-gleaph-decision-model-research.md`: it turns a question into
//! ranked prepared-query candidates built from the local prepared directory, and it evaluates a
//! golden set offline.
//!
//! The router is deliberately model-free. It is a deterministic lexical baseline that establishes
//! the candidate/metadata surface, the abstain contract and the evaluation harness. A decision
//! model replaces [`route`] behind the same [`Candidate`] surface later; nothing here authorizes,
//! executes or mutates anything, so the trust boundary is unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use gleaph_prepared_api::OperationKind;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decision::{DecisionModelClient, JudgeMode, Judgement};
use crate::prepared::{self, PreparedDirArgs};
use crate::slots;

/// Default number of ranked candidates printed.
pub const DEFAULT_LIMIT: usize = 3;

/// Default abstain threshold on question coverage.
pub const DEFAULT_THRESHOLD: f64 = 0.34;

/// Name of the synthetic "none of these" option offered to a decision model.
pub const ABSTAIN_OPTION: &str = "none-of-these";

/// What a natural-language command does after interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskMode {
    /// Interpret and execute the selected read-only operation.
    Answer,
    /// Interpret only; never execute.
    Interpret,
}

/// Arguments for `gleaph ask` and `gleaph interpret`.
#[derive(Clone, Debug, Args)]
pub struct AskArgs {
    /// Prepared-query directory; defaults to `./prepared` (configurable via `[dirs]` in
    /// `gleaph.toml`).
    #[command(flatten)]
    pub dir: PreparedDirArgs,
    /// Question to route. Required unless `--eval` is given.
    #[arg(short = 'q', long, value_name = "TEXT")]
    pub question: Option<String>,
    /// Golden-set file (JSONL) to score instead of routing one question.
    #[arg(long, value_name = "PATH")]
    pub eval: Option<PathBuf>,
    /// Number of ranked candidates to print.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
    /// Abstain when the top candidate's question coverage is below this value.
    #[arg(long, value_name = "0..1", default_value_t = DEFAULT_THRESHOLD)]
    pub threshold: f64,
    /// Print machine-readable JSON instead of text.
    #[arg(long)]
    pub json: bool,
    /// Jev-compatible decision-model endpoint (for example `http://127.0.0.1:8080/v1/systemone`).
    /// When absent, the deterministic lexical baseline is used.
    #[arg(long, value_name = "URL")]
    pub model_url: Option<String>,
    /// Model identifier sent in the request body.
    #[arg(long, value_name = "MODEL", default_value = "jev-latest")]
    pub model: String,
    /// Question primitive: `auto` uses one choice question up to 255 candidates and yes/no
    /// questions beyond that.
    #[arg(long, value_name = "MODE", value_enum, default_value_t = JudgeMode::Auto)]
    pub model_mode: JudgeMode,
    /// Environment variable holding the endpoint bearer token, when the endpoint needs one.
    #[arg(long, value_name = "ENV", default_value = "TYPESAFE_API_KEY")]
    pub api_key_env: String,
    /// Decision-model request timeout in seconds.
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    pub model_timeout_secs: u64,
    /// Append one JSONL record per routed question (lexical and model results) for shadow
    /// evaluation and teacher-label collection.
    #[arg(long, value_name = "PATH")]
    pub shadow: Option<PathBuf>,
    /// JSON object mapping prepared-operation names to routing descriptions ("use when" lines)
    /// that override the source doc comments.
    #[arg(long, value_name = "PATH")]
    pub catalog: Option<PathBuf>,
    /// Add a synthetic "none of these" option with this description to the model's choice set; the
    /// model selecting it is treated as an abstain. Ignored by the lexical baseline.
    #[arg(long, value_name = "TEXT")]
    pub abstain_option: Option<String>,
    /// Entity vocabulary JSON used to bind slot values (`concepts`, `documents`, `teams`).
    #[arg(long, value_name = "PATH")]
    pub entities: Option<PathBuf>,
    /// Slot-labelled golden file (JSONL) to score slot extraction instead of routing questions.
    #[arg(long, value_name = "PATH")]
    pub slot_eval: Option<PathBuf>,
    /// Inference-enabled graph name for general routing; requires `--canister`. The Router builds
    /// the per-graph context and proxies the general route.
    #[arg(long, value_name = "NAME")]
    pub graph: Option<String>,
    /// Append opt-in structured routing events (JSONL) for offline quality analysis.
    #[arg(long, value_name = "PATH")]
    pub routing_log: Option<PathBuf>,
    /// Calibrate the per-graph abstain threshold from `--eval` labelled dev items using the Router's
    /// raw general route. Requires `--graph`, `--canister` and `--eval`; never writes the profile.
    #[arg(long)]
    pub calibrate: bool,
    /// Calibration target: minimum OOS abstention rate (0..1).
    #[arg(long, value_name = "0..1", default_value_t = 0.95)]
    pub target_oos: f64,
    /// Calibration target: minimum selective accuracy on covered in-domain items (0..1).
    #[arg(long, value_name = "0..1", default_value_t = 0.85)]
    pub target_selective: f64,
    /// Policy version written into the recommendation (applied separately by an administrator).
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub policy_version: u32,
    /// Write the calibration report JSON to this path.
    #[arg(long, value_name = "PATH")]
    pub calibration_out: Option<PathBuf>,
    /// Router canister principal for graph-backed `ask` / `interpret`.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Network name (`local`) or endpoint for graph-backed `ask` / `interpret`.
    #[arg(long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM identity file for graph-backed `ask` / `interpret`.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before executing on a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
}

/// Errors raised by `gleaph ask`.
#[derive(Debug, Error)]
pub enum AskError {
    /// Reading the golden set failed.
    #[error("io error: {0}")]
    Io(String),
    /// The golden set line was not valid JSON.
    #[error("invalid golden-set line {line}: {reason}")]
    GoldenSet {
        /// One-based line number.
        line: usize,
        /// Parser or validation reason.
        reason: String,
    },
    /// A golden-set item has neither an expected operation nor an explicit abstain marker.
    #[error("golden-set line {0} needs `expected_op` or `\"expect_abstain\": true`")]
    AmbiguousGoldenItem(usize),
    /// No question was supplied and no evaluation file was given.
    #[error("one of --question or --eval is required")]
    MissingQuestion,
    /// Local prepared-directory validation failed.
    #[error(transparent)]
    Prepared(#[from] prepared::PreparedError),
    /// The prepared directory does not exist.
    #[error("no prepared directory at {0}; run from a project directory or pass --dir")]
    MissingDirectory(String),
    /// Writing the shadow log failed.
    #[error("shadow log error: {0}")]
    Shadow(String),
    /// The routing-description catalog was invalid.
    #[error("invalid routing catalog: {0}")]
    Catalog(String),
    /// Slot extraction input was invalid.
    #[error("slot error: {0}")]
    Slots(String),
    /// Model-based routing (Router general route) failed.
    #[error("model routing: {0}")]
    Inference(String),
    /// The Router answer envelope could not be rendered.
    #[error("answer: {0}")]
    Answer(String),
}

/// One routable candidate derived from the prepared catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Prepared-operation name (the file stem).
    pub name: String,
    /// Human-readable description used as the routing text.
    pub description: String,
    /// Parameter names declared by the source, for display only.
    pub parameters: Vec<String>,
    /// Execution kind; updates are not routable by the natural-language surface.
    pub kind: OperationKind,
}

/// One ranked routing result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RouteHit {
    /// Prepared-operation name.
    pub name: String,
    /// Fraction of the question's weighted tokens matched by this candidate (0..1).
    pub score: f64,
    /// Candidate description, for display.
    pub description: String,
}

/// The routing outcome for one question.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Routing {
    /// Ranked candidates, best first, at most the requested limit.
    pub hits: Vec<RouteHit>,
    /// Whether the caller should ask for clarification instead of executing.
    pub abstained: bool,
    /// Threshold applied to the top candidate's score.
    pub threshold: f64,
}

impl Routing {
    /// The best candidate, if any.
    pub fn best(&self) -> Option<&RouteHit> {
        self.hits.first()
    }
}

/// Build routing candidates from the local prepared directory.
///
/// Update operations are included so callers can see them, but [`route`] never routes to them.
pub fn candidates(root: &Path) -> Result<Vec<Candidate>, AskError> {
    candidates_with_overrides(root, &BTreeMap::new())
}

/// Build routing candidates, overriding descriptions with the given catalog entries.
pub fn candidates_with_overrides(
    root: &Path,
    overrides: &BTreeMap<String, String>,
) -> Result<Vec<Candidate>, AskError> {
    let artifacts = prepared::discover(root)?;
    let mut candidates = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let description = match overrides.get(&artifact.name) {
            Some(description) => description.clone(),
            None => match artifact.sidecar.description.clone() {
                Some(description) => description,
                None => {
                    prepared::description_without_directives(&artifact.source).unwrap_or_default()
                }
            },
        };
        candidates.push(Candidate {
            name: artifact.name.clone(),
            description,
            parameters: declared_parameters(&artifact.source),
            kind: artifact.kind,
        });
    }
    Ok(candidates)
}

/// Collect `$name` parameter references outside quoted strings, in first-appearance order.
fn declared_parameters(source: &str) -> Vec<String> {
    let mut parameters = Vec::new();
    let mut chars = source.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(character) = chars.next() {
        match quote {
            Some(active) => {
                if character == '\\' {
                    chars.next();
                } else if character == active {
                    quote = None;
                }
            }
            None => {
                if character == '\'' || character == '"' {
                    quote = Some(character);
                    continue;
                }
                if character != '$' {
                    continue;
                }
                let mut name = String::new();
                while let Some(next) = chars.peek().copied() {
                    let allowed = next.is_ascii_alphanumeric() || next == '_';
                    if !allowed {
                        break;
                    }
                    name.push(next);
                    chars.next();
                }
                let mut first = name.chars().next();
                if first.is_some_and(|c| c.is_ascii_digit()) {
                    first = None;
                }
                if first.is_some() && !parameters.contains(&name) {
                    parameters.push(name);
                }
            }
        }
    }
    parameters
}

/// Split text into routing tokens: lowercased alphanumeric words plus CJK character bigrams.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk_run: Vec<char> = Vec::new();

    let flush_word = |word: &mut String, tokens: &mut Vec<String>| {
        if !word.is_empty() {
            let lower = normalize_word(&word.to_lowercase());
            if !STOPWORDS.contains(&lower.as_str()) && !tokens.contains(&lower) {
                tokens.push(lower);
            }
            word.clear();
        }
    };
    let flush_cjk = |run: &mut Vec<char>, tokens: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        if run.len() == 1 {
            let single = run[0].to_string();
            if !tokens.contains(&single) {
                tokens.push(single);
            }
        } else {
            for pair in run.windows(2) {
                let bigram: String = pair.iter().collect();
                if !tokens.contains(&bigram) {
                    tokens.push(bigram);
                }
            }
        }
        run.clear();
    };

    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut tokens);
            word.push(character);
        } else if is_cjk(character) {
            flush_word(&mut word, &mut tokens);
            cjk_run.push(character);
        } else {
            flush_word(&mut word, &mut tokens);
            flush_cjk(&mut cjk_run, &mut tokens);
        }
    }
    flush_word(&mut word, &mut tokens);
    flush_cjk(&mut cjk_run, &mut tokens);
    tokens
}

/// Light suffix normalisation so `citation`, `cites`, `cited` and `reachable`/`reach` meet.
///
/// This is intentionally not a stemmer: the baseline removes only the suffixes that most often
/// split one concept into several tokens, and it never shortens a word below three characters.
fn normalize_word(word: &str) -> String {
    if word.len() < 4 || !word.is_ascii() || STOPWORDS.contains(&word) {
        return word.to_owned();
    }
    if let Some(stem) = word.strip_suffix("ies")
        && stem.len() >= 3
    {
        return format!("{stem}y");
    }
    for suffix in SUFFIXES {
        if let Some(stem) = word.strip_suffix(suffix)
            && stem.len() >= 3
        {
            return stem.to_owned();
        }
    }
    word.to_owned()
}

/// Suffixes tried longest-first so `citation` and `cites` both reduce to `cit`.
const SUFFIXES: &[&str] = &[
    "ations", "ation", "ables", "able", "ings", "ing", "edly", "ed", "es", "s", "ly",
];

fn is_cjk(character: char) -> bool {
    matches!(
        character,
        '\u{3040}'..='\u{30ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
    )
}

/// Common English question words that carry no routing signal.
const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "answer", "are", "as", "at", "be", "by", "can", "could", "do",
    "does", "for", "from", "give", "how", "i", "in", "is", "it", "keep", "know", "list", "me",
    "of", "on", "or", "please", "result", "results", "return", "rows", "short", "show", "tell",
    "that", "the", "their", "there", "these", "this", "to", "want", "was", "were", "what", "when",
    "where", "which", "who", "why", "with",
];

/// Route a question against candidates with a deterministic lexical baseline.
///
/// The score is IDF-weighted question coverage: how much of the question's rare vocabulary the
/// candidate text contains. Candidates are ranked by score then name, so equal scores are stable.
pub fn route(question: &str, candidates: &[Candidate], limit: usize, threshold: f64) -> Routing {
    let limit = limit.max(1);
    let question_tokens = tokenize(question);
    let routable: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.kind == OperationKind::Query)
        .collect();

    let document_frequency = document_frequency(&question_tokens, &routable);
    // Tokens that appear in no candidate (entities, surface wrappers, out-of-vocabulary words)
    // cannot discriminate between candidates, so they must not dilute the coverage score.
    let informative: Vec<&String> = question_tokens
        .iter()
        .filter(|token| document_frequency.get(*token).copied().unwrap_or(0) > 0)
        .collect();
    let total_weight: f64 = informative
        .iter()
        .map(|token| {
            idf(
                *document_frequency.get(*token).unwrap_or(&0),
                routable.len(),
            )
        })
        .sum();

    let mut ranked: Vec<(f64, &Candidate)> = Vec::new();
    for candidate in &routable {
        let candidate_tokens: BTreeSet<String> =
            tokenize(&candidate_text(candidate)).into_iter().collect();
        let matched: f64 = informative
            .iter()
            .filter(|token| candidate_tokens.contains(**token))
            .map(|token| {
                idf(
                    *document_frequency.get(*token).unwrap_or(&0),
                    routable.len(),
                )
            })
            .sum();
        let score = if total_weight == 0.0 {
            0.0
        } else {
            matched / total_weight
        };
        ranked.push((score, candidate));
    }
    ranked.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.name.cmp(&right.1.name))
    });

    let hits: Vec<RouteHit> = ranked
        .into_iter()
        .take(limit)
        .map(|(score, candidate)| RouteHit {
            name: candidate.name.clone(),
            score,
            description: candidate.description.clone(),
        })
        .collect();
    let abstained = hits.first().is_none_or(|hit| hit.score < threshold);
    Routing {
        hits,
        abstained,
        threshold,
    }
}

fn candidate_text(candidate: &Candidate) -> String {
    let mut text = String::with_capacity(candidate.name.len() + candidate.description.len() + 32);
    text.push_str(&candidate.name.replace('-', " "));
    text.push(' ');
    text.push_str(&candidate.description);
    for parameter in &candidate.parameters {
        text.push(' ');
        text.push_str(parameter);
    }
    text
}

fn document_frequency(tokens: &[String], candidates: &[&Candidate]) -> BTreeMap<String, usize> {
    let mut frequency = BTreeMap::new();
    for token in tokens {
        let count = candidates
            .iter()
            .filter(|candidate| {
                tokenize(&candidate_text(candidate))
                    .iter()
                    .any(|candidate_token| candidate_token == token)
            })
            .count();
        frequency.insert(token.clone(), count);
    }
    frequency
}

fn idf(document_frequency: usize, candidate_count: usize) -> f64 {
    (1.0 + candidate_count as f64 / (1.0 + document_frequency as f64)).ln()
}

/// One golden-set item.
#[derive(Clone, Debug, Deserialize)]
pub struct GoldenItem {
    /// The natural-language question.
    pub question: String,
    /// Expected prepared-operation name; `null` means the router must abstain.
    #[serde(default)]
    pub expected_op: Option<String>,
    /// Explicit abstain expectation for items with no expected operation.
    #[serde(default)]
    pub expect_abstain: bool,
    /// The request is supported but lacks a required slot; the routing operation is still
    /// expected, while execution must clarify/abstain instead of running.
    #[serde(default)]
    pub expect_incomplete: bool,
    /// Optional language slice (`en`, `ja`, ...).
    #[serde(default)]
    pub lang: Option<String>,
    /// Optional template-family slice emitted by the generator.
    #[serde(default)]
    pub family: Option<String>,
}

/// Per-slice evaluation counters.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SliceReport {
    /// Items in this slice.
    pub items: usize,
    /// Items judged correct.
    pub correct: usize,
}

/// Golden-set evaluation report.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvalReport {
    /// Total items.
    pub items: usize,
    /// Items with an expected operation.
    pub routed_items: usize,
    /// Items with an expected operation that landed on the expected candidate.
    pub routed_correct: usize,
    /// Items that must abstain.
    pub abstain_items: usize,
    /// Abstain items where the router abstained.
    pub abstain_correct: usize,
    /// Per-operation accuracy.
    pub per_op: BTreeMap<String, SliceReport>,
    /// Per-language accuracy.
    pub per_lang: BTreeMap<String, SliceReport>,
    /// Supported complete-intent items whose routed decision was incomplete.
    pub incomplete_items: usize,
    /// Expected-incomplete-intent items.
    pub incomplete_expected_items: usize,
    /// Expected-incomplete-intent items whose operation was selected.
    pub incomplete_correct: usize,
    /// Misclassified items, for triage.
    pub errors: Vec<EvalError>,
}

/// One misclassified golden-set item.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvalError {
    /// Question text.
    pub question: String,
    /// Expected operation, if any.
    pub expected: Option<String>,
    /// Top candidate name, if any.
    pub got: Option<String>,
    /// Whether the router abstained.
    pub abstained: bool,
}

impl EvalReport {
    /// Fraction of routed items that chose the expected operation.
    pub fn top1_accuracy(&self) -> f64 {
        ratio(self.routed_correct, self.routed_items)
    }

    /// Fraction of expected-abstain items where the router abstained.
    pub fn abstain_recall(&self) -> f64 {
        ratio(self.abstain_correct, self.abstain_items)
    }

    /// Fraction of all items judged correct.
    pub fn overall_accuracy(&self) -> f64 {
        ratio(
            self.routed_correct + self.incomplete_correct + self.abstain_correct,
            self.items,
        )
    }
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// Parse a golden-set JSONL document.
pub fn parse_golden_set(input: &str) -> Result<Vec<GoldenItem>, AskError> {
    let mut items = Vec::new();
    for (index, line) in input.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let item: GoldenItem =
            serde_json::from_str(trimmed).map_err(|error| AskError::GoldenSet {
                line: index + 1,
                reason: error.to_string(),
            })?;
        if item.expected_op.is_none() && !item.expect_abstain {
            return Err(AskError::AmbiguousGoldenItem(index + 1));
        }
        items.push(item);
    }
    Ok(items)
}

impl EvalReport {
    fn empty(items: usize) -> Self {
        Self {
            items,
            routed_items: 0,
            routed_correct: 0,
            abstain_items: 0,
            abstain_correct: 0,
            per_op: BTreeMap::new(),
            per_lang: BTreeMap::new(),
            incomplete_items: 0,
            incomplete_expected_items: 0,
            incomplete_correct: 0,
            errors: Vec::new(),
        }
    }
}

/// Whether one routing decision satisfies the item's label.
fn outcome_matches(item: &GoldenItem, routing: &Routing) -> bool {
    match &item.expected_op {
        Some(expected) => {
            !routing.abstained && routing.best().is_some_and(|hit| hit.name == *expected)
        }
        None => routing.abstained,
    }
}

/// Fold one routing decision into the aggregate report and its slices.
fn record_outcome(report: &mut EvalReport, item: &GoldenItem, routing: &Routing) {
    let got = routing.best().map(|hit| hit.name.clone());
    let lang = item.lang.clone().unwrap_or_else(|| "unknown".into());
    let slice = report.per_lang.entry(lang).or_default();
    slice.items += 1;
    match &item.expected_op {
        Some(expected) => {
            report.routed_items += 1;
            let op = report.per_op.entry(expected.clone()).or_default();
            op.items += 1;
            if outcome_matches(item, routing) {
                report.routed_correct += 1;
                op.correct += 1;
                slice.correct += 1;
            } else {
                report.errors.push(EvalError {
                    question: item.question.clone(),
                    expected: Some(expected.clone()),
                    got,
                    abstained: routing.abstained,
                });
            }
        }
        None => {
            report.abstain_items += 1;
            if routing.abstained {
                report.abstain_correct += 1;
                slice.correct += 1;
            } else {
                report.errors.push(EvalError {
                    question: item.question.clone(),
                    expected: None,
                    got,
                    abstained: routing.abstained,
                });
            }
        }
    }
}

/// Route every golden-set item with the lexical baseline and aggregate the result.
pub fn evaluate(
    items: &[GoldenItem],
    candidates: &[Candidate],
    limit: usize,
    threshold: f64,
) -> EvalReport {
    let mut report = EvalReport::empty(items.len());
    for item in items {
        let routing = route(&item.question, candidates, limit, threshold);
        record_outcome(&mut report, item, &routing);
    }
    report
}

/// How often the two routers choose the same top candidate.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FlipCount {
    /// Items where both routers chose the same top candidate (including both abstaining).
    pub agree: usize,
    /// Items the lexical baseline got right and the model did not.
    pub lexical_only: usize,
    /// Items the model got right and the lexical baseline did not.
    pub model_only: usize,
}

/// One model's side of a comparison.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelEval {
    /// Model identifier sent to the endpoint.
    pub model: String,
    /// Aggregated report for the model decisions.
    pub report: EvalReport,
    /// Requests that failed; those items count as abstentions.
    pub errors: usize,
    /// First few failure messages, for triage.
    pub error_examples: Vec<String>,
}

/// Lexical baseline versus decision-model routing on the same golden set.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ComparisonReport {
    /// Number of items evaluated.
    pub items: usize,
    /// Lexical baseline.
    pub lexical: EvalReport,
    /// Decision model.
    pub model: ModelEval,
    /// Top-candidate agreement between the two.
    pub flips: FlipCount,
}

fn abstain_routing(threshold: f64) -> Routing {
    Routing {
        hits: Vec::new(),
        abstained: true,
        threshold,
    }
}

/// Options for one lexical-versus-model comparison run.
#[derive(Clone, Copy)]
pub struct ComparisonOptions<'a> {
    /// Model identifier sent to the endpoint.
    pub model_name: &'a str,
    /// Question primitive used for the model calls.
    pub mode: JudgeMode,
    /// Number of candidates kept per decision.
    pub limit: usize,
    /// Abstain threshold.
    pub threshold: f64,
    /// Optional per-item shadow log.
    pub shadow: Option<&'a Path>,
    /// Optional synthetic "none of these" option description; model selection means abstain.
    pub abstain_option: Option<&'a str>,
}

/// Evaluate the lexical baseline and a decision model on the same items.
///
/// Model failures are counted and treated as abstentions, so a flaky endpoint degrades to the
/// baseline's abstain behaviour instead of aborting a long evaluation. When `shadow` is given,
/// one JSONL record per item captures both decisions for later calibration.
pub fn evaluate_comparison(
    items: &[GoldenItem],
    candidates: &[Candidate],
    client: &DecisionModelClient,
    options: ComparisonOptions<'_>,
) -> Result<ComparisonReport, AskError> {
    let ComparisonOptions {
        model_name,
        mode,
        limit,
        threshold,
        shadow,
        abstain_option,
    } = options;
    let mut described: Vec<(String, String)> = candidates
        .iter()
        .filter(|candidate| candidate.kind == OperationKind::Query)
        .map(|candidate| (candidate.name.clone(), candidate.description.clone()))
        .collect();
    if let Some(description) = abstain_option {
        described.push((ABSTAIN_OPTION.to_owned(), description.to_owned()));
    }
    let abstain_name = abstain_option.map(|_| ABSTAIN_OPTION);
    let mut lexical = EvalReport::empty(items.len());
    let mut model = EvalReport::empty(items.len());
    let mut flips = FlipCount::default();
    let mut errors = 0;
    let mut error_examples = Vec::new();
    for item in items {
        let lexical_routing = route(&item.question, candidates, limit, threshold);
        let (model_routing, error) = match client.judge(&item.question, &described, mode) {
            Ok(judgements) => (
                routing_from_judgements(judgements, candidates, limit, threshold, abstain_name),
                None,
            ),
            Err(error) => {
                errors += 1;
                if error_examples.len() < 5 {
                    error_examples.push(error.to_string());
                }
                (abstain_routing(threshold), Some(error.to_string()))
            }
        };
        let lexical_ok = outcome_matches(item, &lexical_routing);
        let model_ok = outcome_matches(item, &model_routing);
        let lexical_top = lexical_routing.best().map(|hit| hit.name.clone());
        let model_top = model_routing.best().map(|hit| hit.name.clone());
        if lexical_top == model_top {
            flips.agree += 1;
        } else if lexical_ok && !model_ok {
            flips.lexical_only += 1;
        } else if model_ok && !lexical_ok {
            flips.model_only += 1;
        }
        record_outcome(&mut lexical, item, &lexical_routing);
        record_outcome(&mut model, item, &model_routing);
        if let Some(path) = shadow {
            append_shadow_eval(
                path,
                item,
                &lexical_routing,
                &model_routing,
                error.as_deref(),
            )?;
        }
    }
    Ok(ComparisonReport {
        items: items.len(),
        lexical,
        model: ModelEval {
            model: model_name.to_owned(),
            report: model,
            errors,
            error_examples,
        },
        flips,
    })
}

/// Append one comparison record for a golden-set item.
fn append_shadow_eval(
    path: &Path,
    item: &GoldenItem,
    lexical: &Routing,
    model: &Routing,
    error: Option<&str>,
) -> Result<(), AskError> {
    let record = serde_json::json!({
        "ts_unix": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default(),
        "question": item.question,
        "expected_op": item.expected_op,
        "expect_abstain": item.expect_abstain,
        "lang": item.lang,
        "family": item.family,
        "lexical": { "hits": lexical.hits, "abstained": lexical.abstained },
        "model": { "hits": model.hits, "abstained": model.abstained, "error": error },
    });
    append_line(path, &record)
}

fn append_line(path: &Path, record: &serde_json::Value) -> Result<(), AskError> {
    let mut line =
        serde_json::to_string(record).map_err(|error| AskError::Shadow(error.to_string()))?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| AskError::Shadow(error.to_string()))?;
    file.write_all(line.as_bytes())
        .map_err(|error| AskError::Shadow(error.to_string()))
}

/// Run `gleaph ask`.
/// One prepared-query execution requested by the lexical `gleaph ask` fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionRequest {
    /// Registered prepared-operation name, including the depth/hop variant when applicable.
    pub name: String,
    /// Typed parameter bindings in the prepared-query wire order.
    pub params: Vec<(String, gleaph_gql_params::GqlValue)>,
}

/// Map an operation plus extracted bindings onto a registered prepared-query variant and its
/// parameters. Repetition bounds select a variant because GQL quantifiers are literals.
pub fn execution_plan(
    operation: &str,
    bindings: &slots::SlotBindings,
    vocab: Option<&slots::EntityVocab>,
) -> Result<(String, Vec<(String, gleaph_gql_params::GqlValue)>), AskError> {
    let anchor = || {
        bindings
            .anchor
            .clone()
            .ok_or_else(|| AskError::Slots("missing anchor for execution".into()))
    };
    match operation {
        "films-by-conditions" => {
            let from = bindings
                .from
                .as_deref()
                .ok_or_else(|| AskError::Slots("missing from for execution".into()))?
                .parse::<i64>()
                .map_err(|_| AskError::Slots("from is not an integer".into()))?;
            let to = bindings
                .to
                .as_deref()
                .ok_or_else(|| AskError::Slots("missing to for execution".into()))?
                .parse::<i64>()
                .map_err(|_| AskError::Slots("to is not an integer".into()))?;
            let genre = bindings
                .genre
                .clone()
                .ok_or_else(|| AskError::Slots("missing genre for execution".into()))?;
            Ok((
                "films-by-conditions".to_owned(),
                vec![
                    ("from".to_owned(), gleaph_gql_params::GqlValue::Int64(from)),
                    ("to".to_owned(), gleaph_gql_params::GqlValue::Int64(to)),
                    ("genre".to_owned(), gleaph_gql_params::GqlValue::Text(genre)),
                ],
            ))
        }
        "synopsis-search" => Ok((
            "synopsis-search".to_owned(),
            vec![(
                "query".to_owned(),
                gleaph_gql_params::GqlValue::Text(
                    bindings
                        .topic
                        .clone()
                        .ok_or_else(|| AskError::Slots("missing query for execution".into()))?,
                ),
            )],
        )),
        "common-cast" => Ok((
            "common-cast".to_owned(),
            vec![
                (
                    "a".to_owned(),
                    gleaph_gql_params::GqlValue::Text(
                        bindings
                            .a
                            .clone()
                            .ok_or_else(|| AskError::Slots("missing a for execution".into()))?,
                    ),
                ),
                (
                    "b".to_owned(),
                    gleaph_gql_params::GqlValue::Text(
                        bindings
                            .b
                            .clone()
                            .ok_or_else(|| AskError::Slots("missing b for execution".into()))?,
                    ),
                ),
            ],
        )),
        "sequel-chain" => Ok((
            "sequel-chain".to_owned(),
            vec![(
                "from".to_owned(),
                gleaph_gql_params::GqlValue::Text(
                    bindings
                        .anchor
                        .clone()
                        .or_else(|| bindings.from.clone())
                        .ok_or_else(|| {
                            AskError::Slots("missing start film for execution".into())
                        })?,
                ),
            )],
        )),
        "citation-reach" => {
            let depth = bindings.depth.unwrap_or(3).clamp(1, 3);
            Ok((
                format!("citation-reach-depth-{depth}"),
                vec![(
                    "anchor".to_owned(),
                    gleaph_gql_params::GqlValue::Text(anchor()?),
                )],
            ))
        }
        "variable-length-reach" => {
            let hops = bindings.hops.unwrap_or(2).clamp(2, 3);
            Ok((
                format!("variable-length-reach-hops-{hops}"),
                vec![(
                    "anchor".to_owned(),
                    gleaph_gql_params::GqlValue::Text(anchor()?),
                )],
            ))
        }
        "document-text-search" => {
            let topic = bindings
                .topic
                .clone()
                .ok_or_else(|| AskError::Slots("missing topic for text search".into()))?;
            Ok((
                "document-text-search".to_owned(),
                vec![("query".to_owned(), gleaph_gql_params::GqlValue::Text(topic))],
            ))
        }
        "shortest-path" => {
            let from = bindings
                .from
                .clone()
                .ok_or_else(|| AskError::Slots("missing from for execution".into()))?;
            let to = bindings
                .to
                .clone()
                .ok_or_else(|| AskError::Slots("missing to for execution".into()))?;
            Ok((
                "shortest-path-param".to_owned(),
                vec![
                    ("from".to_owned(), gleaph_gql_params::GqlValue::Text(from)),
                    ("to".to_owned(), gleaph_gql_params::GqlValue::Text(to)),
                ],
            ))
        }
        "team-readable-documents" => {
            let team = bindings
                .team
                .clone()
                .ok_or_else(|| AskError::Slots("missing team for execution".into()))?;
            let topic = bindings
                .topic
                .clone()
                .ok_or_else(|| AskError::Slots("missing topic for execution".into()))?;
            let source_id = vocab
                .and_then(|vocab| vocab.document_source_id(&topic))
                .ok_or_else(|| {
                    AskError::Slots(format!(
                        "team search needs a Document topic with a seeded embedding; \
                         {topic:?} is not one"
                    ))
                })?;
            Ok((
                "team-readable-documents-param".to_owned(),
                vec![
                    ("team".to_owned(), gleaph_gql_params::GqlValue::Text(team)),
                    (
                        "query".to_owned(),
                        gleaph_gql_params::GqlValue::Bytes(crate::embedding::query_vector_bytes(
                            source_id,
                        )),
                    ),
                ],
            ))
        }
        other => Err(AskError::Slots(format!(
            "operation {other} has no executable parameter mapping"
        ))),
    }
}

/// Build the Router text-routing client when `--graph` is given.
///
/// The Router owns the per-graph context and the inference canister (ADR 0114), so the CLI only
/// needs the Router principal and graph name. No local tokenizer or neural pack is loaded.
fn model_routing_client(
    args: &AskArgs,
) -> Result<Option<gleaph_sdk::routing::AskClient>, AskError> {
    let Some(graph) = &args.graph else {
        return Ok(None);
    };
    let canister = args.canister.as_deref().ok_or_else(|| {
        AskError::Inference("--graph requires --canister <ROUTER_PRINCIPAL>".into())
    })?;
    let network = args.network.as_deref().unwrap_or("local");
    Ok(Some(connect_routing_client(
        canister,
        graph,
        network,
        args.identity.as_deref(),
        args.fetch_root_key,
    )?))
}

/// Build a Router routing client over a live agent. Shared by `ask` and `route`.
pub fn connect_routing_client(
    canister: &str,
    graph: &str,
    network: &str,
    identity: Option<&Path>,
    fetch_root_key: bool,
) -> Result<gleaph_sdk::routing::AskClient, AskError> {
    let remote =
        crate::remote::RemoteTransport::connect(canister, network, identity, fetch_root_key, None)
            .map_err(AskError::Inference)?;
    let router = candid::Principal::from_text(canister)
        .map_err(|error| AskError::Inference(format!("invalid Router canister: {error}")))?;
    let transport = gleaph_sdk::routing::RouterTransport::new(remote.agent(), router, graph);
    Ok(gleaph_sdk::routing::AskClient::new_text_only(Box::new(
        transport,
    )))
}

/// Map the Router general route's typed slots onto the CLI's prepared-execution binding surface.
/// Unknown names are ignored; missing required values are caught by `execution_plan` (fail-closed).
#[cfg(test)]
fn general_bindings(route: &gleaph_sdk::routing::DebugInterpretation) -> slots::SlotBindings {
    let mut bindings = slots::SlotBindings::default();
    for slot in &route.slots {
        match slot.name.as_str() {
            "anchor" | "start" => {
                if let gleaph_router_wire::AskParameterValue::Text(value) = &slot.value {
                    bindings.anchor = Some(value.clone());
                }
            }
            "from" => assign_text_or_int(slot, &mut bindings.from),
            "to" => assign_text_or_int(slot, &mut bindings.to),
            "topic" | "query" | "text" => {
                if let gleaph_router_wire::AskParameterValue::Text(value) = &slot.value {
                    bindings.topic = Some(value.clone());
                }
            }
            "team" => {
                if let gleaph_router_wire::AskParameterValue::Text(value) = &slot.value {
                    bindings.team = Some(value.clone());
                }
            }
            "genre" => {
                if let gleaph_router_wire::AskParameterValue::Text(value) = &slot.value {
                    bindings.genre = Some(value.clone());
                }
            }
            "a" | "b" => {
                if let gleaph_router_wire::AskParameterValue::Text(value) = &slot.value {
                    if slot.name == "a" {
                        bindings.a = Some(value.clone());
                    } else {
                        bindings.b = Some(value.clone());
                    }
                }
            }
            "depth" => {
                if let gleaph_router_wire::AskParameterValue::Int(value) = slot.value {
                    bindings.depth = u32::try_from(value).ok();
                }
            }
            "hops" => {
                if let gleaph_router_wire::AskParameterValue::Int(value) = slot.value {
                    bindings.hops = u32::try_from(value).ok();
                }
            }
            _ => {}
        }
    }
    bindings
}

/// Format interpreted parameters for `gleaph interpret`.
fn interpretation_display(interpretation: &gleaph_sdk::routing::Interpretation) -> String {
    interpretation
        .parameters
        .iter()
        .map(|parameter| {
            let value = match &parameter.value {
                gleaph_router_wire::AskParameterValue::Int(value) => value.to_string(),
                gleaph_router_wire::AskParameterValue::Text(value) => value.clone(),
            };
            format!("{}={value}", parameter.name)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
fn assign_text_or_int(slot: &gleaph_sdk::routing::DebugSlot, target: &mut Option<String>) {
    *target = match &slot.value {
        gleaph_router_wire::AskParameterValue::Text(value) => Some(value.clone()),
        gleaph_router_wire::AskParameterValue::Int(value) => Some(value.to_string()),
    };
}

/// One labelled calibration row with the raw route's un-thresholded output.
struct CalibrationRow {
    expected_op: Option<String>,
    oos: bool,
    max_probability: f64,
    operation: Option<String>,
    rule_abstain: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct CalibrationReport {
    /// Graph whose dev set was calibrated.
    pub graph: String,
    /// Labelled dev items.
    pub items: usize,
    /// In-domain dev items.
    pub in_domain: usize,
    /// OOS dev items.
    pub oos: usize,
    /// Recommended abstain threshold in basis points.
    pub recommended_threshold_bps: u32,
    /// Policy version to write with the recommendation.
    pub policy_version: u32,
    /// Coverage of in-domain items at the recommended threshold.
    pub coverage: f64,
    /// Selective accuracy on covered in-domain items.
    pub selective_accuracy: f64,
    /// OOS abstention rate at the recommended threshold.
    pub oos_abstain: f64,
    /// Whether the recommendation meets both requested targets.
    pub meets_targets: bool,
    /// Target OOS abstention rate.
    pub target_oos: f64,
    /// Target selective accuracy.
    pub target_selective: f64,
}

/// Sweep thresholds over raw rows and pick the best target-satisfying candidate. Exposed for tests
/// so calibration policy is independent of the Router transport.
fn recommend_threshold(
    rows: &[CalibrationRow],
    target_oos: f64,
    target_selective: f64,
) -> (u32, f64, f64, f64, bool) {
    let in_domain = rows.iter().filter(|row| !row.oos).count();
    let oos = rows.len() - in_domain;
    let mut thresholds: Vec<u32> = rows
        .iter()
        .map(|row| {
            (row.max_probability * 10_000.0)
                .round()
                .clamp(0.0, 10_000.0) as u32
        })
        .collect();
    thresholds.extend([0, 10_000]);
    thresholds.sort_unstable();
    thresholds.dedup();
    let mut best: Option<(u32, f64, f64, f64, bool)> = None;
    for bps in thresholds {
        let threshold = bps as f64 / 10_000.0;
        let mut covered = 0usize;
        let mut correct = 0usize;
        let mut abstained = 0usize;
        for row in rows {
            let abstain = row.rule_abstain || row.max_probability <= threshold;
            if row.oos {
                abstained += abstain as usize;
            } else if !abstain {
                covered += 1;
                correct += (row.operation.as_deref() == row.expected_op.as_deref()) as usize;
            }
        }
        let coverage = covered as f64 / in_domain.max(1) as f64;
        let selective = if covered == 0 {
            0.0
        } else {
            correct as f64 / covered as f64
        };
        let oos_rate = abstained as f64 / oos.max(1) as f64;
        let meets = oos_rate >= target_oos && selective >= target_selective;
        let candidate = (bps, coverage, selective, oos_rate, meets);
        let better = match &best {
            None => true,
            Some((best_bps, best_coverage, best_selective, best_oos, best_meets)) => {
                (meets && !*best_meets)
                    || (meets == *best_meets
                        && (coverage > *best_coverage
                            || (coverage == *best_coverage
                                && (selective > *best_selective
                                    || (selective == *best_selective
                                        && (oos_rate > *best_oos
                                            || (oos_rate == *best_oos && bps < *best_bps)))))))
            }
        };
        if better {
            best = Some(candidate);
        }
    }
    best.expect("at least one threshold")
}

/// Calibrate from labelled dev items using the Router's raw (threshold-zero) general route.
fn calibrate(
    args: &AskArgs,
    client: &gleaph_sdk::routing::AskClient,
) -> Result<CalibrationReport, AskError> {
    if !(0.0..=1.0).contains(&args.target_oos) || !(0.0..=1.0).contains(&args.target_selective) {
        return Err(AskError::Inference(
            "calibration targets must be in 0..=1".into(),
        ));
    }
    let path = args
        .eval
        .as_ref()
        .ok_or_else(|| AskError::Inference("--calibrate requires --eval <dev.jsonl>".into()))?;
    let graph = args
        .graph
        .clone()
        .ok_or_else(|| AskError::Inference("--calibrate requires --graph <NAME>".into()))?;
    let text = fs::read_to_string(path).map_err(|error| AskError::Io(error.to_string()))?;
    let items = parse_golden_set(&text)?;
    if items.is_empty() {
        return Err(AskError::Inference("calibration set is empty".into()));
    }
    let mut rows = Vec::with_capacity(items.len());
    for chunk in items.chunks(gleaph_sdk::routing::MAX_GENERAL_BATCH_QUESTIONS) {
        let questions: Vec<&str> = chunk.iter().map(|item| item.question.as_str()).collect();
        let routes = client
            .debug_interpret_batch(&questions)
            .map_err(|error| AskError::Inference(error.to_string()))?;
        for (item, route) in chunk.iter().zip(routes) {
            let max_probability = route.probabilities.iter().copied().fold(0.0f32, f32::max) as f64;
            let oos = item.expected_op.is_none() || item.expect_abstain;
            rows.push(CalibrationRow {
                expected_op: item.expected_op.clone(),
                oos,
                max_probability,
                operation: route.operation.clone(),
                // With the threshold forced to zero, an absent operation is the strong OOS rule
                // (or an empty catalog); incomplete slots keep the operation and are not a rule
                // abstention.
                rule_abstain: route.operation.is_none(),
            });
        }
    }
    let in_domain = rows.iter().filter(|row| !row.oos).count();
    let oos = rows.len() - in_domain;
    if oos == 0 {
        return Err(AskError::Inference(
            "calibration set needs at least one OOS item".into(),
        ));
    }
    let (recommended_threshold_bps, coverage, selective_accuracy, oos_abstain, meets_targets) =
        recommend_threshold(&rows, args.target_oos, args.target_selective);
    Ok(CalibrationReport {
        graph,
        items: rows.len(),
        in_domain,
        oos,
        recommended_threshold_bps,
        policy_version: args.policy_version,
        coverage,
        selective_accuracy,
        oos_abstain,
        meets_targets,
        target_oos: args.target_oos,
        target_selective: args.target_selective,
    })
}

fn print_calibration(report: &CalibrationReport, json: bool) -> Result<(), AskError> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report)
                .map_err(|error| AskError::Inference(error.to_string()))?
        );
    } else {
        println!(
            "calibration: {} ({} items, {} in-domain, {} OOS)",
            report.graph, report.items, report.in_domain, report.oos
        );
        println!(
            "recommended_threshold_bps: {}",
            report.recommended_threshold_bps
        );
        println!("policy_version: {}", report.policy_version);
        println!("coverage: {:.3}", report.coverage);
        println!("selective_accuracy: {:.3}", report.selective_accuracy);
        println!("oos_abstain: {:.3}", report.oos_abstain);
        println!(
            "meets_targets: {} (oos>={:.2}, selective>={:.2})",
            report.meets_targets, report.target_oos, report.target_selective
        );
        println!(
            "apply with: set_ask_config <graph> {} {}",
            report.recommended_threshold_bps, report.policy_version
        );
    }
    Ok(())
}

/// Lower-case status label for JSON output.
fn ask_status_label(status: gleaph_sdk::routing::AskStatus) -> &'static str {
    match status {
        gleaph_sdk::routing::AskStatus::Answered => "answered",
        gleaph_sdk::routing::AskStatus::NeedsInput => "needs_input",
        gleaph_sdk::routing::AskStatus::Unsupported => "unsupported",
        gleaph_sdk::routing::AskStatus::Failed => "failed",
    }
}

/// Lower-case interpretation status label for JSON output.
fn interpretation_status_label(status: gleaph_sdk::routing::InterpretationStatus) -> &'static str {
    match status {
        gleaph_sdk::routing::InterpretationStatus::Answered => "answered",
        gleaph_sdk::routing::InterpretationStatus::NeedsInput => "needs_input",
        gleaph_sdk::routing::InterpretationStatus::Unsupported => "unsupported",
    }
}

/// Serialize parameters shared by `interpret` and `ask`.
fn parameters_json(parameters: &[gleaph_sdk::routing::AskParameter]) -> Vec<serde_json::Value> {
    parameters
        .iter()
        .map(|parameter| {
            let value = match &parameter.value {
                gleaph_router_wire::AskParameterValue::Int(value) => serde_json::json!(value),
                gleaph_router_wire::AskParameterValue::Text(value) => serde_json::json!(value),
            };
            serde_json::json!({"name": parameter.name, "value": value})
        })
        .collect()
}

fn interpretation_json(
    interpretation: &gleaph_sdk::routing::Interpretation,
) -> Result<serde_json::Value, AskError> {
    Ok(serde_json::json!({
        "status": interpretation_status_label(interpretation.status),
        "operation": interpretation.operation,
        "parameters": parameters_json(&interpretation.parameters),
        "missing": interpretation.missing,
    }))
}

fn answer_json(answer: &gleaph_sdk::routing::Answer) -> Result<serde_json::Value, AskError> {
    let mut object = serde_json::json!({
        "status": ask_status_label(answer.status),
        "operation": answer.interpretation.operation,
        "parameters": parameters_json(&answer.interpretation.parameters),
        "missing": answer.interpretation.missing,
    });
    if let Some(rows) = &answer.rows {
        let mut rows_object = serde_json::json!({"row_count": rows.row_count});
        if let Some(blob) = &rows.rows_blob {
            let wire = prepared::decode_rows_blob(blob)
                .map_err(|error| AskError::Answer(error.to_string()))?;
            let values = prepared::wire_rows_json(&wire)
                .map_err(|error| AskError::Answer(error.to_string()))?;
            rows_object["rows"] = serde_json::Value::Array(values);
        }
        object["rows"] = rows_object;
    }
    if let Some(failure) = &answer.failure {
        let kind = match failure.kind {
            gleaph_sdk::routing::AskFailureKind::Authorization => "authorization",
            gleaph_sdk::routing::AskFailureKind::Catalog => "catalog",
            gleaph_sdk::routing::AskFailureKind::Execution => "execution",
            gleaph_sdk::routing::AskFailureKind::ResponseTooLarge => "response_too_large",
        };
        object["failure"] = serde_json::json!({
            "kind": kind,
            "message": failure.message,
        });
    }
    Ok(object)
}

fn print_answer(
    question: &str,
    answer: &gleaph_sdk::routing::Answer,
    json: bool,
) -> Result<(), AskError> {
    if json {
        let value = answer_json(answer)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|error| AskError::Answer(error.to_string()))?
        );
        return Ok(());
    }
    match answer.status {
        gleaph_sdk::routing::AskStatus::Answered => {
            let operation = answer.interpretation.operation.as_deref().unwrap_or("?");
            let row_count = answer.rows.as_ref().map_or(0, |rows| rows.row_count);
            println!("answer: {operation} ({row_count} rows)");
            if let Some(blob) = answer
                .rows
                .as_ref()
                .and_then(|rows| rows.rows_blob.as_deref())
            {
                let wire = prepared::decode_rows_blob(blob)
                    .map_err(|error| AskError::Answer(error.to_string()))?;
                let table = prepared::render_wire_rows_table(&wire)
                    .map_err(|error| AskError::Answer(error.to_string()))?;
                if !table.is_empty() {
                    println!("{table}");
                }
            }
        }
        gleaph_sdk::routing::AskStatus::NeedsInput => {
            println!("answer: needs input");
            if let Some(operation) = &answer.interpretation.operation {
                println!("  operation: {operation}");
            }
            if !answer.interpretation.missing.is_empty() {
                println!("  missing: {}", answer.interpretation.missing.join(", "));
            }
            println!("  hint: gleaph interpret -q \"{question}\" ...");
        }
        gleaph_sdk::routing::AskStatus::Unsupported => println!("answer: unsupported"),
        gleaph_sdk::routing::AskStatus::Failed => {
            println!("answer: failed");
            if let Some(failure) = &answer.failure {
                println!(
                    "  kind: {}",
                    match failure.kind {
                        gleaph_sdk::routing::AskFailureKind::Authorization => "authorization",
                        gleaph_sdk::routing::AskFailureKind::Catalog => "catalog",
                        gleaph_sdk::routing::AskFailureKind::Execution => "execution",
                        gleaph_sdk::routing::AskFailureKind::ResponseTooLarge => {
                            "response_too_large"
                        }
                        gleaph_sdk::routing::AskFailureKind::ResponseTooLarge =>
                            "response_too_large",
                    }
                );
                println!("  message: {}", failure.message);
            }
        }
    }
    Ok(())
}

pub fn execute(
    args: &AskArgs,
    dir: &Path,
    mode: AskMode,
) -> Result<Option<ExecutionRequest>, AskError> {
    if !dir.is_dir() {
        return Err(AskError::MissingDirectory(dir.display().to_string()));
    }
    let overrides = match &args.catalog {
        Some(path) => load_catalog(path)?,
        None => BTreeMap::new(),
    };
    let candidates = candidates_with_overrides(dir, &overrides)?;
    let mut routing_log = crate::routing_log::RoutingLog::from_args(args.routing_log.as_deref());
    let model_client = model_routing_client(args)?;
    if args.calibrate {
        let client = model_client.as_ref().ok_or_else(|| {
            AskError::Inference("--calibrate requires --graph and --canister".into())
        })?;
        let report = calibrate(args, client)?;
        if let Some(path) = &args.calibration_out {
            let text = serde_json::to_string_pretty(&report)
                .map_err(|error| AskError::Inference(error.to_string()))?;
            fs::write(path, text + "\n").map_err(|error| AskError::Io(error.to_string()))?;
        }
        print_calibration(&report, args.json)?;
        return Ok(None);
    }
    if let Some(client) = &model_client
        && let Some(path) = &args.eval
    {
        let text = fs::read_to_string(path).map_err(|error| AskError::Io(error.to_string()))?;
        let items = parse_golden_set(&text)?;
        let report = evaluate_general_route(&items, client)?;
        print_eval(&report, args.json);
        return Ok(None);
    }
    if model_client.is_some() && args.slot_eval.is_some() {
        return Err(AskError::Inference(
            "model routing supports gleaph ask, gleaph interpret, or --eval, not --slot-eval"
                .into(),
        ));
    }
    if let Some(path) = &args.slot_eval {
        let entities_path = args.entities.as_ref().ok_or_else(|| {
            AskError::Slots("--slot-eval requires --entities <vocabulary.json>".into())
        })?;
        let vocab = slots::EntityVocab::load(entities_path)
            .map_err(|error| AskError::Slots(error.to_string()))?;
        let client = args.model_url.as_ref().map(|endpoint| {
            let api_key = std::env::var(&args.api_key_env).ok();
            DecisionModelClient::new(
                endpoint.clone(),
                args.model.clone(),
                api_key,
                Duration::from_secs(args.model_timeout_secs),
            )
        });
        evaluate_slots(path, &candidates, &vocab, client.as_ref(), args)?;
        return Ok(None);
    }
    if let Some(path) = &args.eval {
        let text = fs::read_to_string(path).map_err(|error| AskError::Io(error.to_string()))?;
        let items = parse_golden_set(&text)?;
        if let Some(endpoint) = &args.model_url {
            let api_key = std::env::var(&args.api_key_env).ok();
            let client = DecisionModelClient::new(
                endpoint.clone(),
                args.model.clone(),
                api_key,
                Duration::from_secs(args.model_timeout_secs),
            );
            let comparison = evaluate_comparison(
                &items,
                &candidates,
                &client,
                ComparisonOptions {
                    model_name: &args.model,
                    mode: args.model_mode,
                    limit: args.limit,
                    threshold: args.threshold,
                    shadow: args.shadow.as_deref(),
                    abstain_option: args.abstain_option.as_deref(),
                },
            )?;
            print_comparison(&comparison, args.json);
        } else {
            let report = evaluate(&items, &candidates, args.limit, args.threshold);
            print_eval(&report, args.json);
        }
        return Ok(None);
    }
    let question = args.question.as_deref().ok_or(AskError::MissingQuestion)?;
    let lexical = route(question, &candidates, args.limit, args.threshold);
    let mut routing = lexical.clone();
    let mut model_record = serde_json::Value::Null;
    if let Some(endpoint) = &args.model_url {
        let api_key = std::env::var(&args.api_key_env).ok();
        let client = DecisionModelClient::new(
            endpoint.clone(),
            args.model.clone(),
            api_key,
            Duration::from_secs(args.model_timeout_secs),
        );
        let mut described: Vec<(String, String)> = candidates
            .iter()
            .filter(|candidate| candidate.kind == OperationKind::Query)
            .map(|candidate| (candidate.name.clone(), candidate.description.clone()))
            .collect();
        if let Some(description) = &args.abstain_option {
            described.push((ABSTAIN_OPTION.to_owned(), description.clone()));
        }
        let abstain_name = args.abstain_option.as_ref().map(|_| ABSTAIN_OPTION);
        match client.judge(question, &described, args.model_mode) {
            Ok(judgements) => {
                routing = routing_from_judgements(
                    judgements,
                    &candidates,
                    args.limit,
                    args.threshold,
                    abstain_name,
                );
                model_record = serde_json::json!({
                    "model": args.model,
                    "hits": routing.hits,
                    "abstained": routing.abstained,
                });
            }
            Err(error) => {
                eprintln!(
                    "warning: decision model unavailable ({error}); using the lexical baseline"
                );
                model_record = serde_json::json!({
                    "model": args.model,
                    "endpoint": endpoint,
                    "error": error.to_string(),
                });
            }
        }
    }
    if let Some(path) = &args.shadow {
        append_shadow(path, question, &candidates, &lexical, &model_record)?;
    }
    // A graph-backed ask/plan reports the Router interpretation below; the local lexical
    // baseline is only the answer when no Router client is configured.
    if model_client.is_none() {
        print_routing(question, &routing, args.json);
    }
    if mode == AskMode::Interpret {
        match &model_client {
            Some(client) => {
                let started = std::time::Instant::now();
                let route = client
                    .interpret(question)
                    .map_err(|error| AskError::Inference(error.to_string()))?;
                let elapsed = started.elapsed();
                let decision = (route.status
                    == gleaph_sdk::routing::InterpretationStatus::Answered)
                    .then(|| route.operation.as_deref())
                    .flatten();
                routing_log.record_interpretation(question, &route, decision, elapsed);
                if args.json {
                    let json = interpretation_json(&route)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json)
                            .map_err(|error| AskError::Answer(error.to_string()))?
                    );
                } else {
                    match (route.status, route.operation.as_deref()) {
                        (gleaph_sdk::routing::InterpretationStatus::Unsupported, _) => {
                            println!("interpretation: unsupported")
                        }
                        (
                            gleaph_sdk::routing::InterpretationStatus::NeedsInput,
                            Some(operation),
                        ) => {
                            println!(
                                "interpretation: {operation} (missing: {})",
                                route.missing.join(", ")
                            );
                        }
                        (_, Some(operation)) => {
                            println!(
                                "interpretation: {operation} {}",
                                interpretation_display(&route)
                            );
                        }
                        (_, None) => println!("interpretation: unsupported"),
                    }
                }
            }
            None => {
                let entities_path = args.entities.as_ref().ok_or_else(|| {
                    AskError::Slots("gleaph interpret requires --entities".into())
                })?;
                let vocab = slots::EntityVocab::load(entities_path)
                    .map_err(|error| AskError::Slots(error.to_string()))?;
                match routing.best() {
                    None => println!("interpretation: unsupported"),
                    Some(best) => match slots::extract(question, &best.name, &vocab) {
                        Ok(bindings) => println!("interpretation: {} {}", best.name, bindings),
                        Err(error) => {
                            println!("interpretation: {} (slots unavailable: {error})", best.name)
                        }
                    },
                }
            }
        }
    }
    if mode == AskMode::Answer {
        if let Some(client) = &model_client {
            let started = std::time::Instant::now();
            let answer = client
                .ask(question, gleaph_sdk::routing::ReadMode::Eventual)
                .map_err(|error| AskError::Inference(error.to_string()))?;
            let elapsed = started.elapsed();
            let decision = (answer.status == gleaph_sdk::routing::AskStatus::Answered)
                .then(|| answer.interpretation.operation.as_deref())
                .flatten();
            routing_log.record_interpretation(question, &answer.interpretation, decision, elapsed);
            print_answer(question, &answer, args.json)?;
            return Ok(None);
        }
        let entities_path = args.entities.as_ref().ok_or_else(|| {
            AskError::Slots("gleaph ask requires --entities when no --graph is set".into())
        })?;
        let vocab = slots::EntityVocab::load(entities_path)
            .map_err(|error| AskError::Slots(error.to_string()))?;
        return match routing.best() {
            None => {
                println!("answer: unsupported");
                Ok(None)
            }
            Some(best) => {
                // A routed operation whose slots cannot be bound is not an answer: abstain rather
                // than error, so out-of-scope questions that merely resemble a catalog entry
                // (no known entity/number) stay fail-closed at the routing layer.
                let bindings = match slots::extract(question, &best.name, &vocab) {
                    Ok(bindings) => bindings,
                    Err(error) => {
                        println!("answer: unsupported (slots unavailable: {error})");
                        return Ok(None);
                    }
                };
                let (name, params) = execution_plan(&best.name, &bindings, Some(&vocab))?;
                println!("answer: {name} {bindings}");
                Ok(Some(ExecutionRequest { name, params }))
            }
        };
    }
    Ok(None)
}

/// Turn per-candidate probabilities into the same routing shape the lexical baseline returns.
fn routing_from_judgements(
    mut judgements: Vec<Judgement>,
    candidates: &[Candidate],
    limit: usize,
    threshold: f64,
    abstain_name: Option<&str>,
) -> Routing {
    // The synthetic "none of these" option competes with the real candidates. If it wins, the
    // routing abstains and it never appears as a hit.
    let mut chose_abstain = false;
    if let Some(name) = abstain_name
        && let Some(position) = judgements
            .iter()
            .position(|judgement| judgement.name == name)
    {
        let abstain = judgements.remove(position);
        chose_abstain = judgements
            .iter()
            .all(|judgement| judgement.noul <= abstain.noul);
    }
    judgements.sort_by(|left, right| {
        right
            .noul
            .partial_cmp(&left.noul)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.name.cmp(&right.name))
    });
    let hits: Vec<RouteHit> = judgements
        .into_iter()
        .take(limit.max(1))
        .map(|judgement| RouteHit {
            description: candidates
                .iter()
                .find(|candidate| candidate.name == judgement.name)
                .map(|candidate| candidate.description.clone())
                .unwrap_or_default(),
            name: judgement.name,
            score: judgement.noul,
        })
        .collect();
    let abstained = chose_abstain || hits.first().is_none_or(|hit| hit.score < threshold);
    Routing {
        hits,
        abstained,
        threshold,
    }
}

/// Append one shadow record so model and baseline decisions can be compared offline.
fn append_shadow(
    path: &Path,
    question: &str,
    candidates: &[Candidate],
    lexical: &Routing,
    model: &serde_json::Value,
) -> Result<(), AskError> {
    let record = serde_json::json!({
        "ts_unix": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default(),
        "question": question,
        "candidates": candidates.iter().map(|candidate| candidate.name.clone()).collect::<Vec<_>>(),
        "lexical": {
            "hits": lexical.hits,
            "abstained": lexical.abstained,
            "threshold": lexical.threshold,
        },
        "model": model,
    });
    let mut line =
        serde_json::to_string(&record).map_err(|error| AskError::Shadow(error.to_string()))?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| AskError::Shadow(error.to_string()))?;
    file.write_all(line.as_bytes())
        .map_err(|error| AskError::Shadow(error.to_string()))
}

/// One slot-labelled golden item.
#[derive(Debug, serde::Deserialize)]
struct SlotItem {
    question: String,
    #[serde(default)]
    expected_op: Option<String>,
    #[serde(default)]
    slots: BTreeMap<String, serde_json::Value>,
}

fn slot_matches(name: &str, expected: &serde_json::Value, bindings: &slots::SlotBindings) -> bool {
    match name {
        "anchor" => expected
            .as_str()
            .is_some_and(|value| bindings.anchor.as_deref() == Some(value)),
        "from" => expected
            .as_str()
            .is_some_and(|value| bindings.from.as_deref() == Some(value)),
        "to" => expected
            .as_str()
            .is_some_and(|value| bindings.to.as_deref() == Some(value)),
        "topic" => expected
            .as_str()
            .is_some_and(|value| bindings.topic.as_deref() == Some(value)),
        "team" => expected
            .as_str()
            .is_some_and(|value| bindings.team.as_deref() == Some(value)),
        "depth" => expected
            .as_u64()
            .is_some_and(|value| bindings.depth == Some(value as u32)),
        "hops" => expected
            .as_u64()
            .is_some_and(|value| bindings.hops == Some(value as u32)),
        _ => false,
    }
}

/// Score slot extraction twice: with the gold operation (extractor quality) and with the routed
/// operation (what a user actually gets).
/// Route every golden-set item through the Router general route and aggregate the result.
fn evaluate_general_route(
    items: &[GoldenItem],
    client: &gleaph_sdk::routing::AskClient,
) -> Result<EvalReport, AskError> {
    let mut report = EvalReport::empty(items.len());
    for item in items {
        let route = client
            .interpret(&item.question)
            .map_err(|error| AskError::Inference(error.to_string()))?;
        let got = route.operation.clone();
        let lang = item.lang.clone().unwrap_or_else(|| "unknown".into());
        let slice = report.per_lang.entry(lang).or_default();
        slice.items += 1;
        match &item.expected_op {
            Some(expected) => {
                let op = report.per_op.entry(expected.clone()).or_default();
                op.items += 1;
                let routed = got.as_deref() == Some(expected.as_str());
                if item.expect_incomplete {
                    report.incomplete_expected_items += 1;
                    if routed {
                        report.incomplete_correct += 1;
                        op.correct += 1;
                        slice.correct += 1;
                    } else {
                        report.errors.push(EvalError {
                            question: item.question.clone(),
                            expected: Some(expected.clone()),
                            got,
                            abstained: route.operation.is_none(),
                        });
                    }
                } else {
                    report.routed_items += 1;
                    let executed = routed
                        && route.status == gleaph_sdk::routing::InterpretationStatus::Answered;
                    if executed {
                        report.routed_correct += 1;
                        op.correct += 1;
                        slice.correct += 1;
                    } else {
                        if route.status == gleaph_sdk::routing::InterpretationStatus::NeedsInput {
                            report.incomplete_items += 1;
                        }
                        report.errors.push(EvalError {
                            question: item.question.clone(),
                            expected: Some(expected.clone()),
                            got,
                            abstained: route.operation.is_none(),
                        });
                    }
                }
            }
            None => {
                report.abstain_items += 1;
                if route.operation.is_none() {
                    report.abstain_correct += 1;
                    slice.correct += 1;
                } else {
                    report.errors.push(EvalError {
                        question: item.question.clone(),
                        expected: None,
                        got,
                        abstained: false,
                    });
                }
            }
        }
    }
    Ok(report)
}

fn evaluate_slots(
    path: &Path,
    candidates: &[Candidate],
    vocab: &slots::EntityVocab,
    client: Option<&DecisionModelClient>,
    args: &AskArgs,
) -> Result<(), AskError> {
    let text = fs::read_to_string(path).map_err(|error| AskError::Io(error.to_string()))?;
    let items: Vec<SlotItem> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|error| AskError::Slots(error.to_string()))?;
    let mut described: Vec<(String, String)> = candidates
        .iter()
        .filter(|candidate| candidate.kind == OperationKind::Query)
        .map(|candidate| (candidate.name.clone(), candidate.description.clone()))
        .collect();
    if let Some(description) = &args.abstain_option {
        described.push((ABSTAIN_OPTION.to_owned(), description.clone()));
    }
    let abstain_name = args.abstain_option.as_ref().map(|_| ABSTAIN_OPTION);

    let mut per_slot: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut total = 0usize;
    let mut oracle_full = 0usize;
    let mut routed_op_correct = 0usize;
    let mut combined_full = 0usize;
    let mut mismatches = Vec::new();
    for item in &items {
        let Some(expected_op) = &item.expected_op else {
            continue;
        };
        total += 1;
        let routed: Option<String> = match client {
            Some(client) => match client.judge(&item.question, &described, args.model_mode) {
                Ok(judgements) => routing_from_judgements(
                    judgements,
                    candidates,
                    args.limit,
                    args.threshold,
                    abstain_name,
                )
                .best()
                .map(|hit| hit.name.clone()),
                Err(_) => None,
            },
            None => route(&item.question, candidates, args.limit, args.threshold)
                .best()
                .map(|hit| hit.name.clone()),
        };
        let oracle = slots::extract(&item.question, expected_op, vocab);
        let oracle_bindings = oracle.as_ref().ok();
        let mut oracle_ok = true;
        for (name, expected) in &item.slots {
            let entry = per_slot.entry(name.clone()).or_default();
            entry.0 += 1;
            let matched =
                oracle_bindings.is_some_and(|bindings| slot_matches(name, expected, bindings));
            if matched {
                entry.1 += 1;
            } else {
                oracle_ok = false;
            }
        }
        if oracle_bindings.is_none() {
            oracle_ok = false;
        }
        if oracle_ok {
            oracle_full += 1;
        }
        let combined_bindings = routed
            .as_deref()
            .and_then(|op| slots::extract(&item.question, op, vocab).ok());
        let op_correct = routed.as_deref() == Some(expected_op.as_str());
        if op_correct {
            routed_op_correct += 1;
        }
        let combined_ok = op_correct
            && combined_bindings.as_ref().is_some_and(|bindings| {
                item.slots
                    .iter()
                    .all(|(name, expected)| slot_matches(name, expected, bindings))
            });
        if combined_ok {
            combined_full += 1;
        } else if mismatches.len() < 10 {
            mismatches.push(format!(
                "expected {} {:?}, routed {:?}, extracted {:?} :: {}",
                expected_op, item.slots, routed, combined_bindings, item.question
            ));
        }
    }
    println!("slot evaluation: {total} items");
    println!(
        "oracle operation (extractor quality): full {oracle_full}/{total} ({:.1}%)",
        percentage(oracle_full, total)
    );
    for (name, (expected, correct)) in &per_slot {
        println!(
            "  {name:8} {correct}/{expected} ({:.1}%)",
            percentage(*correct, *expected)
        );
    }
    let router_name = if client.is_some() { "model" } else { "lexical" };
    println!(
        "{router_name} routing: operation {routed_op_correct}/{total}; operation+slots {combined_full}/{total} ({:.1}%)",
        percentage(combined_full, total)
    );
    for mismatch in &mismatches {
        println!("  mismatch: {mismatch}");
    }
    Ok(())
}

fn percentage(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn print_routing(question: &str, routing: &Routing, json: bool) {
    if json {
        let value = serde_json::json!({
            "question": question,
            "abstained": routing.abstained,
            "threshold": routing.threshold,
            "hits": routing.hits,
        });
        println!("{value}");
        return;
    }
    println!("question: {question}");
    for (index, hit) in routing.hits.iter().enumerate() {
        let first_line = hit.description.lines().next().unwrap_or_default();
        println!(
            "{:>2}. {:<28} {:.3}  {}",
            index + 1,
            hit.name,
            hit.score,
            first_line
        );
    }
    println!(
        "abstain: {} (threshold {:.3})",
        routing.abstained, routing.threshold
    );
}

/// Load a routing-description override catalog (`{ "<op>": "<use when>" }`).
pub fn load_catalog(path: &Path) -> Result<BTreeMap<String, String>, AskError> {
    let text = fs::read_to_string(path).map_err(|error| AskError::Io(error.to_string()))?;
    serde_json::from_str(&text).map_err(|error| AskError::Catalog(error.to_string()))
}

fn print_comparison(comparison: &ComparisonReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(comparison).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!("items: {}", comparison.items);
    println!("== lexical baseline ==");
    print_eval(&comparison.lexical, false);
    println!("== model ({}) ==", comparison.model.model);
    print_eval(&comparison.model.report, false);
    if comparison.model.errors > 0 {
        println!(
            "model errors: {} (counted as abstentions)",
            comparison.model.errors
        );
        for example in &comparison.model.error_examples {
            println!("  {example}");
        }
    }
    println!(
        "flips: agree {} / lexical-only {} / model-only {}",
        comparison.flips.agree, comparison.flips.lexical_only, comparison.flips.model_only
    );
}

fn print_eval(report: &EvalReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!("items: {}", report.items);
    println!(
        "top-1: {}/{} ({:.1}%)",
        report.routed_correct,
        report.routed_items,
        report.top1_accuracy() * 100.0
    );
    println!(
        "abstain recall: {}/{} ({:.1}%)",
        report.abstain_correct,
        report.abstain_items,
        report.abstain_recall() * 100.0
    );
    println!(
        "overall: {}/{} ({:.1}%)",
        report.routed_correct + report.incomplete_correct + report.abstain_correct,
        report.items,
        report.overall_accuracy() * 100.0
    );
    if report.incomplete_expected_items > 0 {
        println!(
            "incomplete-intent: {}/{}",
            report.incomplete_correct, report.incomplete_expected_items
        );
    }
    if !report.per_op.is_empty() {
        println!("per op:");
        for (name, slice) in &report.per_op {
            println!(
                "  {:<28} {}/{} ({:.1}%)",
                name,
                slice.correct,
                slice.items,
                ratio(slice.correct, slice.items) * 100.0
            );
        }
    }
    if !report.per_lang.is_empty() {
        println!("per lang:");
        for (lang, slice) in &report.per_lang {
            println!(
                "  {:<28} {}/{} ({:.1}%)",
                lang,
                slice.correct,
                slice.items,
                ratio(slice.correct, slice.items) * 100.0
            );
        }
    }
    if !report.errors.is_empty() {
        const SHOWN: usize = 10;
        println!("errors (showing {SHOWN}):");
        for error in report.errors.iter().take(SHOWN) {
            println!(
                "  expected {:?}, got {:?} (abstained: {}) :: {}",
                error.expected, error.got, error.abstained, error.question
            );
        }
        if report.errors.len() > SHOWN {
            println!("  ... {} more", report.errors.len() - SHOWN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer_with_rows() -> gleaph_sdk::routing::Answer {
        let rows = gleaph_gql_ic_wire::GqlWireRows {
            rows: vec![gleaph_gql_ic_wire::GqlWireRow {
                columns: vec![
                    (
                        "title".to_owned(),
                        gleaph_gql_ic_wire::GqlWireValue::Text("Seven Samurai".into()),
                    ),
                    (
                        "year".to_owned(),
                        gleaph_gql_ic_wire::GqlWireValue::Uint64(1954),
                    ),
                ],
            }],
        }
        .encode_blob()
        .expect("encode rows");
        gleaph_sdk::routing::Answer {
            status: gleaph_sdk::routing::AskStatus::Answered,
            interpretation: gleaph_sdk::routing::Interpretation {
                status: gleaph_sdk::routing::InterpretationStatus::Answered,
                operation: Some("films-by-conditions".into()),
                parameters: vec![gleaph_sdk::routing::AskParameter {
                    name: "genre".into(),
                    value: gleaph_sdk::routing::AskParameterValue::Text("ドラマ映画".into()),
                }],
                missing: Vec::new(),
            },
            rows: Some(gleaph_sdk::routing::AnswerRows {
                row_count: 1,
                rows_blob: Some(rows),
            }),
            failure: None,
        }
    }

    #[test]
    fn answer_json_includes_decoded_rows() {
        let json = answer_json(&answer_with_rows()).expect("answer json");
        assert_eq!(json["status"], "answered");
        assert_eq!(json["operation"], "films-by-conditions");
        assert_eq!(json["rows"]["row_count"], 1);
        assert_eq!(json["rows"]["rows"][0]["title"], "Seven Samurai");
        assert_eq!(json["rows"]["rows"][0]["year"], 1954);
    }

    #[test]
    fn needs_input_answer_json_keeps_missing_parameters() {
        let answer = gleaph_sdk::routing::Answer {
            status: gleaph_sdk::routing::AskStatus::NeedsInput,
            interpretation: gleaph_sdk::routing::Interpretation {
                status: gleaph_sdk::routing::InterpretationStatus::NeedsInput,
                operation: Some("films-by-conditions".into()),
                parameters: Vec::new(),
                missing: vec!["from".into(), "to".into()],
            },
            rows: None,
            failure: None,
        };
        let json = answer_json(&answer).expect("answer json");
        assert_eq!(json["status"], "needs_input");
        assert_eq!(json["missing"], serde_json::json!(["from", "to"]));
    }

    fn candidate(name: &str, description: &str) -> Candidate {
        Candidate {
            name: name.into(),
            description: description.into(),
            parameters: vec![],
            kind: OperationKind::Query,
        }
    }

    fn demo_candidates() -> Vec<Candidate> {
        vec![
            candidate(
                "variable-length-reach",
                "which Concepts are reachable from a start Concept through RELATED_TO in 1..3 hops",
            ),
            candidate(
                "shortest-path",
                "shortest RELATED_TO chain between two Concepts",
            ),
            candidate(
                "citation-reach",
                "Documents reachable from a document through CITES up to depth 3, returning the citation trail",
            ),
            candidate(
                "team-readable-documents",
                "access-constrained semantic search: documents about a query that a team can read, using vector similarity",
            ),
        ]
    }

    #[test]
    fn tokenize_splits_ascii_words_and_cjk_bigrams() {
        assert_eq!(
            tokenize("Citation graph"),
            vec!["cit".to_string(), "graph".to_string()]
        );
        assert_eq!(tokenize("cites cited citation"), vec!["cit".to_string()]);
        assert_eq!(tokenize("documents"), vec!["document".to_string()]);
        assert_eq!(
            tokenize("引用の経路"),
            vec![
                "引用".to_string(),
                "用の".to_string(),
                "の経".to_string(),
                "経路".to_string()
            ]
        );
        assert!(tokenize("the and or").is_empty());
    }

    #[test]
    fn declared_parameters_ignores_quoted_dollars() {
        let source =
            "MATCH (n {title: '$not-a-param'}) WHERE n.x = $query AND n.y = $depth RETURN n";
        assert_eq!(
            declared_parameters(source),
            vec!["query".to_string(), "depth".to_string()]
        );
    }

    #[test]
    fn route_prefers_the_described_operation() {
        let routing = route(
            "which documents cite the GraphRAG paper up to depth 3?",
            &demo_candidates(),
            DEFAULT_LIMIT,
            DEFAULT_THRESHOLD,
        );
        assert!(!routing.abstained);
        assert_eq!(
            routing.best().map(|hit| hit.name.as_str()),
            Some("citation-reach")
        );
    }

    #[test]
    fn route_abstains_on_unrelated_question() {
        let routing = route(
            "what is the weather in Paris tomorrow?",
            &demo_candidates(),
            DEFAULT_LIMIT,
            DEFAULT_THRESHOLD,
        );
        assert!(routing.abstained);
    }

    #[test]
    fn route_is_deterministic_for_equal_scores() {
        let candidates = vec![
            candidate("beta", "same text"),
            candidate("alpha", "same text"),
        ];
        let routing = route("same text", &candidates, 2, 0.0);
        let names: Vec<&str> = routing.hits.iter().map(|hit| hit.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn evaluate_counts_hits_abstains_and_slices() {
        let items = vec![
            GoldenItem {
                question: "citation reach from a document through cites up to depth 3".into(),
                expected_op: Some("citation-reach".into()),
                expect_abstain: false,
                expect_incomplete: false,
                lang: Some("en".into()),
                family: None,
            },
            GoldenItem {
                question: "weather forecast".into(),
                expected_op: None,
                expect_abstain: true,
                expect_incomplete: false,
                lang: Some("en".into()),
                family: None,
            },
        ];
        let report = evaluate(&items, &demo_candidates(), DEFAULT_LIMIT, DEFAULT_THRESHOLD);
        assert_eq!(report.routed_items, 1);
        assert_eq!(report.routed_correct, 1);
        assert_eq!(report.abstain_items, 1);
        assert_eq!(report.abstain_correct, 1);
        assert_eq!(report.overall_accuracy(), 1.0);
    }

    #[test]
    fn golden_set_rejects_ambiguous_items() {
        let error = parse_golden_set(r#"{"question":"no expectation"}"#).unwrap_err();
        assert!(matches!(error, AskError::AmbiguousGoldenItem(1)));
    }

    fn bindings(anchor: &str, depth: Option<u32>, hops: Option<u32>) -> slots::SlotBindings {
        slots::SlotBindings {
            anchor: Some(anchor.to_owned()),
            depth,
            hops,
            ..Default::default()
        }
    }

    #[test]
    fn execution_plan_selects_depth_and_hop_variants() {
        let (name, params) = execution_plan(
            "citation-reach",
            &bindings("GraphRAG retrieval", Some(2), None),
            None,
        )
        .expect("plan");
        assert_eq!(name, "citation-reach-depth-2");
        assert_eq!(
            params,
            vec![(
                "anchor".to_owned(),
                gleaph_gql_params::GqlValue::Text("GraphRAG retrieval".into())
            )]
        );

        let (name, _) =
            execution_plan("citation-reach", &bindings("X", Some(9), None), None).expect("clamp");
        assert_eq!(name, "citation-reach-depth-3");

        let (name, _) = execution_plan(
            "variable-length-reach",
            &bindings("Graph databases", None, None),
            None,
        )
        .expect("default hops");
        assert_eq!(name, "variable-length-reach-hops-2");
    }

    #[test]
    fn execution_plan_binds_text_search_topic() {
        let bindings = slots::SlotBindings {
            topic: Some("graph traversal".into()),
            ..Default::default()
        };
        let (name, params) = execution_plan("document-text-search", &bindings, None).expect("plan");
        assert_eq!(name, "document-text-search");
        assert_eq!(
            params,
            vec![(
                "query".to_owned(),
                gleaph_gql_params::GqlValue::Text("graph traversal".into())
            )]
        );
        let error = execution_plan(
            "document-text-search",
            &slots::SlotBindings::default(),
            None,
        )
        .expect_err("missing topic");
        assert!(error.to_string().contains("missing topic"));
    }

    #[test]
    fn execution_plan_binds_shortest_path_endpoints() {
        let bindings = slots::SlotBindings {
            from: Some("Graph databases".into()),
            to: Some("Vector search".into()),
            ..Default::default()
        };
        let (name, params) = execution_plan("shortest-path", &bindings, None).expect("plan");
        assert_eq!(name, "shortest-path-param");
        assert_eq!(
            params,
            vec![
                (
                    "from".to_owned(),
                    gleaph_gql_params::GqlValue::Text("Graph databases".into())
                ),
                (
                    "to".to_owned(),
                    gleaph_gql_params::GqlValue::Text("Vector search".into())
                ),
            ]
        );
    }

    #[test]
    fn execution_plan_binds_team_search_embedding() {
        let bindings = slots::SlotBindings {
            team: Some("Platform".into()),
            topic: Some("Indexing".into()),
            ..Default::default()
        };
        let vocab = slots::EntityVocab {
            document_ids: [("Indexing".to_owned(), "d7".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let (name, params) =
            execution_plan("team-readable-documents", &bindings, Some(&vocab)).expect("plan");
        assert_eq!(name, "team-readable-documents-param");
        assert_eq!(params.len(), 2);
        assert_eq!(
            params[0],
            (
                "team".to_owned(),
                gleaph_gql_params::GqlValue::Text("Platform".into())
            )
        );
        assert_eq!(params[1].0, "query");
        match &params[1].1 {
            gleaph_gql_params::GqlValue::Bytes(bytes) => {
                assert_eq!(bytes.len(), crate::embedding::EMBEDDING_DIMS * 4);
            }
            other => panic!("unexpected query parameter: {other:?}"),
        }
    }

    #[test]
    fn execution_plan_rejects_missing_slots() {
        let error = execution_plan("citation-reach", &slots::SlotBindings::default(), None)
            .expect_err("missing");
        assert!(error.to_string().contains("missing anchor"));
        let error = execution_plan(
            "team-readable-documents",
            &bindings("Indexing", None, None),
            None,
        )
        .expect_err("missing team");
        assert!(error.to_string().contains("missing team"));
    }

    #[test]
    fn catalog_overrides_replace_doc_comment_descriptions() {
        let root = std::env::temp_dir().join(format!("gleaph-ask-prepared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("tempdir");
        std::fs::write(
            root.join("citation-reach.gql"),
            "/// Citation reach from the source comments.\nMATCH (n) RETURN n\n",
        )
        .expect("citation source");
        std::fs::write(
            root.join("shortest-path.gql"),
            "/// Shortest path from the source comments.\nMATCH (n) RETURN n\n",
        )
        .expect("shortest source");

        let overrides = BTreeMap::from([(
            "citation-reach".to_string(),
            "Use when the question asks for cited documents.".to_string(),
        )]);
        let catalogued = candidates_with_overrides(&root, &overrides).expect("candidates");
        let citation = catalogued
            .iter()
            .find(|candidate| candidate.name == "citation-reach")
            .expect("citation candidate");
        assert_eq!(
            citation.description,
            "Use when the question asks for cited documents."
        );
        let untouched = catalogued
            .iter()
            .find(|candidate| candidate.name == "shortest-path")
            .expect("shortest candidate");
        assert_eq!(
            untouched.description,
            "Shortest path from the source comments."
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn abstain_option_winning_makes_the_routing_abstain() {
        let routing = routing_from_judgements(
            vec![
                Judgement {
                    name: "citation-reach".into(),
                    noul: 0.2,
                },
                Judgement {
                    name: ABSTAIN_OPTION.into(),
                    noul: 0.7,
                },
            ],
            &demo_candidates(),
            DEFAULT_LIMIT,
            DEFAULT_THRESHOLD,
            Some(ABSTAIN_OPTION),
        );
        assert!(routing.abstained);
        assert!(
            routing.hits.iter().all(|hit| hit.name != ABSTAIN_OPTION),
            "the synthetic option must never be a hit"
        );
    }

    #[test]
    fn model_judgements_rank_and_abstain() {
        let candidates = demo_candidates();
        let routing = routing_from_judgements(
            vec![
                Judgement {
                    name: "shortest-path".into(),
                    noul: 0.2,
                },
                Judgement {
                    name: "citation-reach".into(),
                    noul: 0.9,
                },
            ],
            &candidates,
            2,
            DEFAULT_THRESHOLD,
            None,
        );
        assert!(!routing.abstained);
        let best = routing.best().expect("hit");
        assert_eq!(best.name, "citation-reach");
        assert!(best.description.contains("Documents reachable"));

        let routing = routing_from_judgements(
            vec![Judgement {
                name: "shortest-path".into(),
                noul: 0.1,
            }],
            &candidates,
            2,
            DEFAULT_THRESHOLD,
            None,
        );
        assert!(routing.abstained);
    }

    fn spawn_stub(requests: usize) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let handle = std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 1024];
                let body_start = loop {
                    let read = stream.read(&mut chunk).expect("read");
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(position) =
                        buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let text = String::from_utf8_lossy(&buffer[..body_start]).to_string();
                let content_length = text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while buffer.len() < body_start + content_length {
                    let read = stream.read(&mut chunk).expect("read body");
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&buffer[body_start..]).expect("json body");
                let state = body["state"].as_str().unwrap_or_default();
                let is_choice = body["questions"]
                    .get("route")
                    .is_some_and(|question| question["type"] == "choice");
                let payload = if is_choice {
                    let probabilities = if state.contains("weather") {
                        serde_json::json!({
                            "variable-length-reach": 0.25,
                            "shortest-path": 0.25,
                            "team-readable-documents": 0.25,
                            "citation-reach": 0.25
                        })
                    } else {
                        serde_json::json!({
                            "variable-length-reach": 0.03,
                            "shortest-path": 0.03,
                            "team-readable-documents": 0.04,
                            "citation-reach": 0.9
                        })
                    };
                    serde_json::json!({
                        "answers": {
                            "route": {
                                "type": "choice",
                                "choice": "citation-reach",
                                "probabilities": probabilities,
                                "confidence": 0.9
                            }
                        }
                    })
                    .to_string()
                } else {
                    let question_count = body["questions"]
                        .as_object()
                        .map(|map| map.len())
                        .unwrap_or(0);
                    let mut answers = serde_json::Map::new();
                    for index in 0..question_count {
                        let noul = if state.contains("weather") {
                            0.05
                        } else if index == 2 {
                            0.9
                        } else {
                            0.05
                        };
                        answers.insert(
                            format!("candidate_{index}"),
                            serde_json::json!({ "noul": noul }),
                        );
                    }
                    serde_json::json!({ "answers": answers }).to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                stream.write_all(response.as_bytes()).expect("write");
            }
        });
        (format!("http://{address}/v1/systemone"), handle)
    }

    fn comparison_items() -> Vec<GoldenItem> {
        parse_golden_set(
            r#"{"question":"Which documents are reachable from The GQL standard through CITES up to depth 2?","expected_op":"citation-reach","lang":"en"}
{"question":"What is the weather in Paris tomorrow?","expect_abstain":true,"lang":"en"}"#,
        )
        .expect("golden")
    }

    fn comparison_client(url: String) -> DecisionModelClient {
        DecisionModelClient::new(url, "stub".into(), None, std::time::Duration::from_secs(5))
    }

    #[test]
    fn comparison_reports_both_routers_and_agreement() {
        let items = comparison_items();
        let (url, handle) = spawn_stub(items.len());
        let comparison = evaluate_comparison(
            &items,
            &demo_candidates(),
            &comparison_client(url),
            ComparisonOptions {
                model_name: "stub",
                mode: JudgeMode::Choice,
                limit: DEFAULT_LIMIT,
                threshold: DEFAULT_THRESHOLD,
                shadow: None,
                abstain_option: None,
            },
        )
        .expect("comparison");
        handle.join().expect("stub");
        assert_eq!(comparison.items, 2);
        assert_eq!(comparison.lexical.routed_correct, 1);
        assert_eq!(comparison.lexical.abstain_correct, 1);
        assert_eq!(
            comparison.model.report.routed_correct, 1,
            "{:?}",
            comparison.model.report
        );
        assert_eq!(comparison.model.report.abstain_correct, 1);
        assert_eq!(comparison.model.errors, 0, "{:?}", comparison.model);
        assert_eq!(comparison.flips.agree, 2);
    }

    #[test]
    fn comparison_counts_model_failures_as_abstentions() {
        let items = comparison_items();
        let client = comparison_client("http://127.0.0.1:1/v1/systemone".into());
        let comparison = evaluate_comparison(
            &items,
            &demo_candidates(),
            &client,
            ComparisonOptions {
                model_name: "stub",
                mode: JudgeMode::Choice,
                limit: DEFAULT_LIMIT,
                threshold: DEFAULT_THRESHOLD,
                shadow: None,
                abstain_option: None,
            },
        )
        .expect("comparison");
        assert_eq!(comparison.model.errors, 2);
        assert_eq!(comparison.model.report.routed_correct, 0);
        assert_eq!(comparison.model.report.abstain_correct, 1);
        assert!(!comparison.model.error_examples.is_empty());
    }

    #[test]
    fn comparison_shadow_records_each_item() {
        let items = comparison_items();
        let (url, handle) = spawn_stub(items.len());
        let path = std::env::temp_dir().join(format!(
            "gleaph-ask-comparison-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let comparison = evaluate_comparison(
            &items,
            &demo_candidates(),
            &comparison_client(url),
            ComparisonOptions {
                model_name: "stub",
                mode: JudgeMode::Choice,
                limit: DEFAULT_LIMIT,
                threshold: DEFAULT_THRESHOLD,
                shadow: Some(&path),
                abstain_option: None,
            },
        )
        .expect("comparison");
        handle.join().expect("stub");
        assert_eq!(comparison.items, 2);
        let text = std::fs::read_to_string(&path).expect("shadow");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let record: serde_json::Value = serde_json::from_str(lines[0]).expect("json line");
        assert_eq!(record["question"], items[0].question);
        assert!(record["lexical"]["hits"].is_array());
        assert!(record["model"]["hits"].is_array());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn general_bindings_map_typed_route_slots() {
        use gleaph_router_wire::AskParameterValue;
        use gleaph_sdk::routing::{DebugInterpretation, DebugSlot};
        let route = DebugInterpretation {
            operation: Some("citation-reach".into()),
            probabilities: vec![0.9],
            slots: vec![
                DebugSlot {
                    name: "start".into(),
                    value: gleaph_router_wire::AskParameterValue::Text("The GQL standard".into()),
                    surface: "The GQL standard".into(),
                    start: 19,
                    end: 34,
                    rule: "start".into(),
                },
                DebugSlot {
                    name: "depth".into(),
                    value: gleaph_router_wire::AskParameterValue::Int(2),
                    surface: "two".into(),
                    start: 40,
                    end: 43,
                    rule: "hops".into(),
                },
            ],
            incomplete: false,
            abstain: false,
            context_fingerprint: vec![0; 32],
            model_version: 1,
        };
        let bindings = general_bindings(&route);
        assert_eq!(bindings.anchor.as_deref(), Some("The GQL standard"));
        assert_eq!(bindings.depth, Some(2));
        assert!(bindings.from.is_none());
        let (name, params) = execution_plan("citation-reach", &bindings, None).expect("plan");
        assert_eq!(name, "citation-reach-depth-2");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].0, "anchor");
    }

    #[test]
    fn calibration_sweep_prefers_coverage_under_targets() {
        let rows = vec![
            CalibrationRow {
                expected_op: Some("a".into()),
                oos: false,
                max_probability: 0.9,
                operation: Some("a".into()),
                rule_abstain: false,
            },
            CalibrationRow {
                expected_op: Some("a".into()),
                oos: false,
                max_probability: 0.8,
                operation: Some("b".into()),
                rule_abstain: false,
            },
            CalibrationRow {
                expected_op: None,
                oos: true,
                max_probability: 0.3,
                operation: Some("a".into()),
                rule_abstain: false,
            },
            CalibrationRow {
                expected_op: None,
                oos: true,
                max_probability: 0.1,
                operation: Some("a".into()),
                rule_abstain: false,
            },
        ];
        let (bps, coverage, selective, oos, meets) = recommend_threshold(&rows, 0.5, 0.5);
        assert!(meets);
        assert_eq!(bps, 3_000);
        assert_eq!(coverage, 1.0);
        assert_eq!(selective, 0.5);
        assert_eq!(oos, 1.0);
    }

    #[test]
    fn real_query_gate_parses() {
        let text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../demo/movies/eval/real-queries-2026-11-12.jsonl"
        ));
        let items = parse_golden_set(text).expect("parse real-query gate");
        assert_eq!(items.len(), 58);
        assert!(items.iter().any(|item| item.expected_op.is_some()));
        assert!(items.iter().any(|item| item.expect_incomplete));
        assert!(items.iter().any(|item| item.expect_abstain));
    }

    #[test]
    fn general_bindings_accept_integer_from_and_to() {
        let slot = |name: &str, value: gleaph_router_wire::AskParameterValue, surface: &str| {
            gleaph_sdk::routing::DebugSlot {
                name: name.to_owned(),
                value,
                surface: surface.to_owned(),
                start: 0,
                end: surface.len() as u32,
                rule: "decade".to_owned(),
            }
        };
        let route = gleaph_sdk::routing::DebugInterpretation {
            operation: Some("films-by-conditions".to_owned()),
            probabilities: vec![1.0],
            slots: vec![
                slot(
                    "from",
                    gleaph_router_wire::AskParameterValue::Int(1990),
                    "1990年代",
                ),
                slot(
                    "to",
                    gleaph_router_wire::AskParameterValue::Int(1999),
                    "1990年代",
                ),
                slot(
                    "genre",
                    gleaph_router_wire::AskParameterValue::Text("SF".to_owned()),
                    "SF",
                ),
            ],
            incomplete: false,
            abstain: false,
            context_fingerprint: vec![0; 32],
            model_version: 1,
        };
        let bindings = general_bindings(&route);
        assert_eq!(bindings.from.as_deref(), Some("1990"));
        assert_eq!(bindings.to.as_deref(), Some("1999"));
        assert_eq!(bindings.genre.as_deref(), Some("SF"));

        let (name, params) =
            execution_plan("films-by-conditions", &bindings, None).expect("movie plan");
        assert_eq!(name, "films-by-conditions");
        assert_eq!(params[0].0, "from");
        assert!(matches!(
            params[0].1,
            gleaph_gql_params::GqlValue::Int64(1990)
        ));
        assert_eq!(params[2].0, "genre");
    }

    #[test]
    fn execution_plan_selects_movie_synopsis_and_cast_operations() {
        let mut bindings = slots::SlotBindings::default();
        bindings.topic = Some("time travel".to_owned());
        let (name, params) =
            execution_plan("synopsis-search", &bindings, None).expect("synopsis plan");
        assert_eq!(name, "synopsis-search");
        assert_eq!(params[0].0, "query");

        bindings.a = Some("七人の侍".to_owned());
        bindings.b = Some("用心棒".to_owned());
        let (name, params) = execution_plan("common-cast", &bindings, None).expect("cast plan");
        assert_eq!(name, "common-cast");
        assert_eq!(params[0].0, "a");
        assert_eq!(params[1].0, "b");

        bindings.anchor = Some("七人の侍".to_owned());
        let (name, params) = execution_plan("sequel-chain", &bindings, None).expect("sequel plan");
        assert_eq!(name, "sequel-chain");
        assert_eq!(params[0].0, "from");
    }

    #[test]
    fn general_bindings_keep_text_entities() {
        let route = gleaph_sdk::routing::DebugInterpretation {
            operation: Some("shortest-path".to_owned()),
            probabilities: vec![1.0],
            slots: vec![gleaph_sdk::routing::DebugSlot {
                name: "from".to_owned(),
                value: gleaph_router_wire::AskParameterValue::Text("七人の侍".to_owned()),
                surface: "七人の侍".to_owned(),
                start: 0,
                end: 4,
                rule: "anchor".to_owned(),
            }],
            incomplete: false,
            abstain: false,
            context_fingerprint: vec![0; 32],
            model_version: 1,
        };
        let bindings = general_bindings(&route);
        assert_eq!(bindings.from.as_deref(), Some("七人の侍"));
    }
}
