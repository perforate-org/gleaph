use gleaph_gql::ast::*;

use super::super::result::flatten_conjunction;
use crate::anchor::{self};
use crate::plan::*;
use crate::stats::GraphStats;

/// Collect all inline predicates from a node pattern (properties + WHERE clause)
/// without emitting them as PlanOps. Used by FilterIntoPattern to fuse into ExpandFilter.
pub(super) fn collect_node_inline_predicates(var: &str, node: &NodePattern) -> Vec<Expr> {
    let mut preds = Vec::new();

    for p in &node.properties {
        preds.push(Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable(var.to_string()))),
                property: p.name.clone(),
            })),
            op: CmpOp::Eq,
            right: Box::new(p.value.clone()),
        }));
    }

    if let Some(where_expr) = &node.where_clause {
        preds.extend(flatten_conjunction(where_expr));
    }

    preds
}

pub(super) fn quantifier_to_var_len(q: &PathQuantifier) -> Option<VarLenSpec> {
    match q {
        PathQuantifier::Star => Some(VarLenSpec { min: 0, max: None }),
        PathQuantifier::Plus => Some(VarLenSpec { min: 1, max: None }),
        PathQuantifier::Optional => Some(VarLenSpec {
            min: 0,
            max: Some(1),
        }),
        PathQuantifier::Fixed(n) => Some(VarLenSpec {
            min: *n,
            max: Some(*n),
        }),
        PathQuantifier::Range { lower, upper } => Some(VarLenSpec {
            min: *lower,
            max: *upper,
        }),
    }
}

pub(super) fn emit_bound_node_pattern_checks(
    var: &str,
    node: &NodePattern,
    require_non_null: bool,
    ops: &mut Vec<PlanOp>,
) {
    let mut predicates = Vec::new();
    if require_non_null {
        predicates.push(Expr::new(ExprKind::IsNotNull(Box::new(Expr::var(var)))));
    }
    if let Some(label) = &node.label {
        predicates.push(Expr::new(ExprKind::IsLabeled {
            expr: Box::new(Expr::var(var)),
            label: label.clone(),
            negated: false,
        }));
    }
    if !predicates.is_empty() {
        ops.push(PlanOp::PropertyFilter {
            predicates,
            stage: 0,
        });
    }
}

pub(super) fn emit_node_inline_filters(var: &str, node: &NodePattern, ops: &mut Vec<PlanOp>) {
    if !node.properties.is_empty() {
        let filter_exprs: Vec<Expr> = node
            .properties
            .iter()
            .map(|p| {
                Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable(var.to_string()))),
                        property: p.name.clone(),
                    })),
                    op: CmpOp::Eq,
                    right: Box::new(p.value.clone()),
                })
            })
            .collect();
        ops.push(PlanOp::PropertyFilter {
            predicates: filter_exprs,
            stage: 0,
        });
    }

    if let Some(where_expr) = &node.where_clause {
        ops.push(PlanOp::PropertyFilter {
            predicates: flatten_conjunction(where_expr),
            stage: 0,
        });
    }
}

/// Planner-only: indexed edge equality plus residual edge filters.
#[derive(Default, Clone)]
pub(super) struct EdgeFilterFusion {
    pub(super) indexed_equality: Option<(Str, ScanValue)>,
    pub(super) edge_payload_predicate: Option<EdgePayloadPredicate>,
    pub(super) edge_vector_predicate: Option<EdgeVectorPredicate>,
    pub(super) skip_inline_prop: Option<String>,
    /// `None`: emit full `edge.where_clause`. `Some(predicates)` emits only these (empty = omit).
    pub(super) edge_where_override: Option<Vec<Expr>>,
}

pub(super) fn plan_edge_filter_fusion(
    edge_var: &str,
    edge: &EdgePattern,
    stats: Option<&dyn GraphStats>,
    allow_edge_payload_predicate: bool,
    where_conjuncts: &mut Vec<Expr>,
) -> EdgeFilterFusion {
    let mut out = EdgeFilterFusion::default();
    if allow_edge_payload_predicate
        && let Some((idx, pred)) =
            find_first_edge_vector_predicate_in_conjunctions(where_conjuncts, edge_var)
    {
        where_conjuncts.remove(idx);
        out.edge_vector_predicate = Some(pred);
        return out;
    }
    if allow_edge_payload_predicate && let Some(where_clause) = edge.where_clause.as_ref() {
        let mut conj = flatten_conjunction(where_clause);
        if let Some((idx, pred)) = find_first_edge_vector_predicate_in_conjunctions(&conj, edge_var)
        {
            conj.remove(idx);
            out.edge_vector_predicate = Some(pred);
            out.edge_where_override = Some(conj);
            return out;
        }
    }

    if allow_edge_payload_predicate
        && let Some((idx, pred)) =
            find_first_edge_payload_predicate_in_conjunctions(where_conjuncts, edge_var)
    {
        where_conjuncts.remove(idx);
        out.edge_payload_predicate = Some(pred);
        return out;
    }
    if allow_edge_payload_predicate && let Some(where_clause) = edge.where_clause.as_ref() {
        let mut conj = flatten_conjunction(where_clause);
        if let Some((idx, pred)) =
            find_first_edge_payload_predicate_in_conjunctions(&conj, edge_var)
        {
            conj.remove(idx);
            out.edge_payload_predicate = Some(pred);
            out.edge_where_override = Some(conj);
            return out;
        }
    }

    let Some(stats) = stats else {
        return out;
    };

    for p in &edge.properties {
        if stats.is_edge_property_indexed(&p.name)
            && let Some(sv) = anchor::scan_value_from_expr(&p.value)
        {
            out.indexed_equality = Some((p.name.clone().into(), sv));
            out.skip_inline_prop = Some(p.name.clone());
            strip_edge_var_prop_eq_from_where(where_conjuncts, edge_var, &p.name);
            out.edge_where_override = edge_where_after_fusing_prop(edge, edge_var, &p.name);
            return out;
        }
    }

    if let Some((idx, prop, sv)) =
        find_first_indexed_edge_eq_in_conjunctions(where_conjuncts, edge_var, stats)
    {
        where_conjuncts.remove(idx);
        out.indexed_equality = Some((prop.into(), sv));
        return out;
    }

    if let Some(where_clause) = edge.where_clause.as_ref() {
        let mut conj = flatten_conjunction(where_clause);
        if let Some((idx, prop, sv)) =
            find_first_indexed_edge_eq_in_conjunctions(&conj, edge_var, stats)
        {
            conj.remove(idx);
            out.indexed_equality = Some((prop.into(), sv));
            out.edge_where_override = Some(conj);
        }
    }

    out
}

fn find_first_edge_payload_predicate_in_conjunctions(
    conjuncts: &[Expr],
    edge_var: &str,
) -> Option<(usize, EdgePayloadPredicate)> {
    for (i, c) in conjuncts.iter().enumerate() {
        if let Some((v, pred)) = parse_gleaph_weight_predicate(c)
            && v == edge_var
        {
            return Some((i, pred));
        }
    }
    None
}

fn find_first_edge_vector_predicate_in_conjunctions(
    conjuncts: &[Expr],
    edge_var: &str,
) -> Option<(usize, EdgeVectorPredicate)> {
    for (i, c) in conjuncts.iter().enumerate() {
        if let Some((v, pred)) = parse_gleaph_vector_predicate(c)
            && v == edge_var
        {
            return Some((i, pred));
        }
    }
    None
}

fn parse_gleaph_vector_predicate(expr: &Expr) -> Option<(String, EdgeVectorPredicate)> {
    let ExprKind::Compare { left, op, right } = &expr.kind else {
        return None;
    };
    if let Some((edge_var, metric, query)) = gleaph_vector_call(left) {
        let threshold = anchor::scan_value_from_expr(right)?;
        if vector_metric_accepts_cmp(metric, *op) {
            return Some((
                edge_var,
                EdgeVectorPredicate {
                    metric,
                    query,
                    op: *op,
                    threshold,
                },
            ));
        }
    }
    if let Some((edge_var, metric, query)) = gleaph_vector_call(right) {
        let flipped = flip_cmp_op(*op)?;
        let threshold = anchor::scan_value_from_expr(left)?;
        if vector_metric_accepts_cmp(metric, flipped) {
            return Some((
                edge_var,
                EdgeVectorPredicate {
                    metric,
                    query,
                    op: flipped,
                    threshold,
                },
            ));
        }
    }
    None
}

fn vector_metric_accepts_cmp(metric: EdgeVectorMetric, op: CmpOp) -> bool {
    match metric {
        EdgeVectorMetric::L2Squared | EdgeVectorMetric::CosineDistance => {
            matches!(op, CmpOp::Lt | CmpOp::Le)
        }
        EdgeVectorMetric::Dot => matches!(op, CmpOp::Gt | CmpOp::Ge),
    }
}

fn parse_gleaph_weight_predicate(expr: &Expr) -> Option<(String, EdgePayloadPredicate)> {
    let ExprKind::Compare { left, op, right } = &expr.kind else {
        return None;
    };
    if let Some(edge_var) = gleaph_weight_edge_var(left) {
        return anchor::scan_value_from_expr(right)
            .map(|value| (edge_var, EdgePayloadPredicate { op: *op, value }));
    }
    if let Some(edge_var) = gleaph_weight_edge_var(right) {
        let flipped = flip_cmp_op(*op)?;
        return anchor::scan_value_from_expr(left)
            .map(|value| (edge_var, EdgePayloadPredicate { op: flipped, value }));
    }
    None
}

fn gleaph_vector_call(expr: &Expr) -> Option<(String, EdgeVectorMetric, ScanValue)> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
    else {
        return None;
    };
    if *distinct
        || name.parts.len() != 3
        || !name.parts[0].eq_ignore_ascii_case("gleaph")
        || !name.parts[1].eq_ignore_ascii_case("vector")
        || args.len() != 2
    {
        return None;
    }
    let metric = if name.parts[2].eq_ignore_ascii_case("l2_squared") {
        EdgeVectorMetric::L2Squared
    } else if name.parts[2].eq_ignore_ascii_case("cosine_distance") {
        EdgeVectorMetric::CosineDistance
    } else if name.parts[2].eq_ignore_ascii_case("dot") {
        EdgeVectorMetric::Dot
    } else {
        return None;
    };
    let edge_var = edge_var_from_expr(&args[0])?;
    let query = anchor::scan_value_from_expr(&args[1])?;
    Some((edge_var, metric, query))
}

fn edge_var_from_expr(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Variable(v) => Some(v.clone()),
        ExprKind::Paren(inner) => edge_var_from_expr(inner),
        _ => None,
    }
}

fn flip_cmp_op(op: CmpOp) -> Option<CmpOp> {
    Some(match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    })
}

fn gleaph_weight_edge_var(expr: &Expr) -> Option<String> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
    else {
        return None;
    };
    if *distinct
        || name.parts.len() != 2
        || !name.parts[0].eq_ignore_ascii_case("gleaph")
        || !name.parts[1].eq_ignore_ascii_case("weight")
        || args.len() != 1
    {
        return None;
    }
    match &args[0].kind {
        ExprKind::Variable(v) => Some(v.clone()),
        ExprKind::Paren(inner) => gleaph_weight_edge_var(inner),
        _ => None,
    }
}

fn find_first_indexed_edge_eq_in_conjunctions(
    conjuncts: &[Expr],
    edge_var: &str,
    stats: &dyn GraphStats,
) -> Option<(usize, String, ScanValue)> {
    for (i, c) in conjuncts.iter().enumerate() {
        if let Some((v, p, sv)) = parse_edge_var_property_equality(c)
            && v == edge_var
            && stats.is_edge_property_indexed(&p)
        {
            return Some((i, p, sv));
        }
    }
    None
}

pub(super) fn parse_edge_var_property_equality(expr: &Expr) -> Option<(String, String, ScanValue)> {
    if let ExprKind::Compare { left, op, right } = &expr.kind
        && *op == CmpOp::Eq
        && let ExprKind::PropertyAccess {
            expr: inner,
            property,
        } = &left.kind
        && let ExprKind::Variable(v) = &inner.kind
    {
        return anchor::scan_value_from_expr(right).map(|sv| (v.clone(), property.clone(), sv));
    }
    None
}

fn strip_edge_var_prop_eq_from_where(where_conjuncts: &mut Vec<Expr>, edge_var: &str, prop: &str) {
    where_conjuncts.retain(|c| {
        !parse_edge_var_property_equality(c).is_some_and(|(v, p, _)| v == edge_var && p == prop)
    });
}

fn edge_where_after_fusing_prop(
    edge: &EdgePattern,
    edge_var: &str,
    fused_prop: &str,
) -> Option<Vec<Expr>> {
    edge.where_clause.as_ref()?;
    let mut conj = flatten_conjunction(edge.where_clause.as_ref().unwrap());
    let orig_len = conj.len();
    conj.retain(|c| {
        !parse_edge_var_property_equality(c)
            .is_some_and(|(v, p, _)| v == edge_var && p == fused_prop)
    });
    if conj.len() == orig_len {
        None
    } else {
        Some(conj)
    }
}

pub(super) fn emit_edge_inline_filters(
    edge_var: &str,
    edge: &EdgePattern,
    fusion: &EdgeFilterFusion,
    ops: &mut Vec<PlanOp>,
) {
    let filter_exprs: Vec<Expr> = edge
        .properties
        .iter()
        .filter(|p| fusion.skip_inline_prop.as_deref() != Some(p.name.as_str()))
        .map(|p| {
            Expr::new(ExprKind::Compare {
                left: Box::new(Expr::new(ExprKind::PropertyAccess {
                    expr: Box::new(Expr::new(ExprKind::Variable(edge_var.to_string()))),
                    property: p.name.clone(),
                })),
                op: CmpOp::Eq,
                right: Box::new(p.value.clone()),
            })
        })
        .collect();
    if !filter_exprs.is_empty() {
        ops.push(PlanOp::PropertyFilter {
            predicates: filter_exprs,
            stage: 0,
        });
    }

    match &fusion.edge_where_override {
        None => {
            if let Some(where_expr) = &edge.where_clause {
                ops.push(PlanOp::PropertyFilter {
                    predicates: flatten_conjunction(where_expr),
                    stage: 0,
                });
            }
        }
        Some(preds) if !preds.is_empty() => {
            ops.push(PlanOp::PropertyFilter {
                predicates: preds.clone(),
                stage: 0,
            });
        }
        Some(_) => {}
    }
}

pub(super) fn emit_scan_for_node(
    var: &str,
    label: &Option<String>,
    node: &NodePattern,
    stats: Option<&dyn GraphStats>,
    conditional_candidates: &[ConditionalScanCandidate],
    ops: &mut Vec<PlanOp>,
    annotations: &mut PlanAnnotations,
) {
    // Check for index intersection opportunity (multiple indexed predicates).
    if let Some(stats) = stats
        && let Some(where_expr) = &node.where_clause
        && let Some(specs) = anchor::find_index_intersection(var, where_expr, stats)
    {
        ops.push(PlanOp::IndexIntersection {
            variable: Str::from(var),
            scans: specs,
            property_projection: None,
        });
        return;
    }

    // Check if anchor selection found an index scan for this variable.
    if let Some(anchor) = &annotations.optimizer.anchor
        && &*anchor.variable == var
    {
        match &anchor.source {
            AnchorSource::PropertyEquality { property }
            | AnchorSource::InlinePropertyEquality { property } => {
                // Find the value from inline properties or inline WHERE.
                let scan_value = node
                    .properties
                    .iter()
                    .find(|p| p.name == **property)
                    .map(|p| expr_to_scan_value(&p.value))
                    .or_else(|| {
                        // Try inline WHERE: (n WHERE n.prop = value)
                        node.where_clause
                            .as_ref()
                            .and_then(|w| find_equality_value_in_where(var, property, w))
                    })
                    .unwrap_or(ScanValue::Parameter(format!("${}", property).into()));

                ops.push(PlanOp::IndexScan {
                    variable: Str::from(var),
                    property: property.clone(),
                    value: scan_value,
                    cmp: CmpOp::Eq,
                    property_projection: None,
                });
                return;
            }
            AnchorSource::PropertyRange {
                property,
                value,
                cmp,
            } => {
                ops.push(PlanOp::IndexScan {
                    variable: Str::from(var),
                    property: property.clone(),
                    value: value.clone(),
                    cmp: *cmp,
                    property_projection: None,
                });
                return;
            }
            _ => {}
        }
    }

    // Check for conditional index scan candidates.
    let var_candidates: Vec<_> = conditional_candidates
        .iter()
        .filter(|c| &*c.variable == var)
        .cloned()
        .collect();
    if !var_candidates.is_empty() {
        ops.push(PlanOp::ConditionalIndexScan {
            candidates: var_candidates,
            fallback_label: label.as_ref().map(|s| Str::from(s.as_str())),
            fallback_variable: Str::from(var),
            property_projection: None,
        });
        return;
    }

    // Default: NodeScan.
    ops.push(PlanOp::NodeScan {
        variable: Str::from(var),
        label: label.as_ref().map(|s| Str::from(s.as_str())),
        property_projection: None,
    });
}

fn expr_to_scan_value(expr: &Expr) -> ScanValue {
    match &expr.kind {
        ExprKind::Literal(v) => ScanValue::Literal(v.clone()),
        ExprKind::Parameter(p) => ScanValue::Parameter(p.clone().into()),
        _ => ScanValue::Parameter(Str::from("?")),
    }
}

/// Find the value for `var.property = <value>` in an inline WHERE clause.
fn find_equality_value_in_where(var: &str, property: &str, where_expr: &Expr) -> Option<ScanValue> {
    let conjuncts = flatten_conjunction(where_expr);
    for conjunct in &conjuncts {
        if let ExprKind::Compare { left, op, right } = &conjunct.kind
            && *op == CmpOp::Eq
            && let ExprKind::PropertyAccess {
                expr: inner,
                property: prop,
            } = &left.kind
            && let ExprKind::Variable(v) = &inner.kind
            && v == var
            && prop == property
        {
            return Some(expr_to_scan_value(right));
        }
    }
    None
}
