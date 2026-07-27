use gleaph_gql::ast::*;
use std::collections::BTreeSet;

use super::super::PlannerError;
use super::super::result::flatten_conjunction;
use crate::anchor::{self};
use crate::path_extensions::PlanBuildOptions;
use crate::plan::*;
use crate::stats::GraphStats;

/// Variables bound by a single path pattern, including node, edge, and the path variable itself.
fn path_variables(path: &PathPattern) -> BTreeSet<String> {
    let mut vars = BTreeSet::new();
    collect_path_expr_variables(&path.expr, &mut vars);
    if let Some(v) = &path.variable {
        vars.insert(v.clone());
    }
    vars
}

fn collect_path_expr_variables(expr: &PathPatternExpr, vars: &mut BTreeSet<String>) {
    let terms = match expr {
        PathPatternExpr::Term(term) => std::slice::from_ref(term),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            terms.as_slice()
        }
    };
    for term in terms {
        for factor in &term.factors {
            match &factor.primary {
                PathPrimary::Node(node) => {
                    if let Some(v) = &node.variable {
                        vars.insert(v.clone());
                    }
                }
                PathPrimary::Edge(edge) => {
                    if let Some(v) = &edge.variable {
                        vars.insert(v.clone());
                    }
                }
                PathPrimary::Parenthesized { variable, expr, .. } => {
                    if let Some(v) = variable {
                        vars.insert(v.clone());
                    }
                    collect_path_expr_variables(expr, vars);
                }
                PathPrimary::Simplified(_) => {}
            }
        }
    }
}

/// Group path indices by variable connectivity. Two paths share a group if they
/// bind a common variable (transitive closure). Groups preserve input order.
fn group_connected_paths(paths: &[PathPattern]) -> Vec<Vec<usize>> {
    if paths.len() <= 1 {
        return (0..paths.len()).map(|i| vec![i]).collect();
    }

    let path_vars: Vec<BTreeSet<String>> = paths.iter().map(path_variables).collect();
    let mut assigned = vec![false; paths.len()];
    let mut groups = Vec::new();

    for i in 0..paths.len() {
        if assigned[i] {
            continue;
        }
        let mut group_vars = path_vars[i].clone();
        let mut group = vec![i];
        assigned[i] = true;
        loop {
            let mut added = false;
            for j in 0..paths.len() {
                if assigned[j] {
                    continue;
                }
                if !path_vars[j].is_disjoint(&group_vars) {
                    group_vars.extend(path_vars[j].iter().cloned());
                    group.push(j);
                    assigned[j] = true;
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        groups.push(group);
    }

    groups
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_match(
    match_stmt: &MatchStatement,
    stage: usize,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    referenced_vars: &BTreeSet<String>,
    options: PlanBuildOptions<'_>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    let pattern = &match_stmt.pattern;
    let mut where_conjuncts: Vec<Expr> = pattern
        .where_clause
        .as_ref()
        .map(flatten_conjunction)
        .unwrap_or_default();

    if match_stmt.optional {
        // OPTIONAL MATCH: build sub-plan and wrap in OptionalMatch.
        let mut sub_ops = Vec::new();
        let prior_bound = bound_node_vars.clone();
        let mut sub_bound_node_vars = bound_node_vars.clone();
        let mut sub_optional_node_vars = BTreeSet::new();
        for path_pattern in &pattern.paths {
            plan_path_pattern(
                path_pattern,
                stats,
                conditional_candidates,
                referenced_vars,
                &mut where_conjuncts,
                options,
                &mut sub_bound_node_vars,
                &mut sub_optional_node_vars,
                &mut sub_ops,
                annotations,
            )?;
        }
        if !where_conjuncts.is_empty() {
            sub_ops.push(PlanOp::PropertyFilter {
                predicates: where_conjuncts,
                stage,
            });
        }
        ops.push(PlanOp::OptionalMatch { sub_plan: sub_ops });
        for v in sub_bound_node_vars.difference(&prior_bound) {
            optional_node_vars.insert(v.clone());
        }
        return Ok(());
    }

    // Choose anchor for this match.
    if stage == 0
        && let Some(anchor_info) = anchor::choose_anchor(pattern, stats)
    {
        annotations.optimizer.anchor = Some(anchor_info);
    }

    // Plan each path pattern. If a single MATCH contains multiple disconnected
    // path groups, plan each group as a sub-plan and combine with CartesianProduct.
    // This keeps the top-level plan from presenting several leading scans to the
    // federated execution guard, which rejects unseeded leading NodeScan/IndexScan
    // operators on graph shards.
    let path_groups = group_connected_paths(&pattern.paths);
    if path_groups.len() > 1 {
        let mut group_plans: Vec<Vec<PlanOp>> = Vec::with_capacity(path_groups.len());
        for group in &path_groups {
            let mut group_ops = Vec::new();
            for &idx in group {
                plan_path_pattern(
                    &pattern.paths[idx],
                    stats,
                    conditional_candidates,
                    referenced_vars,
                    &mut where_conjuncts,
                    options,
                    bound_node_vars,
                    optional_node_vars,
                    &mut group_ops,
                    annotations,
                )?;
            }
            group_plans.push(group_ops);
        }
        let mut combined = group_plans.remove(0);
        for next in group_plans {
            combined = vec![PlanOp::CartesianProduct {
                left: combined,
                right: next,
            }];
        }
        ops.extend(combined);
    } else {
        for path_pattern in &pattern.paths {
            plan_path_pattern(
                path_pattern,
                stats,
                conditional_candidates,
                referenced_vars,
                &mut where_conjuncts,
                options,
                bound_node_vars,
                optional_node_vars,
                ops,
                annotations,
            )?;
        }
    }

    if !where_conjuncts.is_empty() {
        ops.push(PlanOp::PropertyFilter {
            predicates: where_conjuncts,
            stage,
        });
    }
    Ok(())
}

mod lower;
mod term;

use lower::plan_path_pattern;
