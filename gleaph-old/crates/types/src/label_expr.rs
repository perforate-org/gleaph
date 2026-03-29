use candid::CandidType;
use serde::{Deserialize, Serialize};

/// A label expression used in node and edge patterns.
///
/// Supports AND (`&`), OR (`|`), NOT (`!`), wildcard (`%`), and plain names.
#[derive(Clone, Debug, PartialEq, CandidType, Deserialize, Serialize)]
pub enum LabelExpr {
    /// A single label name, e.g. `:Person`
    Name(String),
    /// Wildcard `%` — matches any entity that has at least one label
    Wildcard,
    /// AND expression `A&B` — entity must have both labels
    And(Box<LabelExpr>, Box<LabelExpr>),
    /// OR expression `A|B` — entity must have at least one of the labels
    Or(Box<LabelExpr>, Box<LabelExpr>),
    /// NOT expression `!A` — entity must not have the label
    Not(Box<LabelExpr>),
}

/// Evaluate a label expression against a single edge label string.
///
/// An edge has exactly one label (or none). For `And`, both sides must accept
/// that same label. `Wildcard` accepts any `Some(_)` label.
pub fn matches_edge_label(expr: &LabelExpr, edge_label: Option<&str>) -> bool {
    match expr {
        LabelExpr::Name(name) => edge_label.is_some_and(|l| l == name),
        LabelExpr::Wildcard => edge_label.is_some(),
        LabelExpr::And(a, b) => {
            matches_edge_label(a, edge_label) && matches_edge_label(b, edge_label)
        }
        LabelExpr::Or(a, b) => {
            matches_edge_label(a, edge_label) || matches_edge_label(b, edge_label)
        }
        LabelExpr::Not(e) => !matches_edge_label(e, edge_label),
    }
}
