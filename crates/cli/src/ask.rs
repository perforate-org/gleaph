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

/// Arguments for `gleaph ask`.
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
    /// Print the routed operation together with its extracted slot bindings (requires
    /// `--entities`).
    #[arg(long)]
    pub plan: bool,
    /// Route, extract slots, and execute the matching parameterised prepared query through
    /// Router (requires `--entities` and a `--network`/mapping).
    #[arg(long)]
    pub execute: bool,
    /// Inference-enabled graph name for model-based routing; requires `--canister` and
    /// `--tokenizer`. The Router proxies the deployment inference canister.
    #[arg(long, value_name = "NAME")]
    pub graph: Option<String>,
    /// `tokenizer.json` for model-based routing; requires `--graph` and `--canister`.
    #[arg(long, value_name = "PATH")]
    pub tokenizer: Option<PathBuf>,
    /// Append opt-in structured routing events (JSONL) for offline quality analysis.
    #[arg(long, value_name = "PATH")]
    pub routing_log: Option<PathBuf>,
    /// Router canister principal for `--execute`.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Network name (`local`) or endpoint for `--execute`.
    #[arg(long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM identity file for `--execute`.
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
    /// Model-based routing (inference canister + tokenizer) failed.
    #[error("model routing: {0}")]
    Inference(String),
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
        ratio(self.routed_correct + self.abstain_correct, self.items)
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
/// One prepared-query execution requested by `ask --execute`.
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
/// The Router owns the deployment inference canister and tokenizer (ADR 0113), so the CLI only
/// needs the Router principal and graph name.
fn model_routing_client(
    args: &AskArgs,
) -> Result<Option<gleaph_sdk::routing::RoutingClient>, AskError> {
    let Some(graph) = &args.graph else {
        return Ok(None);
    };
    let canister = args.canister.as_deref().ok_or_else(|| {
        AskError::Inference("--graph requires --canister <ROUTER_PRINCIPAL>".into())
    })?;
    let network = args.network.as_deref().unwrap_or("local");
    let remote = crate::remote::RemoteTransport::connect(
        canister,
        network,
        args.identity.as_deref(),
        args.fetch_root_key,
        None,
    )
    .map_err(AskError::Inference)?;
    let router = candid::Principal::from_text(canister)
        .map_err(|error| AskError::Inference(format!("invalid Router canister: {error}")))?;
    let transport = gleaph_sdk::routing::RouterTransport::new(remote.agent(), router, graph);
    Ok(Some(gleaph_sdk::routing::RoutingClient::new_text_only(
        Box::new(transport),
    )))
}

pub fn execute(args: &AskArgs, dir: &Path) -> Result<Option<ExecutionRequest>, AskError> {
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
    if model_client.is_some() && (args.eval.is_some() || args.slot_eval.is_some()) {
        return Err(AskError::Inference(
            "model routing supports --plan/--execute, not --eval/--slot-eval".into(),
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
    print_routing(question, &routing, args.json);
    if args.plan {
        let entities_path = args.entities.as_ref().ok_or_else(|| {
            AskError::Slots("--plan requires --entities <vocabulary.json>".into())
        })?;
        let vocab = slots::EntityVocab::load(entities_path)
            .map_err(|error| AskError::Slots(error.to_string()))?;
        match &model_client {
            Some(client) => {
                let started = std::time::Instant::now();
                let extraction = client
                    .route_text(question)
                    .map_err(|error| AskError::Inference(error.to_string()))?;
                let elapsed = started.elapsed();
                let decision = client.decide_text(&extraction);
                routing_log.record_text(question, &extraction, decision, elapsed);
                match decision {
                    None => println!("plan: abstain"),
                    Some(operation) => match client.bind_text_slots_with_fallback(
                        question,
                        &extraction,
                        operation,
                        &vocab,
                    ) {
                        Ok(bindings) => println!("plan: {operation} {bindings}"),
                        Err(error) => {
                            println!("plan: {operation} (slots unavailable: {error})")
                        }
                    },
                }
            }
            None => match routing.best() {
                None => println!("plan: abstain"),
                Some(best) => match slots::extract(question, &best.name, &vocab) {
                    Ok(bindings) => println!("plan: {} {}", best.name, bindings),
                    Err(error) => println!("plan: {} (slots unavailable: {error})", best.name),
                },
            },
        }
    }
    if args.execute {
        let entities_path = args.entities.as_ref().ok_or_else(|| {
            AskError::Slots("--execute requires --entities <vocabulary.json>".into())
        })?;
        let vocab = slots::EntityVocab::load(entities_path)
            .map_err(|error| AskError::Slots(error.to_string()))?;
        if let Some(client) = &model_client {
            let started = std::time::Instant::now();
            let extraction = client
                .route_text(question)
                .map_err(|error| AskError::Inference(error.to_string()))?;
            let elapsed = started.elapsed();
            let decision = client.decide_text(&extraction);
            routing_log.record_text(question, &extraction, decision, elapsed);
            let operation = match decision {
                Some(operation) if operation != ABSTAIN_OPTION => operation,
                _ => {
                    println!("execute: abstained (no prepared query)");
                    return Ok(None);
                }
            };
            if !candidates
                .iter()
                .any(|candidate| candidate.name == operation)
            {
                println!("execute: abstained (operation {operation} is not in the catalog)");
                return Ok(None);
            }
            let bindings = match client.bind_text_slots_with_fallback(
                question,
                &extraction,
                operation,
                &vocab,
            ) {
                Ok(bindings) => bindings,
                Err(error) => {
                    println!("abstain: slots unavailable ({error})");
                    return Ok(None);
                }
            };
            let (name, params) = execution_plan(operation, &bindings, Some(&vocab))?;
            println!("execute: {name} {bindings}");
            return Ok(Some(ExecutionRequest { name, params }));
        }
        return match routing.best() {
            None => {
                println!("execute: abstained (no prepared query)");
                Ok(None)
            }
            Some(best) => {
                // A routed operation whose slots cannot be bound is not an answer: abstain rather
                // than error, so out-of-scope questions that merely resemble a catalog entry
                // (no known entity/number) stay fail-closed at the routing layer.
                let bindings = match slots::extract(question, &best.name, &vocab) {
                    Ok(bindings) => bindings,
                    Err(error) => {
                        println!("abstain: slots unavailable ({error})");
                        return Ok(None);
                    }
                };
                let (name, params) = execution_plan(&best.name, &bindings, Some(&vocab))?;
                println!("execute: {name} {bindings}");
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
        report.routed_correct + report.abstain_correct,
        report.items,
        report.overall_accuracy() * 100.0
    );
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
                lang: Some("en".into()),
                family: None,
            },
            GoldenItem {
                question: "weather forecast".into(),
                expected_op: None,
                expect_abstain: true,
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
}
