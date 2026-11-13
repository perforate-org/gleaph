//! Public natural-language answer contract.
//!
//! `interpret` returns what the Router understood from a question. `ask` additionally executes a
//! read-only prepared query and returns rows. Internal probabilities, thresholds, fingerprints and
//! model versions do not appear here; they live on the controller-only debug surface.

use candid::{CandidType, Deserialize};

/// Terminal state of an [`Interpretation`] or [`Answer`].
#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskStatus {
    /// A prepared operation and all required parameters were resolved.
    Answered,
    /// An operation was resolved but required parameters are missing; the caller must clarify.
    NeedsInput,
    /// The graph's prepared catalog cannot answer this question.
    Unsupported,
    /// Interpretation succeeded but execution failed.
    Failed,
}

/// State of an [`Interpretation`]; the failure state belongs to [`Answer`] only.
#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpretationStatus {
    /// A prepared operation and all required parameters were resolved.
    Answered,
    /// An operation was resolved but required parameters are missing.
    NeedsInput,
    /// The graph's prepared catalog cannot answer this question.
    Unsupported,
}

/// One typed parameter resolved by interpretation.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AskParameterValue {
    /// A text value.
    Text(String),
    /// A signed integer value.
    Int(i64),
}

/// One typed parameter resolved by interpretation.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AskParameter {
    /// Parameter name from the prepared operation.
    pub name: String,
    /// Resolved value.
    pub value: AskParameterValue,
}

/// What the Router understood from one question, without execution.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Interpretation {
    /// Whether the question can be executed, needs clarification, or is unsupported.
    pub status: InterpretationStatus,
    /// Prepared operation chosen for the question, when one was chosen.
    pub operation: Option<String>,
    /// Resolved parameters.
    pub parameters: Vec<AskParameter>,
    /// Required parameters that are still missing.
    pub missing: Vec<String>,
}

/// Execution failure for an otherwise valid interpretation.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AskFailure {
    /// Stable failure class.
    pub kind: AskFailureKind,
    /// Human-readable detail.
    pub message: String,
}

/// Stable failure class for [`Answer::failure`].
#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskFailureKind {
    /// The caller may not execute the selected prepared operation.
    Authorization,
    /// The prepared operation is no longer present or its schema changed.
    Catalog,
    /// The graph or index rejected the execution.
    Execution,
}

/// Materialized rows for an answered question.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AnswerRows {
    /// Number of rows returned.
    pub row_count: u64,
    /// Compact Candid rows blob, when rows were materialized.
    pub rows_blob: Option<Vec<u8>>,
}

/// A natural-language question and, when possible, its answer rows.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Terminal state of the request.
    pub status: AskStatus,
    /// Interpretation of the question.
    pub interpretation: Interpretation,
    /// Rows when `status` is [`AskStatus::Answered`].
    pub rows: Option<AnswerRows>,
    /// Failure when `status` is [`AskStatus::Failed`].
    pub failure: Option<AskFailure>,
}
