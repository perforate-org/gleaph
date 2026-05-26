use gleaph_gql::ast::Expr;
use gleaph_gql::types::{EdgeDirection, LabelExpr};

use crate::plan::*;
use crate::stats::GraphStats;
// ════════════════════════════════════════════════════════════════════════════════
// Adaptive Reoptimization Hints
// ════════════════════════════════════════════════════════════════════════════════

/// Set reoptimization hints for the executor based on plan uncertainty.
pub(super) fn set_reoptimization_hints(
    ops: &[PlanOp],
    annotations: &mut PlanAnnotations,
    stats: Option<&dyn GraphStats>,
) {
    let has_stats = stats.and_then(|s| s.avg_degree()).is_some();

    for (i, op) in ops.iter().enumerate() {
        match op {
            // Expand without stats: cardinality is uncertain.
            PlanOp::Expand { .. } | PlanOp::ExpandFilter { .. } if !has_stats => {
                annotations.optimizer.cardinality_check_points.push(i);
                if annotations.optimizer.reoptimize_after_rows.is_none() {
                    annotations.optimizer.reoptimize_after_rows = Some(1000);
                }
            }
            // Procedure calls are opaque: always a check point.
            PlanOp::CallProcedure { .. } => {
                annotations.optimizer.cardinality_check_points.push(i);
            }
            _ => {}
        }
    }

    // Large plans: set reoptimization threshold.
    if let Some(rows) = annotations.optimizer.estimated_rows
        && rows > 100_000.0
        && annotations.optimizer.reoptimize_after_rows.is_none()
    {
        annotations.optimizer.reoptimize_after_rows = Some(10_000);
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// WCOJ Replacement
// ════════════════════════════════════════════════════════════════════════════════

/// Replace `Expand` / `ExpandFilter` chains that close a detected cycle with
/// [`PlanOp::WorstCaseOptimalJoin`] when every hop can be represented on the edge ring.
///
/// Skips fusion when any hop combines **`indexed_edge_equality`** with **`var_len`** (executor uses
/// plain expansion for variable-length segments).
pub(super) fn apply_wcoj_replacement(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let cycles = match &annotations.optimizer.cyclic_patterns {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return,
    };

    for cycle in &cycles {
        let uniq = normalize_cycle_variables(&cycle.variables);
        if uniq.len() < 3 {
            continue;
        }
        let n = uniq.len();

        let mut expand_hops: Vec<CollectedWcojHop> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            match op {
                PlanOp::Expand {
                    src,
                    dst,
                    edge,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    hop_aux_binding,
                    ..
                } => {
                    expand_hops.push(CollectedWcojHop {
                        op_idx: i,
                        src: src.clone(),
                        dst: dst.clone(),
                        edge: edge.clone(),
                        label: label.clone(),
                        label_expr: label_expr.clone(),
                        direction: *direction,
                        var_len: *var_len,
                        indexed_edge_equality: indexed_edge_equality.clone(),
                        dst_filter: Vec::new(),
                        hop_aux_binding: hop_aux_binding.clone(),
                    });
                }
                PlanOp::ExpandFilter {
                    src,
                    dst,
                    edge,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    dst_filter,
                    hop_aux_binding,
                    ..
                } => {
                    expand_hops.push(CollectedWcojHop {
                        op_idx: i,
                        src: src.clone(),
                        dst: dst.clone(),
                        edge: edge.clone(),
                        label: label.clone(),
                        label_expr: label_expr.clone(),
                        direction: *direction,
                        var_len: *var_len,
                        indexed_edge_equality: indexed_edge_equality.clone(),
                        dst_filter: dst_filter.clone(),
                        hop_aux_binding: hop_aux_binding.clone(),
                    });
                }
                _ => {}
            }
        }

        if expand_hops
            .iter()
            .any(|h| h.var_len.is_some() && h.indexed_edge_equality.is_some())
        {
            continue;
        }

        let Some((ordered_edges, mut remove_indices)) =
            order_wcoj_edges_for_cycle(&uniq, &expand_hops)
        else {
            continue;
        };
        if ordered_edges.len() != n || remove_indices.len() != n {
            continue;
        }
        remove_indices.sort_unstable();
        for &idx in remove_indices.iter().rev() {
            ops.remove(idx);
        }
        let insert_pos = remove_indices[0].min(ops.len());
        ops.insert(
            insert_pos,
            PlanOp::WorstCaseOptimalJoin {
                variables: uniq,
                edges: ordered_edges,
            },
        );
        break;
    }
}

fn normalize_cycle_variables(variables: &[Str]) -> Vec<Str> {
    if variables.len() >= 2 && variables.first() == variables.last() {
        variables[..variables.len() - 1].to_vec()
    } else {
        variables.to_vec()
    }
}

#[derive(Clone, Debug)]
struct CollectedWcojHop {
    op_idx: usize,
    src: Str,
    dst: Str,
    edge: Str,
    label: Option<Str>,
    label_expr: Option<LabelExpr>,
    direction: EdgeDirection,
    var_len: Option<VarLenSpec>,
    indexed_edge_equality: Option<(Str, ScanValue)>,
    dst_filter: Vec<Expr>,
    hop_aux_binding: Option<Str>,
}

fn order_wcoj_edges_for_cycle(
    uniq: &[Str],
    expands: &[CollectedWcojHop],
) -> Option<(Vec<WcojEdge>, Vec<usize>)> {
    let n = uniq.len();
    let mut used = vec![false; expands.len()];
    let mut out_edges = Vec::with_capacity(n);
    let mut remove_indices = Vec::with_capacity(n);

    for i in 0..n {
        let src = &uniq[i];
        let dst = &uniq[(i + 1) % n];
        let mut found = None;
        for (j, hop) in expands.iter().enumerate() {
            if used[j] {
                continue;
            }
            if hop.src == *src && hop.dst == *dst {
                found = Some((
                    j,
                    WcojEdge {
                        src: src.clone(),
                        dst: dst.clone(),
                        variable: hop.edge.clone(),
                        label: hop.label.clone(),
                        label_expr: hop.label_expr.clone(),
                        direction: hop.direction,
                        var_len: hop.var_len,
                        indexed_edge_equality: hop.indexed_edge_equality.clone(),
                        dst_filter: hop.dst_filter.clone(),
                        hop_aux_binding: hop.hop_aux_binding.clone(),
                    },
                ));
                break;
            }
        }
        let (j, w) = found?;
        used[j] = true;
        remove_indices.push(expands[j].op_idx);
        out_edges.push(w);
    }

    Some((out_edges, remove_indices))
}
