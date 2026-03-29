//! Greedy left-deep join ordering for multi-hop path patterns.
//!
//! When a path pattern has multiple hops (e.g. `(a)-[]->(b)-[]->(c)-[]->(d)`),
//! the order in which we expand edges affects performance. This module uses a
//! greedy heuristic to reorder expansions by estimated cost.
//!
//! Ported from gleaph-old's `greedy_left_deep_join_order`.

use gleaph_gql::ast::*;
use gleaph_gql::types::EdgeDirection;

use crate::anchor::extract_simple_label;
use crate::stats::GraphStats;

/// A hop in a path pattern: edge + destination node.
#[derive(Clone, Debug)]
pub struct PathHop {
    pub index: usize,
    pub edge: EdgePattern,
    pub edge_var: String,
    pub dst_node: NodePattern,
    pub dst_var: String,
    pub quantifier: Option<PathQuantifier>,
}

/// Extract hops from a path term's factors.
///
/// A path term alternates: Node, Edge, Node, Edge, Node...
/// Each (Edge, subsequent Node) pair forms a hop.
pub fn extract_hops(term: &PathTerm) -> Vec<PathHop> {
    let mut hops = Vec::new();
    let mut i = 0;

    while i < term.factors.len() {
        if let PathPrimary::Edge(edge) = &term.factors[i].primary {
            let edge_var = edge
                .variable
                .clone()
                .unwrap_or_else(|| format!("__anon_e{}", i));

            // Look ahead for the destination node.
            let (dst_node, dst_var) = if i + 1 < term.factors.len() {
                if let PathPrimary::Node(node) = &term.factors[i + 1].primary {
                    let var = node
                        .variable
                        .clone()
                        .unwrap_or_else(|| format!("__anon_n{}", i + 1));
                    (node.clone(), var)
                } else {
                    i += 1;
                    continue;
                }
            } else {
                i += 1;
                continue;
            };

            hops.push(PathHop {
                index: hops.len(),
                edge: edge.clone(),
                edge_var,
                dst_node,
                dst_var,
                quantifier: term.factors[i].quantifier.clone(),
            });
        }
        i += 1;
    }

    hops
}

/// Reorder hops using a greedy heuristic: at each step, pick the hop with
/// the lowest estimated cost.
///
/// Returns indices into the original hop array in the reordered sequence.
pub fn greedy_join_order(hops: &[PathHop], stats: Option<&dyn GraphStats>) -> Vec<usize> {
    if hops.len() <= 1 {
        return (0..hops.len()).collect();
    }

    let mut remaining: Vec<usize> = (0..hops.len()).collect();
    let mut order = Vec::with_capacity(remaining.len());

    while !remaining.is_empty() {
        let best_pos = remaining
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                hop_score(&hops[**a], stats)
                    .partial_cmp(&hop_score(&hops[**b], stats))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(pos, _)| pos)
            .expect("non-empty");
        order.push(remaining.remove(best_pos));
    }

    order
}

/// Score a single hop for join ordering. Lower is better.
///
/// Factors:
/// - Destination label cardinality (lower = more selective)
/// - Variable-length path penalty
/// - Direction penalty (outgoing < incoming < either)
/// - Edge property selectivity bonus
fn hop_score(hop: &PathHop, stats: Option<&dyn GraphStats>) -> f64 {
    // Destination label cardinality.
    let dst_label = extract_simple_label(&hop.dst_node.label);
    let label_card = dst_label
        .as_ref()
        .and_then(|l| stats.and_then(|s| s.label_cardinality(l)))
        .unwrap_or(u64::MAX / 4) as f64;

    // Variable-length path penalty.
    let len_penalty = if let Some(q) = &hop.quantifier {
        match q {
            PathQuantifier::Star => 10.0,
            PathQuantifier::Plus => 5.0,
            PathQuantifier::Optional => 1.0,
            PathQuantifier::Fixed(n) => *n as f64,
            PathQuantifier::Range { lower, upper } => {
                let max = upper.unwrap_or(lower + 5);
                ((lower + max) as f64).max(1.0) * 2.0
            }
        }
    } else {
        1.0
    };

    // Direction penalty.
    let dir_penalty = match hop.edge.direction {
        EdgeDirection::PointingRight => 1.0,
        EdgeDirection::PointingLeft => 1.05,
        EdgeDirection::LeftOrRight => 1.1,
        _ => 1.1,
    };

    // Edge property selectivity bonus.
    let prop_bonus = if !hop.edge.properties.is_empty() {
        0.5 // Having edge property constraints helps selectivity.
    } else {
        1.0
    };

    label_card * len_penalty * dir_penalty * prop_bonus
}

// ════════════════════════════════════════════════════════════════════════════════
// Cyclic pattern detection
// ════════════════════════════════════════════════════════════════════════════════

/// Detect cyclic patterns in the path hops.
///
/// A cycle exists when a hop's destination variable equals the first node's
/// variable or any earlier source variable, forming a closed loop.
pub fn detect_cyclic_patterns(
    hops: &[PathHop],
    first_node_var: &str,
) -> Vec<crate::plan::CyclicPattern> {
    let mut cycles = Vec::new();

    // Collect all node variables in traversal order.
    let mut node_vars = vec![first_node_var.to_string()];
    for hop in hops {
        node_vars.push(hop.dst_var.clone());
    }

    // Check if any dst_var matches an earlier node_var (creating a cycle).
    for (i, hop) in hops.iter().enumerate() {
        // Variables seen before this hop's destination.
        let earlier_vars: Vec<&str> = node_vars[..=i].iter().map(|s| s.as_str()).collect();
        if earlier_vars.contains(&hop.dst_var.as_str()) {
            // Found a cycle. Collect the variables in the cycle.
            let cycle_start_idx = earlier_vars
                .iter()
                .position(|v| *v == hop.dst_var)
                .unwrap();
            let cycle_vars: Vec<crate::plan::Str> = node_vars[cycle_start_idx..=i + 1]
                .iter()
                .map(|s| crate::plan::Str::from(s.as_str()))
                .collect();
            cycles.push(crate::plan::CyclicPattern {
                variables: cycle_vars,
            });
        }
    }

    cycles
}
