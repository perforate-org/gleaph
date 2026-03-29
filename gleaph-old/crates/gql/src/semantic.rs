//! Shared semantic-analysis structures used by validation, type checking, and future planner work.
//!
//! The current engine still performs most semantic checks directly in `validate` and
//! `type_check`. This module provides the first reusable semantic layer so those
//! passes can stop re-deriving pattern bindings and row-boundary metadata ad hoc.

use crate::ast::*;

/// Schema-aware property type lookup.
///
/// Implemented by the graph bridge using the active graph type definition.
pub trait PropertySchema {
    /// Return `(property_name, value_type, required)` triples for node types matching the given labels.
    /// `required` is `true` when the property is declared `NOT NULL`.
    fn node_property_types(&self, labels: &[String]) -> Vec<(String, ValueType, bool)>;
    /// Return `(property_name, value_type, required)` triples for edge types matching the given label.
    fn edge_property_types(&self, label: &str) -> Vec<(String, ValueType, bool)>;
    /// Resolve a node type name to its label set, if the schema knows that type.
    fn resolve_node_type_labels(&self, _type_name: &str) -> Option<Vec<String>> {
        None
    }
    /// Return allowed `(from_labels, to_labels)` endpoint pairs for a concrete edge label.
    ///
    /// Empty means "no endpoint restriction known" for this label.
    fn edge_endpoint_types(&self, _label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        vec![]
    }
    /// Resolve an edge type name to `(edge_label, from_labels, to_labels)`, if known.
    fn resolve_edge_type(&self, _type_name: &str) -> Option<(String, Vec<String>, Vec<String>)> {
        None
    }
}

/// No-op schema: returns empty property lists (open schema, all properties unknown).
pub struct NoSchema;

impl PropertySchema for NoSchema {
    fn node_property_types(&self, _: &[String]) -> Vec<(String, ValueType, bool)> {
        vec![]
    }

    fn edge_property_types(&self, _: &str) -> Vec<(String, ValueType, bool)> {
        vec![]
    }
}

/// High-level semantic kind for a binding introduced by pattern matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Node,
    Edge,
    Path,
}

/// Structural information about a single binding introduced by a pattern or row boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingInfo {
    pub name: String,
    pub kind: BindingKind,
    pub labels: Vec<String>,
    pub edge_label: Option<String>,
    pub nullable: bool,
    /// For path bindings: min/max hop bounds from the edge pattern.
    pub path_length: Option<PathLength>,
}

/// Semantic boundary that produces a visible row schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryKind {
    With,
    Return,
    Next,
}

/// Lightweight row schema for a `WITH`, `RETURN`, or `NEXT YIELD` boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSchema {
    pub kind: BoundaryKind,
    pub columns: Vec<String>,
}

/// A narrowing fact derived from a WHERE predicate for flow-sensitive type refinement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NarrowingFact {
    /// `var.property IS NOT NULL` — property access is known non-null in the true branch.
    PropertyNonNull { var: String, property: String },
    /// `var IS LABELED label` or `var:Label` in WHERE — node gains additional label.
    LabelNarrowed { var: String, label: String },
    /// `type(var) = 'LABEL'` — edge label is known.
    EdgeLabelNarrowed { var: String, label: String },
}

/// Reusable structural semantic facts extracted from a statement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SemanticAnalysis {
    pub bindings: Vec<BindingInfo>,
    pub row_schemas: Vec<RowSchema>,
    pub constraints: Vec<SemanticConstraint>,
    /// Flow-sensitive narrowing facts extracted from WHERE predicates.
    pub narrowing_facts: Vec<NarrowingFact>,
}

impl SemanticAnalysis {
    /// All `(variable, property)` pairs with WHERE equality predicates.
    pub fn where_equality_predicates(&self) -> Vec<(String, String)> {
        self.constraints
            .iter()
            .filter_map(|c| match c {
                SemanticConstraint::WhereEqualityPredicate { var, property } => {
                    Some((var.clone(), property.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// All `(variable, property)` pairs with inline node property hints.
    pub fn inline_node_properties(&self) -> Vec<(String, String)> {
        self.constraints
            .iter()
            .filter_map(|c| match c {
                SemanticConstraint::InlineNodeProperty { var, property } => {
                    Some((var.clone(), property.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// First WHERE range predicate as `(variable, property, op)`.
    pub fn first_where_range_predicate(&self) -> Option<(String, String, CmpOp)> {
        self.constraints.iter().find_map(|c| match c {
            SemanticConstraint::WhereRangePredicate { var, property, op } => {
                Some((var.clone(), property.clone(), *op))
            }
            _ => None,
        })
    }

    /// All optional filter predicates: `$param IS NULL OR var.prop <op> $param`.
    pub fn optional_filter_predicates(&self) -> Vec<(String, String, String, CmpOp)> {
        self.constraints
            .iter()
            .filter_map(|c| match c {
                SemanticConstraint::OptionalFilterPredicate {
                    param_name,
                    var,
                    property,
                    op,
                } => Some((param_name.clone(), var.clone(), property.clone(), *op)),
                _ => None,
            })
            .collect()
    }

    /// All inline node WHERE equality predicates.
    pub fn inline_where_predicates(&self) -> Vec<(String, String)> {
        self.constraints
            .iter()
            .filter_map(|c| match c {
                SemanticConstraint::InlineNodeWherePredicate { var, property } => {
                    Some((var.clone(), property.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

/// Minimal semantic constraints extracted before expression typing.
#[derive(Clone, Debug, PartialEq)]
pub enum SemanticConstraint {
    BindingIntroduced {
        name: String,
        kind: BindingKind,
        nullable: bool,
    },
    BoundaryProjects {
        kind: BoundaryKind,
        columns: Vec<String>,
    },
    BooleanContext {
        expr: Expr,
    },
    ArithmeticOperands {
        op: BinaryOp,
        left: Expr,
        right: Expr,
    },
    ComparisonOperands {
        op: CmpOp,
        left: Expr,
        right: Expr,
    },
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    NullTest {
        expr: Expr,
        negated: bool,
    },
    Subquery {
        stmt: Statement,
    },
    PropertyAccess {
        target: Expr,
        property: String,
    },
    AggregateCall {
        func: AggFunc,
        expr: Option<Expr>,
        distinct: bool,
    },
    /// WHERE-level equality predicate: `var.prop = literal_or_param`.
    /// Extracted for planner anchor selection and index scan decisions.
    WhereEqualityPredicate {
        var: String,
        property: String,
    },
    /// WHERE-level range predicate: `var.prop >= literal_or_param` etc.
    /// Extracted for planner range index scan decisions.
    WhereRangePredicate {
        var: String,
        property: String,
        op: CmpOp,
    },
    /// Inline node property hint: `(n:Person {name: 'Alice'})`.
    InlineNodeProperty {
        var: String,
        property: String,
    },
    /// Optional filter pattern: `$param IS NULL OR var.prop <op> $param`.
    /// Used for ConditionalIndexScan decisions.
    OptionalFilterPredicate {
        param_name: String,
        var: String,
        property: String,
        op: CmpOp,
    },
    /// Inline node WHERE equality: `(n:Person WHERE n.name = 'Alice')`.
    InlineNodeWherePredicate {
        var: String,
        property: String,
    },
}

/// Collect structural semantic information from a statement.
///
/// This intentionally stays lightweight: it captures pattern bindings and row-shape
/// boundaries, leaving expression typing and diagnostics to `type_check` for now.
pub fn analyze_statement_structure(stmt: &Statement) -> SemanticAnalysis {
    let mut analysis = SemanticAnalysis::default();
    collect_statement_structure(stmt, &mut analysis);
    analysis
}

/// Collect semantic constraints for a single expression subtree.
pub fn constraints_for_expr(expr: &Expr) -> Vec<SemanticConstraint> {
    let mut analysis = SemanticAnalysis::default();
    collect_expr_constraints(expr, &mut analysis);
    analysis.constraints
}

/// Collect semantic constraints for an expression used in boolean context.
pub fn constraints_for_boolean_expr(expr: &Expr) -> Vec<SemanticConstraint> {
    let mut analysis = SemanticAnalysis::default();
    collect_boolean_context(expr, &mut analysis);
    analysis.constraints
}

fn collect_statement_structure(stmt: &Statement, analysis: &mut SemanticAnalysis) {
    match stmt {
        Statement::Query(q) => collect_query_structure(q, analysis),
        Statement::Compound { left, right, op } => {
            collect_statement_structure(left, analysis);
            if let SetOp::Next(yield_cols) = op {
                let row = RowSchema {
                    kind: BoundaryKind::Next,
                    columns: yield_cols.clone().unwrap_or_default(),
                };
                analysis
                    .constraints
                    .push(SemanticConstraint::BoundaryProjects {
                        kind: row.kind,
                        columns: row.columns.clone(),
                    });
                analysis.row_schemas.push(row);
            }
            collect_statement_structure(right, analysis);
        }
        Statement::Delete(d) => {
            analysis
                .bindings
                .extend(bindings_for_match_clause(&d.match_clause));
            if let Some(where_clause) = &d.where_clause {
                collect_boolean_context(where_clause, analysis);
            }
        }
        Statement::Set(s) => {
            analysis
                .bindings
                .extend(bindings_for_match_clause(&s.match_clause));
            if let Some(where_clause) = &s.where_clause {
                collect_boolean_context(where_clause, analysis);
            }
            for item in &s.set_clause.items {
                if let SetItem::Property { value, .. } = item {
                    collect_expr_constraints(value, analysis);
                }
            }
        }
        Statement::Remove(r) => {
            analysis
                .bindings
                .extend(bindings_for_match_clause(&r.match_clause));
            if let Some(where_clause) = &r.where_clause {
                collect_boolean_context(where_clause, analysis);
            }
        }
        Statement::Filter(f) => {
            analysis
                .bindings
                .extend(bindings_for_match_clause(&f.match_clause));
            if let Some(where_clause) = &f.where_clause {
                collect_boolean_context(where_clause, analysis);
            }
            collect_boolean_context(&f.filter_expr, analysis);
        }
        Statement::Let(l) => {
            analysis
                .bindings
                .extend(bindings_for_match_clause(&l.match_clause));
            if let Some(where_clause) = &l.where_clause {
                collect_boolean_context(where_clause, analysis);
            }
            for (_, expr) in &l.bindings {
                collect_expr_constraints(expr, analysis);
            }
            analysis.row_schemas.push(RowSchema {
                kind: BoundaryKind::Return,
                columns: projected_column_names(&l.return_clause.items),
            });
            for item in &l.return_clause.items {
                collect_expr_constraints(&item.expr, analysis);
            }
        }
        Statement::For(f) => {
            collect_expr_constraints(&f.list_expr, analysis);
            analysis.row_schemas.push(RowSchema {
                kind: BoundaryKind::Return,
                columns: projected_column_names(&f.return_clause.items),
            });
            for item in &f.return_clause.items {
                collect_expr_constraints(&item.expr, analysis);
            }
        }
        Statement::Call(c) => collect_statement_structure(&c.body, analysis),
        Statement::Create(_)
        | Statement::Merge(_)
        | Statement::Finish
        | Statement::UseGraph(_)
        | Statement::CreateGraph { .. }
        | Statement::DropGraph { .. }
        | Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => {}
    }
}

fn collect_query_structure(q: &QueryStmt, analysis: &mut SemanticAnalysis) {
    for entry in &q.match_clauses {
        for binding in bindings_for_match_entry(entry) {
            analysis
                .constraints
                .push(SemanticConstraint::BindingIntroduced {
                    name: binding.name.clone(),
                    kind: binding.kind,
                    nullable: binding.nullable,
                });
            analysis.bindings.push(binding);
        }
    }
    // Extract inline node properties and inline WHERE predicates from MATCH patterns.
    for entry in &q.match_clauses {
        collect_inline_node_properties(&entry.pattern, analysis);
        collect_inline_node_where_predicates(&entry.pattern, analysis);
    }
    if let Some(where_clause) = &q.where_clause {
        collect_boolean_context(where_clause, analysis);
        analysis
            .narrowing_facts
            .extend(extract_narrowing_facts(where_clause));
        collect_where_predicates(where_clause, analysis);
        collect_optional_filter_predicates(where_clause, analysis);
    }

    for with in &q.with_clauses {
        let row = RowSchema {
            kind: BoundaryKind::With,
            columns: projected_column_names(&with.items),
        };
        analysis
            .constraints
            .push(SemanticConstraint::BoundaryProjects {
                kind: row.kind,
                columns: row.columns.clone(),
            });
        analysis.row_schemas.push(row);
        for item in &with.items {
            collect_expr_constraints(&item.expr, analysis);
        }
        if let Some(where_clause) = &with.where_clause {
            collect_boolean_context(where_clause, analysis);
            analysis
                .narrowing_facts
                .extend(extract_narrowing_facts(where_clause));
        }
        for entry in &with.match_clauses {
            for binding in bindings_for_match_entry(entry) {
                analysis
                    .constraints
                    .push(SemanticConstraint::BindingIntroduced {
                        name: binding.name.clone(),
                        kind: binding.kind,
                        nullable: binding.nullable,
                    });
                analysis.bindings.push(binding);
            }
        }
        if let Some(where_clause) = &with.post_match_where {
            collect_boolean_context(where_clause, analysis);
            analysis
                .narrowing_facts
                .extend(extract_narrowing_facts(where_clause));
        }
    }

    let row = RowSchema {
        kind: BoundaryKind::Return,
        columns: projected_column_names(&q.return_clause.items),
    };
    analysis
        .constraints
        .push(SemanticConstraint::BoundaryProjects {
            kind: row.kind,
            columns: row.columns.clone(),
        });
    analysis.row_schemas.push(row);
    for item in &q.return_clause.items {
        collect_expr_constraints(&item.expr, analysis);
    }
    if let Some(group_by) = &q.group_by {
        for expr in group_by {
            collect_expr_constraints(expr, analysis);
        }
    }
    if let Some(having) = &q.having {
        collect_boolean_context(having, analysis);
    }
    if let Some(order_by) = &q.order_by {
        for item in &order_by.items {
            collect_expr_constraints(&item.expr, analysis);
        }
    }
}

/// Extract `$param IS NULL OR var.prop <op> $param` optional filter patterns from a WHERE clause.
fn collect_optional_filter_predicates(expr: &Expr, analysis: &mut SemanticAnalysis) {
    fn try_extract_or(
        null_side: &Expr,
        cmp_side: &Expr,
    ) -> Option<(String, String, String, CmpOp)> {
        // null_side must be `$param IS NULL`
        let param_name = match null_side {
            Expr::IsNull(inner) => match inner.as_ref() {
                Expr::Parameter { name, .. } => name.clone(),
                _ => return None,
            },
            _ => return None,
        };
        // cmp_side must be `var.prop <op> $param` or `$param <op> var.prop`
        match cmp_side {
            Expr::Compare { left, op, right } => {
                if !matches!(
                    op,
                    CmpOp::Eq | CmpOp::Ge | CmpOp::Gt | CmpOp::Le | CmpOp::Lt
                ) {
                    return None;
                }
                match (left.as_ref(), right.as_ref()) {
                    (Expr::PropertyAccess { target, property }, Expr::Parameter { name, .. })
                        if *name == param_name =>
                    {
                        if let Expr::Variable(var) = target.as_ref() {
                            Some((param_name, var.clone(), property.clone(), *op))
                        } else {
                            None
                        }
                    }
                    (Expr::Parameter { name, .. }, Expr::PropertyAccess { target, property })
                        if *name == param_name =>
                    {
                        // Flip: $param >= var.prop → store as the canonical op
                        let flipped = match op {
                            CmpOp::Ge => CmpOp::Le,
                            CmpOp::Gt => CmpOp::Lt,
                            CmpOp::Le => CmpOp::Ge,
                            CmpOp::Lt => CmpOp::Gt,
                            other => *other, // Eq stays Eq
                        };
                        if let Expr::Variable(var) = target.as_ref() {
                            Some((param_name, var.clone(), property.clone(), flipped))
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn walk(expr: &Expr, analysis: &mut SemanticAnalysis) {
        match expr {
            Expr::Or(left, right) => {
                if let Some((param_name, var, property, op)) =
                    try_extract_or(left, right).or_else(|| try_extract_or(right, left))
                {
                    analysis
                        .constraints
                        .push(SemanticConstraint::OptionalFilterPredicate {
                            param_name,
                            var,
                            property,
                            op,
                        });
                }
            }
            Expr::And(l, r) => {
                walk(l, analysis);
                walk(r, analysis);
            }
            _ => {}
        }
    }

    walk(expr, analysis);
}

/// Extract inline node WHERE equality predicates from MATCH patterns.
fn collect_inline_node_where_predicates(mc: &MatchClause, analysis: &mut SemanticAnalysis) {
    fn collect_node_where(node: &NodePattern, analysis: &mut SemanticAnalysis) {
        if let Some(var) = &node.var {
            if let Some(where_expr) = &node.where_clause {
                extract_inline_eq(var, where_expr, analysis);
            }
        }
    }

    fn extract_inline_eq(var: &str, expr: &Expr, analysis: &mut SemanticAnalysis) {
        match expr {
            Expr::Compare {
                left,
                op: CmpOp::Eq,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (
                    Expr::PropertyAccess { target, property },
                    Expr::Literal(_) | Expr::Parameter { .. },
                )
                | (
                    Expr::Literal(_) | Expr::Parameter { .. },
                    Expr::PropertyAccess { target, property },
                ) => {
                    if let Expr::Variable(v) = target.as_ref() {
                        if v == var {
                            analysis.constraints.push(
                                SemanticConstraint::InlineNodeWherePredicate {
                                    var: var.to_string(),
                                    property: property.clone(),
                                },
                            );
                        }
                    }
                }
                _ => {}
            },
            Expr::And(l, r) => {
                extract_inline_eq(var, l, analysis);
                extract_inline_eq(var, r, analysis);
            }
            _ => {}
        }
    }

    collect_node_where(&mc.start, analysis);
    for chain in mc.hops() {
        collect_node_where(&chain.node, analysis);
    }
}

/// Extract equality and range predicates from a WHERE clause for planner consumption.
fn collect_where_predicates(expr: &Expr, analysis: &mut SemanticAnalysis) {
    fn is_value_source(e: &Expr) -> bool {
        matches!(e, Expr::Literal(_) | Expr::Parameter { .. })
    }
    match expr {
        Expr::Compare { left, op, right } => {
            let var_prop = match (left.as_ref(), right.as_ref()) {
                (Expr::PropertyAccess { target, property }, rhs) if is_value_source(rhs) => {
                    if let Expr::Variable(var) = target.as_ref() {
                        Some((var.clone(), property.clone()))
                    } else {
                        None
                    }
                }
                (lhs, Expr::PropertyAccess { target, property }) if is_value_source(lhs) => {
                    if let Expr::Variable(var) = target.as_ref() {
                        Some((var.clone(), property.clone()))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some((var, property)) = var_prop {
                if *op == CmpOp::Eq {
                    analysis
                        .constraints
                        .push(SemanticConstraint::WhereEqualityPredicate { var, property });
                } else if matches!(op, CmpOp::Ge | CmpOp::Gt | CmpOp::Le | CmpOp::Lt) {
                    analysis
                        .constraints
                        .push(SemanticConstraint::WhereRangePredicate {
                            var,
                            property,
                            op: *op,
                        });
                }
            }
        }
        Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r) => {
            collect_where_predicates(l, analysis);
            collect_where_predicates(r, analysis);
        }
        Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
            collect_where_predicates(e, analysis);
        }
        _ => {}
    }
}

/// Extract inline node property hints from MATCH patterns.
fn collect_inline_node_properties(mc: &MatchClause, analysis: &mut SemanticAnalysis) {
    fn collect_node(node: &NodePattern, analysis: &mut SemanticAnalysis) {
        if let Some(var) = &node.var {
            for (property, _) in &node.props_hint {
                analysis
                    .constraints
                    .push(SemanticConstraint::InlineNodeProperty {
                        var: var.clone(),
                        property: property.clone(),
                    });
            }
        }
    }
    collect_node(&mc.start, analysis);
    for chain in mc.hops() {
        collect_node(&chain.node, analysis);
    }
}

fn collect_boolean_context(expr: &Expr, analysis: &mut SemanticAnalysis) {
    analysis
        .constraints
        .push(SemanticConstraint::BooleanContext { expr: expr.clone() });
    collect_expr_constraints(expr, analysis);
}

fn collect_expr_constraints(expr: &Expr, analysis: &mut SemanticAnalysis) {
    match expr {
        Expr::BinaryOp { op, left, right } => {
            analysis
                .constraints
                .push(SemanticConstraint::ArithmeticOperands {
                    op: *op,
                    left: (*left.clone()),
                    right: (*right.clone()),
                });
            collect_expr_constraints(left, analysis);
            collect_expr_constraints(right, analysis);
        }
        Expr::Compare { left, op, right } => {
            analysis
                .constraints
                .push(SemanticConstraint::ComparisonOperands {
                    op: *op,
                    left: (*left.clone()),
                    right: (*right.clone()),
                });
            collect_expr_constraints(left, analysis);
            collect_expr_constraints(right, analysis);
        }
        Expr::FunctionCall { name, args } => {
            analysis.constraints.push(SemanticConstraint::FunctionCall {
                name: name.clone(),
                args: args.clone(),
            });
            for arg in args {
                collect_expr_constraints(arg, analysis);
            }
        }
        Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right)
        | Expr::Concat(left, right) => {
            collect_expr_constraints(left, analysis);
            collect_expr_constraints(right, analysis);
        }
        Expr::UnaryOp { expr: inner, .. } => collect_expr_constraints(inner, analysis),
        Expr::Not(inner)
        | Expr::PathLength(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::IsTruth { expr: inner, .. }
        | Expr::IsLabeled { expr: inner, .. }
        | Expr::IsDirected { expr: inner, .. }
        | Expr::IsType { expr: inner, .. } => collect_expr_constraints(inner, analysis),
        Expr::PropertyAccess { target, property } => {
            analysis
                .constraints
                .push(SemanticConstraint::PropertyAccess {
                    target: (*target.clone()),
                    property: property.clone(),
                });
            collect_expr_constraints(target, analysis);
        }
        Expr::IsNull(inner) => {
            analysis.constraints.push(SemanticConstraint::NullTest {
                expr: (*inner.clone()),
                negated: false,
            });
            collect_expr_constraints(inner, analysis);
        }
        Expr::IsNotNull(inner) => {
            analysis.constraints.push(SemanticConstraint::NullTest {
                expr: (*inner.clone()),
                negated: true,
            });
            collect_expr_constraints(inner, analysis);
        }
        Expr::Exists(stmt) | Expr::ValueSubquery(stmt) => {
            analysis.constraints.push(SemanticConstraint::Subquery {
                stmt: (*stmt.clone()),
            });
            collect_statement_structure(stmt, analysis);
        }
        Expr::InList { expr, list, .. } => {
            collect_expr_constraints(expr, analysis);
            for item in list {
                collect_expr_constraints(item, analysis);
            }
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            collect_expr_constraints(expr, analysis);
            collect_expr_constraints(pattern, analysis);
        }
        Expr::Case(case) => {
            if let Some(operand) = &case.operand {
                collect_expr_constraints(operand, analysis);
            }
            for wt in &case.when_then {
                collect_boolean_context(&wt.when, analysis);
                collect_expr_constraints(&wt.then, analysis);
            }
            if let Some(else_expr) = &case.else_expr {
                collect_expr_constraints(else_expr, analysis);
            }
        }
        Expr::Coalesce(exprs)
        | Expr::AllDifferent(exprs)
        | Expr::Same(exprs)
        | Expr::ListLiteral(exprs)
        | Expr::PathConstructor(exprs) => {
            for expr in exprs {
                collect_expr_constraints(expr, analysis);
            }
        }
        Expr::NullIf { left, right }
        | Expr::IsSourceOf {
            node: left,
            edge: right,
            ..
        }
        | Expr::IsDestOf {
            node: left,
            edge: right,
            ..
        }
        | Expr::ListIndex {
            list: left,
            index: right,
        } => {
            collect_expr_constraints(left, analysis);
            collect_expr_constraints(right, analysis);
        }
        Expr::Aggregate(agg) => {
            analysis
                .constraints
                .push(SemanticConstraint::AggregateCall {
                    func: agg.func,
                    expr: agg.expr.as_ref().map(|inner| *inner.clone()),
                    distinct: agg.distinct,
                });
            if let Some(inner) = &agg.expr {
                collect_expr_constraints(inner, analysis);
            }
            if let Some(separator) = &agg.separator {
                collect_expr_constraints(separator, analysis);
            }
        }
        Expr::PropertyExists { target, .. } => collect_expr_constraints(target, analysis),
        Expr::RecordLiteral(fields) => {
            for (_, expr) in fields {
                collect_expr_constraints(expr, analysis);
            }
        }
        Expr::LetIn { bindings, body } => {
            for (_, expr) in bindings {
                collect_expr_constraints(expr, analysis);
            }
            collect_expr_constraints(body, analysis);
        }
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter { .. } | Expr::PathVar(_) => {}
    }
}

/// Extract flow-sensitive narrowing facts from a WHERE expression.
///
/// Only AND-connected top-level predicates produce narrowing facts, because
/// OR-connected branches require conservative joins.
pub fn extract_narrowing_facts(expr: &Expr) -> Vec<NarrowingFact> {
    let mut facts = Vec::new();
    collect_narrowing_facts(expr, &mut facts);
    facts
}

fn collect_narrowing_facts(expr: &Expr, out: &mut Vec<NarrowingFact>) {
    match expr {
        // IS NOT NULL on a property access → property is non-null downstream
        Expr::IsNotNull(inner) => {
            if let Expr::PropertyAccess { target, property } = inner.as_ref()
                && let Expr::Variable(var) = target.as_ref()
            {
                out.push(NarrowingFact::PropertyNonNull {
                    var: var.clone(),
                    property: property.clone(),
                });
            }
        }
        // IS LABELED check → node gains label (simple Name only)
        Expr::IsLabeled {
            expr: inner,
            negated,
            label_expr,
        } => {
            if !negated
                && let Expr::Variable(var) = inner.as_ref()
                && let gleaph_types::LabelExpr::Name(label) = label_expr
            {
                out.push(NarrowingFact::LabelNarrowed {
                    var: var.clone(),
                    label: label.clone(),
                });
            }
        }
        // type(e) = 'LABEL' → edge label narrowed
        Expr::Compare {
            left,
            op: CmpOp::Eq,
            right,
        } => {
            // Check type(var) = 'literal'
            if let Expr::FunctionCall { name, args } = left.as_ref()
                && name.eq_ignore_ascii_case("type")
                && args.len() == 1
                && let Expr::Variable(var) = &args[0]
                && let Expr::Literal(gleaph_types::Value::Text(label)) = right.as_ref()
            {
                out.push(NarrowingFact::EdgeLabelNarrowed {
                    var: var.clone(),
                    label: label.clone(),
                });
            }
            // Also check 'literal' = type(var)
            if let Expr::FunctionCall { name, args } = right.as_ref()
                && name.eq_ignore_ascii_case("type")
                && args.len() == 1
                && let Expr::Variable(var) = &args[0]
                && let Expr::Literal(gleaph_types::Value::Text(label)) = left.as_ref()
            {
                out.push(NarrowingFact::EdgeLabelNarrowed {
                    var: var.clone(),
                    label: label.clone(),
                });
            }
        }
        // AND: both sides contribute narrowing facts
        Expr::And(left, right) => {
            collect_narrowing_facts(left, out);
            collect_narrowing_facts(right, out);
        }
        // OR and NOT: no narrowing (conservative)
        _ => {}
    }
}

/// Compute combined path length from all hops in a match clause.
/// For single-hop patterns, returns the edge's `PathLength` directly.
/// For multi-hop, sums the min/max across all hops.
fn compute_path_length(mc: &MatchClause) -> Option<PathLength> {
    let hops: Vec<_> = mc.hops().collect();
    if hops.is_empty() {
        return None;
    }
    if hops.len() == 1 {
        return Some(hops[0].edge.length);
    }
    // Multi-hop: sum all min/max.
    let mut total_min = 0u32;
    let mut total_max = 0u32;
    let mut all_fixed = true;
    for chain in &hops {
        match chain.edge.length {
            PathLength::Fixed(n) => {
                total_min += n;
                total_max += n;
            }
            PathLength::Range { min, max } => {
                total_min += min;
                total_max += max;
                all_fixed = false;
            }
        }
    }
    if all_fixed && total_min == total_max {
        Some(PathLength::Fixed(total_min))
    } else {
        Some(PathLength::Range {
            min: total_min,
            max: total_max,
        })
    }
}

/// Collect all explicit bindings introduced by a match clause.
pub fn bindings_for_match_clause(mc: &MatchClause) -> Vec<BindingInfo> {
    let mut out = Vec::new();
    collect_node_binding(&mc.start, false, &mut out);
    for chain in mc.hops() {
        collect_edge_binding(&chain.edge, false, &mut out);
        collect_node_binding(&chain.node, false, &mut out);
    }
    out
}

/// Collect all explicit bindings introduced by a query match entry, including path vars and optionality.
pub fn bindings_for_match_entry(entry: &MatchEntry) -> Vec<BindingInfo> {
    let mut out = Vec::new();
    collect_node_binding(&entry.pattern.start, entry.optional, &mut out);
    for chain in entry.pattern.hops() {
        collect_edge_binding(&chain.edge, entry.optional, &mut out);
        collect_node_binding(&chain.node, entry.optional, &mut out);
    }
    if let Some(path_var) = &entry.path_variable {
        // Compute combined path length from all hops in the pattern.
        let path_length = compute_path_length(&entry.pattern);
        out.push(BindingInfo {
            name: path_var.clone(),
            kind: BindingKind::Path,
            labels: Vec::new(),
            edge_label: None,
            nullable: entry.optional,
            path_length,
        });
    }
    out
}

fn collect_node_binding(np: &NodePattern, nullable: bool, out: &mut Vec<BindingInfo>) {
    if let Some(var) = &np.var {
        out.push(BindingInfo {
            name: var.clone(),
            kind: BindingKind::Node,
            labels: np.labels.clone(),
            edge_label: None,
            nullable,
            path_length: None,
        });
    }
}

fn collect_edge_binding(ep: &EdgePattern, nullable: bool, out: &mut Vec<BindingInfo>) {
    if let Some(var) = &ep.var {
        out.push(BindingInfo {
            name: var.clone(),
            kind: BindingKind::Edge,
            labels: Vec::new(),
            edge_label: ep.label.clone(),
            nullable,
            path_length: None,
        });
    }
}

/// Return all projected names that can be determined structurally.
pub fn projected_column_names(items: &[ReturnItem]) -> Vec<String> {
    items.iter().filter_map(projected_column_name).collect()
}

/// Determine the visible column name for a projection item if it is structurally known.
pub fn projected_column_name(item: &ReturnItem) -> Option<String> {
    item.alias.clone().or(match &item.expr {
        Expr::Variable(v) => Some(v.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Typed constraint system (Phase B: constraint-based type checking)
// ---------------------------------------------------------------------------

/// Index into the solved type table.
pub type TypeVarId = u32;

/// A typed constraint that references type variables rather than AST nodes.
///
/// Generated during a single AST walk, solved in a separate pass.
#[derive(Clone, Debug)]
pub enum TypedConstraint {
    /// A WHERE/FILTER/HAVING expression must be boolean.
    MustBeBoolean { expr_id: TypeVarId },
    /// Arithmetic operation: both operands must be compatible.
    Arithmetic {
        op: BinaryOp,
        left: TypeVarId,
        right: TypeVarId,
        result: TypeVarId,
    },
    /// Comparison: both operands must be comparable.
    Comparison {
        op: CmpOp,
        left: TypeVarId,
        right: TypeVarId,
    },
    /// IS NULL / IS NOT NULL on an expression.
    NullTest { expr_id: TypeVarId, negated: bool },
    /// Function call with typed arguments.
    FunctionCall {
        name: String,
        arg_ids: Vec<TypeVarId>,
        result: TypeVarId,
    },
    /// Subquery — constraints checked recursively.
    Subquery { stmt: Statement },
}

/// Source information for a diagnostic (where did this constraint come from).
#[derive(Clone, Debug)]
pub enum DiagnosticProvenance {
    /// From a specific constraint index.
    Constraint(usize),
    /// From binding validation.
    Binding(String),
    /// From endpoint constraint checking.
    EndpointCheck { edge_label: String },
}

/// A single diagnostic emitted by the constraint solver.
#[derive(Clone, Debug)]
pub struct TypeDiagnosticEntry {
    pub kind: super::type_check::WarningKind,
    pub message: String,
    pub provenance: DiagnosticProvenance,
}

/// Solved type table: maps TypeVarId → inferred Type.
#[derive(Clone, Debug, Default)]
pub struct SolvedTypeTable {
    types: Vec<super::type_check::Type>,
}

impl SolvedTypeTable {
    pub fn new() -> Self {
        Self { types: Vec::new() }
    }

    /// Allocate a new type variable with an initial type.
    pub fn alloc(&mut self, ty: super::type_check::Type) -> TypeVarId {
        let id = self.types.len() as TypeVarId;
        self.types.push(ty);
        id
    }

    /// Get the type for a variable.
    pub fn get(&self, id: TypeVarId) -> &super::type_check::Type {
        &self.types[id as usize]
    }

    /// Set the type for a variable (for refinement during solving).
    pub fn set(&mut self, id: TypeVarId, ty: super::type_check::Type) {
        self.types[id as usize] = ty;
    }

    /// Number of allocated type variables.
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

/// Result of constraint generation: constraints + solved type table.
#[derive(Clone, Debug)]
pub struct ConstraintSet {
    pub constraints: Vec<TypedConstraint>,
    pub types: SolvedTypeTable,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_statement;

    #[test]
    fn collects_match_bindings_for_query() {
        let stmt = parse_statement("MATCH (a:User)-[e:KNOWS]->(b:User) RETURN a, e, b").unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(analysis.bindings.iter().any(|b| {
            b.name == "a"
                && b.kind == BindingKind::Node
                && b.labels == vec!["User".to_string()]
                && !b.nullable
        }));
        assert!(analysis.bindings.iter().any(|b| {
            b.name == "e"
                && b.kind == BindingKind::Edge
                && b.edge_label.as_deref() == Some("KNOWS")
                && !b.nullable
        }));
        assert!(analysis.bindings.iter().any(|b| {
            b.name == "b"
                && b.kind == BindingKind::Node
                && b.labels == vec!["User".to_string()]
                && !b.nullable
        }));
    }

    #[test]
    fn collects_optional_and_path_bindings() {
        let stmt = parse_statement("OPTIONAL MATCH p = (a)-[e:X]->(b) RETURN p").unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(
            analysis
                .bindings
                .iter()
                .any(|b| { b.name == "p" && b.kind == BindingKind::Path && b.nullable })
        );
        assert!(
            analysis
                .bindings
                .iter()
                .any(|b| { b.name == "a" && b.kind == BindingKind::Node && b.nullable })
        );
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::BindingIntroduced { name, kind: BindingKind::Path, nullable: true }
                if name == "p"
        )));
    }

    #[test]
    fn collects_with_and_return_row_schemas() {
        let stmt = parse_statement("MATCH (n:Person) WITH n AS person RETURN person AS p, n.name")
            .unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert_eq!(
            analysis.row_schemas,
            vec![
                RowSchema {
                    kind: BoundaryKind::With,
                    columns: vec!["person".into()],
                },
                RowSchema {
                    kind: BoundaryKind::Return,
                    columns: vec!["p".into()],
                }
            ]
        );
    }

    #[test]
    fn collects_next_boundary() {
        let stmt = parse_statement("RETURN 1 AS x NEXT YIELD x RETURN x").unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(
            analysis.row_schemas.iter().any(|row| {
                row.kind == BoundaryKind::Next && row.columns == vec!["x".to_string()]
            })
        );
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::BoundaryProjects { kind: BoundaryKind::Next, columns }
                if columns == &vec!["x".to_string()]
        )));
    }

    #[test]
    fn collects_expression_constraints() {
        let stmt = parse_statement(
            "MATCH (n:User) WHERE n.age > 18 RETURN n.age + 1 AS score, id(n) AS ident",
        )
        .unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(
            analysis
                .constraints
                .iter()
                .any(|c| matches!(c, SemanticConstraint::BooleanContext { .. }))
        );
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::ComparisonOperands { op: CmpOp::Gt, .. }
        )));
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::ArithmeticOperands {
                op: BinaryOp::Add,
                ..
            }
        )));
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::FunctionCall { name, .. } if name == "id"
        )));
    }

    #[test]
    fn collects_null_test_constraints() {
        let stmt = parse_statement("MATCH (n:Person) WHERE n.name IS NOT NULL RETURN n").unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(
            analysis
                .constraints
                .iter()
                .any(|c| matches!(c, SemanticConstraint::NullTest { negated: true, .. }))
        );
        assert!(
            analysis
                .constraints
                .iter()
                .any(|c| matches!(c, SemanticConstraint::BooleanContext { .. }))
        );
    }

    #[test]
    fn collects_subquery_constraints() {
        let stmt =
            parse_statement("MATCH (n:User) WHERE EXISTS { MATCH (m:User) RETURN m } RETURN n")
                .unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(
            analysis
                .constraints
                .iter()
                .any(|c| matches!(c, SemanticConstraint::Subquery { .. }))
        );
        assert!(
            analysis
                .row_schemas
                .iter()
                .any(|row| row.kind == BoundaryKind::Return)
        );
    }

    #[test]
    fn collects_property_access_constraints() {
        let stmt = parse_statement("MATCH (n:User) RETURN n.name, n.age").unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::PropertyAccess { property, .. } if property == "name"
        )));
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::PropertyAccess { property, .. } if property == "age"
        )));
    }

    #[test]
    fn extracts_narrowing_from_is_not_null() {
        let stmt = parse_statement("MATCH (n:Person) WHERE n.age IS NOT NULL RETURN n").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        assert!(analysis.narrowing_facts.iter().any(|f| matches!(
            f,
            NarrowingFact::PropertyNonNull { var, property }
                if var == "n" && property == "age"
        )));
    }

    #[test]
    fn extracts_narrowing_from_and_connected() {
        let stmt = parse_statement(
            "MATCH (n:Person) WHERE n.age IS NOT NULL AND n.name IS NOT NULL RETURN n",
        )
        .unwrap();
        let analysis = analyze_statement_structure(&stmt);
        assert_eq!(
            analysis
                .narrowing_facts
                .iter()
                .filter(|f| matches!(f, NarrowingFact::PropertyNonNull { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn no_narrowing_from_or() {
        let stmt = parse_statement(
            "MATCH (n:Person) WHERE n.age IS NOT NULL OR n.name IS NOT NULL RETURN n",
        )
        .unwrap();
        let analysis = analyze_statement_structure(&stmt);
        // OR-connected predicates should NOT produce narrowing facts
        assert!(analysis.narrowing_facts.is_empty());
    }

    #[test]
    fn extracts_narrowing_from_is_labeled() {
        let stmt = parse_statement("MATCH (n) WHERE n IS LABELED :Person RETURN n").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        assert!(analysis.narrowing_facts.iter().any(|f| matches!(
            f,
            NarrowingFact::LabelNarrowed { var, label }
                if var == "n" && label == "Person"
        )));
    }

    #[test]
    fn extracts_narrowing_from_type_eq() {
        let stmt =
            parse_statement("MATCH (a)-[e]->(b) WHERE type(e) = 'KNOWS' RETURN a, b").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        assert!(analysis.narrowing_facts.iter().any(|f| matches!(
            f,
            NarrowingFact::EdgeLabelNarrowed { var, label }
                if var == "e" && label == "KNOWS"
        )));
    }

    #[test]
    fn collects_aggregate_constraints() {
        let stmt = parse_statement("MATCH (n:User) RETURN COUNT(n) AS c, SUM(n.age) AS s").unwrap();
        let analysis = analyze_statement_structure(&stmt);

        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::AggregateCall {
                func: AggFunc::Count,
                ..
            }
        )));
        assert!(analysis.constraints.iter().any(|c| matches!(
            c,
            SemanticConstraint::AggregateCall {
                func: AggFunc::Sum,
                ..
            }
        )));
    }

    #[test]
    fn extracts_optional_filter_predicate() {
        let stmt = parse_statement("MATCH (n:Person) WHERE $age IS NULL OR n.age = $age RETURN n")
            .unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let preds = analysis.optional_filter_predicates();
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].0, "age"); // param_name
        assert_eq!(preds[0].1, "n"); // var
        assert_eq!(preds[0].2, "age"); // property
        assert_eq!(preds[0].3, CmpOp::Eq);
    }

    #[test]
    fn extracts_optional_filter_range() {
        let stmt = parse_statement(
            "MATCH (n:Person) WHERE ($min IS NULL OR n.age >= $min) AND ($max IS NULL OR n.age <= $max) RETURN n",
        ).unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let preds = analysis.optional_filter_predicates();
        assert_eq!(preds.len(), 2);
        assert!(
            preds.iter().any(|(p, v, prop, op)| p == "min"
                && v == "n"
                && prop == "age"
                && *op == CmpOp::Ge)
        );
        assert!(
            preds.iter().any(|(p, v, prop, op)| p == "max"
                && v == "n"
                && prop == "age"
                && *op == CmpOp::Le)
        );
    }

    #[test]
    fn extracts_optional_filter_reversed() {
        // $param >= var.prop → stored as var.prop <= $param
        let stmt =
            parse_statement("MATCH (n:Person) WHERE $val IS NULL OR $val >= n.score RETURN n")
                .unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let preds = analysis.optional_filter_predicates();
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].3, CmpOp::Le); // flipped
    }

    #[test]
    fn no_optional_filter_without_is_null() {
        let stmt = parse_statement("MATCH (n:Person) WHERE n.age = $age RETURN n").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        assert!(analysis.optional_filter_predicates().is_empty());
    }

    #[test]
    fn extracts_inline_node_where_predicate() {
        let stmt = parse_statement("MATCH (n:Person WHERE n.name = 'Alice') RETURN n").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let preds = analysis.inline_where_predicates();
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].0, "n");
        assert_eq!(preds[0].1, "name");
    }

    #[test]
    fn extracts_inline_node_where_with_param() {
        let stmt = parse_statement("MATCH (n:Person WHERE n.age = $age) RETURN n").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let preds = analysis.inline_where_predicates();
        assert_eq!(preds.len(), 1);
        assert_eq!(preds[0].0, "n");
        assert_eq!(preds[0].1, "age");
    }

    #[test]
    fn extracts_inline_node_where_and_connected() {
        let stmt =
            parse_statement("MATCH (n:Person WHERE n.name = 'Alice' AND n.age = 30) RETURN n")
                .unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let preds = analysis.inline_where_predicates();
        assert_eq!(preds.len(), 2);
    }

    #[test]
    fn path_binding_carries_fixed_length() {
        let stmt = parse_statement("MATCH p = (a)-[e:X]->(b) RETURN p").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let path_binding = analysis.bindings.iter().find(|b| b.name == "p").unwrap();
        assert_eq!(path_binding.kind, BindingKind::Path);
        assert_eq!(path_binding.path_length, Some(PathLength::Fixed(1)));
    }

    #[test]
    fn path_binding_carries_variable_length() {
        let stmt = parse_statement("MATCH p = (a)-[e:X*2..5]->(b) RETURN p").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let path_binding = analysis.bindings.iter().find(|b| b.name == "p").unwrap();
        assert_eq!(
            path_binding.path_length,
            Some(PathLength::Range { min: 2, max: 5 })
        );
    }

    #[test]
    fn path_binding_multi_hop_sums_lengths() {
        let stmt = parse_statement("MATCH p = (a)-[:X]->(b)-[:Y*2..3]->(c) RETURN p").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        let path_binding = analysis.bindings.iter().find(|b| b.name == "p").unwrap();
        // Fixed(1) + Range{2,3} = Range{3,4}
        assert_eq!(
            path_binding.path_length,
            Some(PathLength::Range { min: 3, max: 4 })
        );
    }

    #[test]
    fn non_path_bindings_have_no_path_length() {
        let stmt = parse_statement("MATCH (a:Person)-[e:KNOWS]->(b) RETURN a").unwrap();
        let analysis = analyze_statement_structure(&stmt);
        for binding in &analysis.bindings {
            assert_eq!(
                binding.path_length, None,
                "binding {} should have no path_length",
                binding.name
            );
        }
    }
}
