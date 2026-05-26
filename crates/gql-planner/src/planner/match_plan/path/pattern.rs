use gleaph_gql::ast::*;
use gleaph_gql::type_check::{BindingKind, PropertySchema};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use std::collections::{BTreeMap, BTreeSet};

use crate::anchor::{self, extract_simple_label};
use crate::cost;
use crate::expr_alias::substitute_return_aliases_in_expr;
use crate::expr_children::for_each_immediate_child_expr;
use crate::join_order;
use crate::path_extensions::{
    PathPatternExtensionContext, PlanBuildOptions, REJECTING_PATH_EXTENSION_HANDLER,
    SingleEdgePathInfo,
};
use crate::plan::*;
use crate::pushdown;
use crate::semantic::{self, SemanticAnalysis, SemanticConstraint};
use crate::stats::GraphStats;
use super::filters::{
    EdgeFilterFusion, parse_edge_var_property_equality, quantifier_to_var_len,
};
use super::super::result::flatten_conjunction;
use super::super::PlannerError;

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
