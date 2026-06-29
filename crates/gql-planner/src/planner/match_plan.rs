use gleaph_gql::ast::*;
use gleaph_gql::type_check::{BindingKind, PropertySchema};
use std::collections::BTreeSet;

use super::PlannerError;
use crate::path_extensions::PlanBuildOptions;
use crate::plan::*;
use crate::semantic::{SemanticAnalysis, SemanticConstraint};
use crate::stats::GraphStats;
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

/// Validates that a SEARCH ... WHERE filter is exactly one equality comparison or exactly
/// two equality comparisons joined by AND, all on the same searched binding, in this slice.
///
/// The planner is provider-neutral: it checks expression shape only. Label and index coverage
/// are validated later by the Router, which owns the named-index catalog.
fn validate_search_filter(
    filter: &gleaph_gql::ast::Expr,
    binding: &str,
) -> Result<(), PlannerError> {
    let conjuncts = flatten_search_filter(filter, binding)?;
    if conjuncts.is_empty() {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE filter is empty".into(),
        ));
    }
    if conjuncts.len() > 2 {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE supports at most two equality conjuncts in this slice".into(),
        ));
    }
    let mut seen_properties = std::collections::HashSet::new();
    for (property, _value) in &conjuncts {
        if !seen_properties.insert(property.to_string()) {
            return Err(PlannerError::UnsupportedPattern(
                "SEARCH ... WHERE equality conjuncts must refer to distinct properties".into(),
            ));
        }
    }
    Ok(())
}

/// Flatten a SEARCH ... WHERE expression into one or two normalized equality conjuncts.
/// Each conjunct is `(property_name, value_expr)` with the property side normalized to the
/// searched binding. Rejects every shape other than a single equality or an AND of one or
/// two equalities on the same binding.
fn flatten_search_filter(
    filter: &gleaph_gql::ast::Expr,
    binding: &str,
) -> Result<Vec<(String, gleaph_gql::ast::Expr)>, PlannerError> {
    fn is_bound_property(expr: &gleaph_gql::ast::Expr, binding: &str) -> Option<String> {
        match &expr.kind {
            gleaph_gql::ast::ExprKind::PropertyAccess {
                expr: base,
                property,
            } => {
                if matches!(&base.kind, gleaph_gql::ast::ExprKind::Variable(name) if name == binding)
                {
                    Some(property.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_literal_or_parameter(expr: &gleaph_gql::ast::Expr) -> bool {
        matches!(
            &expr.kind,
            gleaph_gql::ast::ExprKind::Literal(_) | gleaph_gql::ast::ExprKind::Parameter(_)
        )
    }

    fn validate_and_split_eq(
        expr: &gleaph_gql::ast::Expr,
        binding: &str,
    ) -> Result<(String, gleaph_gql::ast::Expr), PlannerError> {
        let gleaph_gql::ast::ExprKind::Compare { left, op, right } = &expr.kind else {
            return Err(PlannerError::UnsupportedPattern(
                "SEARCH ... WHERE must be an equality comparison in this slice".into(),
            ));
        };
        if *op != gleaph_gql::ast::CmpOp::Eq {
            return Err(PlannerError::UnsupportedPattern(
                "SEARCH ... WHERE only supports equality (=) in this slice".into(),
            ));
        }
        if let Some(property) = is_bound_property(left, binding)
            && is_literal_or_parameter(right)
        {
            return Ok((property, *right.clone()));
        }
        if let Some(property) = is_bound_property(right, binding)
            && is_literal_or_parameter(left)
        {
            return Ok((property, *left.clone()));
        }
        Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE must compare a property of the searched binding with a literal or parameter in this slice".into(),
        ))
    }

    let leaves = result::flatten_conjunction(filter);
    let mut out = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        out.push(validate_and_split_eq(leaf, binding)?);
    }
    Ok(out)
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
                offset_keyword: f.ordinality.as_ref().is_some_and(|o| o.offset_keyword),
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
        SimpleQueryStatement::Search(s) => {
            let binding_kind = binding_kinds.get(&s.binding).copied();
            let is_node_or_edge = matches!(
                binding_kind,
                Some(gleaph_gql::type_check::BindingKind::Node)
                    | Some(gleaph_gql::type_check::BindingKind::Edge)
            );
            if !is_node_or_edge {
                return Err(PlannerError::UnsupportedPattern(format!(
                    "SEARCH binding variable `{}` must refer to a node or edge variable",
                    s.binding
                )));
            }
            if let Some(filter) = s.provider.filter() {
                validate_search_filter(filter, &s.binding)?;
            }
            let provider = match &s.provider {
                SearchProvider::VectorIndex(spec) => SearchProviderPlan::VectorIndex {
                    index_name: spec
                        .index_name
                        .parts
                        .iter()
                        .map(|s| Str::from(s.as_str()))
                        .collect(),
                    query: spec.query.clone(),
                    limit: spec.limit.clone(),
                    filter: spec.filter.clone(),
                },
            };
            let output_kind = match s.output.kind {
                gleaph_gql::ast::SearchOutputKind::Score => crate::plan::SearchOutputKind::Score,
                gleaph_gql::ast::SearchOutputKind::Distance => {
                    crate::plan::SearchOutputKind::Distance
                }
            };
            ops.push(PlanOp::Search {
                binding: Str::from(s.binding.as_str()),
                provider,
                output: SearchOutputPlan {
                    kind: output_kind,
                    alias: Str::from(s.output.alias.as_str()),
                },
            });
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
                scope: match &inline.scope {
                    gleaph_gql::ast::InlineProcedureScope::ImplicitAll => {
                        crate::plan::InlineProcedureScope::ImplicitAll
                    }
                    gleaph_gql::ast::InlineProcedureScope::Explicit(vars) => {
                        crate::plan::InlineProcedureScope::Explicit(
                            vars.iter().map(|s| Str::from(s.as_str())).collect(),
                        )
                    }
                },
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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::{Expr, ExprKind};

    fn prop(var: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable(var.to_string()))),
            property: property.to_string(),
        })
    }

    fn lit(value: Value) -> Expr {
        Expr::new(ExprKind::Literal(value))
    }

    fn param(name: &str) -> Expr {
        Expr::new(ExprKind::Parameter(name.to_string()))
    }

    fn cmp(left: Expr, right: Expr) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(left),
            op: gleaph_gql::ast::CmpOp::Eq,
            right: Box::new(right),
        })
    }

    #[test]
    fn validate_search_filter_accepts_property_literal_equality() {
        let filter = cmp(prop("d", "category"), lit(Value::Text("doc".into())));
        validate_search_filter(&filter, "d").expect("property = literal should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_literal_property_equality_reversed() {
        let filter = cmp(lit(Value::Text("doc".into())), prop("d", "category"));
        validate_search_filter(&filter, "d").expect("literal = property should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_property_parameter_equality() {
        let filter = cmp(prop("d", "category"), param("$category"));
        validate_search_filter(&filter, "d").expect("property = parameter should be accepted");
    }

    #[test]
    fn validate_search_filter_rejects_range_comparison() {
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(prop("d", "category")),
            op: gleaph_gql::ast::CmpOp::Lt,
            right: Box::new(lit(Value::Text("doc".into()))),
        });
        let err = validate_search_filter(&filter, "d").expect_err("range should be rejected");
        assert!(err.to_string().contains("equality"));
    }

    #[test]
    fn validate_search_filter_accepts_two_equality_conjunction() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
            Box::new(cmp(prop("d", "tag"), lit(Value::Text("hot".into())))),
        ));
        validate_search_filter(&filter, "d").expect("two-equality conjunction should be accepted");
    }

    #[test]
    fn validate_search_filter_rejects_three_arm_conjunction() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "a"), lit(Value::Int64(1)))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "b"), lit(Value::Int64(2)))),
                Box::new(cmp(prop("d", "c"), lit(Value::Int64(3)))),
            ))),
        ));
        let err = validate_search_filter(&filter, "d")
            .expect_err("three-arm conjunction should be rejected");
        assert!(err.to_string().contains("at most two equality conjuncts"));
    }

    #[test]
    fn validate_search_filter_rejects_duplicate_property_conjunction() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
            Box::new(cmp(prop("d", "category"), lit(Value::Text("hot".into())))),
        ));
        let err = validate_search_filter(&filter, "d")
            .expect_err("duplicate property conjuncts should be rejected");
        assert!(err.to_string().contains("distinct properties"));
    }

    #[test]
    fn validate_search_filter_rejects_other_binding_property() {
        let filter = cmp(prop("a", "category"), lit(Value::Text("doc".into())));
        let err =
            validate_search_filter(&filter, "d").expect_err("other binding should be rejected");
        assert!(err.to_string().contains("property of the searched binding"));
    }

    #[test]
    fn validate_search_filter_rejects_computed_value_side() {
        let filter = cmp(
            prop("d", "category"),
            Expr::new(ExprKind::BinaryOp {
                left: Box::new(lit(Value::Int64(1))),
                op: gleaph_gql::ast::BinaryOp::Add,
                right: Box::new(lit(Value::Int64(2))),
            }),
        );
        let err =
            validate_search_filter(&filter, "d").expect_err("computed value should be rejected");
        assert!(err.to_string().contains("literal or parameter"));
    }
}
