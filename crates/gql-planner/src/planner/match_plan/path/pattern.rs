use gleaph_gql::ast::*;
use std::collections::BTreeSet;

use super::super::PlannerError;
use super::super::result::flatten_conjunction;
use crate::anchor::{self};
use crate::path_extensions::PlanBuildOptions;
use crate::plan::*;
use crate::stats::GraphStats;

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

    // Plan each path pattern.
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
