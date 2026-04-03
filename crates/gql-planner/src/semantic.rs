//! Semantic analysis for GQL queries.
//!
//! Extracts structured facts from the AST that inform planner decisions:
//! - Property accesses (for index scan candidacy)
//! - Equality/range predicates in WHERE (for anchor selection)
//! - Inline node properties (for index scan fallback)
//! - Aggregate function usage
//! - Optional filter patterns (`$param IS NULL OR var.prop = $param`)
//! - Narrowing facts (flow-sensitive type refinement)

use gleaph_gql::ast::*;
use gleaph_gql::types::LabelExpr;

// ════════════════════════════════════════════════════════════════════════════════
// Core types
// ════════════════════════════════════════════════════════════════════════════════

/// Result of semantic analysis on a linear query.
#[derive(Clone, Debug, Default)]
pub struct SemanticAnalysis {
    /// Structured constraints extracted from the query.
    pub constraints: Vec<SemanticConstraint>,
    /// Flow-sensitive narrowing facts from WHERE predicates.
    pub narrowing_facts: Vec<NarrowingFact>,
}

/// A semantic constraint extracted from the query AST.
#[derive(Clone, Debug)]
pub enum SemanticConstraint {
    /// A property access: `var.property`
    PropertyAccess {
        var: String,
        property: String,
        /// Whether this access is in a WHERE clause.
        in_where: bool,
    },

    /// Equality predicate in WHERE: `var.property = literal_or_param`
    WhereEqualityPredicate {
        var: String,
        property: String,
        /// The right-hand value (literal or parameter name).
        value: PredicateValue,
    },

    /// Range predicate in WHERE: `var.property <op> literal_or_param`
    WhereRangePredicate {
        var: String,
        property: String,
        op: CmpOp,
        value: PredicateValue,
    },

    /// Inline property from node pattern: `(n:Label {prop: value})`
    InlineNodeProperty {
        var: String,
        property: String,
        value: PredicateValue,
    },

    /// Inline WHERE from node pattern: `(n:Label WHERE n.prop = value)`
    InlineNodeWherePredicate {
        var: String,
        property: String,
        value: PredicateValue,
    },

    /// Optional filter pattern: `$param IS NULL OR var.prop <op> $param`
    OptionalFilterPredicate {
        param_name: String,
        var: String,
        property: String,
        op: CmpOp,
    },

    /// Aggregate function call (COUNT, SUM, etc.)
    AggregateCall { func: String },
}

/// The right-hand value in a predicate.
#[derive(Clone, Debug)]
pub enum PredicateValue {
    Literal(gleaph_gql::Value),
    Parameter(String),
}

/// A flow-sensitive narrowing fact from WHERE predicates.
#[derive(Clone, Debug)]
pub enum NarrowingFact {
    /// `var.property IS NOT NULL` — property is known non-null.
    PropertyNonNull { var: String, property: String },
    /// `var IS LABELED label` — node has the given label.
    LabelNarrowed { var: String, label: String },
    /// `type(var) = 'LABEL'` — edge has the given label.
    EdgeLabelNarrowed { var: String, label: String },
}

// ════════════════════════════════════════════════════════════════════════════════
// Analysis entry point
// ════════════════════════════════════════════════════════════════════════════════

/// Perform semantic analysis on a linear query statement.
pub fn analyze(query: &LinearQueryStatement) -> SemanticAnalysis {
    let mut analysis = SemanticAnalysis::default();

    for part in &query.parts {
        analyze_simple_statement(part, &mut analysis);
    }

    if let Some(result) = &query.result {
        analyze_result_statement(result, &mut analysis);
    }

    analysis
}

// ════════════════════════════════════════════════════════════════════════════════
// Internal analysis
// ════════════════════════════════════════════════════════════════════════════════

fn analyze_simple_statement(stmt: &SimpleQueryStatement, analysis: &mut SemanticAnalysis) {
    match stmt {
        SimpleQueryStatement::Match(m) => analyze_match(m, analysis),
        SimpleQueryStatement::Filter(f) => {
            collect_where_predicates(&f.condition, analysis);
            collect_narrowing_facts(&f.condition, analysis);
        }
        SimpleQueryStatement::Let(l) => {
            for binding in &l.bindings {
                collect_expr_property_accesses(&binding.value, false, analysis);
            }
        }
        SimpleQueryStatement::For(f) => {
            collect_expr_property_accesses(&f.list, false, analysis);
        }
        _ => {}
    }
}

fn analyze_match(match_stmt: &MatchStatement, analysis: &mut SemanticAnalysis) {
    let pattern = &match_stmt.pattern;

    // Analyze path patterns for inline properties.
    for path in &pattern.paths {
        analyze_path_pattern(&path.expr, analysis);
    }

    // Analyze WHERE clause.
    if let Some(where_expr) = &pattern.where_clause {
        collect_where_predicates(where_expr, analysis);
        collect_narrowing_facts(where_expr, analysis);
    }
}

fn analyze_path_pattern(expr: &PathPatternExpr, analysis: &mut SemanticAnalysis) {
    match expr {
        PathPatternExpr::Term(term) => {
            for factor in &term.factors {
                analyze_path_primary(&factor.primary, analysis);
            }
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                for factor in &term.factors {
                    analyze_path_primary(&factor.primary, analysis);
                }
            }
        }
    }
}

fn analyze_path_primary(primary: &PathPrimary, analysis: &mut SemanticAnalysis) {
    match primary {
        PathPrimary::Node(node) => {
            if let Some(var) = &node.variable {
                // Inline properties: (n:Label {prop: value})
                for prop in &node.properties {
                    let value = expr_to_predicate_value(&prop.value);
                    analysis
                        .constraints
                        .push(SemanticConstraint::InlineNodeProperty {
                            var: var.clone(),
                            property: prop.name.clone(),
                            value,
                        });
                }

                // Inline WHERE: (n:Label WHERE n.prop = value)
                if let Some(where_expr) = &node.where_clause {
                    collect_inline_where_predicates(var, where_expr, analysis);
                    collect_narrowing_facts(where_expr, analysis);
                }
            }
        }
        PathPrimary::Edge(edge) => {
            if let Some(var) = &edge.variable {
                for prop in &edge.properties {
                    let value = expr_to_predicate_value(&prop.value);
                    analysis
                        .constraints
                        .push(SemanticConstraint::InlineNodeProperty {
                            var: var.clone(),
                            property: prop.name.clone(),
                            value,
                        });
                }
                if let Some(where_expr) = &edge.where_clause {
                    collect_inline_where_predicates(var, where_expr, analysis);
                }
            }
        }
        PathPrimary::Parenthesized {
            expr, where_clause, ..
        } => {
            analyze_path_pattern(expr, analysis);
            if let Some(w) = where_clause {
                collect_where_predicates(w, analysis);
                collect_narrowing_facts(w, analysis);
            }
        }
        PathPrimary::Simplified(_) => {}
    }
}

fn analyze_result_statement(result: &ResultStatement, analysis: &mut SemanticAnalysis) {
    match result {
        ResultStatement::Return(ret) => {
            if let ReturnBody::Items {
                items, group_by, ..
            } = &ret.body
            {
                for item in items {
                    collect_expr_property_accesses(&item.expr, false, analysis);
                    collect_aggregate_calls(&item.expr, analysis);
                }
                if group_by.is_some() {
                    // GROUP BY presence implies aggregation context.
                }
            }
        }
        ResultStatement::Select(sel) => {
            if let SelectBody::Items { items, .. } = &sel.body {
                for item in items {
                    collect_expr_property_accesses(&item.expr, false, analysis);
                    collect_aggregate_calls(&item.expr, analysis);
                }
            }
        }
        ResultStatement::Finish => {}
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Predicate extraction
// ════════════════════════════════════════════════════════════════════════════════

/// Extract equality and range predicates from a WHERE clause.
fn collect_where_predicates(expr: &Expr, analysis: &mut SemanticAnalysis) {
    let conjuncts = flatten_conjunction(expr);
    for conjunct in &conjuncts {
        // Check for optional filter pattern: $param IS NULL OR var.prop <op> $param
        if let Some(opt) = detect_optional_filter(conjunct) {
            analysis.constraints.push(opt);
            continue;
        }

        if let ExprKind::Compare { left, op, right } = &conjunct.kind {
            // var.prop = value
            if let Some((var, prop)) = extract_property_access(left)
                && let Some(value) = expr_to_predicate_value_opt(right)
            {
                collect_expr_property_accesses(left, true, analysis);
                if *op == CmpOp::Eq {
                    analysis
                        .constraints
                        .push(SemanticConstraint::WhereEqualityPredicate {
                            var,
                            property: prop,
                            value,
                        });
                } else {
                    analysis
                        .constraints
                        .push(SemanticConstraint::WhereRangePredicate {
                            var,
                            property: prop,
                            op: *op,
                            value,
                        });
                }
                continue;
            }
            // value = var.prop (reversed)
            if let Some((var, prop)) = extract_property_access(right)
                && let Some(value) = expr_to_predicate_value_opt(left)
            {
                collect_expr_property_accesses(right, true, analysis);
                let reversed_op = reverse_cmp(*op);
                if reversed_op == CmpOp::Eq {
                    analysis
                        .constraints
                        .push(SemanticConstraint::WhereEqualityPredicate {
                            var,
                            property: prop,
                            value,
                        });
                } else {
                    analysis
                        .constraints
                        .push(SemanticConstraint::WhereRangePredicate {
                            var,
                            property: prop,
                            op: reversed_op,
                            value,
                        });
                }
                continue;
            }
        }

        // Collect property accesses from non-predicate expressions too.
        collect_expr_property_accesses(conjunct, true, analysis);
    }
}

/// Collect inline WHERE predicates: `(n WHERE n.prop = value)`.
fn collect_inline_where_predicates(node_var: &str, expr: &Expr, analysis: &mut SemanticAnalysis) {
    let conjuncts = flatten_conjunction(expr);
    for conjunct in &conjuncts {
        if let ExprKind::Compare { left, op, right } = &conjunct.kind
            && *op == CmpOp::Eq
            && let Some((var, prop)) = extract_property_access(left)
            && var == node_var
            && let Some(value) = expr_to_predicate_value_opt(right)
        {
            analysis
                .constraints
                .push(SemanticConstraint::InlineNodeWherePredicate {
                    var,
                    property: prop,
                    value,
                });
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Narrowing fact extraction
// ════════════════════════════════════════════════════════════════════════════════

/// Extract narrowing facts from AND-connected predicates.
fn collect_narrowing_facts(expr: &Expr, analysis: &mut SemanticAnalysis) {
    let conjuncts = flatten_conjunction(expr);
    for conjunct in &conjuncts {
        match &conjunct.kind {
            // var.prop IS NOT NULL → PropertyNonNull
            ExprKind::IsNotNull(inner) => {
                if let Some((var, prop)) = extract_property_access(inner) {
                    analysis
                        .narrowing_facts
                        .push(NarrowingFact::PropertyNonNull {
                            var,
                            property: prop,
                        });
                }
            }
            // var IS LABELED label
            ExprKind::IsLabeled {
                expr: inner,
                label,
                negated: false,
            } => {
                if let ExprKind::Variable(var) = &inner.kind
                    && let LabelExpr::Name(name) = label
                {
                    analysis.narrowing_facts.push(NarrowingFact::LabelNarrowed {
                        var: var.clone(),
                        label: name.clone(),
                    });
                }
            }
            _ => {}
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Aggregate detection
// ════════════════════════════════════════════════════════════════════════════════

fn collect_aggregate_calls(expr: &Expr, analysis: &mut SemanticAnalysis) {
    match &expr.kind {
        ExprKind::Aggregate { func, .. } => {
            analysis
                .constraints
                .push(SemanticConstraint::AggregateCall {
                    func: format!("{:?}", func),
                });
        }
        _ => walk_expr_children(expr, &mut |child| collect_aggregate_calls(child, analysis)),
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Optional filter detection
// ════════════════════════════════════════════════════════════════════════════════

/// Detect `$param IS NULL OR var.prop <op> $param` pattern.
fn detect_optional_filter(expr: &Expr) -> Option<SemanticConstraint> {
    if let ExprKind::Or(left, right) = &expr.kind {
        // Left: $param IS NULL
        if let ExprKind::IsNull(inner) = &left.kind
            && let ExprKind::Parameter(param_name) = &inner.kind
        {
            // Right: var.prop <op> $param
            if let ExprKind::Compare {
                left: cmp_left,
                op,
                right: cmp_right,
            } = &right.kind
                && let Some((var, prop)) = extract_property_access(cmp_left)
                && let ExprKind::Parameter(rhs_param) = &cmp_right.kind
                && rhs_param == param_name
            {
                return Some(SemanticConstraint::OptionalFilterPredicate {
                    param_name: param_name.clone(),
                    var,
                    property: prop,
                    op: *op,
                });
            }
        }
    }
    None
}

// ════════════════════════════════════════════════════════════════════════════════
// Property access collection
// ════════════════════════════════════════════════════════════════════════════════

fn collect_expr_property_accesses(expr: &Expr, in_where: bool, analysis: &mut SemanticAnalysis) {
    match &expr.kind {
        ExprKind::PropertyAccess {
            expr: inner,
            property,
        } => {
            if let ExprKind::Variable(var) = &inner.kind {
                analysis
                    .constraints
                    .push(SemanticConstraint::PropertyAccess {
                        var: var.clone(),
                        property: property.clone(),
                        in_where,
                    });
            }
            collect_expr_property_accesses(inner, in_where, analysis);
        }
        _ => walk_expr_children(expr, &mut |child| {
            collect_expr_property_accesses(child, in_where, analysis)
        }),
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Helpers
// ════════════════════════════════════════════════════════════════════════════════

fn flatten_conjunction(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::And(left, right) => {
            let mut result = flatten_conjunction(left);
            result.extend(flatten_conjunction(right));
            result
        }
        _ => vec![expr],
    }
}

fn extract_property_access(expr: &Expr) -> Option<(String, String)> {
    if let ExprKind::PropertyAccess {
        expr: inner,
        property,
    } = &expr.kind
        && let ExprKind::Variable(var) = &inner.kind
    {
        return Some((var.clone(), property.clone()));
    }
    None
}

fn expr_to_predicate_value(expr: &Expr) -> PredicateValue {
    match &expr.kind {
        ExprKind::Literal(v) => PredicateValue::Literal(v.clone()),
        ExprKind::Parameter(p) => PredicateValue::Parameter(p.clone()),
        _ => PredicateValue::Parameter("?".to_string()),
    }
}

fn expr_to_predicate_value_opt(expr: &Expr) -> Option<PredicateValue> {
    match &expr.kind {
        ExprKind::Literal(v) => Some(PredicateValue::Literal(v.clone())),
        ExprKind::Parameter(p) => Some(PredicateValue::Parameter(p.clone())),
        _ => None,
    }
}

fn reverse_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

/// Walk direct children of an expression, calling `f` on each.
fn walk_expr_children(expr: &Expr, f: &mut dyn FnMut(&Expr)) {
    match &expr.kind {
        ExprKind::Paren(e)
        | ExprKind::UnaryOp { expr: e, .. }
        | ExprKind::Not(e)
        | ExprKind::IsNull(e)
        | ExprKind::IsNotNull(e) => f(e),

        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Compare { left, right, .. }
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right) => {
            f(left);
            f(right);
        }

        ExprKind::PropertyAccess { expr: e, .. } | ExprKind::Cast { expr: e, .. } => f(e),

        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                f(arg);
            }
        }

        ExprKind::Aggregate {
            expr: agg_expr,
            expr2,
            ..
        } => {
            if let Some(e) = agg_expr {
                f(e);
            }
            if let Some(e) = expr2 {
                f(e);
            }
        }

        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            f(operand);
            for wc in when_clauses {
                f(&wc.condition);
                f(&wc.result);
            }
            if let Some(e) = else_clause {
                f(e);
            }
        }

        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            for wc in when_clauses {
                f(&wc.condition);
                f(&wc.result);
            }
            if let Some(e) = else_clause {
                f(e);
            }
        }

        ExprKind::Coalesce(exprs) | ExprKind::ListLiteral(exprs) => {
            for e in exprs {
                f(e);
            }
        }

        ExprKind::StringPredicate {
            expr: e, pattern, ..
        } => {
            f(e);
            f(pattern);
        }

        ExprKind::IsLabeled { expr: e, .. }
        | ExprKind::IsSourceOf { node: e, .. }
        | ExprKind::IsDestOf { node: e, .. }
        | ExprKind::IsTyped { expr: e, .. }
        | ExprKind::IsDirected { expr: e, .. }
        | ExprKind::IsNormalized { expr: e, .. }
        | ExprKind::IsTruth { expr: e, .. } => f(e),

        ExprKind::LetIn { bindings, expr: e } => {
            for b in bindings {
                f(&b.value);
            }
            f(e);
        }

        ExprKind::ListConstructor { items, .. } => {
            for item in items {
                f(item);
            }
        }

        // Terminals and unsupported.
        _ => {}
    }
}
