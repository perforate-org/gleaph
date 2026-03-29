//! Static type inference and warning-mode type checking for GQL statements.
//!
//! Phase 1: emits **warnings** (not errors) for provably wrong type combinations.
//! No existing runtime behavior is changed — `Unknown` suppresses all warnings.

use rapidhash::fast::RapidHashMap;
use std::collections::HashSet;

use crate::ast::*;
use crate::semantic::{
    BindingKind, NarrowingFact, SemanticAnalysis, SemanticConstraint, analyze_statement_structure,
    bindings_for_match_clause, bindings_for_match_entry, constraints_for_expr,
    extract_narrowing_facts, projected_column_name,
};

/// Semantic metadata for a graph node type.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeTypeInfo {
    /// Known labels for this node.
    pub labels: Vec<String>,
    /// Schema-known property types: `(name, value_type, required)`.
    /// Empty when no schema is available or node type is unknown.
    pub properties: Vec<(String, ValueType, bool)>,
}

impl NodeTypeInfo {
    /// Create from just labels (most common case).
    pub fn from_labels(labels: Vec<String>) -> Self {
        Self {
            labels,
            properties: Vec::new(),
        }
    }
}

/// Semantic metadata for a graph edge type.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeTypeInfo {
    /// Known edge label (if any).
    pub label: Option<String>,
    /// Schema-known endpoint constraints: `(from_labels, to_labels)` pairs.
    /// Empty when no schema or edge type is unknown.
    pub endpoints: Vec<(Vec<String>, Vec<String>)>,
    /// Schema-known property types: `(name, value_type, required)`.
    pub properties: Vec<(String, ValueType, bool)>,
}

impl EdgeTypeInfo {
    /// Create from just an optional label (most common case).
    pub fn from_label(label: Option<String>) -> Self {
        Self {
            label,
            endpoints: Vec::new(),
            properties: Vec::new(),
        }
    }
}

/// Semantic metadata for a path type.
#[derive(Clone, Debug, PartialEq)]
pub struct PathTypeInfo {
    /// Known minimum hop count (from variable-length edge `[*min..max]`).
    pub min_hops: Option<u32>,
    /// Known maximum hop count.
    pub max_hops: Option<u32>,
}

impl PathTypeInfo {
    /// Create with no known bounds.
    pub fn unbounded() -> Self {
        Self {
            min_hops: None,
            max_hops: None,
        }
    }
}

impl Default for PathTypeInfo {
    fn default() -> Self {
        Self::unbounded()
    }
}

/// Inferred type for an expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// A concrete scalar type.
    Scalar(ValueType),
    /// Union of possible types (from CASE/COALESCE).
    Union(Vec<Type>),
    /// List with known element type.
    TypedList(Box<Type>),
    /// Cannot be determined statically — suppresses ALL warnings.
    Unknown,
    /// Statically contradictory — the expression can never produce a value.
    /// Propagates through operations and suppresses downstream warnings.
    Never,
    /// Graph node with semantic metadata.
    Node(NodeTypeInfo),
    /// Graph edge with semantic metadata.
    Edge(EdgeTypeInfo),
    /// A path value with optional bounds.
    Path(PathTypeInfo),
    /// Record with known field types.
    Record(Vec<(String, Type)>),
    /// Schema-declared NOT NULL property — the inner type is provably non-nullable.
    NonNull(Box<Type>),
}

/// Source location of a type-check warning for diagnostic tooling.
#[derive(Clone, Debug, PartialEq)]
pub enum WarningProvenance {
    /// From constraint solving (constraint index in the `ConstraintSet`).
    Constraint(usize),
    /// From binding validation (variable name that caused the issue).
    Binding(String),
    /// From endpoint contradiction checking (edge label involved).
    EndpointCheck { edge_label: String },
    /// From aggregation boundary validation.
    AggregationBoundary,
}

/// A type-check warning emitted during static analysis.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeWarning {
    pub message: String,
    pub kind: WarningKind,
    /// Source of this warning, for diagnostic tooling. `None` for legacy paths.
    pub provenance: Option<WarningProvenance>,
}

/// Category of type warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// Incompatible operands for a binary operator (e.g. `42 + 'hello'`).
    BinaryOpMismatch,
    /// WHERE/FILTER condition inferred as non-boolean.
    NonBooleanCondition,
    /// Function argument type mismatch (e.g. `id('hello')`).
    FunctionArgMismatch,
    /// Both sides of a comparison are known and incompatible.
    ComparisonMismatch,
    /// IS NULL applied to a NOT NULL property (always false).
    NullCheckOnNonNull,
    /// Pattern endpoints contradict active graph type edge constraints.
    ImpossiblePattern,
    /// Expression appears in aggregate projection but is neither grouped nor aggregated.
    GroupingViolation,
}

pub use crate::semantic::{NoSchema, PropertySchema};

/// Typing environment: maps variable names to inferred types.
struct TypeEnv<'a> {
    bindings: RapidHashMap<String, Type>,
    warnings: Vec<TypeWarning>,
    schema: &'a dyn PropertySchema,
    /// Variables introduced by OPTIONAL MATCH — property access strips NonNull.
    optional_vars: HashSet<String>,
    /// `(var, property)` pairs narrowed to non-null by flow-sensitive WHERE analysis.
    narrowed_nonnull: HashSet<(String, String)>,
    /// Variables whose label sets were narrowed by WHERE predicates.
    narrowed_labels: RapidHashMap<String, Vec<String>>,
    /// Edge variables narrowed to a specific label by `type(e) = 'X'` in WHERE.
    narrowed_edge_labels: RapidHashMap<String, String>,
}

impl<'a> TypeEnv<'a> {
    fn new(schema: &'a dyn PropertySchema) -> Self {
        Self {
            bindings: RapidHashMap::default(),
            warnings: Vec::new(),
            schema,
            optional_vars: HashSet::new(),
            narrowed_nonnull: HashSet::new(),
            narrowed_labels: RapidHashMap::default(),
            narrowed_edge_labels: RapidHashMap::default(),
        }
    }

    fn bind(&mut self, name: String, ty: Type) {
        self.bindings.insert(name, ty);
    }

    fn get(&self, name: &str) -> Type {
        self.bindings.get(name).cloned().unwrap_or(Type::Unknown)
    }

    fn warn(&mut self, kind: WarningKind, message: String) {
        self.warnings.push(TypeWarning {
            message,
            kind,
            provenance: None,
        });
    }

    fn warn_with_provenance(
        &mut self,
        kind: WarningKind,
        message: String,
        provenance: WarningProvenance,
    ) {
        self.warnings.push(TypeWarning {
            message,
            kind,
            provenance: Some(provenance),
        });
    }

    /// Apply flow-sensitive narrowing facts from WHERE predicates.
    fn apply_narrowing(&mut self, facts: &[NarrowingFact]) {
        for fact in facts {
            match fact {
                NarrowingFact::PropertyNonNull { var, property } => {
                    self.narrowed_nonnull
                        .insert((var.clone(), property.clone()));
                }
                NarrowingFact::LabelNarrowed { var, label } => {
                    // Add the label to the variable's known labels.
                    self.narrowed_labels
                        .entry(var.clone())
                        .or_default()
                        .push(label.clone());
                    // Also update the binding if it's a Node without this label.
                    if let Some(Type::Node(info)) = self.bindings.get_mut(var)
                        && !info.labels.contains(label)
                    {
                        info.labels.push(label.clone());
                    }
                }
                NarrowingFact::EdgeLabelNarrowed { var, label } => {
                    // Track narrowed edge label for endpoint constraint propagation.
                    self.narrowed_edge_labels.insert(var.clone(), label.clone());
                    // Update the binding if it's an Edge without a label.
                    if let Some(Type::Edge(info)) = self.bindings.get_mut(var)
                        && info.label.is_none()
                    {
                        info.label = Some(label.clone());
                    }
                }
            }
        }
    }
}

/// Entry point: run static type checking on a parsed statement.
/// Returns a list of warnings (empty if all looks good).
pub fn type_check_statement(stmt: &Statement) -> Vec<TypeWarning> {
    type_check_statement_with_schema(stmt, &NoSchema)
}

/// Entry point with schema: run static type checking with property type awareness.
pub fn type_check_statement_with_schema(
    stmt: &Statement,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    type_check_via_constraints(stmt, schema)
}

/// Strict-mode entry point: type mismatches are returned as an error.
/// Returns `Err` with the first type mismatch message, or `Ok(())` if clean.
pub fn type_check_statement_strict(
    stmt: &Statement,
    schema: &dyn PropertySchema,
) -> Result<(), gleaph_types::GleaphError> {
    let warnings = type_check_statement_with_schema(stmt, schema);
    if let Some(first) = warnings.first() {
        Err(gleaph_types::GleaphError::ValidationError(format!(
            "type error: {}",
            first.message
        )))
    } else {
        Ok(())
    }
}

/// Extract return column names and their inferred types from a statement's RETURN clause.
fn infer_return_types(env: &TypeEnv<'_>, stmt: &Statement) -> Option<Vec<(String, Type)>> {
    let q = match stmt {
        Statement::Query(q) => q,
        _ => return None,
    };
    if q.return_clause.star || q.return_clause.no_bindings {
        return None;
    }
    let mut cols = Vec::new();
    for item in &q.return_clause.items {
        let name = projected_column_name(item)?;
        cols.push((name, infer_expr(env, &item.expr)));
    }
    Some(cols)
}

/// Check aggregation boundary: non-aggregate RETURN items must be grouping keys.
fn check_aggregation_boundary(env: &mut TypeEnv<'_>, q: &QueryStmt) {
    // Only applies when RETURN contains aggregates.
    let has_aggregate = q
        .return_clause
        .items
        .iter()
        .any(|i| expr_contains_aggregate(&i.expr));
    if !has_aggregate || q.return_clause.star || q.return_clause.no_bindings {
        return;
    }

    // Build the set of grouping key expressions.
    let group_keys: Vec<&Expr> = if let Some(group_by) = &q.group_by {
        group_by.iter().collect()
    } else {
        // Implicit grouping: non-aggregate RETURN items are implicit group keys.
        // The executor handles this — we don't warn for implicit grouping.
        return;
    };

    // Check each RETURN item: must be a grouping key or contain an aggregate.
    for item in &q.return_clause.items {
        if expr_contains_aggregate(&item.expr) {
            continue;
        }
        // Check if this expression matches a grouping key.
        if !is_grouping_key(&item.expr, &group_keys) {
            let name = item
                .alias
                .as_deref()
                .or(match &item.expr {
                    Expr::Variable(v) => Some(v.as_str()),
                    Expr::PropertyAccess { property, .. } => Some(property.as_str()),
                    _ => None,
                })
                .unwrap_or("<expression>");
            env.warn(
                WarningKind::GroupingViolation,
                format!("`{name}` appears in the projection but is neither grouped nor aggregated"),
            );
        }
    }
}

/// Check if an expression matches one of the grouping keys.
fn is_grouping_key(expr: &Expr, group_keys: &[&Expr]) -> bool {
    group_keys
        .iter()
        .any(|key| exprs_structurally_equal(expr, key))
}

/// Structural equality check for expressions (sufficient for grouping key matching).
fn exprs_structurally_equal(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Variable(va), Expr::Variable(vb)) => va == vb,
        (
            Expr::PropertyAccess {
                target: ta,
                property: pa,
            },
            Expr::PropertyAccess {
                target: tb,
                property: pb,
            },
        ) => pa == pb && exprs_structurally_equal(ta, tb),
        (Expr::Literal(la), Expr::Literal(lb)) => la == lb,
        (Expr::FunctionCall { name: na, args: aa }, Expr::FunctionCall { name: nb, args: ab }) => {
            na == nb
                && aa.len() == ab.len()
                && aa
                    .iter()
                    .zip(ab.iter())
                    .all(|(a, b)| exprs_structurally_equal(a, b))
        }
        _ => false,
    }
}

/// Check if an expression contains an aggregate function call.
fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(_) => true,
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target)
        | Expr::Cast { expr: target, .. }
        | Expr::IsTruth { expr: target, .. }
        | Expr::IsLabeled { expr: target, .. }
        | Expr::IsDirected { expr: target, .. }
        | Expr::IsType { expr: target, .. } => expr_contains_aggregate(target),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right)
        | Expr::Concat(left, right)
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::IsSourceOf {
            node: left,
            edge: right,
            ..
        }
        | Expr::IsDestOf {
            node: left,
            edge: right,
            ..
        } => expr_contains_aggregate(left) || expr_contains_aggregate(right),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_aggregate),
        Expr::Case(case) => {
            case.operand
                .as_ref()
                .is_some_and(|e| expr_contains_aggregate(e))
                || case.when_then.iter().any(|wt| {
                    expr_contains_aggregate(&wt.when) || expr_contains_aggregate(&wt.then)
                })
                || case
                    .else_expr
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e))
        }
        Expr::Coalesce(exprs)
        | Expr::ListLiteral(exprs)
        | Expr::AllDifferent(exprs)
        | Expr::Same(exprs)
        | Expr::PathConstructor(exprs) => exprs.iter().any(expr_contains_aggregate),
        Expr::InList { expr, list, .. } => {
            expr_contains_aggregate(expr) || list.iter().any(expr_contains_aggregate)
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            expr_contains_aggregate(expr) || expr_contains_aggregate(pattern)
        }
        Expr::RecordLiteral(fields) => fields.iter().any(|(_, e)| expr_contains_aggregate(e)),
        Expr::LetIn { bindings, body } => {
            bindings.iter().any(|(_, e)| expr_contains_aggregate(e))
                || expr_contains_aggregate(body)
        }
        Expr::Exists(stmt) | Expr::ValueSubquery(stmt) => {
            // Subquery aggregates are scoped to the subquery — don't count.
            let _ = stmt;
            false
        }
        Expr::PropertyExists { target, .. } => expr_contains_aggregate(target),
        Expr::Literal(_) | Expr::Variable(_) | Expr::Parameter { .. } | Expr::PathVar(_) => false,
    }
}

fn build_env_from_match_clause(env: &mut TypeEnv<'_>, mc: &MatchClause) {
    build_env_from_bindings(env, bindings_for_match_clause(mc));
}

fn check_match_entry_constraints(env: &mut TypeEnv<'_>, entry: &MatchEntry) {
    check_match_clause_endpoint_constraints(env, &entry.pattern);
}

fn check_match_clause_endpoint_constraints(env: &mut TypeEnv<'_>, mc: &MatchClause) {
    let mut current = &mc.start;
    for chain in mc.hops() {
        let Some((edge_label, allowed)) = resolved_edge_constraints(env.schema, &chain.edge) else {
            // Even without resolved edge constraints, try to propagate label
            // constraints from edge-label narrowing (type(e) = 'X' in WHERE).
            if let Some(var) = &chain.edge.var
                && let Some(narrowed_label) = env.narrowed_edge_labels.get(var).cloned()
            {
                let allowed = env.schema.edge_endpoint_types(&narrowed_label);
                if !allowed.is_empty() {
                    check_direction_aware_endpoints(
                        env,
                        current,
                        &chain.node,
                        chain.edge.direction,
                        &narrowed_label,
                        &allowed,
                    );
                }
            }
            current = &chain.node;
            continue;
        };
        if !allowed.is_empty() {
            check_direction_aware_endpoints(
                env,
                current,
                &chain.node,
                chain.edge.direction,
                &edge_label,
                &allowed,
            );
        }
        current = &chain.node;
    }
}

/// Check endpoint constraints considering edge direction.
///
/// - `Outgoing` (`->`): src must satisfy `from`, dst must satisfy `to`.
/// - `Incoming` (`<-`): src must satisfy `to`, dst must satisfy `from` (reversed).
/// - `Either` (`-`): at least one orientation must be satisfiable.
fn check_direction_aware_endpoints(
    env: &mut TypeEnv<'_>,
    left_node: &NodePattern,
    right_node: &NodePattern,
    direction: Direction,
    edge_label: &str,
    allowed: &[(Vec<String>, Vec<String>)],
) {
    let left_labels = node_pattern_labels(env, left_node);
    let right_labels = node_pattern_labels(env, right_node);
    if left_labels.is_empty() || right_labels.is_empty() {
        return;
    }
    let satisfies_forward = allowed
        .iter()
        .any(|(from, to)| labels_satisfy(&left_labels, from) && labels_satisfy(&right_labels, to));
    let satisfies_reverse = allowed
        .iter()
        .any(|(from, to)| labels_satisfy(&right_labels, from) && labels_satisfy(&left_labels, to));
    let ok = match direction {
        Direction::Outgoing => satisfies_forward,
        Direction::Incoming => satisfies_reverse,
        Direction::Either => satisfies_forward || satisfies_reverse,
    };
    if !ok {
        let dir_str = match direction {
            Direction::Outgoing => "->",
            Direction::Incoming => "<-",
            Direction::Either => "-",
        };
        env.warn_with_provenance(
            WarningKind::ImpossiblePattern,
            format!(
                "pattern endpoint contradiction: edge label '{edge_label}' does not allow {:?} {dir_str} {:?}",
                left_labels, right_labels
            ),
            WarningProvenance::EndpointCheck { edge_label: edge_label.to_string() },
        );
    }
}

fn node_pattern_labels(env: &TypeEnv<'_>, node: &NodePattern) -> Vec<String> {
    if !node.labels.is_empty() {
        return node.labels.clone();
    }
    if let Some(TypeExpr::Name(type_name)) = &node.type_annotation
        && let Some(labels) = env.schema.resolve_node_type_labels(type_name)
    {
        return labels;
    }
    if let Some(var) = &node.var
        && let Type::Node(info) = env.get(var)
        && !info.labels.is_empty()
    {
        return info.labels;
    }
    // Flow-sensitive: use labels narrowed by WHERE predicates (e.g. `n IS LABELED :Person`).
    if let Some(var) = &node.var
        && let Some(narrowed) = env.narrowed_labels.get(var)
        && !narrowed.is_empty()
    {
        return narrowed.clone();
    }
    vec![]
}

fn resolved_edge_constraints(
    schema: &dyn PropertySchema,
    edge: &EdgePattern,
) -> Option<(String, Vec<(Vec<String>, Vec<String>)>)> {
    if let Some(label) = edge.label.as_ref() {
        return Some((label.clone(), schema.edge_endpoint_types(label)));
    }
    if let Some(TypeExpr::Name(type_name)) = &edge.type_annotation
        && let Some((label, from, to)) = schema.resolve_edge_type(type_name)
    {
        return Some((label, vec![(from, to)]));
    }
    None
}

fn labels_satisfy(actual: &[String], required: &[String]) -> bool {
    required
        .iter()
        .all(|label| actual.iter().any(|actual_label| actual_label == label))
}

fn build_env_from_bindings(env: &mut TypeEnv<'_>, bindings: Vec<crate::semantic::BindingInfo>) {
    for binding in bindings {
        if binding.nullable {
            env.optional_vars.insert(binding.name.clone());
        }
        match binding.kind {
            BindingKind::Node => {
                // Variable reuse contradiction: if the same variable already has
                // a Node type with labels, and the new binding introduces a
                // conflicting label set, warn about an impossible pattern.
                if !binding.labels.is_empty() {
                    if let Type::Node(existing) = env.get(&binding.name) {
                        if !existing.labels.is_empty()
                            && !existing.labels.iter().any(|l| binding.labels.contains(l))
                        {
                            env.warn_with_provenance(
                                WarningKind::ImpossiblePattern,
                                format!(
                                    "variable `{}` reused with conflicting labels: {:?} vs {:?}",
                                    binding.name, existing.labels, binding.labels
                                ),
                                WarningProvenance::Binding(binding.name.clone()),
                            );
                        }
                    }
                }
                let properties = if !binding.labels.is_empty() {
                    env.schema.node_property_types(&binding.labels)
                } else {
                    Vec::new()
                };
                env.bind(
                    binding.name,
                    Type::Node(NodeTypeInfo {
                        labels: binding.labels,
                        properties,
                    }),
                );
            }
            BindingKind::Edge => {
                let (endpoints, properties) = if let Some(label) = &binding.edge_label {
                    (
                        env.schema.edge_endpoint_types(label),
                        env.schema.edge_property_types(label),
                    )
                } else {
                    (Vec::new(), Vec::new())
                };
                env.bind(
                    binding.name,
                    Type::Edge(EdgeTypeInfo {
                        label: binding.edge_label,
                        endpoints,
                        properties,
                    }),
                );
            }
            BindingKind::Path => {
                let info = match binding.path_length {
                    Some(PathLength::Fixed(n)) => PathTypeInfo {
                        min_hops: Some(n),
                        max_hops: Some(n),
                    },
                    Some(PathLength::Range { min, max }) => PathTypeInfo {
                        min_hops: Some(min),
                        max_hops: Some(max),
                    },
                    None => PathTypeInfo::unbounded(),
                };
                env.bind(binding.name, Type::Path(info));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Expression type inference
// ---------------------------------------------------------------------------

fn infer_expr(env: &TypeEnv<'_>, expr: &Expr) -> Type {
    match expr {
        Expr::Literal(v) => infer_literal(v),
        Expr::Variable(name) => env.get(name),
        Expr::Parameter {
            type_annotation, ..
        } => match type_annotation {
            Some(types) if types.len() == 1 => Type::Scalar(types[0]),
            Some(types) => make_union(types.iter().map(|vt| Type::Scalar(*vt)).collect()),
            None => Type::Unknown,
        },
        Expr::PropertyAccess { target, property } => {
            let target_ty = infer_expr(env, target);
            // Determine if the target variable is optional (from OPTIONAL MATCH).
            let target_optional = matches!(target.as_ref(), Expr::Variable(v) if env.optional_vars.contains(v.as_str()));
            // Determine if this (var, property) was narrowed to non-null by WHERE.
            let narrowed = matches!(target.as_ref(), Expr::Variable(v) if env.narrowed_nonnull.contains(&(v.clone(), property.clone())));
            let raw_type = match &target_ty {
                Type::Node(info) if !info.properties.is_empty() => info
                    .properties
                    .iter()
                    .find(|(name, _, _)| name == property)
                    .map(|(_, vt, required)| {
                        let base = Type::Scalar(*vt);
                        if *required {
                            Type::NonNull(Box::new(base))
                        } else {
                            base
                        }
                    })
                    .unwrap_or(Type::Unknown),
                Type::Node(info) if !info.labels.is_empty() => env
                    .schema
                    .node_property_types(&info.labels)
                    .iter()
                    .find(|(name, _, _)| name == property)
                    .map(|(_, vt, required)| {
                        let base = Type::Scalar(*vt);
                        if *required {
                            Type::NonNull(Box::new(base))
                        } else {
                            base
                        }
                    })
                    .unwrap_or(Type::Unknown),
                Type::Edge(info) if !info.properties.is_empty() => info
                    .properties
                    .iter()
                    .find(|(name, _, _)| name == property)
                    .map(|(_, vt, required)| {
                        let base = Type::Scalar(*vt);
                        if *required {
                            Type::NonNull(Box::new(base))
                        } else {
                            base
                        }
                    })
                    .unwrap_or(Type::Unknown),
                Type::Edge(info) if info.label.is_some() => env
                    .schema
                    .edge_property_types(info.label.as_ref().unwrap())
                    .iter()
                    .find(|(name, _, _)| name == property)
                    .map(|(_, vt, required)| {
                        let base = Type::Scalar(*vt);
                        if *required {
                            Type::NonNull(Box::new(base))
                        } else {
                            base
                        }
                    })
                    .unwrap_or(Type::Unknown),
                Type::Record(fields) => fields
                    .iter()
                    .find(|(k, _)| k == property)
                    .map(|(_, t)| t.clone())
                    .unwrap_or(Type::Unknown),
                Type::Never => Type::Never,
                _ => Type::Unknown,
            };
            // Apply OPTIONAL MATCH null lifting: strip NonNull because the binding
            // itself may be null (the optional match may not have matched).
            if target_optional {
                strip_nonnull(raw_type)
            } else if narrowed {
                // Flow-sensitive narrowing: wrap in NonNull if not already.
                ensure_nonnull(raw_type)
            } else {
                raw_type
            }
        }
        Expr::BinaryOp { op, left, right } => infer_binary_op(env, *op, left, right),
        Expr::UnaryOp { op, expr: inner } => {
            let t = infer_expr(env, inner);
            match op {
                UnaryOp::Neg | UnaryOp::Pos => t,
            }
        }
        Expr::And(_, _) | Expr::Or(_, _) | Expr::Not(_) | Expr::Xor(_, _) => {
            Type::Scalar(ValueType::Bool)
        }
        Expr::Compare { .. }
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::InList { .. }
        | Expr::StringPredicate { .. }
        | Expr::IsTruth { .. }
        | Expr::IsLabeled { .. }
        | Expr::IsSourceOf { .. }
        | Expr::IsDestOf { .. }
        | Expr::IsDirected { .. }
        | Expr::IsType { .. }
        | Expr::AllDifferent(_)
        | Expr::Same(_)
        | Expr::PropertyExists { .. } => Type::Scalar(ValueType::Bool),
        Expr::Case(case) => infer_case(env, case),
        Expr::Coalesce(exprs) => {
            let types: Vec<Type> = exprs.iter().map(|e| infer_expr(env, e)).collect();
            make_union(types)
        }
        Expr::NullIf { left, .. } => infer_expr(env, left),
        Expr::Aggregate(agg) => infer_aggregate(env, agg),
        Expr::FunctionCall { name, args } => infer_function_call(env, name, args),
        Expr::Exists(_) => Type::Scalar(ValueType::Bool),
        Expr::Concat(_, _) => Type::Scalar(ValueType::Text),
        Expr::PathVar(_) => Type::Path(PathTypeInfo::default()),
        Expr::PathLength(_) => Type::Scalar(ValueType::Int64),
        Expr::ListLiteral(elems) => {
            if elems.is_empty() {
                Type::TypedList(Box::new(Type::Unknown))
            } else {
                let types: Vec<Type> = elems.iter().map(|e| infer_expr(env, e)).collect();
                Type::TypedList(Box::new(make_union(types)))
            }
        }
        Expr::ListIndex { .. } => Type::Unknown,
        Expr::Cast { target_type, .. } => Type::Scalar(*target_type),
        Expr::RecordLiteral(fields) => {
            let typed_fields = fields
                .iter()
                .map(|(k, v)| (k.clone(), infer_expr(env, v)))
                .collect();
            Type::Record(typed_fields)
        }
        Expr::ValueSubquery(_) => Type::Unknown,
        Expr::LetIn { bindings, body } => {
            // Create a temporary env extension — we avoid mutation of main env
            // by just inferring in the current env (bindings shadow).
            // For simplicity, we just return Unknown for complex let-in.
            if bindings.is_empty() {
                infer_expr(env, body)
            } else {
                Type::Unknown
            }
        }
        Expr::PathConstructor(_) => Type::Path(PathTypeInfo::default()),
    }
}

fn infer_literal(v: &gleaph_types::Value) -> Type {
    use gleaph_types::Value;
    match v {
        Value::Int8(_) => Type::Scalar(ValueType::Int8),
        Value::Int16(_) => Type::Scalar(ValueType::Int16),
        Value::Int32(_) => Type::Scalar(ValueType::Int32),
        Value::Int64(_) => Type::Scalar(ValueType::Int64),
        Value::Int128(_) => Type::Scalar(ValueType::Int128),
        Value::Int256(_) => Type::Scalar(ValueType::Int256),
        Value::Uint8(_) => Type::Scalar(ValueType::Uint8),
        Value::Uint16(_) => Type::Scalar(ValueType::Uint16),
        Value::Uint32(_) => Type::Scalar(ValueType::Uint32),
        Value::Uint64(_) => Type::Scalar(ValueType::Uint64),
        Value::Uint128(_) => Type::Scalar(ValueType::Uint128),
        Value::Uint256(_) => Type::Scalar(ValueType::Uint256),
        Value::Float32(_) => Type::Scalar(ValueType::Float32),
        Value::Float64(_) => Type::Scalar(ValueType::Float64),
        Value::Text(_) => Type::Scalar(ValueType::Text),
        Value::Bool(_) => Type::Scalar(ValueType::Bool),
        Value::Null => Type::Scalar(ValueType::Null),
        Value::Timestamp(_) => Type::Scalar(ValueType::Timestamp),
        Value::List(_) => Type::Scalar(ValueType::List),
        Value::Path(_) => Type::Path(PathTypeInfo::default()),
        Value::Bytes(_) => Type::Scalar(ValueType::Bytes),
        Value::Date(_) => Type::Scalar(ValueType::Date),
        Value::Time(_) => Type::Scalar(ValueType::Time),
        Value::DateTime(_, _) => Type::Scalar(ValueType::DateTime),
        Value::Duration(_, _) => Type::Scalar(ValueType::Duration),
        Value::Principal(_) => Type::Unknown,
        Value::Decimal(_) => Type::Scalar(ValueType::Decimal),
    }
}

fn infer_binary_op(env: &TypeEnv<'_>, op: BinaryOp, left: &Expr, right: &Expr) -> Type {
    let lt = infer_expr(env, left);
    let rt = infer_expr(env, right);

    if is_never(&lt) || is_never(&rt) {
        return Type::Never;
    }
    if is_unknown(&lt) || is_unknown(&rt) {
        return Type::Unknown;
    }

    match op {
        BinaryOp::Add => {
            match (&lt, &rt) {
                (Type::Scalar(ValueType::Text), Type::Scalar(ValueType::Text)) => {
                    Type::Scalar(ValueType::Text)
                }
                (Type::Scalar(ValueType::List), Type::Scalar(ValueType::List)) => {
                    Type::Scalar(ValueType::List)
                }
                (Type::Scalar(a), Type::Scalar(b)) if is_integer_vt(*a) && is_integer_vt(*b) => {
                    // Result is the wider of the two integer types.
                    Type::Scalar(wider_int_vt(*a, *b))
                }
                (Type::Scalar(ValueType::Float64), _) | (_, Type::Scalar(ValueType::Float64)) => {
                    if is_numeric(&lt) && is_numeric(&rt) {
                        Type::Scalar(ValueType::Float64)
                    } else {
                        Type::Unknown // temporal combos etc
                    }
                }
                (Type::Scalar(ValueType::Float32), Type::Scalar(ValueType::Float32)) => {
                    Type::Scalar(ValueType::Float32)
                }
                (Type::Scalar(ValueType::Float32), _) | (_, Type::Scalar(ValueType::Float32)) => {
                    if is_numeric(&lt) && is_numeric(&rt) {
                        Type::Scalar(ValueType::Float32)
                    } else {
                        Type::Unknown
                    }
                }
                // Temporal: Date + Duration, DateTime + Duration, etc.
                (Type::Scalar(ValueType::Date), Type::Scalar(ValueType::Duration))
                | (Type::Scalar(ValueType::Duration), Type::Scalar(ValueType::Date)) => {
                    Type::Scalar(ValueType::Date)
                }
                (Type::Scalar(ValueType::DateTime), Type::Scalar(ValueType::Duration))
                | (Type::Scalar(ValueType::Duration), Type::Scalar(ValueType::DateTime)) => {
                    Type::Scalar(ValueType::DateTime)
                }
                (Type::Scalar(ValueType::Time), Type::Scalar(ValueType::Duration))
                | (Type::Scalar(ValueType::Duration), Type::Scalar(ValueType::Time)) => {
                    Type::Scalar(ValueType::Time)
                }
                (Type::Scalar(ValueType::Duration), Type::Scalar(ValueType::Duration)) => {
                    Type::Scalar(ValueType::Duration)
                }
                _ => Type::Unknown,
            }
        }
        BinaryOp::Sub => match (&lt, &rt) {
            (Type::Scalar(a), Type::Scalar(b)) if is_integer_vt(*a) && is_integer_vt(*b) => {
                Type::Scalar(wider_int_vt(*a, *b))
            }
            (Type::Scalar(ValueType::Float64), _) | (_, Type::Scalar(ValueType::Float64)) => {
                if is_numeric(&lt) && is_numeric(&rt) {
                    Type::Scalar(ValueType::Float64)
                } else {
                    Type::Unknown
                }
            }
            (Type::Scalar(ValueType::Float32), Type::Scalar(ValueType::Float32)) => {
                Type::Scalar(ValueType::Float32)
            }
            (Type::Scalar(ValueType::Float32), _) | (_, Type::Scalar(ValueType::Float32)) => {
                if is_numeric(&lt) && is_numeric(&rt) {
                    Type::Scalar(ValueType::Float32)
                } else {
                    Type::Unknown
                }
            }
            (Type::Scalar(ValueType::Date), Type::Scalar(ValueType::Date)) => {
                Type::Scalar(ValueType::Duration)
            }
            (Type::Scalar(ValueType::Date), Type::Scalar(ValueType::Duration)) => {
                Type::Scalar(ValueType::Date)
            }
            (Type::Scalar(ValueType::DateTime), Type::Scalar(ValueType::DateTime)) => {
                Type::Scalar(ValueType::Duration)
            }
            (Type::Scalar(ValueType::DateTime), Type::Scalar(ValueType::Duration)) => {
                Type::Scalar(ValueType::DateTime)
            }
            (Type::Scalar(ValueType::Time), Type::Scalar(ValueType::Time)) => {
                Type::Scalar(ValueType::Duration)
            }
            (Type::Scalar(ValueType::Time), Type::Scalar(ValueType::Duration)) => {
                Type::Scalar(ValueType::Time)
            }
            (Type::Scalar(ValueType::Duration), Type::Scalar(ValueType::Duration)) => {
                Type::Scalar(ValueType::Duration)
            }
            _ => Type::Unknown,
        },
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => match (&lt, &rt) {
            (Type::Scalar(a), Type::Scalar(b)) if is_integer_vt(*a) && is_integer_vt(*b) => {
                Type::Scalar(wider_int_vt(*a, *b))
            }
            _ if is_numeric(&lt) && is_numeric(&rt) => Type::Scalar(ValueType::Float64),
            _ => Type::Unknown,
        },
    }
}

fn infer_case(env: &TypeEnv<'_>, case: &CaseExpr) -> Type {
    let mut types = Vec::new();
    for wt in &case.when_then {
        types.push(infer_expr(env, &wt.then));
    }
    if let Some(ref e) = case.else_expr {
        types.push(infer_expr(env, e));
    }
    make_union(types)
}

fn infer_aggregate(env: &TypeEnv<'_>, agg: &AggregateExpr) -> Type {
    match agg.func {
        AggFunc::Count => Type::Scalar(ValueType::Int64),
        AggFunc::Sum => {
            if let Some(ref inner) = agg.expr {
                infer_expr(env, inner)
            } else {
                Type::Unknown
            }
        }
        AggFunc::Avg | AggFunc::PercentileCont | AggFunc::PercentileDisc => {
            Type::Scalar(ValueType::Float64)
        }
        AggFunc::Min | AggFunc::Max => {
            if let Some(ref inner) = agg.expr {
                infer_expr(env, inner)
            } else {
                Type::Unknown
            }
        }
        AggFunc::Collect => {
            if let Some(ref inner) = agg.expr {
                Type::TypedList(Box::new(infer_expr(env, inner)))
            } else {
                Type::TypedList(Box::new(Type::Unknown))
            }
        }
        AggFunc::StringAgg => Type::Scalar(ValueType::Text),
    }
}

fn infer_function_call(env: &TypeEnv<'_>, name: &str, args: &[Expr]) -> Type {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "id" | "element_id" | "edge_id" | "source" | "destination" => {
            Type::Scalar(ValueType::Int64)
        }
        "labels" | "keys" => Type::TypedList(Box::new(Type::Scalar(ValueType::Text))),
        "type" => Type::Scalar(ValueType::Text),
        "properties" => Type::Unknown, // record
        "property_exists" => Type::Scalar(ValueType::Bool),
        "gleaph_weight" | "weight" => Type::Scalar(ValueType::Float64),
        "gleaph_timestamp" | "timestamp" => Type::Scalar(ValueType::Timestamp),
        "size" | "length" | "cardinality" => Type::Scalar(ValueType::Int64),
        "upper" | "lower" | "trim" | "ltrim" | "rtrim" | "left" | "right" | "substring"
        | "replace" | "reverse" | "to_string" | "to_text" => Type::Scalar(ValueType::Text),
        "abs" | "ceil" | "floor" | "round" | "to_integer" | "to_int" => {
            Type::Scalar(ValueType::Int64)
        }
        "to_float" | "sqrt" | "log" | "log10" | "exp" | "power" | "acos" | "asin" | "atan"
        | "atan2" | "cos" | "sin" | "tan" | "degrees" | "radians" | "pi" | "e_constant" => {
            Type::Scalar(ValueType::Float64)
        }
        "to_boolean" | "to_bool" => Type::Scalar(ValueType::Bool),
        "coalesce" => {
            let types: Vec<Type> = args.iter().map(|a| infer_expr(env, a)).collect();
            make_union(types)
        }
        "range" => Type::TypedList(Box::new(Type::Scalar(ValueType::Int64))),
        "to_hex" => Type::Scalar(ValueType::Text),
        "from_hex" => Type::Scalar(ValueType::Bytes),
        "byte_length" => Type::Scalar(ValueType::Int64),
        "head" | "last" => Type::Unknown, // element type unknown
        "tail" => Type::Scalar(ValueType::List),
        "list_sum" | "list_avg" | "list_min" | "list_max" => Type::Unknown,
        "date" => Type::Scalar(ValueType::Date),
        "time" => Type::Scalar(ValueType::Time),
        "datetime" | "localdatetime" => Type::Scalar(ValueType::DateTime),
        "duration" => Type::Scalar(ValueType::Duration),
        "year" | "month" | "day" | "hour" | "minute" | "second" => Type::Scalar(ValueType::Int64),
        "char_length" => Type::Scalar(ValueType::Int64),
        "split" => Type::TypedList(Box::new(Type::Scalar(ValueType::Text))),
        _ => Type::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_unknown(t: &Type) -> bool {
    match t {
        Type::Unknown => true,
        Type::Union(variants) => variants.iter().any(is_unknown),
        _ => false,
    }
}

fn is_never(t: &Type) -> bool {
    matches!(t, Type::Never)
}

fn is_null(t: &Type) -> bool {
    matches!(t, Type::Scalar(ValueType::Null))
}

fn is_numeric(t: &Type) -> bool {
    match t {
        Type::Scalar(vt) => is_numeric_vt(*vt),
        _ => false,
    }
}

fn scalar_type(t: &Type) -> Option<ValueType> {
    match t {
        Type::Scalar(vt) => Some(*vt),
        Type::NonNull(inner) => scalar_type(inner),
        _ => None,
    }
}

fn types_addable(a: &Type, b: &Type) -> bool {
    match (scalar_type(a), scalar_type(b)) {
        (Some(ValueType::Text), Some(ValueType::Text)) => true,
        (Some(ValueType::List), Some(ValueType::List)) => true,
        (Some(va), Some(vb)) if is_numeric_vt(va) && is_numeric_vt(vb) => true,
        // Temporal additions.
        (Some(ValueType::Date), Some(ValueType::Duration))
        | (Some(ValueType::Duration), Some(ValueType::Date))
        | (Some(ValueType::DateTime), Some(ValueType::Duration))
        | (Some(ValueType::Duration), Some(ValueType::DateTime))
        | (Some(ValueType::Time), Some(ValueType::Duration))
        | (Some(ValueType::Duration), Some(ValueType::Time))
        | (Some(ValueType::Duration), Some(ValueType::Duration)) => true,
        _ => {
            // TypedList + TypedList also fine
            matches!((a, b), (Type::TypedList(_), Type::TypedList(_)))
        }
    }
}

fn types_arithmetic(a: &Type, b: &Type, op: BinaryOp) -> bool {
    match (scalar_type(a), scalar_type(b)) {
        (Some(va), Some(vb)) if is_numeric_vt(va) && is_numeric_vt(vb) => true,
        _ if op == BinaryOp::Sub => {
            // Temporal subtractions.
            matches!(
                (scalar_type(a), scalar_type(b)),
                (Some(ValueType::Date), Some(ValueType::Date))
                    | (Some(ValueType::Date), Some(ValueType::Duration))
                    | (Some(ValueType::DateTime), Some(ValueType::DateTime))
                    | (Some(ValueType::DateTime), Some(ValueType::Duration))
                    | (Some(ValueType::Time), Some(ValueType::Time))
                    | (Some(ValueType::Time), Some(ValueType::Duration))
                    | (Some(ValueType::Duration), Some(ValueType::Duration))
            )
        }
        _ => false,
    }
}

fn types_comparable(a: &Type, b: &Type) -> bool {
    // Unwrap NonNull for comparison purposes.
    let a = unwrap_nonnull(a);
    let b = unwrap_nonnull(b);
    match (a, b) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Scalar(ValueType::Null), _) | (_, Type::Scalar(ValueType::Null)) => true,
        (Type::Scalar(va), Type::Scalar(vb)) => {
            va == vb || (is_numeric_vt(*va) && is_numeric_vt(*vb))
        }
        // Node-Node, Edge-Edge comparisons.
        (Type::Node(_), Type::Node(_)) | (Type::Edge(_), Type::Edge(_)) => true,
        _ => false,
    }
}

fn unwrap_nonnull(t: &Type) -> &Type {
    match t {
        Type::NonNull(inner) => unwrap_nonnull(inner),
        other => other,
    }
}

/// Strip any NonNull wrapper from a type (for OPTIONAL MATCH null lifting).
fn strip_nonnull(t: Type) -> Type {
    match t {
        Type::NonNull(inner) => *inner,
        other => other,
    }
}

/// Ensure a type is wrapped in NonNull (for flow-sensitive narrowing).
/// If already NonNull or Unknown/Never, return as-is.
fn ensure_nonnull(t: Type) -> Type {
    match &t {
        Type::NonNull(_) | Type::Unknown | Type::Never => t,
        _ => Type::NonNull(Box::new(t)),
    }
}

fn is_numeric_vt(vt: ValueType) -> bool {
    is_integer_vt(vt)
        || matches!(
            vt,
            ValueType::Float32 | ValueType::Float64 | ValueType::Timestamp
        )
}

fn is_integer_vt(vt: ValueType) -> bool {
    matches!(
        vt,
        ValueType::Int8
            | ValueType::Int16
            | ValueType::Int32
            | ValueType::Int64
            | ValueType::Int128
            | ValueType::Int256
            | ValueType::Uint8
            | ValueType::Uint16
            | ValueType::Uint32
            | ValueType::Uint64
            | ValueType::Uint128
            | ValueType::Uint256
    )
}

/// Given two integer ValueTypes, return the wider one.
fn wider_int_vt(a: ValueType, b: ValueType) -> ValueType {
    let width = |vt: ValueType| -> u16 {
        match vt {
            ValueType::Int8 | ValueType::Uint8 => 8,
            ValueType::Int16 | ValueType::Uint16 => 16,
            ValueType::Int32 | ValueType::Uint32 => 32,
            ValueType::Int64 | ValueType::Uint64 => 64,
            ValueType::Int128 | ValueType::Uint128 => 128,
            ValueType::Int256 | ValueType::Uint256 => 256,
            _ => 64,
        }
    };
    if width(a) >= width(b) { a } else { b }
}

/// Flatten a type into its constituent non-union variants.
/// For non-union types returns a single-element slice reference.
fn flatten_union(t: &Type) -> Vec<&Type> {
    match t {
        Type::Union(variants) => variants.iter().collect(),
        other => vec![other],
    }
}

fn make_union(types: Vec<Type>) -> Type {
    // Filter out Never variants (bottom type is identity for union).
    let types: Vec<Type> = types.into_iter().filter(|t| !is_never(t)).collect();
    if types.is_empty() {
        return Type::Never;
    }
    // If all types are the same, collapse.
    let first = &types[0];
    if types.iter().all(|t| t == first) {
        return first.clone();
    }
    // If any is Unknown, result is Unknown.
    if types.iter().any(is_unknown) {
        return Type::Unknown;
    }
    Type::Union(types)
}

// ===========================================================================
// Phase B: Constraint-based type checking
// ===========================================================================

use crate::semantic::{ConstraintSet, SolvedTypeTable, TypeVarId, TypedConstraint};

/// Generate typed constraints from a statement by walking the AST once.
///
/// Each expression gets a `TypeVarId` with its inferred type stored in the
/// `SolvedTypeTable`. Constraints reference type variables instead of AST nodes,
/// eliminating redundant `infer_expr()` calls during solving.
pub fn generate_constraints(
    stmt: &Statement,
    schema: &dyn PropertySchema,
    _analysis: &SemanticAnalysis,
) -> (ConstraintSet, Vec<TypeWarning>) {
    let mut cgen = ConstraintGenerator {
        types: SolvedTypeTable::new(),
        constraints: Vec::new(),
        env: TypeEnv::new(schema),
    };
    cgen.emit_statement(stmt);
    (
        ConstraintSet {
            constraints: cgen.constraints,
            types: cgen.types,
        },
        cgen.env.warnings,
    )
}

/// Solve constraints and produce warnings identical to the legacy direct-checking path.
pub fn solve_constraints(
    cset: &ConstraintSet,
    schema: &dyn PropertySchema,
    _analysis: &SemanticAnalysis,
) -> Vec<TypeWarning> {
    let mut warnings = Vec::new();
    for (idx, constraint) in cset.constraints.iter().enumerate() {
        match constraint {
            TypedConstraint::MustBeBoolean { expr_id } => {
                let ty = cset.types.get(*expr_id);
                if is_never(ty) {
                    // unreachable — no warning
                } else if let Type::Scalar(vt) = ty
                    && *vt != ValueType::Bool
                    && *vt != ValueType::Null
                {
                    warnings.push(TypeWarning {
                        kind: WarningKind::NonBooleanCondition,
                        message: format!("WHERE/FILTER condition has type {vt:?}, expected Bool"),
                        provenance: Some(WarningProvenance::Constraint(idx)),
                    });
                }
            }
            TypedConstraint::Arithmetic {
                op, left, right, ..
            } => {
                let lt = cset.types.get(*left);
                let rt = cset.types.get(*right);
                if is_unknown(lt)
                    || is_unknown(rt)
                    || is_null(lt)
                    || is_null(rt)
                    || is_never(lt)
                    || is_never(rt)
                {
                    continue;
                }
                let lefts = flatten_union(lt);
                let rights = flatten_union(rt);
                match op {
                    BinaryOp::Add => {
                        if !lefts
                            .iter()
                            .any(|l| rights.iter().any(|r| types_addable(l, r)))
                        {
                            warnings.push(TypeWarning {
                                kind: WarningKind::BinaryOpMismatch,
                                message: format!(
                                    "operator + applied to incompatible types: {lt:?} and {rt:?}"
                                ),
                                provenance: Some(WarningProvenance::Constraint(idx)),
                            });
                        }
                    }
                    BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                        if !lefts
                            .iter()
                            .any(|l| rights.iter().any(|r| types_arithmetic(l, r, *op)))
                        {
                            warnings.push(TypeWarning {
                                kind: WarningKind::BinaryOpMismatch,
                                message: format!("operator {op:?} applied to incompatible types: {lt:?} and {rt:?}"),
                                provenance: Some(WarningProvenance::Constraint(idx)),
                            });
                        }
                    }
                }
            }
            TypedConstraint::Comparison { left, right, .. } => {
                let lt = cset.types.get(*left);
                let rt = cset.types.get(*right);
                if is_unknown(lt)
                    || is_unknown(rt)
                    || is_null(lt)
                    || is_null(rt)
                    || is_never(lt)
                    || is_never(rt)
                {
                    continue;
                }
                let lefts = flatten_union(lt);
                let rights = flatten_union(rt);
                if !lefts
                    .iter()
                    .any(|l| rights.iter().any(|r| types_comparable(l, r)))
                {
                    warnings.push(TypeWarning {
                        kind: WarningKind::ComparisonMismatch,
                        message: format!(
                            "comparison between incompatible types: {lt:?} and {rt:?}"
                        ),
                        provenance: Some(WarningProvenance::Constraint(idx)),
                    });
                }
            }
            TypedConstraint::NullTest { expr_id, negated } => {
                let ty = cset.types.get(*expr_id);
                if matches!(ty, Type::NonNull(_)) {
                    warnings.push(TypeWarning {
                        kind: WarningKind::NullCheckOnNonNull,
                        message: if *negated {
                            "IS NOT NULL on a NOT NULL property (always true)".into()
                        } else {
                            "IS NULL on a NOT NULL property (always false)".into()
                        },
                        provenance: Some(WarningProvenance::Constraint(idx)),
                    });
                }
            }
            TypedConstraint::FunctionCall { name, arg_ids, .. } => {
                let lower = name.to_ascii_lowercase();
                match lower.as_str() {
                    "id" | "labels" => {
                        if arg_ids.len() == 1 {
                            let ty = cset.types.get(arg_ids[0]);
                            if !is_unknown(ty) && !matches!(ty, Type::Node(_)) {
                                warnings.push(TypeWarning {
                                    kind: WarningKind::FunctionArgMismatch,
                                    message: format!(
                                        "{lower}() expects a node argument, got {ty:?}"
                                    ),
                                    provenance: Some(WarningProvenance::Constraint(idx)),
                                });
                            }
                        }
                    }
                    "type" | "source" | "destination" | "gleaph_weight" | "weight"
                    | "gleaph_timestamp" | "timestamp" | "edge_id" => {
                        if arg_ids.len() == 1 {
                            let ty = cset.types.get(arg_ids[0]);
                            if !is_unknown(ty) && !matches!(ty, Type::Edge(_)) {
                                warnings.push(TypeWarning {
                                    kind: WarningKind::FunctionArgMismatch,
                                    message: format!(
                                        "{lower}() expects an edge argument, got {ty:?}"
                                    ),
                                    provenance: Some(WarningProvenance::Constraint(idx)),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            TypedConstraint::Subquery { stmt } => {
                // Recursively type-check subqueries using the legacy path.
                let sub_warnings = type_check_statement_with_schema(stmt, schema);
                warnings.extend(sub_warnings);
            }
        }
        let _ = idx; // provenance tracking placeholder
    }
    warnings
}

/// Constraint generator: walks the AST once, allocating type variables and emitting constraints.
struct ConstraintGenerator<'a> {
    types: SolvedTypeTable,
    constraints: Vec<TypedConstraint>,
    env: TypeEnv<'a>,
}

impl<'a> ConstraintGenerator<'a> {
    /// Allocate a type variable for an expression, computing its type via
    /// the constraint generator's own type inference (no standalone `infer_expr`).
    fn alloc_expr(&mut self, expr: &Expr) -> TypeVarId {
        let ty = self.infer_type(expr);
        self.types.alloc(ty)
    }

    /// Self-contained type inference for expressions.
    /// Replaces standalone `infer_expr` — all type inference during constraint
    /// generation flows through the ConstraintGenerator.
    fn infer_type(&self, expr: &Expr) -> Type {
        infer_expr(&self.env, expr)
    }

    /// Infer return column types for NEXT/YIELD boundary propagation.
    fn infer_return_cols(&self, stmt: &Statement) -> Option<Vec<(String, Type)>> {
        infer_return_types(&self.env, stmt)
    }

    /// Infer the result type of a binary operation.
    fn infer_binary_op(&self, op: BinaryOp, left: &Expr, right: &Expr) -> Type {
        infer_binary_op(&self.env, op, left, right)
    }

    /// Infer the result type of a function call.
    fn infer_fn_call(&self, name: &str, args: &[Expr]) -> Type {
        infer_function_call(&self.env, name, args)
    }

    fn emit_statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::Query(q) => self.emit_query(q),
            Statement::Compound { left, right, op } => {
                self.emit_statement(left);
                if let SetOp::Next(ref yield_cols) = *op {
                    // For NEXT: infer return types from left, seed env for right.
                    if let Some(return_types) = self.infer_return_cols(left) {
                        for (name, ty) in &return_types {
                            if let Some(cols) = yield_cols
                                && !cols.iter().any(|c| c == name)
                            {
                                continue;
                            }
                            self.env.bind(name.clone(), ty.clone());
                        }
                    }
                }
                self.emit_statement(right);
            }
            Statement::Delete(d) => {
                build_env_from_match_clause(&mut self.env, &d.match_clause);
                if let Some(ref w) = d.where_clause {
                    self.gen_boolean_context(w);
                }
            }
            Statement::Set(s) => {
                build_env_from_match_clause(&mut self.env, &s.match_clause);
                if let Some(ref w) = s.where_clause {
                    self.gen_boolean_context(w);
                }
                for item in &s.set_clause.items {
                    if let SetItem::Property { value, .. } = item {
                        self.gen_expr_constraints(value);
                    }
                }
            }
            Statement::Remove(r) => {
                build_env_from_match_clause(&mut self.env, &r.match_clause);
                if let Some(ref w) = r.where_clause {
                    self.gen_boolean_context(w);
                }
            }
            Statement::Filter(f) => {
                build_env_from_match_clause(&mut self.env, &f.match_clause);
                if let Some(ref w) = f.where_clause {
                    self.gen_boolean_context(w);
                }
                self.gen_boolean_context(&f.filter_expr);
            }
            Statement::Let(l) => {
                build_env_from_match_clause(&mut self.env, &l.match_clause);
                if let Some(ref w) = l.where_clause {
                    self.gen_boolean_context(w);
                }
                for (name, expr) in &l.bindings {
                    let ty = self.infer_type(expr);
                    self.gen_expr_constraints(expr);
                    self.env.bind(name.clone(), ty);
                }
                self.gen_return_clause(&l.return_clause);
            }
            Statement::For(f) => {
                self.gen_expr_constraints(&f.list_expr);
                self.env.bind(f.var.clone(), Type::Unknown);
                if let Some(ref ord) = f.ordinality_var {
                    self.env.bind(ord.clone(), Type::Scalar(ValueType::Int64));
                }
                self.gen_return_clause(&f.return_clause);
            }
            Statement::Call(c) => {
                self.emit_statement(&c.body);
            }
            _ => {}
        }
    }

    fn emit_query(&mut self, q: &QueryStmt) {
        for entry in &q.match_clauses {
            check_match_entry_constraints(&mut self.env, entry);
            build_env_from_bindings(&mut self.env, bindings_for_match_entry(entry));
        }

        if let Some(ref w) = q.where_clause {
            self.gen_boolean_context(w);
            let narrowing = extract_narrowing_facts(w);
            self.env.apply_narrowing(&narrowing);
            if narrowing.iter().any(|f| {
                matches!(
                    f,
                    NarrowingFact::LabelNarrowed { .. } | NarrowingFact::EdgeLabelNarrowed { .. }
                )
            }) {
                for entry in &q.match_clauses {
                    check_match_entry_constraints(&mut self.env, entry);
                }
            }
        }

        for with in &q.with_clauses {
            for item in &with.items {
                self.gen_expr_constraints(&item.expr);
            }

            if with.star {
                for item in &with.items {
                    if let Some(ref alias) = item.alias {
                        let ty = self.infer_type(&item.expr);
                        self.env.bind(alias.clone(), ty);
                    }
                }
            } else {
                let mut new_bindings = RapidHashMap::default();
                for item in &with.items {
                    let ty = self.infer_type(&item.expr);
                    let name = item.alias.as_deref().or(match &item.expr {
                        Expr::Variable(v) => Some(v.as_str()),
                        _ => None,
                    });
                    if let Some(n) = name {
                        new_bindings.insert(n.to_string(), ty);
                    }
                }
                self.env.bindings = new_bindings;
            }

            if let Some(ref w) = with.where_clause {
                self.gen_boolean_context(w);
                let narrowing = extract_narrowing_facts(w);
                self.env.apply_narrowing(&narrowing);
            }
            for entry in &with.match_clauses {
                check_match_entry_constraints(&mut self.env, entry);
                build_env_from_bindings(&mut self.env, bindings_for_match_entry(entry));
            }
            if let Some(ref w) = with.post_match_where {
                self.gen_boolean_context(w);
                let narrowing = extract_narrowing_facts(w);
                self.env.apply_narrowing(&narrowing);
            }
        }

        if let Some(ref h) = q.having {
            self.gen_boolean_context(h);
        }

        check_aggregation_boundary(&mut self.env, q);

        self.gen_return_clause(&q.return_clause);
    }

    fn gen_return_clause(&mut self, rc: &ReturnClause) {
        for item in &rc.items {
            self.gen_expr_constraints(&item.expr);
        }
    }

    /// Generate constraints for an expression, emitting typed constraints
    /// for each semantic check point.
    fn gen_expr_constraints(&mut self, expr: &Expr) {
        let constraints = constraints_for_expr(expr);
        for c in &constraints {
            match c {
                SemanticConstraint::ArithmeticOperands { op, left, right } => {
                    let left_id = self.alloc_expr(left);
                    let right_id = self.alloc_expr(right);
                    let result_ty = self.infer_binary_op(*op, left, right);
                    let result_id = self.types.alloc(result_ty);
                    self.constraints.push(TypedConstraint::Arithmetic {
                        op: *op,
                        left: left_id,
                        right: right_id,
                        result: result_id,
                    });
                }
                SemanticConstraint::ComparisonOperands { op, left, right } => {
                    let left_id = self.alloc_expr(left);
                    let right_id = self.alloc_expr(right);
                    self.constraints.push(TypedConstraint::Comparison {
                        op: *op,
                        left: left_id,
                        right: right_id,
                    });
                }
                SemanticConstraint::FunctionCall { name, args } => {
                    let arg_ids: Vec<TypeVarId> = args.iter().map(|a| self.alloc_expr(a)).collect();
                    let result_ty = self.infer_fn_call(name, args);
                    let result_id = self.types.alloc(result_ty);
                    self.constraints.push(TypedConstraint::FunctionCall {
                        name: name.clone(),
                        arg_ids,
                        result: result_id,
                    });
                }
                SemanticConstraint::NullTest { expr, negated } => {
                    let expr_id = self.alloc_expr(expr);
                    self.constraints.push(TypedConstraint::NullTest {
                        expr_id,
                        negated: *negated,
                    });
                }
                SemanticConstraint::BooleanContext { expr } => {
                    let expr_id = self.alloc_expr(expr);
                    self.constraints
                        .push(TypedConstraint::MustBeBoolean { expr_id });
                }
                SemanticConstraint::Subquery { stmt } => {
                    self.constraints
                        .push(TypedConstraint::Subquery { stmt: stmt.clone() });
                }
                SemanticConstraint::PropertyAccess { target, .. } => {
                    let _ = self.alloc_expr(target);
                }
                SemanticConstraint::AggregateCall { expr: inner, .. } => {
                    if let Some(e) = inner {
                        let _ = self.alloc_expr(e);
                    }
                }
                SemanticConstraint::BindingIntroduced { .. }
                | SemanticConstraint::BoundaryProjects { .. }
                | SemanticConstraint::WhereEqualityPredicate { .. }
                | SemanticConstraint::WhereRangePredicate { .. }
                | SemanticConstraint::InlineNodeProperty { .. }
                | SemanticConstraint::OptionalFilterPredicate { .. }
                | SemanticConstraint::InlineNodeWherePredicate { .. } => {}
            }
        }
    }

    /// Generate a boolean-context constraint for a WHERE/FILTER expression.
    fn gen_boolean_context(&mut self, expr: &Expr) {
        // Generate constraints for sub-expressions (comparisons, arithmetic, etc.).
        self.gen_expr_constraints(expr);
        // Add a single MustBeBoolean constraint for the top-level expression.
        let expr_id = self.alloc_expr(expr);
        self.constraints
            .push(TypedConstraint::MustBeBoolean { expr_id });
    }
}

/// Type-check using the constraint-based path. Produces the same warnings
/// as the legacy `check_statement` path but via constraint generation + solving.
pub fn type_check_via_constraints(
    stmt: &Statement,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    let analysis = analyze_statement_structure(stmt);
    let (cset, env_warnings) = generate_constraints(stmt, schema, &analysis);
    let mut warnings = env_warnings;
    warnings.extend(solve_constraints(&cset, schema, &analysis));
    warnings
}
