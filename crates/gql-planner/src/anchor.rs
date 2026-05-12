//! Anchor selection logic for the GQL planner.
//!
//! The *anchor* is the variable chosen as the starting point for a scan.
//! Good anchor selection is critical for performance: starting from a
//! highly selective scan (e.g. an indexed property equality) dramatically
//! reduces the rows that flow through subsequent operators.
//!
//! Priority order (ported from gleaph-old):
//! 1. Property equality on an indexed vertex property
//! 2. Property range on a range-indexed vertex property
//! 3. Inline property equality from the pattern
//! 4. Lowest-cardinality label
//! 5. Full scan (fallback)

use gleaph_gql::ast::*;
use gleaph_gql::types::LabelExpr;
use gleaph_graph_kernel::index::value_to_index_key_bytes;

use crate::plan::{AnchorInfo, AnchorSource, ScanValue};
use crate::stats::{self, GraphStats};

/// Choose the best anchor variable for a match statement.
///
/// Examines the graph pattern's node patterns and the WHERE clause to find
/// the variable with the most selective access path.
pub fn choose_anchor(pattern: &GraphPattern, stats: Option<&dyn GraphStats>) -> Option<AnchorInfo> {
    let mut candidates: Vec<AnchorCandidate> = Vec::new();

    // Collect node variables with their labels and inline properties.
    for path in &pattern.paths {
        collect_node_candidates(&path.expr, &mut candidates);
    }

    if candidates.is_empty() {
        return None;
    }

    // Phase 1: Check WHERE clause for property equalities on indexed properties.
    if let Some(where_expr) = &pattern.where_clause {
        if let Some(anchor) = find_equality_anchor(&candidates, where_expr, stats) {
            return Some(anchor);
        }
        if let Some(anchor) = find_range_anchor(&candidates, where_expr, stats) {
            return Some(anchor);
        }
    }

    // Phase 2: Check inline property equalities from the pattern itself.
    for candidate in &candidates {
        if !candidate.inline_properties.is_empty() {
            if let Some(stats) = stats {
                // Prefer indexed properties.
                for prop in &candidate.inline_properties {
                    if stats.is_vertex_property_indexed(prop) {
                        return Some(AnchorInfo {
                            variable: candidate.variable.clone().into(),
                            source: AnchorSource::InlinePropertyEquality {
                                property: prop.clone().into(),
                            },
                        });
                    }
                }
            } else {
                // Without stats, still use the first inline property as anchor hint.
                return Some(AnchorInfo {
                    variable: candidate.variable.clone().into(),
                    source: AnchorSource::InlinePropertyEquality {
                        property: candidate.inline_properties[0].clone().into(),
                    },
                });
            }
        }
    }

    // Phase 2b: Check inline WHERE from node patterns.
    for candidate in &candidates {
        if !candidate.inline_where_properties.is_empty() {
            if let Some(stats) = stats {
                for prop in &candidate.inline_where_properties {
                    if stats.is_vertex_property_indexed(prop) {
                        return Some(AnchorInfo {
                            variable: candidate.variable.clone().into(),
                            source: AnchorSource::InlinePropertyEquality {
                                property: prop.clone().into(),
                            },
                        });
                    }
                }
            } else {
                return Some(AnchorInfo {
                    variable: candidate.variable.clone().into(),
                    source: AnchorSource::InlinePropertyEquality {
                        property: candidate.inline_where_properties[0].clone().into(),
                    },
                });
            }
        }
    }

    // Phase 3: Pick lowest-cardinality label (including schema-inferred labels).
    if let Some(stats) = stats {
        // Phase 3a: Try to infer labels for unlabeled nodes via edge endpoint schema.
        let mut inferred_labels: Vec<(String, String)> = Vec::new(); // (variable, label)
        for candidate in &candidates {
            if candidate.label.is_none() {
                // Look for edges connected to this variable in the pattern.
                for path in &pattern.paths {
                    if let Some(label) =
                        infer_label_from_edges(&candidate.variable, &path.expr, stats)
                    {
                        inferred_labels.push((candidate.variable.clone(), label));
                        break;
                    }
                }
            }
        }

        let mut best: Option<(String, String, u64)> = None;
        for candidate in &candidates {
            let label = candidate.label.clone().or_else(|| {
                inferred_labels
                    .iter()
                    .find(|(v, _)| v == &candidate.variable)
                    .map(|(_, l)| l.clone())
            });
            if let Some(label) = label
                && let Some(card) = stats::label_cardinality_with_id(stats, &label)
                && best
                    .as_ref()
                    .is_none_or(|(_, _, best_card)| card < *best_card)
            {
                best = Some((candidate.variable.clone(), label.clone(), card));
            }
        }
        if let Some((var, label, _)) = best {
            let source = if candidates
                .iter()
                .any(|c| c.variable == var && c.label.is_some())
            {
                AnchorSource::LabelCardinality {
                    label: label.into(),
                }
            } else {
                AnchorSource::SchemaEndpoint
            };
            return Some(AnchorInfo {
                variable: var.into(),
                source,
            });
        }
    }

    // Phase 4: Pick the first labeled node, or the first node at all.
    let labeled = candidates.iter().find(|c| c.label.is_some());
    let chosen = labeled.unwrap_or(&candidates[0]);
    Some(AnchorInfo {
        variable: chosen.variable.clone().into(),
        source: AnchorSource::FullScan,
    })
}

// ════════════════════════════════════════════════════════════════════════════════
// Internal helpers
// ════════════════════════════════════════════════════════════════════════════════

struct AnchorCandidate {
    variable: String,
    label: Option<String>,
    inline_properties: Vec<String>,
    inline_where_properties: Vec<String>,
}

/// Walk a path pattern expression to collect node variables.
fn collect_node_candidates(expr: &PathPatternExpr, out: &mut Vec<AnchorCandidate>) {
    match expr {
        PathPatternExpr::Term(term) => collect_from_term(term, out),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                collect_from_term(term, out);
            }
        }
    }
}

fn collect_from_term(term: &PathTerm, out: &mut Vec<AnchorCandidate>) {
    for factor in &term.factors {
        collect_from_primary(&factor.primary, out);
    }
}

fn collect_from_primary(primary: &PathPrimary, out: &mut Vec<AnchorCandidate>) {
    match primary {
        PathPrimary::Node(node) => {
            if let Some(var) = &node.variable {
                let label = extract_simple_label(&node.label);
                let inline_properties = node.properties.iter().map(|p| p.name.clone()).collect();
                // Extract equality properties from inline WHERE clause.
                let inline_where_properties = node
                    .where_clause
                    .as_ref()
                    .map(|w| extract_where_equality_props(var, w))
                    .unwrap_or_default();
                out.push(AnchorCandidate {
                    variable: var.clone(),
                    label,
                    inline_properties,
                    inline_where_properties,
                });
            }
        }
        PathPrimary::Parenthesized { expr, .. } => {
            collect_node_candidates(expr, out);
        }
        PathPrimary::Edge(_) | PathPrimary::Simplified(_) => {}
    }
}

/// Extract property names from inline WHERE equalities: `(n WHERE n.prop = value)`.
fn extract_where_equality_props(var: &str, expr: &Expr) -> Vec<String> {
    let mut props = Vec::new();
    let conjuncts = flatten_conjunction(expr);
    for conjunct in conjuncts {
        if let ExprKind::Compare { left, op, right } = &conjunct.kind
            && *op == CmpOp::Eq
            && let Some((v, p)) = extract_property_access(left)
            && v == var
            && is_scannable_value(right)
        {
            props.push(p);
        }
    }
    props
}

/// Extract a simple label name from a LabelExpr (only handles single labels).
pub fn extract_simple_label(label: &Option<LabelExpr>) -> Option<String> {
    match label {
        Some(LabelExpr::Name(name)) => Some(name.clone()),
        _ => None,
    }
}

/// Look for `var.property = <literal>` or `var.property = $param` in a WHERE
/// clause, where the property is indexed.
fn find_equality_anchor(
    candidates: &[AnchorCandidate],
    where_expr: &Expr,
    stats: Option<&dyn GraphStats>,
) -> Option<AnchorInfo> {
    let predicates = flatten_conjunction(where_expr);
    for pred in &predicates {
        if let Some((var, prop)) = extract_equality_predicate(pred) {
            // Check if this variable is one of our candidates.
            if candidates.iter().any(|c| c.variable == var) {
                if let Some(stats) = stats {
                    if stats.is_vertex_property_indexed(&prop) {
                        return Some(AnchorInfo {
                            variable: var.into(),
                            source: AnchorSource::PropertyEquality {
                                property: prop.into(),
                            },
                        });
                    }
                } else {
                    // Without stats, assume the first equality predicate is a good anchor.
                    return Some(AnchorInfo {
                        variable: var.into(),
                        source: AnchorSource::PropertyEquality {
                            property: prop.into(),
                        },
                    });
                }
            }
        }
    }
    None
}

/// Look for range predicates (`<`, `<=`, `>`, `>=`) on range-indexed properties.
fn find_range_anchor(
    candidates: &[AnchorCandidate],
    where_expr: &Expr,
    stats: Option<&dyn GraphStats>,
) -> Option<AnchorInfo> {
    let predicates = flatten_conjunction(where_expr);
    for pred in &predicates {
        if let Some((var, prop, value, cmp)) = extract_range_predicate(pred)
            && candidates.iter().any(|c| c.variable == var)
            && let Some(stats) = stats
            && stats.is_vertex_property_range_indexed(&prop)
        {
            return Some(AnchorInfo {
                variable: var.into(),
                source: AnchorSource::PropertyRange {
                    property: prop.into(),
                    value,
                    cmp,
                },
            });
        }
    }
    None
}

/// Flatten an AND chain into individual predicates.
fn flatten_conjunction(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::And(left, right) => {
            let mut result = flatten_conjunction(left);
            result.extend(flatten_conjunction(right));
            result
        }
        _ => vec![expr],
    }
}

/// Extract (variable, property) from `var.prop = <value>`.
fn extract_equality_predicate(expr: &Expr) -> Option<(String, String)> {
    if let ExprKind::Compare { left, op, right } = &expr.kind {
        if *op != CmpOp::Eq {
            return None;
        }
        // Check left side: var.prop
        if let Some((var, prop)) = extract_property_access(left)
            && is_scannable_value(right)
        {
            return Some((var, prop));
        }
        // Check right side (reversed): <value> = var.prop
        if let Some((var, prop)) = extract_property_access(right)
            && is_scannable_value(left)
        {
            return Some((var, prop));
        }
    }
    None
}

fn reverse_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        other => other,
    }
}

fn range_scan_value_from_expr(expr: &Expr) -> Option<ScanValue> {
    match &expr.kind {
        ExprKind::Literal(value) => value_to_index_key_bytes(value)
            .ok()
            .flatten()
            .map(|_| ScanValue::Literal(value.clone())),
        ExprKind::Parameter(parameter) => Some(ScanValue::Parameter(parameter.clone().into())),
        _ => None,
    }
}

/// Extract (variable, property, bound value, cmp) from range predicates (`<`, `<=`, `>`, `>=`).
fn extract_range_predicate(expr: &Expr) -> Option<(String, String, ScanValue, CmpOp)> {
    if let ExprKind::Compare { left, op, right } = &expr.kind {
        match op {
            CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {}
            _ => return None,
        }
        if let Some((var, prop)) = extract_property_access(left)
            && let Some(value) = range_scan_value_from_expr(right)
        {
            return Some((var, prop, value, *op));
        }
        if let Some((var, prop)) = extract_property_access(right)
            && let Some(value) = range_scan_value_from_expr(left)
        {
            return Some((var, prop, value, reverse_cmp(*op)));
        }
    }
    None
}

/// Extract (variable_name, property_name) from a PropertyAccess expression.
fn extract_property_access(expr: &Expr) -> Option<(String, String)> {
    if let ExprKind::PropertyAccess {
        expr: inner,
        property,
    } = &expr.kind
        && let ExprKind::Variable(var) = &inner.kind
    {
        return Some((var.clone(), property.clone()));
    }
    None
}

/// Check if a value expression is something we can use in an index scan.
/// Check if a variable has multiple indexed equality predicates in the WHERE clause,
/// which would benefit from an IndexIntersection scan.
pub fn find_index_intersection(
    variable: &str,
    where_expr: &Expr,
    stats: &dyn GraphStats,
) -> Option<Vec<crate::plan::IndexScanSpec>> {
    let predicates = flatten_conjunction(where_expr);
    let mut specs = Vec::new();

    for pred in &predicates {
        if let Some((var, prop)) = extract_equality_predicate(pred)
            && var == variable
            && stats.is_vertex_property_indexed(&prop)
        {
            // Extract the value from the predicate.
            if let ExprKind::Compare { right, .. } = &pred.kind
                && let Some(sv) = scan_value_from_expr(right)
            {
                specs.push(crate::plan::IndexScanSpec {
                    property: prop.into(),
                    value: sv,
                    cmp: CmpOp::Eq,
                });
            }
        }
    }

    if specs.len() >= 2 { Some(specs) } else { None }
}

/// Convert an expression to a [`ScanValue`] if it's a literal or parameter.
pub(crate) fn scan_value_from_expr(expr: &Expr) -> Option<crate::plan::ScanValue> {
    match &expr.kind {
        ExprKind::Literal(v) => Some(crate::plan::ScanValue::Literal(v.clone())),
        ExprKind::Parameter(p) => Some(crate::plan::ScanValue::Parameter(p.clone().into())),
        _ => None,
    }
}

fn is_scannable_value(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Literal(_) | ExprKind::Parameter(_))
}

/// Infer a node label from connected edges using schema endpoint information.
fn infer_label_from_edges(
    node_var: &str,
    path_expr: &PathPatternExpr,
    stats: &dyn GraphStats,
) -> Option<String> {
    match path_expr {
        PathPatternExpr::Term(term) => infer_from_term(node_var, term, stats),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => terms
            .iter()
            .find_map(|t| infer_from_term(node_var, t, stats)),
    }
}

fn infer_from_term(node_var: &str, term: &PathTerm, stats: &dyn GraphStats) -> Option<String> {
    // Walk factors looking for edge-node pairs where the node matches our variable.
    for (i, factor) in term.factors.iter().enumerate() {
        if let PathPrimary::Edge(edge) = &factor.primary {
            let edge_label = extract_simple_label(&edge.label)?;
            let (src_labels, dst_labels) = stats.edge_endpoint_labels(&edge_label)?;

            // Check if the NEXT node is our target (node is destination).
            if let Some(next) = term.factors.get(i + 1)
                && let PathPrimary::Node(node) = &next.primary
                && node.variable.as_deref() == Some(node_var)
                && node.label.is_none()
            {
                // Pick the lowest-cardinality destination label.
                return dst_labels
                    .iter()
                    .filter_map(|l| stats.label_cardinality(l).map(|c| (l.clone(), c)))
                    .min_by_key(|(_, c)| *c)
                    .map(|(l, _)| l);
            }

            // Check if the PREVIOUS node is our target (node is source).
            if i > 0
                && let Some(prev) = term.factors.get(i - 1)
                && let PathPrimary::Node(node) = &prev.primary
                && node.variable.as_deref() == Some(node_var)
                && node.label.is_none()
            {
                return src_labels
                    .iter()
                    .filter_map(|l| stats.label_cardinality(l).map(|c| (l.clone(), c)))
                    .min_by_key(|(_, c)| *c)
                    .map(|(l, _)| l);
            }
        }
    }
    None
}
