use gleaph_gql::ast::*;
use std::collections::BTreeSet;

use super::super::super::match_plan::{PlanInputs, PlanOutput, PlanVars};
use super::super::PlannerError;
use super::super::result::flatten_conjunction;
use crate::anchor::{self};
use crate::plan::*;

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

/// The working set a path build threads: the WHERE conjuncts it collects, the variable bookkeeping, and where it appends.
///
/// The five were positional arguments of `plan_path_pattern`, `plan_path_term` and `plan_path_expr`, each of them a distinct `&mut` -
/// so a call site could hand the ops vector where the conjunct vector belongs, and both are `Vec`s.
pub(crate) struct PathBuild<'a> {
    pub(crate) where_conjuncts: &'a mut Vec<Expr>,
    pub(crate) vars: PlanVars<'a>,
    pub(crate) output: PlanOutput<'a>,
}

pub(crate) fn plan_match(
    match_stmt: &MatchStatement,
    stage: usize,
    inputs: PlanInputs<'_>,
    vars: PlanVars<'_>,
    output: PlanOutput<'_>,
) -> Result<(), PlannerError> {
    // Clauses this planner does not lower are rejected here, before any operator is emitted:
    // planning the pattern without them would execute a different query than the one written.
    if match_stmt.yield_items.is_some() {
        return Err(PlannerError::UnsupportedStatement(
            "MATCH ... YIELD is not implemented; project the bindings with RETURN".into(),
        ));
    }
    if match_stmt.pattern.match_mode.is_some() {
        return Err(PlannerError::UnsupportedPattern(
            "match modes (REPEATABLE ELEMENTS / DIFFERENT EDGES) are not implemented".into(),
        ));
    }
    if match_stmt.pattern.keep.is_some() {
        return Err(PlannerError::UnsupportedPattern(
            "KEEP is not implemented".into(),
        ));
    }

    // `plan_match` reads only the statistics from the shared inputs; the rest travel to the path builders.
    let stats = inputs.stats;
    let PlanVars {
        bound: bound_node_vars,
        optional: optional_node_vars,
    } = vars;
    let PlanOutput { ops, annotations } = output;
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
                inputs,
                PathBuild {
                    where_conjuncts: &mut where_conjuncts,
                    vars: PlanVars {
                        bound: &mut sub_bound_node_vars,
                        optional: &mut sub_optional_node_vars,
                    },
                    output: PlanOutput {
                        ops: &mut sub_ops,
                        annotations,
                    },
                },
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

    // Full-text threshold seed: drain the matched conjunct so no residual filter can
    // reference the score function, and remember the original for restoration if the
    // seeded variable never receives its own scan (the predicate must never be lost
    // silently; an unconsumed seed falls back to a residual conjunct that plan
    // validation rejects fail-closed).
    let mut restored_text_conjunct: Option<Expr> = None;
    if stage == 0
        && annotations.optimizer.text_seed.is_none()
        && let Some((idx, seed)) = crate::text_scan::find_text_threshold_seed(
            &where_conjuncts,
            pattern,
            bound_node_vars,
            stats,
        )
    {
        restored_text_conjunct = Some(where_conjuncts.remove(idx));
        annotations.optimizer.text_seed = Some(seed);
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
                    inputs,
                    PathBuild {
                        where_conjuncts: &mut where_conjuncts,
                        vars: PlanVars {
                            bound: bound_node_vars,
                            optional: optional_node_vars,
                        },
                        output: PlanOutput {
                            ops: &mut group_ops,
                            annotations,
                        },
                    },
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
                inputs,
                PathBuild {
                    where_conjuncts: &mut where_conjuncts,
                    vars: PlanVars {
                        bound: bound_node_vars,
                        optional: optional_node_vars,
                    },
                    output: PlanOutput { ops, annotations },
                },
            )?;
        }
    }

    // A seed that was never consumed means the seeded variable binds through traversal
    // instead of its own scan; restore the conjunct so it stays residual and plan
    // validation rejects the unfused score expression instead of dropping it.
    if let Some(conjunct) = restored_text_conjunct.take()
        && annotations.optimizer.text_seed.is_some()
    {
        where_conjuncts.push(conjunct);
        annotations.optimizer.text_seed = None;
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
