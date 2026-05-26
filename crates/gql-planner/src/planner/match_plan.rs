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
use super::PlannerError;
/// Detect conditional index scan candidates from semantic analysis.
pub(super) fn detect_conditional_candidates(
    semantic: &SemanticAnalysis,
    stats: Option<&dyn GraphStats>,
) -> Vec<ConditionalScanCandidate> {
    let mut candidates = Vec::new();
    for constraint in &semantic.constraints {
        if let SemanticConstraint::OptionalFilterPredicate {
            param_name,
            var,
            property,
            op,
        } = constraint
        {
            // Only consider if the property is indexed.
            let indexed = stats
                .map(|s| {
                    if *op == CmpOp::Eq {
                        s.is_vertex_property_indexed(property)
                    } else {
                        s.is_vertex_property_range_indexed(property)
                    }
                })
                .unwrap_or(false);
            if indexed {
                candidates.push(ConditionalScanCandidate {
                    param_name: param_name.clone().into(),
                    property: property.clone().into(),
                    variable: var.clone().into(),
                    cmp: *op,
                });
            }
        }
    }
    // Sort by selectivity (equality first, then range).
    candidates.sort_by(|a, b| {
        let a_eq = a.cmp == CmpOp::Eq;
        let b_eq = b.cmp == CmpOp::Eq;
        b_eq.cmp(&a_eq)
    });
    candidates
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_simple_statement(
    stmt: &SimpleQueryStatement,
    stage: usize,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    binding_kinds: &std::collections::BTreeMap<String, BindingKind>,
    referenced_vars: &BTreeSet<String>,
    schema: &dyn PropertySchema,
    options: PlanBuildOptions<'_>,
    bound_node_vars: &mut BTreeSet<String>,
    optional_node_vars: &mut BTreeSet<String>,
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) -> Result<(), PlannerError> {
    match stmt {
        SimpleQueryStatement::Match(m) => {
            path::plan_match(
                m,
                stage,
                stats,
                conditional_candidates,
                referenced_vars,
                options,
                bound_node_vars,
                optional_node_vars,
                ops,
                annotations,
            )?;
            Ok(())
        }
        SimpleQueryStatement::Filter(f) => {
            ops.push(PlanOp::Filter {
                condition: f.condition.clone(),
            });
            Ok(())
        }
        SimpleQueryStatement::Let(l) => {
            ops.push(PlanOp::Let {
                bindings: l.bindings.clone(),
            });
            Ok(())
        }
        SimpleQueryStatement::For(f) => {
            ops.push(PlanOp::For {
                variable: f.variable.clone().into(),
                list: f.list.clone(),
                ordinality: f.ordinality.as_ref().map(|o| o.variable.clone().into()),
            });
            Ok(())
        }
        SimpleQueryStatement::OrderBy(o) => {
            ops.push(PlanOp::Sort {
                order_by: o.clone(),
            });
            Ok(())
        }
        SimpleQueryStatement::Limit(l) => {
            ops.push(PlanOp::Limit {
                count: Some(l.count.clone()),
                offset: None,
            });
            Ok(())
        }
        SimpleQueryStatement::Offset(o) => {
            // Merge with preceding Limit if possible, otherwise standalone.
            if let Some(PlanOp::Limit { offset, .. }) = ops.last_mut() {
                *offset = Some(o.count.clone());
            } else {
                ops.push(PlanOp::Limit {
                    count: None,
                    offset: Some(o.count.clone()),
                });
            }
            Ok(())
        }
        SimpleQueryStatement::Insert(insert_stmt) => {
            super::dml::plan_insert(insert_stmt, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::Set(set_stmt) => {
            super::dml::plan_set(set_stmt, binding_kinds, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::Remove(remove_stmt) => {
            super::dml::plan_remove(remove_stmt, binding_kinds, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::Delete(delete_stmt) => {
            super::dml::plan_delete(delete_stmt, binding_kinds, ops, annotations);
            Ok(())
        }
        SimpleQueryStatement::CallProcedure(call) => {
            let yield_columns = call.yield_items.as_ref().map(|items| {
                items
                    .iter()
                    .map(|yi| YieldColumn {
                        name: yi.name.clone().into(),
                        alias: yi.alias.as_ref().map(|a| Str::from(a.as_str())),
                    })
                    .collect()
            });
            ops.push(PlanOp::CallProcedure {
                name: call
                    .name
                    .parts
                    .iter()
                    .map(|s| Str::from(s.as_str()))
                    .collect(),
                args: call.args.clone(),
                yield_columns,
                optional: call.optional,
            });
            Ok(())
        }
        SimpleQueryStatement::InlineProcedureCall(inline) => {
            let mut sub_plan = super::build_composite_plan_with_binding_kinds_and_options(
                &inline.body,
                options,
                None,
                schema,
            )?;
            if let Some(graph) = &inline.use_graph {
                let wrapped_ops = std::mem::take(&mut sub_plan.ops);
                sub_plan.ops = vec![PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: Some(wrapped_ops),
                }];
            }
            ops.push(PlanOp::InlineProcedureCall {
                sub_plan: Box::new(sub_plan),
                scope_vars: inline
                    .scope_vars
                    .iter()
                    .map(|s| Str::from(s.as_str()))
                    .collect(),
                optional: inline.optional,
            });
            Ok(())
        }
        SimpleQueryStatement::Focused { graph, body } => {
            if let Some(inner) = body {
                let mut sub_ops = Vec::new();
                let mut sub_bound_node_vars = BTreeSet::new();
                let mut sub_optional_node_vars = BTreeSet::new();
                plan_simple_statement(
                    inner,
                    stage,
                    stats,
                    conditional_candidates,
                    binding_kinds,
                    referenced_vars,
                    schema,
                    options,
                    &mut sub_bound_node_vars,
                    &mut sub_optional_node_vars,
                    &mut sub_ops,
                    annotations,
                )?;
                ops.push(PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: Some(sub_ops),
                });
            } else {
                ops.push(PlanOp::UseGraph {
                    graph_name: graph.parts.iter().map(|s| Str::from(s.as_str())).collect(),
                    sub_plan: None,
                });
            }
            Ok(())
        }
    }
}


mod path;
mod result;

pub(crate) use result::plan_result_statement;
