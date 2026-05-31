use gleaph_gql::ast::*;
use std::collections::BTreeSet;

use super::super::filters::{EdgeFilterFusion, quantifier_to_var_len};
use super::lower::{
    PathElement, first_hop_supports_leading_edge_index, hop_aux_binding_for_edge_if_referenced,
    plan_edge_expand_labels,
};
use crate::anchor::extract_simple_label;
use crate::join_order;
use crate::plan::*;
use crate::planner::PlannerError;
use crate::stats::GraphStats;

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_path_term(
    term: &PathTerm,
    shortest_mode: Option<ShortestMode>,
    shortest_path_cost: ShortestPathCost,
    path_var: Option<&str>,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    where_conjuncts: &mut Vec<Expr>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    let term = super::lower::normalize_path_term(term)?;
    // Compute join ordering and detect cyclic patterns.
    let hops = join_order::extract_hops(&term);
    if hops.len() > 1 {
        let order = join_order::greedy_join_order(&hops, stats);
        if order != (0..hops.len()).collect::<Vec<_>>() {
            annotations.optimizer.join_order = Some(order);
        }
    }
    if !hops.is_empty() {
        // Determine the first node variable for cycle detection.
        let first_node_var = term
            .factors
            .iter()
            .find_map(|f| match &f.primary {
                PathPrimary::Node(n) => n.variable.clone(),
                _ => None,
            })
            .unwrap_or_default();
        let cycles = join_order::detect_cyclic_patterns(&hops, &first_node_var);
        if !cycles.is_empty() {
            annotations.optimizer.cyclic_patterns = Some(cycles);
        }
    }

    // Pre-extract node/edge elements with their variables for lookahead.
    // A GQL path term alternates: Node, Edge, Node, Edge, Node, ...
    // We collect all elements first so edges can resolve their dst node.
    let elements: Vec<PathElement> = term
        .factors
        .iter()
        .enumerate()
        .map(|(i, factor)| match &factor.primary {
            PathPrimary::Node(node) => {
                let var = node
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("__anon_n{}", i));
                PathElement::Node {
                    var,
                    node: node.clone(),
                }
            }
            PathPrimary::Edge(edge) => {
                let var = edge
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("__anon_e{}", i));
                PathElement::Edge {
                    var,
                    edge: edge.clone(),
                    quantifier: factor.quantifier.clone(),
                }
            }
            PathPrimary::Parenthesized { expr, .. } => PathElement::Sub(expr.as_ref().clone()),
            PathPrimary::Simplified(_) => {
                unreachable!("normalize_path_term should remove simplified primaries")
            }
        })
        .collect();

    let leading_first_hop_eligible = first_hop_supports_leading_edge_index(
        &elements,
        where_conjuncts.as_slice(),
        stats,
        conditional_candidates,
        annotations,
        shortest_mode,
    );

    let entry_bound_node_vars = bound_node_vars.clone();
    let entry_optional_node_vars = optional_node_vars.clone();

    let mut prev_node_var: Option<String> = None;
    let mut pending_deferred_first_scan: Option<(String, Option<String>, NodePattern)> = None;
    // Track nodes whose inline filters were fused into ExpandFilter.
    let mut fused_nodes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut enforced_reuse_nodes: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut seen_path_node_vars: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut path_bound_node_vars: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    for (idx, elem) in elements.iter().enumerate() {
        match elem {
            PathElement::Node { var, node } => {
                let label = extract_simple_label(&node.label);

                let reuse_from_prior_match =
                    entry_bound_node_vars.contains(var) || entry_optional_node_vars.contains(var);
                let reuse_within_path = seen_path_node_vars.contains(var);
                seen_path_node_vars.insert(var.clone());

                if reuse_from_prior_match || reuse_within_path {
                    if !enforced_reuse_nodes.contains(var) {
                        super::super::filters::emit_bound_node_pattern_checks(
                            var,
                            node,
                            optional_node_vars.contains(var),
                            ops,
                        );
                        enforced_reuse_nodes.insert(var.clone());
                    }
                } else if prev_node_var.is_none() && !path_bound_node_vars.contains(var) {
                    if leading_first_hop_eligible {
                        pending_deferred_first_scan =
                            Some((var.clone(), label.clone(), node.clone()));
                    } else {
                        super::super::filters::emit_scan_for_node(
                            var,
                            &label,
                            node,
                            stats,
                            conditional_candidates,
                            ops,
                            annotations,
                        );
                        bound_node_vars.insert(var.clone());
                        path_bound_node_vars.insert(var.clone());
                    }
                }

                if !fused_nodes.contains(var) {
                    let defer_near = leading_first_hop_eligible
                        && idx == 0
                        && pending_deferred_first_scan.is_some();
                    if !defer_near {
                        super::super::filters::emit_node_inline_filters(var, node, ops);
                    }
                }

                prev_node_var = Some(var.clone());
            }
            PathElement::Edge {
                var: edge_var,
                edge,
                quantifier,
            } => {
                let (label_str, label_expr) = plan_edge_expand_labels(edge);

                if let Some(src_var) = &prev_node_var {
                    // Lookahead: find the next node and its variable.
                    let (dst_var, dst_node) = elements[idx + 1..]
                        .iter()
                        .find_map(|e| match e {
                            PathElement::Node { var, node } => Some((var.clone(), Some(node))),
                            _ => None,
                        })
                        .unwrap_or_else(|| (format!("__anon_dst_{}", idx), None));

                    let var_len = quantifier.as_ref().and_then(quantifier_to_var_len);

                    // FilterIntoPattern: collect dst node's inline filters for fusion.
                    let dst_filters = dst_node
                        .map(|n| super::super::filters::collect_node_inline_predicates(&dst_var, n))
                        .unwrap_or_default();

                    let src_str: Str = src_var.as_str().into();
                    let edge_str: Str = edge_var.as_str().into();
                    let dst_str: Str = dst_var.as_str().into();

                    let try_leading = idx == 1
                        && shortest_mode.is_none()
                        && pending_deferred_first_scan
                            .as_ref()
                            .is_some_and(|p| p.0 == *src_var);
                    let mut wc_clone = where_conjuncts.clone();
                    let fusion_on_clone = super::super::filters::plan_edge_filter_fusion(
                        edge_var,
                        edge,
                        stats,
                        false,
                        &mut wc_clone,
                    );
                    let use_leading = try_leading
                        && fusion_on_clone.indexed_equality.is_some()
                        && var_len.is_none()
                        && label_expr.is_none();

                    if use_leading {
                        *where_conjuncts = wc_clone;
                        let (_, _, near_node) = pending_deferred_first_scan.take().unwrap();
                        let (prop, scan_val) = fusion_on_clone.indexed_equality.as_ref().unwrap();
                        ops.push(PlanOp::EdgeIndexScan {
                            variable: edge_str.clone(),
                            property: prop.clone(),
                            value: scan_val.clone(),
                            property_projection: None,
                        });
                        ops.push(PlanOp::EdgeBindEndpoints {
                            edge: edge_str.clone(),
                            near: src_str.clone(),
                            far: dst_str.clone(),
                            direction: edge.direction,
                            label: label_str.clone(),
                            near_property_projection: None,
                            far_property_projection: None,
                            hop_aux_binding: hop_aux_binding_for_edge_if_referenced(
                                edge_var,
                                referenced_vars,
                            ),
                        });
                        super::super::filters::emit_edge_inline_filters(
                            edge_var,
                            edge,
                            &fusion_on_clone,
                            ops,
                        );
                        bound_node_vars.insert(src_var.clone());
                        bound_node_vars.insert(dst_var.clone());
                        path_bound_node_vars.insert(src_var.clone());
                        path_bound_node_vars.insert(dst_var.clone());
                        if !dst_filters.is_empty() {
                            ops.push(PlanOp::PropertyFilter {
                                predicates: dst_filters,
                                stage: 0,
                            });
                            fused_nodes.insert(dst_var.clone());
                        }
                        super::super::filters::emit_node_inline_filters(src_var, &near_node, ops);
                    } else {
                        if let Some((v, lbl, n)) = pending_deferred_first_scan.take() {
                            super::super::filters::emit_scan_for_node(
                                &v,
                                &lbl,
                                &n,
                                stats,
                                conditional_candidates,
                                ops,
                                annotations,
                            );
                            bound_node_vars.insert(v.clone());
                            path_bound_node_vars.insert(v);
                        }

                        let edge_fusion = if shortest_mode.is_some() {
                            EdgeFilterFusion::default()
                        } else {
                            super::super::filters::plan_edge_filter_fusion(
                                edge_var,
                                edge,
                                stats,
                                label_str.is_some() && label_expr.is_none() && var_len.is_none(),
                                where_conjuncts,
                            )
                        };
                        let indexed_edge_equality = edge_fusion.indexed_equality.clone();
                        let edge_payload_predicate = edge_fusion.edge_payload_predicate.clone();
                        let edge_vector_predicate = edge_fusion.edge_vector_predicate.clone();

                        if let Some(mode) = shortest_mode {
                            if let Some(dst_node) = dst_node.as_ref() {
                                if !entry_bound_node_vars.contains(&dst_var)
                                    && !entry_optional_node_vars.contains(&dst_var)
                                    && !bound_node_vars.contains(&dst_var)
                                    && !optional_node_vars.contains(&dst_var)
                                {
                                    let dst_label = extract_simple_label(&dst_node.label);
                                    super::super::filters::emit_scan_for_node(
                                        &dst_var,
                                        &dst_label,
                                        dst_node,
                                        stats,
                                        conditional_candidates,
                                        ops,
                                        annotations,
                                    );
                                    bound_node_vars.insert(dst_var.clone());
                                    path_bound_node_vars.insert(dst_var.clone());
                                } else if !enforced_reuse_nodes.contains(&dst_var) {
                                    super::super::filters::emit_bound_node_pattern_checks(
                                        &dst_var,
                                        dst_node,
                                        optional_node_vars.contains(&dst_var),
                                        ops,
                                    );
                                    enforced_reuse_nodes.insert(dst_var.clone());
                                }
                            }
                            ops.push(PlanOp::ShortestPath {
                                src: src_str,
                                dst: dst_str.clone(),
                                edge: edge_str,
                                path_var: path_var.map(Into::into),
                                emit_edge_binding: true,
                                emit_path_binding: true,
                                mode,
                                direction: edge.direction,
                                label: label_str.clone(),
                                label_expr,
                                var_len,
                                cost: shortest_path_cost.clone(),
                            });
                            if !dst_filters.is_empty() {
                                ops.push(PlanOp::PropertyFilter {
                                    predicates: dst_filters,
                                    stage: 0,
                                });
                            }
                        } else if !dst_filters.is_empty() {
                            ops.push(PlanOp::ExpandFilter {
                                src: src_str,
                                edge: edge_str,
                                dst: dst_str.clone(),
                                direction: edge.direction,
                                label: label_str,
                                label_expr,
                                var_len,
                                indexed_edge_equality,
                                edge_payload_predicate,
                                edge_vector_predicate,
                                dst_filter: dst_filters,
                                edge_property_projection: None,
                                dst_property_projection: None,
                                hop_aux_binding: hop_aux_binding_for_edge_if_referenced(
                                    edge_var,
                                    referenced_vars,
                                ),
                                emit_edge_binding: true,
                            });
                            bound_node_vars.insert(dst_var.clone());
                            path_bound_node_vars.insert(dst_var.clone());
                            fused_nodes.insert(dst_var.clone());
                        } else {
                            ops.push(PlanOp::Expand {
                                src: src_str,
                                edge: edge_str,
                                dst: dst_str,
                                direction: edge.direction,
                                label: label_str,
                                label_expr,
                                var_len,
                                indexed_edge_equality,
                                edge_payload_predicate,
                                edge_vector_predicate,
                                edge_property_projection: None,
                                dst_property_projection: None,
                                hop_aux_binding: hop_aux_binding_for_edge_if_referenced(
                                    edge_var,
                                    referenced_vars,
                                ),
                                emit_edge_binding: true,
                            });
                            bound_node_vars.insert(dst_var.clone());
                            path_bound_node_vars.insert(dst_var.clone());
                        }

                        super::super::filters::emit_edge_inline_filters(
                            edge_var,
                            edge,
                            &edge_fusion,
                            ops,
                        );
                    }
                }
                prev_node_var = None;
            }
            PathElement::Sub(expr) => {
                super::lower::plan_path_expr(
                    expr,
                    shortest_mode,
                    shortest_path_cost.clone(),
                    path_var,
                    stats,
                    conditional_candidates,
                    referenced_vars,
                    where_conjuncts,
                    bound_node_vars,
                    optional_node_vars,
                    ops,
                    annotations,
                )?;
            }
        }
    }
    Ok(())
}
