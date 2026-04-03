//! Flow-sensitive type narrowing from WHERE predicates.

use crate::ast::{Expr, ExprKind};

use super::env::TypeEnv;
use super::types::Type;

/// A narrowing fact extracted from a WHERE predicate.
#[derive(Clone, Debug)]
pub(crate) enum NarrowingFact {
    /// `var.property IS NOT NULL` → property is non-null.
    PropertyNonNull { var: String, property: String },
    /// `var IS LABELED :Label` → node has additional label (AND).
    LabelNarrowed { var: String, label: String },
    /// OR of label narrowings: `var IS LABELED :A OR var IS LABELED :B`
    /// → node may be any of the listed label sets.
    OrLabelNarrowed {
        var: String,
        label_sets: Vec<Vec<String>>,
    },
    /// `type(e) = 'LABEL'` → edge has specific label (Cypher extension).
    #[cfg(feature = "cypher")]
    EdgeLabelNarrowed { var: String, label: String },
}

/// Extract narrowing facts from a WHERE expression.
///
/// AND-connected conjunctions produce facts directly.
/// OR branches are merged: for label narrowing on the same variable,
/// all alternative labels are collected into an `OrLabelNarrowed` fact.
pub(crate) fn extract_narrowing_facts(expr: &Expr) -> Vec<NarrowingFact> {
    let mut facts = Vec::new();
    collect_facts(&expr.kind, &mut facts);
    facts
}

fn collect_facts(kind: &ExprKind, facts: &mut Vec<NarrowingFact>) {
    match kind {
        // Paren → unwrap.
        ExprKind::Paren(inner) => {
            collect_facts(&inner.kind, facts);
        }
        // AND → recurse into both sides.
        ExprKind::And(left, right) => {
            collect_facts(&left.kind, facts);
            collect_facts(&right.kind, facts);
        }
        // OR → extract facts from each branch and merge label narrowings.
        ExprKind::Or(left, right) => {
            let mut left_facts = Vec::new();
            let mut right_facts = Vec::new();
            collect_facts(&left.kind, &mut left_facts);
            collect_facts(&right.kind, &mut right_facts);
            merge_or_label_facts(&left_facts, &right_facts, facts);
        }
        // var.prop IS NOT NULL → PropertyNonNull.
        ExprKind::IsNotNull(inner) => {
            if let ExprKind::PropertyAccess { expr, property } = &inner.kind
                && let ExprKind::Variable(var) = &expr.kind
            {
                facts.push(NarrowingFact::PropertyNonNull {
                    var: var.clone(),
                    property: property.clone(),
                });
            }
        }
        // var IS LABELED :Label → LabelNarrowed.
        ExprKind::IsLabeled {
            expr,
            label,
            negated: false,
        } => {
            if let ExprKind::Variable(var) = &expr.kind
                && let crate::types::LabelExpr::Name(name) = label
            {
                facts.push(NarrowingFact::LabelNarrowed {
                    var: var.clone(),
                    label: name.clone(),
                });
            }
        }
        // type(e) = 'LABEL' → EdgeLabelNarrowed (Cypher extension).
        // Handles: FunctionCall("type", [var]) = Literal(Text("LABEL"))
        #[cfg(feature = "cypher")]
        ExprKind::Compare {
            left,
            op: crate::ast::CmpOp::Eq,
            right,
        } => {
            // Check both orientations: type(e) = 'X' and 'X' = type(e)
            try_extract_type_eq(left, right, facts);
            try_extract_type_eq(right, left, facts);
        }
        _ => {}
    }
}

/// Merge label-narrowing facts from two OR branches.
///
/// If both branches narrow the same variable to labels, produce an `OrLabelNarrowed`
/// with all alternative label sets. Only label facts are mergeable; other facts
/// (NonNull, EdgeLabel) require ALL branches to agree (intersection), so they're dropped
/// unless both sides have the same fact.
fn merge_or_label_facts(
    left: &[NarrowingFact],
    right: &[NarrowingFact],
    out: &mut Vec<NarrowingFact>,
) {
    // Collect label sets per variable from each branch.
    let left_labels = collect_label_sets(left);
    let right_labels = collect_label_sets(right);

    // For each variable that appears in BOTH branches, emit OrLabelNarrowed.
    for (var, left_sets) in &left_labels {
        if let Some(right_sets) = right_labels.get(var.as_str()) {
            let mut combined = left_sets.clone();
            for rs in right_sets {
                if !combined.contains(rs) {
                    combined.push(rs.clone());
                }
            }
            out.push(NarrowingFact::OrLabelNarrowed {
                var: var.clone(),
                label_sets: combined,
            });
        }
    }

    // NonNull facts: only emit if present in BOTH branches (intersection).
    for lf in left {
        if let NarrowingFact::PropertyNonNull { var, property } = lf
            && right.iter().any(|rf| matches!(rf, NarrowingFact::PropertyNonNull { var: rv, property: rp } if rv == var && rp == property)) {
                out.push(lf.clone());
            }
    }
}

/// Collect label sets per variable from a list of narrowing facts.
fn collect_label_sets(
    facts: &[NarrowingFact],
) -> std::collections::HashMap<String, Vec<Vec<String>>> {
    let mut map: std::collections::HashMap<String, Vec<Vec<String>>> =
        std::collections::HashMap::new();
    for fact in facts {
        match fact {
            NarrowingFact::LabelNarrowed { var, label } => {
                let entry = map.entry(var.clone()).or_default();
                // Each simple LabelNarrowed becomes a single-label set.
                entry.push(vec![label.clone()]);
            }
            NarrowingFact::OrLabelNarrowed { var, label_sets } => {
                let entry = map.entry(var.clone()).or_default();
                entry.extend(label_sets.clone());
            }
            _ => {}
        }
    }
    map
}

#[cfg(feature = "cypher")]
fn try_extract_type_eq(func_side: &Expr, literal_side: &Expr, facts: &mut Vec<NarrowingFact>) {
    if let ExprKind::FunctionCall { name, args, .. } = &func_side.kind {
        let fn_name = name.parts.first().map(|s| s.to_ascii_lowercase());
        if fn_name.as_deref() == Some("type")
            && args.len() == 1
            && let ExprKind::Variable(var) = &args[0].kind
            && let ExprKind::Literal(crate::Value::Text(label)) = &literal_side.kind
        {
            facts.push(NarrowingFact::EdgeLabelNarrowed {
                var: var.clone(),
                label: label.clone(),
            });
        }
    }
}

/// Apply extracted narrowing facts to the type environment.
pub(crate) fn apply_narrowing(env: &mut TypeEnv<'_>, facts: &[NarrowingFact]) {
    for fact in facts {
        match fact {
            NarrowingFact::PropertyNonNull { var, property } => {
                env.narrowed_nonnull.insert((var.clone(), property.clone()));
            }
            NarrowingFact::LabelNarrowed { var, label } => {
                env.narrowed_labels
                    .entry(var.clone())
                    .or_default()
                    .push(label.clone());
                if let Some(Type::Node(info)) = env.bindings.get_mut(var) {
                    // Add label to the primary (first) label set, or create one.
                    if info.label_sets.is_empty() {
                        info.label_sets.push(vec![label.clone()]);
                    } else if !info.label_sets[0].contains(label) {
                        info.label_sets[0].push(label.clone());
                    }
                    // Refresh properties from schema with the updated label set.
                    let refreshed = env.schema.node_property_types(&info.label_sets[0]);
                    if !refreshed.is_empty() {
                        info.properties = refreshed;
                    }
                }
            }
            NarrowingFact::OrLabelNarrowed { var, label_sets } => {
                if let Some(Type::Node(info)) = env.bindings.get_mut(var) {
                    // Replace label_sets with the OR alternatives.
                    info.label_sets = label_sets.clone();
                    // Clear cached properties — they'll be resolved per-set in lookup.
                    info.properties = Vec::new();
                }
            }
            #[cfg(feature = "cypher")]
            NarrowingFact::EdgeLabelNarrowed { var, label } => {
                env.narrowed_edge_labels.insert(var.clone(), label.clone());
                if let Some(Type::Edge(info)) = env.bindings.get_mut(var)
                    && info.label.is_none()
                {
                    info.label = Some(label.clone());
                    // Also populate properties from schema now that the label is known.
                    if info.properties.is_empty() {
                        info.properties = env.schema.edge_property_types(label);
                    }
                    // Also populate endpoint constraints.
                    if info.endpoints.is_empty() {
                        info.endpoints = env.schema.edge_endpoint_types(label);
                    }
                }
            }
        }
    }
}
