//! Extract variable bindings from MATCH graph patterns.

use crate::ast::*;
use crate::token::Span;
use crate::types::{EdgeDirection, LabelExpr};

use super::diagnostics::{DML005_INSERT_EDGE_DIRECTION, DML006_MATCH_EDGE_DIRECTION};
use super::env::{TypeEnv, WarningKind, WarningProvenance};
use super::types::{EdgeTypeInfo, NodeTypeInfo, PathTypeInfo, Type};

/// Extract bindings from a `GraphPattern` and insert them into `env`.
pub(crate) fn build_env_from_graph_pattern(
    env: &mut TypeEnv<'_>,
    gp: &GraphPattern,
    optional: bool,
) {
    for path in &gp.paths {
        if let Some(ref var) = path.variable {
            let info = path_type_info_from_prefix(path);
            if optional {
                env.optional_vars.insert(var.clone());
            }
            env.bind(var.clone(), Type::Path(info));
        }
        extract_from_path_expr(env, &path.expr, optional, false);
    }
    // After all bindings are established, check endpoint constraints.
    for path in &gp.paths {
        check_endpoint_constraints_in_path_expr(env, &path.expr);
    }
}

fn path_type_info_from_prefix(_path: &PathPattern) -> PathTypeInfo {
    PathTypeInfo::unbounded()
}

fn extract_from_path_expr(
    env: &mut TypeEnv<'_>,
    expr: &PathPatternExpr,
    optional: bool,
    group: bool,
) {
    match expr {
        PathPatternExpr::Term(term) => extract_from_path_term(env, term, optional, group),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                extract_from_path_term(env, term, optional, group);
            }
        }
    }
}

fn extract_from_path_term(env: &mut TypeEnv<'_>, term: &PathTerm, optional: bool, group: bool) {
    for factor in &term.factors {
        extract_from_path_primary(env, &factor.primary, optional, &factor.quantifier, group);
    }
}

fn extract_from_path_primary(
    env: &mut TypeEnv<'_>,
    primary: &PathPrimary,
    optional: bool,
    quantifier: &Option<PathQuantifier>,
    group: bool,
) {
    match primary {
        PathPrimary::Node(node) => bind_node(env, node, optional, group),
        PathPrimary::Edge(edge) => bind_edge(env, edge, optional, quantifier, group),
        PathPrimary::Parenthesized { expr, variable, .. } => {
            let is_quantified = quantifier.is_some();
            if let Some(var) = variable {
                if optional {
                    env.optional_vars.insert(var.clone());
                }
                env.bind(var.clone(), Type::Path(PathTypeInfo::unbounded()));
            }
            // Variables inside a quantified parenthesized pattern become group variables (List).
            extract_from_path_expr(env, expr, optional, group || is_quantified);
        }
        PathPrimary::Simplified(_) => {
            // Simplified patterns have no variable bindings.
        }
    }
}

fn bind_node(env: &mut TypeEnv<'_>, node: &NodePattern, optional: bool, group: bool) {
    let Some(ref var) = node.variable else {
        return;
    };
    if optional {
        env.optional_vars.insert(var.clone());
    }
    let labels = extract_label_names(&node.label);
    let properties = if !labels.is_empty() {
        env.schema.node_property_types(&labels)
    } else {
        Vec::new()
    };
    let label_sets = if labels.is_empty() {
        Vec::new()
    } else {
        vec![labels]
    };
    let ty = Type::Node(NodeTypeInfo {
        label_sets,
        properties,
    });
    env.bind(
        var.clone(),
        if group {
            Type::TypedList(Box::new(ty))
        } else {
            ty
        },
    );
}

fn bind_edge(
    env: &mut TypeEnv<'_>,
    edge: &EdgePattern,
    optional: bool,
    quantifier: &Option<PathQuantifier>,
    group: bool,
) {
    let Some(ref var) = edge.variable else {
        return;
    };
    if optional {
        env.optional_vars.insert(var.clone());
    }

    // If the edge has a quantifier (variable-length path), bind as Path type.
    if quantifier.is_some() {
        let info = match quantifier {
            Some(PathQuantifier::Fixed(n)) => PathTypeInfo {
                min_hops: Some(*n as u32),
                max_hops: Some(*n as u32),
            },
            Some(PathQuantifier::Range { lower, upper }) => PathTypeInfo {
                min_hops: Some(*lower as u32),
                max_hops: upper.map(|u| u as u32),
            },
            Some(PathQuantifier::Star) => PathTypeInfo {
                min_hops: Some(0),
                max_hops: None,
            },
            Some(PathQuantifier::Plus) => PathTypeInfo {
                min_hops: Some(1),
                max_hops: None,
            },
            Some(PathQuantifier::Optional) => PathTypeInfo {
                min_hops: Some(0),
                max_hops: Some(1),
            },
            None => PathTypeInfo::unbounded(),
        };
        env.bind(var.clone(), Type::Path(info));
        return;
    }

    let label = extract_single_label(&edge.label);
    let (endpoints, properties) = if let Some(ref l) = label {
        (
            env.schema.edge_endpoint_types(l),
            env.schema.edge_property_types(l),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let undirected = label
        .as_ref()
        .and_then(|l| env.schema.edge_is_undirected(l));
    let ty = Type::Edge(EdgeTypeInfo {
        label,
        endpoints,
        properties,
        undirected,
    });
    env.bind(
        var.clone(),
        if group {
            Type::TypedList(Box::new(ty))
        } else {
            ty
        },
    );
}

/// Extract concrete label names from a LabelExpr.
/// For simple Name labels and AND combinations, returns the label strings.
/// For complex expressions (OR, NOT, Wildcard), returns empty.
fn extract_label_names(label_expr: &Option<LabelExpr>) -> Vec<String> {
    let Some(expr) = label_expr else {
        return vec![];
    };
    let mut labels = Vec::new();
    collect_and_labels(expr, &mut labels);
    labels
}

/// Recursively collect label names from AND expressions.
fn collect_and_labels(expr: &LabelExpr, out: &mut Vec<String>) {
    match expr {
        LabelExpr::Name(name) => out.push(name.clone()),
        LabelExpr::And(a, b) => {
            collect_and_labels(a, out);
            collect_and_labels(b, out);
        }
        // OR, NOT, Wildcard — cannot extract concrete labels.
        _ => out.clear(),
    }
}

/// Extract a single label name, if the expression is a simple Name.
fn extract_single_label(label_expr: &Option<LabelExpr>) -> Option<String> {
    match label_expr {
        Some(LabelExpr::Name(name)) => Some(name.clone()),
        _ => None,
    }
}

// ── Endpoint constraint checking ──

/// Walk a path expression and check edge endpoint constraints for Node-Edge-Node triples.
fn check_endpoint_constraints_in_path_expr(env: &mut TypeEnv<'_>, expr: &PathPatternExpr) {
    match expr {
        PathPatternExpr::Term(term) => check_endpoint_constraints_in_term(env, term),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                check_endpoint_constraints_in_term(env, term);
            }
        }
    }
}

fn check_endpoint_constraints_in_term(env: &mut TypeEnv<'_>, term: &PathTerm) {
    let factors = &term.factors;
    // Walk triples: factors[i] = Node, factors[i+1] = Edge, factors[i+2] = Node.
    if factors.len() < 3 {
        return;
    }
    for window in factors.windows(3) {
        let (node_a, edge_f, node_b) = (&window[0].primary, &window[1].primary, &window[2].primary);
        let (Some(np_a), Some(ep), Some(np_b)) =
            (as_node(node_a), as_edge(edge_f), as_node(node_b))
        else {
            continue;
        };
        let edge_label = match extract_single_label(&ep.label) {
            Some(l) => l,
            None => continue,
        };
        let constraints = env.schema.edge_endpoint_types(&edge_label);
        if constraints.is_empty() {
            continue; // No schema constraint → open-world, skip.
        }
        let labels_a = extract_label_names(&np_a.label);
        let labels_b = extract_label_names(&np_b.label);
        if labels_a.is_empty() && labels_b.is_empty() {
            continue; // No labels on either endpoint → cannot falsify.
        }
        let direction = &ep.direction;
        if !any_endpoint_satisfies(&constraints, &labels_a, &labels_b, direction) {
            env.warnings.push(super::env::TypeWarning {
                code: None,
                message: format!(
                    "edge `:{edge_label}` cannot connect {from} to {to} (schema constraint violation)",
                    from = format_labels(&labels_a),
                    to = format_labels(&labels_b),
                ),
                kind: WarningKind::ImpossiblePattern,
                span: Some(ep.span),
                provenance: Some(WarningProvenance::EndpointCheck { edge_label }),
            });
        }
    }
    // Recurse into parenthesized sub-patterns.
    for factor in factors {
        if let PathPrimary::Parenthesized { expr, .. } = &factor.primary {
            check_endpoint_constraints_in_path_expr(env, expr);
        }
    }
}

fn as_node(primary: &PathPrimary) -> Option<&NodePattern> {
    match primary {
        PathPrimary::Node(n) => Some(n),
        _ => None,
    }
}

fn as_edge(primary: &PathPrimary) -> Option<&EdgePattern> {
    match primary {
        PathPrimary::Edge(e) => Some(e),
        _ => None,
    }
}

/// Check if any schema endpoint constraint is satisfiable given the pattern labels and direction.
///
/// For directed edges (`->`), `(from_labels, to_labels)` maps to `(a_labels, b_labels)`.
/// For reverse (`<-`), it maps to `(b_labels, a_labels)`.
/// For undirected/any-direction, either orientation is tried.
fn any_endpoint_satisfies(
    constraints: &[(Vec<String>, Vec<String>)],
    labels_a: &[String],
    labels_b: &[String],
    direction: &EdgeDirection,
) -> bool {
    let forward = || {
        constraints.iter().any(|(from, to)| {
            labels_subset_matches(labels_a, from) && labels_subset_matches(labels_b, to)
        })
    };
    let reverse = || {
        constraints.iter().any(|(from, to)| {
            labels_subset_matches(labels_b, from) && labels_subset_matches(labels_a, to)
        })
    };
    match direction {
        EdgeDirection::PointingRight => forward(),
        EdgeDirection::PointingLeft => reverse(),
        // Undirected / bidirectional / any: try both orientations.
        EdgeDirection::Undirected
        | EdgeDirection::LeftOrRight
        | EdgeDirection::AnyDirection
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight => forward() || reverse(),
    }
}

/// Check if the pattern labels are a subset of (or compatible with) the constraint labels.
/// Empty pattern labels means "unconstrained" → always compatible.
/// If the pattern specifies labels, every pattern label must appear in the constraint label set.
fn labels_subset_matches(pattern_labels: &[String], constraint_labels: &[String]) -> bool {
    if pattern_labels.is_empty() {
        return true; // No labels specified → unconstrained, compatible.
    }
    if constraint_labels.is_empty() {
        return true; // Constraint doesn't restrict labels → compatible.
    }
    pattern_labels
        .iter()
        .all(|pl| constraint_labels.iter().any(|cl| cl == pl))
}

fn format_labels(labels: &[String]) -> String {
    if labels.is_empty() {
        "(unlabeled)".to_string()
    } else {
        format!(":{}", labels.join(":"))
    }
}

/// Whether `direction` is forbidden for an edge with this schema directedness.
fn edge_direction_conflicts_schema(schema_undirected: bool, direction: &EdgeDirection) -> bool {
    if schema_undirected {
        matches!(
            direction,
            EdgeDirection::PointingRight | EdgeDirection::PointingLeft
        )
    } else {
        matches!(direction, EdgeDirection::Undirected)
    }
}

fn warn_schema_edge_direction_if_needed(
    env: &mut TypeEnv<'_>,
    label: &str,
    direction: &EdgeDirection,
    span: Span,
    is_insert: bool,
) {
    let Some(schema_undirected) = env.schema.edge_is_undirected(label) else {
        return;
    };
    if !edge_direction_conflicts_schema(schema_undirected, direction) {
        return;
    }
    let ctx = if is_insert { "INSERT" } else { "MATCH pattern" };
    let message = if schema_undirected {
        format!(
            "{ctx}: edge `:{label}` is UNDIRECTED in the graph schema but the pattern uses a directed arrow; use `~[{label}]~` or an incident direction"
        )
    } else {
        format!(
            "{ctx}: edge `:{label}` is DIRECTED in the graph schema but the pattern uses `~[{label}]~`; use `->` or `<-`"
        )
    };
    let code = if is_insert {
        DML005_INSERT_EDGE_DIRECTION
    } else {
        DML006_MATCH_EDGE_DIRECTION
    };
    env.warn_at_with_code(
        WarningKind::SchemaEdgeDirectionMismatch,
        code,
        message,
        span,
    );
}

/// Validate INSERT edge arrows against schema for single-label edges.
pub(crate) fn check_insert_path_schema_edge_direction(
    env: &mut TypeEnv<'_>,
    path: &InsertPathPattern,
) {
    for el in &path.elements {
        if let InsertElement::Edge(e) = el
            && e.labels.len() == 1
        {
            warn_schema_edge_direction_if_needed(env, &e.labels[0], &e.direction, e.span, true);
        }
    }
}

/// Validate MATCH edge directions against schema for single-label edges.
pub(crate) fn check_graph_pattern_schema_edge_direction(env: &mut TypeEnv<'_>, gp: &GraphPattern) {
    for path in &gp.paths {
        check_path_expr_schema_edge_direction(env, &path.expr);
    }
}

fn check_path_expr_schema_edge_direction(env: &mut TypeEnv<'_>, expr: &PathPatternExpr) {
    match expr {
        PathPatternExpr::Term(term) => check_path_term_schema_edge_direction(env, term),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                check_path_term_schema_edge_direction(env, term);
            }
        }
    }
}

fn check_path_term_schema_edge_direction(env: &mut TypeEnv<'_>, term: &PathTerm) {
    for factor in &term.factors {
        if let PathPrimary::Edge(ep) = &factor.primary
            && let Some(l) = extract_single_label(&ep.label)
        {
            warn_schema_edge_direction_if_needed(env, &l, &ep.direction, ep.span, false);
        }
        if let PathPrimary::Parenthesized { expr, .. } = &factor.primary {
            check_path_expr_schema_edge_direction(env, expr);
        }
    }
}
