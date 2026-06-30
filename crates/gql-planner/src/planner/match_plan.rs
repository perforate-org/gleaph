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

/// Validates the SEARCH ... WHERE filter shape for ADR 0034 Slices 6-14.
///
/// Accepts:
/// - any number of AND-connected same-binding equality comparisons on distinct properties
///   (pure equality conjunctions are provider-neutral; the Router / Property Index enforces
///   the execution arm limit later),
/// - exactly one same-binding range comparison (`<`, `<=`, `>`, `>=`) between a property and a
///   literal/parameter,
/// - exactly two same-binding range comparisons on the same property where one arm is a lower
///   bound (`>`, `>=`) and the other is an upper bound (`<`, `<=`),
/// - one or more AND-connected equality predicates on distinct properties combined with exactly
///   one one-sided range predicate on a different property of the same searched binding, or
/// - one or more AND-connected equality predicates on distinct properties combined with exactly
///   two range predicates on the same property (one lower bound and one upper bound) of the same
///   searched binding, where the equality properties differ from the range property.
///
/// The planner is provider-neutral: it checks expression shape only. Label, index coverage, and
/// numeric-domain verification are validated later by the Router. The execution bound on the
/// number of equality arms (e.g. the eight-arm limit for bounded server-side intersection) is
/// intentionally not enforced here; the Router / Property Index applies that provider-specific
/// limit.
fn validate_search_filter(
    filter: &gleaph_gql::ast::Expr,
    binding: &str,
) -> Result<(), PlannerError> {
    let predicates = flatten_search_filter(filter, binding)?;
    if predicates.is_empty() {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE filter is empty".into(),
        ));
    }

    let equality_count = predicates
        .iter()
        .filter(|p| matches!(p, SearchFilterPredicate::Equality { .. }))
        .count();
    let range_count = predicates
        .iter()
        .filter(|p| matches!(p, SearchFilterPredicate::Range { .. }))
        .count();

    if equality_count == predicates.len() {
        // Slice 13: provider-neutral pure equality conjunction on distinct properties.
        // The execution arm limit (e.g. eight arms) is enforced later by the Router / Property Index;
        // the planner only validates expression shape.
        validate_pure_equality_conjunction(
            &predicates,
            "SEARCH ... WHERE equality conjuncts must refer to distinct properties",
        )
    } else if range_count == predicates.len() {
        if predicates.len() == 1 {
            Ok(())
        } else if predicates.len() == 2 {
            validate_two_sided_range(&predicates)
        } else {
            Err(PlannerError::UnsupportedPattern(
                "SEARCH ... WHERE supports at most two range predicates in this slice".into(),
            ))
        }
    } else if range_count == 1 && equality_count >= 1 && predicates.len() == equality_count + 1 {
        // Slice 14: N-way equality (N >= 1) plus one range on a distinct property.
        validate_mixed_equality_range(&predicates)
    } else if range_count == 2 && equality_count >= 1 && predicates.len() == equality_count + 2 {
        // Slice 14: N-way equality (N >= 1) plus a two-sided range on a distinct property.
        validate_mixed_equality_and_two_sided_range(&predicates)
    } else {
        Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE does not support this equality/range mixture in this slice".into(),
        ))
    }
}

fn validate_pure_equality_conjunction(
    predicates: &[SearchFilterPredicate],
    duplicate_error: &str,
) -> Result<(), PlannerError> {
    let mut seen = std::collections::HashSet::new();
    for p in predicates {
        if let SearchFilterPredicate::Equality { property, .. } = p
            && !seen.insert(property.clone())
        {
            return Err(PlannerError::UnsupportedPattern(duplicate_error.into()));
        }
    }
    Ok(())
}

fn validate_mixed_equality_and_two_sided_range(
    predicates: &[SearchFilterPredicate],
) -> Result<(), PlannerError> {
    fn is_range_op(op: gleaph_gql::ast::CmpOp) -> bool {
        matches!(
            op,
            gleaph_gql::ast::CmpOp::Lt
                | gleaph_gql::ast::CmpOp::Le
                | gleaph_gql::ast::CmpOp::Gt
                | gleaph_gql::ast::CmpOp::Ge
        )
    }

    let mut equality_properties: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut range_predicates: Vec<SearchFilterPredicate> = Vec::with_capacity(2);
    for p in predicates {
        match p {
            SearchFilterPredicate::Equality { property, .. } => {
                if !equality_properties.insert(property.clone()) {
                    return Err(PlannerError::UnsupportedPattern(
                        "SEARCH ... WHERE mixed equality/range requires equality conjuncts on distinct properties".into(),
                    ));
                }
            }
            SearchFilterPredicate::Range {
                property,
                op,
                value,
            } if is_range_op(*op) => {
                range_predicates.push(SearchFilterPredicate::Range {
                    property: property.clone(),
                    op: *op,
                    value: value.clone(),
                });
            }
            _ => {
                return Err(PlannerError::UnsupportedPattern(
                    "SEARCH ... WHERE mixed arm must be equality and one or two ranges".into(),
                ));
            }
        }
    }
    if range_predicates.len() != 2 {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE mixed equality/range with a two-sided range requires exactly two ranges".into(),
        ));
    }
    validate_two_sided_range(&range_predicates)?;
    let range_property = match &range_predicates[0] {
        SearchFilterPredicate::Range { property, .. } => property.as_str(),
        _ => unreachable!("validate_two_sided_range operates on range predicates"),
    };
    if equality_properties.contains(range_property) {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE mixed equality/range arms must refer to distinct properties".into(),
        ));
    }
    Ok(())
}

fn validate_mixed_equality_range(predicates: &[SearchFilterPredicate]) -> Result<(), PlannerError> {
    fn is_range_op(op: gleaph_gql::ast::CmpOp) -> bool {
        matches!(
            op,
            gleaph_gql::ast::CmpOp::Lt
                | gleaph_gql::ast::CmpOp::Le
                | gleaph_gql::ast::CmpOp::Gt
                | gleaph_gql::ast::CmpOp::Ge
        )
    }

    let mut equality_properties: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut range_property: Option<String> = None;
    for p in predicates {
        match p {
            SearchFilterPredicate::Equality { property, .. } => {
                if !equality_properties.insert(property.clone()) {
                    return Err(PlannerError::UnsupportedPattern(
                        "SEARCH ... WHERE mixed equality/range requires equality conjuncts on distinct properties".into(),
                    ));
                }
            }
            SearchFilterPredicate::Range { property, op, .. } if is_range_op(*op) => {
                if range_property.is_some() {
                    return Err(PlannerError::UnsupportedPattern(
                        "SEARCH ... WHERE mixed equality/range with one-sided range requires exactly one range".into(),
                    ));
                }
                range_property = Some(property.clone());
            }
            _ => {
                return Err(PlannerError::UnsupportedPattern(
                    "SEARCH ... WHERE mixed arm must be equality and one range".into(),
                ));
            }
        }
    }
    let range_property = range_property.ok_or_else(|| {
        PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE mixed arm must contain at least one equality and one range".into(),
        )
    })?;
    if equality_properties.contains(&range_property) {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE mixed equality/range arms must refer to distinct properties".into(),
        ));
    }
    Ok(())
}

fn validate_two_sided_range(predicates: &[SearchFilterPredicate]) -> Result<(), PlannerError> {
    fn is_lower(op: gleaph_gql::ast::CmpOp) -> bool {
        matches!(op, gleaph_gql::ast::CmpOp::Gt | gleaph_gql::ast::CmpOp::Ge)
    }
    fn is_upper(op: gleaph_gql::ast::CmpOp) -> bool {
        matches!(op, gleaph_gql::ast::CmpOp::Lt | gleaph_gql::ast::CmpOp::Le)
    }

    if predicates.len() != 2 {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE two-sided range requires exactly two range predicates".into(),
        ));
    }
    let [first, second] = predicates else {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE two-sided range requires exactly two range predicates".into(),
        ));
    };
    let (prop1, op1) = match first {
        SearchFilterPredicate::Range { property, op, .. } => (property, *op),
        _ => unreachable!("validate_two_sided_range called with non-range"),
    };
    let (prop2, op2) = match second {
        SearchFilterPredicate::Range { property, op, .. } => (property, *op),
        _ => unreachable!("validate_two_sided_range called with non-range"),
    };
    if prop1 != prop2 {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE two-sided range requires both predicates to refer to the same property".into(),
        ));
    }
    if (is_lower(op1) && is_lower(op2)) || (is_upper(op1) && is_upper(op2)) {
        return Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE two-sided range requires one lower bound (> or >=) and one upper bound (< or <=)".into(),
        ));
    }
    if (is_lower(op1) && is_upper(op2)) || (is_upper(op1) && is_lower(op2)) {
        Ok(())
    } else {
        Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE two-sided range requires one lower bound (> or >=) and one upper bound (< or <=)".into(),
        ))
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum SearchFilterPredicate {
    Equality {
        property: String,
        value: gleaph_gql::ast::Expr,
    },
    Range {
        property: String,
        op: gleaph_gql::ast::CmpOp,
        value: gleaph_gql::ast::Expr,
    },
}

/// Flatten a SEARCH ... WHERE expression into normalized predicates.
/// Rejects every shape other than a single accepted comparison or an AND of accepted comparisons
/// on the same binding, where the non-pure-equality shapes are limited to one to three leaves:
/// one or two range arms, or exactly one equality plus one or two range arms on a distinct
/// property. Pure equality conjunctions may contain any number of leaves as long as each compares
/// a distinct property of the searched binding to a literal or parameter.
fn flatten_search_filter(
    filter: &gleaph_gql::ast::Expr,
    binding: &str,
) -> Result<Vec<SearchFilterPredicate>, PlannerError> {
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

    fn split_predicate(
        expr: &gleaph_gql::ast::Expr,
        binding: &str,
    ) -> Result<SearchFilterPredicate, PlannerError> {
        let gleaph_gql::ast::ExprKind::Compare { left, op, right } = &expr.kind else {
            return Err(PlannerError::UnsupportedPattern(
                "SEARCH ... WHERE must be an equality or range comparison in this slice".into(),
            ));
        };

        match op {
            gleaph_gql::ast::CmpOp::Eq => {
                if let Some(property) = is_bound_property(left, binding)
                    && is_literal_or_parameter(right)
                {
                    return Ok(SearchFilterPredicate::Equality {
                        property,
                        value: *right.clone(),
                    });
                }
                if let Some(property) = is_bound_property(right, binding)
                    && is_literal_or_parameter(left)
                {
                    return Ok(SearchFilterPredicate::Equality {
                        property,
                        value: *left.clone(),
                    });
                }
            }
            gleaph_gql::ast::CmpOp::Lt
            | gleaph_gql::ast::CmpOp::Le
            | gleaph_gql::ast::CmpOp::Gt
            | gleaph_gql::ast::CmpOp::Ge => {
                if let Some(property) = is_bound_property(left, binding)
                    && is_literal_or_parameter(right)
                {
                    return Ok(SearchFilterPredicate::Range {
                        property,
                        op: *op,
                        value: *right.clone(),
                    });
                }
                if let Some(property) = is_bound_property(right, binding)
                    && is_literal_or_parameter(left)
                {
                    // Normalize so the predicate always reads `binding.property OP value`.
                    let normalized_op = match op {
                        gleaph_gql::ast::CmpOp::Lt => gleaph_gql::ast::CmpOp::Gt,
                        gleaph_gql::ast::CmpOp::Le => gleaph_gql::ast::CmpOp::Ge,
                        gleaph_gql::ast::CmpOp::Gt => gleaph_gql::ast::CmpOp::Lt,
                        gleaph_gql::ast::CmpOp::Ge => gleaph_gql::ast::CmpOp::Le,
                        _ => unreachable!(),
                    };
                    return Ok(SearchFilterPredicate::Range {
                        property,
                        op: normalized_op,
                        value: *left.clone(),
                    });
                }
            }
            _ => {
                return Err(PlannerError::UnsupportedPattern(
                    "SEARCH ... WHERE only supports equality (=) or a single numeric range predicate (<, <=, >, >=) in this slice".into(),
                ));
            }
        }

        Err(PlannerError::UnsupportedPattern(
            "SEARCH ... WHERE must compare a property of the searched binding with a literal or parameter in this slice".into(),
        ))
    }

    let leaves = result::flatten_conjunction(filter);
    let mut out = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        out.push(split_predicate(leaf, binding)?);
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

    fn cmp_op(left: Expr, op: gleaph_gql::ast::CmpOp, right: Expr) -> Expr {
        Expr::new(ExprKind::Compare {
            left: Box::new(left),
            op,
            right: Box::new(right),
        })
    }

    #[test]
    fn validate_search_filter_accepts_numeric_range() {
        let filter = cmp_op(
            prop("d", "price"),
            gleaph_gql::ast::CmpOp::Ge,
            lit(Value::Int64(5)),
        );
        validate_search_filter(&filter, "d").expect("numeric range predicate should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_all_numeric_range_operators() {
        for op in [
            gleaph_gql::ast::CmpOp::Lt,
            gleaph_gql::ast::CmpOp::Le,
            gleaph_gql::ast::CmpOp::Gt,
            gleaph_gql::ast::CmpOp::Ge,
        ] {
            let filter = cmp_op(prop("d", "price"), op, lit(Value::Int64(5)));
            validate_search_filter(&filter, "d")
                .unwrap_or_else(|e| panic!("range operator {op:?} should be accepted: {e}"));
        }
    }

    #[test]
    fn validate_search_filter_accepts_reversed_range_operands() {
        // `5 < d.price` normalizes to `d.price > 5` and is accepted by the shape check.
        let filter = cmp_op(
            lit(Value::Int64(5)),
            gleaph_gql::ast::CmpOp::Lt,
            prop("d", "price"),
        );
        validate_search_filter(&filter, "d").expect("reversed range operands should be accepted");
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
    fn validate_search_filter_accepts_eight_arm_conjunction() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "p1"), lit(Value::Int64(1)))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "p2"), lit(Value::Int64(2)))),
                Box::new(Expr::new(ExprKind::And(
                    Box::new(cmp(prop("d", "p3"), lit(Value::Int64(3)))),
                    Box::new(Expr::new(ExprKind::And(
                        Box::new(cmp(prop("d", "p4"), lit(Value::Int64(4)))),
                        Box::new(Expr::new(ExprKind::And(
                            Box::new(cmp(prop("d", "p5"), lit(Value::Int64(5)))),
                            Box::new(Expr::new(ExprKind::And(
                                Box::new(cmp(prop("d", "p6"), lit(Value::Int64(6)))),
                                Box::new(Expr::new(ExprKind::And(
                                    Box::new(cmp(prop("d", "p7"), lit(Value::Int64(7)))),
                                    Box::new(cmp(prop("d", "p8"), lit(Value::Int64(8)))),
                                ))),
                            ))),
                        ))),
                    ))),
                ))),
            ))),
        ));
        validate_search_filter(&filter, "d")
            .expect("eight equality arms on distinct properties should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_nine_arm_conjunction() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "p1"), lit(Value::Int64(1)))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "p2"), lit(Value::Int64(2)))),
                Box::new(Expr::new(ExprKind::And(
                    Box::new(cmp(prop("d", "p3"), lit(Value::Int64(3)))),
                    Box::new(Expr::new(ExprKind::And(
                        Box::new(cmp(prop("d", "p4"), lit(Value::Int64(4)))),
                        Box::new(Expr::new(ExprKind::And(
                            Box::new(cmp(prop("d", "p5"), lit(Value::Int64(5)))),
                            Box::new(Expr::new(ExprKind::And(
                                Box::new(cmp(prop("d", "p6"), lit(Value::Int64(6)))),
                                Box::new(Expr::new(ExprKind::And(
                                    Box::new(cmp(prop("d", "p7"), lit(Value::Int64(7)))),
                                    Box::new(Expr::new(ExprKind::And(
                                        Box::new(cmp(prop("d", "p8"), lit(Value::Int64(8)))),
                                        Box::new(cmp(prop("d", "p9"), lit(Value::Int64(9)))),
                                    ))),
                                ))),
                            ))),
                        ))),
                    ))),
                ))),
            ))),
        ));
        validate_search_filter(&filter, "d")
            .expect("nine equality arms on distinct properties should be accepted by the provider-neutral planner");
    }

    #[test]
    fn validate_search_filter_accepts_three_arm_conjunction() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "a"), lit(Value::Int64(1)))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "b"), lit(Value::Int64(2)))),
                Box::new(cmp(prop("d", "c"), lit(Value::Int64(3)))),
            ))),
        ));
        validate_search_filter(&filter, "d")
            .expect("three equality arms on distinct properties should be accepted");
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
    fn validate_search_filter_accepts_two_sided_numeric_range() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Lt,
                lit(Value::Int64(10)),
            )),
        ));
        validate_search_filter(&filter, "d")
            .expect("two-sided range on same property should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_two_sided_range_all_endpoint_combinations() {
        let combos = [
            (gleaph_gql::ast::CmpOp::Ge, gleaph_gql::ast::CmpOp::Lt),
            (gleaph_gql::ast::CmpOp::Ge, gleaph_gql::ast::CmpOp::Le),
            (gleaph_gql::ast::CmpOp::Gt, gleaph_gql::ast::CmpOp::Lt),
            (gleaph_gql::ast::CmpOp::Gt, gleaph_gql::ast::CmpOp::Le),
        ];
        for (lower, upper) in combos {
            let filter = Expr::new(ExprKind::And(
                Box::new(cmp_op(prop("d", "price"), lower, lit(Value::Int64(5)))),
                Box::new(cmp_op(prop("d", "price"), upper, lit(Value::Int64(10)))),
            ));
            validate_search_filter(&filter, "d")
                .unwrap_or_else(|e| panic!("{lower:?}/{upper:?} should be accepted: {e}"));
        }
    }

    #[test]
    fn validate_search_filter_accepts_two_sided_range_reversed_conjunct_order() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Lt,
                lit(Value::Int64(10)),
            )),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
        ));
        validate_search_filter(&filter, "d").expect("upper-then-lower order should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_two_sided_range_reversed_operands() {
        // 10 > d.price >= 5 normalizes to d.price < 10 and d.price >= 5.
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                lit(Value::Int64(10)),
                gleaph_gql::ast::CmpOp::Gt,
                prop("d", "price"),
            )),
            Box::new(cmp_op(
                lit(Value::Int64(5)),
                gleaph_gql::ast::CmpOp::Le,
                prop("d", "price"),
            )),
        ));
        validate_search_filter(&filter, "d").expect("reversed operands should be accepted");
    }

    #[test]
    fn validate_search_filter_rejects_two_range_conjuncts_on_different_properties() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(cmp_op(
                prop("d", "score"),
                gleaph_gql::ast::CmpOp::Lt,
                lit(Value::Int64(10)),
            )),
        ));
        let err = validate_search_filter(&filter, "d")
            .expect_err("different-property ranges should be rejected");
        assert!(err.to_string().contains("same property"));
    }

    #[test]
    fn validate_search_filter_rejects_two_lower_bounds() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Gt,
                lit(Value::Int64(2)),
            )),
        ));
        let err = validate_search_filter(&filter, "d").expect_err("two lower bounds must fail");
        assert!(err.to_string().contains("lower bound"));
    }

    #[test]
    fn validate_search_filter_rejects_two_upper_bounds() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Lt,
                lit(Value::Int64(10)),
            )),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Le,
                lit(Value::Int64(8)),
            )),
        ));
        let err = validate_search_filter(&filter, "d").expect_err("two upper bounds must fail");
        assert!(err.to_string().contains("upper bound"));
    }

    #[test]
    fn validate_search_filter_rejects_three_predicate_range() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Lt,
                    lit(Value::Int64(10)),
                )),
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Le,
                    lit(Value::Int64(8)),
                )),
            ))),
        ));
        let err = validate_search_filter(&filter, "d").expect_err("three range arms must fail");
        assert!(
            err.to_string()
                .contains("SEARCH ... WHERE supports at most two range predicates")
        );
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_and_range_distinct_properties() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
        ));
        validate_search_filter(&filter, "d")
            .expect("mixed equality/range on distinct properties should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_and_range_reversed_conjunct_order() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
        ));
        validate_search_filter(&filter, "d").expect("range-then-equality order should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_and_range_parameter_values() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "category"), param("$category"))),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                param("$minimum_price"),
            )),
        ));
        validate_search_filter(&filter, "d")
            .expect("parameter values in mixed arms should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_and_range_reversed_operands() {
        // 5 <= d.price normalizes to d.price >= 5; equality order is unchanged.
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                lit(Value::Int64(5)),
                gleaph_gql::ast::CmpOp::Le,
                prop("d", "price"),
            )),
            Box::new(cmp(lit(Value::Text("doc".into())), prop("d", "category"))),
        ));
        validate_search_filter(&filter, "d")
            .expect("reversed operands in mixed arms should be accepted");
    }

    #[test]
    fn validate_search_filter_rejects_mixed_equality_and_range_same_property() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "price"), lit(Value::Int64(5)))),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(10)),
            )),
        ));
        let err = validate_search_filter(&filter, "d")
            .expect_err("same-property mixed equality/range should be rejected");
        assert!(err.to_string().contains("distinct properties"));
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_plus_two_sided_range() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Ge,
                    lit(Value::Int64(5)),
                )),
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Lt,
                    lit(Value::Int64(10)),
                )),
            ))),
        ));
        validate_search_filter(&filter, "d")
            .expect("equality plus two-sided range on distinct properties should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_plus_two_sided_range_all_endpoints() {
        let combos = [
            (gleaph_gql::ast::CmpOp::Ge, gleaph_gql::ast::CmpOp::Lt),
            (gleaph_gql::ast::CmpOp::Ge, gleaph_gql::ast::CmpOp::Le),
            (gleaph_gql::ast::CmpOp::Gt, gleaph_gql::ast::CmpOp::Lt),
            (gleaph_gql::ast::CmpOp::Gt, gleaph_gql::ast::CmpOp::Le),
        ];
        for (lower, upper) in combos {
            let filter = Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
                Box::new(Expr::new(ExprKind::And(
                    Box::new(cmp_op(prop("d", "price"), lower, lit(Value::Int64(5)))),
                    Box::new(cmp_op(prop("d", "price"), upper, lit(Value::Int64(10)))),
                ))),
            ));
            validate_search_filter(&filter, "d")
                .unwrap_or_else(|e| panic!("{lower:?}/{upper:?} should be accepted: {e}"));
        }
    }

    #[test]
    fn validate_search_filter_accepts_mixed_equality_plus_two_sided_range_reversed_order() {
        // Equality can appear anywhere and operands may be reversed.
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                lit(Value::Int64(10)),
                gleaph_gql::ast::CmpOp::Gt,
                prop("d", "price"),
            )),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Ge,
                    lit(Value::Int64(5)),
                )),
                Box::new(cmp(lit(Value::Text("doc".into())), prop("d", "category"))),
            ))),
        ));
        validate_search_filter(&filter, "d")
            .expect("reversed order and operands for Slice 12 should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_two_equalities_plus_range() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "category"), lit(Value::Text("doc".into())))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "tenant"), lit(Value::Int64(7)))),
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Ge,
                    lit(Value::Int64(5)),
                )),
            ))),
        ));
        validate_search_filter(&filter, "d")
            .expect("two equalities plus range on distinct properties should be accepted");
    }

    #[test]
    fn validate_search_filter_accepts_four_equalities_plus_one_sided_range() {
        let mut filter = cmp_op(
            prop("d", "price"),
            gleaph_gql::ast::CmpOp::Ge,
            lit(Value::Int64(5)),
        );
        for prop_name in ["a", "b", "c", "d"] {
            filter = Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", prop_name), lit(Value::Text("ok".into())))),
                Box::new(filter),
            ));
        }
        validate_search_filter(&filter, "d")
            .expect("four equalities plus one range on distinct properties should be accepted");
    }

    #[test]
    fn validate_search_filter_rejects_four_equalities_plus_three_ranges() {
        let mut filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Lt,
                    lit(Value::Int64(10)),
                )),
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Le,
                    lit(Value::Int64(8)),
                )),
            ))),
        ));
        for prop_name in ["a", "b", "c", "d"] {
            filter = Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", prop_name), lit(Value::Text("ok".into())))),
                Box::new(filter),
            ));
        }
        let err = validate_search_filter(&filter, "d")
            .expect_err("three range arms in mixed filter must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("at most two range predicates")
                || msg.contains("at most one lower and one upper bound")
                || msg.contains("does not support this equality/range mixture"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_search_filter_accepts_eight_equalities_plus_two_sided_range() {
        let mut filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Lt,
                lit(Value::Int64(10)),
            )),
        ));
        for i in 1..=8u8 {
            filter = Expr::new(ExprKind::And(
                Box::new(cmp(
                    prop("d", &format!("p{i}")),
                    lit(Value::Int64(i as i64)),
                )),
                Box::new(filter),
            ));
        }
        validate_search_filter(&filter, "d").expect(
            "eight equalities plus two-sided range on distinct properties should be accepted",
        );
    }

    #[test]
    fn validate_search_filter_rejects_eight_equalities_plus_range_with_duplicate_equality_property()
    {
        let mut filter = cmp_op(
            prop("d", "price"),
            gleaph_gql::ast::CmpOp::Ge,
            lit(Value::Int64(5)),
        );
        for i in 1..=8u8 {
            let name = if i == 8 {
                "p1".to_string()
            } else {
                format!("p{i}")
            };
            filter = Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", &name), lit(Value::Int64(i as i64)))),
                Box::new(filter),
            ));
        }
        let err = validate_search_filter(&filter, "d")
            .expect_err("duplicate equality properties in mixed filter must fail");
        assert!(err.to_string().contains("distinct properties"));
    }

    #[test]
    fn validate_search_filter_rejects_four_equalities_plus_two_ranges_on_different_properties() {
        let mut filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(5)),
            )),
            Box::new(cmp_op(
                prop("d", "score"),
                gleaph_gql::ast::CmpOp::Lt,
                lit(Value::Int64(10)),
            )),
        ));
        for prop_name in ["a", "b", "c", "d"] {
            filter = Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", prop_name), lit(Value::Text("ok".into())))),
                Box::new(filter),
            ));
        }
        let err = validate_search_filter(&filter, "d")
            .expect_err("two ranges on different properties must fail");
        assert!(err.to_string().contains("same property"));
    }

    #[test]
    fn validate_search_filter_accepts_three_equalities() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "a"), lit(Value::Int64(1)))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp(prop("d", "b"), lit(Value::Int64(2)))),
                Box::new(cmp(prop("d", "c"), lit(Value::Int64(3)))),
            ))),
        ));
        validate_search_filter(&filter, "d").expect("three equalities should be accepted");
    }

    #[test]
    fn validate_search_filter_rejects_three_ranges() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp_op(
                prop("d", "price"),
                gleaph_gql::ast::CmpOp::Ge,
                lit(Value::Int64(1)),
            )),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Lt,
                    lit(Value::Int64(10)),
                )),
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Le,
                    lit(Value::Int64(8)),
                )),
            ))),
        ));
        let err = validate_search_filter(&filter, "d").expect_err("three range arms must fail");
        assert!(
            err.to_string()
                .contains("SEARCH ... WHERE supports at most two range predicates")
        );
    }

    #[test]
    fn validate_search_filter_rejects_mixed_equality_plus_two_sided_range_same_property() {
        let filter = Expr::new(ExprKind::And(
            Box::new(cmp(prop("d", "price"), lit(Value::Int64(5)))),
            Box::new(Expr::new(ExprKind::And(
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Ge,
                    lit(Value::Int64(5)),
                )),
                Box::new(cmp_op(
                    prop("d", "price"),
                    gleaph_gql::ast::CmpOp::Lt,
                    lit(Value::Int64(10)),
                )),
            ))),
        ));
        let err = validate_search_filter(&filter, "d")
            .expect_err("same-property equality/range must fail");
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

    #[test]
    fn validate_search_filter_rejects_non_range_non_equality_operators() {
        let filter = cmp_op(
            prop("d", "category"),
            gleaph_gql::ast::CmpOp::Ne,
            lit(Value::Text("doc".into())),
        );
        let err = validate_search_filter(&filter, "d").expect_err("!= must be rejected");
        assert!(err.to_string().contains("single numeric range predicate"));
    }
}
